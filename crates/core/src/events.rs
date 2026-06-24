//! Normalized event vocabulary shared by every source adapter.

use chrono::{DateTime, FixedOffset, Local};
use serde_json::Value;

/// Mirrors Claude Code's `structuredPatch` entries. Each `lines` entry is already
/// prefixed with ' ' (context), '+' (added), or '-' (removed).
#[derive(Debug, Clone)]
pub struct Hunk {
    pub old_start: i64,
    pub old_lines: i64,
    pub new_start: i64,
    pub new_lines: i64,
    pub lines: Vec<String>,
}

/// Kind-specific payload. The compiler enforces exhaustive handling everywhere.
#[derive(Debug, Clone)]
pub enum EventKind {
    SessionStart { session_id: String, cwd: String, version: String },
    UserPrompt { text: String },
    AssistantText { text: String },
    Thinking { text: String, summary: Option<String> },
    ToolCall { id: String, name: String, input: Value },
    ToolResult { id: String, name: String, ok: bool, summary: String, detail: String },
    FileEdit {
        path: String,
        hunks: Vec<Hunk>,
        added: usize,
        removed: usize,
        original: Option<String>,
        created: bool,
    },
    TurnEnd { reason: String },
    Notification { text: String },
}

impl EventKind {
    /// Stable snake_case name (matches the Python `Kind` values).
    pub fn name(&self) -> &'static str {
        match self {
            EventKind::SessionStart { .. } => "session_start",
            EventKind::UserPrompt { .. } => "user_prompt",
            EventKind::AssistantText { .. } => "assistant_text",
            EventKind::Thinking { .. } => "thinking",
            EventKind::ToolCall { .. } => "tool_call",
            EventKind::ToolResult { .. } => "tool_result",
            EventKind::FileEdit { .. } => "file_edit",
            EventKind::TurnEnd { .. } => "turn_end",
            EventKind::Notification { .. } => "notification",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Event {
    pub kind: EventKind,
    pub ts: DateTime<FixedOffset>,
    pub source: String,
    pub turn_id: Option<String>,
    pub is_sidechain: bool,
    pub raw_ref: Option<String>,
}

impl Event {
    pub fn new(
        kind: EventKind,
        ts: DateTime<FixedOffset>,
        turn_id: Option<String>,
        is_sidechain: bool,
        raw_ref: Option<String>,
    ) -> Self {
        Event { kind, ts, source: "claude-code".to_string(), turn_id, is_sidechain, raw_ref }
    }
}

/// Fallback "now" as a fixed-offset datetime (when a record lacks a timestamp).
pub fn now_fixed() -> DateTime<FixedOffset> {
    Local::now().fixed_offset()
}

#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub tool: String,
    pub session_id: String,
    pub cwd: String,
    pub title: Option<String>,
}
