"""GitHub Copilot provider (experimental).

Consumes Copilot as a model backend via its OpenAI-compatible API. Uses an existing
Copilot OAuth login (VS Code / `gh` / Copilot CLI, or COPILOT_GITHUB_TOKEN), exchanges
it for a short-lived Copilot token, then calls api.githubcopilot.com.

This is not the official Node SDK — it's a dependency-light HTTP bridge for our Python
TUI. Endpoints/headers verified against community implementations; may break if GitHub
changes them.
"""

from __future__ import annotations

import json
import os
import time
from pathlib import Path
from typing import AsyncIterator

import httpx

from understudy.models._openai import list_openai_models, stream_openai_sse
from understudy.models.base import ChatMessage, ProviderError

_EXCHANGE_URL = "https://api.github.com/copilot_internal/v2/token"
_DEFAULT_BASE = "https://api.githubcopilot.com"


def find_copilot_oauth_token() -> str | None:
    """Locate an existing Copilot OAuth token from env or the standard config files."""
    for var in ("COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"):
        value = os.environ.get(var)
        if value:
            return value
    config_home = Path(os.environ.get("XDG_CONFIG_HOME", str(Path.home() / ".config")))
    for name in ("hosts.json", "apps.json"):
        path = config_home / "github-copilot" / name
        if not path.is_file():
            continue
        try:
            data = json.loads(path.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        if isinstance(data, dict):
            for entry in data.values():
                if isinstance(entry, dict) and entry.get("oauth_token"):
                    return entry["oauth_token"]
    return None


def _base_from_token(token: str) -> str:
    """The exchanged token embeds `proxy-ep=proxy.<host>`; api endpoint is `api.<host>`."""
    for part in token.split(";"):
        if part.startswith("proxy-ep="):
            endpoint = part.split("=", 1)[1].strip()
            if endpoint:
                host = "api." + endpoint[len("proxy.") :] if endpoint.startswith("proxy.") else endpoint
                return f"https://{host}"
    return _DEFAULT_BASE


class CopilotProvider:
    kind = "copilot"

    def __init__(
        self,
        *,
        model: str = "gpt-4o-mini",
        temperature: float = 0.3,
        oauth_token: str | None = None,
        timeout: float = 60.0,
    ) -> None:
        self.model = model
        self.temperature = temperature
        self._oauth = oauth_token or find_copilot_oauth_token()
        self._client = httpx.AsyncClient(timeout=timeout)
        self._token: str | None = None
        self._token_exp: float = 0.0
        self._base = _DEFAULT_BASE

    def _api_headers(self) -> dict:
        return {
            "Authorization": f"Bearer {self._token}",
            "Content-Type": "application/json",
            "Copilot-Integration-Id": "vscode-chat",
            "Editor-Version": "vscode/1.95.0",
            "Editor-Plugin-Version": "copilot-chat/0.22.0",
            "User-Agent": "GitHubCopilotChat/0.22.0",
            "Openai-Intent": "conversation-panel",
        }

    async def _ensure_token(self) -> None:
        if self._token and time.time() < self._token_exp - 60:
            return
        if not self._oauth:
            raise ProviderError(
                "No GitHub Copilot login found. Sign in via VS Code, `gh`, or the Copilot "
                "CLI, or set COPILOT_GITHUB_TOKEN."
            )
        try:
            resp = await self._client.get(
                _EXCHANGE_URL,
                headers={
                    "Accept": "application/json",
                    "Authorization": f"Bearer {self._oauth}",
                    "Editor-Version": "vscode/1.95.0",
                    "User-Agent": "GitHubCopilotChat/0.22.0",
                },
            )
        except httpx.HTTPError as exc:
            raise ProviderError(f"Copilot token exchange failed: {exc}") from exc
        if resp.status_code >= 400:
            raise ProviderError(
                f"Copilot token exchange failed (HTTP {resp.status_code}). "
                "Is Copilot active on your GitHub account?"
            )
        data = resp.json()
        self._token = data.get("token")
        if not self._token:
            raise ProviderError("Copilot token exchange returned no token.")
        exp = float(data.get("expires_at", 0) or 0)
        self._token_exp = exp / 1000.0 if exp > 1e11 else exp  # accept seconds or millis
        self._base = _base_from_token(self._token)

    async def stream_chat(self, messages: list[ChatMessage]) -> AsyncIterator[str]:
        await self._ensure_token()
        payload = {
            "model": self.model,
            "messages": messages,
            "stream": True,
            "temperature": self.temperature,
        }
        async for delta in stream_openai_sse(
            self._client, f"{self._base}/chat/completions", payload, self._api_headers()
        ):
            yield delta

    async def list_models(self) -> list[str]:
        await self._ensure_token()
        return await list_openai_models(self._client, f"{self._base}/models", self._api_headers())

    async def check(self) -> str:
        await self._ensure_token()
        try:
            models = await self.list_models()
        except ProviderError:
            models = []
        suffix = f" ({len(models)} models)" if models else ""
        return f"OK · Copilot · {self.model}{suffix}"

    async def aclose(self) -> None:
        await self._client.aclose()
