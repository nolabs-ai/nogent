//! Repository-level config at `.github/nogent.json`.
//!
//! Fail-secure: a *malformed* config is a hard error that disables the bot for
//! that event. We never fall back to "all enabled" on a parse failure — that
//! would be a silent degradation to a less restrictive state, which the nono
//! security model explicitly forbids ("Configuration load failures must be
//! fatal"). An *absent* config is fine and yields conservative defaults.

use serde::{Deserialize, Serialize};

use crate::error::{NogentError, Result};

pub const DEFAULT_MAX_FILES: usize = 25;
pub const DEFAULT_MAX_PATCH_BYTES: usize = 120_000;

/// Raw on-disk shape. All fields optional so an author can set just one.
/// Keys are camelCase to match the documented `.github/nogent.json` surface.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawConfig {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    issue_triage: Option<RawFeature>,
    #[serde(default)]
    pull_request_security_review: Option<RawPrReview>,
    #[serde(default)]
    additional_policy_paths: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawFeature {
    #[serde(default)]
    enabled: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawPrReview {
    #[serde(default)]
    enabled: Option<bool>,
    #[serde(default)]
    max_files: Option<usize>,
    #[serde(default)]
    max_patch_bytes: Option<usize>,
}

/// Fully resolved config with defaults applied. Carried in `EventJob`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedConfig {
    pub enabled: bool,
    pub issue_triage_enabled: bool,
    pub pr_review_enabled: bool,
    pub max_files: usize,
    pub max_patch_bytes: usize,
    pub additional_policy_paths: Vec<String>,
}

impl Default for ResolvedConfig {
    /// Defaults applied when `.github/nogent.json` is absent: both workflows
    /// on, conservative bounds.
    fn default() -> Self {
        ResolvedConfig {
            enabled: true,
            issue_triage_enabled: true,
            pr_review_enabled: true,
            max_files: DEFAULT_MAX_FILES,
            max_patch_bytes: DEFAULT_MAX_PATCH_BYTES,
            additional_policy_paths: Vec::new(),
        }
    }
}

impl ResolvedConfig {
    /// Resolve from optional raw JSON bytes.
    ///
    /// * `None` (file absent) → defaults.
    /// * `Some(bytes)` that parse → resolved with per-field defaults.
    /// * `Some(bytes)` that fail to parse → `Err` (fail-secure; caller must
    ///   skip the event, not proceed with defaults).
    pub fn resolve(raw: Option<&[u8]>) -> Result<Self> {
        let Some(bytes) = raw else {
            return Ok(ResolvedConfig::default());
        };
        let parsed: RawConfig = serde_json::from_slice(bytes)
            .map_err(|e| NogentError::Config(format!(".github/nogent.json is malformed: {e}")))?;
        let d = ResolvedConfig::default();
        let pr = parsed.pull_request_security_review.unwrap_or_default();
        Ok(ResolvedConfig {
            enabled: parsed.enabled.unwrap_or(d.enabled),
            issue_triage_enabled: parsed
                .issue_triage
                .and_then(|f| f.enabled)
                .unwrap_or(d.issue_triage_enabled),
            pr_review_enabled: pr.enabled.unwrap_or(d.pr_review_enabled),
            max_files: pr.max_files.unwrap_or(d.max_files).clamp(1, 300),
            max_patch_bytes: pr
                .max_patch_bytes
                .unwrap_or(d.max_patch_bytes)
                .clamp(1_000, 2_000_000),
            additional_policy_paths: parsed
                .additional_policy_paths
                .unwrap_or(d.additional_policy_paths),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_yields_defaults() {
        let c = ResolvedConfig::resolve(None).expect("defaults");
        assert!(c.enabled && c.pr_review_enabled && c.issue_triage_enabled);
        assert_eq!(c.max_files, DEFAULT_MAX_FILES);
        assert_eq!(c.max_patch_bytes, DEFAULT_MAX_PATCH_BYTES);
    }

    #[test]
    fn partial_config_merges_defaults() {
        let raw = br#"{"pullRequestSecurityReview": {"maxFiles": 5}}"#;
        let c = ResolvedConfig::resolve(Some(raw)).expect("parse");
        assert_eq!(c.max_files, 5);
        assert_eq!(c.max_patch_bytes, DEFAULT_MAX_PATCH_BYTES);
        assert!(c.pr_review_enabled);
    }

    #[test]
    fn disabled_flags_respected() {
        let raw = br#"{"enabled": false, "issueTriage": {"enabled": false}}"#;
        let c = ResolvedConfig::resolve(Some(raw)).expect("parse");
        assert!(!c.enabled);
        assert!(!c.issue_triage_enabled);
    }

    #[test]
    fn malformed_is_fatal_not_defaults() {
        // Fail-secure: a broken config must error, NOT silently enable everything.
        let raw = br#"{ this is not json "#;
        assert!(ResolvedConfig::resolve(Some(raw)).is_err());
    }

    #[test]
    fn unknown_keys_rejected() {
        let raw = br#"{"enabledd": true}"#;
        assert!(ResolvedConfig::resolve(Some(raw)).is_err());
    }

    #[test]
    fn bounds_are_clamped() {
        let raw = br#"{"pullRequestSecurityReview": {"maxFiles": 99999, "maxPatchBytes": 1}}"#;
        let c = ResolvedConfig::resolve(Some(raw)).expect("parse");
        assert_eq!(c.max_files, 300);
        assert_eq!(c.max_patch_bytes, 1_000);
    }
}
