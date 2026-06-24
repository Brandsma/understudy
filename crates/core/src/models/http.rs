//! Shared HTTP helpers: OpenAI-style SSE, Ollama NDJSON, and model listing.

use async_stream::try_stream;
use futures::{Stream, StreamExt};
use reqwest::{Client, RequestBuilder};
use serde_json::Value;

use super::{Headers, ProviderError};

fn truncate(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn apply(mut rb: RequestBuilder, headers: &Headers) -> RequestBuilder {
    for (k, v) in headers {
        rb = rb.header(k.as_str(), v.as_str());
    }
    rb
}

/// POST a chat/completions request and yield streamed content deltas (SSE `data:` lines).
pub fn stream_openai_sse(
    client: Client,
    url: String,
    payload: Value,
    headers: Headers,
) -> impl Stream<Item = Result<String, ProviderError>> {
    try_stream! {
        let resp = apply(client.post(&url).json(&payload), &headers).send().await
            .map_err(|e| ProviderError::msg(format!("connection failed: {e}")))?;
        let status = resp.status().as_u16();
        let mut bytes = if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            Err(ProviderError::msg(format!("HTTP {status}: {}", truncate(&body, 300))))?
        } else {
            resp.bytes_stream()
        };
        let mut buf = String::new();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| ProviderError::msg(format!("stream error: {e}")))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let raw: String = buf.drain(..=nl).collect();
                let line = raw.trim();
                if line.is_empty() || !line.starts_with("data:") {
                    continue;
                }
                let data = line[5..].trim();
                if data == "[DONE]" {
                    return;
                }
                if let Ok(obj) = serde_json::from_str::<Value>(data) {
                    if let Some(choices) = obj.get("choices").and_then(|c| c.as_array()) {
                        for ch in choices {
                            if let Some(delta) = ch.get("delta").and_then(|d| d.get("content")).and_then(|c| c.as_str()) {
                                if !delta.is_empty() {
                                    yield delta.to_string();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// POST an Ollama /api/chat request and yield streamed content (NDJSON lines).
pub fn stream_ollama(
    client: Client,
    url: String,
    payload: Value,
) -> impl Stream<Item = Result<String, ProviderError>> {
    try_stream! {
        let resp = client.post(&url).json(&payload).send().await
            .map_err(|e| ProviderError::msg(format!("cannot reach Ollama ({e})")))?;
        let status = resp.status().as_u16();
        let mut bytes = if status >= 400 {
            let body = resp.text().await.unwrap_or_default();
            Err(ProviderError::msg(format!("HTTP {status}: {}", truncate(&body, 300))))?
        } else {
            resp.bytes_stream()
        };
        let mut buf = String::new();
        while let Some(chunk) = bytes.next().await {
            let chunk = chunk.map_err(|e| ProviderError::msg(format!("stream error: {e}")))?;
            buf.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(nl) = buf.find('\n') {
                let raw: String = buf.drain(..=nl).collect();
                let line = raw.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(obj) = serde_json::from_str::<Value>(line) {
                    if let Some(content) = obj.get("message").and_then(|m| m.get("content")).and_then(|c| c.as_str()) {
                        if !content.is_empty() {
                            yield content.to_string();
                        }
                    }
                    if obj.get("done").and_then(|d| d.as_bool()).unwrap_or(false) {
                        return;
                    }
                }
            }
        }
    }
}

pub async fn list_openai_models(client: &Client, url: &str, headers: Headers) -> Result<Vec<String>, ProviderError> {
    let resp = apply(client.get(url), &headers).send().await
        .map_err(|e| ProviderError::msg(format!("connection failed: {e}")))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        let body = resp.text().await.unwrap_or_default();
        return Err(ProviderError::msg(format!("HTTP {status}: {}", truncate(&body, 200))));
    }
    let json: Value = resp.json().await.map_err(|e| ProviderError::msg(format!("bad json: {e}")))?;
    let mut ids: Vec<String> = json
        .get("data")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(|s| s.to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    Ok(ids)
}
