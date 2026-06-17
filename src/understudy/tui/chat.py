"""Understudy's comprehension chat panel — streams answers from the configured provider."""

from __future__ import annotations

from rich.text import Text
from textual.containers import Vertical, VerticalScroll
from textual.widgets import Input, Static

from understudy.chat.session import ChatSession


class ChatPanel(Vertical):
    def __init__(self) -> None:
        super().__init__(id="chat")
        self.session: ChatSession | None = None

    def compose(self):
        yield VerticalScroll(id="chatlog")
        yield Input(placeholder="Ask Understudy about the agent…  (Enter to send)", id="chatinput")

    def set_session(self, session: ChatSession | None) -> None:
        self.session = session

    async def _note(self, text: str, style: str = "dim") -> None:
        log = self.query_one("#chatlog", VerticalScroll)
        await log.mount(Static(Text(text, style=style)))
        log.scroll_end(animate=False)

    async def on_input_submitted(self, event: Input.Submitted) -> None:
        question = event.value.strip()
        if not question:
            return
        event.input.value = ""
        if self.session is None:
            await self._note("No model configured — press F2 to set one up.", "yellow")
            return
        log = self.query_one("#chatlog", VerticalScroll)
        await log.mount(Static(Text.assemble(("you  ", "bold cyan"), (question, ""))))
        log.scroll_end(animate=False)
        self.run_worker(self._answer(question), exclusive=False)

    async def _answer(self, question: str) -> None:
        log = self.query_one("#chatlog", VerticalScroll)
        bubble = Static(Text.assemble(("bot  ", "bold green"), ("…", "dim")))
        await log.mount(bubble)
        log.scroll_end(animate=False)
        parts: list[str] = []
        try:
            async for delta in self.session.ask(question):
                parts.append(delta)
                bubble.update(Text.assemble(("bot  ", "bold green"), ("".join(parts), "")))
                log.scroll_end(animate=False)
            if not parts:
                bubble.update(Text.assemble(("bot  ", "bold green"), ("(no response)", "dim")))
        except Exception as exc:  # provider/transport errors surface inline
            bubble.update(Text.assemble(("bot  ", "bold red"), (f"error: {exc}", "red")))
            log.scroll_end(animate=False)
