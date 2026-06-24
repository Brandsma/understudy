# Comprehension Debt KPI — design

A best-effort, in-app metric estimating how much of what an agent *produced* a human
has actually *engaged with and understood*. This is the natural "so what" number for
Understudy: the app exists to fight comprehension debt, so it should be able to show you
how much you're carrying.

> **Status:** implemented (K1–K5). Tier-1 (deterministic gauge, `/debt`), Tier-2 (LLM
> tagging via `/tagging`, explain-back via `/explain`), and per-session persistence with
> the `understudy debt` trend view all ship. Prompt/weight tuning of the LLM tiers is
> expected once it runs against a capable local model.

## Why this fits Understudy

Standard engineering metrics are *blind* to comprehension debt — velocity, DORA, code
coverage, and PR counts all stay green while understanding rots (Osmani). The only
reliable signal anyone has identified is **whether a human can explain *why* a change was
made**.

Understudy is unusually well-placed to estimate this because it already sits in the
**inquiry channel**. It observes both sides of the gap:

- **Production** — every `file_edit` (with line ±), tool call, thinking block, and the
  semantic **segments** the work breaks into.
- **Comprehension** — what the human *pins*, *opens in Detail*, *navigates to*, and most
  importantly *asks about* in the chat.

The research (below) says the chat — conceptual inquiry — is the high-signal behavior. So
the KPI is not arbitrary instrumentation bolted on; it measures the exact interaction the
app was built to enable.

## Research grounding

- **Comprehension debt** (coined by Addy Osmani, early 2026): *"the growing gap between
  how much code exists and how much any human genuinely understands."* It accumulates
  invisibly and is unmeasured by standard metrics; the only reliable signal is whether
  someone **can explain why** a decision exists.
- **How you use AI matters more than whether you do.** An Anthropic randomized controlled
  trial (52 engineers) plus several 2026 studies converged: developers who **delegate**
  ("just make it work") score **< 40%** on comprehension quizzes, while those who use AI
  for **conceptual inquiry** — asking questions, exploring tradeoffs — score **> 65%**.
  AI-assisted work also scored ~17% lower on follow-up quizzes overall, worst in debugging.
- **Pace gap:** AI generates code ~5–7× faster than humans can read it.
- **Existing analogues:** *review coverage* (% of PRs with ≥1 review) and *diff coverage*
  (% of changed lines tested). Comprehension Coverage is the missing sibling — **diff
  coverage, but for human attention instead of tests.**

These thresholds (40% / 65%) are reused directly as the gauge's color bands (see UI).

## The metric: Comprehension Coverage

Framed like diff coverage so it reads instantly:

```
Comprehension Coverage = understood production ÷ total production
Comprehension Debt      = 1 − coverage         (also shown absolutely)
```

- **Production unit = a segment** (a model-determined block of coherent work). Segments are
  weighted by **lines changed** in the rollup, so a large unreviewed block counts as more
  debt than a tiny one. (Units stay segment-level; line counts are only a *weight*, not a
  granularity — line-level units were rejected as too narrow.)
- **Coverage** = Σ(segment_lines × segment_score) ÷ Σ(segment_lines), where
  `segment_score ∈ {0.0, 0.5, 1.0}` by comprehension state.
- **Debt** is shown two ways: a **percentage** (`1 − coverage`) and an **absolute**
  count ("6 of 9 segments unreviewed · 287 unread lines").

### Per-segment comprehension state

Each segment carries the **strongest** interaction that has touched it:

| State | Score | Earned by |
|---|---|---|
| ○ unseen | 0.0 | no interaction |
| ◐ skimmed | 0.5 | navigated to it, or pinned/opened one of its diffs in Detail (passive reading) |
| ● understood | 1.0 | genuine **inquiry** attributed to it (asked a real question), or passed an **explain-back** check |

Weighting inquiry far above passive viewing is the whole point: it encodes the
research finding that interrogation predicts understanding and skimming does not.

## Two tiers (matches the app's "instant first, smart second" bet)

**Tier 1 — deterministic, free, live.** Pure interaction accounting, no model. Track a
"seen set" of event/segment indices that were pinned or opened, and attribute chat
questions to whatever segment was pinned at ask-time (heuristic). Recomputes instantly
on every interaction. This alone gives a working gauge with zero API cost.

**Tier 2 — LLM, opt-in, smart.** Two capabilities layered on the configured provider:

1. **Implicit attribution + inquiry classification (the locked attribution choice).** The
   observer model tags each chat exchange with the segment(s) it concerns and classifies
   it as *inquiry* vs *delegation* vs *other*. Inquiry questions raise their segments to
   **understood**; delegation-style questions do not (they count as skim at most). This
   replaces Tier-1's pin heuristic with content-based attribution and operationalizes the
   inquiry/delegation distinction directly. Runs debounced or on demand.
2. **Explain-back check (the headline opt-in).** Select a segment → the model asks *"why
   did the agent do this here?"* → you answer in chat → the model grades your answer
   against that segment's actual activity (events + diffs) → pass / partial / fail. A pass
   sets the segment to **understood**. This is the only mechanism that approximates
   Osmani's "can you explain why," and it turns Understudy from a gauge into an active
   comprehension *trainer*.

