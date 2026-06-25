//! Antigravity CLI source adapter. Google's Antigravity CLI stores each conversation in its
//! own SQLite database (`~/.gemini/antigravity-cli/conversations/<id>.db`): conversation
//! metadata lives in the `trajectory_metadata_blob` table and the turn-by-turn transcript in a
//! `steps` table, ordered by an `idx` primary key. Unlike the other adapters, Antigravity
//! serializes each step as a protobuf `step_payload` blob (no published schema), so this
//! adapter carries a small schema-free protobuf wire reader and pulls the fields it needs by
//! number. The `step_type` column selects how each payload is normalized. Field numbers were
//! reverse-engineered from a real database and may shift across Antigravity releases.
//! Discovery scans the `conversations` directory; tailing polls `steps` rows by `idx`.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
/// `idx` is 0-based, so the cursor starts one below the first row.
const IDX_START: i64 = -1;

// Step kinds (the `step_type` column). Tool-execution steps (view_file, list_dir, run_command,
// grep_search, write_to_file, …) carry varied type ids but all hold a `[5,4]` tool block, so
// they're recognized structurally rather than by an exhaustive list.
const ST_USER_PROMPT: i64 = 14;
const ST_ASSISTANT: i64 = 15;

// --------------------------------------------------------------------------- //
// Locations
// --------------------------------------------------------------------------- //

/// Antigravity CLI's data directory: `~/.gemini/antigravity-cli`.
fn data_dir() -> PathBuf {
    let home = directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".gemini/antigravity-cli")
}

fn conversations_dir() -> PathBuf {
    data_dir().join("conversations")
}

fn open_ro(path: &Path) -> Option<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI)
        .ok()
}

/// Strip a `file://` scheme from a workspace URI, leaving a plain path.
fn uri_to_path(uri: &str) -> String {
    uri.strip_prefix("file://").unwrap_or(uri).to_string()
}

// --------------------------------------------------------------------------- //
// Discovery
// --------------------------------------------------------------------------- //

/// All Antigravity CLI conversations, newest first. Optionally filter by working directory.
pub fn discover_sessions(cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    discover_in(&conversations_dir(), cwd_filter)
}

