//! OpenCode source adapter. OpenCode stores sessions in a SQLite database
//! (`~/.local/share/opencode/opencode.db`) across `session` / `message` / `part` tables,
//! with the per-record payload as JSON in a `data` column. Discovery reads the `session`
//! table; tailing polls `part` rows by rowid and normalizes them into the shared `Event`
//! model. Verified against a real OpenCode database.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, FixedOffset};
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::context::clip;
use crate::events::{now_fixed, Event, EventKind, Hunk};
use crate::sources::{Agent, SessionInfo, Source};

/// Cap synthesized "new file" diffs so a huge write can't flood the detail pane.
const MAX_CREATE_LINES: usize = 400;
/// Most recent events returned by a backfill (bounds cost on long sessions).
const BACKFILL_LIMIT: usize = 400;

// --------------------------------------------------------------------------- //
// Locations
// --------------------------------------------------------------------------- //

/// OpenCode's data directory: `$XDG_DATA_HOME/opencode`, else `~/.local/share/opencode`
/// (OpenCode uses the XDG layout even on macOS).
fn data_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_DATA_HOME") {
        return PathBuf::from(x).join("opencode");
    }
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".local/share/opencode")
}

fn db_path() -> PathBuf {
    data_dir().join("opencode.db")
}

fn open_ro(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        .ok()
}

// --------------------------------------------------------------------------- //
// Discovery
// --------------------------------------------------------------------------- //

/// All top-level OpenCode sessions, newest first. Optionally filter by working directory.
pub fn discover_sessions(cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    discover_in(&db_path(), cwd_filter)
}

fn discover_in(db: &Path, cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    let Some(conn) = open_ro(db) else {
        return Vec::new();
    };
    // Only top-level sessions (skip subagent children with a parent_id).
    let sql = "SELECT id, directory, title, time_updated \
               FROM session WHERE parent_id IS NULL ORDER BY time_updated DESC";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            r.get::<_, i64>(3)?,
        ))
    });
    let Ok(rows) = rows else {
        return Vec::new();
    };
    let db = db.to_path_buf();
    rows.flatten()
        .filter(|(_, dir, _, _)| cwd_filter.is_none_or(|f| dir == f))
        .map(|(id, dir, title, updated)| SessionInfo {
            agent: Agent::OpenCode,
            path: db.clone(),
            session_id: id,
            cwd: dir,
            git_branch: String::new(),
            modified: SystemTime::UNIX_EPOCH + Duration::from_millis(updated.max(0) as u64),
            size: 0,
            summary: clip(title.trim(), 90),
        })
        .collect()
}

// --------------------------------------------------------------------------- //
// Source
// --------------------------------------------------------------------------- //

pub struct OpenCodeSource {
    conn: Option<Connection>,
    session_id: String,
    cwd: String,
    version: String,
    last_rowid: i64,
    started: bool,
}

impl OpenCodeSource {
    pub fn new(db: impl AsRef<Path>, session_id: &str) -> Self {
        let conn = open_ro(db.as_ref());
        let (cwd, version) = conn
            .as_ref()
            .and_then(|c| session_meta(c, session_id))
            .unwrap_or_default();
        OpenCodeSource {
            conn,
            session_id: session_id.to_string(),
            cwd,
            version,
            last_rowid: 0,
            started: false,
        }
    }

    /// Read part rows with `rowid > self.last_rowid`, normalize them, and advance the cursor.
    fn read_parts(&mut self) -> Vec<Event> {
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let sql = "SELECT p.rowid, p.data, json_extract(m.data,'$.role'), p.message_id, p.time_created \
                   FROM part p JOIN message m ON p.message_id = m.id \
                   WHERE p.session_id = ?1 AND p.rowid > ?2 ORDER BY p.rowid";
        let Ok(mut stmt) = conn.prepare(sql) else {
            return Vec::new();
        };
        let rows = stmt.query_map(rusqlite::params![self.session_id, self.last_rowid], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
            ))
        });
        let Ok(rows) = rows else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (rowid, data, role, msg_id, ts_ms) in rows.flatten() {
            self.last_rowid = self.last_rowid.max(rowid);
            let Ok(part) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            let ts = ms_to_ts(ts_ms);
            let raw = part.get("id").and_then(|v| v.as_str()).map(|s| s.to_string());
            for kind in part_to_kinds(&part, &role) {
                out.push(
                    Event::new(kind, ts, Some(msg_id.clone()), false, raw.clone())
                        .with_source("opencode"),
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
                version: self.version.clone(),
            },
            ts,
            None,
            false,
            None,
        )
        .with_source("opencode")
    }
}

