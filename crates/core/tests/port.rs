//! Parity tests: the Rust port must agree with the Python suite on the same fixtures.

use std::path::PathBuf;

use understudy_core::config::{load_config, save_config, Config, ProviderConfig};
use understudy_core::context::render_activity;
use understudy_core::events::{Event, EventKind};
use understudy_core::filters::{strip_think, ThinkFilter};
use understudy_core::models::build_provider;
use understudy_core::sources::claude_code::{read_session_meta, ClaudeCodeSource};
use understudy_core::store::EventStore;

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

fn backfill(name: &str) -> Vec<Event> {
    ClaudeCodeSource::new(fixtures().join(name)).backfill()
}

fn kinds(evs: &[Event]) -> Vec<&'static str> {
    evs.iter().map(|e| e.kind.name()).collect()
}

// ---- normalizer (mirrors test_claude_normalizer.py) ----------------------- //

#[test]
fn event_kinds_in_order() {
    assert_eq!(
        kinds(&backfill("sample_session.jsonl")),
        vec![
            "session_start",
            "user_prompt",
            "thinking",
            "tool_call",   // Bash
            "tool_result", // Bash
            "tool_call",   // Edit
            "tool_result", // Edit
            "file_edit",   // derived from structuredPatch
            "assistant_text",
        ]
    );
}

#[test]
fn tool_results_named_via_tool_use_map() {
    let evs = backfill("sample_session.jsonl");
    let names: Vec<&str> = evs
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::ToolResult { name, ok, .. } => {
                assert!(*ok);
                Some(name.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["Bash", "Edit"]);
}

#[test]
fn file_edit_has_structured_diff() {
    let evs = backfill("sample_session.jsonl");
    let edit = evs
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::FileEdit { .. } => Some(&e.kind),
            _ => None,
        })
        .unwrap();
    let EventKind::FileEdit { path, added, removed, hunks, .. } = edit else {
        unreachable!()
    };
    assert!(path.ends_with("config.json"));
    assert_eq!(*added, 1);
    assert_eq!(*removed, 1);
    assert!(hunks[0].lines[1].starts_with('-'));
}

#[test]
fn turns_grouped_under_user_prompt() {
    let evs = backfill("sample_session.jsonl");
    let prompt_turn = evs
        .iter()
        .find(|e| matches!(e.kind, EventKind::UserPrompt { .. }))
        .unwrap()
        .turn_id
        .clone();
    for e in &evs {
        if matches!(e.kind, EventKind::ToolCall { .. }) {
            assert_eq!(e.turn_id, prompt_turn);
        }
    }
}

#[test]
fn session_meta() {
    let info = read_session_meta(&fixtures().join("sample_session.jsonl")).unwrap();
    assert_eq!(info.session_id, "s1");
    assert_eq!(info.cwd, "/Users/dev/proj");
    assert_eq!(info.git_branch, "main");
}

#[test]
fn meta_skips_leading_queue_operations() {
    let info = read_session_meta(&fixtures().join("write_create.jsonl")).unwrap();
    assert_eq!(info.cwd, "/Users/dev/proj2");
    assert_eq!(info.git_branch, "feature");
}

#[test]
fn write_create_emits_all_added_diff() {
    let evs = backfill("write_create.jsonl");
    let edit = evs.iter().find(|e| matches!(e.kind, EventKind::FileEdit { .. })).unwrap();
    let EventKind::FileEdit { created, added, removed, .. } = &edit.kind else {
        unreachable!()
    };
    assert!(*created);
    assert_eq!(*added, 3); // "# Notes", "", "first"
    assert_eq!(*removed, 0);

    let result = evs.iter().find(|e| matches!(e.kind, EventKind::ToolResult { .. })).unwrap();
    let EventKind::ToolResult { name, summary, .. } = &result.kind else {
        unreachable!()
    };
    assert_eq!(name, "Write");
    assert!(summary.contains("created"), "summary was: {summary}");
}

// ---- think filter (mirrors test_models_and_chat.py) ----------------------- //

#[test]
fn strip_think_whole_string() {
    assert_eq!(strip_think("<think>reasoning here</think>The answer."), "The answer.");
    assert_eq!(strip_think("no tags at all"), "no tags at all");
}

#[test]
fn think_filter_streaming_split_across_chunks() {
    let deltas = ["<thi", "nk>secret rea", "soning</thi", "nk>The ", "final answer."];
    let mut f = ThinkFilter::new();
    let mut out = String::new();
    for d in deltas {
        out.push_str(&f.feed(d));
    }
    out.push_str(&f.flush());
    assert_eq!(out, "The final answer.");
}

#[test]
fn think_filter_passes_plain_text() {
    let mut f = ThinkFilter::new();
    let mut out = String::new();
    for d in ["Hello ", "world"] {
        out.push_str(&f.feed(d));
    }
    out.push_str(&f.flush());
    assert_eq!(out, "Hello world");
}

// ---- context + store ------------------------------------------------------ //

#[test]
fn render_activity_includes_markers() {
    let mut store = EventStore::new();
    store.bulk_add(backfill("sample_session.jsonl"));
    let text = render_activity(&store, 160, 10000);
    assert!(text.contains("USER"));
    assert!(text.contains("TOOL→ Bash"));
    assert!(text.contains("TOOL← Bash ok"));
    assert!(text.contains("EDIT config.json +1-1")); // Old -> New: one +, one -
}

// ---- config + provider factory -------------------------------------------- //

#[test]
fn config_round_trip() {
    let tmp = std::env::temp_dir().join(format!("understudy_cfg_{}.json", std::process::id()));
    std::env::set_var("UNDERSTUDY_CONFIG", &tmp);
    let _ = std::fs::remove_file(&tmp);
    assert!(!load_config().configured); // missing -> defaults

    let mut cfg = Config::default();
    cfg.provider = ProviderConfig { kind: "openai".into(), model: "gpt-4o-mini".into(), ..Default::default() };
    cfg.configured = true;
    save_config(&cfg).unwrap();

    let loaded = load_config();
    assert!(loaded.configured);
    assert_eq!(loaded.provider.kind, "openai");
    assert_eq!(loaded.provider.model, "gpt-4o-mini");
    let _ = std::fs::remove_file(&tmp);
    std::env::remove_var("UNDERSTUDY_CONFIG");
}

#[test]
fn build_provider_kinds() {
    assert!(build_provider(&ProviderConfig { kind: "none".into(), ..Default::default() }).is_none());
    let p = build_provider(&ProviderConfig {
        kind: "openai".into(),
        base_url: "https://x/v1".into(),
        model: "m".into(),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(p.kind(), "openai");
    assert_eq!(p.model(), "m");
}
