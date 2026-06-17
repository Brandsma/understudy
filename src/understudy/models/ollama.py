"""Ollama provider (local). Native /api/chat for streaming, /api/tags for models."""

from __future__ import annotations

import json
from typing import AsyncIterator

import httpx

from understudy.models.base import ChatMessage, ProviderError


class OllamaProvider:
    kind = "ollama"

    def __init__(
        self,
        *,
        base_url: str = "http://localhost:11434",
        model: str = "llama3.1",
        temperature: float = 0.3,
        timeout: float = 120.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.model = model
        self.temperature = temperature
        self._client = httpx.AsyncClient(timeout=timeout)

    async def stream_chat(self, messages: list[ChatMessage]) -> AsyncIterator[str]:
        payload = {
            "model": self.model,
            "messages": messages,
            "stream": True,
            "options": {"temperature": self.temperature},
        }
        try:
            async with self._client.stream("POST", f"{self.base_url}/api/chat", json=payload) as resp:
                if resp.status_code >= 400:
                    body = (await resp.aread()).decode("utf-8", "replace")
                    raise ProviderError(f"HTTP {resp.status_code}: {body[:300]}")
                async for line in resp.aiter_lines():
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        obj = json.loads(line)
                    except json.JSONDecodeError:
                        continue
                    content = (obj.get("message") or {}).get("content")
                    if content:
                        yield content
                    if obj.get("done"):
                        break
        except httpx.HTTPError as exc:
            raise ProviderError(f"cannot reach Ollama at {self.base_url} ({exc})") from exc

    async def list_models(self) -> list[str]:
        try:
            resp = await self._client.get(f"{self.base_url}/api/tags")
        except httpx.HTTPError as exc:
            raise ProviderError(f"cannot reach Ollama at {self.base_url} ({exc})") from exc
        if resp.status_code >= 400:
            raise ProviderError(f"HTTP {resp.status_code}: {resp.text[:200]}")
        return sorted(m.get("name", "") for m in resp.json().get("models", []) if m.get("name"))

    async def check(self) -> str:
        models = await self.list_models()
        if not models:
            return f"Connected @ {self.base_url} · no models — try `ollama pull {self.model}`"
        if self.model not in models:
            return f"Connected · {len(models)} models · '{self.model}' not installed — have: {', '.join(models[:4])}"
        return f"OK · {self.model} @ {self.base_url}"

    async def aclose(self) -> None:
        await self._client.aclose()
