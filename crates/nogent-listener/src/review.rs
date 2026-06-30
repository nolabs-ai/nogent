//! PR review orchestration (general code review + sandbox security), run
//! in-process. To keep token cost down we send the diff + pre-resolved
//! definitions of symbols it references (NOT full file content), and let the
//! model pull what it needs on demand via tools (`definition`, `grep`,
//! `read_file`, `list_files`) in a bounded agentic loop, then a fixed sequence
//! of focused critique lenses (coverage/parity, sinks, bypass) to cut variance.

use nogent_core::diff_digest::{
    DiffDigest, build_digest, commentable_lines, line_content_by_file, referenced_identifiers,
};
use nogent_core::error::Result;
use nogent_core::events::{EventJob, JobKind};
use nogent_core::gemini::{Content, FunctionCall, FunctionDeclaration, Part, Tool};
use nogent_core::output_validator::{
    FALLBACK_MESSAGE, PrReviewOutput, dedup_findings, finding_inline_body, format_pr_review_body,
    format_pr_review_markdown, generate_canary, salvage_pr_review, validate_pr_review,
};
use nogent_core::prompts::pr_review;
use nogent_core::repo_config::ResolvedConfig;
use serde_json::{Value, json};

use crate::config::ListenerConfig;
use crate::gemini_client::GeminiClient;
use crate::github_client::{GithubClient, InlineComment};
use crate::repo_index::RepoIndex;

/// Agentic loop bounds.
const MAX_TURNS: usize = 12;
/// Per critique-lens turn cap — each lens is focused, so it converges fast.
const CRITIQUE_MAX_TURNS: usize = 5;
const MAX_TOOL_OUTPUT_BYTES: usize = 600_000;

/// Focused second-pass lenses, run in sequence over the same conversation to
/// reduce run-to-run variance (each forces one high-impact angle every time).
const CRITIQUE_LENSES: &[(&str, &str)] = &[
    (
        "coverage",
        "COVERAGE & PARITY: for every control, validation, or handling this change adds, find its \
siblings — other code paths (parallel protocols/transports, other entry points), other variants, \
and other inputs/fields of the same kind — and confirm each gets the same treatment. A path or a \
new field that skips the validation or enforcement its peers have is a finding. Use grep/definition \
to locate the siblings.",
    ),
    (
        "sinks",
        "SINKS: trace every externally-influenced value this change introduces or touches to where \
it is used (a request, header, query, command, path, or config) and confirm it is validated or \
escaped for that destination before use.",
    ),
    (
        "bypass",
        "BYPASS: enumerate how any resource this change protects could still be reached WITHOUT the \
new control — other entry points, transports, fallbacks, or default-open paths — and check each.",
    ),
];
/// Repo snapshot bounds.
const MAX_TARBALL_BYTES: usize = 80_000_000;
const MAX_INDEX_BYTES: usize = 50_000_000;
/// Cap on maintainer guidance (NOGENT.md) folded into the system prompt.
const GUIDANCE_MAX_BYTES: usize = 16_384;

