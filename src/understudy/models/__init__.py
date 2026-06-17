"""Model provider layer: factory + protocol re-exports."""

from __future__ import annotations

import os

from understudy.models.base import ChatMessage, ModelProvider, ProviderError, complete

__all__ = ["build_provider", "ModelProvider", "ProviderError", "ChatMessage", "complete"]


def build_provider(pc) -> ModelProvider | None:
    """Construct a provider from a ProviderConfig. Returns None for kind 'none'.

    Construction is cheap (no network) — connectivity is checked on first use.
    """
    kind = (pc.kind or "none").lower()
    if kind in ("none", ""):
        return None

    # Imported lazily to keep this module import-light.
    from understudy.models.copilot import CopilotProvider
    from understudy.models.ollama import OllamaProvider
    from understudy.models.openai_compat import OpenAICompatProvider

    if kind == "ollama":
        return OllamaProvider(
            base_url=pc.base_url or "http://localhost:11434",
            model=pc.model or "llama3.1",
            temperature=pc.temperature,
        )
    if kind == "openai":
        return OpenAICompatProvider(
            base_url=pc.base_url or "https://api.openai.com/v1",
            api_key=pc.api_key or os.environ.get("OPENAI_API_KEY", ""),
            model=pc.model or "gpt-4o-mini",
            temperature=pc.temperature,
        )
    if kind == "copilot":
        return CopilotProvider(model=pc.model or "gpt-4o-mini", temperature=pc.temperature)

    raise ProviderError(f"unknown provider kind: {kind!r}")
