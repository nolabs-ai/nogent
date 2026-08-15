//! Webhook receiver: verify, filter, enqueue, and dispatch.
//!
//! The HTTP path verifies HMAC and performs payload-only filtering before
//! placing an actionable delivery on a bounded in-memory queue. Queue workers
//! perform token minting, config fetches, and model calls after the 202 reply.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use nogent_core::error::Result;
use nogent_core::events::{
    self, EventJob, IssueCommentEvent, IssueEvent, JobKind, PullRequestEvent,
};
use nogent_core::hmac_verify::verify_signature;
use nogent_core::repo_config::ResolvedConfig;
use tokio::sync::{Semaphore, mpsc};

use crate::app_auth::AppAuth;
use crate::config::ListenerConfig;
use crate::{github_mint, worker};

#[derive(Clone)]
pub struct AppState {
    pub cfg: Arc<ListenerConfig>,
    pub auth: Arc<AppAuth>,
    pub job_tx: mpsc::Sender<QueuedDelivery>,
}

pub struct QueuedDelivery {
    delivery: String,
    event: QueuedEvent,
}

enum QueuedEvent {
    Pr(PullRequestEvent),
    Issue(IssueEvent),
    Command(IssueCommentEvent),
}

const SIG_HEADER: &str = "x-hub-signature-256";
const EVENT_HEADER: &str = "x-github-event";
const DELIVERY_HEADER: &str = "x-github-delivery";

/// Start the bounded-concurrency queue consumer. The receiver deliberately
/// waits for a worker slot before taking the next queued item, so queue
/// capacity accurately represents outstanding work waiting to start.
pub fn start_dispatcher(
    state: AppState,
    mut jobs: mpsc::Receiver<QueuedDelivery>,
) -> tokio::task::JoinHandle<()> {
    let worker_slots = Arc::new(Semaphore::new(state.cfg.max_concurrent_jobs));
    tokio::spawn(async move {
        while let Some(queued) = jobs.recv().await {
            let Ok(worker_slot) = worker_slots.clone().acquire_owned().await else {
                tracing::error!("worker semaphore closed; stopping webhook dispatcher");
                return;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let _worker_slot = worker_slot;
                let delivery = queued.delivery.clone();
                if let Err(error) = dispatch(&state, queued).await {
                    tracing::warn!(%delivery, %error, "event dispatch failed");
                }
            });
        }
    })
}