pub async fn run(cfg: &ListenerConfig, token: &str, job: &EventJob) -> Result<()> {
    let gh = GithubClient::new(token)?;
    let files = gh.list_pr_files(&job.owner, &job.repo, job.number).await?;
    let digest = build_digest(&files, job.config.max_files, job.config.max_patch_bytes);

    let head = job.head_sha.as_deref().unwrap_or_default();

    // Repo snapshot at the PR head for navigation tools + symbol resolution.
    let index = if head.is_empty() {
        None
    } else {
        match gh
            .download_tarball(&job.owner, &job.repo, head, MAX_TARBALL_BYTES)
            .await?
        {
            Some(bytes) => RepoIndex::from_tarball(&bytes, MAX_INDEX_BYTES)?,
            None => None,
        }
    };

    // Pre-resolve definitions of symbols the diff references so the model rarely
    // needs to navigate. We deliberately do NOT pre-send full file content — that
    // is the dominant resent-token cost; the model reads files on demand via the
    // tools instead.
    let context = match index.as_ref() {
        Some(idx) => {
            let idents = referenced_identifiers(&files);
            idx.referenced_defs_context(&idents, 40, job.config.max_context_bytes)
        }
        None => String::new(),
    };
    let user = pr_review::user_prompt(job, &digest, &context);

    // Maintainer guidance from NOGENT.md on the BASE ref (trusted — a fork PR
    // cannot change the base, so this can safely steer the system prompt).
    let guidance = match job.base_sha.as_deref() {
        Some(base) if !base.is_empty() => {
            gh.get_repo_guidance(&job.owner, &job.repo, base, GUIDANCE_MAX_BYTES)
                .await?
        }
        _ => None,
    };

    let canary = generate_canary();
    let system = pr_review::system_instruction(&canary, guidance.as_deref());
    let gemini = GeminiClient::new(
        &cfg.gemini_api_key,
        &job.model,
        cfg.gemini_thinking_level.as_deref(),
    )?;

    let mut session = AgentSession::new(&gemini, &system, index.as_ref(), user);
    let review = run_review(&mut session, &canary).await?;

    match review {
        Some(out) => {
            // Anchor findings with a valid changed-line as inline comments; the
            // rest (no line, or a line outside the diff) go in the body so the
            // reviews POST never 422s on an un-commentable line.
            //
            // Defensive: even with line-annotated diffs, the model can still pick
            // a line a few off — snap within ±2 to the nearest commentable line
            // whose content matches a backtick-quoted code token from the
            // description.
            let valid = commentable_lines(&files);
            let line_content = line_content_by_file(&files);
            let mut inline: Vec<InlineComment> = Vec::new();
            let mut leftover = Vec::new();
            for f in &out.findings {
                let snapped = f.line.and_then(|l| {
                    if f.file.is_empty() {
                        return None;
                    }
                    let commentable = valid.get(&f.file)?;
                    let content = line_content.get(&f.file);
                    Some(snap_to_token_match(l, commentable, content, &f.description))
                });
                let anchored =
                    snapped.filter(|l| valid.get(&f.file).is_some_and(|s| s.contains(l)));
                match anchored {
                    Some(line) => inline.push(InlineComment {
                        path: f.file.clone(),
                        line,
                        side: "RIGHT".to_string(),
                        body: finding_inline_body(f),
                    }),
                    None => leftover.push(f.clone()),
                }
            }
            let body = format_pr_review_body(&out.summary, &leftover, inline.len());
            let inline_count = inline.len();
            gh.post_pr_review_with_comments(&job.owner, &job.repo, job.number, &body, inline)
                .await?;
            tracing::info!(
                pr = job.number,
                files = digest.files_included,
                inline = inline_count,
                indexed_files = index.as_ref().map(RepoIndex::file_count).unwrap_or(0),
                indexed_symbols = index.as_ref().map(RepoIndex::symbol_count).unwrap_or(0),
                "posted PR review"
            );
        }
        None => {
            tracing::warn!(
                pr = job.number,
                "model output failed validation after salvage + retry; posting fallback"
            );
            gh.post_pr_review(&job.owner, &job.repo, job.number, FALLBACK_MESSAGE)
                .await?;
        }
    }
    let u = gemini.usage();
    tracing::info!(
        pr = job.number,
        gemini_calls = u.calls,
        tokens_in = u.input_tokens,
        tokens_out = u.output_tokens,
        thinking_tokens = u.thinking_tokens,
        cached_tokens = u.cached_tokens,
        "gemini token usage"
    );
    Ok(())
}

/// A stateful review conversation. Holds the running `contents` so we can drive
/// several "ask → converge" rounds (initial review, then self-critique) over one
/// context without re-sending the diff. `index = None` → no tools (diff-only).
struct AgentSession<'a> {
    gemini: &'a GeminiClient,
    system: &'a str,
    index: Option<&'a RepoIndex>,
    tools: Vec<Tool>,
    contents: Vec<Content>,
    tool_bytes: usize,
}

impl<'a> AgentSession<'a> {
    fn new(
        gemini: &'a GeminiClient,
        system: &'a str,
        index: Option<&'a RepoIndex>,
        first_user: String,
    ) -> Self {
        let tools = if index.is_some() {
            repo_tools()
        } else {
            Vec::new()
        };
        AgentSession {
            gemini,
            system,
            index,
            tools,
            contents: vec![Content::user_text(first_user)],
            tool_bytes: 0,
        }
    }

    /// Append a user turn (e.g. the self-critique instruction).
    fn say(&mut self, text: String) {
        self.contents.push(Content::user_text(text));
    }

