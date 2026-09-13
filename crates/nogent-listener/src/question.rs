//! Question (source-agnostic Q&A triage) orchestration.
//!
//! Unlike PR review / issue triage, this path touches NO GitHub: no token, no
//! tarball, no `RepoIndex`, no comment. It grounds the answer solely on the
//! caller-supplied `context`, validates the model output against the run canary,
//! and RETURNS the answer JSON to the caller (the worker prints it to stdout;
//! the listener relays it in the HTTP response). Egress is Gemini-only.

use nogent_core::error::Result;
use nogent_core::events::EventJob;
use nogent_core::output_validator::{
    format_question_answer_json, generate_canary, question_answer_fallback_json,
    validate_question_answer,
};
use nogent_core::prompts::question;

use crate::gemini_client::GeminiClient;

/// Answer a question job. Returns the canary-validated answer JSON, or the fixed
/// fallback body on validation failure. Never returns model text unvalidated.
pub async fn run(
    gemini_api_key: &str,
    thinking_level: Option<&str>,
    job: &EventJob,
) -> Result<String> {
    let canary = generate_canary();
    let system = question::system_instruction(&canary);
    let user = question::user_prompt(job);

    // The set of source_ids the caller actually supplied — the validator filters
    // citations/duplicates to these so a hallucinated id can't reach the caller.
    let valid_source_ids: Vec<String> = job
        .question
        .as_ref()
        .map(|q| q.context.iter().map(|s| s.source_id.clone()).collect())
        .unwrap_or_default();

    let gemini = GeminiClient::new(gemini_api_key, &job.model, thinking_level)?;
    let raw = gemini.generate(&system, &user).await?;

    let body = match validate_question_answer(&raw, &canary, &valid_source_ids) {
        Some(out) => format_question_answer_json(&out),
        None => {
            tracing::warn!(
                raw = %raw.chars().take(6000).collect::<String>(),
                "question answer failed validation; returning fallback"
            );
            question_answer_fallback_json()
        }
    };

    let u = gemini.usage();
    tracing::info!(
        gemini_calls = u.calls,
        tokens_in = u.input_tokens,
        tokens_out = u.output_tokens,
        thinking_tokens = u.thinking_tokens,
        cached_tokens = u.cached_tokens,
        "answered question"
    );
    Ok(body)
}