fn discover_in(dir: &Path, cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("db") {
            continue;
        }
        if let Some(info) = read_session_meta(&path) {
            if cwd_filter.is_none_or(|f| info.cwd == f) {
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

fn read_session_meta(db: &Path) -> Option<SessionInfo> {
    let conn = open_ro(db)?;
    let cwd = trajectory_cwd(&conn).unwrap_or_default();
    let id = db.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
    let modified = std::fs::metadata(db).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    Some(SessionInfo {
        agent: Agent::Antigravity,
        path: db.to_path_buf(),
        session_id: id,
        cwd,
        git_branch: String::new(),
        modified,
        size: 0,
        summary: clip(session_summary(&conn).trim(), 90),
    })
}

/// The workspace path for a conversation, from `trajectory_metadata_blob` (field 7 is the
/// `file://` workspace URI).
fn trajectory_cwd(conn: &Connection) -> Option<String> {
    let blob: Vec<u8> = conn
        .query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get(0))
        .ok()?;
    str_at(&blob, &[7]).map(|u| uri_to_path(&u))
}

/// A best-effort one-liner: the first user prompt, else the generated title.
fn session_summary(conn: &Connection) -> String {
    let payload: Option<Vec<u8>> = conn
        .query_row(
            "SELECT step_payload FROM steps WHERE step_type = ?1 ORDER BY idx LIMIT 1",
            [ST_USER_PROMPT],
            |r| r.get(0),
        )
        .ok();
    if let Some(text) = payload.and_then(|p| str_at(&p, &[19, 2])) {
        return text;
    }
    // Fall back to a planning/title step (`[30,4]`).
    conn.query_row("SELECT step_payload FROM steps ORDER BY idx", [], |r| r.get::<_, Vec<u8>>(0))
        .ok()
        .and_then(|p| str_at(&p, &[30, 4]))
        .unwrap_or_default()
}

// --------------------------------------------------------------------------- //
// Source
// --------------------------------------------------------------------------- //

pub struct AntigravitySource {
    conn: Option<Connection>,
    session_id: String,
    cwd: String,
    created: Option<DateTime<FixedOffset>>,
    last_idx: i64,
    started: bool,
}

impl AntigravitySource {
    pub fn new(db: impl AsRef<Path>, session_id: &str) -> Self {
        let conn = open_ro(db.as_ref());
        let cwd = conn.as_ref().and_then(trajectory_cwd).unwrap_or_default();
        let created = conn.as_ref().and_then(trajectory_created);
        AntigravitySource {
            conn,
            session_id: session_id.to_string(),
            cwd,
            created,
            last_idx: IDX_START,
            started: false,
        }
    }

    /// Read step rows with `idx > self.last_idx`, normalize them, and advance the cursor.
    fn read_steps(&mut self) -> Vec<Event> {
        let Some(conn) = self.conn.as_ref() else {
            return Vec::new();
        };
        let sql = "SELECT idx, step_type, error_details, step_payload \
                   FROM steps WHERE idx > ?1 ORDER BY idx";
        let Ok(mut stmt) = conn.prepare(sql) else {
            return Vec::new();
        };
        let rows = stmt.query_map([self.last_idx], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default(),
                r.get::<_, Option<Vec<u8>>>(3)?.unwrap_or_default(),
            ))
        });
        let Ok(rows) = rows else {
            return Vec::new();
        };
        let fallback = self.created.unwrap_or_else(now_fixed);
        let mut out = Vec::new();
        for (idx, step_type, err, payload) in rows.flatten() {
            self.last_idx = self.last_idx.max(idx);
            let ts = step_ts(&payload).unwrap_or(fallback);
            let turn = Some(idx.to_string());
            for kind in normalize_step(step_type, &payload, err.is_empty()) {
                out.push(
                    Event::new(kind, ts, turn.clone(), false, None).with_source("antigravity"),
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
        .with_source("antigravity")
    }
}

impl Source for AntigravitySource {
    fn backfill(&mut self) -> Vec<Event> {
        self.last_idx = IDX_START;
        let mut events = self.read_steps();
        if events.len() > BACKFILL_LIMIT {
            events = events.split_off(events.len() - BACKFILL_LIMIT);
        }
        if !self.started {
            self.started = true;
            let ts = self.created.or_else(|| events.first().map(|e| e.ts)).unwrap_or_else(now_fixed);
            events.insert(0, self.session_start(ts));
        }
        events
    }

    fn read_new(&mut self) -> Vec<Event> {
        self.read_steps()
    }
}

/// Conversation creation time, from `trajectory_metadata_blob` (`[2,1]` seconds, `[2,2]` nanos).
fn trajectory_created(conn: &Connection) -> Option<DateTime<FixedOffset>> {
    let blob: Vec<u8> = conn
        .query_row("SELECT data FROM trajectory_metadata_blob LIMIT 1", [], |r| r.get(0))
        .ok()?;
    let secs = varint_at(&blob, &[2, 1])? as i64;
    let nanos = varint_at(&blob, &[2, 2]).unwrap_or(0) as u32;
    DateTime::from_timestamp(secs, nanos).map(|d| d.fixed_offset())
}

// --------------------------------------------------------------------------- //
// Normalization (pure: a step's type + protobuf payload → event kinds)
// --------------------------------------------------------------------------- //

/// Normalize one step into zero or more event kinds. `ok` is false when the step recorded an
/// error (the `error_details` column was non-empty).
fn normalize_step(step_type: i64, payload: &[u8], ok: bool) -> Vec<EventKind> {
    match step_type {
        ST_USER_PROMPT => str_at(payload, &[19, 2])
            .filter(|t| !t.trim().is_empty())
            .map(|text| EventKind::UserPrompt { text })
            .into_iter()
            .collect(),
        ST_ASSISTANT => assistant_kinds(payload),
        // Any other step that carries a tool block is a tool execution result.
        _ if field_at(payload, &[5, 4]).is_some() => tool_result_kinds(payload, ok),
        _ => Vec::new(), // planning titles / system metadata carry no observer-facing content
    }
}

/// An assistant turn: its prose text, then the tool call it announces (if any).
fn assistant_kinds(payload: &[u8]) -> Vec<EventKind> {
    let mut out = Vec::new();
    if let Some(text) = str_at(payload, &[20, 1]).filter(|t| !t.trim().is_empty()) {
        out.push(EventKind::AssistantText { text });
    }
    // The announced tool call lives at `[20,7]`: .1 id, .2 name, .3 args (JSON).
    if let Some(name) = str_at(payload, &[20, 7, 2]) {
        let id = str_at(payload, &[20, 7, 1]).unwrap_or_default();
        let input = str_at(payload, &[20, 7, 3])
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| Value::Object(Default::default()));
        out.push(EventKind::ToolCall { id, name, input });
    }
    out
}

/// A tool execution result. The tool block is at `[5,4]` (.1 id, .2 name, .3 args JSON); the
/// human summary at `[5,30]`; the result body is the largest text leaf outside that block.
fn tool_result_kinds(payload: &[u8], ok: bool) -> Vec<EventKind> {
    let id = str_at(payload, &[5, 4, 1]).unwrap_or_default();
    let name = str_at(payload, &[5, 4, 2]).unwrap_or_else(|| "tool".to_string());
    let args = str_at(payload, &[5, 4, 3]).and_then(|s| serde_json::from_str::<Value>(&s).ok());

    let detail = longest_text(payload, &[5]);
    let summary = str_at(payload, &[5, 30])
        .or_else(|| str_at(payload, &[5, 31]))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| clip(detail.lines().next().unwrap_or(""), 80));

    let mut out = vec![EventKind::ToolResult {
        id,
        name: name.clone(),
        ok,
        summary: clip(&summary, 80),
        detail: clip(&detail, 2000),
    }];
    if name == "write_to_file" {
        if let Some(edit) = file_write(args.as_ref()) {
            out.push(edit);
        }
    }
    out
}

