"""Understudy's comprehension conversation — fully decoupled from the observed agent."""

from __future__ import annotations

from typing import AsyncIterator

from understudy.context import render_activity
from understudy.models._filters import ThinkFilter, strip_think
from understudy.models.base import ChatMessage, ModelProvider
from understudy.store import EventStore

SYSTEM = (
    "You are Understudy, a read-only observer of ANOTHER coding "
    "agent's live session. You cannot act, edit files, run tools, or message that agent — "
    "you only explain what it is doing and why, grounded in the activity stream below. "
    "Be concise and concrete: name the specific files, tools, and steps. If the answer "
    "isn't in the stream, say so rather than guessing."
)


class ChatSession:
    def __init__(self, provider: ModelProvider, store: EventStore, *, max_history: int = 12) -> None:
        self.provider = provider
        self.store = store
        self.max_history = max_history
        self.history: list[ChatMessage] = []

    def _messages(self, question: str) -> list[ChatMessage]:
        # The activity stream is rebuilt every turn so answers reflect the agent's
        # latest state; prior Q&A is carried in `history`.
        activity = render_activity(self.store)
        system = f"{SYSTEM}\n\n=== AGENT ACTIVITY (most recent last) ===\n{activity}"
        return [{"role": "system", "content": system}, *self.history, {"role": "user", "content": question}]

    async def ask(self, question: str) -> AsyncIterator[str]:
        raw: list[str] = []
        think = ThinkFilter()
        async for delta in self.provider.stream_chat(self._messages(question)):
            raw.append(delta)
            visible = think.feed(delta)
            if visible:
                yield visible
        tail = think.flush()
        if tail:
            yield tail
        answer = strip_think("".join(raw)) or "".join(raw)
        self.history.append({"role": "user", "content": question})
        self.history.append({"role": "assistant", "content": answer})
        if len(self.history) > self.max_history:
            self.history = self.history[-self.max_history :]
