"""Tier-1 summary: instant, free, recomputed on every event from store indices."""

from __future__ import annotations

from understudy.store import EventStore


def summary_line(store: EventStore) -> str:
    parts = [store.last_action]
    if store.files_touched:
        parts.append(f"{len(store.files_touched)} file(s) touched")
    if store.last_tool:
        name, ok = store.last_tool
        parts.append(f"last: {name} {'✓' if ok else '✗'}")
    if store.error_count:
        parts.append(f"{store.error_count} error(s)")
    line = "   ·   ".join(parts)

    if store.turn_tool_counts:
        hist = "  ".join(f"{name}×{count}" for name, count in store.turn_tool_counts.most_common(6))
        line += f"\ntools this turn: {hist}"
    return line
