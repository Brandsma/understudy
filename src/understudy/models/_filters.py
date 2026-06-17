"""Strip <think>…</think> reasoning blocks emitted by local reasoning models
(qwen3, deepseek-r1, etc.) so the understudy answer stays clean."""

from __future__ import annotations

import re

_THINK_RE = re.compile(r"<think>.*?</think>", re.DOTALL)

_OPEN = "<think>"
_CLOSE = "</think>"


def strip_think(text: str) -> str:
    """Remove complete <think>…</think> blocks from a finished string."""
    return _THINK_RE.sub("", text).strip()


class ThinkFilter:
    """Streaming filter: feed deltas, get back only the non-think visible text.

    Handles tags split across chunk boundaries by holding back a tail that could
    be the start of an opening/closing tag.
    """

    def __init__(self) -> None:
        self._buf = ""
        self._in_think = False

    def feed(self, delta: str) -> str:
        self._buf += delta
        out: list[str] = []
        progress = True
        while progress:
            progress = False
            if not self._in_think:
                idx = self._buf.find(_OPEN)
                if idx != -1:
                    out.append(self._buf[:idx])
                    self._buf = self._buf[idx + len(_OPEN) :]
                    self._in_think = True
                    progress = True
                else:
                    safe = self._safe_len(self._buf, _OPEN)
                    out.append(self._buf[:safe])
                    self._buf = self._buf[safe:]
            else:
                idx = self._buf.find(_CLOSE)
                if idx != -1:
                    self._buf = self._buf[idx + len(_CLOSE) :]
                    self._in_think = False
                    progress = True
                else:
                    safe = self._safe_len(self._buf, _CLOSE)
                    self._buf = self._buf[safe:]  # drop reasoning text
        return "".join(out)

    def flush(self) -> str:
        out = self._buf if not self._in_think else ""
        self._buf = ""
        return out

    @staticmethod
    def _safe_len(buf: str, tag: str) -> int:
        """Leading chars that cannot be the start of `tag` (keep the rest as a tail)."""
        for keep in range(min(len(buf), len(tag) - 1), 0, -1):
            if buf[-keep:] == tag[:keep]:
                return len(buf) - keep
        return len(buf)
