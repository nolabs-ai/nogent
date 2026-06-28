//! Issue-triage orchestration (runs in-process in the listener).

use nogent_core::error::Result;
use nogent_core::events::EventJob;
use nogent_core::output_validator::{
    FALLBACK_MESSAGE, format_issue_triage_markdown, generate_canary, validate_issue_triage,
};
use nogent_core::prompts::issue_triage;

use crate::config::ListenerConfig;
use crate::gemini_client::GeminiClient;
use crate::github_client::GithubClient;

pub async fn run(cfg: &ListenerConfig, token: &str, job: &EventJob) -> Result<()> {
    let gh = GithubClient::new(token)?;

    // Maintainer guidance from NOGENT.md on the default branch (trusted).
    let guidance = if job.default_branch.is_empty() {
        None
    } else {
        gh.get_repo_guidance(&job.owner, &job.repo, &job.default_branch, 16_384)
            .await?
    };

    let canary = generate_canary();
    let system = issue_triage::system_instruction(&canary, guidance.as_deref());
    let user = issue_triage::user_prompt(job);

    let gemini = GeminiClient::new(
        &cfg.gemini_api_key,
        &job.model,
        cfg.gemini_thinking_level.as_deref(),
    )?;
    let raw = gemini.generate(&system, &user).await?;

    let body = match validate_issue_triage(&raw, &canary) {
        Some(out) => format_issue_triage_markdown(&out),
        None => {
            tracing::warn!(
                issue = job.number,
                raw = %raw.chars().take(6000).collect::<String>(),
                "model output failed validation; posting fallback"
            );
            FALLBACK_MESSAGE.to_string()
        }
    };

    gh.post_issue_comment(&job.owner, &job.repo, job.number, &body)
        .await?;
    let u = gemini.usage();
    tracing::info!(
        issue = job.number,
        gemini_calls = u.calls,
        tokens_in = u.input_tokens,
        tokens_out = u.output_tokens,
        thinking_tokens = u.thinking_tokens,
        cached_tokens = u.cached_tokens,
        "posted issue triage"
    );
    Ok(())
}
