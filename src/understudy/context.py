"""Render the event store into a compact transcript for the understudy model."""

from __future__ import annotations

from pathlib import Path

from understudy.events import Event, Kind
from understudy.store import EventStore


def render_activity(store: EventStore, *, max_events: int = 160, max_chars: int = 10000) -> str:
    """A token-frugal, most-recent-last view of the agent's activity."""
    lines = [_serialize(ev) for ev in store.events[-max_events:]]
    text = "\n".join(line for line in lines if line)
    if len(text) > max_chars:
        text = "…(earlier activity elided)…\n" + text[-max_chars:]
    return text or "(no activity yet)"


def _serialize(ev: Event) -> str:
    p = ev.payload
    t = ev.ts.strftime("%H:%M:%S")
    tag = " [subagent]" if ev.is_sidechain else ""
    match ev.kind:
        case Kind.USER_PROMPT:
            return f"{t} USER{tag}: {_clip(p.get('text', ''), 400)}"
        case Kind.ASSISTANT_TEXT:
            return f"{t} ASSISTANT{tag}: {_clip(p.get('text', ''), 400)}"
        case Kind.THINKING:
            text = p.get("text", "")
            body = _clip(text, 400) if text.strip() else "(not exposed)"
            return f"{t} THINKING{tag}: {body}"
        case Kind.TOOL_CALL:
            return f"{t} TOOL→ {p.get('name', '')}({_clip(_args(p), 160)}){tag}"
        case Kind.TOOL_RESULT:
            status = "ok" if p.get("ok", True) else "ERROR"
            return f"{t} TOOL← {p.get('name', '')} {status}: {_clip(p.get('summary', ''), 160)}{tag}"
        case Kind.FILE_EDIT:
            verb = "CREATE" if p.get("created") else "EDIT"
            name = Path(p.get("path", "")).name
            return f"{t} {verb} {name} +{p.get('added', 0)}-{p.get('removed', 0)}{tag}"
        case Kind.SESSION_START:
            return f"{t} SESSION cwd={p.get('cwd', '')}"
        case _:
            return ""


def _args(p: dict) -> str:
    inp = p.get("input") or {}
    for key in ("command", "file_path", "path", "pattern", "query", "url"):
        if key in inp and inp[key]:
            return f"{key}={inp[key]}"
    return ""


def _clip(s: str, n: int) -> str:
    s = " ".join(str(s).split())
    return s if len(s) <= n else s[: n - 1] + "…"
