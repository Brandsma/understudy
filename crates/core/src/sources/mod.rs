//! Source adapters and the shared session vocabulary. Each coding agent stores its
//! transcript differently (Claude Code tails JSONL, OpenCode reads a SQLite DB), but every
//! adapter discovers [`SessionInfo`]s and produces normalized [`Event`]s through [`Source`].

use std::path::PathBuf;
use std::time::SystemTime;

use crate::events::Event;

pub mod antigravity;
pub mod claude_code;
pub mod copilot;
pub mod opencode;

/// Which coding agent a session belongs to. Drives the picker label and which adapter
/// opens it. Variants without an adapter yet are still labelled when discovered elsewhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    ClaudeCode,
    OpenCode,
    Copilot,
    Codex,
    GeminiCli,
    Antigravity,
    Unknown,
}

impl Agent {
    /// Human-readable name for the picker.
    pub fn name(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "Claude Code",
            Agent::OpenCode => "OpenCode",
            Agent::Copilot => "Copilot",
            Agent::Codex => "Codex",
            Agent::GeminiCli => "Gemini CLI",
            Agent::Antigravity => "Antigravity",
            Agent::Unknown => "Unknown",
        }
    }

    /// Short two-letter tag for the no-font fallback label.
    pub fn tag(self) -> &'static str {
        match self {
            Agent::ClaudeCode => "CC",
            Agent::OpenCode => "OC",
            Agent::Copilot => "CP",
            Agent::Codex => "CX",
            Agent::GeminiCli => "GM",
            Agent::Antigravity => "AG",
            Agent::Unknown => "??",
        }
    }
}

/// A discoverable agent session. `path` is the adapter's locator: the JSONL file for Claude
/// Code, the SQLite DB for OpenCode (paired with `session_id`).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub agent: Agent,
    pub path: PathBuf,
    pub session_id: String,
    pub cwd: String,
    pub git_branch: String,
    pub modified: SystemTime,
    pub size: u64,
    pub summary: String,
}

/// A normalized event stream for one session: a one-shot history plus incremental tailing.
pub trait Source: Send {
    /// Read the whole transcript once, returning the most recent events.
    fn backfill(&mut self) -> Vec<Event>;
    /// Read events that have appeared since the last call.
    fn read_new(&mut self) -> Vec<Event>;
}

/// Every discoverable session across all supported agents, newest first. Optionally filter
/// to a working directory.
pub fn discover_all(cwd_filter: Option<&str>) -> Vec<SessionInfo> {
    let mut out = claude_code::discover_sessions(cwd_filter);
    out.extend(opencode::discover_sessions(cwd_filter));
    out.extend(copilot::discover_sessions(cwd_filter));
    out.extend(antigravity::discover_sessions(cwd_filter));
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Open the right adapter for a discovered session.
pub fn open_source(info: &SessionInfo) -> Box<dyn Source + Send> {
    match info.agent {
        Agent::OpenCode => Box::new(opencode::OpenCodeSource::new(&info.path, &info.session_id)),
        Agent::Copilot => Box::new(copilot::CopilotSource::new(&info.path, &info.session_id)),
        Agent::Antigravity => {
            Box::new(antigravity::AntigravitySource::new(&info.path, &info.session_id))
        }
        _ => Box::new(claude_code::ClaudeCodeSource::new(&info.path)),
    }
}
