//! Comprehension Coverage: a best-effort, deterministic estimate of how much of what the
//! agent produced (its segments) a human has engaged with. Pure accounting over recorded
//! interaction signals — no model, no UI. See docs/comprehension-debt-kpi.md.
//!
//! Signals are recorded as event-index sets so they can be captured continuously (even
//! before segmentation runs) and mapped to segments at compute time. The Tier-2 LLM layer
//! contributes through `segment_overrides`, keeping it additive rather than a rewrite.

use std::collections::{HashMap, HashSet};

use crate::context::event_line;
use crate::filters::strip_think;
use crate::models::ChatMessage;
use crate::segments::Segment;
use crate::store::EventStore;

/// How well a single segment is understood. Ordered Unseen < Skimmed < Understood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SegmentState {
    Unseen,
    Skimmed,
    Understood,
}

impl SegmentState {
    /// Contribution to coverage.
    pub fn score(self) -> f32 {
        match self {
            SegmentState::Unseen => 0.0,
            SegmentState::Skimmed => 0.5,
            SegmentState::Understood => 1.0,
        }
    }

    /// Glyph for the segments timeline.
    pub fn glyph(self) -> char {
        match self {
            SegmentState::Unseen => '○',
            SegmentState::Skimmed => '◐',
            SegmentState::Understood => '●',
        }
    }

    fn rank(self) -> u8 {
        match self {
            SegmentState::Unseen => 0,
            SegmentState::Skimmed => 1,
            SegmentState::Understood => 2,
        }
    }
}

/// Coverage band, using the research thresholds (delegation < 40% < inquiry < 65%).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Band {
    Low,
    Mid,
    High,
}

/// Raw comprehension signals collected by the cockpit, keyed by event index (or, for the
/// Tier-2 overrides, by segment index).
#[derive(Debug, Default, Clone)]
pub struct Interactions {
    /// Events pinned / opened in Detail / navigated to (passive reading → Skimmed).
    pub seen_events: HashSet<usize>,
    /// Events that were the focus of a genuine question (Tier-1 inquiry → Understood).
    pub inquiry_events: HashSet<usize>,
    /// Tier-2 segment-level upgrades (LLM inquiry tags, explain-back passes).
    pub segment_overrides: HashMap<usize, SegmentState>,
}

impl Interactions {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mark_seen(&mut self, event_idx: usize) {
        self.seen_events.insert(event_idx);
    }

    pub fn mark_inquiry(&mut self, event_idx: usize) {
        self.inquiry_events.insert(event_idx);
    }

    /// Raise a segment to at least `state` (never downgrades).
    pub fn set_override(&mut self, segment_idx: usize, state: SegmentState) {
        let slot = self.segment_overrides.entry(segment_idx).or_insert(state);
        if state.rank() > slot.rank() {
            *slot = state;
        }
    }

    /// Apply Tier-2 question tags: an *inquiry* raises its segments to Understood, a
    /// *delegation* to at most Skimmed, and *other* is ignored. `n_segments` bounds the
    /// valid indices (the model may hallucinate).
    pub fn apply_tags(&mut self, tags: &QuestionTags, n_segments: usize) {
        let state = match tags.kind {
            InquiryKind::Inquiry => SegmentState::Understood,
            InquiryKind::Delegation => SegmentState::Skimmed,
            InquiryKind::Other => return,
        };
        for &idx in &tags.segments {
            if idx < n_segments {
                self.set_override(idx, state);
            }
        }
    }
}

// ---- Tier-2: LLM question tagging (attribution + inquiry/delegation) --------- //

/// How a question engages with the work — the research's inquiry-vs-delegation axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InquiryKind {
    Inquiry,
    Delegation,
    Other,
}

/// Which segments a question concerns, and how it engages them.
#[derive(Debug, Clone)]
pub struct QuestionTags {
    pub segments: Vec<usize>,
    pub kind: InquiryKind,
}

const TAG_SYS: &str =
    "You classify a question an observer asked while watching a coding agent work.";
const TAG_PROMPT: &str = "Below is a numbered list of work segments, then a QUESTION. Identify which segment \
number(s) the question is about, and classify the question as \"inquiry\" (seeking to understand why or how — \
reasoning, tradeoffs, design), \"delegation\" (asking to produce, change, or fix something), or \"other\". \
Return ONLY JSON: {\"segments\": [<numbers>], \"kind\": \"inquiry|delegation|other\"}.";

