//! OpenAI-compatible provider: OpenAI, OpenRouter, vLLM, LM Studio, etc.

use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::json;

use super::http::{list_openai_models, stream_openai_sse};
use super::{ChatMessage, Headers, ProviderError};

pub struct OpenAiProvider {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub temperature: f32,
    client: Client,
}

impl OpenAiProvider {
    pub fn new(base_url: String, api_key: String, model: String, temperature: f32) -> Self {
        OpenAiProvider {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            temperature,
            client: Client::new(),
        }
    }

    fn headers(&self) -> Headers {
        let mut h: Headers = Vec::new();
        if !self.api_key.is_empty() {
            h.push(("Authorization".into(), format!("Bearer {}", self.api_key)));
        }
        h
    }

    pub fn stream_chat(&self, messages: Vec<ChatMessage>) -> BoxStream<'static, Result<String, ProviderError>> {
        let url = format!("{}/chat/completions", self.base_url);
        let payload = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "temperature": self.temperature,
        });
        Box::pin(stream_openai_sse(self.client.clone(), url, payload, self.headers()))
    }

    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        list_openai_models(&self.client, &format!("{}/models", self.base_url), self.headers()).await
    }

    pub async fn check(&self) -> Result<String, ProviderError> {
        let models = self.list_models().await?;
        let note = if models.is_empty() || models.contains(&self.model) {
            String::new()
        } else {
            format!("  (model '{}' not listed)", self.model)
        };
        Ok(format!("OK · {} models @ {}{}", models.len(), self.base_url, note))
    }
}
