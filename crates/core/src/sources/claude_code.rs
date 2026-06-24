//! Claude Code source adapter: discovery, JSONL tailing, and normalization.
//! Verified against real transcripts — see docs/claude-code-integration.md.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use chrono::{DateTime, FixedOffset};
use serde_json::Value;

use crate::context::clip;
use crate::events::{now_fixed, Event, EventKind, Hunk, SourceInfo};
use crate::store::basename;

/// Cap synthesized "new file" diffs so a huge Write can't flood the detail pane.
const MAX_CREATE_LINES: usize = 400;

// --------------------------------------------------------------------------- //
// Locations
// --------------------------------------------------------------------------- //

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_dir() -> PathBuf {
    std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".claude"))
}

pub fn projects_dir() -> PathBuf {
    config_dir().join("projects")
}

// --------------------------------------------------------------------------- //
// Discovery
// --------------------------------------------------------------------------- //

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub session_id: String,
    pub cwd: String,
    pub git_branch: String,
    pub modified: SystemTime,
    pub size: u64,
    pub summary: String,
}

/// All Claude Code sessions, newest first. Optionally filter by cwd.
pub fn discover_sessions(cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    let mut out = Vec::new();
    let Ok(subdirs) = std::fs::read_dir(projects_dir()) else {
        return out;
    };
    for sub in subdirs.flatten() {
        let dir = sub.path();
        if !dir.is_dir() {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for f in files.flatten() {
            let p = f.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(info) = read_session_meta(&p) {
                if let Some(filter) = cwd_filter {
                    if info.cwd != filter {
                        continue;
                    }
                }
                out.push(info);
            }
        }
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

pub fn read_session_meta(path: &Path) -> Option<SessionInfo> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() == 0 {
        return None;
    }
    let head = head_meta(path, 20);
    if head.is_empty() {
        return None;
    }
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    Some(SessionInfo {
        path: path.to_path_buf(),
        session_id: head.get("sessionId").and_then(|v| v.as_str()).unwrap_or(stem).to_string(),
        cwd: head.get("cwd").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        git_branch: head.get("gitBranch").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        size: meta.len(),
        summary: session_summary(path, 65536),
    })
}

/// Merge identity fields from the first records (early lines can be
/// `queue-operation`/`summary` records that lack `cwd`/`gitBranch`).
fn head_meta(path: &Path, max_lines: usize) -> serde_json::Map<String, Value> {
    use std::io::BufRead;
    let mut meta = serde_json::Map::new();
    let Ok(file) = std::fs::File::open(path) else {
        return meta;
    };
    for (i, line) in std::io::BufReader::new(file).lines().enumerate() {
        if i >= max_lines {
            break;
        }
        let Ok(line) = line else { continue };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        for key in ["sessionId", "cwd", "gitBranch", "version"] {
            if !meta.contains_key(key) {
                if let Some(v) = rec.get(key) {
                    if v.as_str().map(|s| !s.is_empty()).unwrap_or(false) {
                        meta.insert(key.to_string(), v.clone());
                    }
                }
            }
        }
        if meta.contains_key("cwd") && meta.contains_key("sessionId") {
            break;
        }
    }
    meta
}

fn read_tail(path: &Path, n: u64) -> String {
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let size = file.seek(SeekFrom::End(0)).unwrap_or(0);
    if file.seek(SeekFrom::Start(size.saturating_sub(n))).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Cheap, best-effort one-liner from the file tail for the picker.
fn session_summary(path: &Path, tail_bytes: u64) -> String {
    let data = read_tail(path, tail_bytes);
    let (mut title, mut last_prompt, mut last_user): (Option<String>, Option<String>, Option<String>) =
        (None, None, None);
    for line in data.split('\n').skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(rec) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match rec.get("type").and_then(|v| v.as_str()) {
            Some("ai-title") => {
                title = vstr_opt(&rec, "aiTitle").or_else(|| vstr_opt(&rec, "title")).or(title);
            }
            Some("last-prompt") => {
                last_prompt = vstr_opt(&rec, "lastPrompt").or_else(|| vstr_opt(&rec, "prompt")).or(last_prompt);
            }
            Some("user") => {
                if let Some(t) = user_text(&rec) {
                    last_user = Some(t);
                }
            }
            _ => {}
        }
    }
    one_line(&title.or(last_prompt).or(last_user).unwrap_or_default(), 90)
}

pub fn info_from_path(path: &Path) -> SessionInfo {
    if let Some(info) = read_session_meta(path) {
        return info;
    }
    let meta = std::fs::metadata(path).ok();
    SessionInfo {
        path: path.to_path_buf(),
        session_id: path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string(),
        cwd: String::new(),
        git_branch: String::new(),
        modified: meta.as_ref().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH),
        size: meta.map(|m| m.len()).unwrap_or(0),
        summary: String::new(),
    }
}

/// Resolve a `--session` token: an existing path, or a UUID to search for.
pub fn resolve_session(token: &str) -> Option<PathBuf> {
    let p = Path::new(token);
    if p.is_file() {
        return Some(p.to_path_buf());
    }
    if let Ok(subs) = std::fs::read_dir(projects_dir()) {
        for sub in subs.flatten() {
            let candidate = sub.path().join(format!("{token}.jsonl"));
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

// --------------------------------------------------------------------------- //
// Source
// --------------------------------------------------------------------------- //

pub struct ClaudeCodeSource {
    pub path: PathBuf,
    pub backfill_limit: usize,
    offset: u64,
    buf: String,
    tool_names: HashMap<String, String>, // tool_use_id -> tool name
    current_turn: Option<String>,
    started: bool,
}

impl ClaudeCodeSource {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        ClaudeCodeSource {
            path: path.into(),
            backfill_limit: 400,
            offset: 0,
            buf: String::new(),
            tool_names: HashMap::new(),
            current_turn: None,
            started: false,
        }
    }

    pub fn describe(&self) -> SourceInfo {
        if let Some(info) = read_session_meta(&self.path) {
            SourceInfo {
                tool: "claude-code".to_string(),
                session_id: info.session_id,
                cwd: info.cwd,
                title: (!info.summary.is_empty()).then_some(info.summary),
            }
        } else {
            SourceInfo {
                tool: "claude-code".to_string(),
                session_id: self.path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string(),
                cwd: String::new(),
                title: None,
            }
        }
    }

    /// Read the whole file once; return the last `backfill_limit` normalized events.
    pub fn backfill(&mut self) -> Vec<Event> {
        self.offset = 0;
        self.buf.clear();
        let mut events = Vec::new();
        for line in self.read_new_lines() {
            events.extend(self.normalize_line(&line));
        }
        let n = self.backfill_limit;
        if events.len() > n {
            events.split_off(events.len() - n)
        } else {
            events
        }
    }

    /// Read appended bytes since the last offset; normalize complete lines.
    pub fn read_new(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        for line in self.read_new_lines() {
            events.extend(self.normalize_line(&line));
        }
        events
    }

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
        match rec.get("type").and_then(|v| v.as_str()) {
            Some("assistant") => self.from_assistant(&rec),
            Some("user") => self.from_user(&rec),
            _ => Vec::new(),
        }
    }

    fn from_assistant(&mut self, rec: &Value) -> Vec<Event> {
        let ts = ts(rec);
        let side = vbool(rec, "isSidechain");
        let mut out = self.maybe_session_start(rec, ts);
        let Some(content) = rec.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_array()) else {
            return out;
        };
        for block in content {
            match block.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "text" => {
                    let text = vstr(block, "text");
                    if !text.trim().is_empty() {
                        out.push(self.ev(EventKind::AssistantText { text: text.to_string() }, ts, side, rec));
                    }
                }
                bt @ ("thinking" | "redacted_thinking") => {
                    let text = if bt == "thinking" { vstr(block, "thinking").to_string() } else { String::new() };
                    out.push(self.ev(EventKind::Thinking { text, summary: None }, ts, side, rec));
                }
                "tool_use" => {
                    let id = vstr(block, "id").to_string();
                    let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("tool").to_string();
                    self.tool_names.insert(id.clone(), name.clone());
                    let input = block.get("input").cloned().unwrap_or_else(|| Value::Object(Default::default()));
                    out.push(self.ev(EventKind::ToolCall { id, name, input }, ts, side, rec));
                }
                _ => {}
            }
        }
        out
    }

    fn from_user(&mut self, rec: &Value) -> Vec<Event> {
        let ts = ts(rec);
        let side = vbool(rec, "isSidechain");
        let mut out = self.maybe_session_start(rec, ts);
        if vbool(rec, "isMeta") {
            return out;
        }
        let content = rec.get("message").and_then(|m| m.get("content"));
        let result_block = content
            .and_then(|c| c.as_array())
            .and_then(|arr| arr.iter().find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result")));

        if let Some(block) = result_block {
            out.extend(self.from_tool_result(rec, block, ts, side));
            return out;
        }
        if let Some(text) = user_text(rec) {
            if !text.trim().is_empty() {
                let uuid = rec.get("uuid").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(|s| s.to_string());
                self.current_turn = uuid.or_else(|| self.current_turn.clone());
                out.push(self.ev(EventKind::UserPrompt { text }, ts, side, rec));
            }
        }
        out
    }

    fn from_tool_result(&self, rec: &Value, block: &Value, ts: DateTime<FixedOffset>, side: bool) -> Vec<Event> {
        let id = vstr(block, "tool_use_id").to_string();
        let name = self.tool_names.get(&id).cloned().unwrap_or_else(|| "tool".to_string());
        let ok = !vbool(block, "is_error");
        let tur = rec.get("toolUseResult");
        let (summary, detail) = summarize_result(tur, block, ok);
        let mut out = vec![self.ev(
            EventKind::ToolResult { id, name, ok, summary, detail },
            ts,
            side,
            rec,
        )];
        if let Some(tur) = tur {
            if tur.get("filePath").and_then(|v| v.as_str()).is_some() {
                if let Some(edit) = self.file_edit(tur, ts, side, rec) {
                    out.push(edit);
                }
            }
        }
        out
    }

    fn file_edit(&self, tur: &Value, ts: DateTime<FixedOffset>, side: bool, rec: &Value) -> Option<Event> {
        let path = tur.get("filePath").and_then(|v| v.as_str())?.to_string();
        let patch = tur.get("structuredPatch").and_then(|p| p.as_array());
        let content = tur.get("content").and_then(|v| v.as_str());
        let mut created = tur.get("type").and_then(|v| v.as_str()) == Some("create");

        let (hunks, added, removed) = if patch.map(|a| !a.is_empty()).unwrap_or(false) {
            let mut hunks = Vec::new();
            for h in patch.unwrap() {
                if !h.is_object() {
                    continue;
                }
                let lines = h
                    .get("lines")
                    .and_then(|l| l.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
                    .unwrap_or_default();
                hunks.push(Hunk {
                    old_start: vint(h, "oldStart"),
                    old_lines: vint(h, "oldLines"),
                    new_start: vint(h, "newStart"),
                    new_lines: vint(h, "newLines"),
                    lines,
                });
            }
            let added = hunks.iter().flat_map(|h| &h.lines).filter(|l| l.starts_with('+')).count();
            let removed = hunks.iter().flat_map(|h| &h.lines).filter(|l| l.starts_with('-')).count();
            (hunks, added, removed)
        } else if let Some(content) = content {
            // Write/create: no structured patch — synthesize an all-added diff.
            let mut lines: Vec<&str> = content.split('\n').collect();
            if lines.last() == Some(&"") {
                lines.pop();
            }
            let total = lines.len();
            let mut shown: Vec<String> = lines.iter().take(MAX_CREATE_LINES).map(|l| format!("+{l}")).collect();
            if total > MAX_CREATE_LINES {
                shown.push(format!("… (+{} more lines)", total - MAX_CREATE_LINES));
            }
            created = true;
            (vec![Hunk { old_start: 0, old_lines: 0, new_start: 1, new_lines: total as i64, lines: shown }], total, 0)
        } else {
            return None;
        };

        let original = tur.get("originalFile").and_then(|v| v.as_str()).map(|s| s.to_string());
        Some(self.ev(
            EventKind::FileEdit { path, hunks, added, removed, original, created },
            ts,
            side,
            rec,
        ))
    }

    fn ev(&self, kind: EventKind, ts: DateTime<FixedOffset>, side: bool, rec: &Value) -> Event {
        Event::new(kind, ts, self.current_turn.clone(), side, vstr_opt(rec, "uuid"))
    }

    fn maybe_session_start(&mut self, rec: &Value, ts: DateTime<FixedOffset>) -> Vec<Event> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![Event::new(
            EventKind::SessionStart {
                session_id: vstr(rec, "sessionId").to_string(),
                cwd: vstr(rec, "cwd").to_string(),
                version: vstr(rec, "version").to_string(),
            },
            ts,
            None,
            false,
            vstr_opt(rec, "uuid"),
        )]
    }
}

// --------------------------------------------------------------------------- //
// Module helpers
// --------------------------------------------------------------------------- //

fn vstr<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("")
}

