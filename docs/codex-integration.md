# Codex CLI integration

Discovery, the rollout envelope, and the user-message shape were **verified against real files**
on this machine. The richer `response_item` payload variants follow `openai/codex`'s
`codex-rs/protocol/src/models.rs` (the `ResponseItem` enum), since the local sessions available
here were stubs with no assistant/tool activity to verify against.

## Storage location

```
$CODEX_HOME/sessions/<YYYY>/<MM>/<DD>/rollout-<timestamp>-<uuid>.jsonl
```

- `CODEX_HOME` defaults to `~/.codex`. The adapter honors the env var.
- One **append-only JSONL file per session**, date-sharded. Discovery walks the tree recursively
  and matches `rollout-*.jsonl`; the session id is the trailing UUID (also stored in the meta).
- Append-only growth is exactly what we tail (same offset/cursor approach as the Claude Code
  adapter).

## Record envelope

Every line is `{ "timestamp": <rfc3339>, "type": <kind>, "payload": { … } }`. Record kinds:

| `type` | handled? | notes |
| --- | --- | --- |
| `session_meta` | yes → `SessionStart` | first line: `id`, `cwd`, `cli_version`, `git.branch` |
| `response_item` | yes | the canonical conversation stream (see below) |
| `event_msg` | skipped | UI events (`task_started`, `token_count`, `agent_message`, …) that **duplicate** the response-item stream |
| `turn_context`, `compacted` | skipped | bookkeeping |

Normalizing only `response_item` (plus `session_meta`) avoids double-counting: Codex writes both
an `event_msg` `agent_message` and a `response_item` `message` for the same assistant turn.

## `response_item` payload variants → events

`ResponseItem` is internally tagged on `type` (snake_case):

| payload `type` | → event | mapping |
| --- | --- | --- |
| `message` (role `user`) | `UserPrompt` | join `content[].text` (`input_text`/`output_text`) |
| `message` (role `assistant`) | `AssistantText` | same join |
| `message` (role `developer`) | skipped | injected instructions, not conversation |
| `reasoning` | `Thinking` | `summary[].text`, falling back to `content[].text` |
| `function_call` | `ToolCall` | `name`, `call_id`, `arguments` parsed from its JSON **string** |
| `local_shell_call` | `ToolCall` (name `shell`) | `action.command[]` joined into one line |
| `custom_tool_call` | `ToolCall` | `name`, `call_id`, `input` |
| `function_call_output`, `custom_tool_call_output` | `ToolResult` | paired to the call by `call_id`; `output` is a plain string or `{content/output, success}` |
| `web_search_call` | `ToolCall` (name `web_search`) | `action.query` |
| `image_generation_call`, `tool_search_*`, `compaction`, `additional_tools` | skipped | no observer-facing content |

Tool results are paired to their calls via a `call_id → name` map, the same technique the Claude
Code adapter uses for `tool_use` / `tool_result`.

## Filtered noise

- User messages whose text begins with `<environment_context>` or `<user_instructions>` are
  **Codex-injected** context, not typed prompts, and are dropped (including from the picker
  summary).
- `developer`-role messages are dropped.

## Not captured (deferred)

- **File edits.** Codex applies edits through the `shell` tool running `apply_patch` (a bespoke
  `*** Begin Patch` text format embedded in the tool arguments), not as a structured diff in the
  rollout. These surface as `ToolCall`/`ToolResult` rather than `FileEdit`. Synthesizing a
  `FileEdit` would require parsing the `apply_patch` envelope and was deferred (no local sample
  to verify against).
