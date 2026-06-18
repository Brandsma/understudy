"""Phase 2: a dedicated, collapsible thinking-token viewer.

Lists the session's chain-of-thought blocks (collapsed by default). Each block's
title is a one-line LLM "thought pattern" — computed lazily on expand, or in bulk
via `s`. Degrades gracefully when thinking text is absent/redacted.
"""

from __future__ import annotations

from rich.text import Text
from textual.app import ComposeResult
from textual.containers import VerticalScroll
from textual.screen import Screen
from textual.widgets import Collapsible, Footer, Header, Static

from understudy.events import Event, Kind
from understudy.store import EventStore
from understudy.summary.thinking import ThinkingSummarizer


class ThinkingBlock(Collapsible):
    def __init__(self, ev: Event) -> None:
        text = ev.payload.get("text", "")
        has_text = bool(text.strip())
        body = Static(
            text if has_text else "Thinking occurred, but its content is not exposed in this session.",
            classes="think-body",
        )
        super().__init__(body, title=_title(ev), collapsed=True)
        self.ev = ev
        self.has_text = has_text
        self._summarized = bool(ev.payload.get("summary"))

    async def summarize_with(self, summarizer: ThinkingSummarizer) -> None:
        if self._summarized or not self.has_text:
            return
        self._summarized = True
        previous = self.title
        self.title = "✲ summarizing…"
        try:
            summary = await summarizer.summarize(self.ev.payload.get("text", ""))
        except Exception as exc:
            self._summarized = False
            self.title = f"✲ {self.ev.ts:%H:%M:%S}  (summary failed: {exc})"
            return
        if summary:
            self.ev.payload["summary"] = summary
            self.title = f"✲ {self.ev.ts:%H:%M:%S}  {summary}"
        else:
            self.title = previous


class ThinkingScreen(Screen):
    BINDINGS = [
        ("escape", "back", "Back"),
        ("s", "summarize_all", "Summarize all"),
        ("r", "refresh", "Refresh"),
        ("q", "app.quit", "Quit"),
    ]

    def __init__(self, store: EventStore) -> None:
        super().__init__()
        self.store = store

    def compose(self) -> ComposeResult:
        yield Header(show_clock=True)
        yield Static(id="thinking_header", classes="summary")
        yield VerticalScroll(id="thinking_list")
        yield Footer()

    def on_mount(self) -> None:
        self.app.sub_title = "thinking"
        self._rebuild()

    def _blocks(self) -> list[Event]:
        return [e for e in self.store.events if e.kind == Kind.THINKING]

    def _rebuild(self) -> None:
        events = self._blocks()
        with_text = sum(1 for e in events if e.payload.get("text", "").strip())
        approx_tokens = sum(len(e.payload.get("text", "")) for e in events) // 4

        header = Text()
        header.append(f"{len(events)} thinking block(s)", style="bold magenta")
        header.append(f"  ·  {with_text} with readable text")
        if approx_tokens:
            header.append(f"  ·  ~{approx_tokens} tokens")
        if with_text:
            header.append("   ·   press s to summarize, or expand a block", style="dim")
        elif events:
            header.append("   ·   content not exposed in this session", style="dim")
        self.query_one("#thinking_header", Static).update(header)

        lst = self.query_one("#thinking_list", VerticalScroll)
        lst.remove_children()
        if not events:
            lst.mount(Static("No thinking blocks captured in this session yet.", classes="hint"))
            return
        for ev in events:
            lst.mount(ThinkingBlock(ev))

    def action_refresh(self) -> None:
        self._rebuild()

    def action_back(self) -> None:
        self.app.pop_screen()

    async def action_summarize_all(self) -> None:
        provider = self.app.provider
        if provider is None:
            self.notify("No model configured — press F2 to set one up.", severity="warning")
            return
        summarizer = ThinkingSummarizer(provider)
        blocks = [b for b in self.query(ThinkingBlock) if b.has_text and not b._summarized]
        if not blocks:
            self.notify("Nothing to summarize.")
            return
        self.notify(f"Summarizing {len(blocks)} block(s)…")
        for block in blocks:
            await block.summarize_with(summarizer)

    async def on_collapsible_expanded(self, event: Collapsible.Expanded) -> None:
        block = event.collapsible
        if isinstance(block, ThinkingBlock) and self.app.provider is not None:
            await block.summarize_with(ThinkingSummarizer(self.app.provider))


def _title(ev: Event) -> str:
    stamp = f"{ev.ts:%H:%M:%S}"
    summary = ev.payload.get("summary")
    if summary:
        return f"✲ {stamp}  {summary}"
    text = ev.payload.get("text", "")
    if text.strip():
        snippet = " ".join(text.split())
        return f"✲ {stamp}  {snippet[:79] + '…' if len(snippet) > 80 else snippet}"
    return f"✲ {stamp}  (content not exposed)"
