//! Render the event store into a compact transcript for the comprehension model.

use serde_json::Value;

use crate::events::{Event, EventKind};
use crate::store::{basename, EventStore};

pub fn render_activity(store: &EventStore, max_events: usize, max_chars: usize) -> String {
    let start = store.events.len().saturating_sub(max_events);
    let mut lines: Vec<String> = Vec::new();
    for ev in &store.events[start..] {
        let line = serialize(ev);
        if !line.is_empty() {
            lines.push(line);
        }
    }
    let mut text = lines.join("\n");
    if text.len() > max_chars {
        let mut cut = text.len() - max_chars;
        while cut < text.len() && !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = format!("…(earlier activity elided)…\n{}", &text[cut..]);
    }
    if text.is_empty() {
        "(no activity yet)".to_string()
    } else {
        text
    }
}

/// One-line, most-recent-last representation of a single event (used by the
/// headless CLI and the activity feed).
pub fn event_line(ev: &Event) -> String {
    serialize(ev)
}

fn serialize(ev: &Event) -> String {
    let t = ev.ts.format("%H:%M:%S");
    let tag = if ev.is_sidechain { " [subagent]" } else { "" };
    match &ev.kind {
        EventKind::UserPrompt { text } => format!("{t} USER{tag}: {}", clip(text, 400)),
        EventKind::AssistantText { text } => format!("{t} ASSISTANT{tag}: {}", clip(text, 400)),
        EventKind::Thinking { text, .. } => {
            let body = if text.trim().is_empty() {
                "(not exposed)".to_string()
            } else {
                clip(text, 400)
            };
            format!("{t} THINKING{tag}: {body}")
        }
        EventKind::ToolCall { name, input, .. } => {
            format!("{t} TOOL→ {name}({}){tag}", clip(&args(input), 160))
        }
        EventKind::ToolResult { name, ok, summary, .. } => {
            let status = if *ok { "ok" } else { "ERROR" };
            format!("{t} TOOL← {name} {status}: {}{tag}", clip(summary, 160))
        }
        EventKind::FileEdit { path, added, removed, created, .. } => {
            let verb = if *created { "CREATE" } else { "EDIT" };
            format!("{t} {verb} {} +{added}-{removed}{tag}", basename(path))
        }
        EventKind::SessionStart { cwd, .. } => format!("{t} SESSION cwd={cwd}"),
        _ => String::new(),
    }
}

fn args(input: &Value) -> String {
    for key in ["command", "file_path", "path", "pattern", "query", "url"] {
        if let Some(v) = input.get(key) {
            let s = match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            if !s.is_empty() {
                return format!("{key}={s}");
            }
        }
    }
    String::new()
}

/// Collapse whitespace and clip to `n` characters (char count, matching Python).
pub fn clip(s: &str, n: usize) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= n {
        collapsed
    } else {
        let truncated: String = collapsed.chars().take(n.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
