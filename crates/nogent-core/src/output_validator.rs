//! Structured model-output validation with a canary token.
//!
//! Prompt-injection defense: the system prompt embeds a random canary and
//! instructs the model to echo it in a strict JSON object. Untrusted PR/issue
//! content cannot know the canary, so a response that omits or mismatches it —
//! or that deviates from the schema (unknown keys, oversized fields) — is
//! rejected and replaced by a fixed "manual review" fallback. This prevents a
//! malicious diff from steering the bot into posting attacker-chosen text.

use rand::RngCore;
use serde::Deserialize;

/// Posted verbatim when validation fails. Never contains model-derived text.
pub const FALLBACK_MESSAGE: &str = "🛡️ nogent could not produce a schema-conforming security review for this change \
(the model output failed canary/shape validation). A maintainer should review manually.";

const MAX_ITEMS: usize = 20;
const MAX_ITEM_LEN: usize = 500;
const MAX_SECTION_LEN: usize = 2_000;

/// Generate a 16-byte random canary, hex-encoded (32 chars).
#[must_use]
pub fn generate_canary() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

// ── PR review ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrReviewOutput {
    pub canary: String,
    #[serde(default)]
    pub findings: Vec<String>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default)]
    pub security_posture: String,
}

/// Validate a raw model string as a PR review. Returns `None` on any failure
/// (caller emits `FALLBACK_MESSAGE`).
#[must_use]
pub fn validate_pr_review(raw: &str, expected_canary: &str) -> Option<PrReviewOutput> {
    let stripped = strip_code_fence(raw);
    let parsed: PrReviewOutput = serde_json::from_str(stripped).ok()?;
    if !canary_matches(&parsed.canary, expected_canary) {
        return None;
    }
    if !bounded_items(&parsed.findings) || !bounded_items(&parsed.open_questions) {
        return None;
    }
    if parsed.security_posture.chars().count() > MAX_SECTION_LEN {
        return None;
    }
    Some(parsed)
}

/// Render a validated PR review as a Markdown comment body.
#[must_use]
pub fn format_pr_review_markdown(out: &PrReviewOutput) -> String {
    let mut s = String::new();
    s.push_str("## 🛡️ nogent security review\n\n");
    if !out.security_posture.is_empty() {
        s.push_str(&out.security_posture);
        s.push_str("\n\n");
    }
    if out.findings.is_empty() {
        s.push_str("**Findings:** none flagged in scope.\n\n");
    } else {
        s.push_str("**Findings:**\n\n");
        for f in &out.findings {
            s.push_str(&format!("- {f}\n"));
        }
        s.push('\n');
    }
    if !out.open_questions.is_empty() {
        s.push_str("**Open questions:**\n\n");
        for q in &out.open_questions {
            s.push_str(&format!("- {q}\n"));
        }
        s.push('\n');
    }
    s.push_str("<sub>Automated semantic security review. CI already covers clippy, rustfmt, tests, cargo-audit and commit-lint.</sub>");
    s
}

// ── Issue triage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IssueTriageOutput {
    pub canary: String,
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub suggested_resolution_path: String,
    #[serde(default)]
    pub maintainer_notes: String,
}

#[must_use]
pub fn validate_issue_triage(raw: &str, expected_canary: &str) -> Option<IssueTriageOutput> {
    let stripped = strip_code_fence(raw);
    let parsed: IssueTriageOutput = serde_json::from_str(stripped).ok()?;
    if !canary_matches(&parsed.canary, expected_canary) {
        return None;
    }
    for field in [
        &parsed.verdict,
        &parsed.suggested_resolution_path,
        &parsed.maintainer_notes,
    ] {
        if field.chars().count() > MAX_SECTION_LEN {
            return None;
        }
    }
    Some(parsed)
}

#[must_use]
pub fn format_issue_triage_markdown(out: &IssueTriageOutput) -> String {
    let mut s = String::new();
    s.push_str("## 🛡️ nogent issue triage\n\n");
    if !out.verdict.is_empty() {
        s.push_str(&format!("**Assessment:** {}\n\n", out.verdict));
    }
    if !out.suggested_resolution_path.is_empty() {
        s.push_str(&format!(
            "**Suggested resolution:** {}\n\n",
            out.suggested_resolution_path
        ));
    }
    if !out.maintainer_notes.is_empty() {
        s.push_str(&format!(
            "**Notes for maintainers:** {}\n\n",
            out.maintainer_notes
        ));
    }
    s.push_str("<sub>Automated triage suggestion. Not a maintainer decision.</sub>");
    s
}

// ── helpers ─────────────────────────────────────────────────────────────────

/// Strip a leading/trailing Markdown code fence the model may wrap JSON in.
fn strip_code_fence(raw: &str) -> &str {
    let t = raw.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    t.strip_suffix("```").unwrap_or(t).trim()
}

/// Constant-time-ish canary comparison. Canaries are not secrets to an
/// attacker who can read the prompt, but a stable equal check is fine here.
fn canary_matches(got: &str, expected: &str) -> bool {
    !expected.is_empty() && got == expected
}

fn bounded_items(items: &[String]) -> bool {
    items.len() <= MAX_ITEMS && items.iter().all(|i| i.chars().count() <= MAX_ITEM_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_json(canary: &str) -> String {
        format!(
            r#"{{"canary":"{canary}","findings":["f1"],"open_questions":[],"security_posture":"ok"}}"#
        )
    }

    #[test]
    fn canary_is_32_hex_chars() {
        let c = generate_canary();
        assert_eq!(c.len(), 32);
        assert!(c.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn valid_pr_output_passes() {
        let c = "abc123";
        let out = validate_pr_review(&pr_json(c), c).expect("valid");
        assert_eq!(out.findings, vec!["f1"]);
    }

    #[test]
    fn wrong_canary_rejected() {
        assert!(validate_pr_review(&pr_json("zzz"), "abc123").is_none());
    }

    #[test]
    fn empty_expected_canary_never_matches() {
        assert!(validate_pr_review(&pr_json(""), "").is_none());
    }

    #[test]
    fn unknown_key_rejected() {
        let raw = r#"{"canary":"c","findings":[],"evil":"steal"}"#;
        assert!(validate_pr_review(raw, "c").is_none());
    }

    #[test]
    fn non_object_rejected() {
        assert!(validate_pr_review("[1,2,3]", "c").is_none());
        assert!(validate_pr_review("not json", "c").is_none());
    }

    #[test]
    fn too_many_findings_rejected() {
        let items = (0..21)
            .map(|i| format!("\"f{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let raw = format!(r#"{{"canary":"c","findings":[{items}]}}"#);
        assert!(validate_pr_review(&raw, "c").is_none());
    }

    #[test]
    fn oversized_finding_rejected() {
        let big = "x".repeat(501);
        let raw = format!(r#"{{"canary":"c","findings":["{big}"]}}"#);
        assert!(validate_pr_review(&raw, "c").is_none());
    }

    #[test]
    fn code_fence_is_stripped() {
        let c = "c";
        let fenced = format!("```json\n{}\n```", pr_json(c));
        assert!(validate_pr_review(&fenced, c).is_some());
    }

    #[test]
    fn issue_triage_valid_passes() {
        let raw = r#"{"canary":"c","verdict":"config","suggested_resolution_path":".github/nogent.json","maintainer_notes":""}"#;
        assert!(validate_issue_triage(raw, "c").is_some());
    }
}
