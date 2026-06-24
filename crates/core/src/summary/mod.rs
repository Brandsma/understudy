//! Summaries: Tier-1 deterministic line, Tier-2 "what & why", and thought-pattern.

use crate::context::{clip, render_activity};
use crate::filters::strip_think;
use crate::models::{complete, ChatMessage, Provider, ProviderError};
use crate::store::EventStore;

/// Tier-1: instant, free, recomputed on every event from store indices.
pub fn summary_line(store: &EventStore) -> String {
    let mut parts = vec![store.last_action.clone()];
    if !store.files_touched.is_empty() {
        parts.push(format!("{} file(s) touched", store.files_touched.len()));
    }
    if let Some((name, ok)) = &store.last_tool {
        parts.push(format!("last: {} {}", name, if *ok { "✓" } else { "✗" }));
    }
    if store.error_count > 0 {
        parts.push(format!("{} error(s)", store.error_count));
    }
    let mut line = parts.join("   ·   ");

    if !store.turn_tool_counts.is_empty() {
        let mut pairs: Vec<(&String, &usize)> = store.turn_tool_counts.iter().collect();
        pairs.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        let hist = pairs
            .iter()
            .take(6)
            .map(|(name, count)| format!("{name}×{count}"))
            .collect::<Vec<_>>()
            .join("  ");
        line.push_str(&format!("\ntools this turn: {hist}"));
    }
    line
}

const LIVE_SYS: &str = "You summarize another coding agent's recent activity for someone watching over its shoulder.";
const LIVE_PROMPT: &str = "In ONE or TWO short sentences, say what the agent is doing right now and why. \
Be specific (name files and tools). No preamble, no bullet points, no markdown.";

/// Build the Tier-2 "what & why" prompt messages over the rolling window. Exposed so a
/// caller (e.g. the TUI) can run the request as a detached `'static` stream itself.
pub fn live_summary_messages(store: &EventStore) -> Vec<ChatMessage> {
    let activity = render_activity(store, 80, 6000);
    vec![
        ChatMessage::system(LIVE_SYS),
        ChatMessage::user(format!("{LIVE_PROMPT}\n\n=== ACTIVITY ===\n{activity}")),
    ]
}

/// Tier-2: debounced "what & why" over the rolling window.
pub async fn live_summary(provider: &Provider, store: &EventStore) -> Result<String, ProviderError> {
    Ok(strip_think(&complete(provider, live_summary_messages(store)).await?))
}

const THINK_SYS: &str = "You distill another coding agent's chain-of-thought into a short 'thought pattern' for an observer.";
const THINK_PROMPT: &str = "Summarize the reasoning below in ONE short sentence naming the key decision or \
self-correction — e.g. \"considered moving X to Y, then chose to rename instead\". No preamble, no markdown, no quotes.";

/// Distill a thinking block into a one-line thought pattern.
pub async fn summarize_thinking(provider: &Provider, text: &str) -> Result<String, ProviderError> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(String::new());
    }
    let messages = vec![
        ChatMessage::system(THINK_SYS),
        ChatMessage::user(format!("{THINK_PROMPT}\n\n=== REASONING ===\n{}", clip(text, 4000))),
    ];
    let raw = strip_think(&complete(provider, messages).await?);
    Ok(tidy_line(&raw))
}

/// Collapse a possibly-rambly reply to one clean line for a title.
fn tidy_line(raw: &str) -> String {
    for line in raw.lines() {
        let line = line.trim().trim_start_matches(['-', '•', '*', ' ']).trim_matches('"');
        if !line.is_empty() {
            return clip(line, 160);
        }
    }
    String::new()
}
