"""Persistent configuration (provider choice, summary settings)."""

from __future__ import annotations

import json
import os
from dataclasses import asdict, dataclass, field
from pathlib import Path

try:
    from platformdirs import user_config_dir
except Exception:  # pragma: no cover - fallback if platformdirs missing

    def user_config_dir(appname: str) -> str:
        return str(Path.home() / ".config" / appname)


APP_NAME = "understudy"


@dataclass
class ProviderConfig:
    kind: str = "ollama"  # ollama | openai | copilot | none
    base_url: str = "http://localhost:11434"
    api_key: str = ""
    model: str = ""
    temperature: float = 0.3


@dataclass
class Config:
    provider: ProviderConfig = field(default_factory=ProviderConfig)
    summary_enabled: bool = True
    summary_debounce: float = 2.0
    configured: bool = False  # has the user completed (or skipped) first-run setup?


def config_path() -> Path:
    override = os.environ.get("UNDERSTUDY_CONFIG")
    if override:
        return Path(override)
    return Path(user_config_dir(APP_NAME)) / "config.json"


def load_config() -> Config:
    path = config_path()
    if not path.is_file():
        return Config()
    try:
        raw = json.loads(path.read_text())
    except (OSError, json.JSONDecodeError):
        return Config()
    pc = raw.get("provider", {}) or {}
    return Config(
        provider=ProviderConfig(
            kind=pc.get("kind", "ollama"),
            base_url=pc.get("base_url", "http://localhost:11434"),
            api_key=pc.get("api_key", ""),
            model=pc.get("model", ""),
            temperature=float(pc.get("temperature", 0.3)),
        ),
        summary_enabled=bool(raw.get("summary_enabled", True)),
        summary_debounce=float(raw.get("summary_debounce", 2.0)),
        configured=bool(raw.get("configured", False)),
    )


def save_config(cfg: Config) -> Path:
    path = config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(
            {
                "provider": asdict(cfg.provider),
                "summary_enabled": cfg.summary_enabled,
                "summary_debounce": cfg.summary_debounce,
                "configured": cfg.configured,
            },
            indent=2,
        )
    )
    return path
