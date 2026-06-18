"""Phase 2: thinking summarizer + collapsible viewer."""

from __future__ import annotations

import asyncio
from datetime import datetime

from understudy.config import Config, ProviderConfig
from understudy.events import Event, Kind
from understudy.store import EventStore
from understudy.summary.thinking import ThinkingSummarizer
from understudy.tui.app import FeedScreen, UnderstudyApp
from understudy.tui.thinking import ThinkingBlock, ThinkingScreen, _title

NO_MODEL = Config(provider=ProviderConfig(kind="none"), summary_enabled=False, configured=True)
TS = datetime(2026, 6, 17, 12, 0, 0)


class FakeProvider:
    kind = "fake"
    model = "fake"

    def __init__(self, chunks=("ok",)):
        self.chunks = chunks

    async def stream_chat(self, messages):
        for c in self.chunks:
            yield c

    async def list_models(self):
        return ["fake"]

    async def check(self):
        return "OK"

    async def aclose(self):
        pass


# ---- summarizer ----------------------------------------------------------- #


def test_summarizer_strips_think_tags():
    provider = FakeProvider(["<think>noise</think>considered reading first, then editing"])
    out = asyncio.run(ThinkingSummarizer(provider).summarize("some long reasoning"))
    assert out == "considered reading first, then editing"


def test_summarizer_skips_empty():
    out = asyncio.run(ThinkingSummarizer(FakeProvider()).summarize("   "))
    assert out == ""


# ---- title rendering / degradation ---------------------------------------- #


def test_title_variants():
    assert "chose rename" in _title(Event(Kind.THINKING, TS, payload={"summary": "chose rename"}))
    assert "✲" in _title(Event(Kind.THINKING, TS, payload={"text": "long reasoning text"}))
    assert "not exposed" in _title(Event(Kind.THINKING, TS, payload={"text": ""}))


# ---- the viewer ----------------------------------------------------------- #


def _store_with_two_blocks() -> EventStore:
    store = EventStore()
    store.add(Event(Kind.THINKING, TS, payload={"text": "Maybe move foo.py, but renaming is cleaner."}))
    store.add(Event(Kind.THINKING, TS, payload={"text": ""}))  # redacted / not exposed
    return store


def test_thinking_screen_lists_and_summarizes():
    async def run():
        app = UnderstudyApp(config=NO_MODEL)
        async with app.run_test() as pilot:
            await pilot.pause()
            app.push_screen(ThinkingScreen(_store_with_two_blocks()))
            await pilot.pause()
            screen = app.screen
            blocks = list(screen.query(ThinkingBlock))

            empty = next(b for b in blocks if not b.has_text)
            texty = next(b for b in blocks if b.has_text)
            degraded_title = empty.title

            app.provider = FakeProvider(["considered ", "renaming"])
            await screen.action_summarize_all()
            await pilot.pause(0.2)
            return len(blocks), str(texty.title), str(empty.title), str(degraded_title)

    count, texty_title, empty_title, degraded_title = asyncio.run(run())
    assert count == 2
    assert "considered renaming" in texty_title  # summarized
    assert "not exposed" in empty_title  # empty block left as-is
    assert empty_title == degraded_title


def test_feed_opens_thinking_screen():
    from pathlib import Path

    fixture = Path(__file__).parent / "fixtures" / "sample_session.jsonl"

    async def run():
        app = UnderstudyApp(session_path=fixture, config=NO_MODEL)
        async with app.run_test() as pilot:
            await pilot.pause(0.4)
            assert isinstance(app.screen, FeedScreen)
            app.screen.action_thinking()
            await pilot.pause()
            return isinstance(app.screen, ThinkingScreen)

    assert asyncio.run(run()) is True