impl Source for OpenCodeSource {
    fn backfill(&mut self) -> Vec<Event> {
        self.last_rowid = 0;
        let mut events = self.read_parts();
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
        self.read_parts()
    }
}

/// `(cwd, version)` for a session, from the `session` row.
fn session_meta(conn: &Connection, session_id: &str) -> Option<(String, String)> {
    conn.query_row(
        "SELECT directory, version FROM session WHERE id = ?1",
        rusqlite::params![session_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?.unwrap_or_default())),
    )
    .ok()
}

// --------------------------------------------------------------------------- //
// Normalization (pure: a part's JSON → event kinds)
// --------------------------------------------------------------------------- //

/// Normalize one `part` payload into zero or more event kinds. `role` is the parent
/// message's role (`user` / `assistant`).
fn part_to_kinds(part: &Value, role: &str) -> Vec<EventKind> {
    match part.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "text" => {
            let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
            if text.is_empty() {
                return Vec::new();
            }
            let kind = if role == "user" {
                EventKind::UserPrompt { text: text.to_string() }
            } else {
                EventKind::AssistantText { text: text.to_string() }
            };
            vec![kind]
        }
        "reasoning" => {
            let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
            vec![EventKind::Thinking { text, summary: None }]
        }
        "tool" => tool_to_kinds(part),
        _ => Vec::new(), // step-start / step-finish carry no observer-facing content
    }
}

fn tool_to_kinds(part: &Value) -> Vec<EventKind> {
    let name = part.get("tool").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
    let id = part.get("callID").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let state = part.get("state").cloned().unwrap_or(Value::Null);
    let input = state.get("input").cloned().unwrap_or(Value::Object(Default::default()));
    let status = state.get("status").and_then(|v| v.as_str()).unwrap_or("");

    let mut out = vec![EventKind::ToolCall { id: id.clone(), name: name.clone(), input: input.clone() }];

    // Terminal tool states carry a result.
    if matches!(status, "completed" | "error") {
        let ok = status == "completed";
        let (summary, detail) = tool_result(&state);
        out.push(EventKind::ToolResult { id, name: name.clone(), ok, summary, detail });
    }

    // Edit/write tools also produce a file change.
    if let Some(edit) = file_edit(&name, &input) {
        out.push(edit);
    }
    out
}

fn tool_result(state: &Value) -> (String, String) {
    if state.get("status").and_then(|v| v.as_str()) == Some("error") {
        let err = state.get("error").and_then(|v| v.as_str()).unwrap_or("tool error");
        return (clip(err, 80), clip(err, 2000));
    }
    let output = state.get("output").and_then(|v| v.as_str()).unwrap_or("");
    let title = state.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let summary = if !title.is_empty() {
        clip(title, 80)
    } else {
        clip(output.lines().next().unwrap_or(""), 80)
    };
    (summary, clip(output, 2000))
}

/// Synthesize a `FileEdit` from a write (whole new content) or edit (old → new) tool input.
fn file_edit(tool: &str, input: &Value) -> Option<EventKind> {
    let path = input.get("filePath").and_then(|v| v.as_str())?.to_string();
    match tool {
        "write" => {
            let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");
            let mut lines: Vec<&str> = content.split('\n').collect();
            if lines.last() == Some(&"") {
                lines.pop();
            }
            let total = lines.len();
            let mut shown: Vec<String> =
                lines.iter().take(MAX_CREATE_LINES).map(|l| format!("+{l}")).collect();
            if total > MAX_CREATE_LINES {
                shown.push(format!("… (+{} more lines)", total - MAX_CREATE_LINES));
            }
            Some(EventKind::FileEdit {
                path,
                hunks: vec![Hunk { old_start: 0, old_lines: 0, new_start: 1, new_lines: total as i64, lines: shown }],
                added: total,
                removed: 0,
                original: None,
                created: true,
            })
        }
        "edit" => {
            let old = input.get("oldString").and_then(|v| v.as_str()).unwrap_or("");
            let new = input.get("newString").and_then(|v| v.as_str()).unwrap_or("");
            let old_lines: Vec<&str> = old.split('\n').collect();
            let new_lines: Vec<&str> = new.split('\n').collect();
            let mut lines: Vec<String> = old_lines.iter().map(|l| format!("-{l}")).collect();
            lines.extend(new_lines.iter().map(|l| format!("+{l}")));
            Some(EventKind::FileEdit {
                path,
                hunks: vec![Hunk {
                    old_start: 1,
                    old_lines: old_lines.len() as i64,
                    new_start: 1,
                    new_lines: new_lines.len() as i64,
                    lines,
                }],
                added: new_lines.len(),
                removed: old_lines.len(),
                original: Some(old.to_string()),
                created: false,
            })
        }
        _ => None,
    }
}

