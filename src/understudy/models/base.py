"""Model-provider protocol shared by Ollama, OpenAI-compatible, and Copilot.

The chat session and the live summarizer depend only on this protocol, so swapping
providers (via the settings screen) is transparent.
"""

from __future__ import annotations

from typing import AsyncIterator, Protocol, runtime_checkable

# OpenAI-style message: {"role": "system"|"user"|"assistant", "content": str}
ChatMessage = dict[str, str]


class ProviderError(Exception):
    """Raised for any provider/transport failure, with a user-facing message."""


@runtime_checkable
class ModelProvider(Protocol):
    kind: str
    model: str

    def stream_chat(self, messages: list[ChatMessage]) -> AsyncIterator[str]:
        """Yield response text deltas as they stream in."""
        ...

    async def list_models(self) -> list[str]:
        """Model ids available to this provider (best-effort; may be empty)."""
        ...

    async def check(self) -> str:
        """Validate connectivity/auth; return a status line or raise ProviderError."""
        ...

    async def aclose(self) -> None:
        """Release the underlying HTTP client."""
        ...


async def complete(provider: ModelProvider, messages: list[ChatMessage]) -> str:
    """Drain a streaming chat into a single string."""
    parts: list[str] = []
    async for delta in provider.stream_chat(messages):
        parts.append(delta)
    return "".join(parts)
