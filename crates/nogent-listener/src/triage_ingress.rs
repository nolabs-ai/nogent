//! Non-GitHub triage ingress: `POST /api/triage`.
//!
//! Unlike the webhook, there is no GitHub signature to verify, so this endpoint
//! authenticates with a shared secret (`NOGENT_TRIAGE_SECRET`) presented as a
//! bearer token, compared in constant time. It grounds the answer solely on
//! caller-supplied `context` (wrapped as untrusted + size-capped), runs the same
//! sandboxed worker as issue triage, and RETURNS the canary-validated answer.
//!
//! Fail-secure: if the secret is unset the endpoint is disabled (404).

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use nogent_core::events::{ContextSource, ConversationTurn, EventJob, QuestionInput};
use nogent_core::hmac_verify::constant_time_eq;
use serde::Deserialize;

use crate::webhook::AppState;
use crate::worker;

/// Upper bound on the number of context sources. Total serialized context bytes
/// are separately bounded by `NOGENT_TRIAGE_MAX_CONTEXT_BYTES`.
const MAX_CONTEXT_SOURCES: usize = 50;
/// Bounds on the question + conversation (the whole body is already limited by
/// the server's request-body layer; these keep individual fields sane).
const MAX_QUESTION_CHARS: usize = 4_000;
const MAX_CONVERSATION_TURNS: usize = 50;
const MAX_TURN_CHARS: usize = 8_000;
/// Accepted conversation roles and context trust levels.
const ROLES: [&str; 2] = ["user", "assistant"];
const TRUST_LEVELS: [&str; 3] = ["verified", "community", "unverified"];

#[derive(Debug, Deserialize)]
struct TriageRequest {
    #[serde(default)]
    question: String,
    #[serde(default)]
    conversation: Vec<ConversationTurn>,
    #[serde(default)]
    context: Vec<ContextSource>,
}

/// POST /api/triage
pub async fn handle_triage(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    // 1. Auth — fail-secure when the endpoint is not configured.
    let Some(secret) = state.cfg.triage_ingress_secret.as_ref() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !bearer_authorized(&headers, secret) {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // 2. Parse + bound the request.
    let req: TriageRequest = match serde_json::from_slice(&body) {
        Ok(req) => req,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid JSON body: {e}")).into_response();
        }
    };
    if let Err((code, msg)) = validate_request(&req, state.cfg.triage_max_context_bytes) {
        return (code, msg).into_response();
    }

    // 3. Backpressure: share the worker pool with the webhook path.
    let Ok(worker_slot) = state.worker_slots.clone().try_acquire_owned() else {
        tracing::warn!("worker capacity exhausted; rejecting triage request");
        return StatusCode::TOO_MANY_REQUESTS.into_response();
    };

    let job = EventJob::question(
        QuestionInput {
            question: req.question,
            conversation: req.conversation,
            context: req.context,
        },
        state.cfg.gemini_model.clone(),
    );

    let result = worker::run_question(&state.cfg, &job).await;
    drop(worker_slot);

    match result {
        Ok(answer) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            answer,
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(error = %e, "triage question job failed");
            StatusCode::BAD_GATEWAY.into_response()
        }
    }
}

/// Constant-time bearer-token check against the configured secret.
fn bearer_authorized(headers: &HeaderMap, secret: &str) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(token) = value.strip_prefix("Bearer ") else {
        return false;
    };
    constant_time_eq(token.as_bytes(), secret.as_bytes())
}

