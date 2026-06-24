//! Comprehension Coverage: a best-effort, deterministic estimate of how much of what the
//! agent produced (its segments) a human has engaged with. Pure accounting over recorded
//! interaction signals — no model, no UI. See docs/comprehension-debt-kpi.md.
//!
//! Signals are recorded as event-index sets so they can be captured continuously (even
//! before segmentation runs) and mapped to segments at compute time. The Tier-2 LLM layer
//! contributes through `segment_overrides`, keeping it additive rather than a rewrite.

use std::collections::{HashMap, HashSet};

use crate::segments::Segment;

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
