"""Claude Code source adapter.

Discovers sessions, tails the append-only JSONL transcript, and normalizes each
record into the shared `Event` vocabulary. Verified against real transcripts —
see docs/claude-code-integration.md.
"""

from __future__ import annotations

import asyncio
import json
import os
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from typing import Any, AsyncIterator

from understudy.events import Event, Hunk, Kind, SourceInfo

# Cap synthesized "new file" diffs so a huge Write can't flood the detail pane.
MAX_CREATE_LINES = 400

# --------------------------------------------------------------------------- #
# Locations
# --------------------------------------------------------------------------- #


def config_dir() -> Path:
    return Path(os.environ.get("CLAUDE_CONFIG_DIR", str(Path.home() / ".claude")))


def projects_dir() -> Path:
    return config_dir() / "projects"


# --------------------------------------------------------------------------- #
# Discovery
# --------------------------------------------------------------------------- #


@dataclass(slots=True)
class SessionInfo:
    path: Path
    session_id: str
    cwd: str
    git_branch: str
    modified: datetime
    size: int
    summary: str  # ai-title, last prompt, or last user text — whichever we find


def discover_sessions(cwd_filter: str | None = None) -> list[SessionInfo]:
    """All Claude Code sessions, newest first. Optionally filter by cwd."""
    root = projects_dir()
    if not root.is_dir():
        return []
    sessions: list[SessionInfo] = []
    for path in root.glob("*/*.jsonl"):
        info = read_session_meta(path)
        if info is None:
            continue
        if cwd_filter and info.cwd != cwd_filter:
            continue
        sessions.append(info)
    sessions.sort(key=lambda s: s.modified, reverse=True)
    return sessions


def read_session_meta(path: Path) -> SessionInfo | None:
    try:
        stat = path.stat()
        if stat.st_size == 0:
            return None
        meta = _head_meta(path)
    except OSError:
        return None
    if not meta:
        return None
    return SessionInfo(
        path=path,
        session_id=meta.get("sessionId", path.stem),
        cwd=meta.get("cwd", ""),
        git_branch=meta.get("gitBranch", "") or "",
        modified=datetime.fromtimestamp(stat.st_mtime),
        size=stat.st_size,
        summary=_session_summary(path),
    )


def _head_meta(path: Path, max_lines: int = 20) -> dict[str, Any]:
    """Merge identity fields from the first records.

    The first lines can be `queue-operation` / `summary` records that lack
    `cwd`/`gitBranch`, so scan a few and take each field from the first record
    that carries it.
    """
    meta: dict[str, Any] = {}
    with path.open("r", encoding="utf-8", errors="replace") as fh:
        for i, line in enumerate(fh):
            if i >= max_lines:
                break
            line = line.strip()
            if not line:
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            for key in ("sessionId", "cwd", "gitBranch", "version"):
                if not meta.get(key) and rec.get(key):
                    meta[key] = rec[key]
            if meta.get("cwd") and meta.get("sessionId"):
                break
    return meta


def _read_tail(path: Path, n: int) -> str:
    with path.open("rb") as fh:
        fh.seek(0, os.SEEK_END)
        size = fh.tell()
        fh.seek(max(0, size - n))
        return fh.read().decode("utf-8", errors="replace")


def _session_summary(path: Path, tail_bytes: int = 65536) -> str:
    """Cheap, best-effort one-liner from the file tail for the picker."""
    title = last_prompt = last_user = None
    data = _read_tail(path, tail_bytes)
    for line in data.splitlines()[1:]:  # skip possibly-partial first line
        line = line.strip()
        if not line:
            continue
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            continue
        rtype = rec.get("type")
        if rtype == "ai-title":
            title = rec.get("aiTitle") or rec.get("title") or title
        elif rtype == "last-prompt":
            last_prompt = rec.get("lastPrompt") or rec.get("prompt") or last_prompt
        elif rtype == "user":
            text = _user_text(rec)
            if text:
                last_user = text
    return _one_line(title or last_prompt or last_user or "", 90)


# --------------------------------------------------------------------------- #
# Source
# --------------------------------------------------------------------------- #


