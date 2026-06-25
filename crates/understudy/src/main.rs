//! Understudy CLI. Headless subcommands today (`sessions`, `tail`, `ask`, `check`);
//! the ratatui TUI is the default command, landing in the next migration phase.

use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};
use futures::StreamExt;

use understudy_core::chat::system_with_activity;
use understudy_core::config::load_config;
use understudy_core::context::event_line;
use understudy_core::filters::ThinkFilter;
use understudy_core::models::{build_provider, ChatMessage};
use understudy_core::sources::claude_code::{
    discover_sessions, projects_dir, resolve_session, ClaudeCodeSource,
};
use understudy_core::sources::{discover_all, Source};
use understudy_core::store::EventStore;

mod tui;

#[derive(Parser)]
#[command(name = "understudy", about = "Read-only side-car that comprehends a coding-agent session.")]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List discovered Claude Code sessions (newest first).
    Sessions,
    /// Tail a session and print normalized events live.
    Tail {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        here: bool,
    },
    /// Ask the comprehension model about a session.
    Ask {
        question: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        here: bool,
    },
    /// Split a session into model-determined semantic segments.
    Segments {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        here: bool,
    },
    /// Show the comprehension-coverage trend per project from the local ledger.
    Debt,
    /// Check the configured model provider.
    Check,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Some(Cmd::Sessions) => sessions(),
        Some(Cmd::Tail { session, here }) => tail(session, here).await,
        Some(Cmd::Ask { question, session, here }) => ask(question, session, here).await,
        Some(Cmd::Segments { session, here }) => segments(session, here).await,
        Some(Cmd::Debt) => debt(),
        Some(Cmd::Check) => check().await,
        None => tui::run().await,
    }
}

fn pick_session(session: Option<String>, here: bool) -> Result<PathBuf> {
    if let Some(s) = session {
        return resolve_session(&s).ok_or_else(|| anyhow!("no session found for {s:?}"));
    }
    let cwd = std::env::current_dir().ok().and_then(|p| p.to_str().map(|s| s.to_string()));
    let filter = if here { cwd.as_deref() } else { None };
    discover_sessions(filter)
        .into_iter()
        .next()
        .map(|s| s.path)
        .ok_or_else(|| anyhow!("no sessions found under {}", projects_dir().display()))
}

fn sessions() -> Result<()> {
    let list = discover_all(None);
    if list.is_empty() {
        println!("no sessions found (looked under {} and the OpenCode database)", projects_dir().display());
        return Ok(());
    }
    for s in list.iter().take(40) {
        let project = s.cwd.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("?");
        let id = s.session_id.get(..8).unwrap_or(&s.session_id);
        println!("{:<2}  {id}  {project:<24.24}  {:<12.12}  {}", s.agent.tag(), s.git_branch, s.summary);
    }
    Ok(())
}

async fn tail(session: Option<String>, here: bool) -> Result<()> {
    let path = pick_session(session, here)?;
    let mut src = ClaudeCodeSource::new(&path);
    eprintln!("# attached: {}", path.display());
    for ev in src.backfill() {
        println!("{}", event_line(&ev));
    }
    eprintln!("# live — Ctrl+C to stop");
    loop {
        tokio::time::sleep(Duration::from_millis(250)).await;
        for ev in src.read_new() {
            println!("{}", event_line(&ev));
        }
    }
}

async fn ask(question: String, session: Option<String>, here: bool) -> Result<()> {
    let path = pick_session(session, here)?;
    let mut src = ClaudeCodeSource::new(&path);
    let mut store = EventStore::new();
    store.bulk_add(src.backfill());

    let cfg = load_config();
    let provider = build_provider(&cfg.provider)
        .ok_or_else(|| anyhow!("no model configured (provider kind is 'none')"))?;

    let messages = vec![system_with_activity(&store), ChatMessage::user(question)];

    let mut stream = provider.stream_chat(messages);
    let mut filter = ThinkFilter::new();
    let mut stdout = std::io::stdout();
    while let Some(item) = stream.next().await {
        let delta = item.map_err(|e| anyhow!(e.to_string()))?;
        let visible = filter.feed(&delta);
        if !visible.is_empty() {
            print!("{visible}");
            stdout.flush().ok();
        }
    }
    let tail = filter.flush();
    if !tail.is_empty() {
        print!("{tail}");
    }
    println!();
    Ok(())
}

async fn segments(session: Option<String>, here: bool) -> Result<()> {
    let path = pick_session(session, here)?;
    let mut src = ClaudeCodeSource::new(&path);
    let mut store = EventStore::new();
    store.bulk_add(src.backfill());

    let cfg = load_config();
    let provider = build_provider(&cfg.provider)
        .ok_or_else(|| anyhow!("no model configured (provider kind is 'none')"))?;

    let segs = understudy_core::segments::segment_session(&provider, &store)
        .await
        .map_err(|e| anyhow!(e.to_string()))?;
    if segs.is_empty() {
        println!("no activity to segment");
        return Ok(());
    }
    for (i, s) in segs.iter().enumerate() {
        let span = match (s.first_ts, s.last_ts) {
            (Some(a), Some(b)) => format!("{}–{}", a.format("%H:%M"), b.format("%H:%M")),
            _ => String::new(),
        };
        let errors = if s.errors > 0 { format!(" · {} error(s)", s.errors) } else { String::new() };
        println!("{}. {}  [{span}]", i + 1, s.title);
        println!(
            "   events {}..{} · {} file(s) · +{} -{}{errors}",
            s.start_idx, s.end_idx, s.files.len(), s.lines_added, s.lines_removed
        );
        if !s.tool_counts.is_empty() {
            let tools = s.tool_counts.iter().map(|(n, c)| format!("{n}×{c}")).collect::<Vec<_>>().join(" ");
            println!("   tools: {tools}");
        }
    }
    Ok(())
}

fn debt() -> Result<()> {
    let trends = understudy_core::comprehension::ledger::trends();
    if trends.is_empty() {
        println!("no comprehension records yet — they're written when you leave a session in the cockpit");
        return Ok(());
    }
    println!("{:<26} {:>8} {:>8} {:>8}", "PROJECT", "SESSIONS", "LATEST", "AVG");
    for t in trends {
        let pct = |c: f32| (c * 100.0).round() as u8;
        println!("{:<26.26} {:>8} {:>7}% {:>7}%", t.project, t.sessions, pct(t.latest_coverage), pct(t.avg_coverage));
    }
    Ok(())
}

async fn check() -> Result<()> {
    let cfg = load_config();
    match build_provider(&cfg.provider) {
        None => println!("provider: none (feed-only) — configure a model to enable chat/summaries."),
        Some(p) => {
            println!("provider: {} / {}", p.kind(), p.model());
            match p.check().await {
                Ok(s) => println!("✓ {s}"),
                Err(e) => println!("✗ {e}"),
            }
        }
    }
    Ok(())
}
