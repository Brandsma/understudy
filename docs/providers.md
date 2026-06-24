# Model providers

The comprehension chat and the Tier-2 summary talk to a model through one `Provider`
enum ([models/mod.rs](../crates/core/src/models/mod.rs)). Three implementations ship
today; all stream over `reqwest` (no heavy SDKs).

| Provider | Kind | Where it runs | Auth |
|---|---|---|---|
| **Ollama** | `ollama` | Local | none |
| **OpenAI-compatible** | `openai` | Cloud or local server | Bearer API key (or env) |
| **GitHub Copilot** *(experimental)* | `copilot` | Cloud | your existing Copilot login |

Set `kind` in the config file below (the in-TUI setup wizard is part of the in-progress
UX). `understudy check` validates connectivity/auth; each provider can list models.

## Configuration

Stored as JSON at the platform config dir (e.g. `~/Library/Application Support/understudy/config.json`
on macOS, `~/.config/understudy/config.json` on Linux). Override the path with
`UNDERSTUDY_CONFIG`. Shape:

```json
{
  "provider": { "kind": "ollama", "base_url": "http://localhost:11434",
                "api_key": "", "model": "qwen3:4b", "temperature": 0.3 },
  "summary_enabled": true,
  "summary_debounce": 2.0,
  "configured": true
}
```

`configured` flips to `true` after setup (or *Skip*) so the wizard doesn't nag again.

## Ollama (local, private)

- Native `/api/chat` for streaming, `/api/tags` for model discovery.
- Default base URL `http://localhost:11434`. Keeps the agent's activity entirely
  on your machine — the privacy-preserving default.
- Reasoning models (qwen3, deepseek-r1) emit `<think>…</think>`; the chat/summary
  strip these automatically ([filters.rs](../crates/core/src/filters.rs)).

## OpenAI-compatible

- Generic `/v1/chat/completions` + `/v1/models`. Works with OpenAI, OpenRouter,
  Together, vLLM, LM Studio, llama.cpp, etc. Base URL should end in `/v1`.
- API key from the config field, or `OPENAI_API_KEY` if it is blank.

## GitHub Copilot (experimental)

Consumes Copilot as a model backend via its OpenAI-compatible API — **not** the official
Node SDK, but a dependency-light HTTP bridge:

1. Find an existing OAuth token: env `COPILOT_GITHUB_TOKEN` / `GH_TOKEN` / `GITHUB_TOKEN`,
   else `~/.config/github-copilot/hosts.json` (created by VS Code / `gh` / Copilot CLI).
2. Exchange it at `GET https://api.github.com/copilot_internal/v2/token` for a
   short-lived Copilot token (cached until ~60s before expiry).
3. Call `{base}/chat/completions` with the Copilot headers (`Copilot-Integration-Id`,
   `Editor-Version`, …). The base host is derived from the token's `proxy-ep`.

**Caveats:** unofficial and may break if GitHub changes endpoints/headers; requires an
active Copilot subscription; respect GitHub's terms. Verified working (token exchange +
32 models listed) at the time of writing.

## Adding a provider

Add a struct with `stream_chat`, `list_models`, and `check`, a variant to the `Provider`
enum, and a branch in `build_provider` — all in
[models/mod.rs](../crates/core/src/models/mod.rs). The chat, summarizer, and CLI need no
changes (a thin SSE/NDJSON parser in `models/http.rs` is reused).
