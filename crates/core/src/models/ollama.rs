//! Ollama provider (local). Native /api/chat streaming, /api/tags for models.

use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::{json, Value};

use super::http::stream_ollama;
use super::{ChatMessage, ProviderError};

pub struct OllamaProvider {
    pub base_url: String,
    pub model: String,
    pub temperature: f32,
    client: Client,
}

impl OllamaProvider {
    pub fn new(base_url: String, model: String, temperature: f32) -> Self {
        OllamaProvider {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            temperature,
            client: Client::new(),
        }
    }

    pub fn stream_chat(&self, messages: Vec<ChatMessage>) -> BoxStream<'static, Result<String, ProviderError>> {
        let url = format!("{}/api/chat", self.base_url);
        let payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "options": { "temperature": self.temperature },
        });
        Box::pin(stream_ollama(self.client.clone(), url, payload))
    }

    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let resp = self
            .client
            .get(format!("{}/api/tags", self.base_url))
            .send()
            .await
            .map_err(|e| ProviderError::msg(format!("cannot reach Ollama at {} ({e})", self.base_url)))?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::msg(format!("HTTP {status}: {}", body.chars().take(200).collect::<String>())));
        }
        let json: Value = resp.json().await.map_err(|e| ProviderError::msg(format!("bad json: {e}")))?;
        let mut names: Vec<String> = json
            .get("models")
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(|s| s.to_string()))
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        Ok(names)
    }

    pub async fn check(&self) -> Result<String, ProviderError> {
        let models = self.list_models().await?;
        if models.is_empty() {
            return Ok(format!("Connected @ {} · no models — try `ollama pull {}`", self.base_url, self.model));
        }
        if !models.contains(&self.model) {
            let have: Vec<&String> = models.iter().take(4).collect();
            let have = have.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
            return Ok(format!("Connected · {} models · '{}' not installed — have: {}", models.len(), self.model, have));
        }
        Ok(format!("OK · {} @ {}", self.model, self.base_url))
    }
}
