"""Textual app: first-run setup → session picker → live feed + comprehension chat."""

from __future__ import annotations

import time
from datetime import datetime
from pathlib import Path

from rich.text import Text
from textual.app import App, ComposeResult
from textual.binding import Binding
from textual.containers import Horizontal, VerticalScroll
from textual.screen import Screen
from textual.widgets import DataTable, Footer, Header, Input, Label, ListItem, ListView, Static

from understudy.chat.session import ChatSession
from understudy.config import Config, load_config
from understudy.models import build_provider
from understudy.sources.claude_code import (
    ClaudeCodeSource,
    SessionInfo,
    _info_from_path,
    discover_sessions,
)
from understudy.store import EventStore
from understudy.summary.deterministic import summary_line
from understudy.summary.llm import LiveSummarizer
from understudy.tui.chat import ChatPanel
from understudy.tui.render import detail_view, row_text
from understudy.tui.setup import SetupScreen
from understudy.tui.thinking import ThinkingScreen

MAX_FEED_ITEMS = 800


# --------------------------------------------------------------------------- #
# Session picker
# --------------------------------------------------------------------------- #


class SessionPicker(Screen):
    BINDINGS = [
        Binding("r", "refresh", "Refresh"),
        Binding("f2", "app.settings", "Settings"),
        Binding("q", "quit", "Quit"),
    ]

    def __init__(self, cwd_filter: str | None = None) -> None:
        super().__init__()
        self.cwd_filter = cwd_filter
        self.sessions: list[SessionInfo] = []

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Static("Select a Claude Code session to observe (read-only).", classes="hint")
        yield DataTable(id="sessions", cursor_type="row", zebra_stripes=True)
        yield Footer()

    def on_mount(self) -> None:
        self.app.sub_title = "select a session"
        table = self.query_one("#sessions", DataTable)
        table.add_columns("When", "Project", "Branch", "Recent activity")
        self._load()
        table.focus()

    def _load(self) -> None:
        table = self.query_one("#sessions", DataTable)
        table.clear()
        self.sessions = discover_sessions(self.cwd_filter)
        for s in self.sessions:
            table.add_row(_ago(s.modified), _short_project(s.cwd), s.git_branch or "—", s.summary or "—")
        if not self.sessions:
            table.add_row("—", "(no Claude Code sessions found)", "", "")

    def action_refresh(self) -> None:
        self._load()

    def on_data_table_row_selected(self, event: DataTable.RowSelected) -> None:
        if not self.sessions:
            return
        row = self.query_one("#sessions", DataTable).cursor_row
        if row is None or row >= len(self.sessions):
            return
        self.app.push_screen(FeedScreen(self.sessions[row]))


# --------------------------------------------------------------------------- #
# Live feed + chat
# --------------------------------------------------------------------------- #


class EventItem(ListItem):
    def __init__(self, ev) -> None:
        super().__init__(Label(row_text(ev)))
        self.ev = ev


