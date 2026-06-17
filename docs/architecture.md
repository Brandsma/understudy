# Architecture

## Design principles

1. **Read-only observer.** The side-car never writes to the agent's session, never
   sends it messages, never holds a lock it could block on. Worst case if the
   side-car crashes: it stops updating. The agent is unaffected.
2. **Source-agnostic core.** Everything above the adapter layer speaks one
   normalized `Event` vocabulary. Adding a coding agent = writing one adapter.
3. **Instant first, smart second.** The UI is never blocked on a model call. A
   deterministic summary updates on every event; an LLM summary refines it on a
   debounce. The LLM determines whether or not the event update is relevant to show. The feed and diff viewer work with zero API calls.
4. **Cost- and privacy-aware by construction.** Activity can be large and sensitive.
   Prompt caching, debouncing, redaction, and a local-model option are first-class,
   not afterthoughts.

## Layered view

```
┌─────────────────────────────────────────────────────────────────────┐
│ TUI (Textual)   [feed] [live summary] [diff] [thinking] [chat input]  │
├─────────────────────────────────────────────────────────────────────┤
│ App services                                                          │
│   • Live-summary engine (deterministic + debounced LLM)               │
│   • Comprehension chat session (own conversation, prompt-cached)      │
│   • Model provider layer  ── Ollama | OpenAI-compatible | Copilot      │
├─────────────────────────────────────────────────────────────────────┤
│ Event store   rolling window · turn grouping (parentUuid) · sidechains │
├─────────────────────────────────────────────────────────────────────┤
│ Event normalizer   raw source record → normalized Event               │
├─────────────────────────────────────────────────────────────────────┤
│ Source adapters   ClaudeCodeSource(tail) · OpenCodeSource(SSE) · …     │
└─────────────────────────────────────────────────────────────────────┘
```

## The `Source` adapter pattern

A source is anything that can produce a backfill of recent history and then an async
stream of new events. That is the *only* contract the core depends on.

```python
# sources/base.py
from typing import AsyncIterator, Protocol
from understudy.events import Event, SourceInfo

class Source(Protocol):
    def describe(self) -> SourceInfo:
        """Human-readable identity: tool name, session id, cwd, model if known."""

    async def backfill(self) -> list[Event]:
        """Recent history so the panel isn't empty on attach (bounded)."""

    def stream(self) -> AsyncIterator[Event]:
        """Yield normalized events as they arrive, until the session ends."""
```

- `ClaudeCodeSource` — tails `~/.claude/projects/<encoded-cwd>/<session>.jsonl`,
  parses each appended line, emits normalized events. (See
  [claude-code-integration.md](claude-code-integration.md).)
- `OpenCodeSource` — opens an SSE connection to the OpenCode server `/event` stream
  and maps `message.updated` / `part.updated` events.
- `CopilotSource` — tails `~/.copilot/session-state/<session>/events.jsonl`.

Each adapter owns *all* tool-specific knowledge. The normalizer below is the only
place that maps vendor shapes to our vocabulary, so the rest of the app stays clean.

## Normalized event model

```python
# events.py  (illustrative — dataclasses or pydantic)
class Kind(StrEnum):
    SESSION_START = "session_start"
    USER_PROMPT   = "user_prompt"
    ASSISTANT_TEXT= "assistant_text"
    THINKING      = "thinking"
    TOOL_CALL     = "tool_call"
    TOOL_RESULT   = "tool_result"
    FILE_EDIT     = "file_edit"     # derived from Edit/Write tool results
    TURN_END      = "turn_end"
    NOTIFICATION  = "notification"

@dataclass
class Event:
    kind: Kind
    ts: datetime
    source: str                 # "claude-code" | "opencode" | "copilot"
    turn_id: str | None         # groups events within one agent response
    is_sidechain: bool = False  # subagent activity
    payload: dict               # kind-specific (see below)
    raw_ref: str | None = None  # uuid/offset back into the source for drill-down
```

Per-kind payloads (the fields the UI and summarizer actually use):

