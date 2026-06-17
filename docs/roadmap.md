# Roadmap

Phased so that **value lands early and risk is retired in order**. The riskiest
assumption (can we observe the stream read-only?) is already validated; the next
risk (is the live feed actually useful?) is retired in Phase 0 *before* any LLM spend.

## Phase 0 — MVP: the read-only live feed (no LLM)

**Goal:** attach to a running Claude Code session and watch normalized events stream
in, with working diffs. Proves the whole ingestion spine end-to-end.

- [ ] `events.py` — `Event`, `Kind`, `Hunk`, `SourceInfo`.
- [ ] `sources/claude_code.py` — session discovery + robust JSONL tail (offset,
      partial-line buffer, rotation) + normalizer (records → events).
- [ ] `store.py` — append log, rolling window, turn grouping, derived indices
      (files touched, tool histogram, current tool, errors).
- [ ] `summary/deterministic.py` — Tier-1 instant summary line.
- [ ] `tui/` — Textual app with **Feed** + **Live summary (Tier-1)** + **Diff/Detail**
      panes; select an event → render its `structuredPatch` / tool I/O.
- [ ] `cli.py` — `understudy [--cwd] [--session]`.
- [ ] `tests/` — golden-file normalizer tests from captured fixtures.

**Exit criteria:** run `understudy` in one terminal, drive Claude Code in another,
and see edits/tool-calls appear within ~1 s with correct diffs. **No API key needed.**

## Phase 1 — Comprehension chat + smart summary  ✅

**Goal:** ask questions about the stream and get a debounced "what & why."

- [x] `models/` — `ModelProvider` protocol + **Ollama**, **OpenAI-compatible**, and
      **GitHub Copilot** (token-exchange) providers, all `httpx`-streamed.
- [x] First-run setup wizard + **F2** settings screen; persisted config; *Detect models*
      / *Test connection*.
- [x] `chat/session.py` — decoupled conversation; activity stream rebuilt as context
      each turn; `<think>…</think>` stripping for local reasoning models.
- [x] `tui/chat.py` — toggleable panel, streaming transcript.
- [x] `summary/llm.py` — Tier-2 debounced "what & why", layered over the deterministic line.
- [ ] Later: prompt caching for cloud providers, context compaction for very long
      sessions, per-session model override flag.

**Exit criteria:** ✅ asked "what is it doing and why?" against a real session and got a
correct, grounded answer from a live local model — agent undisturbed, fully offline in
Ollama mode. Copilot token exchange + chat verified live.

## Phase 2 — Thinking-token viewer

**Goal:** make reasoning legible without drowning the user.

- [ ] `tui/thinking.py` — collapsible raw view (collapsed by default).
- [ ] LLM-summarized "thought pattern" header, e.g. *"considered moving X→Y, chose to
      rename instead"* (the motivating example).
- [ ] Graceful degradation when thinking text is absent/redacted (show block count +
      token estimate, not an error). **Validate against the live setup first** — see
      the thinking caveat in the integration doc.

## Phase 3 — Prove the abstraction: second & third sources

**Goal:** show the source-agnostic core pays off.

- [ ] `sources/opencode.py` — SSE adapter to the OpenCode server `/event` stream.
      *Easiest of the three* (real push API); good validation that the `Source`
      contract holds for a non-file source.
- [ ] `sources/copilot.py` — tail `~/.copilot/session-state/<session>/events.jsonl`
      (same pattern as Claude Code).
- [ ] Source auto-detect + picker when multiple agents are running.

## Phase 4 — Polish & distribution

- [ ] Optional hooks installer (`understudy install-hooks`) for lower latency + clean
      turn signals (opt-in; see integration doc).
- [ ] Sidechain/subagent track UI; session switching; feed filters; search.
- [ ] Config file (model, redaction rules, panes, key bindings).
- [ ] Packaging: `uv`/`pipx`-installable; `understudy` console script.

## The concrete first week

1. **Day 1** — scaffold repo (`uv init`, deps), `events.py`, capture real JSONL
   fixtures from `~/.claude/projects/...`.
2. **Day 2** — `sources/claude_code.py` normalizer against fixtures (golden tests
   green) — *no live tailing yet, just record → Event*.
3. **Day 3** — robust tail + session discovery; print events to stdout live.
4. **Day 4** — `store.py` + Tier-1 deterministic summary (still stdout/console).
5. **Day 5** — minimal Textual app: Feed + Summary + Diff panes wired to the store.

End of week = a working read-only live panel for Claude Code. LLM features build on
top from there.

## Risks & mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Thinking text empty/redacted in user's setup | Medium | Treat as bonus channel; degrade gracefully; validate live before Phase 2 |
| JSONL format drifts across CC versions | Medium | Golden-file tests; log-and-continue on unknown shapes; pin to observed schema + version field |
| Tail misses/duplicates on flush quirks | Low–Med | Offset tracking + partial-line buffer + rotation handling; optional hook nudges |
| LLM cost creeps with long sessions | Medium | Debounce + hash gate + prompt caching + context compaction + local mode |
| Privacy: activity streamed to cloud | Medium | `--local` (Ollama) from day one; redaction rules; cloud is opt-in per session |
| Feed too noisy to comprehend (irony) | Medium | Filters, sidechain collapsing, Tier-1 digest, truncation — the whole point is *less* noise |

## Open questions (for later, not blocking)

1. **Attach UX** — standalone terminal (default), or a tmux split / launcher that
   opens the side-car beside the agent automatically? Default to standalone now.
2. **Multi-session** — watch several projects at once, or one at a time? One first.
3. **Persistence** — keep a comprehension history per session for later review, or
   ephemeral? Ephemeral first.
4. **Redaction policy** — secrets/.env contents can appear in diffs and tool output;
   what's redacted before it reaches a cloud model? Needs a default rule set.
5. **Thinking summary granularity** — per-block, per-turn, or running? Decide with
   real data in Phase 2.
