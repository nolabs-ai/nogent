//! A bounded, in-memory snapshot of the repository at the PR head, built from
//! the GitHub tarball. Backs the agentic review tools (`definition`, `grep`,
//! `read_file`, `list_files`) and diff symbol pre-resolution, so the model can
//! resolve symbols and inspect files the diff references — without cloning to
//! disk or executing anything. Also builds a regex symbol table on construction.

use std::collections::BTreeMap;
use std::io::Read;

use flate2::read::GzDecoder;
use nogent_core::error::{NogentError, Result};
use regex::Regex;
use tar::Archive;

/// Skip individual files larger than this (vendored blobs, generated data).
const MAX_FILE_BYTES: usize = 1_000_000;
/// Cap returned content / match volume per tool call.
const READ_RETURN_CAP: usize = 60_000;
const GREP_LINE_CAP: usize = 300;
const LIST_CAP: usize = 800;

/// A symbol definition: where it's defined and its signature line.
#[derive(Debug, Clone)]
pub struct Def {
    pub file: String,
    pub line: u64,
    pub signature: String,
}

pub struct RepoIndex {
    files: BTreeMap<String, String>,
    /// symbol name → its definition site(s), for precise lookups without grep.
    symbols: BTreeMap<String, Vec<Def>>,
}

impl RepoIndex {
    /// Build from gzip'd tarball bytes (GitHub `/tarball/{ref}`). Returns
    /// `Ok(None)` if the indexed text exceeds `max_total_bytes` (caller falls
    /// back to diff-only). GitHub wraps everything in a top-level
    /// `owner-repo-<sha>/` dir, which we strip.
    pub fn from_tarball(gz_bytes: &[u8], max_total_bytes: usize) -> Result<Option<Self>> {
        let mut archive = Archive::new(GzDecoder::new(gz_bytes));
        let entries = archive
            .entries()
            .map_err(|e| NogentError::Io(format!("tarball read: {e}")))?;

        let mut files = BTreeMap::new();
        let mut total = 0usize;
        for entry in entries {
            let mut e = entry.map_err(|e| NogentError::Io(format!("tarball entry: {e}")))?;
            if !e.header().entry_type().is_file() {
                continue;
            }
            let size = e.size() as usize;
            if size > MAX_FILE_BYTES {
                continue;
            }
            let path = e
                .path()
                .map_err(|e| NogentError::Io(format!("tarball path: {e}")))?
                .to_string_lossy()
                .into_owned();
            // Strip the leading "owner-repo-sha/" component.
            let rel = match path.split_once('/') {
                Some((_, rest)) if !rest.is_empty() => rest.to_string(),
                _ => continue,
            };

            let mut buf = Vec::with_capacity(size);
            e.read_to_end(&mut buf)
                .map_err(|e| NogentError::Io(format!("tarball body: {e}")))?;
            if buf.contains(&0) {
                continue; // binary
            }
            if let Ok(text) = String::from_utf8(buf) {
                total = total.saturating_add(text.len());
                if total > max_total_bytes {
                    return Ok(None); // over cap → diff-only fallback
                }
                files.insert(rel, text);
            }
        }
        Ok(Some(Self::from_files(files)))
    }

