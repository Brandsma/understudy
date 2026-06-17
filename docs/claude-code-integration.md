# Claude Code integration (the priority adapter)

Everything here was **verified against real transcripts** on this machine, not just
docs. Where behavior varies, it's called out as a caveat.

## Storage location

```
~/.claude/projects/<encoded-cwd>/<session-uuid>.jsonl
```

- `<encoded-cwd>` is the absolute working directory with `/` and `.` replaced by `-`.
  Example: `/Users/abrandsma/main/personal/side-car-comprehension` →
  `-Users-abrandsma-main-personal-side-car-comprehension`.
- One `.jsonl` file per session, named by session UUID. It is **append-only** and
  grows in near-real-time as the agent works — exactly what we need to tail.
- The root honors `CLAUDE_CONFIG_DIR` (defaults to `~/.claude`). The adapter must
  read that env var, not hardcode `~/.claude`.
- Files are garbage-collected after `cleanupPeriodDays` (default 30) — irrelevant for
  a live tail, relevant only for "open a past session."

## Session discovery

To attach to "the session running in this project right now":

1. Encode the target cwd → locate `~/.claude/projects/<encoded-cwd>/`.
2. Pick the `.jsonl` with the newest mtime (the active session).
3. Watch the **directory** too: a brand-new session (e.g. after `/clear` or a fresh
   `claude`) creates a new file; when a newer file appears, offer to follow it.

> **Verified gotcha:** the *first* record(s) of a transcript are often
> `type: "queue-operation"` (or `summary`/`ai-title`) which carry `sessionId` but
> **no `cwd`/`gitBranch`**. Don't read identity from line 1 — scan the first ~20
> records and take each field from the first record that has it.

CLI surface: `understudy` (defaults to `$PWD`), `--cwd <path>` to target another
project, `--session <uuid>` to pin a specific transcript.

## Record shape (observed)

Each line is one JSON object. Top-level keys seen across records:

```
type            "user" | "assistant" | "attachment" | "ai-title" |
                "last-prompt" | "queue-operation" | "summary" | ...
sessionId       session UUID
timestamp       ISO-8601
uuid            this record's id
parentUuid      previous record's id  → forms a linked list (null at root)
isSidechain     bool  → true for subagent/Task activity
gitBranch       branch at time of record
cwd             working dir
version         Claude Code version
message         the Anthropic-style message (see below)  [user/assistant only]
requestId       API request id          [assistant only]
toolUseResult   structured result of a tool             [on the user record that
                                                         carries a tool_result]
```

`message.content` is an **array of content blocks**, with these `type`s observed:

| Block type | Where | Fields |
|---|---|---|
| `text` | assistant & user | `text` |
| `thinking` | assistant | `thinking` (plaintext), `signature` |
| `tool_use` | assistant | `id`, `name`, `input` |
| `tool_result` | user | `tool_use_id`, `content`, `is_error` |

> The pairing: an `assistant` record emits `tool_use` blocks; the following `user`
> record carries the matching `tool_result` block **and** a top-level `toolUseResult`
> with the richer structured payload.

## `toolUseResult` shapes (observed, per tool)

This is where the good structured data lives. Confirmed shapes:

**Bash**
```json
{ "stdout": "...", "stderr": "...", "interrupted": false,
  "isImage": false, "noOutputExpected": false }
```

**Edit / Write — the diff is pre-computed for us:**
```json
{ "filePath": "/abs/path/file.json",
  "oldString": "...", "newString": "...",
  "originalFile": "<full file before edit>",
  "replaceAll": false, "userModified": false,
  "structuredPatch": [
    { "oldStart": 1, "oldLines": 6, "newStart": 1, "newLines": 6,
      "lines": [
        " {",
        "   \"metadata\": {",
        "-    \"title\": \"Lely GitHub Copilot Workshop Challenge\",",
        "+    \"title\": \"ASR GitHub Copilot Workshop Challenge\",",
        "     \"description\": \"...\","
      ] } ] }
```

**WebSearch**
```json
{ "query": "...", "results": [...], "searchCount": 1, "durationSeconds": 2.1 }
```

→ The **diff viewer renders `structuredPatch` directly**. Each `lines` entry is
already prefixed with ` ` (context), `+` (added), or `-` (removed); `oldStart/
newStart/oldLines/newLines` place the hunk. No diff algorithm needed on our side.

**Write (new file) is different — verified during implementation.** A `Write` that
creates a file produces `"type": "create"`, an **empty** `structuredPatch: []`, an
empty `originalFile`, and the full new file in a top-level `content` field:

```json
{ "type": "create", "filePath": "/abs/path/new.py",
  "content": "<full file>", "originalFile": "", "structuredPatch": [] }
```

So the adapter must special-case it: when `structuredPatch` is empty but `content`
is present, synthesize an all-added diff from `content` (added = line count,
removed = 0) and mark the edit as a creation. An overwrite of an existing file
instead carries a populated `structuredPatch` like Edit.

## Thinking tokens — feasible, but best-effort ⚠️