    /// Run the tool loop until the model emits a final (text) answer, the turn
    /// cap is hit, or the shared tool-output budget is spent. The answer is
    /// retained in history so a follow-up round can build on it.
    async fn answer(&mut self, max_turns: usize) -> Result<String> {
        for _ in 0..max_turns {
            let active: Vec<Tool> = if self.tool_bytes > MAX_TOOL_OUTPUT_BYTES {
                Vec::new()
            } else {
                self.tools.clone()
            };
            let parts = self
                .gemini
                .generate_turn(self.system, &self.contents, &active)
                .await?;
            let calls: Vec<FunctionCall> = parts
                .iter()
                .filter_map(|p| p.function_call.clone())
                .collect();
            if calls.is_empty() {
                let text = concat_text(&parts);
                self.contents.push(Content::model(parts));
                return Ok(text);
            }
            self.contents.push(Content::model(parts.clone()));
            let mut responses = Vec::with_capacity(calls.len());
            for call in &calls {
                tracing::info!(tool = %call.name, args = %call.args, "review tool call");
                let result = self
                    .index
                    .map(|idx| dispatch_tool(idx, call))
                    .unwrap_or_else(|| json!({"error": "no repository index available"}));
                self.tool_bytes = self
                    .tool_bytes
                    .saturating_add(serde_json::to_string(&result).map(|s| s.len()).unwrap_or(0));
                responses.push(Part::function_response(
                    call.id.as_deref(),
                    &call.name,
                    result,
                ));
            }
            self.contents.push(Content::tool_results(responses));
        }
        self.say("Stop investigating and output your final JSON review now.".to_string());
        let parts = self
            .gemini
            .generate_turn(self.system, &self.contents, &[])
            .await?;
        let text = concat_text(&parts);
        self.contents.push(Content::model(parts));
        Ok(text)
    }
}

/// Produce the merged review: initial pass + a self-critique pass for anything
/// missed, deduped. `None` only if the first pass couldn't yield valid output
/// (even after salvage + one retry).
async fn run_review(
    session: &mut AgentSession<'_>,
    canary: &str,
) -> Result<Option<PrReviewOutput>> {
    let raw1 = session.answer(MAX_TURNS).await?;
    let Some(mut out) = finalize(session, &raw1, canary).await? else {
        return Ok(None);
    };

    // Deterministic self-critique: run each focused lens in sequence so every
    // high-impact angle is checked on every review (cuts run-to-run variance).
    // The big context stays cached across these passes.
    let mut all = std::mem::take(&mut out.findings);
    for (label, instruction) in CRITIQUE_LENSES {
        session.say(lens_prompt(instruction, canary));
        let raw = session.answer(CRITIQUE_MAX_TURNS).await?;
        if let Some(extra) =
            validate_pr_review(&raw, canary).or_else(|| salvage_pr_review(&raw, canary))
        {
            tracing::info!(lens = label, added = extra.findings.len(), "critique lens");
            all.extend(extra.findings);
        }
    }
    out.findings = dedup_findings(all); // dedup across all passes + severity-sort
    Ok(Some(out))
}

/// Validate the first answer; on failure try salvage, then one re-ask for a
/// complete object. Prevents a truncated/invalid response from yielding nothing.
async fn finalize(
    session: &mut AgentSession<'_>,
    raw: &str,
    canary: &str,
) -> Result<Option<PrReviewOutput>> {
    if let Some(out) = validate_pr_review(raw, canary) {
        return Ok(Some(out));
    }
    if let Some(out) = salvage_pr_review(raw, canary) {
        tracing::warn!("recovered review from truncated/invalid output");
        return Ok(Some(out));
    }
    session.say(format!(
        "Your previous response was not a single valid JSON object (it may have been \
truncated). Re-send the COMPLETE JSON object only — including the canary \"{canary}\" — \
and keep finding descriptions concise."
    ));
    let raw2 = session.answer(3).await?;
    Ok(validate_pr_review(&raw2, canary).or_else(|| salvage_pr_review(&raw2, canary)))
}

/// Wrap a focused lens instruction with the shared output contract.
fn lens_prompt(instruction: &str, canary: &str) -> String {
    format!(
        "Now a focused, adversarial pass for HIGH-IMPACT issues you may have missed — the kind \
that are absent rather than wrong-on-a-line. {instruction}\n\
Output the SAME JSON object containing ONLY additional findings (do not repeat ones you already \
raised); use an empty findings array if there are none. Include the canary \"{canary}\"."
    )
}