/// Enforce field + context bounds before building a job. All fields are
/// untrusted; the question/conversation are char-capped and the `context` is
/// capped by count and total serialized bytes. Roles and trust levels must be
/// in-contract.
fn validate_request(
    req: &TriageRequest,
    max_context_bytes: usize,
) -> std::result::Result<(), (StatusCode, String)> {
    if req.question.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "question is required".into()));
    }
    if req.question.chars().count() > MAX_QUESTION_CHARS {
        return Err((StatusCode::PAYLOAD_TOO_LARGE, "question too long".into()));
    }

    if req.conversation.len() > MAX_CONVERSATION_TURNS {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("too many conversation turns (max {MAX_CONVERSATION_TURNS})"),
        ));
    }
    for turn in &req.conversation {
        if !ROLES.contains(&turn.role.trim().to_ascii_lowercase().as_str()) {
            return Err((
                StatusCode::BAD_REQUEST,
                "conversation role must be 'user' or 'assistant'".into(),
            ));
        }
        if turn.content.chars().count() > MAX_TURN_CHARS {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "conversation turn too long".into(),
            ));
        }
    }

    if req.context.len() > MAX_CONTEXT_SOURCES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("too many context sources (max {MAX_CONTEXT_SOURCES})"),
        ));
    }
    let mut context_bytes = 0usize;
    for s in &req.context {
        if s.source_id.trim().is_empty() {
            return Err((
                StatusCode::BAD_REQUEST,
                "each context source needs a source_id".into(),
            ));
        }
        if !s.trust.is_empty()
            && !TRUST_LEVELS.contains(&s.trust.trim().to_ascii_lowercase().as_str())
        {
            return Err((
                StatusCode::BAD_REQUEST,
                "context trust must be 'verified', 'community', or 'unverified'".into(),
            ));
        }
        context_bytes += s.source_id.len()
            + s.source_type.len()
            + s.title.len()
            + s.url.len()
            + s.version.len()
            + s.content.len()
            + s.trust.len();
    }
    if context_bytes > max_context_bytes {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("context exceeds {max_context_bytes} bytes"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str, content_len: usize) -> ContextSource {
        ContextSource {
            source_id: id.into(),
            source_type: "doc".into(),
            title: String::new(),
            url: String::new(),
            version: String::new(),
            content: "x".repeat(content_len),
            trust: "verified".into(),
        }
    }

    fn req(question: &str, sources: usize, content_len: usize) -> TriageRequest {
        TriageRequest {
            question: question.into(),
            conversation: vec![],
            context: (0..sources)
                .map(|i| source(&format!("s{i}"), content_len))
                .collect(),
        }
    }

    #[test]
    fn requires_non_empty_question() {
        assert!(validate_request(&req("  ", 0, 0), 1024).is_err());
        assert!(validate_request(&req("hi", 0, 0), 1024).is_ok());
    }

    #[test]
    fn rejects_oversized_context() {
        // 3 sources * 1000 bytes each > 2048 cap.
        let err = validate_request(&req("q", 3, 1000), 2048).unwrap_err();
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn rejects_too_many_sources() {
        let err = validate_request(&req("q", MAX_CONTEXT_SOURCES + 1, 1), usize::MAX).unwrap_err();
        assert_eq!(err.0, StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[test]
    fn rejects_source_without_id() {
        let mut r = req("q", 1, 1);
        r.context[0].source_id = "  ".into();
        assert_eq!(
            validate_request(&r, usize::MAX).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn rejects_bad_role_and_bad_trust() {
        let mut r = req("q", 0, 0);
        r.conversation = vec![ConversationTurn {
            role: "system".into(),
            content: "x".into(),
            name: String::new(),
        }];
        assert_eq!(
            validate_request(&r, usize::MAX).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );

        let mut r = req("q", 1, 1);
        r.context[0].trust = "gospel".into();
        assert_eq!(
            validate_request(&r, usize::MAX).unwrap_err().0,
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn bearer_auth_requires_exact_secret() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, "Bearer topsecret".parse().unwrap());
        assert!(bearer_authorized(&headers, "topsecret"));
        assert!(!bearer_authorized(&headers, "wrong"));

        let mut bad = HeaderMap::new();
        bad.insert(header::AUTHORIZATION, "topsecret".parse().unwrap()); // no scheme
        assert!(!bearer_authorized(&bad, "topsecret"));

        assert!(!bearer_authorized(&HeaderMap::new(), "topsecret"));
    }
}