/// POST /api/github/webhooks
pub async fn handle_webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let sig = headers
        .get(SIG_HEADER)
        .and_then(|value| value.to_str().ok());
    let Some(sig) = sig else {
        return StatusCode::UNAUTHORIZED;
    };
    if !verify_signature(state.cfg.webhook_secret.as_bytes(), &body, sig) {
        return StatusCode::UNAUTHORIZED;
    }

    let event = headers
        .get(EVENT_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let delivery = headers
        .get(DELIVERY_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();

    if event == "ping" {
        return StatusCode::OK;
    }
    if !matches!(event, "pull_request" | "issues" | "issue_comment") {
        tracing::debug!(%delivery, %event, "ignoring unsubscribed event");
        return StatusCode::NO_CONTENT;
    }

    let queued = match classify_delivery(&state.cfg, event, &delivery, &body) {
        Ok(Some(queued)) => queued,
        Ok(None) => return StatusCode::NO_CONTENT,
        Err(error) => {
            tracing::warn!(%delivery, %event, %error, "invalid signed webhook payload");
            return StatusCode::BAD_REQUEST;
        }
    };

    match state.job_tx.try_send(queued) {
        Ok(()) => StatusCode::ACCEPTED,
        Err(mpsc::error::TrySendError::Full(_)) => {
            // GitHub does not automatically retry failed webhook deliveries.
            // A 503 makes the failure visible in delivery history; operators
            // must manually redeliver it (or run an automated redelivery job).
            tracing::error!(
                %delivery,
                %event,
                "job queue is full; delivery was not accepted and must be redelivered"
            );
            StatusCode::SERVICE_UNAVAILABLE
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            tracing::error!(%delivery, %event, "job dispatcher is unavailable");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

fn classify_delivery(
    cfg: &ListenerConfig,
    event: &str,
    delivery: &str,
    body: &[u8],
) -> Result<Option<QueuedDelivery>> {
    let event = match event {
        "pull_request" => {
            let ev: PullRequestEvent = serde_json::from_slice(body).map_err(|error| {
                nogent_core::NogentError::Payload(format!("pull_request: {error}"))
            })?;
            if !events::is_actionable_pr_action(&ev.action) || ev.pull_request.draft {
                tracing::debug!(
                    %delivery,
                    action = %ev.action,
                    draft = ev.pull_request.draft,
                    "skipping PR"
                );
                return Ok(None);
            }
            if ev.pull_request.user.is_bot() {
                tracing::info!(
                    %delivery,
                    author = %ev.pull_request.user.login,
                    "skipping bot-authored PR"
                );
                return Ok(None);
            }
            let Some((owner, _)) = ev.repository.owner_repo() else {
                tracing::warn!(
                    %delivery,
                    full_name = %ev.repository.full_name,
                    "bad repo full_name"
                );
                return Ok(None);
            };
            if !cfg.allowlist_permits(ev.installation.id, owner) {
                tracing::info!(
                    %delivery,
                    %owner,
                    installation = ev.installation.id,
                    "blocked by allowlist"
                );
                return Ok(None);
            }
            QueuedEvent::Pr(ev)
        }
        "issues" => {
            let ev: IssueEvent = serde_json::from_slice(body)
                .map_err(|error| nogent_core::NogentError::Payload(format!("issues: {error}")))?;
            if ev.issue.pull_request.is_some() || !events::is_actionable_issue_action(&ev.action) {
                return Ok(None);
            }
            let Some((owner, _)) = ev.repository.owner_repo() else {
                return Ok(None);
            };
            if !cfg.allowlist_permits(ev.installation.id, owner) {
                tracing::info!(
                    %delivery,
                    %owner,
                    installation = ev.installation.id,
                    "blocked by allowlist"
                );
                return Ok(None);
            }
            QueuedEvent::Issue(ev)
        }
        "issue_comment" => {
            let ev: IssueCommentEvent = serde_json::from_slice(body).map_err(|error| {
                nogent_core::NogentError::Payload(format!("issue_comment: {error}"))
            })?;
            if !matches!(ev.action.as_str(), "created" | "edited")
                || ev.issue.pull_request.is_none()
                || !events::is_review_command(&ev.comment.body)
                || ev.comment.user.is_bot()
            {
                return Ok(None);
            }
            let Some((owner, _)) = ev.repository.owner_repo() else {
                return Ok(None);
            };
            if !cfg.allowlist_permits(ev.installation.id, owner) {
                tracing::info!(
                    %delivery,
                    %owner,
                    installation = ev.installation.id,
                    "blocked by allowlist"
                );
                return Ok(None);
            }
            QueuedEvent::Command(ev)
        }
        _ => return Ok(None),
    };

    Ok(Some(QueuedDelivery {
        delivery: delivery.to_string(),
        event,
    }))
}

async fn dispatch(state: &AppState, queued: QueuedDelivery) -> Result<()> {
    match queued.event {
        QueuedEvent::Pr(event) => dispatch_pr(state, &queued.delivery, event).await,
        QueuedEvent::Issue(event) => dispatch_issue(state, &queued.delivery, event).await,
        QueuedEvent::Command(event) => dispatch_command(state, &queued.delivery, event).await,
    }
}

/// Build the per-PR review job from a fetched/received pull request.
fn pr_job(
    repo: &nogent_core::events::Repository,
    pr: &nogent_core::events::PullRequest,
    cfg: ResolvedConfig,
    model: String,
) -> Option<EventJob> {
    let (owner, name) = repo.owner_repo()?;
    Some(EventJob {
        kind: JobKind::PrReview,
        repo_full_name: repo.full_name.clone(),
        owner: owner.to_string(),
        repo: name.to_string(),
        number: pr.number,
        title: pr.title.clone(),
        body: pr.body.clone().unwrap_or_default(),
        author: pr.user.login.clone(),
        html_url: pr.html_url.clone(),
        default_branch: repo.default_branch.clone(),
        base_ref: Some(pr.base.ref_name.clone()),
        base_sha: Some(pr.base.sha.clone()),
        head_ref: Some(pr.head.ref_name.clone()),
        head_sha: Some(pr.head.sha.clone()),
        config: cfg,
        model,
    })
}

async fn dispatch_command(state: &AppState, delivery: &str, ev: IssueCommentEvent) -> Result<()> {
    let Some((owner, repo)) = ev.repository.owner_repo() else {
        return Ok(());
    };
    let token = state
        .auth
        .installation_token(ev.installation.id, owner, repo)
        .await?;
    let cfg = resolve_config(&token, owner, repo).await?;
    if !cfg.enabled || !cfg.pr_review_enabled {
        return Ok(());
    }

    // Fetch the PR for fresh head/base SHAs (the comment payload lacks them).
    // This is a trusted-listener direct client; it never honors worker base URLs.
    let gh = crate::github_client::GithubClient::new(&token)?;
    let pr = gh.get_pull_request(owner, repo, ev.issue.number).await?;
    let Some(job) = pr_job(&ev.repository, &pr, cfg, state.cfg.gemini_model.clone()) else {
        return Ok(());
    };

    worker::run(&state.cfg, &token, &job).await?;
    tracing::info!(%delivery, pr = ev.issue.number, "reviewed PR on /nogent review");
    Ok(())
}

async fn dispatch_pr(state: &AppState, delivery: &str, ev: PullRequestEvent) -> Result<()> {
    let Some((owner, repo)) = ev.repository.owner_repo() else {
        return Ok(());
    };
    let token = state
        .auth
        .installation_token(ev.installation.id, owner, repo)
        .await?;
    let cfg = resolve_config(&token, owner, repo).await?;
    if !cfg.enabled || !cfg.pr_review_enabled {
        tracing::info!(%delivery, "PR review disabled by repo config");
        return Ok(());
    }

    let number = ev.pull_request.number;
    let Some(job) = pr_job(
        &ev.repository,
        &ev.pull_request,
        cfg,
        state.cfg.gemini_model.clone(),
    ) else {
        return Ok(());
    };
    worker::run(&state.cfg, &token, &job).await?;
    tracing::info!(%delivery, pr = number, "reviewed PR");
    Ok(())
}

async fn dispatch_issue(state: &AppState, delivery: &str, ev: IssueEvent) -> Result<()> {
    let Some((owner, repo)) = ev.repository.owner_repo() else {
        return Ok(());
    };
    let token = state
        .auth
        .installation_token(ev.installation.id, owner, repo)
        .await?;
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
        default_branch: ev.repository.default_branch.clone(),
        base_ref: None,
        base_sha: None,
        head_ref: None,
        head_sha: None,
        config: cfg,
        model: state.cfg.gemini_model.clone(),
    };
    worker::run(&state.cfg, &token, &job).await?;
    tracing::info!(%delivery, issue = issue.number, "triaged issue");
    Ok(())
}

/// Fetch + resolve repo config. Fail-secure: a malformed config propagates as
/// an error and the caller aborts the event (no fallback to "enabled").
async fn resolve_config(token: &str, owner: &str, repo: &str) -> Result<ResolvedConfig> {
    let raw = github_mint::fetch_repo_config(token, owner, repo).await?;
    ResolvedConfig::resolve(raw.as_deref())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use zeroize::Zeroizing;

    use super::*;
    use crate::config::WorkerIsolation;

    fn config() -> ListenerConfig {
        ListenerConfig {
            app_id: "1".into(),
            private_key_pem: Zeroizing::new(String::new()),
            webhook_secret: Zeroizing::new("secret".into()),
            gemini_api_key: Zeroizing::new("gemini".into()),
            gemini_model: "model".into(),
            gemini_thinking_level: None,
            bind_addr: String::new(),
            max_body_bytes: 2_097_152,
            allowed_owners: Vec::new(),
            allowed_installations: Vec::new(),
            worker_isolation: WorkerIsolation::InProcess,
            max_concurrent_jobs: 2,
            max_queued_jobs: 100,
            worker_timeout: Duration::from_secs(900),
            prompts_dir: None,
        }
    }

    #[test]
    fn no_op_pr_is_filtered_before_queueing() {
        let raw = br#"{
            "action":"synchronize",
            "installation":{"id":1},
            "repository":{"full_name":"o/r","default_branch":"main"},
            "pull_request":{
                "number":7,"title":"t","draft":false,"html_url":"https://x",
                "user":{"login":"alice","type":"User"},
                "head":{"ref":"feature","sha":"abc"},
                "base":{"ref":"main","sha":"def"}
            }
        }"#;
        assert!(
            classify_delivery(&config(), "pull_request", "d1", raw)
                .expect("valid")
                .is_none()
        );
    }

    #[test]
    fn actionable_pr_is_queued_after_payload_filtering() {
        let raw = br#"{
            "action":"opened",
            "installation":{"id":1},
            "repository":{"full_name":"o/r","default_branch":"main"},
            "pull_request":{
                "number":7,"title":"t","draft":false,"html_url":"https://x",
                "user":{"login":"alice","type":"User"},
                "head":{"ref":"feature","sha":"abc"},
                "base":{"ref":"main","sha":"def"}
            }
        }"#;
        assert!(
            classify_delivery(&config(), "pull_request", "d1", raw)
                .expect("valid")
                .is_some()
        );
    }

    #[test]
    fn bot_comment_is_filtered_before_queueing() {
        let raw = br#"{
            "action":"created",
            "installation":{"id":1},
            "repository":{"full_name":"o/r","default_branch":"main"},
            "issue":{
                "number":7,"title":"t","html_url":"https://x",
                "user":{"login":"alice"},"pull_request":{"url":"https://api"}
            },
            "comment":{"body":"/nogent review","user":{"login":"nogent[bot]","type":"Bot"}}
        }"#;
        assert!(
            classify_delivery(&config(), "issue_comment", "d1", raw)
                .expect("valid")
                .is_none()
        );
    }
}
