//! Persistent configuration (provider choice, summary settings).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

fn default_kind() -> String {
    "ollama".to_string()
}
fn default_base_url() -> String {
    "http://localhost:11434".to_string()
}
fn default_temp() -> f32 {
    0.3
}
fn default_true() -> bool {
    true
}
fn default_debounce() -> f32 {
    2.0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_kind")]
    pub kind: String, // ollama | openai | copilot | none
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub model: String,
    #[serde(default = "default_temp")]
    pub temperature: f32,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        ProviderConfig {
            kind: default_kind(),
            base_url: default_base_url(),
            api_key: String::new(),
            model: String::new(),
            temperature: default_temp(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub provider: ProviderConfig,
    #[serde(default = "default_true")]
    pub summary_enabled: bool,
    #[serde(default = "default_debounce")]
    pub summary_debounce: f32,
    #[serde(default)]
    pub configured: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            provider: ProviderConfig::default(),
            summary_enabled: true,
            summary_debounce: 2.0,
            configured: false,
        }
    }
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("UNDERSTUDY_CONFIG") {
        return PathBuf::from(p);
    }
    if let Some(dirs) = directories::ProjectDirs::from("", "", "understudy") {
        return dirs.config_dir().join("config.json");
    }
    PathBuf::from("understudy-config.json")
}

pub fn load_config() -> Config {
    match std::fs::read_to_string(config_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => Config::default(),
    }
}

pub fn save_config(cfg: &Config) -> std::io::Result<PathBuf> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(cfg).expect("config serializes");
    std::fs::write(&path, json)?;
    Ok(path)
}