fn ms_to_ts(ms: i64) -> DateTime<FixedOffset> {
    DateTime::from_timestamp_millis(ms).map(|d| d.fixed_offset()).unwrap_or_else(now_fixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal OpenCode database with the columns the adapter reads.
    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, \
                directory TEXT, title TEXT, version TEXT, time_created INTEGER, time_updated INTEGER);
             CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
             CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, data TEXT);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO session VALUES ('ses_1', 'prj', NULL, '/work/proj', 'Fix the parser', '1.0.0', 1000, 2000)",
            [],
        )
        .unwrap();
        // A subagent child session that should not be discovered.
        conn.execute(
            "INSERT INTO session VALUES ('ses_2', 'prj', 'ses_1', '/work/proj', 'child', '1.0.0', 1000, 2500)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO message VALUES ('msg_u', 'ses_1', 1000, '{\"role\":\"user\"}')", []).unwrap();
        conn.execute("INSERT INTO message VALUES ('msg_a', 'ses_1', 1100, '{\"role\":\"assistant\"}')", []).unwrap();
        let parts = [
            ("p1", "msg_u", 1000, r#"{"id":"p1","type":"text","text":"please fix it"}"#),
            ("p2", "msg_a", 1100, r#"{"id":"p2","type":"reasoning","text":"thinking about it"}"#),
            ("p3", "msg_a", 1110, r#"{"id":"p3","type":"tool","tool":"bash","callID":"c1","state":{"status":"completed","input":{"command":"ls"},"output":"a\nb","title":"List files"}}"#),
            ("p4", "msg_a", 1120, r#"{"id":"p4","type":"tool","tool":"write","callID":"c2","state":{"status":"completed","input":{"filePath":"/work/proj/x.rs","content":"fn main() {}\n"}}}"#),
            ("p5", "msg_a", 1130, r#"{"id":"p5","type":"text","text":"done"}"#),
            ("p6", "msg_a", 1140, r#"{"id":"p6","type":"step-finish"}"#),
        ];
        for (id, msg, ts, data) in parts {
            conn.execute(
                "INSERT INTO part VALUES (?1, ?2, 'ses_1', ?3, ?4)",
                rusqlite::params![id, msg, ts, data],
            )
            .unwrap();
        }
    }

    #[test]
    fn discovers_top_level_sessions_only() {
        let dir = std::env::temp_dir().join(format!("oc_disc_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        make_db(&db);

        let found = discover_in(&db, None);
        assert_eq!(found.len(), 1, "subagent child must be filtered out");
        assert_eq!(found[0].session_id, "ses_1");
        assert_eq!(found[0].agent, Agent::OpenCode);
        assert_eq!(found[0].cwd, "/work/proj");
        assert_eq!(found[0].summary, "Fix the parser");
        // cwd filter
        assert_eq!(discover_in(&db, Some("/nope")).len(), 0);
        assert_eq!(discover_in(&db, Some("/work/proj")).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_normalizes_parts_in_order() {
        let dir = std::env::temp_dir().join(format!("oc_norm_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        make_db(&db);

        let mut src = OpenCodeSource::new(&db, "ses_1");
        let events = src.backfill();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.name()).collect();
        // SessionStart, user prompt, thinking, (bash) call+result, (write) call+result+edit, assistant text.
        assert_eq!(
            kinds,
            vec![
                "session_start",
                "user_prompt",
                "thinking",
                "tool_call",
                "tool_result",
                "tool_call",
                "tool_result",
                "file_edit",
                "assistant_text",
            ]
        );
        assert!(events.iter().all(|e| e.source == "opencode"));

        // The write became a created FileEdit on the right path.
        let edit = events.iter().find_map(|e| match &e.kind {
            EventKind::FileEdit { path, created, added, .. } => Some((path.clone(), *created, *added)),
            _ => None,
        });
        assert_eq!(edit, Some(("/work/proj/x.rs".to_string(), true, 1)));

        // Nothing new on a second poll.
        assert!(src.read_new().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
