# Understudy

A read-only side-car for coding agents. It attaches to an agent session, shows what the agent is doing as it happens, and helps you understand the work well enough to answer for it later.

[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](#license)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange.svg)](https://www.rust-lang.org)

Understudy is an open-source tool for fighting **comprehension debt**: the gap between how much code an agent produces and how much of it you actually understand. It tails a coding-agent session, normalizes the activity into a live feed, and gives you a private model to question the work with. It runs as a passive observer and talks only to its own model, so the agent you are watching is never touched or interrupted.

## What is comprehension debt

The term comes from Addy Osmani: the growing distance between how much code exists and how much any human genuinely understands. It builds up quietly because the usual signals stay green. Velocity, test coverage, and PR counts all look healthy while understanding erodes.

The research points to one reliable signal: whether a person can explain *why* a change was made. An Anthropic study of 52 engineers, alongside several 2026 follow-ups, found that developers who delegate to AI ("just make it work") score under 40% on later comprehension quizzes, while developers who use AI for conceptual inquiry score over 65%. Agents also write code roughly five to seven times faster than people read it, so the gap widens by default.

Understudy sits in the inquiry channel. It watches both what the agent produces and what you engage with, then estimates your coverage and gives you ways to close it. Background and sources are in [docs/comprehension-debt-kpi.md](docs/comprehension-debt-kpi.md).

## The cockpit

Launching Understudy opens a session picker. Attaching to a session opens a chat-first cockpit that keeps the live state on one screen:

```
 Understudy   project@branch  ·  ollama/qwen3  ·  142 events  ·  ● live  ·  comp 38% (est.)
┌ Glance ────────────┬ Activity ───────────────────┬ Thinking ──────────┐
│ what & why summary │ ▷ user prompt               │ ✲ thought pattern  │
│ files · +/- · tools│ → Edit  store.rs            │ ...                │
├ Segments ──────────┤ ← ok                        ├ Detail ────────────┤
│ ● Refactor store   │ ± events.rs                 │ diff / tool output │
│ ◐ Fix failing tests│ ...                         │                    │
└────────────────────┴─────────────────────────────┴────────────────────┘
┌ Chat ─────────────────────────────────────────────────────────────────┐
│ you> why did it change the event store?                                │
│ understudy> ...                                                         │
│ › _                                                                    │
└────────────────────────────────────────────────────────────────────────┘
```

- **Glance** shows a deterministic activity line plus a debounced "what and why" summary from your model.
- **Activity** is the normalized event feed: prompts, thinking, tool calls, results, and file edits.
- **Segments** splits the session into coherent blocks of work and marks how much of each you have engaged with.
- **Thinking** lists the agent's reasoning as one-line thought patterns.
- **Detail** renders the selected event: a diff for an edit, or the input and output for a tool call.
- **Chat** is a separate conversation with your own model. Questions carry the live activity as context, and answers stream in.

## Features

- **Multiple agents.** The session picker discovers and labels sessions from each supported tool, so you can attach to any of them from one place.
- **Read-only attachment.** Reads the agent's own transcript (a JSONL log for Claude Code, the SQLite database for OpenCode). The observed agent does not know Understudy exists.
- **Normalized event stream.** One `Event` model covers prompts, thinking, tool calls, tool results, and file edits with diffs.
- **Your own model.** Chat and summaries run on a provider you configure (local Ollama, any OpenAI-compatible endpoint, or GitHub Copilot). Offline with Ollama.
- **Semantic segmentation.** The session is split into named blocks of work. Segmentation is incremental and cached per session, so reopening does not re-run everything.
- **Comprehension Coverage.** A best-effort gauge of how much produced work you have engaged with, colored by the research bands.
- **Explain-back.** Pick a segment, answer "why did the agent do this here," and have your model grade the answer against the actual activity.
- **Debt trend.** Per-session coverage is recorded to a local ledger so you can track it over time.
- **Headless CLI.** Discover sessions, tail events, ask one-off questions, segment, and read the debt trend without the TUI.

## Quickstart

You need a [Rust toolchain](https://www.rust-lang.org/tools/install), at least one Claude Code session on disk, and a model provider (a local [Ollama](https://ollama.com) is the simplest start).

```bash
cargo run             # launch the cockpit: pick a session, then watch and chat
cargo build --release # produce ./target/release/understudy
cargo test            # run the test suite
```

Keys in the cockpit:

- **Picker:** up and down to select, **Enter** to attach, **q** to quit.
- **Chat:** type to ask, **Enter** to send. Up and down walk command history.
- **Panels:** **Tab** and **Shift+Tab** focus a panel. While a panel is focused, up and down select, **PageUp** and **PageDown** scroll.
- **Esc:** unfocus a panel, then unpin the selection, then return to the picker.
- **Ctrl+q:** quit from anywhere.

Slash commands in the chat input:

| Command | Action |
|---|---|
| `/segments [--force]` | Segment the session. `--force` re-segments from the start |
| `/debt` | Print the coverage breakdown for the session |
| `/explain [n]` | Start an explain-back check on a segment |
| `/tagging` | Toggle LLM classification of each question as inquiry or delegation |
| `/follow` | Resume live following and clear the selection |
| `/model [name]` | Show or switch the active model |
| `/session` | Return to the session picker |
| `/clear` | Clear the chat log |
| `/help` | List the commands |

## Configuration

Configuration lives at the platform config directory, overridable with `UNDERSTUDY_CONFIG`:

- macOS: `~/Library/Application Support/understudy/config.json`
- Linux: `~/.config/understudy/config.json`

```json
{
  "provider": {
    "kind": "ollama",
    "base_url": "http://localhost:11434",
    "api_key": "",
    "model": "qwen3:4b",
    "temperature": 0.3
  },
  "configured": true
}
```

`kind` is one of `ollama`, `openai`, `copilot`, or `none`. For `openai`, `api_key` falls back to `$OPENAI_API_KEY`. For `copilot`, an existing Copilot login (VS Code, `gh`, the Copilot CLI, or `$COPILOT_GITHUB_TOKEN`) is used. With `none`, the cockpit runs as a feed without chat or summaries. Provider details are in [docs/providers.md](docs/providers.md).

Segmentation caches and the debt ledger are stored under the platform data directory and can be redirected with `UNDERSTUDY_CACHE_DIR` and `UNDERSTUDY_LEDGER`. All data stays local.

## CLI

Running `understudy` with no subcommand launches the cockpit. The headless subcommands are:

| Command | Description |
|---|---|
| `understudy sessions` | List discovered sessions across agents, newest first |
| `understudy tail [--here] [--session <id\|path>]` | Stream normalized events for a session |
| `understudy ask "<question>" [--here] [--session <id>]` | Get a one-shot comprehension answer |
| `understudy segments [--here] [--session <id>]` | Print the session split into semantic segments |
| `understudy debt` | Show the coverage trend per project from the local ledger |
| `understudy check` | Validate the configured model provider |

`--here` filters to sessions whose working directory matches the current one.

## How it works

Understudy is built around a single normalized `Event` model and a source adapter that produces it. The Claude Code adapter tails the append-only JSONL transcript that Claude Code writes per session. The OpenCode adapter reads OpenCode's SQLite database and polls for new records. Each adapter normalizes its native records into the same `Event` model, so everything downstream stays source-agnostic.

```
Source adapter (tail JSONL / read SQLite) -> Event normalizer -> Event store + rolling window
        |                                                |               |
        |                                      Summaries (deterministic   Comprehension chat
        |                                       + debounced LLM)          (separate model)
        |                                                |
        |                                      Segmentation + Comprehension Coverage
        |
        +------------------ Cockpit TUI: glance, activity, segments, thinking, detail, chat
```

The event store, summaries, segmentation, chat, and TUI are all source-agnostic, so additional adapters slot in behind the same `Event` model. Full integration notes are in [docs/claude-code-integration.md](docs/claude-code-integration.md) and the architecture in [docs/rust-migration.md](docs/rust-migration.md).

## Status

This is early software at version 0.0.1. The ingestion spine, the model-provider layer, the chat-first cockpit, semantic segmentation, and the Comprehension Coverage KPI are implemented and covered by tests over real session fixtures.

Claude Code and OpenCode are supported sources today, both discovered and labelled in the session picker. Copilot CLI, Codex, and Gemini CLI have reserved labels, with discovery and adapters planned. An in-app setup wizard is planned; until it lands, configure the provider by editing the config file described above.

## Documentation

- [docs/comprehension-debt-kpi.md](docs/comprehension-debt-kpi.md): the Comprehension Coverage metric, its research grounding, and the locked design decisions.
- [docs/rust-migration.md](docs/rust-migration.md): the Rust and ratatui architecture, crate choices, and module map.
- [docs/claude-code-integration.md](docs/claude-code-integration.md): the Claude Code adapter, JSONL schema, and tailing.
- [docs/providers.md](docs/providers.md): model providers and configuration.
- [docs/ux-redesign.md](docs/ux-redesign.md): the chat-first cockpit design.
- [docs/architecture.md](docs/architecture.md): the original conceptual system design.
- [docs/roadmap.md](docs/roadmap.md): phased milestones and history.

## Contributing

Issues and pull requests are welcome. The workspace has two crates: `crates/core` holds the library (events, store, sources, models, summaries, segmentation, comprehension), and `crates/understudy` holds the CLI and the ratatui TUI. Run `cargo test` before opening a pull request. The golden tests in `crates/core/tests` run against the real transcripts in `fixtures/`.

## License

Released under the [MIT License](LICENSE).
