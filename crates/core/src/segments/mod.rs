//! On-demand semantic segmentation: split a session's activity into model-determined
//! blocks of "what was being worked on". Boundaries come from the LLM; every per-block
//! statistic is computed deterministically from the events in range — the model is
//! trusted only for *where* a new piece of work begins and *what to call it*.

use std::collections::BTreeMap;

use chrono::{DateTime, FixedOffset};

use crate::context::{clip, event_line};
use crate::filters::strip_think;
use crate::models::{complete, ChatMessage, Provider, ProviderError};
use crate::store::EventStore;
use crate::events::EventKind;

/// Most recent events fed to the segmenter (bounds cost on long sessions).
const MAX_EVENTS: usize = 400;

/// One coherent block of work, with deterministic stats over `[start_idx, end_idx)`.
#[derive(Debug, Clone)]
pub struct Segment {
    pub title: String,
    pub start_idx: usize, // inclusive index into store.events
    pub end_idx: usize,   // exclusive
    pub files: Vec<String>,
    pub lines_added: usize,
    pub lines_removed: usize,
    pub tool_counts: BTreeMap<String, usize>,
    pub errors: usize,
    pub first_ts: Option<DateTime<FixedOffset>>,
    pub last_ts: Option<DateTime<FixedOffset>>,
}

const SEG_SYS: &str = "You analyze a coding agent's activity log and split it into a small number of \
contiguous segments, where each segment is one coherent piece of work. A new segment begins when the \
agent clearly shifts to a different task, file area, or goal.";

const SEG_PROMPT: &str = "Below is a numbered activity log (one line per event). Divide it into segments. \
Return ONLY a JSON array, no prose, where each element is {\"start\": <line number where the segment begins>, \
\"title\": \"<3-6 word description>\"}. The first segment must start at 0. Use as few segments as capture the \
real shifts in work (typically 2-8). Titles name concrete work, e.g. \"Refactor event store\", \"Fix failing tests\".";

/// Segment the session by asking the configured model for boundaries, then computing
/// stats deterministically. On any model/parse failure the session degrades to a
/// single "whole session" block rather than erroring.
pub async fn segment_session(provider: &Provider, store: &EventStore) -> Result<Vec<Segment>, ProviderError> {
    let (messages, map) = segment_request(store);
    if map.is_empty() {
        return Ok(Vec::new());
    }
    let raw = complete(provider, messages).await?;
    Ok(build_segments(parse_boundaries(&raw), &map, store))
}

/// Build the segmentation prompt plus the listing-line → event-index map. Exposed so a
/// caller (e.g. the TUI) can run the request as a detached `'static` stream and then call
/// [`build_segments`] with the (still valid, append-only) `map` once the reply arrives.
pub fn segment_request(store: &EventStore) -> (Vec<ChatMessage>, Vec<usize>) {
    let (listing, map) = segment_input(store, MAX_EVENTS);
    if map.is_empty() {
        return (Vec::new(), map);
    }
    let messages = vec![
        ChatMessage::system(SEG_SYS),
        ChatMessage::user(format!("{SEG_PROMPT}\n\n=== ACTIVITY ===\n{listing}")),
    ];
    (messages, map)
}

/// Render the recent events as a numbered listing for the prompt, plus the parallel map
/// from listing line number (0-based) → index into `store.events`, so a model-returned
/// boundary maps back to a real event.
fn segment_input(store: &EventStore, max_events: usize) -> (String, Vec<usize>) {
    let start = store.events.len().saturating_sub(max_events);
    let mut lines = Vec::new();
    let mut map = Vec::new();
    for (i, ev) in store.events.iter().enumerate().skip(start) {
        let line = event_line(ev);
        if line.is_empty() {
            continue;
        }
        lines.push(format!("{}: {}", map.len(), clip(&line, 200)));
        map.push(i);
    }
    (lines.join("\n"), map)
}

/// Extract the boundary list from a model reply that may be wrapped in `<think>`,
/// markdown fences, or prose. Returns (listing_line_number, tidy_title) pairs.
pub fn parse_boundaries(raw: &str) -> Vec<(usize, String)> {
    let cleaned = strip_think(raw);
    let (Some(s), Some(e)) = (cleaned.find('['), cleaned.rfind(']')) else {
        return Vec::new();
    };
    if e <= s {
        return Vec::new();
    }
    let parsed: Vec<RawBoundary> = serde_json::from_str(&cleaned[s..=e]).unwrap_or_default();
    parsed.into_iter().map(|b| (b.start, tidy_title(&b.title))).collect()
}

#[derive(serde::Deserialize)]
struct RawBoundary {
    start: usize,
    title: String,
}

