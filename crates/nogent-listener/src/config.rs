//! Listener process configuration, loaded from the environment.
//!
//! The listener holds the real secrets (App private key, webhook secret, Gemini
//! key) in `Zeroizing` memory and never logs them.

use std::env;

use nogent_core::error::{NogentError, Result};
use zeroize::Zeroizing;

pub struct ListenerConfig {
    pub app_id: String,
    pub private_key_pem: Zeroizing<String>,
    pub webhook_secret: Zeroizing<String>,
    pub gemini_api_key: Zeroizing<String>,
    pub gemini_model: String,
    /// Reasoning level for thinking-capable models (Gemini 3.x): minimal | low |
    /// medium | high. Empty → omitted (use for 2.5 models).
    pub gemini_thinking_level: Option<String>,
    pub bind_addr: String,
    /// Max webhook body size in bytes.
    pub max_body_bytes: usize,
}

fn req(key: &str) -> Result<String> {
    env::var(key).map_err(|_| NogentError::Config(format!("missing required env var {key}")))
}

fn opt(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Load the App private key PEM from `GITHUB_APP_PRIVATE_KEY` (inline) or, if
/// that is unset, from the file at `GITHUB_APP_PRIVATE_KEY_FILE`. The file form
/// is preferred for deployment: a multi-line PEM does not fit cleanly in a
/// systemd `EnvironmentFile`, so the deploy writes it to a 0600 file instead.
fn load_private_key() -> Result<Zeroizing<String>> {
    if let Ok(inline) = env::var("GITHUB_APP_PRIVATE_KEY") {
        return Ok(Zeroizing::new(inline));
    }
    let path = env::var("GITHUB_APP_PRIVATE_KEY_FILE").map_err(|_| {
        NogentError::Config(
            "set GITHUB_APP_PRIVATE_KEY (inline PEM) or GITHUB_APP_PRIVATE_KEY_FILE (path)".into(),
        )
    })?;
    let pem = std::fs::read_to_string(&path).map_err(|e| {
        NogentError::Config(format!("reading GITHUB_APP_PRIVATE_KEY_FILE {path}: {e}"))
    })?;
    Ok(Zeroizing::new(pem))
}

impl ListenerConfig {
    /// Load from environment, failing if any required secret is absent
    /// (fail-secure: we never start without the webhook secret + app key).
    pub fn from_env() -> Result<Self> {
        let private_key_pem = load_private_key()?;
        let webhook_secret = Zeroizing::new(req("GITHUB_WEBHOOK_SECRET")?);
        let gemini_api_key = Zeroizing::new(req("GEMINI_API_KEY")?);

        let max_body_bytes = opt("NOGENT_MAX_BODY_BYTES", "2097152")
            .parse::<usize>()
            .map_err(|e| NogentError::Config(format!("NOGENT_MAX_BODY_BYTES invalid: {e}")))?;

        Ok(ListenerConfig {
            app_id: req("GITHUB_APP_ID")?,
            private_key_pem,
            webhook_secret,
            gemini_api_key,
            gemini_model: opt("GEMINI_MODEL", "gemini-3.5-flash"),
            gemini_thinking_level: match opt("GEMINI_THINKING_LEVEL", "high").trim() {
                "" => None,
                level => Some(level.to_string()),
            },
            bind_addr: opt("NOGENT_BIND_ADDR", "127.0.0.1:8080"),
            max_body_bytes,
        })
    }
}
