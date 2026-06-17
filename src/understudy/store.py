"""In-memory event store with the derived indices the deterministic summary needs."""

from __future__ import annotations

from collections import Counter
from pathlib import Path

from understudy.events import Event, Kind


class EventStore:
    def __init__(self) -> None:
        self.events: list[Event] = []
        self.files_touched: Counter[str] = Counter()
        self.tool_counts: Counter[str] = Counter()
        self.turn_tool_counts: Counter[str] = Counter()
        self.last_tool: tuple[str, bool] | None = None
        self.last_action: str = "waiting…"
        self.current_turn: str | None = None
        self.error_count: int = 0

    def add(self, ev: Event) -> None:
        self.events.append(ev)
        payload = ev.payload
        if ev.kind == Kind.USER_PROMPT:
            self.current_turn = ev.turn_id
            self.turn_tool_counts = Counter()
            self.last_action = "reading user prompt"
        elif ev.kind == Kind.TOOL_CALL:
            name = payload.get("name", "tool")
            self.tool_counts[name] += 1
            self.turn_tool_counts[name] += 1
            self.last_action = f"calling {name}"
        elif ev.kind == Kind.TOOL_RESULT:
            ok = bool(payload.get("ok", True))
            self.last_tool = (payload.get("name", "tool"), ok)
            if not ok:
                self.error_count += 1
        elif ev.kind == Kind.FILE_EDIT:
            self.files_touched[payload.get("path", "")] += 1
            self.last_action = f"editing {Path(payload.get('path', '')).name}"
        elif ev.kind == Kind.THINKING:
            self.last_action = "thinking"
        elif ev.kind == Kind.ASSISTANT_TEXT:
            self.last_action = "responding"

    def bulk_add(self, events: list[Event]) -> None:
        for ev in events:
            self.add(ev)
