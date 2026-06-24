//! GitHub Copilot provider (experimental). Consumes Copilot via its OpenAI-compatible
//! API: reads an existing OAuth login, exchanges it for a short-lived token, then calls
//! api.githubcopilot.com. Not the official SDK — a dependency-light HTTP bridge.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_stream::try_stream;
use futures::stream::BoxStream;
use futures::StreamExt;
use reqwest::Client;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::http::{list_openai_models, stream_openai_sse};
use super::{ChatMessage, Headers, ProviderError};

const EXCHANGE_URL: &str = "https://api.github.com/copilot_internal/v2/token";
const DEFAULT_BASE: &str = "https://api.githubcopilot.com";

/// Locate an existing Copilot OAuth token from env or the standard config files.
pub fn find_copilot_oauth_token() -> Option<String> {
    for var in ["COPILOT_GITHUB_TOKEN", "GH_TOKEN", "GITHUB_TOKEN"] {
        if let Ok(v) = std::env::var(var) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            directories::BaseDirs::new()
                .map(|b| b.home_dir().join(".config"))
                .unwrap_or_else(|| PathBuf::from(".config"))
        });
    for name in ["hosts.json", "apps.json"] {
        let path = config_home.join("github-copilot").join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(data) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(obj) = data.as_object() {
            for entry in obj.values() {
                if let Some(tok) = entry.get("oauth_token").and_then(|v| v.as_str()) {
                    if !tok.is_empty() {
                        return Some(tok.to_string());
                    }
                }
            }
        }
    }
    None
}

/// The exchanged token embeds `proxy-ep=proxy.<host>`; the API endpoint is `api.<host>`.
fn base_from_token(token: &str) -> String {
    for part in token.split(';') {
        if let Some(ep) = part.strip_prefix("proxy-ep=") {
            let ep = ep.trim();
            if !ep.is_empty() {
                let host = ep.strip_prefix("proxy.").map(|rest| format!("api.{rest}")).unwrap_or_else(|| ep.to_string());
                return format!("https://{host}");
            }
        }
    }
    DEFAULT_BASE.to_string()
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

#[derive(Default)]
struct TokenCache {
    token: Option<String>,
    exp: f64,
    base: String,
}

pub struct CopilotProvider {
    pub model: String,
    pub temperature: f32,
    oauth: Option<String>,
    client: Client,
    cache: Arc<Mutex<TokenCache>>,
}

fn api_headers(token: &str) -> Headers {
    vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("Copilot-Integration-Id".into(), "vscode-chat".into()),
        ("Editor-Version".into(), "vscode/1.95.0".into()),
        ("Editor-Plugin-Version".into(), "copilot-chat/0.22.0".into()),
        ("User-Agent".into(), "GitHubCopilotChat/0.22.0".into()),
        ("Openai-Intent".into(), "conversation-panel".into()),
    ]
}

async fn ensure_token(
    client: &Client,
    oauth: &Option<String>,
    cache: &Arc<Mutex<TokenCache>>,
) -> Result<(String, String), ProviderError> {
    {
        let c = cache.lock().await;
        if let Some(tok) = &c.token {
            if now_secs() < c.exp - 60.0 {
                return Ok((tok.clone(), c.base.clone()));
            }
        }
    }
    let oauth = oauth.as_ref().ok_or_else(|| {
        ProviderError::msg(
            "No GitHub Copilot login found. Sign in via VS Code, `gh`, or the Copilot CLI, or set COPILOT_GITHUB_TOKEN.",
        )
    })?;
    let resp = client
        .get(EXCHANGE_URL)
        .header("Accept", "application/json")
        .header("Authorization", format!("Bearer {oauth}"))
        .header("Editor-Version", "vscode/1.95.0")
        .header("User-Agent", "GitHubCopilotChat/0.22.0")
        .send()
        .await
        .map_err(|e| ProviderError::msg(format!("Copilot token exchange failed: {e}")))?;
    let status = resp.status().as_u16();
    if status >= 400 {
        return Err(ProviderError::msg(format!(
            "Copilot token exchange failed (HTTP {status}). Is Copilot active on your GitHub account?"
        )));
    }
    let data: Value = resp.json().await.map_err(|e| ProviderError::msg(format!("bad token json: {e}")))?;
    let token = data
        .get("token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| ProviderError::msg("Copilot token exchange returned no token."))?
        .to_string();
    let exp_raw = data.get("expires_at").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let exp = if exp_raw > 1e11 { exp_raw / 1000.0 } else { exp_raw };
    let base = base_from_token(&token);
    {
        let mut c = cache.lock().await;
        c.token = Some(token.clone());
        c.exp = exp;
        c.base = base.clone();
    }
    Ok((token, base))
}

impl CopilotProvider {
    pub fn new(model: String, temperature: f32, oauth_token: Option<String>) -> Self {
        CopilotProvider {
            model,
            temperature,
            oauth: oauth_token.or_else(find_copilot_oauth_token),
            client: Client::new(),
            cache: Arc::new(Mutex::new(TokenCache::default())),
        }
    }

    pub fn stream_chat(&self, messages: Vec<ChatMessage>) -> BoxStream<'static, Result<String, ProviderError>> {
        let client = self.client.clone();
        let oauth = self.oauth.clone();
        let cache = self.cache.clone();
        let model = self.model.clone();
        let temperature = self.temperature;
        Box::pin(try_stream! {
            let (token, base) = ensure_token(&client, &oauth, &cache).await?;
            let payload = json!({
                "model": model,
                "messages": messages,
                "stream": true,
                "temperature": temperature,
            });
            let url = format!("{base}/chat/completions");
            let inner = stream_openai_sse(client.clone(), url, payload, api_headers(&token));
            futures::pin_mut!(inner);
            while let Some(item) = inner.next().await {
                let delta = item?;
                yield delta;
            }
        })
    }

    pub async fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        let (token, base) = ensure_token(&self.client, &self.oauth, &self.cache).await?;
        list_openai_models(&self.client, &format!("{base}/models"), api_headers(&token)).await
    }

    pub async fn check(&self) -> Result<String, ProviderError> {
        let _ = ensure_token(&self.client, &self.oauth, &self.cache).await?;
        let models = self.list_models().await.unwrap_or_default();
        let suffix = if models.is_empty() {
            String::new()
        } else {
            format!(" ({} models)", models.len())
        };
        Ok(format!("OK · Copilot · {}{}", self.model, suffix))
    }
}
