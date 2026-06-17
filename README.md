# Understudy

A read-only side-car TUI that attaches to a coding-agent session and maintains a
live, queryable understanding of what the agent is doing — surfacing thinking
tokens, tool calls, file edits, and intermediate steps as they happen.

It addresses **comprehension debt**: the tendency to only understand what an agent
did at the very end, via a model-generated summary. Instead you can watch and
*interrogate* the process as it unfolds — without ever sending a message to, or
interrupting, the main agent loop.

> Understudy is a passive observer. It reads the agent's activity
> stream and talks to its *own* model. The main session never knows it exists.

## Status

**Phase 1 built.** Claude Code session auto-discovery → a selectable picker → a live,
read-only feed with a structured diff viewer, **plus** a comprehension chat and a
debounced "what & why" summary backed by a pluggable model provider (Ollama,
OpenAI-compatible, or GitHub Copilot). A first-run wizard and an F2 settings screen
configure the model. Verified end-to-end against real sessions and a live local model.

## Quickstart

```bash
uv sync                 # install deps (textual, httpx, platformdirs)
uv run understudy       # first run → model setup → pick a session → watch it live
# options:
uv run understudy --here              # only sessions for the current directory
uv run understudy --session <uuid>    # skip the picker, attach directly
uv run --extra dev pytest -q          # run the test suite
```

- **First run** opens a setup wizard: choose **Ollama** (local/private), an
  **OpenAI-compatible API**, or **GitHub Copilot** (experimental, reuses your existing
  Copilot login). *Detect models* / *Test* validate it; *Skip* runs feed-only.
- **Picker:** ↑/↓ move, **Enter** attach, **r** refresh, **q** quit.
- **Feed:** ↑/↓ inspect an event (drives the diff/detail pane), **c** toggle the
  comprehension chat, **p** toggle follow-tail, **F2** settings, **Esc** back.

It is read-only — it talks only to *its own* model and never touches the agent.

## Decisions locked in

| Decision | Choice | Why |
|---|---|---|
| Language / TUI | **Python + [Textual](https://textual.textualize.io/)** | Fastest path to a rich multi-pane TUI; async-native; `httpx` for provider streaming |
| Comprehension model | **Pluggable, cloud + local from day one** | A `ModelProvider` seam over **Ollama**, **OpenAI-compatible** APIs, and **GitHub Copilot**; chosen in setup / settings. Local keeps activity on-device |
| First integration | **Claude Code** | Verified feasible & zero-config (see below) |
| Integration posture | **Read-only, zero-config** | Tail the on-disk activity log; never touch the agent |

## Why this is feasible (verified against real data)

The hard question was *"how do you tap the activity stream without interrupting the
agent?"* It turns out every priority target is passively observable, and the
integration collapses to **one small adapter each**:

| Target | Mechanism | Source of truth |
|---|---|---|
| **Claude Code** (priority) | Tail append-only JSONL | `~/.claude/projects/<encoded-cwd>/<session>.jsonl` |
| **Copilot CLI** | Tail append-only JSONL | `~/.copilot/session-state/<session>/events.jsonl` |
| **OpenCode** | Subscribe to SSE | server `/event` stream |

Inspecting real Claude Code transcripts confirmed the data we need is already there:

- **Diffs are free** — every Edit/Write result ships a `structuredPatch` (ready-made
  unified-diff hunks) plus the original file and old/new strings. The diff viewer
  computes nothing.
- **Thinking text is present** in most sessions (one session had 331 readable
  thinking blocks). ⚠️ *Best-effort:* it was empty in some sessions, so availability
  varies by model/version/config and must be validated against the live setup.
- **Structure is recoverable** — entries form a `parentUuid` linked list with
  `isSidechain` flags, so turn boundaries and subagent sidechains can be
  reconstructed.

Full detail in [docs/claude-code-integration.md](docs/claude-code-integration.md).

## The core architectural bet

Two of three targets are *tailable event logs* and the third is a push stream.
So the whole app is built around **one normalized `Event` model and a `Source`
adapter interface**. Claude Code is just the first adapter; the event store,
summary engine, comprehension chat, and TUI are all source-agnostic. This makes
the multi-tool roadmap nearly free.

```
Source adapter (tail JSONL / SSE) → Event normalizer → Event store + rolling window
         │                                                  │              │
         │                                        Live-summary engine   Comprehension chat
         │                                        (instant deterministic   (separate model
         │                                         + debounced LLM)          conversation)
         └──────────────────── TUI: [feed] [summary] [diff] [thinking] [chat] ───────────┘
```

## Documentation

- [docs/architecture.md](docs/architecture.md) — system design: source-adapter
  pattern, normalized event model, pipeline, components, model layer, TUI, repo layout.
- [docs/claude-code-integration.md](docs/claude-code-integration.md) — the verified
  Claude Code adapter spec: storage, JSONL schema, `structuredPatch`, session
  discovery, tailing strategy, optional hooks.
- [docs/providers.md](docs/providers.md) — model providers: Ollama, OpenAI-compatible,
  and GitHub Copilot; config file location, env vars, and the Copilot caveats.
- [docs/roadmap.md](docs/roadmap.md) — phased milestones, the concrete first slice,
  risks, and open questions.

## Roadmap at a glance

- **Phase 0 — MVP:** ✅ Claude Code tail adapter + event store + live feed pane + diff
  viewer. No LLM. Read-only stream proven end-to-end (incl. session picker).
- **Phase 1:** ✅ Comprehension chat + debounced LLM summary over a pluggable provider
  (Ollama / OpenAI-compatible / Copilot), first-run wizard + settings.
- **Phase 2:** Collapsible thinking-token viewer with summarized "thought patterns."
- **Phase 3:** Second adapter (OpenCode SSE — easiest) to prove the abstraction; then
  Copilot CLI.
- **Phase 4:** Polish — sidechains, session switching, packaging, config.
