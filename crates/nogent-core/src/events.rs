//! Minimal subsets of the GitHub webhook payloads we consume, plus the
//! `EventJob` the listener builds from them for the review/triage orchestration.
//!
//! We deserialize only the fields we use. `serde` ignores unknown fields by
//! default, so this stays resilient to GitHub adding payload keys.

use serde::{Deserialize, Serialize};

use crate::repo_config::ResolvedConfig;

#[derive(Debug, Clone, Deserialize)]
pub struct Installation {
    pub id: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Actor {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    /// e.g. "nolabs-ai/nono"
    pub full_name: String,
    #[serde(default)]
    pub default_branch: String,
}

impl Repository {
    /// Split `full_name` into (owner, repo). Returns `None` if the shape is
    /// not exactly `owner/repo`.
    #[must_use]
    pub fn owner_repo(&self) -> Option<(&str, &str)> {
        let mut parts = self.full_name.splitn(2, '/');
        match (parts.next(), parts.next()) {
            (Some(owner), Some(repo)) if !owner.is_empty() && !repo.is_empty() => {
                Some((owner, repo))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    pub html_url: String,
    pub user: Actor,
    pub head: GitRef,
    pub base: GitRef,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PullRequestEvent {
    pub action: String,
    pub installation: Installation,
    pub repository: Repository,
    pub pull_request: PullRequest,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    pub html_url: String,
    pub user: Actor,
    /// Present (and non-null) when the "issue" is really a pull request. We
    /// skip those on the `issues` event to avoid double-handling.
    #[serde(default)]
    pub pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueEvent {
    pub action: String,
    pub installation: Installation,
    pub repository: Repository,
    pub issue: Issue,
}

/// PR actions we auto-review on. NOT `synchronize` — we review once when a PR
/// appears; pushes are re-reviewed on demand via the `/nogent review` comment
/// command instead of on every push.
#[must_use]
pub fn is_actionable_pr_action(action: &str) -> bool {
    matches!(action, "opened" | "reopened" | "ready_for_review")
}

/// A comment on an issue or PR (from the `issue_comment` event).
#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    #[serde(default)]
    pub body: String,
    pub user: Actor,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IssueCommentEvent {
    pub action: String,
    pub installation: Installation,
    pub repository: Repository,
    pub issue: Issue,
    pub comment: Comment,
}

/// True if any line of the comment is exactly the `/nogent review` command.
#[must_use]
pub fn is_review_command(body: &str) -> bool {
    body.lines().any(|l| l.trim() == "/nogent review")
}

/// Issue actions we act on.
#[must_use]
pub fn is_actionable_issue_action(action: &str) -> bool {
    matches!(action, "opened" | "edited" | "reopened")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    PrReview,
    IssueTriage,
}

/// A normalized per-event unit of work: event facts + resolved config + the
/// model name. Built by the listener from a webhook payload and handed to the
/// review/triage orchestration. Carries no credentials.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventJob {
    pub kind: JobKind,
    pub repo_full_name: String,
    pub owner: String,
    pub repo: String,
    pub number: u64,
    pub title: String,
    pub body: String,
    pub author: String,
    pub html_url: String,
    /// Repo default branch — used to read maintainer-authored guidance
    /// (`NOGENT.md`) from a trusted ref for issue triage.
    pub default_branch: String,
    /// PR-only fields (None for issue triage).
    pub base_ref: Option<String>,
    pub base_sha: Option<String>,
    pub head_ref: Option<String>,
    pub head_sha: Option<String>,
    pub config: ResolvedConfig,
    pub model: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_repo_splits() {
        let r = Repository {
            full_name: "nolabs-ai/nono".to_string(),
            default_branch: "main".to_string(),
        };
        assert_eq!(r.owner_repo(), Some(("nolabs-ai", "nono")));
    }

    #[test]
    fn owner_repo_rejects_bad_shape() {
        for bad in ["nono", "/nono", "owner/", ""] {
            let r = Repository {
                full_name: bad.to_string(),
                default_branch: String::new(),
            };
            assert_eq!(r.owner_repo(), None, "should reject {bad:?}");
        }
    }

    #[test]
    fn pr_payload_subset_parses_and_ignores_extra() {
        let raw = r#"{
            "action": "opened",
            "extra_top_level": 1,
            "installation": {"id": 42, "node_id": "x"},
            "repository": {"full_name": "o/r", "default_branch": "main", "private": false},
            "pull_request": {
                "number": 7, "title": "t", "body": null, "draft": false,
                "html_url": "https://x", "user": {"login": "alice", "id": 1},
                "head": {"ref": "feature", "sha": "abc"},
                "base": {"ref": "main", "sha": "def"}
            }
        }"#;
        let ev: PullRequestEvent = serde_json::from_str(raw).expect("parse");
        assert_eq!(ev.action, "opened");
        assert_eq!(ev.installation.id, 42);
        assert_eq!(ev.pull_request.number, 7);
        assert_eq!(ev.pull_request.user.login, "alice");
        assert_eq!(ev.pull_request.head.ref_name, "feature");
        assert!(ev.pull_request.body.is_none());
    }

    #[test]
    fn issue_pr_marker_detected() {
        let raw = r#"{
            "action": "opened",
            "installation": {"id": 1},
            "repository": {"full_name": "o/r"},
            "issue": {"number": 3, "title": "t", "html_url": "https://x",
                      "user": {"login": "bob"}, "pull_request": {"url": "https://api"}}
        }"#;
        let ev: IssueEvent = serde_json::from_str(raw).expect("parse");
        assert!(ev.issue.pull_request.is_some());
    }
}
