//! GitHub Copilot CLI source adapter. The Copilot CLI stores sessions in a SQLite database
//! (`~/.copilot/session-store.db`): a `sessions` table (id, cwd, branch, summary, timestamps)
//! and a `turns` table holding one row per turn (`user_message` / `assistant_response`).
//! Copilot persists only turn-level text — tool calls, file edits, and reasoning are not kept
//! in its store — so this adapter emits user prompts and assistant responses, nothing finer.
//! Discovery reads the `sessions` table; tailing polls `turns` rows by their autoincrement id.
//! Verified against a real Copilot CLI database.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, FixedOffset};
use rusqlite::{Connection, OpenFlags};

use crate::context::clip;
use crate::events::{now_fixed, Event, EventKind};
use crate::sources::{Agent, SessionInfo, Source};

/// Most recent events returned by a backfill (bounds cost on long sessions).
const BACKFILL_LIMIT: usize = 400;

// --------------------------------------------------------------------------- //
// Locations
// --------------------------------------------------------------------------- //

/// Copilot CLI's data directory: `~/.copilot`.
fn data_dir() -> PathBuf {
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".copilot")
}

fn db_path() -> PathBuf {
    data_dir().join("session-store.db")
}

fn open_ro(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        .ok()
}

/// Copilot writes ISO-8601 timestamps (`2026-06-18T13:07:27.063Z`).
fn parse_ts(s: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(s).ok()
}

// --------------------------------------------------------------------------- //
// Discovery
// --------------------------------------------------------------------------- //

/// All Copilot CLI sessions, newest first. Optionally filter by working directory.
pub fn discover_sessions(cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    discover_in(&db_path(), cwd_filter)
}

fn discover_in(db: &Path, cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    let Some(conn) = open_ro(db) else {
        return Vec::new();
    };
    let sql = "SELECT id, cwd, branch, summary, updated_at \
               FROM sessions ORDER BY updated_at DESC";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?.unwrap_or_default(),
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, Option<String>>(3)?.unwrap_or_default(),
            r.get::<_, Option<String>>(4)?.unwrap_or_default(),
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let db = db.to_path_buf();
    rows.flatten()
        .filter(|(_, cwd, _, _, _)| cwd_filter.is_none_or(|f| cwd == f))
        .map(|(id, cwd, branch, summary, updated)| SessionInfo {
            agent: Agent::Copilot,
            path: db.clone(),
            session_id: id,
            cwd,
            git_branch: branch,
            modified: parse_ts(&updated).map(SystemTime::from).unwrap_or(SystemTime::UNIX_EPOCH),
            size: 0,
            summary: clip(summary.trim(), 90),
        })
        .collect()
}

// --------------------------------------------------------------------------- //
// Source
// --------------------------------------------------------------------------- //

pub struct CopilotSource {
    conn: Option<Connection>,
    session_id: String,
    cwd: String,
    last_id: i64,
    started: bool,
}

impl CopilotSource {
    pub fn new(db: impl AsRef<Path>, session_id: &str) -> Self {
        let conn = open_ro(db.as_ref());
        let cwd = conn.as_ref().and_then(|c| session_cwd(c, session_id)).unwrap_or_default();
        CopilotSource {
            conn,
            session_id: session_id.to_string(),
            cwd,
            last_id: 0,
            started: false,
        }
    }

    /// Read turn rows with `id > self.last_id`, normalize them, and advance the cursor.
    /// Each turn yields a user prompt and the assistant's response (skipping empty halves).
    fn read_turns(&mut self) -> Vec<Event> {
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let sql = "SELECT id, turn_index, user_message, assistant_response, timestamp \
                   FROM turns WHERE session_id = ?1 AND id > ?2 ORDER BY id";
        let Ok(mut stmt) = conn.prepare(sql) else {
            return Vec::new();
        };
        let rows = stmt.query_map(rusqlite::params![self.session_id, self.last_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                r.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        });
        let Ok(rows) = rows else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (id, turn_index, user, assistant, ts_str) in rows.flatten() {
            self.last_id = self.last_id.max(id);
            let ts = parse_ts(&ts_str).unwrap_or_else(now_fixed);
            let turn = Some(turn_index.to_string());
            let user = user.trim();
            if !user.is_empty() {
                out.push(
                    Event::new(
                        EventKind::UserPrompt { text: user.to_string() },
                        ts,
                        turn.clone(),
                        false,
                        None,
                    )
                    .with_source("copilot"),
                );
            }
            let assistant = assistant.trim();
            if !assistant.is_empty() {
                out.push(
                    Event::new(
                        EventKind::AssistantText { text: assistant.to_string() },
                        ts,
                        turn,
                        false,
                        None,
                    )
                    .with_source("copilot"),
                );
            }
        }
        out
    }

