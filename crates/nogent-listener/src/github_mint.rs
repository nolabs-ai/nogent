//! GitHub API calls made by the TRUSTED listener only: minting installation
//! tokens and fetching the repo-level `.github/nogent.json`.
//!
//! These run with real credentials and talk to `api.github.com` directly (no
//! nono proxy) — the listener is the trusted boundary. The sandboxed worker
//! never calls these functions.

use std::time::Duration;

use nogent_core::error::{NogentError, Result, redact_error_body};
use serde::Deserialize;
use serde_json::json;
use zeroize::Zeroizing;

const GITHUB_API: &str = "https://api.github.com";
const UA: &str = "nogent-listener";
const ACCEPT: &str = "application/vnd.github+json";

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(5))
        // Token minting is a single POST; no legitimate reason to follow any
        // redirect to a non-api.github.com host.
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .build()
        .map_err(NogentError::from)
}

/// A minted installation token plus GitHub's stated expiry (RFC 3339 string).
pub struct MintedToken {
    pub token: Zeroizing<String>,
    /// `expires_at` exactly as returned by GitHub (e.g. `2026-06-29T12:34:56Z`).
    pub expires_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResp {
    token: String,
    #[serde(default)]
    expires_at: Option<String>,
}

/// Mint a short-lived installation token using a signed App JWT.
///
/// The token is scoped down to the single `owner/repo` the event concerns, with
/// only the permissions nogent actually exercises:
///   - `contents: read`        — fetch `.github/nogent.json` and changed-file content
///   - `pull_requests: write`  — post PR reviews / inline comments
///   - `issues: write`         — post issue-triage comments
///
/// Without an explicit body GitHub mints a token covering *every* repo in the
/// installation with *all* granted permissions; an empty `repositories`/
/// `permissions` is the difference between blast-radius-of-one-repo and
/// blast-radius-of-the-whole-installation if the token leaks.
pub async fn mint_installation_token(
    jwt: &str,
    installation_id: u64,
    repo: &str,
) -> Result<MintedToken> {
    let url = format!("{GITHUB_API}/app/installations/{installation_id}/access_tokens");
    // `repositories` takes bare repo names: the installation already belongs to
    // a single account, so the owner is implicit (it scopes the cache key only).
    let body = json!({
        "repositories": [repo],
        "permissions": {
            "contents": "read",
            "pull_requests": "write",
            "issues": "write",
        },
    });
    let resp = client()?
        .post(&url)
        .header(reqwest::header::AUTHORIZATION, sensitive_bearer(jwt)?)
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", "2022-11-28")
        .json(&body)
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
    let parsed: InstallationTokenResp = serde_json::from_str(&body)?;
    Ok(MintedToken {
        token: Zeroizing::new(parsed.token),
        expires_at: parsed.expires_at,
    })
}

fn sensitive_bearer(jwt: &str) -> Result<reqwest::header::HeaderValue> {
    let mut v = reqwest::header::HeaderValue::from_str(&format!("Bearer {jwt}"))
        .map_err(|e| NogentError::Auth(format!("JWT is not a valid header value: {e}")))?;
    v.set_sensitive(true);
    Ok(v)
}

fn sensitive_token(token: &str) -> Result<reqwest::header::HeaderValue> {
    let mut v = reqwest::header::HeaderValue::from_str(&format!("token {token}"))
        .map_err(|e| NogentError::Auth(format!("token is not a valid header value: {e}")))?;
    v.set_sensitive(true);
    Ok(v)
}

/// Fetch `.github/nogent.json` raw bytes for a repo. Returns `Ok(None)` when
/// the file is absent (404), `Err` on other failures.
pub async fn fetch_repo_config(token: &str, owner: &str, repo: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("{GITHUB_API}/repos/{owner}/{repo}/contents/.github/nogent.json");
    let resp = client()?
        .get(&url)
        .header(reqwest::header::AUTHORIZATION, sensitive_token(token)?)
        // Raw media type returns the file body directly instead of base64 JSON.
        .header("Accept", "application/vnd.github.raw+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .send()
        .await?;
    if resp.status().as_u16() == 404 {
        return Ok(None);
    }
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await?;
        return Err(NogentError::GitHubApi {
            status: status.as_u16(),
            body: redact_error_body(&body),
        });
    }
    let bytes = resp.bytes().await?;
    Ok(Some(bytes.to_vec()))
}