fn vstr_opt(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(|s| s.to_string())
}

fn vbool(v: &Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

fn vint(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

fn ts(rec: &Value) -> DateTime<FixedOffset> {
    if let Some(s) = rec.get("timestamp").and_then(|v| v.as_str()) {
        if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
            return dt;
        }
    }
    now_fixed()
}

fn user_text(rec: &Value) -> Option<String> {
    let content = rec.get("message")?.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    let arr = content.as_array()?;
    if arr.iter().any(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_result")) {
        return None; // tool output, not a human prompt
    }
    let parts: Vec<&str> = arr
        .iter()
        .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
        .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
        .filter(|s| !s.is_empty())
        .collect();
    let joined = parts.join(" ");
    (!joined.is_empty()).then_some(joined)
}

fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter(|b| b.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|b| b.get("text").and_then(|v| v.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn summarize_result(tur: Option<&Value>, block: &Value, ok: bool) -> (String, String) {
    if let Some(tur) = tur.filter(|t| t.is_object()) {
        if let Some(fp) = tur.get("filePath").and_then(|v| v.as_str()) {
            if tur.get("structuredPatch").is_some() || tur.get("content").is_some() {
                let verb = if tur.get("type").and_then(|v| v.as_str()) == Some("create") { "created" } else { "edited" };
                let preview = tur
                    .get("newString")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .or_else(|| tur.get("content").and_then(|v| v.as_str()))
                    .unwrap_or("");
                return (format!("{verb} {}", basename(fp)), one_line(preview, 200));
            }
        }
        if tur.get("stdout").is_some() || tur.get("stderr").is_some() {
            let stdout = tur.get("stdout").and_then(|v| v.as_str()).unwrap_or("");
            let stderr = tur.get("stderr").and_then(|v| v.as_str()).unwrap_or("");
            let body = if !stdout.trim().is_empty() { stdout } else { stderr };
            let head = body.trim().lines().next().filter(|s| !s.is_empty()).unwrap_or("(no output)");
            let mut full = stdout.to_string();
            if !stderr.trim().is_empty() {
                full.push('\n');
                full.push_str(stderr);
            }
            return (one_line(head, 80), full.trim().to_string());
        }
        if let Some(results) = tur.get("results") {
            let count = tur
                .get("searchCount")
                .and_then(|v| v.as_i64())
                .or_else(|| results.as_array().map(|a| a.len() as i64));
            let count_str = count.map(|c| c.to_string()).unwrap_or_else(|| "None".to_string());
            let detail = if !results.is_null() {
                serde_json::to_string_pretty(results).unwrap_or_default().chars().take(4000).collect()
            } else {
                String::new()
            };
            return (format!("{count_str} results"), detail);
        }
    }
    let detail = content_to_text(block.get("content"));
    let head = if !detail.is_empty() {
        detail.clone()
    } else if ok {
        "ok".to_string()
    } else {
        "error".to_string()
    };
    (one_line(&head, 80), detail)
}

fn one_line(s: &str, limit: usize) -> String {
    clip(s, limit)
}
