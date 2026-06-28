//! PR review orchestration (general code review + sandbox security), run
//! in-process. The model gets the diff and the full changed files up front, and
//! can navigate the rest of the repo at the PR head via tools (grep, read_file,
//! list_files) in a bounded agentic loop — so it can resolve symbols the diff
//! references (e.g. find the definition of a called function).

use nogent_core::diff_digest::{
    DiffDigest, FileContent, build_digest, build_file_context, commentable_lines,
};
use nogent_core::error::Result;
use nogent_core::events::{EventJob, JobKind};
use nogent_core::gemini::{Content, FunctionCall, FunctionDeclaration, Part, Tool};
use nogent_core::output_validator::{
    FALLBACK_MESSAGE, finding_inline_body, format_pr_review_body, format_pr_review_markdown,
    generate_canary, validate_pr_review,
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
const MAX_TOOL_OUTPUT_BYTES: usize = 600_000;
/// Repo snapshot bounds.
const MAX_TARBALL_BYTES: usize = 80_000_000;
const MAX_INDEX_BYTES: usize = 50_000_000;
/// Cap on maintainer guidance (NOGENT.md) folded into the system prompt.
const GUIDANCE_MAX_BYTES: usize = 16_384;

pub async fn run(cfg: &ListenerConfig, token: &str, job: &EventJob) -> Result<()> {
    let gh = GithubClient::new(token)?;
    let files = gh.list_pr_files(&job.owner, &job.repo, job.number).await?;
    let digest = build_digest(&files, job.config.max_files, job.config.max_patch_bytes);

    // Full content of the changed files, as the model's starting context.
    let head = job.head_sha.as_deref().unwrap_or_default();
    let selected = &files[..files.len().min(job.config.max_files)];
    let mut contents_for_ctx: Vec<FileContent> = Vec::new();
    if !head.is_empty() {
        for f in selected {
            if f.status == "removed" || f.patch.is_none() {
                continue;
            }
            if let Some(c) = gh
                .get_file_raw(&job.owner, &job.repo, &f.filename, head)
                .await?
            {
                contents_for_ctx.push(FileContent {
                    filename: f.filename.clone(),
                    content: c,
                });
            }
        }
    }
    let file_context = build_file_context(&contents_for_ctx, job.config.max_context_bytes);
    let user = pr_review::user_prompt(job, &digest, &file_context);

    // Repo snapshot at the PR head for navigation tools (bounded; None → the
    // model reviews from diff + changed files only).
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

    let raw = match index.as_ref() {
        Some(idx) => run_agentic(&gemini, &system, &user, idx).await?,
        None => {
            let parts = gemini
                .generate_turn(&system, &[Content::user_text(user)], &[])
                .await?;
            concat_text(&parts)
        }
    };

    match validate_pr_review(&raw, &canary) {
        Some(out) => {
            // Anchor findings with a valid changed-line as inline comments; the
            // rest (no line, or a line outside the diff) go in the body so the
            // reviews POST never 422s on an un-commentable line.
            let valid = commentable_lines(&files);
            let mut inline: Vec<InlineComment> = Vec::new();
            let mut leftover = Vec::new();
            for f in &out.findings {
                let anchored = f.line.filter(|l| {
                    !f.file.is_empty() && valid.get(&f.file).is_some_and(|s| s.contains(l))
                });
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
                "posted PR review"
            );
        }
        None => {
            tracing::warn!(
                pr = job.number,
                raw = %raw.chars().take(6000).collect::<String>(),
                "model output failed validation; posting fallback"
            );
            gh.post_pr_review(&job.owner, &job.repo, job.number, FALLBACK_MESSAGE)
                .await?;
        }
    }
    Ok(())
}

/// Drive the tool-calling loop until the model emits a final (text) answer, the
/// turn cap is hit, or the tool-output budget is exhausted (after which tools
/// are withdrawn so the model must conclude).
async fn run_agentic(
    gemini: &GeminiClient,
    system: &str,
    user: &str,
    index: &RepoIndex,
) -> Result<String> {
    let tools = repo_tools();
    let mut contents = vec![Content::user_text(user.to_string())];
    let mut tool_bytes = 0usize;

    for _ in 0..MAX_TURNS {
        let active: Vec<Tool> = if tool_bytes > MAX_TOOL_OUTPUT_BYTES {
            Vec::new() // budget spent → force a conclusion
        } else {
            tools.clone()
        };
        let parts = gemini.generate_turn(system, &contents, &active).await?;
        let calls: Vec<FunctionCall> = parts
            .iter()
            .filter_map(|p| p.function_call.clone())
            .collect();
        if calls.is_empty() {
            return Ok(concat_text(&parts)); // final answer
        }

        // Echo the model's tool-call turn, then answer each call.
        contents.push(Content::model(parts.clone()));
        let mut responses = Vec::with_capacity(calls.len());
        for call in &calls {
            tracing::info!(tool = %call.name, args = %call.args, "review tool call");
            let result = dispatch_tool(index, call);
            tool_bytes = tool_bytes
                .saturating_add(serde_json::to_string(&result).map(|s| s.len()).unwrap_or(0));
            // Echo the call id (Gemini 3.x requires id+name match, one per call).
            responses.push(Part::function_response(
                call.id.as_deref(),
                &call.name,
                result,
            ));
        }
        contents.push(Content::tool_results(responses));
    }

    // Turn cap reached without a final answer: force one, no tools.
    contents.push(Content::user_text(
        "Stop investigating and output your final JSON review now.".to_string(),
    ));
    let parts = gemini.generate_turn(system, &contents, &[]).await?;
    Ok(concat_text(&parts))
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
    let raw = run_agentic(&gemini, &system, &user, index).await?;
    Ok(match validate_pr_review(&raw, &canary) {
        Some(out) => format_pr_review_markdown(&out),
        None => {
            tracing::warn!(
                raw = %raw.chars().take(6000).collect::<String>(),
                "model output failed validation"
            );
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
