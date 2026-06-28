//! GitHub API access using an installation token, direct to `api.github.com`.
//!
//! (Without nono there is no proxy/phantom indirection; the listener holds the
//! real token and calls GitHub directly.)

use nogent_core::diff_digest::ChangedFile;
use nogent_core::error::{NogentError, Result};
use serde_json::json;

const GITHUB_API: &str = "https://api.github.com";
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";

pub struct GithubClient {
    http: reqwest::Client,
    token: String,
}

impl GithubClient {
    pub fn new(token: &str) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent("nogent").build()?;
        Ok(GithubClient {
            http,
            token: token.to_string(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{GITHUB_API}{path}")
    }

    /// List a PR's changed files (first page, up to 100). Logs if more exist.
    pub async fn list_pr_files(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<Vec<ChangedFile>> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/files?per_page=100");
        let resp = self
            .http
            .get(self.url(&path))
            .header("Authorization", format!("token {}", self.token))
            .header("Accept", ACCEPT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(NogentError::GitHubApi {
                status: status.as_u16(),
                body,
            });
        }
        let files: Vec<ChangedFile> = serde_json::from_str(&body)?;
        if files.len() == 100 {
            tracing::warn!("PR has >=100 changed files; only the first page was fetched");
        }
        Ok(files)
    }

    /// Post a review comment on a PR (event = COMMENT, non-blocking).
    pub async fn post_pr_review(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/reviews");
        self.post_json(&path, &json!({ "event": "COMMENT", "body": body }))
            .await
    }

    /// Post a comment on an issue.
    pub async fn post_issue_comment(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<()> {
        let path = format!("/repos/{owner}/{repo}/issues/{number}/comments");
        self.post_json(&path, &json!({ "body": body })).await
    }

    async fn post_json(&self, path: &str, payload: &serde_json::Value) -> Result<()> {
        let resp = self
            .http
            .post(self.url(path))
            .header("Authorization", format!("token {}", self.token))
            .header("Accept", ACCEPT)
            .header("X-GitHub-Api-Version", API_VERSION)
            .json(payload)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await?;
            return Err(NogentError::GitHubApi {
                status: status.as_u16(),
                body,
            });
        }
        Ok(())
    }
}
