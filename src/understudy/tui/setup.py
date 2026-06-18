"""First-run setup wizard / settings screen for the model provider."""

from __future__ import annotations

from rich.text import Text
from textual.app import ComposeResult
from textual.containers import Horizontal, Vertical
from textual.screen import Screen
from textual.widgets import Button, Footer, Header, Input, Label, RadioButton, RadioSet, Static

from understudy.config import Config, ProviderConfig, save_config
from understudy.models import build_provider

KINDS = [
    ("ollama", "Ollama  (local, private)"),
    ("openai", "OpenAI-compatible API"),
    ("copilot", "GitHub Copilot  (experimental)"),
    ("none", "None Selected  ·  feed only"),
]
DEFAULT_BASE = {
    "ollama": "http://localhost:11434",
    "openai": "https://api.openai.com/v1",
    "copilot": "",
    "none": "",
}
DEFAULT_MODEL = {"ollama": "llama3.1", "openai": "gpt-4o-mini", "copilot": "gpt-4o-mini", "none": ""}
HINTS = {
    "ollama": "Ollama must be running locally. Use 'Detect models' to list what's installed.",
    "openai": "Works with OpenAI, OpenRouter, vLLM, LM Studio, etc. Base URL should end in /v1.",
    "copilot": "Uses your existing GitHub Copilot login (VS Code / gh / Copilot CLI). Experimental.",
    "none": "No model: the comprehension chat and live summary are off. The feed and diffs still work.",
}


def _display_kind(kind: str) -> str:
    """Map a stored kind to a selectable one. Unknown values fall back to 'none'."""
    return kind if kind in HINTS else "none"


class SetupScreen(Screen):
    BINDINGS = [("escape", "cancel", "Cancel")]

    def __init__(self, config: Config, *, first_run: bool = False) -> None:
        super().__init__()
        self.config = config
        self.first_run = first_run

    def compose(self) -> ComposeResult:
        yield Header()
        with Vertical(id="setup"):
            yield Static(
                "Set up Understudy's model" if self.first_run else "Model settings",
                classes="title",
            )
            yield Static(
                "The side-car sends the observed agent's activity to this model. "
                "Ollama keeps everything on your machine.",
                classes="hint",
            )
            yield Static(
                "↑/↓ then Enter to pick a provider · Tab between fields · "
                "Enter in a field saves · Esc skips · Ctrl+Q quits",
                classes="hint",
            )
            selected = _display_kind(self.config.provider.kind)
            with RadioSet(id="kind"):
                for value, label in KINDS:
                    yield RadioButton(label, value=(value == selected), id=f"kind-{value}")
            yield Label("Base URL")
            yield Input(value=self.config.provider.base_url, id="base_url")
            yield Label("API key  (blank = use env var / not needed)")
            yield Input(value=self.config.provider.api_key, password=True, id="api_key")
            yield Label("Model")
            yield Input(value=self.config.provider.model, placeholder="model name", id="model")
            with Horizontal(id="buttons"):
                yield Button("Detect models", id="detect")
                yield Button("Test", id="test", variant="primary")
                yield Button("Save", id="save", variant="success")
                yield Button("Skip" if self.first_run else "Cancel", id="cancel")
            yield Static("", id="status", classes="status")
        yield Footer()

    def on_mount(self) -> None:
        self.app.sub_title = "setup" if self.first_run else "settings"
        self._apply_kind(_display_kind(self.config.provider.kind), fill_defaults=False)
        self.query_one("#kind", RadioSet).focus()

    def on_input_submitted(self, event: Input.Submitted) -> None:
        # Enter in any field submits the wizard, so users don't have to find Save.
        self._save()

    # -- provider-kind reactive fields -------------------------------------- #

    def _current_kind(self) -> str:
        idx = self.query_one("#kind", RadioSet).pressed_index
        return KINDS[idx][0] if 0 <= idx < len(KINDS) else "ollama"

    def _apply_kind(self, kind: str, *, fill_defaults: bool = True) -> None:
        base = self.query_one("#base_url", Input)
        key = self.query_one("#api_key", Input)
        model = self.query_one("#model", Input)
        if fill_defaults or not base.value:
            base.value = DEFAULT_BASE.get(kind, "")
        if fill_defaults or not model.value:
            model.value = DEFAULT_MODEL.get(kind, "")
        base.disabled = kind in ("copilot", "none")
        key.disabled = kind != "openai"
        model.disabled = kind == "none"
        self._status(HINTS.get(kind, ""), "dim")

    def on_radio_set_changed(self, event: RadioSet.Changed) -> None:
        self._apply_kind(self._current_kind(), fill_defaults=True)

    # -- actions ------------------------------------------------------------ #

    def _provider_config(self) -> ProviderConfig:
        return ProviderConfig(
            kind=self._current_kind(),
            base_url=self.query_one("#base_url", Input).value.strip(),
            api_key=self.query_one("#api_key", Input).value.strip(),
            model=self.query_one("#model", Input).value.strip(),
            temperature=self.config.provider.temperature,
        )

    def _status(self, text: str, style: str = "") -> None:
        self.query_one("#status", Static).update(Text(text, style=style))

    async def on_button_pressed(self, event: Button.Pressed) -> None:
        match event.button.id:
            case "detect":
                await self._detect()
            case "test":
                await self._test()
            case "save":
                self._save()
            case "cancel":
                self.action_cancel()

    async def _detect(self) -> None:
        self._status("Detecting models…", "yellow")
        provider = build_provider(self._provider_config())
        if provider is None:
            self._status("No provider selected.", "red")
            return
        try:
            models = await provider.list_models()
            if models:
                shown = ", ".join(models[:12]) + ("…" if len(models) > 12 else "")
                self._status(f"Models: {shown}", "green")
                model_input = self.query_one("#model", Input)
                if not model_input.value:
                    model_input.value = models[0]
            else:
                self._status("Connected, but found no models.", "yellow")
        except Exception as exc:
            self._status(f"Detect failed: {exc}", "red")
        finally:
            await provider.aclose()

    async def _test(self) -> None:
        self._status("Testing connection…", "yellow")
        provider = build_provider(self._provider_config())
        if provider is None:
            self._status("No provider selected.", "red")
            return
        try:
            self._status(f"✓ {await provider.check()}", "green")
        except Exception as exc:
            self._status(f"✗ {exc}", "red")
        finally:
            await provider.aclose()

    def _save(self) -> None:
        self.config.provider = self._provider_config()
        self.config.configured = True
        save_config(self.config)
        self.app.apply_config(self.config)
        self._leave()

    def action_cancel(self) -> None:
        if self.first_run:
            # Skip: proceed without a model (chat/summary disabled), don't nag again.
            self.config.provider = ProviderConfig(kind="none")
            self.config.configured = True
            save_config(self.config)
            self.app.apply_config(self.config)
        self._leave()

    def _leave(self) -> None:
        self.app.pop_screen()
        if self.first_run:
            self.app.open_initial()
