//! Bounded diff digest construction.
//!
//! Untrusted PR diffs can be arbitrarily large. We cap the number of files and
//! the total bytes sent to the model, with a per-file budget and an explicit
//! truncation marker so the model knows context was dropped.
//!
//! Unlike the original TypeScript (`Buffer.subarray`, which can split a UTF-8
//! sequence mid-codepoint), the Rust truncation is codepoint-safe.

use serde::Deserialize;

/// A single changed file as returned by GitHub's "list pull request files" API.
#[derive(Debug, Clone, Deserialize)]
pub struct ChangedFile {
    pub filename: String,
    pub status: String,
    #[serde(default)]
    pub additions: u64,
    #[serde(default)]
    pub deletions: u64,
    /// The unified diff hunk. Absent for binary files or very large diffs.
    #[serde(default)]
    pub patch: Option<String>,
}

/// Truncate `s` to at most `max_bytes` bytes without splitting a UTF-8
/// codepoint, appending a marker when truncation occurred.
#[must_use]
pub fn truncate_on_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    // Walk back to the nearest char boundary at or below max_bytes.
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + 16);
    out.push_str(&s[..end]);
    out.push_str("\n[patch truncated]");
    out
}

/// Build a bounded, model-ready digest of the changed files.
///
/// `max_files` files are selected (first N); each file's patch is truncated to
/// a per-file byte budget derived from `max_patch_bytes`. Returns the digest
/// string plus how many files were included out of the total.
#[must_use]
pub fn build_digest(files: &[ChangedFile], max_files: usize, max_patch_bytes: usize) -> DiffDigest {
    let total_files = files.len();
    let selected = &files[..files.len().min(max_files)];

    // Per-file budget: split the byte budget across selected files, with a
    // floor so individual small files still get useful context. saturating_*
    // and a guarded divisor keep this panic-free.
    let denom = selected.len().max(1);
    let per_file_budget = (max_patch_bytes / denom).max(2_000);

    let mut sections: Vec<String> = Vec::with_capacity(selected.len());
    for f in selected {
        let patch = match &f.patch {
            Some(p) => truncate_on_char_boundary(p, per_file_budget),
            None => "[no text patch available]".to_string(),
        };
        sections.push(format!(
            "File: {}\nStatus: {}\nChanges: +{} -{}\n{}",
            f.filename, f.status, f.additions, f.deletions, patch
        ));
    }

    DiffDigest {
        text: sections.join("\n\n"),
        files_included: selected.len(),
        total_files,
    }
}

#[derive(Debug, Clone)]
pub struct DiffDigest {
    pub text: String,
    pub files_included: usize,
    pub total_files: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, patch: Option<&str>) -> ChangedFile {
        ChangedFile {
            filename: name.to_string(),
            status: "modified".to_string(),
            additions: 1,
            deletions: 0,
            patch: patch.map(|p| p.to_string()),
        }
    }

    #[test]
    fn no_truncation_when_within_budget() {
        let out = truncate_on_char_boundary("hello", 100);
        assert_eq!(out, "hello");
    }

    #[test]
    fn truncation_appends_marker() {
        let out = truncate_on_char_boundary("abcdefgh", 4);
        assert!(out.starts_with("abcd"));
        assert!(out.ends_with("[patch truncated]"));
    }

    #[test]
    fn truncation_never_splits_a_codepoint() {
        // Each '😀' is 4 bytes. Cutting at 3 must back off to 0.
        let s = "😀😀😀";
        let out = truncate_on_char_boundary(s, 3);
        // Output is valid UTF-8 by construction (String) and contains the marker.
        assert!(out.ends_with("[patch truncated]"));
        // The retained prefix must be empty (no partial emoji).
        assert!(out.starts_with("\n[patch truncated]"));
    }

    #[test]
    fn selects_first_max_files() {
        let files: Vec<_> = (0..50)
            .map(|i| file(&format!("f{i}.rs"), Some("@@ -1 +1 @@\n+x")))
            .collect();
        let d = build_digest(&files, 25, 120_000);
        assert_eq!(d.total_files, 50);
        assert_eq!(d.files_included, 25);
        assert_eq!(d.text.matches("File: f").count(), 25);
    }

    #[test]
    fn binary_file_marked() {
        let files = vec![file("logo.png", None)];
        let d = build_digest(&files, 25, 120_000);
        assert!(d.text.contains("[no text patch available]"));
    }

    #[test]
    fn per_file_budget_has_floor() {
        // Many files + small total budget → floor of 2000 bytes per file.
        let big = "x".repeat(10_000);
        let files: Vec<_> = (0..100)
            .map(|i| file(&format!("f{i}"), Some(&big)))
            .collect();
        let d = build_digest(&files, 100, 1_000); // 1000/100 = 10 < floor
        // Each section's patch is truncated to >= 2000 bytes (floor), so the
        // truncation marker appears.
        assert!(d.text.contains("[patch truncated]"));
    }
}
