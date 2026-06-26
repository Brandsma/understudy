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

/// Events sent to the segmenter per incremental batch. Small batches keep each model call
/// cheap and fast and let segments stream in progressively as a session grows, at the cost of
/// more (sequential) calls. Each batch may only show part of a session, so the prompt tells the
/// model the batch can be a continuation of the previous segment.
pub const BATCH_SIZE: usize = 20;

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

const SEG_SYS: &str = "You are an expert at reading a coding agent's activity log and breaking it into the \
distinct pieces of work it represents. Each segment is one coherent task or goal. A new segment begins \
whenever the agent shifts focus — to a different feature or bug, a different area of the codebase, or a \
different phase of work (e.g. exploring → implementing, implementing → fixing tests, one feature → the next).";

/// The per-batch user prompt. A batch is a numbered slice that may only show part of a session,
/// so `prev_title` names the segment the slice might continue (None only for the first slice,
/// which must open a segment at line 0).
fn seg_batch_prompt(prev_title: Option<&str>) -> String {
    let context = match prev_title {
        Some(t) => format!(
            "This slice may begin in the MIDDLE of an ongoing session: the events before line 0 are \
already grouped into a segment titled \"{t}\". If the first events here continue that same work, do \
NOT emit a boundary for them — only emit a boundary where genuinely new work begins. If the ENTIRE \
slice continues that segment, return an empty array [].",
        ),
        None => "This is the START of the session, so your first boundary must be at line 0.".to_string(),
    };
    format!(
        "Below is a numbered slice of a coding agent's activity log, one line per event. {context}\n\
Mark where the work shifts to a NEW coherent piece of work — a different feature or bug, a different \
area of the codebase, or a different phase (e.g. exploring \u{2192} implementing, implementing \u{2192} \
fixing tests). A short slice may be entirely one piece of work; only add a boundary at a real shift, \
not at arbitrary intervals.\n\
Rules:\n\
- Return ONLY a JSON array, no prose and no markdown fences. Each element is {{\"start\": <0-based line \
number where a new segment begins>, \"title\": \"<3-6 word description of that work>\"}}. An empty array \
[] is allowed.\n\
- Boundaries must be in order and non-overlapping.\n\
- Title each segment with the concrete work it contains, e.g. \"Refactor event store\", \"Fix failing \
port tests\", \"Add segmentation cache\". Never use vague catch-all titles like \"Session\", \"Coding\", \
or \"Misc\"."
    )
}

/// Segment the session by asking the configured model for boundaries in batches of
/// [`BATCH_SIZE`] events, then computing stats deterministically. Each batch is told whether it
/// continues the previous segment, so leading events can fold into it instead of forcing a new
/// block. On any model/parse failure the session degrades to a single block rather than erroring.
pub async fn segment_session(provider: &Provider, store: &EventStore) -> Result<Vec<Segment>, ProviderError> {
    let n = store.events.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let mut starts: Vec<(usize, String)> = Vec::new();
    let mut pos = 0;
    while pos < n {
        let prev_title = starts.last().map(|(_, t)| t.clone());
        let (messages, map) = segment_batch_request(store, pos, prev_title.as_deref());
        if !map.is_empty() {
            let raw = complete(provider, messages).await?;
            let mut batch = batch_starts(&raw, &map, prev_title.is_some());
            starts.append(&mut batch);
        }
        pos += BATCH_SIZE;
    }
    if starts.is_empty() {
        starts.push((0, "Unsegmented activity".to_string()));
    }
    Ok(build_segments_from_starts(starts, store))
}

/// Build the prompt + listing-line → event-index map for one incremental batch: up to
/// [`BATCH_SIZE`] events starting at `start_idx`. `prev_title` is the segment the batch may be
/// continuing (None for the first batch, which must open a segment at line 0). Exposed so the
/// TUI can run the request as a detached `'static` stream and assemble segments once it arrives.
pub fn segment_batch_request(
    store: &EventStore,
    start_idx: usize,
    prev_title: Option<&str>,
) -> (Vec<ChatMessage>, Vec<usize>) {
    let end = (start_idx + BATCH_SIZE).min(store.events.len());
    let (listing, map) = segment_listing(store, start_idx, end);
    if map.is_empty() {
        return (Vec::new(), map);
    }
    let messages = vec![
        ChatMessage::system(SEG_SYS),
        ChatMessage::user(format!("{}\n\n=== ACTIVITY ===\n{listing}", seg_batch_prompt(prev_title))),
    ];
    (messages, map)
}

