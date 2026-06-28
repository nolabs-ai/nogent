//! Constant-time verification of GitHub's `X-Hub-Signature-256` header.
//!
//! GitHub signs the raw request body with HMAC-SHA256 keyed by the webhook
//! secret and sends `sha256=<hexdigest>`. We recompute and compare in constant
//! time. The comparison must not short-circuit on the first differing byte, so
//! we use `Mac::verify_slice`, which is constant-time over the tag.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Verify a GitHub webhook signature header against the raw body.
///
/// `header` is the full header value, e.g. `sha256=ab12...`. Returns `true`
/// only on an exact, valid HMAC match.
#[must_use]
pub fn verify_signature(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    // Hmac::new_from_slice accepts any key length for HMAC; it does not fail in
    // practice, but we treat an error as "cannot verify" → reject.
    let Ok(mut mac) = HmacSha256::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&expected).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Known vector: secret "It's a Secret to Everybody", body "Hello, World!"
    // is GitHub's documented example for X-Hub-Signature-256.
    const SECRET: &[u8] = b"It's a Secret to Everybody";
    const BODY: &[u8] = b"Hello, World!";
    const EXPECTED: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    #[test]
    fn accepts_valid_signature() {
        assert!(verify_signature(SECRET, BODY, EXPECTED));
    }

    #[test]
    fn rejects_wrong_secret() {
        assert!(!verify_signature(b"wrong", BODY, EXPECTED));
    }

    #[test]
    fn rejects_tampered_body() {
        assert!(!verify_signature(SECRET, b"Hello, World?", EXPECTED));
    }

    #[test]
    fn rejects_missing_prefix() {
        let bare = EXPECTED.trim_start_matches("sha256=");
        assert!(!verify_signature(SECRET, BODY, bare));
    }

    #[test]
    fn rejects_non_hex() {
        assert!(!verify_signature(SECRET, BODY, "sha256=zzzz"));
    }

    #[test]
    fn rejects_truncated_digest() {
        assert!(!verify_signature(SECRET, BODY, "sha256=7571"));
    }
}
