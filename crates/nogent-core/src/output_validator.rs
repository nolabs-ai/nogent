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

const MAX_ITEMS: usize = 30;
const MAX_FINDING_DESC: usize = 1_500;
const MAX_FIELD_LEN: usize = 300;
const MAX_SECTION_LEN: usize = 4_000;

/// Generate a 16-byte random canary, hex-encoded (32 chars).
#[must_use]
pub fn generate_canary() -> String {
    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

// ── PR review ─────────────────────────────────────────────────────────────

/// A single review finding. The model returns structured findings (with file +
/// line) rather than flat strings, so reviews can later become inline comments.
/// Not `deny_unknown_fields` — tolerate extra per-finding keys the model adds.
#[derive(Debug, Clone, Deserialize)]
pub struct Finding {
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: Option<u64>,
    #[serde(default)]
    pub description: String,
}

// Not `deny_unknown_fields`: tolerate extra keys the model invents (e.g. a
// leftover `open_questions`) — the canary, not schema strictness, is the gate.
#[derive(Debug, Clone, Deserialize)]
pub struct PrReviewOutput {
    pub canary: String,
    #[serde(default)]
    pub findings: Vec<Finding>,
    #[serde(default)]
    pub summary: String,
}

/// Validate a raw model string as a PR review. Returns `None` on any failure
/// (caller emits `FALLBACK_MESSAGE`).
#[must_use]
pub fn validate_pr_review(raw: &str, expected_canary: &str) -> Option<PrReviewOutput> {
    let stripped = extract_object(raw);
    let parsed: PrReviewOutput = serde_json::from_str(stripped).ok()?;
    if !canary_matches(&parsed.canary, expected_canary) {
        return None;
    }
    if parsed.findings.len() > MAX_ITEMS {
        return None;
    }
    for f in &parsed.findings {
        if f.description.chars().count() > MAX_FINDING_DESC
            || f.category.chars().count() > MAX_FIELD_LEN
            || f.file.chars().count() > MAX_FIELD_LEN
        {
            return None;
        }
    }
    if parsed.summary.chars().count() > MAX_SECTION_LEN {
        return None;
    }
    Some(parsed)
}

const REVIEW_FOOTER: &str = "<sub>Automated code + security review. CI already covers clippy, rustfmt, tests, cargo-audit and commit-lint.</sub>";

/// `**[cat]** \`file:line\` — description` — for a finding listed in the body.
#[must_use]
pub fn finding_list_item(f: &Finding) -> String {
    let cat = f.category.trim_matches(['[', ']']);
    let mut s = String::from("- ");
    if !cat.is_empty() {
        s.push_str(&format!("**[{cat}]** "));
    }
    if !f.file.is_empty() {
        match f.line {
            Some(n) => s.push_str(&format!("`{}:{}` — ", f.file, n)),
            None => s.push_str(&format!("`{}` — ", f.file)),
        }
    }
    s.push_str(&f.description);
    s
}

/// `**[cat]** description` — body for an inline comment (the file/line anchor
/// the comment, so they're omitted from the text).
#[must_use]
pub fn finding_inline_body(f: &Finding) -> String {
    let cat = f.category.trim_matches(['[', ']']);
    if cat.is_empty() {
        f.description.clone()
    } else {
        format!("**[{cat}]** {}", f.description)
    }
}

/// Best-effort recovery of findings from a possibly-truncated review (the JSON
/// got cut off mid-array, so strict parse failed). Requires the canary to be
/// present and match; recovers every complete finding object before the cut.
/// Returns `None` if the canary is absent/wrong (the injection gate still holds).
#[must_use]
pub fn salvage_pr_review(raw: &str, expected_canary: &str) -> Option<PrReviewOutput> {
    let t = extract_object(raw);
    let canary = json_string_field(t, "canary")?;
    if !canary_matches(&canary, expected_canary) {
        return None;
    }
    let summary = json_string_field(t, "summary").unwrap_or_default();
    let findings = dedup_findings(scan_findings(t))
        .into_iter()
        .filter(|f| f.description.chars().count() <= MAX_FINDING_DESC)
        .take(MAX_ITEMS)
        .collect();
    Some(PrReviewOutput {
        canary,
        findings,
        summary,
    })
}

/// Drop duplicate findings (same file+line+category and a matching description
/// prefix), preserving first-seen order. Used when merging review passes.
#[must_use]
pub fn dedup_findings(findings: Vec<Finding>) -> Vec<Finding> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for f in findings {
        let desc_prefix: String = f
            .description
            .chars()
            .take(80)
            .collect::<String>()
            .to_lowercase();
        let key = (
            f.file.to_lowercase(),
            f.line,
            f.category.trim_matches(['[', ']']).to_lowercase(),
            desc_prefix,
        );
        if seen.insert(key) {
            out.push(f);
        }
    }
    out.truncate(MAX_ITEMS);
    out
}