class FeedScreen(Screen):
    BINDINGS = [
        Binding("escape", "back", "Back / close chat"),
        Binding("c", "toggle_chat", "Chat"),
        Binding("t", "thinking", "Thinking"),
        Binding("p", "toggle_follow", "Follow"),
        Binding("f2", "app.settings", "Settings"),
        Binding("q", "quit", "Quit"),
    ]

    def __init__(self, info: SessionInfo) -> None:
        super().__init__()
        self.info = info
        self.store = EventStore()
        self.source = ClaudeCodeSource(info.path)
        self.follow = True
        self.provider = None
        self.chat: ChatPanel | None = None
        self._summarizer: LiveSummarizer | None = None
        self._summary_pending = False
        self._last_event_monotonic = 0.0

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Static(id="summary", classes="summary")
        yield Static(id="llm_summary", classes="llm-summary")
        with Horizontal():
            yield ListView(id="feed")
            with VerticalScroll(id="detailwrap"):
                yield Static("select an event to inspect", id="detail")
        yield ChatPanel()
        yield Footer()

    def on_mount(self) -> None:
        self.app.sub_title = _short_project(self.info.cwd) or self.info.session_id[:8]
        self.chat = self.query_one(ChatPanel)
        self.set_provider(self.app.provider)
        self._refresh_summary()
        self.run_worker(self._consume(), exclusive=True, name="ingest")
        self.run_worker(self._summary_loop(), name="summary")

    # -- provider wiring ---------------------------------------------------- #

    def set_provider(self, provider) -> None:
        self.provider = provider
        if provider is not None:
            self._summarizer = LiveSummarizer(provider, self.store)
            if self.chat is not None:
                self.chat.set_session(ChatSession(provider, self.store))
            # nudge an initial/refreshed summary
            self._summary_pending = True
            self._last_event_monotonic = time.monotonic() - self.app.config.summary_debounce
        else:
            self._summarizer = None
            if self.chat is not None:
                self.chat.set_session(None)
            self._set_llm_summary("", "")

    # -- ingestion ---------------------------------------------------------- #

    async def _consume(self) -> None:
        feed = self.query_one("#feed", ListView)
        backfill = await self.source.backfill()
        self.store.bulk_add(backfill)
        for ev in backfill[-MAX_FEED_ITEMS:]:
            await feed.append(EventItem(ev))
        self._refresh_summary()
        self._follow_to_end(feed)
        if self.provider is not None:  # summarize the backfilled context once
            self._summary_pending = True
            self._last_event_monotonic = time.monotonic() - self.app.config.summary_debounce

        async for ev in self.source.stream():
            self.store.add(ev)
            await feed.append(EventItem(ev))
            self._trim(feed)
            self._refresh_summary()
            self._follow_to_end(feed)
            self._summary_pending = True
            self._last_event_monotonic = time.monotonic()

    def _trim(self, feed: ListView) -> None:
        while len(feed.children) > MAX_FEED_ITEMS:
            try:
                feed.children[0].remove()
            except Exception:
                break

    def _follow_to_end(self, feed: ListView) -> None:
        if self.follow and len(feed.children):
            feed.index = len(feed.children) - 1
            feed.scroll_end(animate=False)

    # -- summaries ---------------------------------------------------------- #

    def _refresh_summary(self) -> None:
        head = Text("▶ ", style="bold green")
        head.append(summary_line(self.store))
        self.query_one("#summary", Static).update(head)

    def _set_llm_summary(self, text: str, style: str) -> None:
        self.query_one("#llm_summary", Static).update(Text(text, style=style))

    async def _summary_loop(self) -> None:
        import asyncio

        while True:
            await asyncio.sleep(0.4)
            if not self._summary_pending:
                continue
            if self._summarizer is None or not self.app.config.summary_enabled:
                self._summary_pending = False
                continue
            if time.monotonic() - self._last_event_monotonic < self.app.config.summary_debounce:
                continue
            self._summary_pending = False
            self._set_llm_summary("≈ summarizing…", "dim")
            try:
                text = await self._summarizer.summarize()
                self._set_llm_summary(f"≈ {text}", "italic cyan")
            except Exception as exc:
                self._set_llm_summary(f"≈ summary unavailable: {exc}", "dim red")

    # -- interactions ------------------------------------------------------- #

    def on_list_view_highlighted(self, event: ListView.Highlighted) -> None:
        if isinstance(event.item, EventItem):
            self.query_one("#detail", Static).update(detail_view(event.item.ev))

    def action_toggle_chat(self) -> None:
        chat = self.query_one(ChatPanel)
        chat.display = not chat.display
        if chat.display:
            self.query_one("#chatinput", Input).focus()
        else:
            self.query_one("#feed", ListView).focus()

    def action_thinking(self) -> None:
        self.app.push_screen(ThinkingScreen(self.store))

    def action_back(self) -> None:
        chat = self.query_one(ChatPanel)
        if chat.display:
            chat.display = False
            self.query_one("#feed", ListView).focus()
            return
        self.source.stop()
        self.app.pop_screen()

    def action_toggle_follow(self) -> None:
        self.follow = not self.follow
        self.notify(f"Follow {'on' if self.follow else 'off'}", timeout=1.5)


