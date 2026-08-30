//! Boundary between the credential-bearing webhook listener and untrusted
//! review/triage processing.
//!
//! Production mode launches this same binary under `nono run`. The nono
//! supervisor consumes the real per-repository GitHub token and Gemini key,
//! while the sandboxed child receives only a shared random phantom token plus
//! loopback proxy URLs. Fork-controlled repository data is parsed and exposed
//! to model tools only on the child side of that boundary.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::process::Stdio;

use nogent_core::error::{NogentError, Result};
use nogent_core::events::{EventJob, JobKind};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use zeroize::Zeroizing;

use crate::config::{ListenerConfig, WorkerIsolation};
use crate::{question, review, triage};

const MAX_JOB_BYTES: u64 = 4 * 1024 * 1024;
const PHANTOM_LEN: usize = 64;

/// Run a GitHub-derived job (PR review / issue triage). These post their own
/// output to GitHub, so nothing is returned.
pub async fn run(cfg: &ListenerConfig, token: &str, job: &EventJob) -> Result<()> {
    match &cfg.worker_isolation {
        WorkerIsolation::InProcess => {
            tracing::warn!(
                job = ?job.kind,
                "processing untrusted job in-process; enable NOGENT_WORKER_MODE=nono in production"
            );
            run_job(
                &cfg.gemini_api_key,
                cfg.gemini_thinking_level.as_deref(),
                token,
                job,
            )
            .await
        }
        WorkerIsolation::Nono {
            binary, profile, ..
        } => {
            run_sandboxed(cfg, Some(token), job, binary, profile, false).await?;
            Ok(())
        }
    }
}

/// Run a source-agnostic [`JobKind::Question`] job and RETURN the answer JSON.
/// No GitHub token is used; egress is restricted to Gemini via a dedicated nono
/// profile in sandboxed mode.
pub async fn run_question(cfg: &ListenerConfig, job: &EventJob) -> Result<String> {
    match &cfg.worker_isolation {
        WorkerIsolation::InProcess => {
            tracing::warn!(
                "answering question in-process; enable NOGENT_WORKER_MODE=nono in production"
            );
            question::run(
                &cfg.gemini_api_key,
                cfg.gemini_thinking_level.as_deref(),
                job,
            )
            .await
        }
        WorkerIsolation::Nono {
            binary,
            question_profile,
            ..
        } => run_sandboxed(cfg, None, job, binary, question_profile, true)
            .await?
            .ok_or_else(|| NogentError::Io("question worker produced no answer".into())),
    }
}

/// Spawn the worker under `nono run`. `github_token` is `None` for Question jobs
/// (Gemini-only egress). When `capture_stdout` is set, the child's stdout is
/// piped and returned as the answer body; otherwise it is inherited.
async fn run_sandboxed(
    cfg: &ListenerConfig,
    github_token: Option<&str>,
    job: &EventJob,
    nono_binary: &str,
    profile: &str,
    capture_stdout: bool,
) -> Result<Option<String>> {
    let current_exe = std::env::current_exe()
        .map_err(|e| NogentError::Io(format!("resolving worker executable: {e}")))?;
    let payload = serde_json::to_vec(job)?;
    if payload.len() as u64 > MAX_JOB_BYTES {
        return Err(NogentError::Payload(format!(
            "worker job exceeds {MAX_JOB_BYTES} bytes"
        )));
    }

    let thinking = cfg.gemini_thinking_level.as_deref();
    let env = match github_token {
        Some(token) => supervisor_environment(token, &cfg.gemini_api_key, thinking),
        None => question_supervisor_environment(&cfg.gemini_api_key, thinking),
    };

    let mut command = Command::new(nono_binary);
    command
        .arg("-s")
        .arg("run")
        .arg("--profile")
        .arg(profile)
        .arg("--")
        .arg(current_exe)
        .arg("--worker")
        .stdin(Stdio::piped())
        .stdout(if capture_stdout {
            Stdio::piped()
        } else {
            Stdio::inherit()
        })
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        // Never leak the listener's App private key, webhook secret, or any
        // unrelated service credentials into the nono supervisor environment.
        .env_clear()
        .envs(env);

    let mut child = command
        .spawn()
        .map_err(|e| NogentError::Io(format!("starting nono worker: {e}")))?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| NogentError::Io("nono worker stdin was not piped".into()))?;
    stdin
        .write_all(&payload)
        .await
        .map_err(|e| NogentError::Io(format!("writing nono worker job: {e}")))?;
    drop(stdin);

    // Read the answer (bounded) before waiting, so a child writing more than a
    // pipe buffer cannot deadlock. Only Question jobs pipe stdout.
    let captured = if capture_stdout {
        let out = child
            .stdout
            .take()
            .ok_or_else(|| NogentError::Io("nono worker stdout was not piped".into()))?;
        let mut buf = Vec::new();
        out.take(MAX_JOB_BYTES)
            .read_to_end(&mut buf)
            .await
            .map_err(|e| NogentError::Io(format!("reading nono worker answer: {e}")))?;
        let answer = String::from_utf8_lossy(&buf).trim().to_string();
        // A zero-exit worker that wrote nothing is a bug, not a valid answer;
        // surface it as an error rather than returning an empty body.
        (!answer.is_empty()).then_some(answer)
    } else {
        None
    };

    let status = child
        .wait()
        .await
        .map_err(|e| NogentError::Io(format!("waiting for nono worker: {e}")))?;
    if !status.success() {
        return Err(NogentError::Io(format!(
            "nono worker exited unsuccessfully ({status})"
        )));
    }
    Ok(captured)
}

