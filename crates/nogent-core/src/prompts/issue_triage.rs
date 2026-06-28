//! Issue-triage prompts.
//!
//! System instruction is loaded from out-of-tree Markdown (see
//! [`crate::prompts`]); the user prompt is assembled here.

use crate::events::EventJob;

/// System instruction (role + guidelines + output contract), canary-substituted,
/// with optional maintainer guidance from the repo's `NOGENT.md` appended.
#[must_use]
pub fn system_instruction(canary: &str, repo_guidance: Option<&str>) -> String {
    crate::prompts::append_repo_guidance(crate::prompts::issue_triage_system(canary), repo_guidance)
}

#[must_use]
pub fn user_prompt(job: &EventJob) -> String {
    let body = job.body.trim();
    let body = if body.is_empty() {
        "(no description)"
    } else {
        body
    };
    format!(
        "Repository: {repo}\nIssue: {url}\nAuthor: {author}\n\n\
<untrusted_issue>\nTitle: {title}\n</untrusted_issue>\n\n\
<untrusted_body>\n{body}\n</untrusted_body>",
        repo = job.repo_full_name,
        url = job.html_url,
        author = job.author,
        title = job.title,
        body = body,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::JobKind;
    use crate::repo_config::ResolvedConfig;

    #[test]
    fn issue_user_prompt_tags_untrusted() {
        let job = EventJob {
            kind: JobKind::IssueTriage,
            repo_full_name: "o/r".into(),
            owner: "o".into(),
            repo: "r".into(),
            number: 9,
            title: "How do I allow /tmp?".into(),
            body: String::new(),
            author: "bob".into(),
            html_url: "https://x".into(),
            default_branch: "main".into(),
            base_ref: None,
            base_sha: None,
            head_ref: None,
            head_sha: None,
            config: ResolvedConfig::default(),
            model: "gemini-2.5-pro".into(),
        };
        let u = user_prompt(&job);
        assert!(u.contains("<untrusted_issue>"));
        assert!(u.contains("(no description)"));
    }
}
