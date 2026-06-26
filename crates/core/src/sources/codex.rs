//! Codex CLI source adapter: discovery, JSONL tailing, and normalization.
//!
//! OpenAI's Codex CLI records each session as an append-only "rollout" JSONL file under
//! `~/.codex/sessions/<YYYY>/<MM>/<DD>/rollout-<timestamp>-<uuid>.jsonl` (honoring `CODEX_HOME`).
//! Every line is an envelope `{ "timestamp", "type", "payload" }`. The first line is a
//! `session_meta` record (id, cwd, git, cli_version); the conversation itself is a stream of
//! `response_item` records mirroring the OpenAI Responses API (messages, reasoning, function
//! calls and their outputs). Other record types (`event_msg`, `turn_context`, `compacted`) are
//! UI/bookkeeping duplicates of the response-item stream and are skipped.
//!
//! Discovery and the rollout envelope were verified against real files; the richer
//! `response_item` payload variants follow `openai/codex`'s `protocol/src/models.rs`. See
//! docs/codex-integration.md.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, FixedOffset};
use serde_json::Value;

use crate::context::clip;
use crate::events::{now_fixed, Event, EventKind};
use crate::sources::{Agent, SessionInfo, Source};

/// Most recent events returned by a backfill (bounds cost on long sessions).
const BACKFILL_LIMIT: usize = 400;

// --------------------------------------------------------------------------- //
// Locations
// --------------------------------------------------------------------------- //

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Codex's home directory: `$CODEX_HOME`, else `~/.codex`.
fn codex_home() -> PathBuf {
    std::env::var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|_| home().join(".codex"))
}

fn sessions_dir() -> PathBuf {
    codex_home().join("sessions")
}

// --------------------------------------------------------------------------- //
// Discovery
// --------------------------------------------------------------------------- //

/// All Codex sessions, newest first. Optionally filter by cwd.
pub fn discover_sessions(cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    collect_rollouts(&sessions_dir(), &mut out);
    let mut sessions: Vec<SessionInfo> = out
        .into_iter()
        .filter_map(|p| read_session_meta(&p))
        .filter(|info| cwd_filter.is_none_or(|f| info.cwd == f))
        .collect();
    sessions.sort_by(|a, b| b.modified.cmp(&a.modified));
    sessions
}

/// Recursively gather `rollout-*.jsonl` files under the date-sharded sessions tree.
fn collect_rollouts(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rollouts(&path, out);
        } else if is_rollout(&path) {
            out.push(path);
        }
    }
}

fn is_rollout(path: &Path) -> bool {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    name.starts_with("rollout-") && name.ends_with(".jsonl")
}

fn read_session_meta(path: &Path) -> Option<SessionInfo> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let payload = first_session_meta(path)?;
    let git = payload.get("git");
    Some(SessionInfo {
        agent: Agent::Codex,
        path: path.to_path_buf(),
        session_id: payload
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| stem_uuid(path)),
        cwd: payload.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        git_branch: git.and_then(|g| g.get("branch")).and_then(|v| v.as_str()).unwrap_or("").to_string(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: meta.len(),
        summary: clip(session_summary(path).trim(), 90),
    })
}

/// The `session_meta` payload from the first line of a rollout file.
fn first_session_meta(path: &Path) -> Option<Value> {
    use std::io::BufRead;
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines().take(5).map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let rec: Value = serde_json::from_str(line).ok()?;
        if rec.get("type").and_then(|v| v.as_str()) == Some("session_meta") {
            return rec.get("payload").cloned();
        }
    }
    None
}

/// The trailing UUID of a `rollout-<ts>-<uuid>.jsonl` filename (discovery fallback id).
fn stem_uuid(path: &Path) -> String {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    // The UUID is the last 5 dash-separated groups.
    let parts: Vec<&str> = stem.split('-').collect();
    if parts.len() >= 5 {
        parts[parts.len() - 5..].join("-")
    } else {
        stem.to_string()
    }
}

/// A best-effort one-liner for the picker: the first real user prompt.
fn session_summary(path: &Path) -> String {
    use std::io::BufRead;
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    for line in std::io::BufReader::new(file).lines().take(200).map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if rec.get("type").and_then(|v| v.as_str()) != Some("response_item") {
            continue;
        }
        let payload = rec.get("payload");
        if payload.and_then(|p| p.get("type")).and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        if payload.and_then(|p| p.get("role")).and_then(|v| v.as_str()) != Some("user") {
            continue;
        }
        let text = message_text(payload.unwrap());
        if !text.is_empty() && !is_injected(&text) {
            return text;
        }
    }
    String::new()
}

// --------------------------------------------------------------------------- //
// Source
// --------------------------------------------------------------------------- //

