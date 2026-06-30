//! GitHub App authentication: RS256 JWT signing + installation-token minting
//! with a short-lived in-memory cache.
//!
//! This is the ONLY place the App private key is used. JWT signing is a local,
//! offline operation — it cannot be proxied — which is precisely why the
//! private key must live here in the trusted listener and never in the
//! sandboxed worker. The worker receives only a minted, short-lived
//! installation token (via the nono proxy as a phantom), never the key.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use nogent_core::error::{NogentError, Result};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::github_mint;

#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iat: u64,
    exp: u64,
    iss: String,
}

struct CachedToken {
    token: Zeroizing<String>,
    /// Local instant after which we must re-mint.
    refresh_after: Instant,
}

/// Cache key: a token is scoped to a single `(installation, owner, repo)`, so
/// it must be keyed by all three — keying by `installation_id` alone would hand
/// a repo-scoped token back for a *different* repo in the same installation.
type CacheKey = (u64, String, String);

pub struct AppAuth {
    app_id: String,
    encoding_key: EncodingKey,
    cache: Mutex<HashMap<CacheKey, CachedToken>>,
}

impl AppAuth {
    /// Build from the App id and PEM private key. Errors if the PEM is invalid.
    pub fn new(app_id: &str, private_key_pem: &str) -> Result<Self> {
        let encoding_key = EncodingKey::from_rsa_pem(private_key_pem.as_bytes())
            .map_err(|e| NogentError::Auth(format!("invalid App private key PEM: {e}")))?;
        Ok(AppAuth {
            app_id: app_id.to_string(),
            encoding_key,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Sign a short-lived App JWT (≤10 min; we use ~9 min, backdated 60s to
    /// tolerate clock skew, per GitHub guidance).
    pub fn make_jwt(&self) -> Result<Zeroizing<String>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| NogentError::Auth(format!("system clock before epoch: {e}")))?
            .as_secs();
        let iat = now.saturating_sub(60);
        let exp = now.saturating_add(9 * 60);
        let claims = Claims {
            iat,
            exp,
            iss: self.app_id.clone(),
        };
        let token =
            jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.encoding_key)
                .map_err(|e| NogentError::Auth(format!("failed to sign App JWT: {e}")))?;
        Ok(Zeroizing::new(token))
    }

    /// Return a valid installation token for a specific repo, minting (and
    /// caching) one if needed. The token is scoped to `owner/repo` with only
    /// the permissions nogent uses (see `github_mint::mint_installation_token`).
    ///
    /// GitHub installation tokens live ~1 hour; we refresh well before expiry.
    pub async fn installation_token(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
    ) -> Result<Zeroizing<String>> {
        if let Some(tok) = self.cached(installation_id, owner, repo) {
            return Ok(tok);
        }
        let jwt = self.make_jwt()?;
        let minted = github_mint::mint_installation_token(&jwt, installation_id, repo).await?;
        self.store(installation_id, owner, repo, &minted);
        Ok(minted.token)
    }

    fn cached(&self, installation_id: u64, owner: &str, repo: &str) -> Option<Zeroizing<String>> {
        let guard = self.cache.lock().ok()?;
        let entry = guard.get(&(installation_id, owner.to_string(), repo.to_string()))?;
        if Instant::now() < entry.refresh_after {
            Some(entry.token.clone())
        } else {
            None
        }
    }

    fn store(
        &self,
        installation_id: u64,
        owner: &str,
        repo: &str,
        minted: &github_mint::MintedToken,
    ) {
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(
                (installation_id, owner.to_string(), repo.to_string()),
                CachedToken {
                    token: minted.token.clone(),
                    refresh_after: Self::refresh_after(minted.expires_at.as_deref()),
                },
            );
        }
    }