# --------------------------------------------------------------------------- #
# App
# --------------------------------------------------------------------------- #


class UnderstudyApp(App):
    TITLE = "Understudy"
    BINDINGS = [Binding("f2", "settings", "Settings")]
    CSS = """
    .hint { padding: 1 2; color: $text-muted; }
    .summary {
        padding: 1 2 0 2;
        height: auto;
        background: $panel;
    }
    .llm-summary {
        padding: 0 2 1 2;
        height: auto;
        background: $panel;
        border-bottom: solid $primary;
    }
    Horizontal { height: 1fr; }
    #feed { width: 3fr; height: 1fr; border-right: solid $primary; }
    #detailwrap { width: 2fr; height: 1fr; padding: 0 1; }
    #detail { width: 1fr; }
    DataTable { height: 1fr; }

    #chat { height: 45%; display: none; border-top: solid $primary; background: $panel; }
    #chatlog { height: 1fr; padding: 0 1; }
    #chatinput { dock: bottom; }

    #thinking_list { height: 1fr; padding: 0 1; }
    .think-body { padding: 0 2 1 2; color: $text-muted; }

    #setup { padding: 1 2; }
    #setup .title { text-style: bold; padding-bottom: 1; }
    #setup Label { padding-top: 1; color: $text-muted; }
    #buttons { height: auto; padding-top: 1; }
    #buttons Button { margin-right: 2; }
    .status { padding-top: 1; height: auto; }
    """

    def __init__(
        self,
        *,
        cwd_filter: str | None = None,
        session_path: Path | None = None,
        config: Config | None = None,
    ) -> None:
        super().__init__()
        self.cwd_filter = cwd_filter
        self.session_path = session_path
        self._injected_config = config
        self.config: Config = config or Config()
        self.provider = None

    def on_mount(self) -> None:
        self.config = self._injected_config or load_config()
        self.provider = self._safe_build(self.config)
        if not self.config.configured:
            self.push_screen(SetupScreen(self.config, first_run=True))
        else:
            self.open_initial()

    def open_initial(self) -> None:
        if self.session_path is not None:
            self.push_screen(FeedScreen(_info_from_path(self.session_path)))
        else:
            self.push_screen(SessionPicker(self.cwd_filter))

    def action_settings(self) -> None:
        if isinstance(self.screen, SetupScreen):
            return
        self.push_screen(SetupScreen(self.config, first_run=False))

    def apply_config(self, config: Config) -> None:
        self.config = config
        old = self.provider
        self.provider = self._safe_build(config)
        for screen in list(self.screen_stack):
            if isinstance(screen, FeedScreen):
                screen.set_provider(self.provider)
        if old is not None:
            self.run_worker(old.aclose(), exclusive=False)

    @staticmethod
    def _safe_build(config: Config):
        try:
            return build_provider(config.provider)
        except Exception:
            return None


# --------------------------------------------------------------------------- #
# Small formatting helpers
# --------------------------------------------------------------------------- #


def _ago(dt: datetime) -> str:
    secs = (datetime.now() - dt).total_seconds()
    if secs < 60:
        return "just now"
    if secs < 3600:
        return f"{int(secs // 60)}m ago"
    if secs < 86400:
        return f"{int(secs // 3600)}h ago"
    return f"{int(secs // 86400)}d ago"


def _short_project(cwd: str) -> str:
    if not cwd:
        return ""
    parts = Path(cwd).parts
    return "/".join(parts[-2:]) if len(parts) >= 2 else cwd
