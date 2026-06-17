"""Smoke tests: the feed composes and ingests, and the chat panel streams."""

from __future__ import annotations

import asyncio
from pathlib import Path

from understudy.config import Config, ProviderConfig
from understudy.tui.app import UnderstudyApp, FeedScreen
from understudy.tui.chat import ChatPanel

FIXTURE = Path(__file__).parent / "fixtures" / "sample_session.jsonl"

# A configured config with no model keeps the app off the network during tests.
NO_MODEL = Config(provider=ProviderConfig(kind="none"), summary_enabled=False, configured=True)


class FakeProvider:
    kind = "fake"
    model = "fake"

    async def stream_chat(self, messages):
        for chunk in ("Looking ", "at the ", "edits."):
            yield chunk

    async def list_models(self):
        return ["fake"]

    async def check(self):
        return "OK"

    async def aclose(self):
        pass


def test_feed_screen_runs():
    async def run() -> int:
        app = UnderstudyApp(session_path=FIXTURE, config=NO_MODEL)
        async with app.run_test() as pilot:
            await pilot.pause()
            await pilot.pause(0.4)  # let the ingest worker backfill
            feed = app.screen.query_one("#feed")
            count = len(feed.children)
        return count

    # 9 normalized events from the fixture should be rendered as feed rows.
    assert asyncio.run(run()) == 9


def test_chat_panel_streams_answer():
    async def run() -> str:
        app = UnderstudyApp(session_path=FIXTURE, config=NO_MODEL)
        async with app.run_test() as pilot:
            await pilot.pause(0.4)
            screen = app.screen
            assert isinstance(screen, FeedScreen)
            screen.set_provider(FakeProvider())  # inject a working provider
            screen.action_toggle_chat()  # show chat + focus input
            await pilot.pause()
            chat_input = app.screen.query_one("#chatinput")
            chat_input.value = "what is it doing?"
            await pilot.press("enter")
            await pilot.pause(0.4)  # let the answer stream
            log = app.screen.query_one(ChatPanel).query_one("#chatlog")
            rendered = "\n".join(str(child.render()) for child in log.children)
        return rendered

    out = asyncio.run(run())
    assert "what is it doing?" in out  # the user's question bubble
    assert "Looking at the edits." in out  # the streamed answer


def test_first_run_shows_setup():
    async def run() -> bool:
        from understudy.tui.setup import SetupScreen

        unconfigured = Config(configured=False)
        app = UnderstudyApp(session_path=FIXTURE, config=unconfigured)
        async with app.run_test() as pilot:
            await pilot.pause()
            return isinstance(app.screen, SetupScreen)

    assert asyncio.run(run()) is True
