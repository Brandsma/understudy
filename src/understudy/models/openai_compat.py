"""OpenAI-compatible provider: OpenAI, OpenRouter, vLLM, LM Studio, etc."""

from __future__ import annotations

from typing import AsyncIterator

import httpx

from understudy.models._openai import list_openai_models, stream_openai_sse
from understudy.models.base import ChatMessage


class OpenAICompatProvider:
    kind = "openai"

    def __init__(
        self,
        *,
        base_url: str,
        api_key: str = "",
        model: str = "gpt-4o-mini",
        temperature: float = 0.3,
        timeout: float = 60.0,
        extra_headers: dict | None = None,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self.model = model
        self.temperature = temperature
        self.extra_headers = dict(extra_headers or {})
        self._client = httpx.AsyncClient(timeout=timeout)

    def _headers(self) -> dict:
        headers = {"Content-Type": "application/json", **self.extra_headers}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        return headers

    async def stream_chat(self, messages: list[ChatMessage]) -> AsyncIterator[str]:
        payload = {
            "model": self.model,
            "messages": messages,
            "stream": True,
            "temperature": self.temperature,
        }
        async for delta in stream_openai_sse(
            self._client, f"{self.base_url}/chat/completions", payload, self._headers()
        ):
            yield delta

    async def list_models(self) -> list[str]:
        return await list_openai_models(self._client, f"{self.base_url}/models", self._headers())

    async def check(self) -> str:
        models = await self.list_models()
        note = "" if (not models or self.model in models) else f"  (model '{self.model}' not listed)"
        return f"OK · {len(models)} models @ {self.base_url}{note}"

    async def aclose(self) -> None:
        await self._client.aclose()
