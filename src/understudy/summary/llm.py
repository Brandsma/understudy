"""Tier-2 summary: a debounced LLM "what & why", layered over the deterministic line."""

from __future__ import annotations

from understudy.context import render_activity
from understudy.models._filters import strip_think
from understudy.models.base import ModelProvider, complete
from understudy.store import EventStore

_SYSTEM = "You summarize another coding agent's recent activity for someone watching over its shoulder."
_PROMPT = (
    "In ONE or TWO short sentences, say what the agent is doing right now and why. "
    "Be specific (name files and tools). No preamble, no bullet points, no markdown."
)


class LiveSummarizer:
    def __init__(self, provider: ModelProvider, store: EventStore) -> None:
        self.provider = provider
        self.store = store

    async def summarize(self) -> str:
        activity = render_activity(self.store, max_events=80, max_chars=6000)
        messages = [
            {"role": "system", "content": _SYSTEM},
            {"role": "user", "content": f"{_PROMPT}\n\n=== ACTIVITY ===\n{activity}"},
        ]
        return strip_think(await complete(self.provider, messages))
