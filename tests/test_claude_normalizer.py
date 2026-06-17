"""Golden-ish tests pinning the Claude Code record -> Event mapping."""

from __future__ import annotations

import asyncio
from pathlib import Path

from understudy.events import Kind
from understudy.sources.claude_code import ClaudeCodeSource, read_session_meta

FIXTURE = Path(__file__).parent / "fixtures" / "sample_session.jsonl"
WRITE_FIXTURE = Path(__file__).parent / "fixtures" / "write_create.jsonl"


def _backfill(fixture=FIXTURE):
    return asyncio.run(ClaudeCodeSource(fixture).backfill())


def test_event_kinds_in_order():
    kinds = [e.kind for e in _backfill()]
    assert kinds == [
        Kind.SESSION_START,
        Kind.USER_PROMPT,
        Kind.THINKING,
        Kind.TOOL_CALL,      # Bash
        Kind.TOOL_RESULT,    # Bash
        Kind.TOOL_CALL,      # Edit
        Kind.TOOL_RESULT,    # Edit
        Kind.FILE_EDIT,      # derived from structuredPatch
        Kind.ASSISTANT_TEXT,
    ]


def test_tool_results_are_named_via_tool_use_map():
    events = _backfill()
    results = [e for e in events if e.kind == Kind.TOOL_RESULT]
    assert [r.payload["name"] for r in results] == ["Bash", "Edit"]
    assert all(r.payload["ok"] for r in results)


def test_file_edit_has_structured_diff():
    events = _backfill()
    edit = next(e for e in events if e.kind == Kind.FILE_EDIT)
    assert edit.payload["path"].endswith("config.json")
    assert edit.payload["added"] == 1
    assert edit.payload["removed"] == 1
    assert edit.payload["hunks"][0].lines[1].startswith("-")


def test_turns_grouped_under_user_prompt():
    events = _backfill()
    prompt = next(e for e in events if e.kind == Kind.USER_PROMPT)
    tool_calls = [e for e in events if e.kind == Kind.TOOL_CALL]
    assert all(tc.turn_id == prompt.turn_id for tc in tool_calls)


def test_session_meta():
    info = read_session_meta(FIXTURE)
    assert info is not None
    assert info.session_id == "s1"
    assert info.cwd == "/Users/dev/proj"
    assert info.git_branch == "main"


def test_meta_skips_leading_queue_operations():
    # cwd/branch live on the 3rd record; the first two are queue-operations.
    info = read_session_meta(WRITE_FIXTURE)
    assert info is not None
    assert info.cwd == "/Users/dev/proj2"
    assert info.git_branch == "feature"


def test_write_create_emits_all_added_diff():
    events = _backfill(WRITE_FIXTURE)
    edit = next(e for e in events if e.kind == Kind.FILE_EDIT)
    assert edit.payload["created"] is True
    assert edit.payload["added"] == 3  # "# Notes", "", "first"
    assert edit.payload["removed"] == 0
    result = next(e for e in events if e.kind == Kind.TOOL_RESULT)
    assert result.payload["name"] == "Write"
    assert "created" in result.payload["summary"]
