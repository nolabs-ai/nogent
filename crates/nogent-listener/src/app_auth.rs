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

pub struct AppAuth {
    app_id: String,
    encoding_key: EncodingKey,
    cache: Mutex<HashMap<u64, CachedToken>>,
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

    /// Return a valid installation token, minting (and caching) one if needed.
    ///
    /// GitHub installation tokens live ~1 hour; we refresh well before expiry.
    pub async fn installation_token(&self, installation_id: u64) -> Result<Zeroizing<String>> {
        if let Some(tok) = self.cached(installation_id) {
            return Ok(tok);
        }
        let jwt = self.make_jwt()?;
        let minted = github_mint::mint_installation_token(&jwt, installation_id).await?;
        self.store(installation_id, &minted);
        Ok(minted)
    }

    fn cached(&self, installation_id: u64) -> Option<Zeroizing<String>> {
        let guard = self.cache.lock().ok()?;
        let entry = guard.get(&installation_id)?;
        if Instant::now() < entry.refresh_after {
            Some(entry.token.clone())
        } else {
            None
        }
    }

    fn store(&self, installation_id: u64, token: &Zeroizing<String>) {
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(
                installation_id,
                CachedToken {
                    token: token.clone(),
                    // Refresh after 50 minutes (tokens last ~60).
                    refresh_after: Instant::now() + Duration::from_secs(50 * 60),
                },
            );
        }
    }
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
}
