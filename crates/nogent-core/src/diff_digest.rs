//! Bounded diff digest construction.
//!
//! Untrusted PR diffs can be arbitrarily large. We cap the number of files and
//! the total bytes sent to the model, with a per-file budget and an explicit
//! truncation marker so the model knows context was dropped.
//!
//! Unlike the original TypeScript (`Buffer.subarray`, which can split a UTF-8
//! sequence mid-codepoint), the Rust truncation is codepoint-safe.

use std::collections::{BTreeMap, BTreeSet};

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
            Some(p) => annotate_patch(&truncate_on_char_boundary(p, per_file_budget)),
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

/// Annotate each line of a unified-diff hunk with the **new-side line number**
/// as a `L<n>` prefix. Removed (`-`) lines have no new-side number and get a
/// blank-width prefix so the column stays aligned.
///
/// Why: models reading raw unified diffs have to mentally count from each
/// `@@ -a,b +c,d @@` header to determine the line number of each line, and
/// that count drifts across multi-hunk / multi-file diffs — producing inline
/// review comments anchored at the wrong line. Pre-annotating removes the
/// counting step: the model copies `L<n>` into the finding's `line` field.
#[must_use]
pub fn annotate_patch(patch: &str) -> String {
    let mut out = String::with_capacity(patch.len() + patch.len() / 8);
    let mut new_line: u64 = 0;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("@@") {
            // Pass the header through; reset new-side counter from "+c[,d]".
            out.push_str(line);
            out.push('\n');
            if let Some(plus) = rest.split('+').nth(1) {
                let num: String = plus
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                new_line = num.parse().unwrap_or(0);
            }
            continue;
        }
        match line.chars().next() {
            Some('+') => {
                if new_line > 0 {
                    out.push_str(&format!("L{new_line:<6} "));
                    new_line += 1;
                } else {
                    out.push_str("        ");
                }
                out.push_str(line);
                out.push('\n');
            }
            Some('-') => {
                // Removed line has no new-side number — keep column alignment.
                out.push_str("        ");
                out.push_str(line);
                out.push('\n');
            }
            _ => {
                // Context or blank line — advances the new side, commentable.
                if new_line > 0 {
                    out.push_str(&format!("L{new_line:<6} "));
                    new_line += 1;
                } else {
                    out.push_str("        ");
                }
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    out
}

#[derive(Debug, Clone)]
pub struct DiffDigest {
    pub text: String,
    pub files_included: usize,
    pub total_files: usize,
}

/// Map each changed file to a `line_number → line_content` table for the
/// new-side (RIGHT) lines visible in the diff. Content has the diff marker
/// (`+`, ` `) stripped. Used by the listener to verify that the model's
/// reported line actually contains the code the finding talks about — and to
/// snap to a nearby line if it doesn't.
#[must_use]
pub fn line_content_by_file(files: &[ChangedFile]) -> BTreeMap<String, BTreeMap<u64, String>> {
    let mut out = BTreeMap::new();
    for f in files {
        let Some(patch) = &f.patch else { continue };
        let mut content: BTreeMap<u64, String> = BTreeMap::new();
        let mut new_line: u64 = 0;
        for line in patch.lines() {
            if let Some(rest) = line.strip_prefix("@@") {
                if let Some(plus) = rest.split('+').nth(1) {
                    let num: String = plus
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    new_line = num.parse().unwrap_or(0);
                }
                continue;
            }
            match line.chars().next() {
                Some('+') | Some(' ') => {
                    if new_line > 0 {
                        content.insert(new_line, line.get(1..).unwrap_or("").to_string());
                    }
                    new_line += 1;
                }
                Some('-') => { /* removed, no new-side line */ }
                _ => {
                    // Blank or unknown — treat as context.
                    if new_line > 0 {
                        content.insert(new_line, String::new());
                    }
                    new_line += 1;
                }
            }
        }
        if !content.is_empty() {
            out.insert(f.filename.clone(), content);
        }
    }
    out
}

/// Map each changed file to the set of new-side (RIGHT) line numbers that fall
/// inside a diff hunk — the only lines GitHub will accept an inline review
/// comment on. Added (`+`) and context (` `) lines count; removed (`-`) lines do
/// not advance the new-side counter. A finding whose line isn't in this set must
/// go in the review body, or the whole `reviews` POST is rejected (422).
#[must_use]
pub fn commentable_lines(files: &[ChangedFile]) -> BTreeMap<String, BTreeSet<u64>> {
    let mut map = BTreeMap::new();
    for f in files {
        let Some(patch) = &f.patch else { continue };
        let mut lines = BTreeSet::new();
        let mut new_line: u64 = 0;
        for hunk in patch.lines() {
            if let Some(rest) = hunk.strip_prefix("@@") {
                // "@@ -a,b +c,d @@" — start the new-side counter at c.
                if let Some(plus) = rest.split('+').nth(1) {
                    let num: String = plus
                        .trim_start()
                        .chars()
                        .take_while(|c| c.is_ascii_digit())
                        .collect();
                    new_line = num.parse().unwrap_or(0);
                }
                continue;
            }
            match hunk.chars().next() {
                Some('+') => {
                    if new_line > 0 {
                        lines.insert(new_line);
                    }
                    new_line += 1;
                }
                Some('-') => { /* removed: new side does not advance */ }
                _ => {
                    // context line (or empty) — commentable, advances new side
                    if new_line > 0 {
                        lines.insert(new_line);
                    }
                    new_line += 1;
                }
            }
        }
        if !lines.is_empty() {
            map.insert(f.filename.clone(), lines);
        }
    }
    map
}

/// Identifiers referenced on ADDED diff lines (`+`), deduped in first-seen
/// order — used to pre-resolve their definitions from the repo so the model
/// doesn't have to navigate for them. Skips diff headers, short tokens, and
/// Rust keywords. Capped.
#[must_use]
pub fn referenced_identifiers(files: &[ChangedFile]) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for f in files {
        let Some(patch) = &f.patch else { continue };
        for line in patch.lines() {
            let Some(rest) = line.strip_prefix('+') else {
                continue;
            };
            if rest.starts_with("++") {
                continue; // "+++ b/file" header
            }
            for tok in identifiers_in(rest) {
                if tok.len() >= 3 && !is_rust_keyword(tok) && seen.insert(tok.to_string()) {
                    out.push(tok.to_string());
                    if out.len() >= 300 {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Split a line into identifier tokens (`[A-Za-z_][A-Za-z0-9_]*`).
fn identifiers_in(line: &str) -> Vec<&str> {
    let mut toks = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'_' || c.is_ascii_alphabetic() {
            let start = i;
            while i < bytes.len() && (bytes[i] == b'_' || bytes[i].is_ascii_alphanumeric()) {
                i += 1;
            }
            toks.push(&line[start..i]);
        } else {
            i += 1;
        }
    }
    toks
}

fn is_rust_keyword(t: &str) -> bool {
    matches!(
        t,
        "as" | "async"
            | "await"
            | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "union"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "let_else"
    )
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
    fn annotate_patch_prefixes_new_side_line_numbers() {
        let patch = "@@ -1,3 +10,5 @@\n line_10\n-removed\n+line_11\n+line_12\n line_13\n";
        let out = annotate_patch(patch);
        assert!(out.contains("@@ -1,3 +10,5 @@"));
        assert!(out.contains("L10"));
        assert!(out.contains("L11"));
        assert!(out.contains("L12"));
        assert!(out.contains("L13"));
        // Removed lines have no new-side number.
        for line in out.lines() {
            if line.contains("-removed") {
                assert!(!line.contains('L'));
            }
        }
    }

    #[test]
    fn annotate_patch_resets_counter_per_hunk() {
        let patch =
            "@@ -1 +1,2 @@\n+first_a\n+first_b\n@@ -10 +100,2 @@\n+second_a\n+second_b\n";
        let out = annotate_patch(patch);
        assert!(out.contains("L1     "));
        assert!(out.contains("L2     "));
        assert!(out.contains("L100   "));
        assert!(out.contains("L101   "));
    }

    #[test]
    fn line_content_by_file_indexes_new_side() {
        let patch = "@@ -1,2 +5,3 @@\n context\n-old\n+brand_new\n+another\n";
        let files = vec![file("a.rs", Some(patch))];
        let idx = line_content_by_file(&files);
        let entries = idx.get("a.rs").expect("file present");
        assert_eq!(entries.get(&5).map(String::as_str), Some("context"));
        assert_eq!(entries.get(&6).map(String::as_str), Some("brand_new"));
        assert_eq!(entries.get(&7).map(String::as_str), Some("another"));
        assert!(!entries.values().any(|v| v == "old"));
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
    fn commentable_lines_tracks_new_side() {
        let patch = "@@ -1,3 +1,4 @@\n context1\n-old\n+new1\n+new2\n context2";
        let files = vec![ChangedFile {
            filename: "a.rs".into(),
            status: "modified".into(),
            additions: 2,
            deletions: 1,
            patch: Some(patch.into()),
        }];
        let map = commentable_lines(&files);
        let lines = &map["a.rs"];
        // context1=1, new1=2, new2=3, context2=4 are commentable on the new side.
        assert!(
            lines.contains(&1) && lines.contains(&2) && lines.contains(&3) && lines.contains(&4)
        );
        assert!(!lines.contains(&5));
    }

    #[test]
    fn referenced_identifiers_from_added_lines() {
        let patch = "@@ -1 +1,2 @@\n+    let r = add_numbers(5, 10);\n+    let x = Foo::new();\n-    old_thing();";
        let files = vec![ChangedFile {
            filename: "m.rs".into(),
            status: "modified".into(),
            additions: 2,
            deletions: 1,
            patch: Some(patch.into()),
        }];
        let ids = referenced_identifiers(&files);
        assert!(ids.iter().any(|i| i == "add_numbers"));
        assert!(ids.iter().any(|i| i == "Foo"));
        // keywords filtered, removed-line idents excluded
        assert!(!ids.iter().any(|i| i == "let"));
        assert!(!ids.iter().any(|i| i == "old_thing"));
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
