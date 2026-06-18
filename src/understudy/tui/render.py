"""Render normalized events into Rich renderables for the feed and detail panes."""

from __future__ import annotations

import json
from pathlib import Path

from rich.console import Group, RenderableType
from rich.text import Text

from understudy.events import Event, Kind

# icon, style, short label
_KIND = {
    Kind.USER_PROMPT: ("▷", "bold cyan"),
    Kind.ASSISTANT_TEXT: ("✎", "white"),
    Kind.THINKING: ("✲", "magenta"),
    Kind.TOOL_CALL: ("→", "yellow"),
    Kind.TOOL_RESULT: ("←", "green"),
    Kind.FILE_EDIT: ("±", "bold blue"),
    Kind.SESSION_START: ("●", "dim"),
    Kind.TURN_END: ("■", "dim"),
    Kind.NOTIFICATION: ("!", "yellow"),
}


def row_text(ev: Event) -> Text:
    """One compact, selectable line for the feed."""
    icon, style = _KIND.get(ev.kind, ("•", "white"))
    t = Text(no_wrap=True, overflow="ellipsis")
    t.append(ev.ts.strftime("%H:%M:%S "), style="dim")
    if ev.is_sidechain:
        t.append("┊ ", style="dim")
    t.append(f"{icon} ", style=style)
    t.append(_row_body(ev))
    return t


def _row_body(ev: Event) -> str:
    p = ev.payload
    match ev.kind:
        case Kind.TOOL_CALL:
            hint = _arg_hint(p)
            return f"{p.get('name', 'tool')}  {hint}".rstrip()
        case Kind.TOOL_RESULT:
            mark = "✓" if p.get("ok", True) else "✗"
            return f"{p.get('name', 'tool')} {mark}  {p.get('summary', '')}".rstrip()
        case Kind.FILE_EDIT:
            tag = " (new)" if p.get("created") else ""
            return f"{Path(p.get('path', '')).name}  +{p.get('added', 0)} -{p.get('removed', 0)}{tag}"
        case Kind.USER_PROMPT | Kind.ASSISTANT_TEXT:
            return _one_line(p.get("text", ""), 110)
        case Kind.THINKING:
            summary = p.get("summary")
            if summary:
                return _one_line(summary, 110)
            text = p.get("text", "")
            return _one_line(text, 110) if text.strip() else "(thinking — content not exposed)"
        case Kind.SESSION_START:
            return f"session {p.get('session_id', '')[:8]} · {p.get('cwd', '')}"
        case _:
            return str(ev.kind)


def _arg_hint(p: dict) -> str:
    inp = p.get("input") or {}
    for key in ("command", "file_path", "path", "pattern", "query", "url", "description"):
        if key in inp and inp[key]:
            return _one_line(str(inp[key]), 80)
    return ""


def detail_view(ev: Event) -> RenderableType:
    """Full detail for the right-hand pane when a feed row is highlighted."""
    p = ev.payload
    match ev.kind:
        case Kind.FILE_EDIT:
            return _diff_view(ev)
        case Kind.TOOL_CALL:
            head = Text(f"→ {p.get('name', 'tool')}", style="bold yellow")
            body = json.dumps(p.get("input", {}), indent=2, default=str)
            return Group(head, Text(""), Text(body))
        case Kind.TOOL_RESULT:
            ok = p.get("ok", True)
            head = Text(
                f"← {p.get('name', 'tool')} {'✓' if ok else '✗'}",
                style="bold green" if ok else "bold red",
            )
            return Group(head, Text(""), Text(p.get("detail", "") or p.get("summary", "")))
        case Kind.THINKING:
            text = p.get("text", "")
            if not text.strip():
                return Text(
                    "Thinking occurred, but its content is not exposed in this session.\n"
                    "(Availability varies by model/version/config — see the integration doc.)",
                    style="italic dim",
                )
            return Group(Text("✲ thinking", style="bold magenta"), Text(""), Text(text, style="magenta"))
        case Kind.USER_PROMPT | Kind.ASSISTANT_TEXT:
            return Text(p.get("text", ""))
        case Kind.SESSION_START:
            return Text(json.dumps(p, indent=2, default=str), style="dim")
        case _:
            return Text(json.dumps(p, indent=2, default=str), style="dim")


def _diff_view(ev: Event) -> Text:
    p = ev.payload
    out = Text()
    out.append(f"{p.get('path', '')}\n", style="bold")
    if p.get("created"):
        out.append("new file   ", style="green")
    out.append(f"+{p.get('added', 0)} ", style="green")
    out.append(f"-{p.get('removed', 0)}\n\n", style="red")
    for hunk in p.get("hunks", []):
        out.append(
            f"@@ -{hunk.old_start},{hunk.old_lines} +{hunk.new_start},{hunk.new_lines} @@\n",
            style="cyan",
        )
        for ln in hunk.lines:
            if ln.startswith("+"):
                out.append(ln + "\n", style="green")
            elif ln.startswith("-"):
                out.append(ln + "\n", style="red")
            else:
                out.append(ln + "\n", style="dim")
        out.append("\n")
    return out


def _one_line(s: str, limit: int) -> str:
    s = " ".join(s.split())
    return s if len(s) <= limit else s[: limit - 1] + "…"