/// Turn (listing_line, title) boundaries into segments over real event ranges, with
/// deterministic stats. Pure and provider-free, so it is directly testable.
pub fn build_segments(pairs: Vec<(usize, String)>, map: &[usize], store: &EventStore) -> Vec<Segment> {
    if map.is_empty() || store.events.is_empty() {
        return Vec::new();
    }
    let pairs = normalize(pairs, map.len());
    let mut segments = Vec::with_capacity(pairs.len());
    for (k, (line, title)) in pairs.iter().enumerate() {
        let start_idx = map[*line];
        let end_idx = if k + 1 < pairs.len() {
            map[pairs[k + 1].0]
        } else {
            store.events.len()
        };
        segments.push(stats_for(store, start_idx, end_idx, title.clone()));
    }
    segments
}

/// Drop out-of-range boundaries, sort, dedupe, and guarantee a segment starting at 0.
fn normalize(mut pairs: Vec<(usize, String)>, map_len: usize) -> Vec<(usize, String)> {
    pairs.retain(|(s, _)| *s < map_len);
    pairs.sort_by_key(|(s, _)| *s);
    pairs.dedup_by_key(|(s, _)| *s);
    if pairs.is_empty() {
        pairs.push((0, "Session".to_string()));
    } else if pairs[0].0 != 0 {
        pairs.insert(0, (0, "Session start".to_string()));
    }
    pairs
}

fn stats_for(store: &EventStore, start: usize, end: usize, title: String) -> Segment {
    let mut files: Vec<String> = Vec::new();
    let mut lines_added = 0;
    let mut lines_removed = 0;
    let mut tool_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors = 0;
    let mut first_ts = None;
    let mut last_ts = None;
    for ev in &store.events[start..end] {
        if first_ts.is_none() {
            first_ts = Some(ev.ts);
        }
        last_ts = Some(ev.ts);
        match &ev.kind {
            EventKind::ToolCall { name, .. } => {
                *tool_counts.entry(name.clone()).or_default() += 1;
            }
            EventKind::ToolResult { ok, .. } if !ok => errors += 1,
            EventKind::FileEdit { path, added, removed, .. } => {
                if !files.contains(path) {
                    files.push(path.clone());
                }
                lines_added += *added;
                lines_removed += *removed;
            }
            _ => {}
        }
    }
    Segment {
        title,
        start_idx: start,
        end_idx: end,
        files,
        lines_added,
        lines_removed,
        tool_counts,
        errors,
        first_ts,
        last_ts,
    }
}

fn tidy_title(raw: &str) -> String {
    let t = raw.trim().trim_matches('"').trim();
    let t = clip(t, 48);
    if t.is_empty() {
        "Untitled".to_string()
    } else {
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::claude_code::ClaudeCodeSource;
    use std::path::PathBuf;

    fn fixture_store() -> EventStore {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sample_session.jsonl");
        let mut store = EventStore::new();
        store.bulk_add(ClaudeCodeSource::new(path).backfill());
        store
    }

    #[test]
    fn single_segment_covers_all_and_matches_store_totals() {
        let store = fixture_store();
        let (_, map) = segment_input(&store, 1000);
        let segs = build_segments(vec![(0, "All".into())], &map, &store);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].start_idx, 0);
        assert_eq!(segs[0].end_idx, store.events.len());
        // Deterministic stats must agree with the store's own accumulators.
        assert_eq!(segs[0].lines_added, store.lines_added);
        assert_eq!(segs[0].lines_removed, store.lines_removed);
        assert_eq!(segs[0].files.len(), store.files_touched.len());
        assert_eq!(segs[0].errors, store.error_count);
    }

    #[test]
    fn boundary_splits_into_contiguous_segments() {
        let store = fixture_store();
        let (_, map) = segment_input(&store, 1000);
        assert!(map.len() >= 2, "fixture needs at least two listed events");
        let segs = build_segments(vec![(0, "A".into()), (1, "B".into())], &map, &store);
        assert_eq!(segs.len(), 2);
        assert_eq!(segs[0].start_idx, 0);
        assert_eq!(segs[0].end_idx, map[1]);
        assert_eq!(segs[1].start_idx, map[1]);
        assert_eq!(segs[1].end_idx, store.events.len());
    }

    #[test]
    fn parse_boundaries_extracts_json_from_noise() {
        let raw = "Sure!\n```json\n[{\"start\":0,\"title\":\"Setup deps\"},{\"start\":5,\"title\":\"Refactor\"}]\n```";
        let bs = parse_boundaries(raw);
        assert_eq!(bs, vec![(0, "Setup deps".to_string()), (5, "Refactor".to_string())]);
    }

    #[test]
    fn normalize_forces_a_zero_start_and_drops_out_of_range() {
        let out = normalize(vec![(3, "b".into()), (99, "x".into())], 10);
        assert_eq!(out[0].0, 0); // a leading segment is synthesized
        assert!(out.iter().all(|(s, _)| *s < 10)); // 99 dropped
    }

    #[test]
    fn empty_store_yields_no_segments() {
        let store = EventStore::new();
        assert!(build_segments(vec![(0, "x".into())], &[], &store).is_empty());
    }
}