/// Entry point used only by the nono child process.
pub async fn run_from_stdin() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_JOB_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_JOB_BYTES {
        return Err(Box::new(NogentError::Payload(format!(
            "worker job exceeds {MAX_JOB_BYTES} bytes"
        ))));
    }
    let job: EventJob = serde_json::from_slice(&bytes)?;

    let gemini_key = Zeroizing::new(required_env("GEMINI_API_KEY")?);
    let require_phantom = std::env::var("NOGENT_REQUIRE_PHANTOM_CREDENTIALS").as_deref() == Ok("1");
    let thinking = std::env::var("GEMINI_THINKING_LEVEL")
        .ok()
        .filter(|value| !value.trim().is_empty());

    match job.kind {
        // Question jobs use Gemini only — no GitHub token is present or needed.
        // The answer is the sole thing written to stdout (logs go to stderr),
        // so the parent listener can capture it as the HTTP response body.
        JobKind::Question => {
            if require_phantom && !is_phantom_token(&gemini_key) {
                return Err(Box::new(NogentError::Auth(
                    "sandboxed worker did not receive a nono phantom Gemini credential".into(),
                )));
            }
            let answer = question::run(&gemini_key, thinking.as_deref(), &job).await?;
            let mut stdout = std::io::stdout();
            stdout.write_all(answer.as_bytes())?;
            stdout.flush()?;
        }
        JobKind::PrReview | JobKind::IssueTriage => {
            let github_token = Zeroizing::new(required_env("GITHUB_TOKEN")?);
            if require_phantom && !are_phantom_credentials(&github_token, &gemini_key) {
                return Err(Box::new(NogentError::Auth(
                    "sandboxed worker did not receive matching nono phantom credentials".into(),
                )));
            }
            run_job(&gemini_key, thinking.as_deref(), &github_token, &job).await?;
        }
    }
    Ok(())
}

async fn run_job(
    gemini_api_key: &str,
    thinking_level: Option<&str>,
    github_token: &str,
    job: &EventJob,
) -> Result<()> {
    match job.kind {
        JobKind::PrReview => review::run(gemini_api_key, thinking_level, github_token, job).await,
        JobKind::IssueTriage => {
            triage::run(gemini_api_key, thinking_level, github_token, job).await
        }
        // Question jobs are dispatched via `run_question` / the stdout path
        // above and never reach this GitHub-posting helper.
        JobKind::Question => Err(NogentError::Io(
            "question jobs must be run via run_question, not run_job".into(),
        )),
    }
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .map_err(|_| NogentError::Config(format!("sandboxed worker is missing {name}")))
}