class ClaudeCodeSource:
    def __init__(
        self,
        path: Path,
        *,
        backfill_limit: int = 400,
        poll_interval: float = 0.25,
    ) -> None:
        self.path = Path(path)
        self.backfill_limit = backfill_limit
        self.poll_interval = poll_interval
        self._offset = 0
        self._buf = ""
        self._tool_names: dict[str, str] = {}  # tool_use_id -> tool name
        self._current_turn: str | None = None
        self._started = False
        self._stop = asyncio.Event()

    # -- Source protocol ---------------------------------------------------- #

    def describe(self) -> SourceInfo:
        info = read_session_meta(self.path)
        if info:
            return SourceInfo("claude-code", info.session_id, info.cwd, info.summary or None)
        return SourceInfo("claude-code", self.path.stem, "", None)

    async def backfill(self) -> list[Event]:
        self._offset = 0
        self._buf = ""
        events: list[Event] = []
        for line in self._read_new_lines():
            events.extend(self._normalize_line(line))
        return events[-self.backfill_limit :]

    async def stream(self) -> AsyncIterator[Event]:
        while not self._stop.is_set():
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=self.poll_interval)
            except asyncio.TimeoutError:
                pass
            for line in self._read_new_lines():
                for ev in self._normalize_line(line):
                    yield ev

    def stop(self) -> None:
        self._stop.set()

    # -- Tailing ------------------------------------------------------------ #

    def _read_new_lines(self) -> list[str]:
        """Read appended bytes since the last offset; return complete lines."""
        try:
            with self.path.open("rb") as fh:
                fh.seek(0, os.SEEK_END)
                size = fh.tell()
                if size < self._offset:  # truncated / rotated
                    self._offset = 0
                    self._buf = ""
                if size == self._offset:
                    return []
                fh.seek(self._offset)
                chunk = fh.read(size - self._offset)
                self._offset = size
        except FileNotFoundError:
            return []
        self._buf += chunk.decode("utf-8", errors="replace")
        parts = self._buf.split("\n")
        self._buf = parts.pop()  # trailing partial line (or "")
        return [p for p in parts if p.strip()]

    # -- Normalization ------------------------------------------------------ #

    def _normalize_line(self, line: str) -> list[Event]:
        try:
            rec = json.loads(line)
        except json.JSONDecodeError:
            return []
        rtype = rec.get("type")
        if rtype == "assistant":
            return self._from_assistant(rec)
        if rtype == "user":
            return self._from_user(rec)
        return []

    def _from_assistant(self, rec: dict) -> list[Event]:
        ts = _ts(rec)
        side = bool(rec.get("isSidechain"))
        out = self._maybe_session_start(rec, ts)
        content = (rec.get("message") or {}).get("content")
        if not isinstance(content, list):
            return out
        for block in content:
            if not isinstance(block, dict):
                continue
            bt = block.get("type")
            if bt == "text":
                text = block.get("text", "")
                if text.strip():
                    out.append(self._ev(Kind.ASSISTANT_TEXT, ts, side, {"text": text}, rec))
            elif bt in ("thinking", "redacted_thinking"):
                text = block.get("thinking", "") if bt == "thinking" else ""
                out.append(self._ev(Kind.THINKING, ts, side, {"text": text}, rec))
            elif bt == "tool_use":
                tid = block.get("id", "")
                name = block.get("name", "tool")
                self._tool_names[tid] = name
                out.append(
                    self._ev(
                        Kind.TOOL_CALL,
                        ts,
                        side,
                        {"id": tid, "name": name, "input": block.get("input", {})},
                        rec,
                    )
                )
        return out

    def _from_user(self, rec: dict) -> list[Event]:
        ts = _ts(rec)
        side = bool(rec.get("isSidechain"))
        out = self._maybe_session_start(rec, ts)
        if rec.get("isMeta"):
            return out

        content = (rec.get("message") or {}).get("content")
        result_block = None
        if isinstance(content, list):
            for b in content:
                if isinstance(b, dict) and b.get("type") == "tool_result":
                    result_block = b
                    break

        if result_block is not None:
            out.extend(self._from_tool_result(rec, result_block, ts, side))
            return out

        text = _user_text(rec)
        if text and text.strip():
            self._current_turn = rec.get("uuid") or self._current_turn
            out.append(self._ev(Kind.USER_PROMPT, ts, side, {"text": text}, rec))
        return out

    def _from_tool_result(self, rec: dict, block: dict, ts: datetime, side: bool) -> list[Event]:
        tid = block.get("tool_use_id", "")
        name = self._tool_names.get(tid, "tool")
        ok = not bool(block.get("is_error"))
        tur = rec.get("toolUseResult")
        summary, detail = _summarize_result(name, tur, block, ok)
        out = [
            self._ev(
                Kind.TOOL_RESULT,
                ts,
                side,
                {"id": tid, "name": name, "ok": ok, "summary": summary, "detail": detail},
                rec,
            )
        ]
        if isinstance(tur, dict) and tur.get("filePath"):
            edit = self._file_edit(tur, ts, side, rec)
            if edit is not None:
                out.append(edit)
        return out

    def _file_edit(self, tur: dict, ts: datetime, side: bool, rec: dict) -> Event | None:
        """Build a FILE_EDIT from an Edit (structuredPatch) or a Write (content)."""
        patch = tur.get("structuredPatch")
        content = tur.get("content")
        created = tur.get("type") == "create"

        if isinstance(patch, list) and patch:
            hunks = [
                Hunk(
                    h.get("oldStart", 0),
                    h.get("oldLines", 0),
                    h.get("newStart", 0),
                    h.get("newLines", 0),
                    list(h.get("lines", [])),
                )
                for h in patch
                if isinstance(h, dict)
            ]
            added = sum(1 for h in hunks for ln in h.lines if ln.startswith("+"))
            removed = sum(1 for h in hunks for ln in h.lines if ln.startswith("-"))
        elif isinstance(content, str):
            # Write/create: no structured patch — synthesize an all-added diff.
            lines = content.split("\n")
            if lines and lines[-1] == "":
                lines.pop()
            shown = ["+" + ln for ln in lines[:MAX_CREATE_LINES]]
            if len(lines) > MAX_CREATE_LINES:
                shown.append(f"… (+{len(lines) - MAX_CREATE_LINES} more lines)")
            hunks = [Hunk(0, 0, 1, len(lines), shown)]
            added, removed, created = len(lines), 0, True
        else:
            return None

        return self._ev(
            Kind.FILE_EDIT,
            ts,
            side,
            {
                "path": tur["filePath"],
                "hunks": hunks,
                "added": added,
                "removed": removed,
                "original": tur.get("originalFile"),
                "created": created,
            },
            rec,
        )

    # -- helpers ------------------------------------------------------------ #

    def _ev(self, kind: Kind, ts: datetime, side: bool, payload: dict, rec: dict) -> Event:
        return Event(
            kind=kind,
            ts=ts,
            source="claude-code",
            turn_id=self._current_turn,
            is_sidechain=side,
            payload=payload,
            raw_ref=rec.get("uuid"),
        )

    def _maybe_session_start(self, rec: dict, ts: datetime) -> list[Event]:
        if self._started:
            return []
        self._started = True
        return [
            Event(
                Kind.SESSION_START,
                ts,
                "claude-code",
                None,
                False,
                {
                    "session_id": rec.get("sessionId", ""),
                    "cwd": rec.get("cwd", ""),
                    "version": rec.get("version", ""),
                },
                rec.get("uuid"),
            )
        ]


