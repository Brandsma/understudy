"""The `Source` contract — the only thing the core depends on.

A source produces a bounded backfill of recent history, then an async stream of
new normalized events until the session ends.
"""

from __future__ import annotations

from typing import AsyncIterator, Protocol, runtime_checkable

from understudy.events import Event, SourceInfo


@runtime_checkable
class Source(Protocol):
    def describe(self) -> SourceInfo:
        """Human-readable identity: tool, session id, cwd, title if known."""
        ...

    async def backfill(self) -> list[Event]:
        """Recent history so the panel isn't empty on attach (bounded)."""
        ...

    def stream(self) -> AsyncIterator[Event]:
        """Yield normalized events as they arrive, until stopped."""
        ...