| Kind             | Payload                                                               |
| ---------------- | --------------------------------------------------------------------- |
| `user_prompt`    | `text`                                                                |
| `assistant_text` | `text`                                                                |
| `thinking`       | `text`, optional `summary` (filled lazily by the thinking viewer)     |
| `tool_call`      | `id`, `name`, `input` (dict)                                          |
| `tool_result`    | `id`, `name`, `ok: bool`, `summary` (one-liner), `detail` (truncated) |
| `file_edit`      | `path`, `hunks: [Hunk]`, `added: int`, `removed: int`, `original?`    |
| `turn_end`       | `reason` ("stop" \| "interrupt")                                      |

`Hunk` mirrors Claude Code's `structuredPatch`: `old_start, old_lines, new_start,
new_lines, lines: list[str]` where each line is prefixed ` `/`+`/`-`. This lets the
diff viewer render directly, no diffing.

## Event store

- **Append-only log** of all events for the session, plus a **rolling window** (last
  N events / last M turns) that is the working context for summaries and chat.
- **Turn grouping:** events are bucketed into turns using `parentUuid` linkage (CC)
  or message ids (OpenCode). A "turn" = one user prompt → the agent's full response
  including its thinking, tool calls, and edits.
- **Sidechain handling:** `is_sidechain` events (subagents) are stored but rendered
  in a collapsed/indented track so the main thread stays readable.
- **Derived indices** maintained incrementally for the deterministic summary: files
  touched (+counts), tool-call histogram, current/most-recent tool, error count.

## Live-summary engine (two tiers)

**Tier 1 — deterministic, instant, free.** Recomputed on every event from the
derived indices. Always shown. Example:

```
▶ Editing  src/store.py  (edit 3/3)   ·   7 files touched   ·   last: Bash `pytest -q` ✗
   tools this turn: Read ×4 · Edit ×3 · Bash ×2
```

**Tier 2 — LLM, debounced, smart.** On a turn boundary (or a quiet-period debounce,
e.g. 1.5 s after the last event), send the rolling window to the comprehension model
for a 1–2 sentence "what & why": *"Refactoring the event store to group by turn; just
ran the tests, which failed on an import error it's now tracing."* Cached; only re-run
when the window has meaningfully changed (cheap hash gate). The Tier-1 line is shown
immediately; Tier-2 replaces a subtitle line when it returns.

This keeps the panel responsive and keeps spend proportional to *activity*, not wall
clock.

## Comprehension chat

A **separate conversation** owned entirely by the side-car. It is never mixed with
the agent's session.

- **System prompt:** "You are observing another coding agent. Below is its live
  activity stream (thinking, tool calls, file edits). Answer the user's questions
  about what it is doing and why. You cannot act — you only explain."
- **Context = the activity stream**, compacted: render the rolling window as a
  compact transcript, summarize/evict older turns to a running digest as it grows.
- **Prompt caching:** the activity-stream prefix is marked cacheable so repeated
  questions over a stable stream are cheap; only the growing tail and the user's
  question are uncached.
- **Decoupling guarantee:** the chat has no tool that can reach the agent. It only
  reads the store. Asking "why did it delete that?" pulls the relevant `file_edit` /
  `tool_call` events into context and explains them.

## Model provider layer (cloud + local, day one)

Per the locked decision, a thin protocol with two implementations, selected by
config or `--model`:

```python
# models/base.py
class ModelProvider(Protocol):
    async def complete(self, messages, *, system, cacheable_prefix=None) -> str: ...
    async def stream(self, messages, *, system) -> AsyncIterator[str]: ...
    @property
    def supports_prompt_cache(self) -> bool: ...