# --------------------------------------------------------------------------- #
# Module-level helpers
# --------------------------------------------------------------------------- #


def _ts(rec: dict) -> datetime:
    raw = rec.get("timestamp")
    if isinstance(raw, str):
        try:
            return datetime.fromisoformat(raw.replace("Z", "+00:00"))
        except ValueError:
            pass
    return datetime.now()


def _user_text(rec: dict) -> str | None:
    msg = rec.get("message")
    if not isinstance(msg, dict):
        return None
    content = msg.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        if any(isinstance(b, dict) and b.get("type") == "tool_result" for b in content):
            return None  # tool output, not a human prompt
        parts = [b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"]
        joined = " ".join(p for p in parts if p)
        return joined or None
    return None


def _content_to_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "\n".join(
            b.get("text", "") for b in content if isinstance(b, dict) and b.get("type") == "text"
        )
    return ""


def _summarize_result(name: str, tur: Any, block: dict, ok: bool) -> tuple[str, str]:
    if isinstance(tur, dict):
        if tur.get("filePath") and ("structuredPatch" in tur or "content" in tur):
            verb = "created" if tur.get("type") == "create" else "edited"
            preview = tur.get("newString") or tur.get("content") or ""
            return f"{verb} {Path(tur['filePath']).name}", _one_line(preview, 200)
        if "stdout" in tur or "stderr" in tur:
            stdout = tur.get("stdout", "") or ""
            stderr = tur.get("stderr", "") or ""
            body = stdout if stdout.strip() else stderr
            head = body.strip().splitlines()[0] if body.strip() else "(no output)"
            full = stdout + (("\n" + stderr) if stderr.strip() else "")
            return _one_line(head, 80), full.strip()
        if "results" in tur:
            count = tur.get("searchCount")
            if count is None and isinstance(tur.get("results"), list):
                count = len(tur["results"])
            detail = json.dumps(tur.get("results"), indent=2)[:4000] if tur.get("results") else ""
            return f"{count} results", detail
    detail = _content_to_text(block.get("content"))
    return _one_line(detail or ("ok" if ok else "error"), 80), detail


def _one_line(s: str, limit: int) -> str:
    s = " ".join(s.split())
    return s if len(s) <= limit else s[: limit - 1] + "…"


def _info_from_path(path: Path) -> SessionInfo:
    info = read_session_meta(path)
    if info is not None:
        return info
    stat = path.stat()
    return SessionInfo(
        path=path,
        session_id=path.stem,
        cwd="",
        git_branch="",
        modified=datetime.fromtimestamp(stat.st_mtime),
        size=stat.st_size,
        summary="",
    )


def resolve_session(token: str) -> Path | None:
    """Resolve a --session token: an existing path, or a UUID to search for."""
    p = Path(token)
    if p.is_file():
        return p
    for candidate in projects_dir().glob(f"*/{token}.jsonl"):
        return candidate
    return None
