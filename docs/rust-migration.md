# Rust + ratatui migration plan

A staged rewrite of Understudy from Python/Textual to **Rust + ratatui**, targeting the
new UX in [ux-redesign.md](ux-redesign.md). The strategy: port the *domain core* first
(it's the hard-won, well-specified value and it ports cleanly), then build the chat-first
cockpit on top — never rebuilding today's Textual UI in Rust.

## Why Rust — and an honest framing of the win

The stated goal is performance. Be precise about where the gains actually are: today's
Python/Textual app is **not** CPU- or render-bound — the LLM call dominates every
latency that matters, and a single tailed JSONL file is trivial I/O. So raw speed is a
*secondary* benefit. The real wins of Rust/ratatui for an always-on side-car:

- **Single static binary.** A drop-in `understudy` with zero runtime deps — ideal for a
  tool you attach to any project on any machine. *(The strongest reason.)*
- **Tiny, steady footprint.** A background observer should sip RAM/CPU; immediate-mode
  rendering + no GC keeps it lean over long sessions.
- **Type-safe event modeling.** The many CC record shapes map naturally to `enum`s +
  `serde`; the verified gotchas become exhaustive matches the compiler enforces.
- **Fearless concurrency** for the tail + stream + render loop, and **longevity**.

**Costs (stated plainly):** slower iteration than Python; the UI is a from-scratch
rebuild (ratatui is immediate-mode — no retained widget tree); and you maintain two
codebases during the transition. The phasing below keeps each step shippable to contain
that cost.

## Target architecture

The canonical async-ratatui pattern: **tokio** runtime, an **`Action`/`Message` enum**, a
**`tokio::sync::mpsc`** channel, and a main loop that `tokio::select!`s over input,
internal messages, and a render tick. Background tasks push `Action`s; the loop
`update`s state and `draw`s a frame.

```
            ┌──────────────── tokio runtime ─────────────────┐
 crossterm  │  ┌─ tailer task (notify + offset reader) ─┐     │
 EventStream│  │   → Action::Event(Event)               │     │   ┌──────────────┐
   (input) ─┼─▶│                                        ├──▶  │   │  App (Model) │
            │  ├─ provider stream task (reqwest SSE) ───┤ mpsc│──▶│  update(act) │
 render     │  │   → Action::ChatDelta / SummaryReady   │ rx  │   │  draw(frame) │
  tick  ────┼─▶├─ summary debounce task (timer) ────────┤     │   └──────┬───────┘
            │  └────────────────────────────────────────┘     │          │ ratatui
            └──────────────── tokio::select! ─────────────────┘     terminal frame
```

- **App** holds all state (the Model): the event store + indices, panel/focus state,
  chat transcript, provider handle, config. `update(&mut self, Action)` is the only
  mutator; it may emit follow-up actions (e.g. a chat submit spawns a stream task).
- **Components** (Glance, Activity, Thinking, Detail, Chat, StatusBar, Picker,
  CommandPalette, Settings): each is `draw(&self, frame, area, &App)` plus
  `handle_key(&mut App, key) -> Option<Action>` when active. Keep it a lightweight
  trait, not a deep hierarchy.
- **Cancellation:** each chat/summary request carries an id; stale deltas (superseded by
  a newer request or a session switch) are dropped. Streams run on abortable tasks.

## Crate selection

| Need | Crate | Notes |
|---|---|---|
| TUI | `ratatui` | immediate-mode core |
| Terminal / async input | `crossterm` | `EventStream` for non-blocking key/mouse |
| Async runtime | `tokio` | `select!`, `mpsc`, `spawn`, timers |
| HTTP + streaming | `reqwest` (rustls-tls) | `bytes_stream()` + shared SSE parser |
| SSE parsing (optional) | `eventsource-stream` | or hand-roll `data:` lines |
| JSON | `serde`, `serde_json` | derive structs/enums for CC records |
| File watching | `notify` (+ debounce) | replaces the Python poll loop |
| Config dir | `directories` | per-OS config path; honor `UNDERSTUDY_CONFIG` |
| Config format | `serde` + `toml` | (or keep JSON for parity) |
| Chat input | `tui-textarea` | multiline editor, crossterm key mapping |
| Scroll lists | built-in `List`/`ListState` | `tui-scrollview` if a panel needs it |
| Markdown (chat) | `tui-markdown` | pulldown-cmark → ratatui `Text` |
| Spinner | `throbber-widgets-tui` | streaming indicator |
| Errors | `anyhow` / `thiserror` | app vs library |
| CLI | `clap` | `--here`, `--session`, subcommands |
| Diff/code color (later) | `syntect` / `two-face` | optional; we already have `structuredPatch` |

**LLM access: hand-roll on `reqwest`, don't adopt an OpenAI SDK crate.** We need three
shapes — Ollama native `/api/chat` (NDJSON), OpenAI-compatible `/v1/chat/completions`
(SSE), and Copilot (token exchange + OpenAI-compatible with custom headers). A thin
shared SSE/NDJSON parser serves all three, exactly mirroring today's `models/_openai.py`
+ per-provider files. An SDK like `async-openai` covers only one shape and adds coupling.

## Workspace layout & Python→Rust mapping

A cargo **workspace** splitting the pure domain (`core`, a lib) from the UI (`tui`), so
the domain is unit-testable headlessly and reusable (e.g. a `--json` mode).

```
understudy/                         today (Python)                → Rust
├─ Cargo.toml (workspace)
├─ crates/
│  ├─ core/        (lib, no TUI)
│  │   ├─ events.rs        events.py            → enum Event, Hunk, SourceInfo
│  │   ├─ store.rs         store.py             → EventStore + derived indices
│  │   ├─ context.rs       context.py           → activity → prompt string
│  │   ├─ config.rs        config.py            → serde Config + load/save
│  │   ├─ sources/
│  │   │   ├─ mod.rs       sources/base.py      → trait Source
│  │   │   └─ claude_code  sources/claude_code  → discovery + tail + normalize
│  │   ├─ models/
│  │   │   ├─ mod.rs       models/__init__,base → trait Provider, factory
│  │   │   ├─ sse.rs       models/_openai.py    → shared SSE/NDJSON stream
│  │   │   ├─ ollama.rs    models/ollama.py
│  │   │   ├─ openai.rs    models/openai_compat
│  │   │   ├─ copilot.rs   models/copilot.py    → token exchange + headers
│  │   │   └─ filters.rs   models/_filters.py   → <think> stripping
│  │   └─ summary/         summary/*.py         → deterministic, llm, thinking
│  └─ tui/         (bin, ratatui)               ← all of tui/*.py is REBUILT
│      ├─ app.rs           App/Model + update()
│      ├─ event.rs         Action enum + loop (select!)
│      ├─ components/      glance,activity,thinking,detail,chat,statusbar,
│      │                   picker,palette,settings
│      ├─ layout.rs        responsive breakpoints
│      └─ main.rs          clap CLI → run
└─ tests/fixtures/         tests/fixtures/*.jsonl  ← REUSED verbatim as golden inputs
```

### What ports vs what's rebuilt

- **Ports (≈70% of the value — logic, well-specified, fixture-backed):** event
  normalization including the verified gotchas (queue-operation-first metadata,
  `Write`→`create` with empty `structuredPatch` + `content`, `redacted_thinking`,
  sidechains), `structuredPatch`→`Hunk`, robust tailing (offset + partial-line buffer +
  rotation, now `notify`-driven), provider streaming + Copilot token exchange +
  `<think>` filter, store/indices, the three summaries, config.
- **Rebuilt (the UI):** everything under `tui/`. Immediate-mode is a different model and
  it's where the new cockpit UX lives — so this is intentional, not loss.

## Phased migration (incremental / strangler)

Each phase is independently shippable and testable. Python stays usable through R0–R4.

> **Status (big-bang in progress):** R0 + R1 **done and verified**; R2 foundation +
> **streaming chat (R3 core)** working. The Rust workspace lives in `crates/core` (lib) +
> `crates/understudy` (bin); `cargo test` is green (13 core parity tests over the *same*
> fixtures + 4 ratatui/chat render tests). Verified live: `understudy sessions`/`check`/
> `ask` against real transcripts with Ollama, and the Copilot token exchange. The TUI has
> a session picker, a live activity feed, and a streaming comprehension chat panel.

- **R0 — Workspace + ingestion parity (headless).** ✅ Cargo workspace; `core` with
  `events`, `config`, `store`, `context`, `filters`; the normalizer ported over
  `serde_json::Value` (robust to drift) with **all verified gotchas** (queue-op-first
  metadata, Write→create empty `structuredPatch`, redacted thinking, sidechains). The
  `tests/fixtures/*.jsonl` run as **golden tests**; `understudy tail` streams normalized
  events live.
- **R1 — Providers in core (headless).** ✅ Ollama / OpenAI-compatible / Copilot over
  `reqwest` behind a `Provider` enum (no `async-trait`) + a shared SSE/NDJSON parser;
  `<think>` streaming filter; `understudy ask` answers against a session. Live-verified:
  Ollama streamed a correct read of a real session; Copilot token exchange + model list.
- **R2 — ratatui shell: Activity + Glance.** 🟡 *Foundation built:* async loop
  (`tokio::select!` over crossterm `EventStream` + an `mpsc` tailer task + render tick),
  session **picker**, live **Activity** feed (color-coded), and the **Glance/Tier-1**
  summary header; `TestBackend` snapshot tests. *Remaining:* the full cockpit layout +
  Glance Tier-2/at-a-glance synthesis.
- **R3 — Chat-first interaction.** 🟡 *Chat done:* a toggleable streaming **Chat** panel
  (`c`/`/` opens it beside the feed) — questions carry the activity stream + conversation
  history as context, answers stream in via a spawned task + `mpsc` with the `<think>`
  filter applied, decoupled from the agent. *Remaining:* the always-typeable focus model
  (`Tab` panels), `Enter`-pin → Detail + chat reference, **slash commands**, **palette**,
  and richer markdown via `tui-markdown`.
- **R4 — Thinking, settings, polish, packaging.** Thinking lane + expand; setup/settings
  as an overlay + slash-driven; responsive breakpoints; mouse; theme; **single-binary
  packaging** (`cargo install`, release artifacts, Homebrew tap).
- **R5 — Cutover.** Parity checklist vs Python; make Rust the repo's primary; move Python
  to `legacy/` (or a `python-final` tag); update README/CI.

## Testing

- **Domain golden tests** — reuse `tests/fixtures/*.jsonl`; assert the same facts the
  Python suite does (kinds-in-order, `structuredPatch`, `Write`-create all-added diff,
  queue-op metadata scan, `<think>` streaming filter). This is the safety net for the
  port.
- **Provider parsing** — unit tests over canned SSE/NDJSON chunks (mirrors the Python
  SSE/think-filter tests).
- **UI** — ratatui **`TestBackend`** snapshot tests: render each panel/the cockpit into a
  fixed-size buffer from a known `App` state and assert the buffer; key-routing tests for
  the focus model (the analog of today's Textual `run_test` pilot tests).
- **Live smoke** — `understudy --check`: tail a session + one provider round-trip, like
  the live verifications done during Python phases.

## Risks & mitigations

| Risk | Mitigation |
|---|---|
| Iteration slows vs Python | `core`/`tui` split keeps most logic unit-testable without the UI; lean on headless `tail`/`ask` |
| Two codebases during migration | Freeze Python to bugfix-only after R2; time-box R3–R5 |
| ratatui immediate-mode learning curve | Adopt the official async/component template; avoid deep abstraction |
| Streaming/cancellation bugs | Request ids + abortable tasks; drop stale deltas on new request / session switch |
| CC JSONL format drift | Same defense as today: `serde` with `#[serde(other)]`/optional fields, log-and-continue, golden tests pinned to observed shapes |
| Rendering fidelity (markdown/diff) | `tui-markdown` + manual diff coloring from `structuredPatch`; `syntect` later if wanted |
| Rewrite + redesign at once is a lot | Phasing makes each Rxx shippable; the redesign rides on R2–R4 only |

## Decisions for you

1. **Migration shape:** incremental/strangler (recommended) vs. big-bang on a branch.
2. **Python after R2:** freeze to bugfix-only (recommended) vs. keep evolving in parallel.
3. **Build the new UX once, in Rust** (recommended) vs. prototype it in Textual first
   (faster to feel, but double-builds the UI).
4. **LLM access:** hand-rolled `reqwest` (recommended, covers all three providers) vs.
   an SDK crate.
5. **Config format:** TOML (nicer to hand-edit) vs. keep JSON (parity with today).

## Sources

- [Ratatui — Async Event Stream](https://ratatui.rs/tutorials/counter-async-app/async-event-stream/) and [Full Async Events](https://ratatui.rs/tutorials/counter-async-app/full-async-events/)
- [Ratatui async-template (structure)](https://ratatui.github.io/async-template/02-structure.html) · [component template](https://ratatui.rs/templates/component/tui-rs/)
- [tui-textarea](https://github.com/rhysd/tui-textarea) · [tui-markdown](https://docs.rs/tui-markdown) · [awesome-ratatui](https://github.com/ratatui/awesome-ratatui)
- [reqwest streaming OpenAI demo](https://github.com/a-poor/openai-stream-rust-demo) · [async-openai](https://github.com/64bit/async-openai) (reference)
