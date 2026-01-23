use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub ui: UiConfig,
    pub network: NetworkConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_server_url")]
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    #[serde(default = "default_token_path")]
    pub token_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    #[serde(default = "default_true")]
    pub show_timestamps: bool,
    #[serde(default = "default_message_limit")]
    pub message_limit: usize,
    #[serde(default = "default_false")]
    pub multiline_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    #[serde(default = "default_reconnect_attempts")]
    pub reconnect_attempts: usize,
    #[serde(default = "default_ping_interval")]
    pub ping_interval: u64,
}

fn default_server_url() -> String {
    std::env::var("EURUS_SERVER_URL").unwrap_or_else(|_| "wss://eurus.sreus.tech/ws".to_string())
}

fn default_token_path() -> String {
    "~/.config/eurus/token".to_string()
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_message_limit() -> usize {
    1000
}

fn default_reconnect_attempts() -> usize {
    10
}

fn default_ping_interval() -> u64 {
    30
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                url: default_server_url(),
            },
            auth: AuthConfig {
                token_path: default_token_path(),
            },
            ui: UiConfig {
                show_timestamps: true,
                message_limit: 1000,
                multiline_mode: false,
            },
            network: NetworkConfig {
                reconnect_attempts: 10,
                ping_interval: 30,
            },
        }
    }
}

impl Config {
    pub fn load() -> Self {
        // Try to load from config file
        if let Some(config_path) = Self::config_path() {
            if let Ok(contents) = fs::read_to_string(&config_path) {
                if let Ok(config) = toml::from_str(&contents) {
                    return config;
                }
            }
        }

        // Fall back to defaults
        Self::default()
    }

    pub fn config_path() -> Option<PathBuf> {
        dirs::config_dir().map(|mut path| {
            path.push("eurus");
            path.push("config.toml");
            path
        })
    }
}
