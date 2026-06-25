//! Per-session segmentation + summary cache. Reopening a session shouldn't re-run the
//! segmenter or summarizer from scratch: we persist the model-determined boundaries (as
//! event-index starts) plus the latest "what & why" summary, keyed by session id. Segment
//! stats are *not* stored — they're rebuilt deterministically from the backfilled events.
//!
//! Local-only, one small JSON file per session under the understudy data dir (override with
//! `$UNDERSTUDY_CACHE_DIR`).

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::segments::Segment;

/// One persisted segment boundary: where it begins (absolute event index) and its title.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedSegment {
    pub start: usize,
    pub title: String,
}

/// A session's cached comprehension state. `watermark` is the event count covered by these
/// segments, so on reopen we segment only events beyond it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCache {
    pub session_id: String,
    pub watermark: usize,
    #[serde(default)]
    pub segments: Vec<CachedSegment>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub summary_len: usize, // event count the summary covered
}

impl SessionCache {
    pub fn from_state(
        session_id: String,
        watermark: usize,
        segments: &[Segment],
        summary: String,
        summary_len: usize,
    ) -> Self {
        SessionCache {
            session_id,
            watermark,
            segments: segments
                .iter()
                .map(|s| CachedSegment { start: s.start_idx, title: s.title.clone() })
                .collect(),
            summary,
            summary_len,
        }
    }

    /// The cached boundaries as (start, title) pairs for `build_segments_from_starts`.
    pub fn starts(&self) -> Vec<(usize, String)> {
        self.segments.iter().map(|s| (s.start, s.title.clone())).collect()
    }
}

/// Cache directory: `$UNDERSTUDY_CACHE_DIR`, else `sessions/` under the platform data dir.
pub fn cache_dir() -> PathBuf {
    if let Ok(p) = std::env::var("UNDERSTUDY_CACHE_DIR") {
        return PathBuf::from(p);
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "understudy") {
        return dirs.data_dir().join("sessions");
    }
    PathBuf::from(".understudy/sessions")
}

fn cache_path(session_id: &str) -> PathBuf {
    // Session ids are UUIDs, but guard against any path-unsafe character just in case.
    let safe: String = session_id.chars().map(|c| if c.is_alphanumeric() || c == '-' { c } else { '_' }).collect();
    cache_dir().join(format!("{safe}.json"))
}

/// Load a session's cache, if present and parseable.
pub fn load(session_id: &str) -> Option<SessionCache> {
    let content = std::fs::read_to_string(cache_path(session_id)).ok()?;
    serde_json::from_str(&content).ok()
}

/// Write a session's cache (creating the directory as needed). Best-effort.
pub fn save(cache: &SessionCache) -> std::io::Result<()> {
    let path = cache_path(&cache.session_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cache).expect("cache serializes");
    std::fs::write(&path, json)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_round_trip() {
        let dir = std::env::temp_dir().join(format!("understudy_cache_{}", std::process::id()));
        std::env::set_var("UNDERSTUDY_CACHE_DIR", &dir);
        let _ = std::fs::remove_dir_all(&dir);

        let cache = SessionCache {
            session_id: "abc-123".into(),
            watermark: 42,
            segments: vec![
                CachedSegment { start: 0, title: "Setup".into() },
                CachedSegment { start: 17, title: "Refactor store".into() },
            ],
            summary: "doing the thing".into(),
            summary_len: 42,
        };
        save(&cache).unwrap();

        let got = load("abc-123").expect("cache loads");
        assert_eq!(got.watermark, 42);
        assert_eq!(got.segments.len(), 2);
        assert_eq!(got.starts(), vec![(0, "Setup".into()), (17, "Refactor store".into())]);
        assert_eq!(got.summary, "doing the thing");

        assert!(load("does-not-exist").is_none());

        let _ = std::fs::remove_dir_all(&dir);
        std::env::remove_var("UNDERSTUDY_CACHE_DIR");
    }
}
