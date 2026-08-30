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
    /// GitHub user type — typically `"User"` or `"Bot"`. Missing on some
    /// payload shapes (older fixtures, embedded users), treated as `"User"`.
    #[serde(default, rename = "type")]
    pub user_type: String,
}

impl Actor {
    /// True if this actor is a GitHub App / Bot account — used to skip
    /// auto-review of dependabot/renovate/etc. PRs by default.
    #[must_use]
    pub fn is_bot(&self) -> bool {
        self.user_type.eq_ignore_ascii_case("bot")
    }
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
    /// Source-agnostic Q&A triage. Grounded solely on caller-supplied
    /// `context`; needs no GitHub token, fetches no tarball/`RepoIndex`, and
    /// returns its answer to the caller instead of posting a comment.
    Question,
}

/// One caller-supplied grounding source for a [`JobKind::Question`] run. The
/// caller (which owns retrieval/memory) supplies retrieved KB/doc/issue chunks,
/// each identified by `source_id` so the model can cite it reliably. ALL fields
/// are UNTRUSTED — they may echo user content — and are wrapped in
/// untrusted-content framing by the question prompt. `trust` is the one signal
/// the model is told to weight (verified over community over unverified).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextSource {
    /// Stable identifier the model must echo in `citations`/`possible_duplicates`
    /// (e.g. `github:issue:123`). Answers referencing an id not present here are
    /// rejected by the validator.
    pub source_id: String,
    /// Caller-defined taxonomy label (e.g. `closed_issue`, `doc`, `discussion`).
    /// Free-form — a hint to the model, not constrained by nogent.
    #[serde(default)]
    pub source_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    /// Product/doc version this source pertains to, for version-aware answers.
    #[serde(default)]
    pub version: String,
    pub content: String,
    /// Canonicality signal: `verified` | `community` | `unverified`.
    #[serde(default)]
    pub trust: String,
}

/// One turn of prior conversation for a [`JobKind::Question`] run. `role` is the
/// source-agnostic dialogue position (`user` | `assistant`); speaker authority
/// is NOT encoded here — it belongs on [`ContextSource::trust`]. UNTRUSTED.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    /// `user` (asker) or `assistant` (a prior reply). No `system` role: caller
    /// content is never treated as authoritative instructions.
    pub role: String,
    pub content: String,
    /// Optional display name; a label only, never weighted by the model.
    #[serde(default)]
    pub name: String,
}

/// The full source-agnostic input for a [`JobKind::Question`] run. The caller
/// owns retrieval and dialogue assembly; nogent grounds solely on this plus
/// model knowledge.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QuestionInput {
    pub question: String,
    #[serde(default)]
    pub conversation: Vec<ConversationTurn>,
    #[serde(default)]
    pub context: Vec<ContextSource>,
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
    /// Caller-supplied input for [`JobKind::Question`] jobs (`None` for
    /// GitHub-derived jobs). Defaulted so existing PR/issue payloads still
    /// deserialize unchanged.
    #[serde(default)]
    pub question: Option<QuestionInput>,
    pub config: ResolvedConfig,
    pub model: String,
}

impl EventJob {
    /// Build a source-agnostic [`JobKind::Question`] job. Carries no GitHub
    /// identity (owner/repo/number/URLs are empty) — grounding comes solely
    /// from the [`QuestionInput`].
    #[must_use]
    pub fn question(input: QuestionInput, model: String) -> Self {
        EventJob {
            kind: JobKind::Question,
            repo_full_name: String::new(),
            owner: String::new(),
            repo: String::new(),
            number: 0,
            title: String::new(),
            body: String::new(),
            author: String::new(),
            html_url: String::new(),
            default_branch: String::new(),
            base_ref: None,
            base_sha: None,
            head_ref: None,
            head_sha: None,
            question: Some(input),
            config: ResolvedConfig::default(),
            model,
        }
    }
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
    fn actor_type_drives_is_bot() {
        let human: Actor = serde_json::from_str(r#"{"login":"alice","type":"User"}"#).unwrap();
        let bot: Actor =
            serde_json::from_str(r#"{"login":"dependabot[bot]","type":"Bot"}"#).unwrap();
        let bot_lower: Actor = serde_json::from_str(r#"{"login":"x","type":"bot"}"#).unwrap();
        let missing: Actor = serde_json::from_str(r#"{"login":"legacy"}"#).unwrap();
        assert!(!human.is_bot());
        assert!(bot.is_bot());
        assert!(bot_lower.is_bot());
        assert!(!missing.is_bot()); // default → "" → treated as human
    }

    #[test]
    fn question_job_has_no_github_identity() {
        let job = EventJob::question(
            QuestionInput {
                question: "How do I allow /tmp?".into(),
                conversation: vec![],
                context: vec![ContextSource {
                    source_id: "github:issue:123".into(),
                    source_type: "closed_issue".into(),
                    title: "allow /tmp".into(),
                    url: "https://x".into(),
                    version: "2.4".into(),
                    content: "use --allow".into(),
                    trust: "verified".into(),
                }],
            },
            "gemini-3.5-flash".into(),
        );
        assert_eq!(job.kind, JobKind::Question);
        assert!(job.owner.is_empty() && job.repo.is_empty());
        assert_eq!(job.number, 0);
        assert!(job.default_branch.is_empty());
        assert_eq!(job.question.expect("question").context.len(), 1);
    }

    #[test]
    fn legacy_job_without_question_deserializes() {
        // Payloads serialized before `question` existed must still parse.
        let job = EventJob::question(QuestionInput::default(), "m".into());
        let mut value = serde_json::to_value(&job).expect("serialize");
        value
            .as_object_mut()
            .expect("object")
            .remove("question")
            .expect("question present");
        let parsed: EventJob = serde_json::from_value(value).expect("parse without question");
        assert!(parsed.question.is_none());
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