pub struct CodexSource {
    path: PathBuf,
    offset: u64,
    buf: String,
    tool_names: HashMap<String, String>, // call_id -> tool name
    current_turn: Option<String>,
    started: bool,
}

impl CodexSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        CodexSource {
            path: path.into(),
            offset: 0,
            buf: String::new(),
            tool_names: HashMap::new(),
            current_turn: None,
            started: false,
        }
    }

    /// Read appended bytes since the last offset, splitting into complete lines.
    fn read_new_lines(&mut self) -> Vec<String> {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return Vec::new();
        };
        let size = meta.len();
        if size < self.offset {
            self.offset = 0; // truncated / rotated
            self.buf.clear();
        }
        if size == self.offset {
            return Vec::new();
        }
        let Ok(mut file) = std::fs::File::open(&self.path) else {
            return Vec::new();
        };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return Vec::new();
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            return Vec::new();
        }
        self.offset += bytes.len() as u64;
        self.buf.push_str(&String::from_utf8_lossy(&bytes));

        let buf = std::mem::take(&mut self.buf);
        let mut parts: Vec<String> = buf.split('\n').map(|s| s.to_string()).collect();
        self.buf = parts.pop().unwrap_or_default(); // trailing partial line
        parts.into_iter().filter(|p| !p.trim().is_empty()).collect()
    }

    fn normalize_line(&mut self, line: &str) -> Vec<Event> {
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            return Vec::new();
        };
        let ts = ts(&rec);
        match rec.get("type").and_then(|v| v.as_str()) {
            Some("session_meta") => self.session_start(rec.get("payload"), ts),
            Some("response_item") => match rec.get("payload") {
                Some(p) => self.from_response_item(p, ts),
                None => Vec::new(),
            },
            _ => Vec::new(), // event_msg / turn_context / compacted: duplicates or bookkeeping
        }
    }

    fn session_start(&mut self, payload: Option<&Value>, ts: DateTime<FixedOffset>) -> Vec<Event> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let p = payload.cloned().unwrap_or(Value::Null);
        vec![self.ev(
            EventKind::SessionStart {
                session_id: p.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                cwd: p.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                version: p.get("cli_version").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            },
            ts,
        )]
    }

    fn from_response_item(&mut self, p: &Value, ts: DateTime<FixedOffset>) -> Vec<Event> {
        match p.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message" => self.from_message(p, ts),
            "reasoning" => {
                let text = reasoning_text(p);
                if text.is_empty() {
                    Vec::new()
                } else {
                    vec![self.ev(EventKind::Thinking { text, summary: None }, ts)]
                }
            }
            "function_call" => {
                let id = vstr(p, "call_id").to_string();
                let name = vstr(p, "name").to_string();
                self.tool_names.insert(id.clone(), name.clone());
                let input = serde_json::from_str(vstr(p, "arguments"))
                    .unwrap_or_else(|_| Value::String(vstr(p, "arguments").to_string()));
                vec![self.ev(EventKind::ToolCall { id, name, input }, ts)]
            }
            "local_shell_call" => {
                let id = p.get("call_id").and_then(|v| v.as_str()).or_else(|| p.get("id").and_then(|v| v.as_str())).unwrap_or("").to_string();
                let command = shell_command(p);
                self.tool_names.insert(id.clone(), "shell".to_string());
                vec![self.ev(
                    EventKind::ToolCall { id, name: "shell".to_string(), input: serde_json::json!({ "command": command }) },
                    ts,
                )]
            }
            "custom_tool_call" => {
                let id = vstr(p, "call_id").to_string();
                let name = vstr(p, "name").to_string();
                self.tool_names.insert(id.clone(), name.clone());
                let input = serde_json::from_str(vstr(p, "input"))
                    .unwrap_or_else(|_| Value::String(vstr(p, "input").to_string()));
                vec![self.ev(EventKind::ToolCall { id, name, input }, ts)]
            }
            "function_call_output" | "custom_tool_call_output" => {
                let id = vstr(p, "call_id").to_string();
                let name = self.tool_names.get(&id).cloned().unwrap_or_else(|| "tool".to_string());
                let (text, ok) = output_text_ok(p.get("output"));
                vec![self.ev(
                    EventKind::ToolResult {
                        id,
                        name,
                        ok,
                        summary: clip(text.lines().next().unwrap_or(""), 80),
                        detail: clip(&text, 2000),
                    },
                    ts,
                )]
            }
            "web_search_call" => {
                let query = p
                    .get("action")
                    .and_then(|a| a.get("query"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                vec![self.ev(
                    EventKind::ToolCall {
                        id: vstr(p, "id").to_string(),
                        name: "web_search".to_string(),
                        input: serde_json::json!({ "query": query }),
                    },
                    ts,
                )]
            }
            _ => Vec::new(), // image_generation_call / tool_search_* / compaction / additional_tools
        }
    }

    fn from_message(&mut self, p: &Value, ts: DateTime<FixedOffset>) -> Vec<Event> {
        let role = vstr(p, "role");
        if role == "developer" {
            return Vec::new(); // injected instructions, not conversation
        }
        let text = message_text(p);
        if text.trim().is_empty() {
            return Vec::new();
        }
        if role == "user" {
            if is_injected(&text) {
                return Vec::new(); // Codex-generated environment/context blocks
            }
            self.current_turn = vstr_opt(p, "id").or_else(|| self.current_turn.clone());
            vec![self.ev(EventKind::UserPrompt { text }, ts)]
        } else {
            vec![self.ev(EventKind::AssistantText { text }, ts)]
        }
    }

    fn ev(&self, kind: EventKind, ts: DateTime<FixedOffset>) -> Event {
        Event::new(kind, ts, self.current_turn.clone(), false, None).with_source("codex")
    }
}