- In most inspected sessions, `thinking` blocks contain **readable plaintext** (one
  session had 331 non-empty blocks). So the thinking viewer (Phase 2) is viable.
- **But** in some sessions — including one captured live during this planning — the
  `thinking` field was **empty**. Availability varies with model, Claude Code
  version, and config (e.g. interleaved/extended-thinking settings). There may also
  be `redacted_thinking` blocks that are encrypted by design.
- **Design rule:** treat readable thinking as a *bonus channel*. The viewer must
  degrade gracefully — when text is absent, show "thinking occurred (N blocks, ~T
  tokens), content not exposed" rather than breaking. Validate against the user's
  actual setup before promising the summarized-thought-pattern feature.

## Mapping CC records → normalized events

| CC record / block | Normalized `Event` |
|---|---|
| first record of session | `SESSION_START { session_id, cwd, version }` |
| `user` text block (real user) | `USER_PROMPT { text }` |
| assistant `text` block | `ASSISTANT_TEXT { text }` |
| assistant `thinking` block | `THINKING { text }` (text may be empty) |
| assistant `tool_use` block | `TOOL_CALL { id, name, input }` |
| `user` `tool_result` + `toolUseResult` | `TOOL_RESULT { id, name, ok, summary, detail }` |
| `toolUseResult` with `structuredPatch` | also emit `FILE_EDIT { path, hunks, +/- }` |
| `Stop` hook (optional) / next user prompt | `TURN_END` |

Carry `is_sidechain` straight through; group by `turn_id` derived from the
`parentUuid` chain between consecutive user prompts.

## Tailing strategy

A robust file tail, not a naive `tail -f`:

1. On attach: read the whole file once for **backfill** (bounded to last N turns),
   recording the byte **offset** at EOF.
2. Watch the file with `watchfiles` (Rust-backed; FSEvents on macOS). On change,
   `seek(offset)` and read appended bytes; advance offset.
3. **Buffer partial lines** — Claude Code may flush mid-line; only parse on `\n`,
   keep the remainder for the next read.
4. **Handle rotation/truncation** — if size < offset (file replaced) or a newer file
   appears in the dir, reset and re-attach.
5. Parse each complete line as JSON; skip/"log-and-continue" on malformed lines
   (forward-compat: never crash on an unknown `type` or new field).

Latency: file-flush driven, typically sub-second per event — fine for "what is it
doing *right now*." For tighter latency or an explicit turn-complete signal, layer in
hooks (below).

## Optional enhancement: hooks (push + turn signal)

Purely optional — the MVP needs none of this — but hooks give lower latency and a
clean turn-boundary signal. They require editing `~/.claude/settings.json` (or
project `.claude/settings.json`), so they're **opt-in** and offered via a
`understudy install-hooks` helper, never silently.

Each hook receives JSON on **stdin** including `session_id`, `transcript_path`,
`cwd`, `hook_event_name`, plus event-specific fields (`tool_name`, `tool_input`,
`tool_use_id` for tool hooks). Useful events:

- `PostToolUse` — immediate "a tool just ran" ping (lower latency than waiting for
  the JSONL flush).
- `Stop` / `SubagentStop` — authoritative "turn finished" → trigger the Tier-2 LLM
  summary at exactly the right moment.
- `UserPromptSubmit` — clean turn-start marker.

Transport: the hook is a tiny script that POSTs its stdin JSON to a localhost port
(or unix socket) the side-car listens on. The side-car still relies on the JSONL as
the source of truth for *content*; hooks are just nudges + boundaries.

```jsonc
// ~/.claude/settings.json (installed opt-in)
{
  "hooks": {
    "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command",
      "command": "understudy-hook --port 7717" }] }],
    "Stop":        [{ "hooks": [{ "type": "command",
      "command": "understudy-hook --port 7717" }] }]
  }
}
```

## Edge cases to handle

- **Sidechains / subagents** (`isSidechain: true`) — render in a collapsed, indented
  track; don't let parallel subagent spew drown the main thread.
- **Attachments** (`type: "attachment"`) — images/file refs; show a chip, don't try
  to inline.
- **Queue operations** (`type: "queue-operation"`) — user queued a message; surface
  as a faint marker, it's not agent activity.
- **Compaction** — long sessions get summarized/compacted; a `summary`/compaction
  record appears. Treat it as a turn boundary and reset the rolling window's older
  tail to the provided summary.
- **Large tool output** — Bash `stdout` can be huge; store full, but truncate in the
  feed and for the summarizer (keep head+tail, note elided bytes).
- **Unknown/new `type` or fields** — log-and-continue. The format evolves across CC
  versions; the adapter must never hard-fail on it.

## Test fixtures

Capture a handful of real lines into `tests/fixtures/` (one per shape: `thinking`,
`tool_use` Bash, `tool_result` Bash, Edit with `structuredPatch`, WebSearch,
sidechain, attachment) and write golden-file tests `raw line → Event`. This pins the
normalizer against the real format and catches CC-version drift early.