    /// Derive the local refresh deadline from GitHub's stated `expires_at`,
    /// refreshing 5 minutes early. Behaviour:
    ///   - parseable `expires_at`, plenty of lifetime → `now + (remaining − 5min)`
    ///   - parseable `expires_at`, already past or within the skew → `now`
    ///     (refresh immediately on the next call; do NOT silently extend the
    ///     token to the conservative fallback)
    ///   - missing or unparseable `expires_at` → `now + 50min` fallback
    ///
    /// The expired-but-falls-to-fallback case is the dangerous one: a clock-
    /// ahead box or a future shortened token lifetime would otherwise keep
    /// serving a dead token for up to 50 minutes.
    fn refresh_after(expires_at: Option<&str>) -> Instant {
        const SKEW: Duration = Duration::from_secs(5 * 60);
        const FALLBACK: Duration = Duration::from_secs(50 * 60);
        let now = Instant::now();

        match expires_at.and_then(parse_rfc3339_secs) {
            Some(exp_secs) => {
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let remaining = Duration::from_secs(exp_secs.saturating_sub(now_secs));
                // saturating_sub: if remaining ≤ SKEW, lifetime is zero and
                // refresh_after == now → cached() sees the token as stale.
                now + remaining.saturating_sub(SKEW)
            }
            None => now + FALLBACK,
        }
    }
}

/// Minimal RFC 3339 / ISO-8601 UTC parser for GitHub's `expires_at`
/// (`YYYY-MM-DDTHH:MM:SSZ`) → unix seconds. Returns `None` on any deviation so
/// the caller falls back to the conservative TTL rather than trusting garbage.
fn parse_rfc3339_secs(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hour: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    // Seconds may carry a fractional part; take the integer portion.
    let sec: i64 = t.next()?.split('.').next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // Days since 1970-01-01 via a civil-calendar algorithm (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3_600 + min * 60 + sec;
    u64::try_from(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // A throwaway 2048-bit RSA test key. NOT a real credential — generated
    // solely to exercise JWT signing/decoding in tests.
    const TEST_KEY: &str = include_str!("../testdata/test_rsa_key.pem");

    #[test]
    fn signs_and_decodes_jwt() {
        let auth = AppAuth::new("123456", TEST_KEY).expect("load key");
        let jwt = auth.make_jwt().expect("sign");

        // Decode with the matching public key derivation is overkill here;
        // assert structure + claims by disabling validation against a key.
        use jsonwebtoken::{DecodingKey, Validation};
        let mut v = Validation::new(Algorithm::RS256);
        v.insecure_disable_signature_validation();
        v.validate_exp = false;
        let data = jsonwebtoken::decode::<Claims>(&jwt, &DecodingKey::from_secret(b"ignored"), &v)
            .expect("decode");
        assert_eq!(data.claims.iss, "123456");
        assert!(data.claims.exp > data.claims.iat);
    }

    #[test]
    fn rejects_bad_pem() {
        assert!(AppAuth::new("1", "not a pem").is_err());
    }

    #[test]
    fn parses_github_expires_at() {
        // 2026-06-29T00:00:00Z = 1782691200 unix seconds.
        assert_eq!(
            parse_rfc3339_secs("2026-06-29T00:00:00Z"),
            Some(1_782_691_200)
        );
        // Epoch.
        assert_eq!(parse_rfc3339_secs("1970-01-01T00:00:00Z"), Some(0));
        // A known leap-day timestamp: 2024-02-29T12:00:00Z = 1709208000.
        assert_eq!(
            parse_rfc3339_secs("2024-02-29T12:00:00Z"),
            Some(1_709_208_000)
        );
        // Fractional seconds tolerated (integer part only).
        assert_eq!(
            parse_rfc3339_secs("2026-06-29T00:00:01.500Z"),
            Some(1_782_691_201)
        );
    }

    #[test]
    fn rejects_malformed_expires_at() {
        assert_eq!(parse_rfc3339_secs("not-a-date"), None);
        assert_eq!(parse_rfc3339_secs("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_secs("2026-06-29"), None);
    }

    #[test]
    fn refresh_after_uses_fallback_only_when_expiry_missing() {
        // No `expires_at` → conservative fallback (well in the future).
        let fallback = AppAuth::refresh_after(None);
        assert!(fallback > Instant::now() + Duration::from_secs(40 * 60));

        // Unparseable → same fallback.
        assert!(
            AppAuth::refresh_after(Some("garbage")) > Instant::now() + Duration::from_secs(40 * 60)
        );
    }

    #[test]
    fn refresh_after_immediate_when_already_expired() {
        // A past `expires_at` must NOT silently extend to the 50-min fallback
        // — refresh_after collapses to "now" so the next cached() call re-mints.
        let r = AppAuth::refresh_after(Some("1970-01-01T00:00:00Z"));
        assert!(r <= Instant::now());
    }
}
