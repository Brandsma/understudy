//! Model-provider layer: Ollama, OpenAI-compatible, and GitHub Copilot, behind a
//! single `Provider` enum (no `async-trait`, no `dyn`). Mirrors the Python `models/`.

pub mod http;

mod copilot;
mod ollama;
mod openai;

pub use copilot::{find_copilot_oauth_token, CopilotProvider};
pub use ollama::OllamaProvider;
pub use openai::OpenAiProvider;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Serialize;

use crate::config::ProviderConfig;

/// Extra HTTP headers as (name, value) pairs (applied to the request builder).
pub type Headers = Vec<(String, String)>;

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("{0}")]
    Msg(String),
}

impl ProviderError {
    pub fn msg(s: impl Into<String>) -> Self {
        ProviderError::Msg(s.into())
    }
}

#[derive(Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(c: impl Into<String>) -> Self {
        Self { role: "system".into(), content: c.into() }
    }
    pub fn user(c: impl Into<String>) -> Self {
        Self { role: "user".into(), content: c.into() }
    }
    pub fn assistant(c: impl Into<String>) -> Self {
        Self { role: "assistant".into(), content: c.into() }
    }
}

pub enum Provider {
    Ollama(OllamaProvider),
    OpenAi(OpenAiProvider),
    Copilot(CopilotProvider),
}

impl Provider {
    pub fn kind(&self) -> &'static str {
        match self {
            Provider::Ollama(_) => "ollama",
            Provider::OpenAi(_) => "openai",
            Provider::Copilot(_) => "copilot",
        }
    }

    pub fn model(&self) -> &str {
        match self {
            Provider::Ollama(p) => &p.model,
            Provider::OpenAi(p) => &p.model,
            Provider::Copilot(p) => &p.model,
        }
    }

    pub fn stream_chat(&self, messages: Vec<ChatMessage>) -> BoxStream<'static, Result<String, ProviderError>> {
        match self {
            Provider::Ollama(p) => p.stream_chat(messages),
            Provider::OpenAi(p) => p.stream_chat(messages),
            Provider::Copilot(p) => p.stream_chat(messages),
        }
    }

    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        match self {
            Provider::Ollama(p) => p.list_models().await,
            Provider::OpenAi(p) => p.list_models().await,
            Provider::Copilot(p) => p.list_models().await,
        }
    }

    pub async fn check(&self) -> Result<String, ProviderError> {
        match self {
            Provider::Ollama(p) => p.check().await,
            Provider::OpenAi(p) => p.check().await,
            Provider::Copilot(p) => p.check().await,
        }
    }
}

fn or_default(s: &str, default: &str) -> String {
    if s.is_empty() {
        default.to_string()
    } else {
        s.to_string()
    }
}

/// Construct a provider from config. Returns None for kind "none"/unknown.
pub fn build_provider(pc: &ProviderConfig) -> Option<Provider> {
    match pc.kind.to_lowercase().as_str() {
        "none" | "" => None,
        "ollama" => Some(Provider::Ollama(OllamaProvider::new(
            or_default(&pc.base_url, "http://localhost:11434"),
            or_default(&pc.model, "llama3.1"),
            pc.temperature,
        ))),
        "openai" => {
            let key = if pc.api_key.is_empty() {
                std::env::var("OPENAI_API_KEY").unwrap_or_default()
            } else {
                pc.api_key.clone()
            };
            Some(Provider::OpenAi(OpenAiProvider::new(
                or_default(&pc.base_url, "https://api.openai.com/v1"),
                key,
                or_default(&pc.model, "gpt-4o-mini"),
                pc.temperature,
            )))
        }
        "copilot" => Some(Provider::Copilot(CopilotProvider::new(
            or_default(&pc.model, "gpt-4o-mini"),
            pc.temperature,
            None,
        ))),
        _ => None,
    }
}

/// Drain a streaming chat into a single string.
pub async fn complete(provider: &Provider, messages: Vec<ChatMessage>) -> Result<String, ProviderError> {
    let mut stream = provider.stream_chat(messages);
    let mut out = String::new();
    while let Some(item) = stream.next().await {
        out.push_str(&item?);
    }
    Ok(out)
}
