//! nogent-listener — the GitHub App webhook server.
//!
//! Verifies webhook HMAC, mints installation tokens, and runs the PR
//! security-review / issue-triage in-process: fetch the diff/issue, call Gemini,
//! validate the canary-gated output, and post the comment.

mod app_auth;
mod config;
mod gemini_client;
mod github_client;
mod github_mint;
mod review;
mod triage;
mod webhook;

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use nogent_core::error::NogentError;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;

use crate::app_auth::AppAuth;
use crate::config::ListenerConfig;
use crate::webhook::AppState;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // Load .env in dev; ignore if absent.
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = ListenerConfig::from_env()?;
    let auth = AppAuth::new(&cfg.app_id, &cfg.private_key_pem)?;
    let bind_addr = cfg.bind_addr.clone();
    let max_body = cfg.max_body_bytes;

    let state = AppState {
        cfg: Arc::new(cfg),
        auth: Arc::new(auth),
    };

    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/github/webhooks", post(webhook::handle_webhook))
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&bind_addr)
        .await
        .map_err(|e| NogentError::Config(format!("cannot bind {bind_addr}: {e}")))?;
    tracing::info!(%bind_addr, "nogent-listener started");
    axum::serve(listener, app)
        .await
        .map_err(|e| NogentError::Io(e.to_string()))?;
    Ok(())
}
