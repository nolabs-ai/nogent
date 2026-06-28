//! Shared error type. Per nono coding standards we never `.unwrap()`/`.expect()`
//! on fallible paths; everything propagates through `NogentError` via `?`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NogentError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid webhook signature")]
    InvalidSignature,

    #[error("malformed webhook payload: {0}")]
    Payload(String),

    #[error("github api error ({status}): {body}")]
    GitHubApi { status: u16, body: String },

    #[error("gemini api error ({status}): {body}")]
    GeminiApi { status: u16, body: String },

    #[error("model output failed validation: {0}")]
    OutputValidation(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("http transport error: {0}")]
    Http(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("serialization error: {0}")]
    Serde(String),
}

impl From<reqwest::Error> for NogentError {
    fn from(e: reqwest::Error) -> Self {
        NogentError::Http(e.to_string())
    }
}

impl From<std::io::Error> for NogentError {
    fn from(e: std::io::Error) -> Self {
        NogentError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for NogentError {
    fn from(e: serde_json::Error) -> Self {
        NogentError::Serde(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, NogentError>;
