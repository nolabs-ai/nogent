//! GitHub API access using an installation token, direct to `api.github.com`.
//!
//! (Without nono there is no proxy/phantom indirection; the listener holds the
//! real token and calls GitHub directly.)

use std::time::Duration;

use nogent_core::diff_digest::ChangedFile;
use nogent_core::error::{redact_error_body, NogentError, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, ACCEPT as ACCEPT_HEADER, AUTHORIZATION};
use serde::Serialize;
use serde_json::json;

/// An inline review comment anchored to a line of the diff (new side).
#[derive(Debug, Clone, Serialize)]
pub struct InlineComment {
    pub path: String,
    pub line: u64,
    pub side: String,
    pub body: String,
}

const GITHUB_API: &str = "https://api.github.com";
const ACCEPT: &str = "application/vnd.github+json";
const API_VERSION: &str = "2022-11-28";

/// Percent-encode a single path segment (RFC 3986 unreserved set passes through).
fn urlencode_segment(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for b in seg.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

pub struct GithubClient {
    http: reqwest::Client,
}

impl GithubClient {
    pub fn new(token: &str) -> Result<Self> {
        // Bake Authorization + X-GitHub-Api-Version into default headers so the
        // token bytes don't sit in a plain `String` field for the client's
        // lifetime, and so the bearer is marked sensitive (tracing layers like
        // tower_http will print `Sensitive` instead of the value).
        let mut auth = HeaderValue::from_str(&format!("token {token}"))
            .map_err(|e| NogentError::Auth(format!("token is not a valid header value: {e}")))?;
        auth.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, auth);
        headers.insert(
            HeaderName::from_static("x-github-api-version"),
            HeaderValue::from_static(API_VERSION),
        );

        let http = reqwest::Client::builder()
            .user_agent("nogent")
            // Bound total + connect time: a stalled upstream must not pin a
            // webhook spawn indefinitely.
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(5))
            // Cap redirect chains. We can't use Policy::none() because the
            // tarball endpoint legitimately 302s to codeload.github.com.
            .redirect(reqwest::redirect::Policy::limited(3))
            // Refuse any plaintext leg even via redirect.
            .https_only(true)
            .default_headers(headers)
            .build()?;
        Ok(GithubClient { http })
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
            .header(ACCEPT_HEADER, ACCEPT)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(NogentError::GitHubApi {
                status: status.as_u16(),
                body: redact_error_body(&body),
            });
        }
        let files: Vec<ChangedFile> = serde_json::from_str(&body)?;
        if files.len() == 100 {
            tracing::warn!("PR has >=100 changed files; only the first page was fetched");
        }
        Ok(files)
    }

    /// Fetch a file's raw content at `git_ref`. Returns `None` when the file is
    /// absent, too large, binary, or otherwise not retrievable — callers fall
    /// back to the diff for that file rather than failing the whole review.
    pub async fn get_file_raw(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>> {
        let enc_path = path
            .split('/')
            .map(urlencode_segment)
            .collect::<Vec<_>>()
            .join("/");
        let url = format!("{GITHUB_API}/repos/{owner}/{repo}/contents/{enc_path}?ref={git_ref}");
        let resp = self
            .http
            .get(&url)
            // Raw media type returns the file body directly (not base64 JSON).
            .header(ACCEPT_HEADER, "application/vnd.github.raw+json")
            .send()
            .await?;
        if !resp.status().is_success() {
            tracing::debug!(
                path,
                status = resp.status().as_u16(),
                "skipping file content"
            );
            return Ok(None);
        }
        let bytes = resp.bytes().await?;
        // Skip binary content (NUL byte) — only text is useful to the model.
        if bytes.contains(&0) {
            return Ok(None);
        }
        match String::from_utf8(bytes.to_vec()) {
            Ok(s) => Ok(Some(s)),
            Err(_) => Ok(None),
        }
    }

    /// Fetch a pull request by number (for the `/nogent review` command, where
    /// the webhook only gives us the issue/PR number).
    pub async fn get_pull_request(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<nogent_core::events::PullRequest> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}");
        let resp = self
            .http
            .get(self.url(&path))
            .header(ACCEPT_HEADER, ACCEPT)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(NogentError::GitHubApi {
                status: status.as_u16(),
                body: redact_error_body(&body),
            });
        }
        serde_json::from_str(&body).map_err(NogentError::from)
    }

    /// Read maintainer-authored review guidance from a TRUSTED ref (the base
    /// branch / base SHA — never the PR head, which a fork can modify). Tries
    /// `NOGENT.md` then `.github/nogent.md`. Bounded to `max_bytes`.
    pub async fn get_repo_guidance(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
        max_bytes: usize,
    ) -> Result<Option<String>> {
        for path in ["NOGENT.md", ".github/nogent.md"] {
            if let Some(mut content) = self.get_file_raw(owner, repo, path, git_ref).await? {
                if content.len() > max_bytes {
                    let end = content
                        .char_indices()
                        .take_while(|(i, _)| *i <= max_bytes)
                        .last()
                        .map(|(i, _)| i)
                        .unwrap_or(0);
                    content.truncate(end);
                    content.push_str("\n[guidance truncated]");
                }
                return Ok(Some(content));
            }
        }
        Ok(None)
    }

    /// Download the repo tarball (gzip) at `git_ref`. Returns `None` if it is
    /// missing or exceeds `max_bytes` (caller falls back to diff-only review).
    /// reqwest follows GitHub's redirect to codeload automatically.
    pub async fn download_tarball(
        &self,
        owner: &str,
        repo: &str,
        git_ref: &str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>> {
        let url = format!("{GITHUB_API}/repos/{owner}/{repo}/tarball/{git_ref}");
        let resp = self
            .http
            .get(&url)
            .header(ACCEPT_HEADER, ACCEPT)
            .send()
            .await?;
        if !resp.status().is_success() {
            tracing::debug!(status = resp.status().as_u16(), "tarball fetch failed");
            return Ok(None);
        }
        let bytes = resp.bytes().await?;
        if bytes.len() > max_bytes {
            tracing::info!(
                bytes = bytes.len(),
                "repo tarball exceeds cap; diff-only review"
            );
            return Ok(None);
        }
        Ok(Some(bytes.to_vec()))
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

    /// Post a PR review with inline comments anchored to diff lines, plus an
    /// overall body. Every comment's `line` MUST be within a diff hunk or GitHub
    /// rejects the whole review (422) — callers filter via `commentable_lines`.
    pub async fn post_pr_review_with_comments(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
        body: &str,
        comments: Vec<InlineComment>,
    ) -> Result<()> {
        let path = format!("/repos/{owner}/{repo}/pulls/{number}/reviews");
        self.post_json(
            &path,
            &json!({ "event": "COMMENT", "body": body, "comments": comments }),
        )
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
            .header(ACCEPT_HEADER, ACCEPT)
            .json(payload)
            .send()
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await?;
            return Err(NogentError::GitHubApi {
                status: status.as_u16(),
                body: redact_error_body(&body),
            });
        }
        Ok(())
    }
}