/// Build the tagging prompt for `question` against the current `segments` (0-indexed to
/// match the array). Exposed so the caller can run it as a detached stream.
pub fn tag_request(segments: &[Segment], question: &str) -> Vec<ChatMessage> {
    let listing = segments
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{i}: {}", s.title))
        .collect::<Vec<_>>()
        .join("\n");
    vec![
        ChatMessage::system(TAG_SYS),
        ChatMessage::user(format!(
            "{TAG_PROMPT}\n\n=== SEGMENTS ===\n{listing}\n\n=== QUESTION ===\n{question}"
        )),
    ]
}

/// Parse a tagging reply (tolerant of `<think>`, fences, prose). Returns None if no usable
/// JSON object is found.
pub fn parse_tags(raw: &str) -> Option<QuestionTags> {
    let cleaned = strip_think(raw);
    let (s, e) = (cleaned.find('{')?, cleaned.rfind('}')?);
    if e <= s {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        segments: Vec<usize>,
        #[serde(default)]
        kind: String,
    }
    let r: Raw = serde_json::from_str(&cleaned[s..=e]).ok()?;
    let kind = match r.kind.to_lowercase().as_str() {
        "inquiry" => InquiryKind::Inquiry,
        "delegation" => InquiryKind::Delegation,
        _ => InquiryKind::Other,
    };
    Some(QuestionTags { segments: r.segments, kind })
}

// ---- Tier-2: explain-back check (the only ~ground-truth comprehension signal) - //

/// Grade of an observer's explain-back answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Partial,
    Fail,
}

/// The events of one segment rendered as a compact transcript (bounded), preserving lines.
fn segment_activity(store: &EventStore, seg: &Segment) -> String {
    let end = seg.end_idx.min(store.events.len());
    let start = seg.start_idx.min(end);
    let mut text = store.events[start..end]
        .iter()
        .map(event_line)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if text.chars().count() > 4000 {
        text = text.chars().take(4000).collect();
    }
    text
}

const EXPLAIN_SYS: &str = "You quiz an observer on their understanding of a coding agent's work.";
const EXPLAIN_PROMPT: &str = "Below is one segment of the agent's activity. Ask the observer ONE concise \
question that tests whether they understand WHY the agent did this — the reasoning or design choice, not \
trivia. Output only the question, no preamble.";

/// Prompt the model to pose one "why" question about a segment. The reply is free text.
pub fn explain_request(store: &EventStore, seg: &Segment) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(EXPLAIN_SYS),
        ChatMessage::user(format!(
            "{EXPLAIN_PROMPT}\n\n=== SEGMENT: {} ===\n{}",
            seg.title,
            segment_activity(store, seg)
        )),
    ]
}

const GRADE_SYS: &str = "You grade an observer's explanation of a coding agent's work against what actually happened.";
const GRADE_PROMPT: &str = "Given the segment activity, the question asked, and the observer's answer, judge \
whether the answer shows genuine understanding of WHY the agent did this. Reply ONLY with JSON: \
{\"verdict\": \"pass|partial|fail\", \"note\": \"<one short sentence>\"}.";

/// Prompt the model to grade an explain-back answer against the segment's activity.
pub fn grade_request(store: &EventStore, seg: &Segment, question: &str, answer: &str) -> Vec<ChatMessage> {
    vec![
        ChatMessage::system(GRADE_SYS),
        ChatMessage::user(format!(
            "{GRADE_PROMPT}\n\n=== SEGMENT: {} ===\n{}\n\n=== QUESTION ===\n{}\n\n=== OBSERVER ANSWER ===\n{}",
            seg.title,
            segment_activity(store, seg),
            question,
            answer
        )),
    ]
}

/// Parse a grading reply (tolerant of `<think>`, fences, prose).
pub fn parse_verdict(raw: &str) -> Option<(Verdict, String)> {
    let cleaned = strip_think(raw);
    let (s, e) = (cleaned.find('{')?, cleaned.rfind('}')?);
    if e <= s {
        return None;
    }
    #[derive(serde::Deserialize)]
    struct Raw {
        #[serde(default)]
        verdict: String,
        #[serde(default)]
        note: String,
    }
    let r: Raw = serde_json::from_str(&cleaned[s..=e]).ok()?;
    let verdict = match r.verdict.to_lowercase().as_str() {
        "pass" => Verdict::Pass,
        "partial" => Verdict::Partial,
        _ => Verdict::Fail,
    };
    Some((verdict, r.note))
}

/// A line-weighted comprehension snapshot over a set of segments.
#[derive(Debug, Clone)]
pub struct CoverageReport {
    pub coverage: f32, // 0..=1, line-weighted
    pub debt: f32,     // 1 - coverage
    pub total_lines: usize,
    pub unread_lines: usize,        // lines in Unseen segments
    pub unreviewed_segments: usize, // segments not yet Understood
    pub per_segment: Vec<SegmentState>,
}