/// A nono phantom token is a 256-bit random value, hex-encoded (64 chars).
fn is_phantom_token(token: &str) -> bool {
    token.len() == PHANTOM_LEN && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn are_phantom_credentials(github: &str, gemini: &str) -> bool {
    github == gemini && is_phantom_token(github)
}

/// Minimal environment passed to the trusted nono supervisor. Credential
/// routes consume the two real secrets and replace them with the same random
/// phantom token before the child starts.
fn supervisor_environment(
    github_token: &str,
    gemini_api_key: &str,
    thinking_level: Option<&str>,
) -> Vec<(OsString, OsString)> {
    let mut env = base_supervisor_environment(gemini_api_key, thinking_level);
    env.push((OsString::from("GITHUB_TOKEN"), OsString::from(github_token)));
    env
}

/// Environment for a Question worker: Gemini only. No `GITHUB_TOKEN` is passed,
/// so the sandboxed child cannot reach GitHub even if it tried.
fn question_supervisor_environment(
    gemini_api_key: &str,
    thinking_level: Option<&str>,
) -> Vec<(OsString, OsString)> {
    base_supervisor_environment(gemini_api_key, thinking_level)
}

/// Shared base env for any sandboxed worker: safe passthrough vars, the Gemini
/// key, the phantom-credential requirement, and the optional thinking level.
/// Never carries the App private key or webhook secret.
fn base_supervisor_environment(
    gemini_api_key: &str,
    thinking_level: Option<&str>,
) -> Vec<(OsString, OsString)> {
    let mut env = Vec::new();
    for name in [
        "HOME",
        "PATH",
        "TMPDIR",
        "XDG_CONFIG_HOME",
        "XDG_STATE_HOME",
        "XDG_CACHE_HOME",
        "RUST_LOG",
        "RUST_BACKTRACE",
    ] {
        if let Some(value) = std::env::var_os(name) {
            env.push((OsString::from(name), value));
        }
    }
    env.push((
        OsString::from("GEMINI_API_KEY"),
        OsString::from(gemini_api_key),
    ));
    env.push((
        OsString::from("NOGENT_REQUIRE_PHANTOM_CREDENTIALS"),
        OsString::from("1"),
    ));
    if let Some(level) = thinking_level {
        env.push((
            OsString::from("GEMINI_THINKING_LEVEL"),
            OsString::from(level),
        ));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phantom_credentials_must_be_matching_256_bit_hex_tokens() {
        let phantom = "a1".repeat(32);
        assert!(are_phantom_credentials(&phantom, &phantom));
        assert!(!are_phantom_credentials("ghs_real", "ghs_real"));
        assert!(!are_phantom_credentials(&phantom, &"b2".repeat(32)));
    }

    #[test]
    fn question_supervisor_environment_omits_github_token() {
        let env = question_supervisor_environment("gemini-real", Some("high"));
        let names = env
            .iter()
            .filter_map(|(name, _)| name.to_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"GEMINI_API_KEY"));
        assert!(names.contains(&"NOGENT_REQUIRE_PHANTOM_CREDENTIALS"));
        // The whole point: a Question worker can never reach GitHub.
        assert!(!names.contains(&"GITHUB_TOKEN"));
        assert!(!names.contains(&"GITHUB_APP_PRIVATE_KEY"));
    }

    #[test]
    fn question_worker_accepts_lone_gemini_phantom() {
        let phantom = "ab".repeat(32);
        assert!(is_phantom_token(&phantom));
        assert!(!is_phantom_token("gemini-real"));
    }

    #[test]
    fn question_profile_is_gemini_only_and_has_no_github_reach() {
        let profile: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/nogent-question.nono.json"))
                .expect("valid JSON profile");
        let network = &profile["network"];
        assert_eq!(network["credentials"], serde_json::json!(["nogent_gemini"]));
        // No GitHub credential and no GitHub domain allowed.
        assert!(network["custom_credentials"].get("nogent_github").is_none());
        let domains = network["allow_domain"].as_array();
        assert!(domains.is_none_or(|d| d.iter().all(|x| x != "codeload.github.com")));
        assert!(profile["filesystem"].get("write").is_none());
        assert_eq!(profile["security"]["capability_elevation"], false);
    }

    #[test]
    fn supervisor_environment_contains_only_worker_secrets() {
        let env = supervisor_environment("github-real", "gemini-real", Some("high"));
        let names = env
            .iter()
            .filter_map(|(name, _)| name.to_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"GITHUB_TOKEN"));
        assert!(names.contains(&"GEMINI_API_KEY"));
        assert!(names.contains(&"NOGENT_REQUIRE_PHANTOM_CREDENTIALS"));
        assert!(!names.contains(&"GITHUB_APP_PRIVATE_KEY"));
        assert!(!names.contains(&"GITHUB_WEBHOOK_SECRET"));
    }

    #[test]
    fn production_profile_has_bounded_credentials_and_no_write_grants() {
        let profile: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/nogent-worker.nono.json"))
                .expect("valid JSON profile");

        let network = &profile["network"];
        assert_eq!(
            network["allow_domain"],
            serde_json::json!(["codeload.github.com"])
        );
        assert_eq!(
            network["credentials"],
            serde_json::json!(["nogent_github", "nogent_gemini"])
        );
        assert!(profile["filesystem"].get("allow").is_none());
        assert!(profile["filesystem"].get("write").is_none());
        assert!(profile["filesystem"].get("allow_file").is_none());
        assert_eq!(
            profile["filesystem"]["deny"],
            serde_json::json!(["/etc/nogent", "/private/etc/nogent"])
        );

        let excluded = profile["groups"]["exclude"]
            .as_array()
            .expect("group exclusions");
        assert!(excluded.iter().any(|group| group == "system_write_linux"));
        assert!(excluded.iter().any(|group| group == "system_read_macos"));
        assert_eq!(profile["security"]["capability_elevation"], false);
    }
}