impl Source for CodexSource {
    fn backfill(&mut self) -> Vec<Event> {
        self.offset = 0;
        self.buf.clear();
        let mut events = Vec::new();
        for line in self.read_new_lines() {
            events.extend(self.normalize_line(&line));
        }
        if events.len() > BACKFILL_LIMIT {
            // Keep the SessionStart (if first) plus the most recent events.
            let keep_start = matches!(events.first().map(|e| &e.kind), Some(EventKind::SessionStart { .. }));
            let tail = events.split_off(events.len() - BACKFILL_LIMIT);
            if keep_start {
                let start = std::mem::take(&mut events).into_iter().next().unwrap();
                let mut out = vec![start];
                out.extend(tail);
                return out;
            }
            return tail;
        }
        events
    }

    fn read_new(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for line in self.read_new_lines() {
            events.extend(self.normalize_line(&line));
        }
        events
    }
}

// --------------------------------------------------------------------------- //
// Normalization helpers (pure)
// --------------------------------------------------------------------------- //

fn vstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

fn vstr_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string())
}

fn ts(rec: &Value) -> DateTime<FixedOffset> {
    rec.get("timestamp")
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .unwrap_or_else(now_fixed)
}

/// Join the `text` of a message's content blocks (`input_text` / `output_text`).
fn message_text(p: &Value) -> String {
    let Some(arr) = p.get("content").and_then(|c| c.as_array()) else {
        return String::new();
    };
    arr.iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
        .collect::<Vec<_>>()
        .join("")
}

/// Reasoning text: the visible `summary` blocks, falling back to `content` blocks.
fn reasoning_text(p: &Value) -> String {
    let texts = |key: &str| -> String {
        p.get(key)
            .and_then(|c| c.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    let summary = texts("summary");
    if summary.trim().is_empty() {
        texts("content")
    } else {
        summary
    }
}

/// The shell command for a `local_shell_call`, joined into a single line.
fn shell_command(p: &Value) -> String {
    p.get("action")
        .and_then(|a| a.get("command"))
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ")
        })
        .unwrap_or_default()
}

/// Whether a user message is a Codex-injected context block rather than a typed prompt.
fn is_injected(text: &str) -> bool {
    let t = text.trim_start();
    t.starts_with("<environment_context>") || t.starts_with("<user_instructions>")
}

/// Extract human-readable text and a success flag from a `*_output` payload's `output` field,
/// which is either a plain string or `{ "content"/"output": …, "success": bool }`.
fn output_text_ok(output: Option<&Value>) -> (String, bool) {
    match output {
        Some(Value::String(s)) => (s.clone(), true),
        Some(Value::Object(m)) => {
            let ok = m.get("success").and_then(|b| b.as_bool()).unwrap_or(true);
            let text = m
                .get("content")
                .and_then(|c| c.as_str())
                .or_else(|| m.get("output").and_then(|c| c.as_str()))
                .map(|s| s.to_string())
                .or_else(|| m.get("content_items").map(content_items_text))
                .unwrap_or_else(|| Value::Object(m.clone()).to_string());
            (text, ok)
        }
        Some(other) => (other.to_string(), true),
        None => (String::new(), true),
    }
}

