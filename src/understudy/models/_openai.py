"""Shared OpenAI-compatible HTTP helpers (used by OpenAI and Copilot providers)."""

from __future__ import annotations

import json
from typing import AsyncIterator

import httpx

from understudy.models.base import ProviderError


async def stream_openai_sse(
    client: httpx.AsyncClient,
    url: str,
    payload: dict,
    headers: dict,
) -> AsyncIterator[str]:
    """POST a chat/completions request and yield streamed content deltas."""
    try:
        async with client.stream("POST", url, json=payload, headers=headers) as resp:
            if resp.status_code >= 400:
                body = (await resp.aread()).decode("utf-8", "replace")
                raise ProviderError(f"HTTP {resp.status_code}: {body[:300]}")
            async for line in resp.aiter_lines():
                line = line.strip()
                if not line or not line.startswith("data:"):
                    continue
                data = line[len("data:") :].strip()
                if data == "[DONE]":
                    break
                try:
                    obj = json.loads(data)
                except json.JSONDecodeError:
                    continue
                for choice in obj.get("choices", []):
                    delta = (choice.get("delta") or {}).get("content")
                    if delta:
                        yield delta
    except httpx.HTTPError as exc:
        raise ProviderError(f"connection failed: {exc}") from exc


async def list_openai_models(client: httpx.AsyncClient, url: str, headers: dict) -> list[str]:
    try:
        resp = await client.get(url, headers=headers)
    except httpx.HTTPError as exc:
        raise ProviderError(f"connection failed: {exc}") from exc
    if resp.status_code >= 400:
        raise ProviderError(f"HTTP {resp.status_code}: {resp.text[:200]}")
    data = resp.json().get("data", [])
    return sorted(m.get("id", "") for m in data if m.get("id"))
