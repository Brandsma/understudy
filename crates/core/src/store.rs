//! In-memory event store with the derived indices the deterministic summary needs.

use std::collections::HashMap;

use crate::events::{Event, EventKind};

pub struct EventStore {
    pub events: Vec<Event>,
    pub files_touched: HashMap<String, usize>,
    pub tool_counts: HashMap<String, usize>,
    pub turn_tool_counts: HashMap<String, usize>,
    pub last_tool: Option<(String, bool)>,
    pub last_action: String,
    pub current_turn: Option<String>,
    pub error_count: usize,
}

impl Default for EventStore {
    fn default() -> Self {
        EventStore {
            events: Vec::new(),
            files_touched: HashMap::new(),
            tool_counts: HashMap::new(),
            turn_tool_counts: HashMap::new(),
            last_tool: None,
            last_action: "waiting…".to_string(),
            current_turn: None,
            error_count: 0,
        }
    }
}

impl EventStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, ev: Event) {
        match &ev.kind {
            EventKind::UserPrompt { .. } => {
                self.current_turn = ev.turn_id.clone();
                self.turn_tool_counts.clear();
                self.last_action = "reading user prompt".to_string();
            }
            EventKind::ToolCall { name, .. } => {
                *self.tool_counts.entry(name.clone()).or_default() += 1;
                *self.turn_tool_counts.entry(name.clone()).or_default() += 1;
                self.last_action = format!("calling {name}");
            }
            EventKind::ToolResult { name, ok, .. } => {
                self.last_tool = Some((name.clone(), *ok));
                if !*ok {
                    self.error_count += 1;
                }
            }
            EventKind::FileEdit { path, .. } => {
                *self.files_touched.entry(path.clone()).or_default() += 1;
                self.last_action = format!("editing {}", basename(path));
            }
            EventKind::Thinking { .. } => self.last_action = "thinking".to_string(),
            EventKind::AssistantText { .. } => self.last_action = "responding".to_string(),
            _ => {}
        }
        self.events.push(ev);
    }

    pub fn bulk_add(&mut self, events: Vec<Event>) {
        for ev in events {
            self.add(ev);
        }
    }
}

pub(crate) fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}