/// Join the `text` of structured `content_items` (the array form of a tool output body).
fn content_items_text(items: &Value) -> String {
    items
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Write a rollout JSONL file from pre-built envelope lines.
    fn write_rollout(path: &Path, lines: &[Value]) {
        let mut f = std::fs::File::create(path).unwrap();
        for l in lines {
            writeln!(f, "{}", serde_json::to_string(l).unwrap()).unwrap();
        }
    }

    fn meta_line() -> Value {
        serde_json::json!({
            "timestamp": "2026-06-25T20:06:14.258Z",
            "type": "session_meta",
            "payload": {
                "id": "sess-1",
                "cwd": "/work/proj",
                "originator": "codex_cli_rs",
                "cli_version": "0.34.0",
                "git": { "branch": "main", "commit_hash": "abc" }
            }
        })
    }

    fn item(ts: &str, payload: Value) -> Value {
        serde_json::json!({ "timestamp": ts, "type": "response_item", "payload": payload })
    }

    fn sample() -> Vec<Value> {
        vec![
            meta_line(),
            // Codex-injected env context (user role) — must be skipped.
            item("2026-06-25T20:06:14.300Z", serde_json::json!({
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": "<environment_context>\n  <cwd>/work/proj</cwd>\n</environment_context>" }]
            })),
            // Real user prompt.
            item("2026-06-25T20:06:15.000Z", serde_json::json!({
                "type": "message", "role": "user",
                "content": [{ "type": "input_text", "text": "please fix the parser" }]
            })),
            // Reasoning.
            item("2026-06-25T20:06:16.000Z", serde_json::json!({
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": "Looking at the parser" }]
            })),
            // Assistant message.
            item("2026-06-25T20:06:17.000Z", serde_json::json!({
                "type": "message", "role": "assistant",
                "content": [{ "type": "output_text", "text": "I'll run the tests." }]
            })),
            // Function call + its output.
            item("2026-06-25T20:06:18.000Z", serde_json::json!({
                "type": "function_call", "name": "shell",
                "arguments": "{\"command\":[\"cargo\",\"test\"]}", "call_id": "c1"
            })),
            item("2026-06-25T20:06:19.000Z", serde_json::json!({
                "type": "function_call_output", "call_id": "c1",
                "output": { "content": "test result: ok. 5 passed", "success": true }
            })),
            // Developer message — skipped.
            item("2026-06-25T20:06:20.000Z", serde_json::json!({
                "type": "message", "role": "developer",
                "content": [{ "type": "input_text", "text": "<user_instructions>be nice</user_instructions>" }]
            })),
        ]
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cx_{name}_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn discovers_rollout_with_metadata() {
        let dir = tmp("disc");
        // Mirror the date-sharded layout to exercise the recursive walk.
        let nested = dir.join("2026/06/25");
        std::fs::create_dir_all(&nested).unwrap();
        let file = nested.join("rollout-2026-06-25T20-06-14-0bb5c8aa-d732-45d1-aead-605cd78bf9f3.jsonl");
        write_rollout(&file, &sample());

        let found: Vec<SessionInfo> = {
            let mut out = Vec::new();
            collect_rollouts(&dir, &mut out);
            out.into_iter().filter_map(|p| read_session_meta(&p)).collect()
        };
        assert_eq!(found.len(), 1);
        let s = &found[0];
        assert_eq!(s.agent, Agent::Codex);
        assert_eq!(s.session_id, "sess-1");
        assert_eq!(s.cwd, "/work/proj");
        assert_eq!(s.git_branch, "main");
        assert_eq!(s.summary, "please fix the parser"); // skipped the injected env block

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn backfill_normalizes_response_items() {
        let dir = tmp("norm");
        let file = dir.join("rollout-2026-06-25T20-06-14-0bb5c8aa-d732-45d1-aead-605cd78bf9f3.jsonl");
        write_rollout(&file, &sample());

        let mut src = CodexSource::new(&file);
        let events = src.backfill();
        let kinds: Vec<&str> = events.iter().map(|e| e.kind.name()).collect();
        assert_eq!(
            kinds,
            vec![
                "session_start",
                "user_prompt", // injected env block was skipped
                "thinking",
                "assistant_text",
                "tool_call",
                "tool_result",
                // developer message skipped
            ]
        );
        assert!(events.iter().all(|e| e.source == "codex"));

        match &events[0].kind {
            EventKind::SessionStart { session_id, cwd, version } => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(cwd, "/work/proj");
                assert_eq!(version, "0.34.0");
            }
            other => panic!("expected session_start, got {other:?}"),
        }

        // function_call parsed its JSON arguments; the output paired by call_id.
        let call = events.iter().find_map(|e| match &e.kind {
            EventKind::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
            _ => None,
        });
        let (name, input) = call.unwrap();
        assert_eq!(name, "shell");
        assert_eq!(input.get("command").and_then(|c| c.as_array()).map(|a| a.len()), Some(2));

        let result = events.iter().find_map(|e| match &e.kind {
            EventKind::ToolResult { name, ok, detail, .. } => Some((name.clone(), *ok, detail.clone())),
            _ => None,
        });
        assert_eq!(result, Some(("shell".to_string(), true, "test result: ok. 5 passed".to_string())));

        // Nothing new on a second poll.
        assert!(src.read_new().is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