```

- `AnthropicProvider` — default `claude-haiku-4-5-20251001`; uses streaming and
  `cache_control` for the activity prefix.
- `OllamaProvider` — talks to a local Ollama endpoint (OpenAI-compatible) for a
  fully-private/offline run; no prompt-cache, larger context budget management.

The provider is injected into *both* the Tier-2 summarizer and the chat session, so
"local mode" instantly makes the whole side-car offline. (A shortcut worth noting:
`litellm` could back both providers behind one call; the protocol above keeps us
free to drop it in or hand-roll.)

## TUI (Textual)

Proposed default layout (resizable; panes toggle with keys):

```
┌───────────────────────────────┬───────────────────────────────┐
│ ACTIVITY FEED                 │ LIVE SUMMARY                  │
│ chronological normalized       │ Tier-1 deterministic line     │
│ events; tool calls, edits,     │ + Tier-2 LLM "what & why"     │
│ thinking markers; sidechains   ├───────────────────────────────┤
│ indented/collapsed             │ DIFF / DETAIL                 │
│                                │ structuredPatch for the        │
│                                │ selected file_edit; tool I/O   │
│                                │ for the selected tool_call     │
├───────────────────────────────┴───────────────────────────────┤
│ THINKING  (collapsible)  ▸ "considered moving X→Y, chose rename"│
├───────────────────────────────────────────────────────────────┤
│ COMPREHENSION CHAT                                             │
│ > why did it edit store.py instead of events.py?              │
└───────────────────────────────────────────────────────────────┘
```

- **Feed** — a Textual `RichLog`/`ListView`; each event is a compact row, selectable.
  Selecting a `file_edit`/`tool_call` drives the Detail pane.
- **Diff/Detail** — renders `Hunk`s with `+`/`-` coloring (Rich); for tool calls,
  shows input and truncated output.
- **Thinking** — collapsible widget; raw text collapsed by default, with an optional
  one-line LLM-summarized "thought pattern" header (Phase 2).
- **Chat** — `Input` + streaming `Markdown` transcript, backed by the
  comprehension session. Posts to the model, never to the agent.
- **Key bindings (proposed):** `Tab` cycle panes · `t` toggle thinking · `/` focus
  chat · `f` filter feed by kind · `g`/`G` top/bottom · `p` pause autoscroll.

## Concurrency model (asyncio)

Textual is asyncio-native, so everything is one event loop with cooperating tasks:

- **Tail/stream task** — the active `Source.stream()`; pushes events onto an
  `asyncio.Queue`.
- **Ingest task** — drains the queue, normalizes, updates the store + derived
  indices, posts UI messages (Textual `post_message`) to refresh the feed/summary.
- **Summary task** — watches a debounce/turn-boundary signal, calls the provider.
- **Chat task** — spawned per user question; streams tokens into the chat pane.

Back-pressure: if events outpace the UI (huge tool output), batch UI refreshes per
frame; the store always ingests fully so summaries/chat stay correct.

## Proposed repo layout

```
side-car-comprehension/
  pyproject.toml                # uv-managed; Python 3.12+
  README.md
  docs/
    architecture.md
    claude-code-integration.md
    roadmap.md
  src/understudy/
    cli.py                      # `understudy` entry: --cwd/--session/--model/--local
    app.py                      # Textual App, wiring, key bindings
    config.py
    events.py                   # Event, Kind, Hunk, SourceInfo
    store.py                    # EventStore, rolling window, turn grouping
    sources/
      base.py                   # Source protocol + normalizer helpers
      claude_code.py            # JSONL tail adapter  (Phase 0)
      opencode.py               # SSE adapter         (Phase 3)
      copilot.py                # events.jsonl adapter (Phase 3)
    summary/
      deterministic.py          # Tier-1
      llm.py                    # Tier-2 (debounced)
    chat/session.py             # comprehension conversation
    models/
      base.py                   # ModelProvider protocol
      anthropic.py              # cloud (Haiku default)
      ollama.py                 # local
    tui/
      feed.py  detail.py  diff.py  thinking.py  chat.py  summary.py
  tests/
    fixtures/                   # captured JSONL lines (Bash, Edit, thinking, …)
    test_claude_normalizer.py   # golden-file: raw line → Event
    test_store.py               # turn grouping, rolling window, indices
```

## Key dependencies

| Need              | Library                                      |
| ----------------- | -------------------------------------------- |
| TUI               | `textual` (+ `rich`)                         |
| Fast file tailing | `watchfiles` (Rust-backed)                   |
| Cloud model       | `anthropic` (async, streaming, prompt cache) |
| Local model       | `ollama` or `httpx` to the local endpoint    |
| Config/validation | `pydantic` + `pydantic-settings`             |
| Packaging         | `uv` / `hatchling`                           |
