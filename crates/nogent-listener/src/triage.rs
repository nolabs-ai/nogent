//! Issue-triage orchestration (runs in-process in the listener).

use nogent_core::error::Result;
use nogent_core::events::EventJob;
use nogent_core::output_validator::{
    ISSUE_TRIAGE_MARKER, format_issue_triage_fallback_markdown, format_issue_triage_markdown,
    generate_canary, validate_issue_triage,
};
use nogent_core::prompts::issue_triage;

use crate::api_base::WorkerApiBases;
use crate::gemini_client::GeminiClient;
use crate::github_client::GithubClient;

pub async fn run(
    gemini_api_key: &str,
    thinking_level: Option<&str>,
    token: &str,
    job: &EventJob,
    api_bases: Option<&WorkerApiBases>,
) -> Result<()> {
    let gh = match api_bases {
        Some(bases) => GithubClient::new_for_worker(token, &bases.github)?,
        None => GithubClient::new(token)?,
    };

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

    let gemini = match api_bases {
        Some(bases) => {
            GeminiClient::new_for_worker(gemini_api_key, &job.model, thinking_level, &bases.gemini)?
        }
        None => GeminiClient::new(gemini_api_key, &job.model, thinking_level)?,
    };
    let raw = gemini.generate(&system, &user).await?;

    let body = match validate_issue_triage(&raw, &canary) {
        Some(out) => format_issue_triage_markdown(&out),
        None => {
            tracing::warn!(
                issue = job.number,
                raw = %raw.chars().take(6000).collect::<String>(),
                "model output failed validation; posting fallback"
            );
            format_issue_triage_fallback_markdown()
        }
    };

    post_or_update_triage_comment(&gh, job, &body).await?;
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

async fn post_or_update_triage_comment(
    gh: &GithubClient,
    job: &EventJob,
    body: &str,
) -> Result<()> {
    let comments = gh
        .list_issue_comments(&job.owner, &job.repo, job.number)
        .await?;
    let triage_comments = comments
        .into_iter()
        .filter(|c| c.user.is_bot() && is_issue_triage_comment(&c.body))
        .collect::<Vec<_>>();

    let Some((first, duplicates)) = triage_comments.split_first() else {
        gh.post_issue_comment(&job.owner, &job.repo, job.number, body)
            .await?;
        return Ok(());
    };

    gh.update_issue_comment(&job.owner, &job.repo, first.id, body)
        .await?;
    for duplicate in duplicates {
        if let Err(err) = gh
            .delete_issue_comment(&job.owner, &job.repo, duplicate.id)
            .await
        {
            tracing::warn!(
                issue = job.number,
                comment_id = duplicate.id,
                error = %err,
                "failed to delete duplicate issue triage comment"
            );
        }
    }
    Ok(())
}

fn is_issue_triage_comment(body: &str) -> bool {
    body.contains(ISSUE_TRIAGE_MARKER) || body.contains("## 🛡️ nogent issue triage")
}

#[cfg(test)]
mod tests {
    use super::is_issue_triage_comment;
    use nogent_core::output_validator::ISSUE_TRIAGE_MARKER;

    #[test]
    fn detects_current_marker() {
        assert!(is_issue_triage_comment(&format!(
            "{ISSUE_TRIAGE_MARKER}\n\n## 🛡️ nogent issue triage"
        )));
    }

    #[test]
    fn detects_legacy_heading() {
        assert!(is_issue_triage_comment(
            "## 🛡️ nogent issue triage\n\n**Assessment:** needs-code-change"
        ));
    }

    #[test]
    fn ignores_other_bot_comments() {
        assert!(!is_issue_triage_comment(
            "## unrelated\n\nAutomated triage suggestion."
        ));
    }
}