/// Synthesize a `FileEdit` from a `write_to_file` call (`TargetFile` + whole-file `CodeContent`).
fn file_write(args: Option<&Value>) -> Option<EventKind> {
    let args = args?;
    let path = args.get("TargetFile").and_then(|v| v.as_str())?.to_string();
    let content = args.get("CodeContent").and_then(|v| v.as_str()).unwrap_or("");
    let overwrite = args.get("Overwrite").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let total = lines.len();
    let mut shown: Vec<String> = lines.iter().take(MAX_CREATE_LINES).map(|l| format!("+{l}")).collect();
    if total > MAX_CREATE_LINES {
        shown.push(format!("… (+{} more lines)", total - MAX_CREATE_LINES));
    }
    Some(EventKind::FileEdit {
        path,
        hunks: vec![Hunk { old_start: 0, old_lines: 0, new_start: 1, new_lines: total as i64, lines: shown }],
        added: total,
        removed: 0,
        original: None,
        created: !overwrite,
    })
}

// --------------------------------------------------------------------------- //
// Minimal protobuf wire reader (schema-free; we only read varints and length-delimited fields)
// --------------------------------------------------------------------------- //

fn read_varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut val = 0u64;
    let mut shift = 0u32;
    while *pos < buf.len() {
        let b = buf[*pos];
        *pos += 1;
        val |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(val);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

/// Skip the value of the field whose `wire_type` was just read, advancing `pos`. Returns false
/// on a malformed or unsupported (group) field.
fn skip_value(buf: &[u8], pos: &mut usize, wire_type: u64) -> bool {
    match wire_type {
        0 => read_varint(buf, pos).is_some(),
        1 => {
            *pos += 8;
            *pos <= buf.len()
        }
        5 => {
            *pos += 4;
            *pos <= buf.len()
        }
        2 => match read_varint(buf, pos) {
            Some(len) => {
                *pos = pos.saturating_add(len as usize);
                *pos <= buf.len()
            }
            None => false,
        },
        _ => false,
    }
}

/// Bytes of the first length-delimited field numbered `field` in `buf`.
fn field_bytes(buf: &[u8], field: u64) -> Option<&[u8]> {
    let mut pos = 0;
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        let (f, wt) = (key >> 3, key & 7);
        if wt == 2 {
            let len = read_varint(buf, &mut pos)? as usize;
            let end = pos.checked_add(len)?;
            if end > buf.len() {
                return None;
            }
            if f == field {
                return Some(&buf[pos..end]);
            }
            pos = end;
        } else if !skip_value(buf, &mut pos, wt) {
            return None;
        }
    }
    None
}

/// First varint field numbered `field` in `buf`.
fn field_varint(buf: &[u8], field: u64) -> Option<u64> {
    let mut pos = 0;
    while pos < buf.len() {
        let key = read_varint(buf, &mut pos)?;
        let (f, wt) = (key >> 3, key & 7);
        if wt == 0 {
            let v = read_varint(buf, &mut pos)?;
            if f == field {
                return Some(v);
            }
        } else if !skip_value(buf, &mut pos, wt) {
            return None;
        }
    }
    None
}

/// Descend a path of nested length-delimited fields, returning the final field's bytes.
fn field_at<'a>(buf: &'a [u8], path: &[u64]) -> Option<&'a [u8]> {
    let mut cur = buf;
    for &f in path {
        cur = field_bytes(cur, f)?;
    }
    Some(cur)
}