impl CoverageReport {
    pub fn band(&self) -> Band {
        if self.coverage < 0.40 {
            Band::Low
        } else if self.coverage < 0.65 {
            Band::Mid
        } else {
            Band::High
        }
    }

    pub fn percent(&self) -> u8 {
        (self.coverage * 100.0).round() as u8
    }
}

/// Compute line-weighted Comprehension Coverage over `segments` given the recorded
/// `interactions`. With no segments there is nothing produced, so coverage is 1.0 (no debt).
pub fn coverage(segments: &[Segment], ix: &Interactions) -> CoverageReport {
    let mut per_segment = Vec::with_capacity(segments.len());
    let mut weighted_score = 0.0f32;
    let mut total_weight = 0.0f32;
    let mut total_lines = 0usize;
    let mut unread_lines = 0usize;
    let mut unreviewed_segments = 0usize;

    for (i, seg) in segments.iter().enumerate() {
        let lines = seg.lines_added + seg.lines_removed;
        let state = resolve_state(i, seg, ix);
        // A zero-line segment still carries minimal weight so reading it isn't "free".
        let weight = lines.max(1) as f32;

        weighted_score += weight * state.score();
        total_weight += weight;
        total_lines += lines;
        if state != SegmentState::Understood {
            unreviewed_segments += 1;
        }
        if state == SegmentState::Unseen {
            unread_lines += lines;
        }
        per_segment.push(state);
    }

    let coverage = if total_weight > 0.0 { weighted_score / total_weight } else { 1.0 };
    CoverageReport {
        coverage,
        debt: 1.0 - coverage,
        total_lines,
        unread_lines,
        unreviewed_segments,
        per_segment,
    }
}

