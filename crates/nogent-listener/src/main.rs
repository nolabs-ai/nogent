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
mod repo_index;
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

    // Local eval mode: review a diff against a local checkout, no GitHub.
    //   GEMINI_API_KEY=... nogent-listener --review-local --repo . --diff change.diff
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--review-local") {
        return review_local(&args).await;
    }

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

/// `--review-local --repo <dir> --diff <path|->`: build the repo index from a
/// local checkout, read a unified diff (file or stdin), run the real agentic
/// review against Gemini, and print the Markdown review. Needs GEMINI_API_KEY.
async fn review_local(args: &[String]) -> std::result::Result<(), Box<dyn std::error::Error>> {
    use std::io::Read;

    let arg = |flag: &str| -> Option<String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1).cloned())
    };

    let repo = arg("--repo").unwrap_or_else(|| ".".to_string());
    let api_key = std::env::var("GEMINI_API_KEY")
        .map_err(|_| NogentError::Config("GEMINI_API_KEY is required for --review-local".into()))?;
    let model = std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-3.5-flash".to_string());
    let thinking = std::env::var("GEMINI_THINKING_LEVEL").unwrap_or_else(|_| "high".to_string());
    let thinking = if thinking.trim().is_empty() {
        None
    } else {
        Some(thinking)
    };

    let diff = match arg("--diff").as_deref() {
        Some("-") | None => {
            let mut buf = String::new();
            std::io::stdin().read_to_string(&mut buf)?;
            buf
        }
        Some(path) => std::fs::read_to_string(path)?,
    };
    if diff.trim().is_empty() {
        return Err(Box::new(NogentError::Config(
            "empty diff (pass --diff <file> or pipe one on stdin)".into(),
        )));
    }

    // 50 MB index cap, same as the production path.
    let index = repo_index::RepoIndex::from_dir(std::path::Path::new(&repo), 50_000_000)?
        .ok_or_else(|| NogentError::Config(format!("repo at {repo} exceeds the index cap")))?;
    tracing::info!(files = index.file_count(), %model, "indexed local repo; reviewing");

    let markdown = review::run_local(&api_key, &model, thinking.as_deref(), &diff, &index).await?;
    println!("\n{markdown}");
    Ok(())
}
