//! Webhook receiver: verify, parse, dispatch.
//!
//! The hot path is intentionally short: verify the HMAC, identify the event,
//! then hand off to a detached task and fast-ack. GitHub expects a response
//! within ~10s; minting tokens and running the model happen off the response
//! path.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use nogent_core::error::Result;
use nogent_core::events::{self, EventJob, IssueEvent, JobKind, PullRequestEvent};
use nogent_core::hmac_verify::verify_signature;
use nogent_core::repo_config::ResolvedConfig;

use crate::app_auth::AppAuth;
use crate::config::ListenerConfig;
use crate::{github_mint, review, triage};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<ListenerConfig>,
    pub auth: Arc<AppAuth>,
}

const SIG_HEADER: &str = "x-hub-signature-256";
const EVENT_HEADER: &str = "x-github-event";
const DELIVERY_HEADER: &str = "x-github-delivery";

/// POST /api/github/webhooks
pub async fn handle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    // 1. Verify signature over the RAW body.
    let sig = headers.get(SIG_HEADER).and_then(|v| v.to_str().ok());
    let Some(sig) = sig else {
        return StatusCode::UNAUTHORIZED;
    };
    if !verify_signature(state.cfg.webhook_secret.as_bytes(), &body, sig) {
        return StatusCode::UNAUTHORIZED;
    }

    let event = headers
        .get(EVENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let delivery = headers
        .get(DELIVERY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    match event.as_str() {
        "ping" => StatusCode::OK,
        "pull_request" | "issues" => {
            // Detach: token minting + config fetch + model call off the ack path.
            let body = body.to_vec();
            tokio::spawn(async move {
                if let Err(e) = dispatch(&state, &event, &delivery, &body).await {
                    tracing::warn!(%delivery, %event, error = %e, "event dispatch failed");
                }
            });
            StatusCode::ACCEPTED
        }
        other => {
            tracing::debug!(%delivery, event = other, "ignoring unsubscribed event");
            StatusCode::NO_CONTENT
        }
    }
}

async fn dispatch(state: &AppState, event: &str, delivery: &str, body: &[u8]) -> Result<()> {
    match event {
        "pull_request" => dispatch_pr(state, delivery, body).await,
        "issues" => dispatch_issue(state, delivery, body).await,
        _ => Ok(()),
    }
}

async fn dispatch_pr(state: &AppState, delivery: &str, body: &[u8]) -> Result<()> {
    let ev: PullRequestEvent = serde_json::from_slice(body)
        .map_err(|e| nogent_core::NogentError::Payload(format!("pull_request: {e}")))?;

    if !events::is_actionable_pr_action(&ev.action) || ev.pull_request.draft {
        tracing::debug!(%delivery, action = %ev.action, draft = ev.pull_request.draft, "skipping PR");
        return Ok(());
    }
    let Some((owner, repo)) = ev.repository.owner_repo() else {
        tracing::warn!(%delivery, full_name = %ev.repository.full_name, "bad repo full_name");
        return Ok(());
    };

    let token = state.auth.installation_token(ev.installation.id).await?;
    let cfg = resolve_config(&token, owner, repo).await?;
    if !cfg.enabled || !cfg.pr_review_enabled {
        tracing::info!(%delivery, "PR review disabled by repo config");
        return Ok(());
    }

    let pr = &ev.pull_request;
    let job = EventJob {
        kind: JobKind::PrReview,
        repo_full_name: ev.repository.full_name.clone(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number: pr.number,
        title: pr.title.clone(),
        body: pr.body.clone().unwrap_or_default(),
        author: pr.user.login.clone(),
        html_url: pr.html_url.clone(),
        base_ref: Some(pr.base.ref_name.clone()),
        base_sha: Some(pr.base.sha.clone()),
        head_ref: Some(pr.head.ref_name.clone()),
        head_sha: Some(pr.head.sha.clone()),
        config: cfg,
        model: state.cfg.gemini_model.clone(),
    };

    review::run(&state.cfg, &token, &job).await?;
    tracing::info!(%delivery, pr = pr.number, "reviewed PR");
    Ok(())
}

async fn dispatch_issue(state: &AppState, delivery: &str, body: &[u8]) -> Result<()> {
    let ev: IssueEvent = serde_json::from_slice(body)
        .map_err(|e| nogent_core::NogentError::Payload(format!("issues: {e}")))?;

    // Skip PRs that arrive on the issues event, and non-actionable actions.
    if ev.issue.pull_request.is_some() || !events::is_actionable_issue_action(&ev.action) {
        return Ok(());
    }
    let Some((owner, repo)) = ev.repository.owner_repo() else {
        return Ok(());
    };

    let token = state.auth.installation_token(ev.installation.id).await?;
    let cfg = resolve_config(&token, owner, repo).await?;
    if !cfg.enabled || !cfg.issue_triage_enabled {
        tracing::info!(%delivery, "issue triage disabled by repo config");
        return Ok(());
    }

    let issue = &ev.issue;
    let job = EventJob {
        kind: JobKind::IssueTriage,
        repo_full_name: ev.repository.full_name.clone(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number: issue.number,
        title: issue.title.clone(),
        body: issue.body.clone().unwrap_or_default(),
        author: issue.user.login.clone(),
        html_url: issue.html_url.clone(),
        base_ref: None,
        base_sha: None,
        head_ref: None,
        head_sha: None,
        config: cfg,
        model: state.cfg.gemini_model.clone(),
    };

    triage::run(&state.cfg, &token, &job).await?;
    tracing::info!(%delivery, issue = issue.number, "triaged issue");
    Ok(())
}

/// Fetch + resolve repo config. Fail-secure: a malformed config propagates as
/// an error and the caller aborts the event (no fallback to "enabled").
async fn resolve_config(token: &str, owner: &str, repo: &str) -> Result<ResolvedConfig> {
    let raw = github_mint::fetch_repo_config(token, owner, repo).await?;
    ResolvedConfig::resolve(raw.as_deref())
}