    /// Build from a local directory (for `--review-local` eval). Skips VCS/build
    /// dirs and binary/large files. `Ok(None)` if it exceeds `max_total_bytes`.
    pub fn from_dir(root: &std::path::Path, max_total_bytes: usize) -> Result<Option<Self>> {
        let mut files = BTreeMap::new();
        let mut total = 0usize;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let rd = std::fs::read_dir(&dir)
                .map_err(|e| NogentError::Io(format!("read_dir {}: {e}", dir.display())))?;
            for entry in rd {
                let entry = entry.map_err(|e| NogentError::Io(e.to_string()))?;
                let ft = entry
                    .file_type()
                    .map_err(|e| NogentError::Io(e.to_string()))?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let path = entry.path();
                if ft.is_dir() {
                    if matches!(name.as_str(), ".git" | "target" | "node_modules" | ".cargo") {
                        continue;
                    }
                    stack.push(path);
                    continue;
                }
                if !ft.is_file() {
                    continue;
                }
                if entry
                    .metadata()
                    .map(|m| m.len() as usize)
                    .unwrap_or(usize::MAX)
                    > MAX_FILE_BYTES
                {
                    continue;
                }
                let bytes = std::fs::read(&path).map_err(|e| NogentError::Io(e.to_string()))?;
                if bytes.contains(&0) {
                    continue;
                }
                if let Ok(text) = String::from_utf8(bytes) {
                    let rel = path
                        .strip_prefix(root)
                        .map(|p| p.to_path_buf())
                        .unwrap_or_else(|_| path.clone())
                        .to_string_lossy()
                        .replace('\\', "/");
                    total = total.saturating_add(text.len());
                    if total > max_total_bytes {
                        return Ok(None);
                    }
                    files.insert(rel, text);
                }
            }
        }
        Ok(Some(Self::from_files(files)))
    }

    /// Assemble the index from a file map, building the symbol table once.
    fn from_files(files: BTreeMap<String, String>) -> Self {
        let symbols = build_symbols(&files);
        RepoIndex { files, symbols }
    }

    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    #[must_use]
    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// Definitions of `name`, each with a bounded source body — for the
    /// `definition` tool (precise + small vs grep-across-repo + whole-file read).
    #[must_use]
    pub fn definition(&self, name: &str, max_defs: usize) -> Vec<(String, u64, String)> {
        let Some(defs) = self.symbols.get(name) else {
            return Vec::new();
        };
        defs.iter()
            .take(max_defs)
            .map(|d| {
                let body = self
                    .files
                    .get(&d.file)
                    .map(|c| extract_body(c, d.line))
                    .unwrap_or_else(|| d.signature.clone());
                (d.file.clone(), d.line, body)
            })
            .collect()
    }

    /// A compact, bounded "definitions referenced by the diff" block: for each
    /// identifier with a known definition, its location + signature (no bodies —
    /// the model can call `definition`/`read_file` for those). Empty if none.
    #[must_use]
    pub fn referenced_defs_context(
        &self,
        idents: &[String],
        max_defs: usize,
        max_bytes: usize,
    ) -> String {
        let mut lines = Vec::new();
        let mut used = 0usize;
        for ident in idents {
            if lines.len() >= max_defs || used >= max_bytes {
                break;
            }
            if let Some(defs) = self.symbols.get(ident) {
                // At most one site per identifier in the pre-resolution hint.
                if let Some(d) = defs.first() {
                    let line = format!("- `{}` — {}:{}: `{}`", ident, d.file, d.line, d.signature);
                    used += line.len();
                    lines.push(line);
                }
            }
        }
        lines.join("\n")
    }

    /// Full content of a file (capped), or `None` if absent.
    #[must_use]
    pub fn read_file(&self, path: &str) -> Option<String> {
        self.files.get(path).map(|c| {
            if c.len() > READ_RETURN_CAP {
                format!(
                    "{}\n[file truncated]",
                    &c[..floor_char_boundary(c, READ_RETURN_CAP)]
                )
            } else {
                c.clone()
            }
        })
    }

    /// Lines matching `pattern` (regex), as (path, line_no, line). Bounded.
    pub fn grep(
        &self,
        pattern: &str,
        max_matches: usize,
    ) -> std::result::Result<Vec<(String, usize, String)>, String> {
        let re = Regex::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;
        let cap = max_matches.clamp(1, 200);
        let mut out = Vec::new();
        for (path, content) in &self.files {
            for (i, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    let snippet: String = line.chars().take(GREP_LINE_CAP).collect();
                    out.push((path.clone(), i + 1, snippet));
                    if out.len() >= cap {
                        return Ok(out);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Paths, optionally filtered by a plain substring, capped.
    #[must_use]
    pub fn list_files(&self, contains: Option<&str>) -> Vec<String> {
        self.files
            .keys()
            .filter(|p| contains.is_none_or(|c| p.contains(c)))
            .take(LIST_CAP)
            .cloned()
            .collect()
    }
}

/// Scan `.rs` files for top-level Rust definitions (regex, not a full parser —
/// good enough to locate a symbol's definition site + signature).
fn build_symbols(files: &BTreeMap<String, String>) -> BTreeMap<String, Vec<Def>> {
    // Compiled once per index build (once per review).
    let fn_re = Regex::new(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:default\s+)?(?:async\s+)?(?:const\s+)?(?:unsafe\s+)?(?:extern\s+\x22[^\x22]*\x22\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)",
    );
    let ty_re = Regex::new(
        r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:struct|enum|trait|union|type|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)",
    );
    let macro_re = Regex::new(r"^\s*macro_rules!\s+([A-Za-z_][A-Za-z0-9_]*)");
    let (Ok(fn_re), Ok(ty_re), Ok(macro_re)) = (fn_re, ty_re, macro_re) else {
        return BTreeMap::new();
    };

    let mut symbols: BTreeMap<String, Vec<Def>> = BTreeMap::new();
    for (path, content) in files {
        if !path.ends_with(".rs") {
            continue;
        }
        for (i, line) in content.lines().enumerate() {
            let name = fn_re
                .captures(line)
                .or_else(|| ty_re.captures(line))
                .or_else(|| macro_re.captures(line))
                .and_then(|c| c.get(1))
                .map(|m| m.as_str().to_string());
            if let Some(name) = name {
                let sig: String = line.trim().chars().take(200).collect();
                let entry = symbols.entry(name).or_default();
                if entry.len() < 8 {
                    entry.push(Def {
                        file: path.clone(),
                        line: (i + 1) as u64,
                        signature: sig,
                    });
                }
            }
        }
    }
    symbols
}

/// Extract a bounded source body starting at `start_line` (1-based): from the
/// definition line until braces balance (or a `;` for declarations), capped.
fn extract_body(content: &str, start_line: u64) -> String {
    const MAX_LINES: usize = 120;
    const MAX_BYTES: usize = 6_000;
    let lines: Vec<&str> = content.lines().collect();
    let start = (start_line.saturating_sub(1)) as usize;
    let mut out = String::new();
    let mut depth: i32 = 0;
    let mut seen_brace = false;
    for line in lines.iter().skip(start).take(MAX_LINES) {
        out.push_str(line);
        out.push('\n');
        for c in line.chars() {
            match c {
                '{' => {
                    depth += 1;
                    seen_brace = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if (seen_brace && depth <= 0) || (!seen_brace && line.trim_end().ends_with(';')) {
            break;
        }
        if out.len() >= MAX_BYTES {
            out.push_str("[…truncated]\n");
            break;
        }
    }
    out
}

/// Largest char boundary <= `max` (so slicing never splits a codepoint).
fn floor_char_boundary(s: &str, max: usize) -> usize {
    let mut end = max.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a gzip'd tar with a leading owner-repo-sha/ prefix, like GitHub.
    fn make_tarball(files: &[(&str, &str)]) -> Vec<u8> {
        let mut tar_bytes = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_bytes);
            for (name, body) in files {
                let full = format!("acme-repo-deadbeef/{name}");
                let mut header = tar::Header::new_gnu();
                header.set_size(body.len() as u64);
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, &full, body.as_bytes())
                    .expect("append");
            }
            builder.finish().expect("finish");
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(&tar_bytes).expect("gz write");
        gz.finish().expect("gz finish")
    }

    #[test]
    fn indexes_and_strips_prefix() {
        let gz = make_tarball(&[(
            "src/math.rs",
            "fn add_numbers(a: i32, b: i32) -> i32 { a + b }",
        )]);
        let idx = RepoIndex::from_tarball(&gz, 10_000_000)
            .expect("ok")
            .expect("some");
        assert_eq!(idx.file_count(), 1);
        assert!(
            idx.read_file("src/math.rs")
                .unwrap()
                .contains("add_numbers")
        );
        assert!(idx.read_file("nope.rs").is_none());
    }

    #[test]
    fn grep_finds_definition() {
        let gz = make_tarball(&[
            (
                "src/math.rs",
                "fn add_numbers(a: i32, b: i32) -> i32 {\n    a + b\n}",
            ),
            ("src/main.rs", "let r = add_numbers(5, 10);"),
        ]);
        let idx = RepoIndex::from_tarball(&gz, 10_000_000)
            .expect("ok")
            .expect("some");
        let hits = idx.grep("fn add_numbers", 50).expect("grep");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "src/math.rs");
        assert_eq!(hits[0].1, 1);
    }

    #[test]
    fn over_cap_returns_none() {
        let big = "x".repeat(20_000);
        let gz = make_tarball(&[("a.txt", &big), ("b.txt", &big)]);
        // Cap below total → None (caller falls back to diff-only).
        assert!(RepoIndex::from_tarball(&gz, 25_000).expect("ok").is_none());
    }

    #[test]
    fn symbol_index_resolves_definitions_and_context() {
        let gz = make_tarball(&[
            (
                "src/math.rs",
                "pub fn add_numbers(a: i32, b: i32) -> i32 {\n    a + b\n}\n\npub struct Calc;",
            ),
            ("src/main.rs", "let r = add_numbers(5, 10);"),
        ]);
        let idx = RepoIndex::from_tarball(&gz, 10_000_000)
            .expect("ok")
            .expect("some");
        assert!(idx.symbol_count() >= 2); // add_numbers + Calc

        let defs = idx.definition("add_numbers", 3);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].0, "src/math.rs");
        assert!(defs[0].2.contains("a + b")); // body extracted, not just signature

        let ctx = idx.referenced_defs_context(
            &[
                "add_numbers".to_string(),
                "Calc".to_string(),
                "missing".to_string(),
            ],
            10,
            10_000,
        );
        assert!(ctx.contains("add_numbers") && ctx.contains("src/math.rs:1"));
        assert!(ctx.contains("Calc"));
        assert!(!ctx.contains("missing"));
    }

    #[test]
    fn list_files_filters_by_substring() {
        let gz = make_tarball(&[("src/a.rs", "x"), ("docs/b.md", "y")]);
        let idx = RepoIndex::from_tarball(&gz, 10_000_000)
            .expect("ok")
            .expect("some");
        assert_eq!(idx.list_files(Some(".rs")), vec!["src/a.rs"]);
        assert_eq!(idx.list_files(None).len(), 2);
    }
}
