"""Normalized event vocabulary shared by every source adapter.

The whole app above the adapter layer speaks only these types — see
docs/architecture.md.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from enum import StrEnum
from typing import Any


class Kind(StrEnum):
    SESSION_START = "session_start"
    USER_PROMPT = "user_prompt"
    ASSISTANT_TEXT = "assistant_text"
    THINKING = "thinking"
    TOOL_CALL = "tool_call"
    TOOL_RESULT = "tool_result"
    FILE_EDIT = "file_edit"  # derived from Edit/Write tool results
    TURN_END = "turn_end"
    NOTIFICATION = "notification"


@dataclass(slots=True)
class Hunk:
    """Mirrors Claude Code's `structuredPatch` entries.

    Each entry in `lines` is already prefixed with ' ' (context), '+' (added),
    or '-' (removed), so the diff viewer renders it directly — no diffing.
    """

    old_start: int
    old_lines: int
    new_start: int
    new_lines: int
    lines: list[str]


@dataclass(slots=True)
class Event:
    kind: Kind
    ts: datetime
    source: str = "claude-code"
    turn_id: str | None = None
    is_sidechain: bool = False
    payload: dict[str, Any] = field(default_factory=dict)
    raw_ref: str | None = None  # uuid/offset back into the source for drill-down


@dataclass(slots=True)
class SourceInfo:
    """Human-readable identity of an attached source."""

    tool: str
    session_id: str
    cwd: str
    title: str | None = None