/// Local eval entrypoint: review a diff against a locally-built repo index using
/// real Gemini, returning the Markdown review. No GitHub, no webhook, no posting.
pub async fn run_local(
    api_key: &str,
    model: &str,
    thinking_level: Option<&str>,
    diff_text: &str,
    index: &RepoIndex,
) -> Result<String> {
    // For local eval the index IS your local checkout (trusted), so read
    // NOGENT.md from it directly to exercise repo guidance.
    let guidance = index
        .read_file("NOGENT.md")
        .or_else(|| index.read_file(".github/nogent.md"));
    let canary = generate_canary();
    let system = pr_review::system_instruction(&canary, guidance.as_deref());
    let digest = DiffDigest {
        text: diff_text.to_string(),
        files_included: 0,
        total_files: 0,
    };
    let job = eval_job(model);
    let user = pr_review::user_prompt(&job, &digest, "");
    let gemini = GeminiClient::new(api_key, model, thinking_level)?;
    let mut session = AgentSession::new(&gemini, &system, Some(index), user);
    let review = run_review(&mut session, &canary).await?;
    let u = gemini.usage();
    eprintln!(
        "tokens: in={} (cached={}) out={} thinking={} (calls={})",
        u.input_tokens, u.cached_tokens, u.output_tokens, u.thinking_tokens, u.calls
    );
    Ok(match review {
        Some(out) => format_pr_review_markdown(&out),
        None => {
            tracing::warn!("model output failed validation after salvage + retry");
            FALLBACK_MESSAGE.to_string()
        }
    })
}

fn eval_job(model: &str) -> EventJob {
    EventJob {
        kind: JobKind::PrReview,
        repo_full_name: "local/eval".to_string(),
        owner: "local".to_string(),
        repo: "eval".to_string(),
        number: 0,
        title: "local eval".to_string(),
        body: String::new(),
        author: "local".to_string(),
        html_url: String::new(),
        default_branch: "main".to_string(),
        base_ref: None,
        base_sha: None,
        head_ref: None,
        head_sha: None,
        config: ResolvedConfig::default(),
        model: model.to_string(),
    }
}

fn concat_text(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| p.text.clone())
        .collect::<String>()
}

/// Window for the line-anchor safety net.
const SNAP_WINDOW: u64 = 2;

/// Return the model's `line` unchanged if its content matches a backtick-quoted
/// token from the description, otherwise look ±`SNAP_WINDOW` lines for the
/// first commentable line whose content does — handy when the model drifts a
/// line or two from the true location.
fn snap_to_token_match(
    line: u64,
    commentable: &std::collections::BTreeSet<u64>,
    content: Option<&std::collections::BTreeMap<u64, String>>,
    description: &str,
) -> u64 {
    let Some(content) = content else { return line };
    let tokens = backticked_tokens(description);
    if tokens.is_empty() {
        return line;
    }
    let line_matches = |l: u64| -> bool {
        content
            .get(&l)
            .map(|c| tokens.iter().any(|t| c.contains(t.as_str())))
            .unwrap_or(false)
    };
    if line_matches(line) {
        return line;
    }
    // Try ±1, ±2 — prefer closer offsets; on tie, prefer earlier line.
    for offset in 1..=SNAP_WINDOW {
        let below = line.saturating_sub(offset);
        if below != line && commentable.contains(&below) && line_matches(below) {
            return below;
        }
        let above = line.saturating_add(offset);
        if above != line && commentable.contains(&above) && line_matches(above) {
            return above;
        }
    }
    line
}

/// Pull out the substrings between backticks in `s`, skipping empty/short
/// tokens. Used by the snap heuristic — these are the bits of code the model
/// most often quotes when describing a finding (`pickle.loads`, `eval`, …).
fn backticked_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_tick = false;
    let mut cur = String::new();
    for ch in s.chars() {
        if ch == '`' {
            if in_tick && cur.len() >= 3 && !cur.contains('\n') {
                out.push(cur.clone());
            }
            cur.clear();
            in_tick = !in_tick;
        } else if in_tick {
            cur.push(ch);
        }
    }
    out
}

/// Tool declarations exposed to the model.
fn repo_tools() -> Vec<Tool> {
    vec![Tool {
        function_declarations: vec![
            FunctionDeclaration {
                name: "grep".to_string(),
                description:
                    "Search the repository at the PR head for lines matching a Rust regex. \
Use it to locate definitions (e.g. \"fn add_numbers\"), call sites, or symbols the diff references."
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "pattern": {"type": "string", "description": "a Rust regex"},
                        "max_results": {"type": "integer", "description": "max matches (default 50)"}
                    },
                    "required": ["pattern"]
                }),
            },
            FunctionDeclaration {
                name: "definition".to_string(),
                description:
                    "Resolve a symbol (function, type, trait, const, macro) by name to its \
definition site(s) and source body. Prefer this over grep+read_file when you need what a \
referenced symbol does — it's precise and returns just that definition."
                        .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }),
            },
            FunctionDeclaration {
                name: "read_file".to_string(),
                description: "Return the full text of a repository file at the PR head, by path."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }),
            },
            FunctionDeclaration {
                name: "list_files".to_string(),
                description: "List repository file paths, optionally filtered by a path substring."
                    .to_string(),
                parameters: json!({
                    "type": "object",
                    "properties": {"contains": {"type": "string"}}
                }),
            },
        ],
    }]
}