## Honesty & anti-gaming

This is a **best-effort proxy for engagement, not a measurement of true understanding** —
and the doc, the UI label, and the framing must all say so.

- "Seen" ≠ "understood." Opening every diff for 200 ms could inflate the *skimmed* tier;
  we mitigate by weighting inquiry and explain-back far above passive views, and by never
  claiming the gauge equals understanding.
- It is framed as a **risk indicator**, not a verdict: high debt reads as *"you've engaged
  little with a lot of produced code — here's what's unreviewed,"* not *"you don't
  understand this."*
- Per Osmani, no metric fully captures comprehension; **explain-back** (Tier 2) is the only
  feature that comes close, which is exactly why it exists. The label carries an `(est.)`.

## Persistence & trending

Per-session scores are **persisted** so comprehension debt can be **trended over time** —
this is where it becomes a real team KPI rather than a momentary gauge. (A deliberate
departure from the roadmap's "ephemeral first" default, chosen for this feature; all data
stays local, consistent with the privacy-aware principle.)

- At session checkpoints / end, append one record to a local JSONL ledger under the
  platform data dir (e.g. `~/.local/share/understudy/comprehension.jsonl`):

  ```json
  { "ts": "2026-06-24T16:40:00Z", "project": "understudy", "branch": "main",
    "session_id": "…", "segments": 9, "lines_changed": 412,
    "coverage": 0.38, "debt": 0.62, "per_segment": [{"title":"…","score":0.5}, …] }
  ```

- A trend view (`understudy debt` CLI and/or an in-app summary) aggregates the ledger by
  project to show coverage over time — the "are we accumulating debt?" question.

## Surfacing in the UI

- **Status-bar gauge:** `comp 38% (est.)`, colored by the research bands — **red < 40%**,
  **yellow 40–65%**, **green ≥ 65%**.
- **Segments timeline:** each row carries its state glyph (○ / ◐ / ●), so the sidebar
  doubles as a review checklist; the rollup is the gauge.
- **`/debt`** chat command prints the breakdown (coverage, unreviewed segments, unread
  lines). **`/explain [segment]`** triggers the explain-back check.

## Locked decisions

| Decision | Choice | Rationale |
|---|---|---|
| Granularity | **Segment-level** (line-weighted) | Reuses the segmenter; "did you understand each block of work?"; line-level rejected as too narrow |
| Attribution | **Implicit — LLM tags questions → segments** | Content-based, also classifies inquiry vs delegation; pin-at-ask-time is the Tier-1 fallback |
| Honesty stance | **Humble risk indicator**, with **explain-back** as opt-in | Ship the gauge as a proxy; the only true signal (explain-why) is the opt-in superpower |
| Scope | **Persist per-session, trend over time** | Becomes a real KPI; local-only data |

## Data: have vs. need

**Already have:** segments (event ranges, lines ±, tool counts), `file_edit` events, the
chat log, and pin/selection state in the cockpit.

**Need to add:**
- a tracked **seen set** (event/segment indices pinned or opened);
- **attribution** of chat questions to segments (Tier-1 pin heuristic → Tier-2 LLM tagging);
- a **per-segment comprehension state** derived from the above;
- a **persistence layer** (new `core` module) for the JSONL ledger + a trend reader.

## Open questions (non-blocking)

1. **Score for "asked a question" without explain-back** — is inquiry alone a full 1.0, or
   capped (e.g. 0.8) until an explain-back pass? Decide with real data.
2. **Decay** — should an old session's understood segments decay over time (you forget)?
   Relevant only once trending exists.
3. **Multi-human / team rollups** — the ledger is per-machine; team aggregation is a later,
   opt-in concern.
4. **Thinking-token engagement** — expanding a thinking block is a comprehension signal we
   don't yet surface in the cockpit; fold it into the seen set when the thinking lane gains
   expansion.

## Sources

- [Addy Osmani — *Comprehension Debt: the hidden cost of AI-generated code*](https://addyosmani.com/blog/comprehension-debt/)
- [O'Reilly Radar — *Comprehension Debt*](https://www.oreilly.com/radar/comprehension-debt-the-hidden-cost-of-ai-generated-code/)
- [Anthropic — *How AI assistance impacts the formation of coding skills*](https://www.anthropic.com/research/AI-assistance-coding-skills)
- [InfoQ — *AI Coding Assistance Reduces Developer Skill Mastery by 17%*](https://www.infoq.com/news/2026/02/ai-coding-skill-formation/)
- [arXiv 2512.08942 — *Comprehension Debt in Resource-Constrained Indie Teams*](https://arxiv.org/pdf/2512.08942)
- [CodePulse — *Review Coverage*](https://codepulsehq.com/guides/review-coverage-guide/)
- [GitHub Changelog — *Code coverage in pull requests*](https://github.blog/changelog/2026-05-26-code-coverage-in-pull-requests-is-now-in-public-preview/)