/// Render events `[start, end)` as a numbered listing for the prompt, plus the parallel map
/// from listing line number (0-based) → index into `store.events`, so a model-returned boundary
/// maps back to a real event. Events with no renderable line are skipped (and not mapped).
fn segment_listing(store: &EventStore, start: usize, end: usize) -> (String, Vec<usize>) {
    let mut lines = Vec::new();
    let mut map = Vec::new();
    for (i, ev) in store.events.iter().enumerate().take(end).skip(start) {
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

/// Extract every *complete* `{"start":..,"title":..}` object from a reply that may still be
/// streaming (no closing `]` yet). Used to surface segmentation progress live as the model
/// emits each boundary, before the full array is available to [`parse_boundaries`].
pub fn parse_partial_boundaries(raw: &str) -> Vec<(usize, String)> {
    let cleaned = strip_think(raw);
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(open) = cleaned[i..].find('{') {
        let start = i + open;
        let Some(close_rel) = cleaned[start..].find('}') else {
            break; // object still arriving — stop here
        };
        let end = start + close_rel;
        if let Ok(b) = serde_json::from_str::<RawBoundary>(&cleaned[start..=end]) {
            out.push((b.start, tidy_title(&b.title)));
        }
        i = end + 1;
    }
    out
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
    build_segments_from_starts(boundaries_to_starts(pairs, map), store)
}

/// Map (listing_line, title) boundaries through `map` to absolute event-index starts,
/// guaranteeing the window's first listed event is itself a boundary (so the segments
/// cover the whole listed range contiguously).
pub fn boundaries_to_starts(pairs: Vec<(usize, String)>, map: &[usize]) -> Vec<(usize, String)> {
    normalize(pairs, map.len()).into_iter().map(|(line, title)| (map[line], title)).collect()
}

/// Parse a model reply into absolute event-index starts for one batch. `has_prev` selects the
/// continuation semantics: with a previous segment, no synthetic start-at-0 is injected, so
/// leading events with no boundary fold into the previous (frozen) segment; without one (the
/// first batch) a start at line 0 is guaranteed.
pub fn batch_starts(raw: &str, map: &[usize], has_prev: bool) -> Vec<(usize, String)> {
    let pairs = parse_boundaries(raw);
    if has_prev {
        batch_boundaries_to_starts(pairs, map)
    } else {
        boundaries_to_starts(pairs, map)
    }
}

/// Like [`boundaries_to_starts`] but without the synthetic start-at-0 (used for continuation
/// batches). Out-of-range lines are dropped; starts are sorted and deduped.
pub fn batch_boundaries_to_starts(pairs: Vec<(usize, String)>, map: &[usize]) -> Vec<(usize, String)> {
    let mut pairs: Vec<(usize, String)> = pairs.into_iter().filter(|(s, _)| *s < map.len()).collect();
    pairs.sort_by_key(|(s, _)| *s);
    pairs.dedup_by_key(|(s, _)| *s);
    pairs.into_iter().map(|(line, title)| (map[line], title)).collect()
}

/// Build contiguous segments from absolute event-index starts (each segment runs to the
/// next start, the last to the end of the store). Starts are sorted/deduped/range-checked;
/// coverage begins at the smallest start. Used for both fresh and incremental segmentation,
/// and to rebuild segments from a persisted cache.
pub fn build_segments_from_starts(mut starts: Vec<(usize, String)>, store: &EventStore) -> Vec<Segment> {
    let n = store.events.len();
    if n == 0 {
        return Vec::new();
    }
    starts.retain(|(s, _)| *s < n);
    starts.sort_by_key(|(s, _)| *s);
    starts.dedup_by_key(|(s, _)| *s);
    if starts.is_empty() {
        return Vec::new();
    }
    let mut segments = Vec::with_capacity(starts.len());
    for (k, (start, title)) in starts.iter().enumerate() {
        let end = if k + 1 < starts.len() { starts[k + 1].0 } else { n };
        segments.push(stats_for(store, *start, end, title.clone()));
    }
    segments
}

/// Drop out-of-range boundaries, sort, dedupe, and guarantee a segment starting at 0.
fn normalize(mut pairs: Vec<(usize, String)>, map_len: usize) -> Vec<(usize, String)> {
    pairs.retain(|(s, _)| *s < map_len);
    pairs.sort_by_key(|(s, _)| *s);
    pairs.dedup_by_key(|(s, _)| *s);
    // Fallback only when the model returned nothing usable; label it honestly rather than as
    // a real "Session" block (the prompt asks for >= 2 concrete segments).
    if pairs.is_empty() {
        pairs.push((0, "Unsegmented activity".to_string()));
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
    use crate::sources::Source;
    use std::path::PathBuf;

    fn fixture_store() -> EventStore {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/sample_session.jsonl");
        let mut store = EventStore::new();
        store.bulk_add(ClaudeCodeSource::new(path).backfill());
        store
    }

    /// Test shim: render the whole store as a listing (the batch lister bounds real runs).
    fn segment_input(store: &EventStore, start: usize, _max: usize) -> (String, Vec<usize>) {
        segment_listing(store, start, store.events.len())
    }

    #[test]
    fn single_segment_covers_all_and_matches_store_totals() {
        let store = fixture_store();
        let (_, map) = segment_input(&store, 0, 1000);
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
    fn build_from_starts_merges_frozen_and_new_contiguously() {
        // Mimics an incremental pass: frozen block at 0, two new blocks from the model.
        let store = fixture_store();
        let n = store.events.len();
        assert!(n >= 6, "fixture needs enough events");
        let segs = build_segments_from_starts(
            vec![(0, "frozen".into()), (5, "new B".into()), (2, "new A".into())], // unsorted on purpose
            &store,
        );
        assert_eq!(segs.len(), 3);
        // Sorted, contiguous, and covering the whole store.
        assert_eq!((segs[0].start_idx, segs[0].end_idx), (0, 2));
        assert_eq!((segs[1].start_idx, segs[1].end_idx), (2, 5));
        assert_eq!((segs[2].start_idx, segs[2].end_idx), (5, n));
        assert_eq!(segs[0].title, "frozen");
    }

    #[test]
    fn boundary_splits_into_contiguous_segments() {
        let store = fixture_store();
        let (_, map) = segment_input(&store, 0, 1000);
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

    #[test]
    fn parse_partial_boundaries_reads_objects_before_array_closes() {
        // Mid-stream: array opened, two objects complete, a third still arriving.
        let raw = "[{\"start\":0,\"title\":\"Setup deps\"},{\"start\":5,\"title\":\"Refactor\"},{\"start\":9,\"tit";
        let bs = parse_partial_boundaries(raw);
        assert_eq!(bs, vec![(0, "Setup deps".to_string()), (5, "Refactor".to_string())]);
    }

    #[test]
    fn parse_boundaries_returns_empty_on_garbage() {
        assert!(parse_boundaries("I could not produce JSON, sorry.").is_empty());
        assert!(parse_boundaries("").is_empty());
        assert!(parse_boundaries("[not valid json]").is_empty());
    }

    #[test]
    fn tidy_title_trims_quotes_clips_and_defaults() {
        assert_eq!(tidy_title("  \"Refactor store\"  "), "Refactor store");
        assert_eq!(tidy_title(""), "Untitled");
        assert_eq!(tidy_title("\"\""), "Untitled");
        assert!(tidy_title(&"x".repeat(100)).chars().count() <= 48);
    }

    #[test]
    fn segment_batch_request_builds_prompt_and_index_map() {
        let store = fixture_store();
        let (messages, map) = segment_batch_request(&store, 0, None);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].role, "system");
        assert!(messages[1].content.contains("=== ACTIVITY ==="));
        assert!(messages[1].content.contains("START of the session")); // None → first-batch framing
        // Map has one entry per listed (non-empty) event, each a valid index, capped to a batch.
        assert!(!map.is_empty());
        assert!(map.len() <= BATCH_SIZE);
        assert!(map.iter().all(|&i| i < store.events.len()));
    }

    #[test]
    fn batch_prompt_frames_continuation_vs_first_slice() {
        let cont = seg_batch_prompt(Some("Refactor store"));
        assert!(cont.contains("Refactor store"));
        assert!(cont.contains("empty array")); // may fold entirely into the previous segment
        assert!(seg_batch_prompt(None).contains("START of the session"));
    }

    #[test]
    fn batch_starts_continuation_skips_forced_zero() {
        // listing-line → event-index map for a mid-session batch.
        let map = vec![10, 11, 12, 13];
        // Continuation: a boundary at line 2 only; leading lines fold into the previous segment.
        let cont = batch_starts("[{\"start\":2,\"title\":\"New work\"}]", &map, true);
        assert_eq!(cont, vec![(12, "New work".to_string())]);
        // Whole batch continues the previous segment → no new starts at all.
        assert!(batch_starts("[]", &map, true).is_empty());
        // First batch (no previous segment) still guarantees a leading start at the window head.
        let first = batch_starts("[{\"start\":2,\"title\":\"New work\"}]", &map, false);
        assert_eq!(first[0].0, map[0]);
    }

    #[test]
    fn split_preserves_total_tool_counts() {
        let store = fixture_store();
        let (_, map) = segment_input(&store, 0, 1000);
        let segs = build_segments(vec![(0, "A".into()), (1, "B".into())], &map, &store);
        // Tool counts summed across segments equal the store's overall histogram.
        let mut combined: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        for s in &segs {
            for (k, v) in &s.tool_counts {
                *combined.entry(k.clone()).or_default() += v;
            }
        }
        let expected: std::collections::BTreeMap<String, usize> =
            store.tool_counts.iter().map(|(k, v)| (k.clone(), *v)).collect();
        assert_eq!(combined, expected);
    }
}
