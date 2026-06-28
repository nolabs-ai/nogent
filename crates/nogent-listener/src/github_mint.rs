//! GitHub API calls made by the TRUSTED listener only: minting installation
//! tokens and fetching the repo-level `.github/nogent.json`.
//!
//! These run with real credentials and talk to `api.github.com` directly (no
//! nono proxy) — the listener is the trusted boundary. The sandboxed worker
//! never calls these functions.

use nogent_core::error::{NogentError, Result};
use serde::Deserialize;
use zeroize::Zeroizing;

const GITHUB_API: &str = "https://api.github.com";
const UA: &str = "nogent-listener";
const ACCEPT: &str = "application/vnd.github+json";

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .build()
        .map_err(NogentError::from)
}

#[derive(Debug, Deserialize)]
struct InstallationTokenResp {
    token: String,
}

/// Mint a short-lived installation token using a signed App JWT.
pub async fn mint_installation_token(jwt: &str, installation_id: u64) -> Result<Zeroizing<String>> {
    let url = format!("{GITHUB_API}/app/installations/{installation_id}/access_tokens");
    let resp = client()?
        .post(&url)
        .bearer_auth(jwt)
        .header("Accept", ACCEPT)
        .header("X-GitHub-Api-Version", "2022-11-28")
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
    let parsed: InstallationTokenResp = serde_json::from_str(&body)?;
    Ok(Zeroizing::new(parsed.token))
}

/// Fetch `.github/nogent.json` raw bytes for a repo. Returns `Ok(None)` when
/// the file is absent (404), `Err` on other failures.
pub async fn fetch_repo_config(token: &str, owner: &str, repo: &str) -> Result<Option<Vec<u8>>> {
    let url = format!("{GITHUB_API}/repos/{owner}/{repo}/contents/.github/nogent.json");
    let resp = client()?
        .get(&url)
        .header("Authorization", format!("token {token}"))
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
            body,
        });
    }
    let bytes = resp.bytes().await?;
    Ok(Some(bytes.to_vec()))
}
