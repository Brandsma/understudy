"""Regression tests for quit + setup-wizard keyboard handling."""

from __future__ import annotations

import asyncio

from textual.widgets import Input

from understudy.config import Config, ProviderConfig
from understudy.tui.app import SessionPicker, UnderstudyApp
from understudy.tui.setup import SetupScreen

NO_MODEL = Config(provider=ProviderConfig(kind="none"), summary_enabled=False, configured=True)


def test_q_quits_from_picker():
    """`q` must resolve to app.quit (bare 'quit' silently did nothing)."""

    async def run() -> bool:
        app = UnderstudyApp(config=NO_MODEL)
        async with app.run_test() as pilot:
            await pilot.pause()
            assert isinstance(app.screen, SessionPicker)
            await pilot.press("q")
            await pilot.pause()
            return app._exit

    assert asyncio.run(run()) is True


def test_ctrl_q_quits_from_setup():
    """The setup screen has no `q`; the global Ctrl+Q must still quit."""

    async def run() -> bool:
        app = UnderstudyApp(config=Config(configured=False))
        async with app.run_test() as pilot:
            await pilot.pause()
            assert isinstance(app.screen, SetupScreen)
            await pilot.press("ctrl+q")
            await pilot.pause()
            return app._exit

    assert asyncio.run(run()) is True


def test_enter_in_setup_field_saves_and_continues(tmp_path, monkeypatch):
    monkeypatch.setenv("UNDERSTUDY_CONFIG", str(tmp_path / "config.json"))

    async def run() -> str:
        app = UnderstudyApp(config=Config(configured=False))
        async with app.run_test() as pilot:
            await pilot.pause()
            field = app.screen.query_one("#model", Input)
            field.focus()
            field.value = "llama3.1"
            await pilot.press("enter")
            await pilot.pause()
            return type(app.screen).__name__

    assert asyncio.run(run()) == "SessionPicker"
