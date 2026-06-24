//! Comprehension chat: the system prompt + context builder. The conversation is
//! completely decoupled from the observed agent (read-only).

use crate::context::render_activity;
use crate::models::ChatMessage;
use crate::store::EventStore;

pub const SYSTEM: &str = "You are Understudy, a read-only observer of ANOTHER coding agent's live \
session. You cannot act, edit files, run tools, or message that agent — you only explain what it is \
doing and why, grounded in the activity stream below. Be concise and concrete: name the specific \
files, tools, and steps. If the answer isn't in the stream, say so rather than guessing.";

/// The system message: prompt + the agent's activity stream as context. Rebuilt each
/// turn so answers reflect the agent's latest state.
pub fn system_with_activity(store: &EventStore) -> ChatMessage {
    let activity = render_activity(store, 160, 10000);
    ChatMessage::system(format!("{SYSTEM}\n\n=== AGENT ACTIVITY (most recent last) ===\n{activity}"))
}