/// A UTF-8 string at `path` (all-but-last fields are nested messages; the last is the string).
fn str_at(buf: &[u8], path: &[u64]) -> Option<String> {
    field_at(buf, path).map(|b| String::from_utf8_lossy(b).into_owned())
}

/// A varint at `path` (all-but-last fields are nested messages; the last is the varint).
fn varint_at(buf: &[u8], path: &[u64]) -> Option<u64> {
    let (last, parents) = path.split_last()?;
    let mut cur = buf;
    for &f in parents {
        cur = field_bytes(cur, f)?;
    }
    field_varint(cur, *last)
}

/// The longest human-readable text leaf in `buf`, skipping whole top-level subtrees whose field
/// number is in `skip_top` (used to ignore the tool-call block when hunting for a result body).
fn longest_text(buf: &[u8], skip_top: &[u64]) -> String {
    fn rec(buf: &[u8], top_level: bool, skip: &[u64], best: &mut String) {
        let mut pos = 0;
        while pos < buf.len() {
            let Some(key) = read_varint(buf, &mut pos) else {
                return;
            };
            let (f, wt) = (key >> 3, key & 7);
            if wt != 2 {
                if !skip_value(buf, &mut pos, wt) {
                    return;
                }
                continue;
            }
            let Some(len) = read_varint(buf, &mut pos) else {
                return;
            };
            let Some(end) = pos.checked_add(len as usize) else {
                return;
            };
            if end > buf.len() {
                return;
            }
            let sub = &buf[pos..end];
            pos = end;
            if top_level && skip.contains(&f) {
                continue;
            }
            match std::str::from_utf8(sub) {
                Ok(s) if is_texty(s) => {
                    if s.len() > best.len() && !s.starts_with("file://") {
                        *best = s.to_string();
                    }
                }
                _ => rec(sub, false, skip, best),
            }
        }
    }
    let mut best = String::new();
    rec(buf, true, skip_top, &mut best);
    best
}

/// Whether a decoded string looks like real text rather than a binary blob that happened to be
/// valid UTF-8: it must contain a letter or digit and no control chars besides whitespace.
fn is_texty(s: &str) -> bool {
    !s.is_empty()
        && s.chars().any(|c| c.is_alphanumeric())
        && s.chars().all(|c| !c.is_control() || c == '\n' || c == '\t' || c == '\r')
}

