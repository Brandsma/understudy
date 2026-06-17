"""Tests for config, context rendering, chat session, and Copilot helpers."""

from __future__ import annotations

import asyncio
from datetime import datetime

from understudy.chat.session import ChatSession
from understudy.config import Config, ProviderConfig, load_config, save_config
from understudy.context import render_activity
from understudy.events import Event, Hunk, Kind
from understudy.models import build_provider
from understudy.models._filters import ThinkFilter, strip_think
from understudy.models.copilot import _base_from_token, find_copilot_oauth_token
from understudy.store import EventStore


# ---- config round-trip ---------------------------------------------------- #


def test_config_round_trip(tmp_path, monkeypatch):
    monkeypatch.setenv("UNDERSTUDY_CONFIG", str(tmp_path / "config.json"))
    assert load_config().configured is False  # missing file -> defaults
    cfg = Config(provider=ProviderConfig(kind="openai", model="gpt-4o-mini"), configured=True)
    save_config(cfg)
    loaded = load_config()
    assert loaded.configured is True
    assert loaded.provider.kind == "openai"
    assert loaded.provider.model == "gpt-4o-mini"


def test_build_provider_kinds(monkeypatch):
    assert build_provider(ProviderConfig(kind="none")) is None
    monkeypatch.setenv("OPENAI_API_KEY", "sk-test")
    prov = build_provider(ProviderConfig(kind="openai", base_url="https://x/v1", model="m"))
    assert prov.kind == "openai" and prov.api_key == "sk-test"
    asyncio.run(prov.aclose())


# ---- context rendering ---------------------------------------------------- #


def _store_with_events() -> EventStore:
    store = EventStore()
    ts = datetime(2026, 6, 17, 12, 0, 0)
    store.add(Event(Kind.USER_PROMPT, ts, payload={"text": "rename the title"}))
    store.add(Event(Kind.TOOL_CALL, ts, payload={"name": "Bash", "input": {"command": "ls"}}))
    store.add(Event(Kind.TOOL_RESULT, ts, payload={"name": "Bash", "ok": True, "summary": "3 files"}))
    store.add(
        Event(
            Kind.FILE_EDIT,
            ts,
            payload={"path": "/p/config.json", "hunks": [Hunk(1, 1, 1, 1, ["+x"])], "added": 1, "removed": 0},
        )
    )
    return store


def test_render_activity_includes_markers():
    text = render_activity(_store_with_events())
    assert "USER: rename the title" in text
    assert "TOOL→ Bash(command=ls)" in text
    assert "TOOL← Bash ok" in text
    assert "EDIT config.json +1-0" in text


# ---- chat session with a fake provider ------------------------------------ #


class FakeProvider:
    kind = "fake"
    model = "fake"

    def __init__(self, chunks):
        self.chunks = chunks
        self.seen_messages = None

    async def stream_chat(self, messages):
        self.seen_messages = messages
        for c in self.chunks:
            yield c

    async def list_models(self):
        return ["fake"]

    async def check(self):
        return "OK"

    async def aclose(self):
        pass


def test_chat_session_streams_and_records_history():
    provider = FakeProvider(["Hel", "lo"])
    session = ChatSession(provider, _store_with_events())

    async def run():
        out = "".join([c async for c in session.ask("what happened?")])
        return out

    assert asyncio.run(run()) == "Hello"
    # the agent activity must be embedded in the system message
    assert "AGENT ACTIVITY" in provider.seen_messages[0]["content"]
    # history captured the exchange
    assert session.history[-2:] == [
        {"role": "user", "content": "what happened?"},
        {"role": "assistant", "content": "Hello"},
    ]


# ---- copilot helpers ------------------------------------------------------ #


def test_copilot_base_from_token():
    token = "tid=abc;exp=123;proxy-ep=proxy.individual.githubcopilot.com;more=x"
    assert _base_from_token(token) == "https://api.individual.githubcopilot.com"
    assert _base_from_token("no-proxy-ep-here") == "https://api.githubcopilot.com"


def test_copilot_token_from_env(monkeypatch):
    monkeypatch.setenv("COPILOT_GITHUB_TOKEN", "gho_xyz")
    assert find_copilot_oauth_token() == "gho_xyz"


# ---- think-tag filtering (qwen3 / deepseek-r1 style) ---------------------- #


def test_strip_think_whole_string():
    assert strip_think("<think>reasoning here</think>The answer.") == "The answer."
    assert strip_think("no tags at all") == "no tags at all"


def test_think_filter_streaming_split_across_chunks():
    # tags arrive split across deltas; only visible text should come out
    deltas = ["<thi", "nk>secret rea", "soning</thi", "nk>The ", "final answer."]
    flt = ThinkFilter()
    out = "".join(flt.feed(d) for d in deltas) + flt.flush()
    assert out == "The final answer."


def test_think_filter_passes_plain_text():
    flt = ThinkFilter()
    out = "".join(flt.feed(d) for d in ["Hello ", "world"]) + flt.flush()
    assert out == "Hello world"
