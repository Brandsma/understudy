"""Distill a chain-of-thought block into a one-line "thought pattern"."""

from __future__ import annotations

from understudy.models._filters import strip_think
from understudy.models.base import ModelProvider, complete

_SYSTEM = "You distill another coding agent's chain-of-thought into a short 'thought pattern' for an observer."
_PROMPT = (
    "Summarize the reasoning below in ONE short sentence naming the key decision or "
    "self-correction — e.g. \"considered moving X to Y, then chose to rename instead\". "
    "No preamble, no markdown, no quotes."
)


class ThinkingSummarizer:
    def __init__(self, provider: ModelProvider) -> None:
        self.provider = provider

    async def summarize(self, text: str) -> str:
        text = text.strip()
        if not text:
            return ""
        messages = [
            {"role": "system", "content": _SYSTEM},
            {"role": "user", "content": f"{_PROMPT}\n\n=== REASONING ===\n{_clip(text, 4000)}"},
        ]
        raw = strip_think(await complete(self.provider, messages))
        return _tidy_line(raw)


def _tidy_line(raw: str) -> str:
    """Collapse a possibly-rambly model reply to one clean line for a collapsible title."""
    for line in raw.splitlines():
        line = line.strip().lstrip("-•* ").strip('"')
        if line:
            return _clip(line, 160)
    return ""


def _clip(s: str, n: int) -> str:
    return s if len(s) <= n else s[: n - 1] + "…"