/// A step's timestamp, from one of the nested time fields (unix seconds + nanos), newest first.
fn step_ts(payload: &[u8]) -> Option<DateTime<FixedOffset>> {
    for path in [[5, 8], [5, 7], [5, 6], [5, 1]] {
        if let Some(secs) = varint_at(payload, &[path[0], path[1], 1]) {
            let nanos = varint_at(payload, &[path[0], path[1], 2]).unwrap_or(0) as u32;
            if let Some(dt) = DateTime::from_timestamp(secs as i64, nanos) {
                return Some(dt.fixed_offset());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- tiny protobuf encoder, just enough to build fixture payloads --- //

    fn put_key(out: &mut Vec<u8>, field: u64, wt: u64) {
        let mut v = (field << 3) | wt;
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }
    fn put_varint(out: &mut Vec<u8>, mut v: u64) {
        loop {
            let mut b = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                b |= 0x80;
            }
            out.push(b);
            if v == 0 {
                break;
            }
        }
    }
    /// Encode a length-delimited field (string or nested message).
    fn ld(field: u64, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        put_key(&mut out, field, 2);
        put_varint(&mut out, bytes.len() as u64);
        out.extend_from_slice(bytes);
        out
    }
    /// Encode a varint field.
    fn vint(field: u64, v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        put_key(&mut out, field, 0);
        put_varint(&mut out, v);
        out
    }
    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    fn make_db(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE trajectory_metadata_blob (id TEXT DEFAULT 'main', data BLOB, PRIMARY KEY (id));
             CREATE TABLE steps (idx INTEGER, step_type INTEGER NOT NULL DEFAULT 0, \
                error_details BLOB, step_payload BLOB, PRIMARY KEY (idx));",
        )
        .unwrap();

        // metadata: created time [2,1]=1000, workspace URI [7]
        let meta = cat(&[
            ld(2, &cat(&[vint(1, 1000), vint(2, 0)])),
            ld(7, b"file:///work/proj"),
        ]);
        conn.execute("INSERT INTO trajectory_metadata_blob VALUES ('main', ?1)", [meta]).unwrap();

        // idx0: user prompt [19,2]
        let prompt = cat(&[vint(1, 14), ld(19, &ld(2, b"please fix it"))]);
        // idx1: assistant text [20,1] + tool call [20,7].{1,2,3}
        let announce = cat(&[
            ld(1, b"I'll list the dir"),
            ld(7, &cat(&[ld(1, b"call1"), ld(2, b"list_dir"), ld(3, br#"{"DirectoryPath":"/work/proj"}"#)])),
        ]);
        let assistant = cat(&[vint(1, 15), ld(20, &announce)]);
        // idx2: list_dir result (type 9). Field 5 is one sub-message holding the time ([5,1]),
        // the tool block ([5,4]) and the summary ([5,30]); the result body is a sibling field.
        let meta5 = cat(&[
            ld(1, &time_sub(1100)),
            ld(4, &cat(&[ld(1, b"call1"), ld(2, b"list_dir"), ld(3, br#"{}"#)])),
            ld(30, b"List proj"),
        ]);
        let result = cat(&[
            vint(1, 9),
            ld(5, &meta5),
            ld(15, &ld(3, b"main.rs\nlib.rs and more entries here")),
        ]);
        // idx3: write_to_file (type 5): FileEdit from args
        let write_args = br#"{"TargetFile":"/work/proj/x.rs","CodeContent":"fn main() {}\n","Overwrite":false}"#;
        let write_meta5 = cat(&[
            ld(1, &time_sub(1200)),
            ld(4, &cat(&[ld(1, b"call2"), ld(2, b"write_to_file"), ld(3, write_args)])),
            ld(30, b"Create x.rs"),
        ]);
        let write = cat(&[vint(1, 5), ld(5, &write_meta5)]);
        // idx4: planning title (type 23) — should be skipped
        let title = cat(&[vint(1, 23), ld(30, &ld(4, b"A Title"))]);

        for (idx, st, payload) in [
            (0i64, 14i64, prompt),
            (1, 15, assistant),
            (2, 9, result),
            (3, 5, write),
            (4, 23, title),
        ] {
            conn.execute(
                "INSERT INTO steps (idx, step_type, error_details, step_payload) VALUES (?1, ?2, x'', ?3)",
                rusqlite::params![idx, st, payload],
            )
            .unwrap();
        }
    }

    /// A time sub-message (seconds, nanos) for the step_ts chain.
    fn time_sub(secs: u64) -> Vec<u8> {
        cat(&[vint(1, secs), vint(2, 0)])
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ag_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_conversation_with_metadata() {
        let dir = tmp("disc");
        let db = dir.join("conv1.db");
        let _ = std::fs::remove_file(&db);
        make_db(&db);

        let found = discover_in(&dir, None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].agent, Agent::Antigravity);
        assert_eq!(found[0].session_id, "conv1");
        assert_eq!(found[0].cwd, "/work/proj");
        assert_eq!(found[0].summary, "please fix it");
        // cwd filter
        assert_eq!(discover_in(&dir, Some("/nope")).len(), 0);
        assert_eq!(discover_in(&dir, Some("/work/proj")).len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_normalizes_steps_in_order() {
        let dir = tmp("norm");
        let db = dir.join("conv1.db");
        let _ = std::fs::remove_file(&db);
        make_db(&db);

        let mut src = AntigravitySource::new(&db, "conv1");
        let events = src.backfill();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.name()).collect();
        assert_eq!(
            kinds,
            vec![
                "session_start",
                "user_prompt",
                "assistant_text",
                "tool_call",
                "tool_result",
                "tool_result",
                "file_edit",
                // type-23 title is skipped
            ]
        );
        assert!(events.iter().all(|e| e.source == "antigravity"));

        // SessionStart carries the workspace path.
        match &events[0].kind {
            EventKind::SessionStart { cwd, session_id, .. } => {
                assert_eq!(cwd, "/work/proj");
                assert_eq!(session_id, "conv1");
            }
            other => panic!("expected session_start, got {other:?}"),
        }

        // The announced tool call parsed its JSON args.
        let call = events.iter().find_map(|e| match &e.kind {
            EventKind::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        });
        let (name, input) = call.unwrap();
        assert_eq!(name, "list_dir");
        assert_eq!(input.get("DirectoryPath").and_then(|v| v.as_str()), Some("/work/proj"));

        // The list_dir result body came from the longest non-tool-block text leaf.
        let detail = events.iter().find_map(|e| match &e.kind {
            EventKind::ToolResult { name, detail, summary, .. } if name == "list_dir" => {
                Some((detail.clone(), summary.clone()))
            }
            _ => None,
        });
        let (detail, summary) = detail.unwrap();
        assert!(detail.contains("main.rs"));
        assert_eq!(summary, "List proj");

        // write_to_file became a created FileEdit on the target path.
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