fn dispatch_tool(index: &RepoIndex, call: &FunctionCall) -> Value {
    match call.name.as_str() {
        "grep" => {
            let pattern = call
                .args
                .get("pattern")
                .and_then(Value::as_str)
                .unwrap_or("");
            if pattern.is_empty() {
                return json!({"error": "pattern is required"});
            }
            let max = call
                .args
                .get("max_results")
                .and_then(Value::as_u64)
                .unwrap_or(50) as usize;
            match index.grep(pattern, max) {
                Ok(hits) => json!({
                    "matches": hits.into_iter()
                        .map(|(path, line, text)| json!({"path": path, "line": line, "text": text}))
                        .collect::<Vec<_>>()
                }),
                Err(e) => json!({"error": e}),
            }
        }
        "definition" => {
            let name = call.args.get("name").and_then(Value::as_str).unwrap_or("");
            let defs = index.definition(name, 3);
            if defs.is_empty() {
                json!({"error": "no definition found", "name": name})
            } else {
                json!({
                    "definitions": defs.into_iter()
                        .map(|(file, line, body)| json!({"file": file, "line": line, "body": body}))
                        .collect::<Vec<_>>()
                })
            }
        }
        "read_file" => {
            let path = call.args.get("path").and_then(Value::as_str).unwrap_or("");
            match index.read_file(path) {
                Some(content) => json!({"path": path, "content": content}),
                None => json!({"error": "file not found", "path": path}),
            }
        }
        "list_files" => {
            let contains = call.args.get("contains").and_then(Value::as_str);
            json!({"files": index.list_files(contains)})
        }
        other => json!({"error": format!("unknown tool: {other}")}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn backticked_tokens_extracts_code_quotes() {
        let s = "Use `pickle.loads` on `request.get_data()` — RCE in `import_state`.";
        let toks = backticked_tokens(s);
        assert_eq!(
            toks,
            vec!["pickle.loads", "request.get_data()", "import_state"]
        );
    }

    #[test]
    fn snap_keeps_line_when_already_matches() {
        let commentable: BTreeSet<u64> = (1..=20).collect();
        let mut content: BTreeMap<u64, String> = BTreeMap::new();
        content.insert(12, "    return eval(expression)".into());
        let desc = "uses `eval` on untrusted input";
        assert_eq!(
            snap_to_token_match(12, &commentable, Some(&content), desc),
            12,
        );
    }

    #[test]
    fn snap_corrects_off_by_two_drift() {
        let commentable: BTreeSet<u64> = (1..=20).collect();
        let mut content: BTreeMap<u64, String> = BTreeMap::new();
        content.insert(10, "def calculate(expression):".into());
        content.insert(11, r#"    """evaluate""""#.into());
        content.insert(12, "    return eval(expression)".into());
        let desc = "the `calculate` function uses `eval` — RCE";
        // Model said 10; the eval is at 12 (offset +2); should snap to 12.
        // (10 also matches `calculate` though, so 10 wins as the original.)
        assert_eq!(
            snap_to_token_match(10, &commentable, Some(&content), desc),
            10,
        );
        // Model said 14 (no token); within ±2 only 12 matches → snap to 12.
        assert_eq!(
            snap_to_token_match(14, &commentable, Some(&content), desc),
            12,
        );
    }

    #[test]
    fn snap_keeps_line_when_no_match_in_window() {
        let commentable: BTreeSet<u64> = (1..=20).collect();
        let mut content: BTreeMap<u64, String> = BTreeMap::new();
        content.insert(5, "unrelated".into());
        let desc = "uses `eval` — RCE";
        assert_eq!(
            snap_to_token_match(5, &commentable, Some(&content), desc),
            5,
        );
    }

    #[test]
    fn snap_no_op_without_backtick_tokens() {
        let commentable: BTreeSet<u64> = (1..=10).collect();
        let mut content: BTreeMap<u64, String> = BTreeMap::new();
        content.insert(3, "eval(x)".into());
        let desc = "no backticks in this description at all";
        assert_eq!(
            snap_to_token_match(7, &commentable, Some(&content), desc),
            7,
        );
    }
}
