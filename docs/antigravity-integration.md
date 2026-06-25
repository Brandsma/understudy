# Antigravity CLI integration

Everything here was **verified against a real database** on this machine. Unlike the other
adapters, Antigravity persists its transcript as **protobuf blobs with no published schema**, so
the field numbers below were reverse-engineered and may shift across Antigravity releases.

## Storage location

```
~/.gemini/antigravity-cli/conversations/<conversation-id>.db
```

- One **SQLite database per conversation**, named by the conversation id (the cascade id).
- `~/.gemini/antigravity-cli/cache/last_conversations.json` maps each workspace to its most
  recent conversation id; `~/.gemini/antigravity-cli/history.jsonl` logs prompts with their
  workspace and conversation id. The adapter doesn't need either — it reads the DBs directly.
- The DB is opened **read-only** (with `SQLITE_OPEN_URI`), tolerating the live `-wal`/`-shm`
  files Antigravity keeps while a session is open.

## Schema

Two tables matter:

- **`trajectory_metadata_blob`** — a single protobuf blob with conversation metadata. Field `7`
  is the `file://` workspace URI (→ cwd); field `2` is the creation time (`.1` seconds, `.2`
  nanos).
- **`steps`** — the transcript, one row per step. Relevant columns:
  - `idx` — **0-based** ordering key (the tailing cursor; note the cursor starts at `-1`).
  - `step_type` — selects how the payload is normalized (see below).
  - `error_details` — non-empty when the step failed (→ `ToolResult.ok = false`).
  - `step_payload` — the protobuf blob.

## Step payload layout (protobuf field paths)

The adapter carries a tiny schema-free wire reader and pulls fields by number. Paths are written
`a.b.c` (nested length-delimited messages).

| `step_type` | meaning | normalized to | key paths |
| --- | --- | --- | --- |
| 14 | user prompt | `UserPrompt` | text `19.2` |
| 15 | assistant turn | `AssistantText` + `ToolCall` | text `20.1`; call `20.7` → id `.1`, name `.2`, args JSON `.3` |
| any with a `5.4` block | tool execution result | `ToolResult` (+ `FileEdit` for `write_to_file`) | id `5.4.1`, name `5.4.2`, args JSON `5.4.3`, summary `5.30`/`5.31` |
| other (e.g. 23 title, 98 system) | — | skipped | — |

Notes:

- A tool call is announced in a **type-15** step and its result lands in a **following** step
  (`view_file` = 8, `list_dir` = 9, `grep_search` = 7, `run_command` = 21, `write_to_file` = 5,
  …). Rather than enumerate every tool's type id, any step carrying a `5.4` block is treated as
  a result.
- **Tool args are JSON strings embedded in the protobuf** (e.g.
  `{"AbsolutePath":"…/Cargo.toml","toolAction":"Viewing Cargo.toml"}`), parsed with `serde_json`.
- The **result body** lives in a tool-specific field (file contents, command output, grep
  matches, …). Instead of mapping each, the adapter takes the longest human-readable text leaf
  outside the `5.x` tool block as the detail (clipped to 2000 chars).
- **Timestamps** come from the first present nested time sub-message (`5.8` → `5.7` → `5.6` →
  `5.1`, each `.1` seconds + `.2` nanos), falling back to the conversation creation time.
- `write_to_file` carries `TargetFile` + whole-file `CodeContent` in its args, synthesized into
  an all-added `FileEdit` (`created = !Overwrite`), mirroring the OpenCode `write` path.

## Not captured

- Per-tool result bodies are best-effort (longest text leaf), not field-precise.
- Edit tools other than `write_to_file` (if any) emit a `ToolResult` but no `FileEdit`.
- `git_branch` is left empty (it isn't cleanly addressable in the metadata blob).