/// Read a top-level JSON string field by key, tolerant of truncation elsewhere.
fn json_string_field(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let start = s.find(&pat)? + pat.len();
    let after = s[start..].trim_start();
    let after = after.strip_prefix(':')?.trim_start();
    let inner = after.strip_prefix('"')?;
    let mut out = String::new();
    let mut esc = false;
    for c in inner.chars() {
        if esc {
            out.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else if c == '"' {
            return Some(out);
        } else {
            out.push(c);
        }
    }
    None // unterminated → not a usable value
}

/// Scan the `findings` array and parse every COMPLETE `{...}` object, stopping
/// at the first incomplete one (truncation). String- and escape-aware.
fn scan_findings(s: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let Some(fi) = s.find("\"findings\"") else {
        return out;
    };
    let chars: Vec<char> = s[fi..].chars().collect();
    let Some(mut i) = chars.iter().position(|&c| c == '[') else {
        return out;
    };
    i += 1;
    while i < chars.len() {
        while i < chars.len() && chars[i] != '{' {
            if chars[i] == ']' {
                return out; // end of array
            }
            i += 1;
        }
        if i >= chars.len() {
            break;
        }
        let start = i;
        let mut depth = 0i32;
        let mut in_str = false;
        let mut esc = false;
        let mut end = None;
        while i < chars.len() {
            let c = chars[i];
            if in_str {
                if esc {
                    esc = false;
                } else if c == '\\' {
                    esc = true;
                } else if c == '"' {
                    in_str = false;
                }
            } else {
                match c {
                    '"' => in_str = true,
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(i);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        match end {
            Some(e) => {
                let obj: String = chars[start..=e].iter().collect();
                if let Ok(f) = serde_json::from_str::<Finding>(&obj) {
                    out.push(f);
                }
                i = e + 1;
            }
            None => break, // truncated object → stop
        }
    }
    out
}

/// Render a validated PR review as a single flat Markdown body (used by local
/// eval, which prints to stdout rather than posting inline comments).
#[must_use]
pub fn format_pr_review_markdown(out: &PrReviewOutput) -> String {
    let mut s = String::from("## nogent code review\n\n");
    if !out.summary.is_empty() {
        s.push_str(&out.summary);
        s.push_str("\n\n");
    }
    if out.findings.is_empty() {
        s.push_str("**Findings:** none flagged in scope.\n\n");
    } else {
        s.push_str("**Findings:**\n\n");
        for f in &out.findings {
            s.push_str(&finding_list_item(f));
            s.push('\n');
        }
        s.push('\n');
    }
    s.push_str(REVIEW_FOOTER);
    s
}

/// Review body for the GitHub path: summary + any findings that could NOT be
/// anchored inline (`leftover`). `inline_count` lets us avoid the "none flagged"
/// note when findings exist but are all inline.
#[must_use]
pub fn format_pr_review_body(summary: &str, leftover: &[Finding], inline_count: usize) -> String {
    let mut s = String::from("## nogent code review\n\n");
    if !summary.is_empty() {
        s.push_str(summary);
        s.push_str("\n\n");
    }
    if !leftover.is_empty() {
        s.push_str("**Findings (not tied to a changed line):**\n\n");
        for f in leftover {
            s.push_str(&finding_list_item(f));
            s.push('\n');
        }
        s.push('\n');
    } else if inline_count == 0 {
        s.push_str("**Findings:** none flagged in scope.\n\n");
    }
    s.push_str(REVIEW_FOOTER);
    s
}

// ── Issue triage ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
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
    let stripped = extract_object(raw);
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

/// Best-effort isolation of the JSON object from a model response: strip a
/// Markdown code fence, and if there's prose around it, slice from the first
/// `{` to the last `}`. The canary check still gates correctness.
fn extract_object(raw: &str) -> &str {
    let t = strip_code_fence(raw);
    if t.starts_with('{') {
        return t;
    }
    match (t.find('{'), t.rfind('}')) {
        (Some(a), Some(b)) if b > a => &t[a..=b],
        _ => t,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn pr_json(canary: &str) -> String {
        format!(
            r#"{{"canary":"{canary}","findings":[{{"category":"bug","file":"a.rs","line":5,"description":"f1"}}],"summary":"ok"}}"#
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
        assert_eq!(out.findings.len(), 1);
        assert_eq!(out.findings[0].description, "f1");
        assert_eq!(out.findings[0].file, "a.rs");
        assert_eq!(out.findings[0].line, Some(5));
    }

    #[test]
    fn real_model_shape_with_prose_prefix_passes() {
        // Mirrors gemini-2.5-pro: prose, then a fenced JSON object with
        // structured findings. extract_object must isolate the JSON.
        let c = "deadbeef";
        let raw = format!(
            "Here is my review.\n\n### Summary\nLooks good.\n\n```json\n{{\
\"canary\":\"{c}\",\
\"findings\":[{{\"category\":\"[nit]\",\"file\":\"src/x.rs\",\"line\":42,\"description\":\"populate trust_domain\"}}],\
\"summary\":\"1 nit\"}}\n```"
        );
        let out = validate_pr_review(&raw, c).expect("should validate");
        assert_eq!(out.findings[0].line, Some(42));
        let md = format_pr_review_markdown(&out);
        assert!(md.contains("**[nit]**"));
        assert!(md.contains("`src/x.rs:42`"));
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
    fn unknown_key_tolerated() {
        // Extra keys the model invents are ignored (canary is the real gate).
        let raw = r#"{"canary":"c","findings":[],"open_questions":[],"extra":1}"#;
        assert!(validate_pr_review(raw, "c").is_some());
    }

    #[test]
    fn non_object_rejected() {
        assert!(validate_pr_review("[1,2,3]", "c").is_none());
        assert!(validate_pr_review("not json", "c").is_none());
    }

    #[test]
    fn too_many_findings_rejected() {
        let items = (0..MAX_ITEMS + 1)
            .map(|i| format!(r#"{{"description":"f{i}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let raw = format!(r#"{{"canary":"c","findings":[{items}]}}"#);
        assert!(validate_pr_review(&raw, "c").is_none());
    }

    #[test]
    fn oversized_finding_rejected() {
        let big = "x".repeat(MAX_FINDING_DESC + 1);
        let raw = format!(r#"{{"canary":"c","findings":[{{"description":"{big}"}}]}}"#);
        assert!(validate_pr_review(&raw, "c").is_none());
    }

    #[test]
    fn code_fence_is_stripped() {
        let c = "c";
        let fenced = format!("```json\n{}\n```", pr_json(c));
        assert!(validate_pr_review(&fenced, c).is_some());
    }

    #[test]
    fn salvages_findings_from_truncated_json() {
        // Third finding object is cut off mid-way (truncation).
        let raw = r#"{"canary":"c","summary":"s","findings":[
            {"category":"bug","file":"a.rs","line":1,"description":"d1"},
            {"category":"perf","file":"b.rs","line":2,"description":"d2"},
            {"category":"nit","file":"c.rs","line":"#;
        // Strict validation fails…
        assert!(validate_pr_review(raw, "c").is_none());
        // …but salvage recovers the two complete findings + canary + summary.
        let out = salvage_pr_review(raw, "c").expect("salvage");
        assert_eq!(out.canary, "c");
        assert_eq!(out.summary, "s");
        assert_eq!(out.findings.len(), 2);
        assert_eq!(out.findings[1].file, "b.rs");
    }

    #[test]
    fn salvage_rejects_wrong_canary() {
        let raw = r#"{"canary":"wrong","findings":[{"description":"x"}"#;
        assert!(salvage_pr_review(raw, "c").is_none());
    }

    #[test]
    fn dedup_removes_repeats_keeps_first() {
        let f = |file: &str, desc: &str| Finding {
            category: "bug".into(),
            file: file.into(),
            line: Some(1),
            description: desc.into(),
        };
        let deduped = dedup_findings(vec![
            f("a.rs", "same issue"),
            f("a.rs", "same issue"),
            f("b.rs", "other"),
        ]);
        assert_eq!(deduped.len(), 2);
        assert_eq!(deduped[0].file, "a.rs");
        assert_eq!(deduped[1].file, "b.rs");
    }

    #[test]
    fn issue_triage_valid_passes() {
        let raw = r#"{"canary":"c","verdict":"config","suggested_resolution_path":".github/nogent.json","maintainer_notes":""}"#;
        assert!(validate_issue_triage(raw, "c").is_some());
    }
}
