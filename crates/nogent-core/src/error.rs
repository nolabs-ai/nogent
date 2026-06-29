//! Shared error type. Per nono coding standards we never `.unwrap()`/`.expect()`
//! on fallible paths; everything propagates through `NogentError` via `?`.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NogentError {
    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid webhook signature")]
    InvalidSignature,

    #[error("malformed webhook payload: {0}")]
    Payload(String),

    #[error("github api error ({status}): {body}")]
    GitHubApi { status: u16, body: String },

    #[error("gemini api error ({status}): {body}")]
    GeminiApi { status: u16, body: String },

    #[error("model output failed validation: {0}")]
    OutputValidation(String),

    #[error("auth error: {0}")]
    Auth(String),

    #[error("http transport error: {0}")]
    Http(String),

    #[error("io error: {0}")]
    Io(String),

    #[error("serialization error: {0}")]
    Serde(String),
}

impl From<reqwest::Error> for NogentError {
    fn from(e: reqwest::Error) -> Self {
        NogentError::Http(e.to_string())
    }
}

impl From<std::io::Error> for NogentError {
    fn from(e: std::io::Error) -> Self {
        NogentError::Io(e.to_string())
    }
}

impl From<serde_json::Error> for NogentError {
    fn from(e: serde_json::Error) -> Self {
        NogentError::Serde(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, NogentError>;

/// Sanitise an upstream API error body before stuffing it into a `NogentError`
/// (which ends up in operator logs / tracing). Two concrete worries:
///   1. GitHub error bodies can echo the bearer token on certain auth failures.
///   2. Gemini error bodies can echo prompt fragments (untrusted PR/issue text).
///
/// We cap the size and mask anything that looks like a GitHub installation/
/// OAuth token. The mask preserves the *prefix* so the kind of token is still
/// triage-able from logs without exposing the secret material.
#[must_use]
pub fn redact_error_body(body: &str) -> String {
    const MAX: usize = 1024;

    // Codepoint-safe truncation.
    let mut end = body.len().min(MAX);
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 32);
    out.push_str(&body[..end]);
    if body.len() > MAX {
        out.push_str("…[truncated]");
    }
    mask_gh_tokens(&out)
}

/// Replace `gh[spour]_<token>` runs with `gh<x>_REDACTED`. Token charset is
/// base62-ish; we accept ASCII alphanumerics conservatively.
fn mask_gh_tokens(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        let is_prefix = i + 4 <= bytes.len()
            && bytes[i] == b'g'
            && bytes[i + 1] == b'h'
            && matches!(bytes[i + 2], b's' | b'p' | b'o' | b'u' | b'r')
            && bytes[i + 3] == b'_';
        if is_prefix {
            out.push_str(&s[i..i + 4]);
            i += 4;
            // Consume the token body (alphanumeric run).
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            out.push_str("REDACTED");
            continue;
        }
        // Codepoint-safe single-char copy.
        let mut ch_end = i + 1;
        while ch_end <= bytes.len() && !s.is_char_boundary(ch_end) {
            ch_end += 1;
        }
        out.push_str(&s[i..ch_end]);
        i = ch_end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_oversized_bodies() {
        let big = "x".repeat(5_000);
        let r = redact_error_body(&big);
        assert!(r.len() < big.len());
        assert!(r.ends_with("[truncated]"));
    }

    #[test]
    fn masks_github_installation_tokens() {
        let body = r#"{"message":"bad creds","token":"ghs_AbCdEf0123456789xyzAbCdEf012345"}"#;
        let r = redact_error_body(body);
        assert!(!r.contains("ghs_AbCdEf0123456789"));
        assert!(r.contains("ghs_REDACTED"));
    }

    #[test]
    fn masks_multiple_token_shapes() {
        let body = "saw ghp_aaaaaaaaaaaaaaaaaaaa and gho_bbbbbbbbbbbbbbbbbbbb plus ghu_cc";
        let r = redact_error_body(body);
        assert!(!r.contains("ghp_aaaaaaaaaaaaaaaaaaaa"));
        assert!(!r.contains("gho_bbbbbbbbbbbbbbbbbbbb"));
        assert!(r.matches("REDACTED").count() == 3);
    }

    #[test]
    fn leaves_innocuous_text_alone() {
        let body = "404 Not Found: repo missing";
        assert_eq!(redact_error_body(body), body);
    }

    #[test]
    fn truncation_is_codepoint_safe() {
        // 4-byte emoji repeated past the cap must not split a codepoint.
        let body = "😀".repeat(400); // 1600 bytes
        let r = redact_error_body(&body);
        // Output is valid UTF-8 by construction (String). Spot-check no '\u{FFFD}'.
        assert!(!r.contains('\u{FFFD}'));
    }
}
