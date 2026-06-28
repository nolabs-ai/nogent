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
    let canary = generate_canary();
    let system = issue_triage::system_instruction(&canary);
    let user = issue_triage::user_prompt(job);

    let gemini = GeminiClient::new(&cfg.gemini_api_key, &job.model)?;
    let raw = gemini.generate(&system, &user).await?;

    let body = match validate_issue_triage(&raw, &canary) {
        Some(out) => format_issue_triage_markdown(&out),
        None => {
            tracing::warn!(
                issue = job.number,
                "model output failed validation; posting fallback"
            );
            FALLBACK_MESSAGE.to_string()
        }
    };

    let gh = GithubClient::new(token)?;
    gh.post_issue_comment(&job.owner, &job.repo, job.number, &body)
        .await?;
    tracing::info!(issue = job.number, "posted issue triage");
    Ok(())
}