/// Resolve one segment's state: the strongest of its event-range signals and any Tier-2
/// override. Signal sets are small (human interactions), so we scan them against the range.
fn resolve_state(idx: usize, seg: &Segment, ix: &Interactions) -> SegmentState {
    let in_range = |e: &usize| *e >= seg.start_idx && *e < seg.end_idx;
    let mut state = SegmentState::Unseen;
    if ix.seen_events.iter().any(in_range) {
        state = SegmentState::Skimmed;
    }
    if ix.inquiry_events.iter().any(in_range) {
        state = SegmentState::Understood;
    }
    if let Some(&ov) = ix.segment_overrides.get(&idx) {
        if ov.rank() > state.rank() {
            state = ov;
        }
    }
    state
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn seg(start: usize, end: usize, added: usize, removed: usize) -> Segment {
        Segment {
            title: "x".into(),
            start_idx: start,
            end_idx: end,
            files: Vec::new(),
            lines_added: added,
            lines_removed: removed,
            tool_counts: BTreeMap::new(),
            errors: 0,
            first_ts: None,
            last_ts: None,
        }
    }

    #[test]
    fn empty_segments_is_fully_covered() {
        let r = coverage(&[], &Interactions::new());
        assert_eq!(r.coverage, 1.0);
        assert_eq!(r.debt, 0.0);
    }

    #[test]
    fn unseen_segments_are_full_debt() {
        let segs = [seg(0, 5, 10, 0), seg(5, 9, 6, 0)];
        let r = coverage(&segs, &Interactions::new());
        assert_eq!(r.coverage, 0.0);
        assert_eq!(r.unreviewed_segments, 2);
        assert_eq!(r.unread_lines, 16);
        assert!(r.per_segment.iter().all(|&s| s == SegmentState::Unseen));
    }

    #[test]
    fn inquiry_beats_seen_which_beats_unseen() {
        let segs = [seg(0, 10, 10, 0)];
        let mut ix = Interactions::new();
        ix.mark_seen(3);
        assert_eq!(coverage(&segs, &ix).per_segment[0], SegmentState::Skimmed);
        ix.mark_inquiry(3);
        assert_eq!(coverage(&segs, &ix).per_segment[0], SegmentState::Understood);
    }

    #[test]
    fn coverage_is_line_weighted() {
        // A big unseen block dominates a small understood one.
        let segs = [seg(0, 10, 100, 0), seg(10, 12, 2, 0)];
        let mut ix = Interactions::new();
        ix.mark_inquiry(10); // understand only the small segment
        let r = coverage(&segs, &ix);
        assert!(r.coverage < 0.05, "coverage was {}", r.coverage);
        assert_eq!(r.unread_lines, 100); // the big block
        assert_eq!(r.unreviewed_segments, 1);
        assert_eq!(r.per_segment[0], SegmentState::Unseen);
        assert_eq!(r.per_segment[1], SegmentState::Understood);
    }

    #[test]
    fn override_upgrades_and_never_downgrades() {
        let segs = [seg(0, 10, 10, 0)];
        let mut ix = Interactions::new();
        ix.set_override(0, SegmentState::Understood);
        assert_eq!(coverage(&segs, &ix).per_segment[0], SegmentState::Understood);
        ix.set_override(0, SegmentState::Skimmed); // weaker — ignored
        assert_eq!(coverage(&segs, &ix).per_segment[0], SegmentState::Understood);
    }

    #[test]
    fn override_merges_with_event_signals_as_max() {
        let segs = [seg(0, 10, 10, 0)];
        let mut ix = Interactions::new();
        ix.mark_seen(2); // Skimmed from events
        ix.set_override(0, SegmentState::Understood); // Tier-2 wins
        assert_eq!(coverage(&segs, &ix).per_segment[0], SegmentState::Understood);
    }

    #[test]
    fn parse_tags_reads_object_and_classifies() {
        let t = parse_tags("```json\n{\"segments\":[0,2],\"kind\":\"Inquiry\"}\n```").unwrap();
        assert_eq!(t.segments, vec![0, 2]);
        assert_eq!(t.kind, InquiryKind::Inquiry);
        assert_eq!(parse_tags("{\"segments\":[1],\"kind\":\"delegation\"}").unwrap().kind, InquiryKind::Delegation);
        assert_eq!(parse_tags("{\"segments\":[],\"kind\":\"chitchat\"}").unwrap().kind, InquiryKind::Other);
        assert!(parse_tags("no json here").is_none());
    }

    #[test]
    fn apply_tags_upgrades_by_kind_and_drops_out_of_range() {
        let segs = [seg(0, 5, 10, 0), seg(5, 9, 6, 0)];
        let mut ix = Interactions::new();
        ix.apply_tags(&QuestionTags { segments: vec![0, 99], kind: InquiryKind::Inquiry }, segs.len());
        ix.apply_tags(&QuestionTags { segments: vec![1], kind: InquiryKind::Delegation }, segs.len());
        let r = coverage(&segs, &ix);
        assert_eq!(r.per_segment[0], SegmentState::Understood); // inquiry; index 99 ignored
        assert_eq!(r.per_segment[1], SegmentState::Skimmed); // delegation caps at skim
    }

    #[test]
    fn apply_tags_other_is_ignored() {
        let segs = [seg(0, 5, 10, 0)];
        let mut ix = Interactions::new();
        ix.apply_tags(&QuestionTags { segments: vec![0], kind: InquiryKind::Other }, segs.len());
        assert_eq!(coverage(&segs, &ix).per_segment[0], SegmentState::Unseen);
    }

    #[test]
    fn tag_request_lists_segments_and_question() {
        let segs = [seg(0, 5, 10, 0)];
        let msgs = tag_request(&segs, "why this approach?");
        assert_eq!(msgs.len(), 2);
        assert!(msgs[1].content.contains("=== QUESTION ===") && msgs[1].content.contains("why this approach?"));
    }

    #[test]
    fn explain_and_grade_requests_carry_context() {
        let store = EventStore::new();
        let s = seg(0, 0, 0, 0);
        let ex = explain_request(&store, &s);
        assert_eq!(ex.len(), 2);
        assert!(ex[1].content.contains("=== SEGMENT"));
        let gr = grade_request(&store, &s, "why X?", "because Y");
        assert!(gr[1].content.contains("why X?"));
        assert!(gr[1].content.contains("because Y"));
        assert!(gr[1].content.contains("OBSERVER ANSWER"));
    }

    #[test]
    fn parse_verdict_reads_all_grades() {
        assert_eq!(parse_verdict("{\"verdict\":\"pass\",\"note\":\"good\"}").unwrap().0, Verdict::Pass);
        assert_eq!(parse_verdict("```\n{\"verdict\":\"Partial\"}\n```").unwrap().0, Verdict::Partial);
        assert_eq!(parse_verdict("{\"verdict\":\"nope\"}").unwrap().0, Verdict::Fail);
        assert!(parse_verdict("no json at all").is_none());
    }

    #[test]
    fn bands_and_percent_follow_thresholds() {
        let low = CoverageReport { coverage: 0.30, debt: 0.70, total_lines: 0, unread_lines: 0, unreviewed_segments: 0, per_segment: vec![] };
        let mid = CoverageReport { coverage: 0.50, ..low.clone() };
        let high = CoverageReport { coverage: 0.80, ..low.clone() };
        assert_eq!(low.band(), Band::Low);
        assert_eq!(mid.band(), Band::Mid);
        assert_eq!(high.band(), Band::High);
        assert_eq!(high.percent(), 80);
    }
}
