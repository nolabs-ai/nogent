//! PR security-review orchestration (runs in-process in the listener).

use nogent_core::diff_digest::build_digest;
use nogent_core::error::Result;
use nogent_core::events::EventJob;
use nogent_core::output_validator::{
    FALLBACK_MESSAGE, format_pr_review_markdown, generate_canary, validate_pr_review,
};
use nogent_core::prompts::pr_review;

use crate::config::ListenerConfig;
use crate::gemini_client::GeminiClient;
use crate::github_client::GithubClient;

pub async fn run(cfg: &ListenerConfig, token: &str, job: &EventJob) -> Result<()> {
    let gh = GithubClient::new(token)?;
    let files = gh.list_pr_files(&job.owner, &job.repo, job.number).await?;
    let digest = build_digest(&files, job.config.max_files, job.config.max_patch_bytes);

    let canary = generate_canary();
    let system = pr_review::system_instruction(&canary);
    let user = pr_review::user_prompt(job, &digest);

    let gemini = GeminiClient::new(&cfg.gemini_api_key, &job.model)?;
    let raw = gemini.generate(&system, &user).await?;

    let body = match validate_pr_review(&raw, &canary) {
        Some(out) => format_pr_review_markdown(&out),
        None => {
            tracing::warn!(
                pr = job.number,
                "model output failed validation; posting fallback"
            );
            FALLBACK_MESSAGE.to_string()
        }
    };
    gh.post_pr_review(&job.owner, &job.repo, job.number, &body)
        .await?;
    tracing::info!(
        pr = job.number,
        files = digest.files_included,
        "posted PR review"
    );
    Ok(())
}
