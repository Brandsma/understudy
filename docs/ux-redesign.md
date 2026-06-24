# UX redesign — the chat-first comprehension cockpit

Target experience for v2. Implementation-agnostic (it informs the Rust build in
[rust-migration.md](rust-migration.md) — we build *this*, not a port of today's UI).

## North star

Understudy becomes a **chat-first comprehension cockpit**. The chat is the spine —
an always-present input and transcript, like Claude Code — and live panels around it
render the agent's state so you grasp the whole session from **one screen**. You
interrogate by typing; you never "open" a chat mode, and you rarely change screens.

## Principles

1. **Chat is always live.** The input bar is focused by default, from first launch to
   exit. Typing always works; Enter always sends. Meta-actions happen via slash
   commands and a command palette, so you stay on the chat surface "from beginning to
   end."
2. **One screen to comprehend a session.** A dedicated *Glance* panel synthesizes
   what/why/where so you understand the run without scrolling the raw feed.
3. **Progressive disclosure on a single screen.** Glance = digest · Activity = stream ·
   Detail/Thinking = drill-down · Chat = ask. Four depths, no screen changes.
4. **Read-only, calm, real-time.** Panels update live without stealing focus or
   reflowing under you. Generation shows a spinner, never a freeze.
5. **Responsive.** Degrades from a wide cockpit to a stacked layout on small terminals.

## The cockpit (default wide layout)

```
┌ Understudy · waggle @ main · ollama/qwen3 · ● live ─────────────────────────────┐
│ ┌ GLANCE ──────────────────┐ ┌ ACTIVITY ─ live ──────────────┐ ┌ THINKING ─────┐ │
│ │ ▶ editing store.py        │ │ 12:01 → Read store.py         │ │ ✲ refactor    │ │
│ │   (edit 3/3 this turn)    │ │ 12:01 ✲ refactor over rename  │ │   over rename │ │
│ │ 7 files · 12 tools · 1 ✗  │ │ 12:02 ± store.py +14 −3       │ │ ✲ group events│ │
│ │                           │ │ 12:02 ← pytest ✗ 1 failed     │ │   by turn_id  │ │
│ │ ≈ refactoring the event   │ │ 12:03 → Edit store.py         │ │ 3 of 8 shown  │ │
│ │   store to group by turn; │ │ ▏streaming…                   │ │ · expand: /th │ │
│ │   tests just failed, it's │ │                               │ └───────────────┘ │
│ │   tracing the import      │ │                               │ ┌ DETAIL ───────┐ │
│ │                           │ │                               │ │ store.py +14−3│ │
│ │ files  store.py app.py …  │ │                               │ │  +def group_b…│ │
│ └───────────────────────────┘ └───────────────────────────────┘ │  −old_loop()  │ │
│ ┌ CHAT ─────────────────────────────────────────────────────────│───────────────┐ │
│ │ ◆ understudy  It refactored the store to group events by turn; │ pytest then   │ │
│ │               failed on an import it is now tracing.           │ failed…       │ │
│ │ › why did pytest fail?▏                                        │               │ │
│ └───────────────────────────────────────────────────────────────────────────────┘ │
└─ tab: panes · ↑↓ scroll · enter: pin→chat · / slash · ^k palette · ^q quit ────────┘
```

### Panels

| Panel | Role | Source |
|---|---|---|
| **Status bar** | session identity · branch · model · ● live/idle · token/cost meter | store + provider |
| **Glance** | the comprehension digest: current action, file/tool/error tallies, the Tier-2 "what & why", files-touched list | deterministic indices + Tier-2 LLM |
| **Activity** | the live, color-coded event stream; selectable rows | normalized events |
| **Thinking** | compact lane of recent thought-pattern summaries; expandable | thinking events + summarizer |
| **Detail** | context-sensitive: diff for a selected `file_edit`, I/O for a `tool_call`, text for thinking | selected event |
| **Chat** | persistent transcript + input; the primary interaction | comprehension model |

Color semantics carry over from today: cyan user · yellow tool calls · green results ·
blue edits · magenta thinking. The **active** panel (for scroll/selection) shows a
highlighted border; the chat input keeps a caret regardless.

## Interaction model (the crux of "chat-first")

A chat-first app must let you both *watch* (scroll panels) and *ask* (type) without
mode friction. Recommended model:

- **Default focus = chat input.** Type anytime; Enter sends to the comprehension model.
- **`Tab` / `Shift-Tab`** cycle the *active panel* (border highlight) for scrolling and
  selection. Typing still goes to chat — Tab only changes what `↑↓`/`PgUp`-`PgDn`/`Enter`
  act on. **`Esc`** returns to "no active panel" (pure chat).
- **`Enter` on a selected Activity row** pins it to Detail *and* drops a reference token
  into the chat input (e.g. `@edit:store.py@12:02`), so "why this?" is one keystroke
  away. This is the core comprehension gesture.
- **Slash commands in chat** for everything meta — so you never leave the surface:
  `/session` (switch), `/here`, `/model`, `/settings`, `/thinking`, `/summarize`,
  `/diff <file>`, `/follow`, `/clear`, `/help`.
- **Command palette (`Ctrl-K`)** — fuzzy launcher for the same actions, for discovery.
- **Mouse** — click a panel to make it active; wheel scrolls; click a row to pin it.

> Considered and rejected as the default: a **modal (vim-like)** normal/insert split.
> It's powerful but adds a mode the chat-first goal is trying to remove. We can offer it
> later as an opt-in for power users.

## Session selection — understandable from one screen

Two halves, both required:

**1. A scannable picker.** Each session is a rich row/card, not just a filename:

```
 PROJECT                BRANCH   WHEN      STATUS   SUMMARY
 waggle                 main     2m ago    ● live   Refactor event store to group by turn
 tether-art             main     1h ago    idle     Add SVG export + palette picker
 asr-innovation         master   yest.     idle     Slide deck: tighten copy, 2-col layout
 ────────────────────────────────────────────────────────────────────────────────────
 ↑↓ select · enter attach · / filter · n new-only · ^q quit
```

Columns: project · branch · relative time · **live/idle** (is something writing now) ·
AI-title/last-prompt. Optional: a tiny activity sparkline, files-touched count. Sorted
by recency; type-to-filter. So you comprehend each option before attaching.

**2. Instant at-a-glance on attach.** Selecting a session backfills and immediately
populates the **Glance** panel — current/last action, tallies, files touched, and a
first Tier-2 "what & why". Within one screen you know: which project/branch, what it's
been doing, whether it's live, and the gist — before reading a single raw event.

Entry paths: `understudy` (picker) · `understudy --here` (auto-attach this cwd) ·
`/session` from chat to switch live.

## Visual & responsive

- **Status bar + footer** frame every screen; footer keys are context-sensitive to the
  active panel.
- **Throbber** while the model streams (chat and Tier-2); partial tokens render live.
- **Breakpoints:**
  - *Wide* (≥120 cols): full cockpit as above.
  - *Medium* (80–120): Glance left, a tabbed right column (Activity / Thinking / Detail
    via `Tab`), Chat docked bottom.
  - *Narrow* (<80) / short: stacked — Chat + Glance always visible; Activity/Detail on a
    key. Chat is never sacrificed.
- **Theme:** honor terminal palette; a light/dark-safe default; configurable accent.

## What changes vs today

| Today (Textual) | v2 (cockpit) |
|---|---|
| Feed screen is primary; chat is a toggled bottom panel (`c`) | Chat is the persistent spine; feed is one panel among several |
| Thinking is a separate pushed screen (`t`) | Thinking is a live lane; expand on demand / `/thinking` |
| Settings & session-switch are separate screens | Slash commands + palette, on the chat surface |
| Detail shows on row highlight | Row `Enter` pins to Detail **and** references it in chat |
| "Understand the session" = read the feed | Glance panel synthesizes it in one block |

## Open decisions

1. **Focus model:** chat-first + `Tab` panels + slash/palette (recommended) vs. opt-in
   modal. → default to the former.
2. **Reference tokens** (`@edit:store.py@12:02`): auto-inserted on pin, or only via a
   `/diff`/`/why` command? → start with auto-insert on `Enter`-pin.
3. **Session picker as a panel vs. a first screen:** a first screen is simpler; a
   `/session` overlay keeps the "never leave chat" promise. → first screen for launch,
   overlay for switching.
4. **Persistence:** keep a per-session comprehension transcript across runs, or
   ephemeral? → ephemeral first (matches read-only, low-stakes).