    fn session_start(&self, ts: DateTime<FixedOffset>) -> Event {
        Event::new(
            EventKind::SessionStart {
                session_id: self.session_id.clone(),
                cwd: self.cwd.clone(),
                version: String::new(),
            },
            ts,
            None,
            false,
            None,
        )
        .with_source("copilot")
    }
}

impl Source for CopilotSource {
    fn backfill(&mut self) -> Vec<Event> {
        self.last_id = 0;
        let mut events = self.read_turns();
        if events.len() > BACKFILL_LIMIT {
            events = events.split_off(events.len() - BACKFILL_LIMIT);
        }
        if !self.started {
            self.started = true;
            let ts = events.first().map(|e| e.ts).unwrap_or_else(now_fixed);
            events.insert(0, self.session_start(ts));
        }
        events
    }

    fn read_new(&mut self) -> Vec<Event> {
        self.read_turns()
    }
}

/// The working directory recorded for a session, from the `sessions` row.
fn session_cwd(conn: &Connection, session_id: &str) -> Option<String> {
    conn.query_row(
        "SELECT cwd FROM sessions WHERE id = ?1",
        rusqlite::params![session_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal Copilot database with the columns the adapter reads.
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, cwd TEXT, repository TEXT, \
                host_type TEXT, branch TEXT, summary TEXT, created_at TEXT, updated_at TEXT);
             CREATE TABLE turns (id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, \
                turn_index INTEGER NOT NULL, user_message TEXT, assistant_response TEXT, timestamp TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('ses_1', '/work/proj', 'proj', 'github', 'main', \
                'Fix the parser', '2026-06-01T10:00:00.000Z', '2026-06-01T10:05:00.000Z')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions VALUES ('ses_2', '/other', 'other', 'github', '', '', \
                '2026-06-02T10:00:00.000Z', '2026-06-02T10:05:00.000Z')",
            [],
        )
        .unwrap();
        let turns = [
            (1, "ses_1", 0, "please fix it", "I'll take a look.", "2026-06-01T10:01:00.000Z"),
            // A turn whose assistant half is still empty: only the prompt should surface.
            (2, "ses_1", 1, "and add a test", "", "2026-06-01T10:03:00.000Z"),
            (3, "ses_1", 2, "thanks", "Done — tests pass.", "2026-06-01T10:05:00.000Z"),
            // Belongs to another session; must not leak into ses_1's stream.
            (4, "ses_2", 0, "unrelated", "ok", "2026-06-02T10:01:00.000Z"),
        ];
        for (id, sid, idx, u, a, ts) in turns {
            conn.execute(
                "INSERT INTO turns VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![id, sid, idx, u, a, ts],
            )
            .unwrap();
        }
    }

    #[test]
    fn discovers_sessions_with_metadata() {
        let dir = std::env::temp_dir().join(format!("cp_disc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("session-store.db");
        let _ = std::fs::remove_file(&db);
        make_db(&db);

        let found = discover_in(&db, None);
        assert_eq!(found.len(), 2);
        // Newest first by updated_at.
        assert_eq!(found[0].session_id, "ses_2");
        let s1 = found.iter().find(|s| s.session_id == "ses_1").unwrap();
        assert_eq!(s1.agent, Agent::Copilot);
        assert_eq!(s1.cwd, "/work/proj");
        assert_eq!(s1.git_branch, "main");
        assert_eq!(s1.summary, "Fix the parser");
        // cwd filter
        assert_eq!(discover_in(&db, Some("/nope")).len(), 0);
        assert_eq!(discover_in(&db, Some("/work/proj")).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_normalizes_turns_in_order() {
        let dir = std::env::temp_dir().join(format!("cp_norm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("session-store.db");
        let _ = std::fs::remove_file(&db);
        make_db(&db);

        let mut src = CopilotSource::new(&db, "ses_1");
        let events = src.backfill();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.name()).collect();
        // SessionStart, then per turn: prompt (+response when present); empty response is skipped.
        assert_eq!(
            kinds,
            vec![
                "session_start",
                "user_prompt",
                "assistant_text",
                "user_prompt",
                "user_prompt",
                "assistant_text",
            ]
        );
        assert!(events.iter().all(|e| e.source == "copilot"));

        // SessionStart carries the session's cwd.
        match &events[0].kind {
            EventKind::SessionStart { cwd, session_id, .. } => {
                assert_eq!(cwd, "/work/proj");
                assert_eq!(session_id, "ses_1");
            }
            other => panic!("expected session_start, got {other:?}"),
        }

        // Nothing new on a second poll.
        assert!(src.read_new().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
