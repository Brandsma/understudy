# Understudy

A read-only side-car TUI that attaches to a coding-agent session and maintains a
live, queryable understanding of what the agent is doing — surfacing thinking
tokens, tool calls, file edits, and intermediate steps as they happen.

It addresses **comprehension debt**: the tendency to only understand what an agent
did at the very end, via a model-generated summary. Instead you can watch and
*interrogate* the process as it unfolds — without ever sending a message to, or
interrupting, the main agent loop.

> Understudy is a passive observer. It reads the agent's activity stream and talks
> to its *own* model. The main session never knows it exists.

**Stack:** Rust + [ratatui](https://ratatui.rs) (single static binary). Async via
tokio; providers over `reqwest`. *(Migrated from the original Python/Textual
prototype — see [docs/rust-migration.md](docs/rust-migration.md).)*

## Status

The **core is complete and verified** — Claude Code session discovery + JSONL
tailing + normalization, the model-provider layer (Ollama / OpenAI-compatible /
GitHub Copilot), `<think>` filtering, and the deterministic + LLM summaries — with
golden tests over real fixtures. The **TUI** has a session picker, a live
color-coded activity feed with the Tier-1 summary, and a **streaming comprehension
chat**. Remaining UX (thinking viewer, settings wizard, the full cockpit) is in
progress; see [docs/rust-migration.md](docs/rust-migration.md).

## Quickstart

```bash
cargo run                          # launch the TUI: pick a session → live feed + chat
cargo build --release              # produce ./target/release/understudy

# headless subcommands
understudy sessions                # list discovered Claude Code sessions
understudy tail   [--here] [--session <uuid|path>]   # stream normalized events
understudy ask "what is it doing?" [--here]          # one-shot comprehension answer
understudy check                   # validate the configured model provider

cargo test                         # run the suite (core parity + TUI render tests)
```

- **Picker:** ↑/↓ select · **Enter** attach · **^q** quit.
- **Feed:** ↑/↓ · PgUp/PgDn scroll · **g/G** top/bottom · **c** (or `/`) chat ·
  **Esc** back to picker · **^q** quit.
- **Chat:** type to ask · **Enter** send · **Esc** close · **^q** quit. Questions
  carry the live activity stream + conversation history as context; answers stream in.

It is read-only — it talks only to *its own* model and never touches the agent.

## Configuring the model

Config lives at the platform config dir (`~/Library/Application Support/understudy/config.json`
on macOS, `~/.config/understudy/config.json` on Linux), overridable with
`UNDERSTUDY_CONFIG`. Shape:

```json
{ "provider": { "kind": "ollama", "base_url": "http://localhost:11434",
                "api_key": "", "model": "qwen3:4b", "temperature": 0.3 },
  "configured": true }
```

`kind` is `ollama` | `openai` | `copilot` | `none`. For `openai`, `api_key` falls back
to `$OPENAI_API_KEY`; for `copilot`, an existing Copilot login (VS Code / `gh` / Copilot
CLI, or `$COPILOT_GITHUB_TOKEN`) is used. Details: [docs/providers.md](docs/providers.md).
*(The in-TUI setup wizard is part of the in-progress UX; until then, edit this file.)*

## Why this is feasible (verified against real data)

The hard question was *"how do you tap the activity stream without interrupting the
agent?"* Every priority target is passively observable, and the integration collapses
to **one small adapter each**:

| Target | Mechanism | Source of truth |
|---|---|---|
| **Claude Code** (priority) | Tail append-only JSONL | `~/.claude/projects/<encoded-cwd>/<session>.jsonl` |
| **Copilot CLI** | Tail append-only JSONL | `~/.copilot/session-state/<session>/events.jsonl` |
| **OpenCode** | Subscribe to SSE | server `/event` stream |

From real Claude Code transcripts: **diffs are free** (every Edit/Write ships a
`structuredPatch`), **thinking text is usually present** (best-effort — empty in some
sessions), and structure is recoverable via `parentUuid` + `isSidechain`. Full detail in
[docs/claude-code-integration.md](docs/claude-code-integration.md).

## The core architectural bet

Two of three targets are *tailable event logs* and the third is a push stream, so the
whole app is built around **one normalized `Event` model and a source adapter**. Claude
Code is the first adapter; the event store, summaries, chat, and TUI are all
source-agnostic.

```
Source adapter (tail JSONL / SSE) → Event normalizer → Event store + rolling window
         │                                                  │              │
         │                                        Live-summary engine   Comprehension chat
         │                                        (instant deterministic   (separate model
         │                                         + debounced LLM)          conversation)
         └──────────────────── TUI: [feed] [summary] [diff] [thinking] [chat] ───────────┘
```

## Repo layout

```
crates/
  core/          # lib: events, store, context, filters, config, chat,
                 #      sources/claude_code, models/{ollama,openai,copilot}, summary
  understudy/    # bin: CLI (sessions/tail/ask/check) + the ratatui TUI
fixtures/        # real JSONL transcripts used by the golden tests
docs/            # design docs + the migration/UX plans
```

## Documentation

- [docs/rust-migration.md](docs/rust-migration.md) — the Rust/ratatui architecture,
  crate choices, module map, phased status, and testing.
- [docs/claude-code-integration.md](docs/claude-code-integration.md) — the verified
  Claude Code adapter spec: storage, JSONL schema, `structuredPatch`, tailing.
- [docs/providers.md](docs/providers.md) — model providers and config.
- [docs/ux-redesign.md](docs/ux-redesign.md) — the chat-first cockpit UX being built.
- [docs/comprehension-debt-kpi.md](docs/comprehension-debt-kpi.md) — the Comprehension Debt
  KPI design: what it measures, the research grounding, and the locked decisions.
- [docs/architecture.md](docs/architecture.md) — original system design (conceptual).
- [docs/roadmap.md](docs/roadmap.md) — phased milestones and history.
