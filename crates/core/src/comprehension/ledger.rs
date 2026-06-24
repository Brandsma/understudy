//! Per-session comprehension persistence: append one record per session to a local JSONL
//! ledger so coverage can be trended over time. Local-only (privacy-aware). See
//! docs/comprehension-debt-kpi.md.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::comprehension::{coverage, Interactions};
use crate::segments::Segment;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentScore {
    pub title: String,
    pub score: f32,
}

/// One session's comprehension snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerRecord {
    pub ts: String, // RFC3339
    pub project: String,
    pub branch: String,
    pub session_id: String,
    pub segments: usize,
    pub lines_changed: usize,
    pub coverage: f32,
    pub debt: f32,
    pub per_segment: Vec<SegmentScore>,
}

/// Build a record from the live session state.
pub fn record_from(
    session_id: String,
    project: String,
    branch: String,
    segments: &[Segment],
    ix: &Interactions,
) -> LedgerRecord {
    let report = coverage(segments, ix);
    let per_segment = segments
        .iter()
        .zip(report.per_segment.iter())
        .map(|(s, st)| SegmentScore { title: s.title.clone(), score: st.score() })
        .collect();
    LedgerRecord {
        ts: chrono::Local::now().to_rfc3339(),
        project,
        branch,
        session_id,
        segments: segments.len(),
        lines_changed: report.total_lines,
        coverage: report.coverage,
        debt: report.debt,
        per_segment,
    }
}

/// Ledger location: `$UNDERSTUDY_LEDGER`, else the platform data dir.
pub fn ledger_path() -> PathBuf {
    if let Ok(p) = std::env::var("UNDERSTUDY_LEDGER") {
        return PathBuf::from(p);
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "understudy") {
        return dirs.data_dir().join("comprehension.jsonl");
    }
    PathBuf::from("understudy-comprehension.jsonl")
}

/// Append a record as one JSON line (creating the ledger and its dir as needed).
pub fn append(rec: &LedgerRecord) -> std::io::Result<()> {
    use std::io::Write;
    let path = ledger_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut line = serde_json::to_string(rec).expect("record serializes");
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
    f.write_all(line.as_bytes())
}

/// Read all records (skipping malformed lines). Empty if the ledger is absent.
pub fn read_all() -> Vec<LedgerRecord> {
    let Ok(content) = std::fs::read_to_string(ledger_path()) else {
        return Vec::new();
    };
    content.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Per-project trend: session count, latest and average coverage.
#[derive(Debug, Clone)]
pub struct ProjectTrend {
    pub project: String,
    pub sessions: usize,
    pub latest_coverage: f32,
    pub avg_coverage: f32,
}

/// Aggregate the ledger by project (records are appended chronologically, so the last per
/// project is the latest).
pub fn trends() -> Vec<ProjectTrend> {
    use std::collections::BTreeMap;
    let records = read_all();
    let mut by: BTreeMap<String, Vec<f32>> = BTreeMap::new();
    for r in &records {
        by.entry(r.project.clone()).or_default().push(r.coverage);
    }
    by.into_iter()
        .map(|(project, covs)| {
            let sessions = covs.len();
            let avg = covs.iter().sum::<f32>() / sessions as f32;
            let latest = *covs.last().unwrap();
            ProjectTrend { project, sessions, latest_coverage: latest, avg_coverage: avg }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn seg(lines: usize) -> Segment {
        Segment {
            title: "t".into(),
            start_idx: 0,
            end_idx: 1,
            files: Vec::new(),
            lines_added: lines,
            lines_removed: 0,
            tool_counts: BTreeMap::new(),
            errors: 0,
            first_ts: None,
            last_ts: None,
        }
    }

    #[test]
    fn record_reflects_coverage() {
        let segs = [seg(10)];
        let mut ix = Interactions::new();
        ix.mark_inquiry(0); // understand the only segment
        let rec = record_from("s1".into(), "proj".into(), "main".into(), &segs, &ix);
        assert_eq!(rec.segments, 1);
        assert_eq!(rec.lines_changed, 10);
        assert!((rec.coverage - 1.0).abs() < 1e-6);
        assert_eq!(rec.per_segment[0].score, 1.0);
    }

    #[test]
    fn append_read_and_trend_round_trip() {
        let tmp = std::env::temp_dir().join(format!("understudy_ledger_{}.jsonl", std::process::id()));
        std::env::set_var("UNDERSTUDY_LEDGER", &tmp);
        let _ = std::fs::remove_file(&tmp);

        let mk = |project: &str, cov: f32| LedgerRecord {
            ts: "2026-06-24T00:00:00Z".into(),
            project: project.into(),
            branch: "main".into(),
            session_id: "s".into(),
            segments: 1,
            lines_changed: 10,
            coverage: cov,
            debt: 1.0 - cov,
            per_segment: vec![],
        };
        append(&mk("proj", 0.2)).unwrap();
        append(&mk("proj", 0.8)).unwrap(); // later session, higher coverage
        append(&mk("other", 0.5)).unwrap();

        assert_eq!(read_all().len(), 3);
        let trends = trends();
        let proj = trends.iter().find(|t| t.project == "proj").unwrap();
        assert_eq!(proj.sessions, 2);
        assert!((proj.latest_coverage - 0.8).abs() < 1e-6); // last appended
        assert!((proj.avg_coverage - 0.5).abs() < 1e-6); // (0.2 + 0.8) / 2

        let _ = std::fs::remove_file(&tmp);
        std::env::remove_var("UNDERSTUDY_LEDGER");
    }
}
