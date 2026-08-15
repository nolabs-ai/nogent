//! Boundary between the credential-bearing webhook listener and untrusted
//! review/triage processing.
//!
//! Production mode launches this same binary under `nono run`. The nono
//! supervisor consumes the real per-repository GitHub token and Gemini key,
//! while the sandboxed child receives only a shared random phantom token plus
//! loopback proxy URLs. Fork-controlled repository data is parsed and exposed
//! to model tools only on the child side of that boundary.

use std::ffi::OsString;
use std::future::Future;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use nogent_core::error::{NogentError, Result};
use nogent_core::events::{EventJob, JobKind};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use zeroize::Zeroizing;

use crate::api_base::WorkerApiBases;
use crate::config::{ListenerConfig, WorkerIsolation};
use crate::{review, triage};

const MAX_JOB_BYTES: u64 = 4 * 1024 * 1024;
const PHANTOM_LEN: usize = 64;
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(30);

struct WorkerContext {
    github_token: Zeroizing<String>,
    gemini_key: Zeroizing<String>,
    api_bases: WorkerApiBases,
}

/// Validate the configured sandbox before binding the webhook listener. The
/// preflight runs this binary as a real nono child, so missing binaries,
/// malformed/missing profiles, unsupported OS sandboxing, executable-path
/// grants, credential injection, and proxy base URLs all fail startup.
pub async fn preflight(cfg: &ListenerConfig) -> Result<()> {
    if matches!(cfg.worker_isolation, WorkerIsolation::InProcess) {
        return Ok(());
    }

    with_deadline(
        PREFLIGHT_TIMEOUT,
        "nono worker preflight",
        run_sandboxed_child(cfg, "nogent-preflight", "--worker-preflight", None),
    )
    .await
}

pub async fn run(cfg: &ListenerConfig, token: &str, job: &EventJob) -> Result<()> {
    with_deadline(cfg.worker_timeout, "review/triage worker", async {
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
                    None,
                )
                .await
            }
            WorkerIsolation::Nono { .. } => run_sandboxed(cfg, token, job).await,
        }
    })
    .await
}

async fn with_deadline<T>(
    deadline: Duration,
    label: &str,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(deadline, future).await.map_err(|_| {
        NogentError::Io(format!(
            "{label} exceeded its {} second deadline",
            deadline.as_secs()
        ))
    })?
}

async fn run_sandboxed(cfg: &ListenerConfig, token: &str, job: &EventJob) -> Result<()> {
    let payload = serde_json::to_vec(job)?;
    if payload.len() as u64 > MAX_JOB_BYTES {
        return Err(NogentError::Payload(format!(
            "worker job exceeds {MAX_JOB_BYTES} bytes"
        )));
    }
    run_sandboxed_child(cfg, token, "--worker", Some(&payload)).await
}

async fn run_sandboxed_child(
    cfg: &ListenerConfig,
    github_token: &str,
    child_arg: &str,
    payload: Option<&[u8]>,
) -> Result<()> {
    let (nono_binary, profile) = match &cfg.worker_isolation {
        WorkerIsolation::Nono { binary, profile } => (binary.as_path(), profile.as_path()),
        WorkerIsolation::InProcess => {
            return Err(NogentError::Config(
                "attempted to spawn a nono child in in-process mode".into(),
            ));
        }
    };
    let current_exe = std::env::current_exe()
        .map_err(|e| NogentError::Io(format!("resolving worker executable: {e}")))?;
    let mut command = sandbox_command(
        cfg,
        github_token,
        nono_binary,
        profile,
        &current_exe,
        child_arg,
    );
    command.stdin(if payload.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });

    let mut child = command
        .spawn()
        .map_err(|e| NogentError::Io(format!("starting nono worker: {e}")))?;
    if let Some(payload) = payload {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| NogentError::Io("nono worker stdin was not piped".into()))?;
        stdin
            .write_all(payload)
            .await
            .map_err(|e| NogentError::Io(format!("writing nono worker job: {e}")))?;
        drop(stdin);
    }

    let status = child
        .wait()
        .await
        .map_err(|e| NogentError::Io(format!("waiting for nono worker: {e}")))?;
    if !status.success() {
        return Err(NogentError::Io(format!(
            "nono worker exited unsuccessfully ({status})"
        )));
    }
    Ok(())
}

fn sandbox_command(
    cfg: &ListenerConfig,
    github_token: &str,
    nono_binary: &Path,
    profile: &Path,
    current_exe: &Path,
    child_arg: &str,
) -> Command {
    let mut command = Command::new(nono_binary);
    command
        .arg("-s")
        .arg("run")
        .arg("--profile")
        .arg(profile)
        // Profiles cannot predict source-build or /opt installation paths.
        // Grant exactly the executable resolved for this listener invocation.
        .arg("--read-file")
        .arg(current_exe);
    if let Some(prompts_dir) = &cfg.prompts_dir {
        command.arg("--read").arg(prompts_dir);
    }
    command
        .arg("--")
        .arg(current_exe)
        .arg(child_arg)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        // Never leak the listener's App private key, webhook secret, or any
        // unrelated service credentials into the nono supervisor environment.
        .env_clear()
        .envs(supervisor_environment(
            github_token,
            &cfg.gemini_api_key,
            cfg.gemini_thinking_level.as_deref(),
            cfg.prompts_dir.as_deref(),
        ));
    command
}

/// Entry point used only by the nono child process.
pub async fn run_from_stdin() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let context = load_worker_context()?;
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
    let thinking = std::env::var("GEMINI_THINKING_LEVEL")
        .ok()
        .filter(|value| !value.trim().is_empty());

    run_job(
        &context.gemini_key,
        thinking.as_deref(),
        &context.github_token,
        &job,
        Some(&context.api_bases),
    )
    .await?;
    Ok(())
}

/// Minimal child entrypoint used by startup preflight.
pub async fn preflight_child() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let context = load_worker_context()?;
    if let Ok(dir) = std::env::var("NOGENT_PROMPTS_DIR") {
        for file in [
            nogent_core::prompts::PR_REVIEW_SYSTEM_FILE,
            nogent_core::prompts::ISSUE_TRIAGE_SYSTEM_FILE,
        ] {
            std::fs::read_to_string(Path::new(&dir).join(file)).map_err(|error| {
                NogentError::Config(format!(
                    "sandboxed worker cannot read NOGENT_PROMPTS_DIR/{file}: {error}"
                ))
            })?;
        }
    }
    let github = crate::github_client::GithubClient::new_for_worker(
        &context.github_token,
        &context.api_bases.github,
    )?;
    github.verify_worker_proxy_auth().await?;
    let gemini = crate::gemini_client::GeminiClient::new_for_worker(
        &context.gemini_key,
        "preflight",
        None,
        &context.api_bases.gemini,
    )?;
    gemini.verify_worker_proxy_auth().await?;
    Ok(())
}

fn load_worker_context() -> Result<WorkerContext> {
    for forbidden in [
        "GITHUB_APP_PRIVATE_KEY",
        "GITHUB_APP_PRIVATE_KEY_FILE",
        "GITHUB_WEBHOOK_SECRET",
    ] {
        if std::env::var_os(forbidden).is_some() {
            return Err(NogentError::Auth(format!(
                "sandboxed worker inherited forbidden listener secret {forbidden}"
            )));
        }
    }

    let github_token = Zeroizing::new(required_env("GITHUB_TOKEN")?);
    let gemini_key = Zeroizing::new(required_env("GEMINI_API_KEY")?);
    // `--worker` is exclusively a sandbox child entrypoint. Verification is
    // therefore unconditional; a missing profile variable cannot disable it.
    if !are_phantom_credentials(&github_token, &gemini_key) {
        return Err(NogentError::Auth(
            "sandboxed worker did not receive matching nono phantom credentials".into(),
        ));
    }
    let api_bases = WorkerApiBases::from_env()?;
    Ok(WorkerContext {
        github_token,
        gemini_key,
        api_bases,
    })
}

async fn run_job(
    gemini_api_key: &str,
    thinking_level: Option<&str>,
    github_token: &str,
    job: &EventJob,
    api_bases: Option<&WorkerApiBases>,
) -> Result<()> {
    match job.kind {
        JobKind::PrReview => {
            review::run(gemini_api_key, thinking_level, github_token, job, api_bases).await
        }
        JobKind::IssueTriage => {
            triage::run(gemini_api_key, thinking_level, github_token, job, api_bases).await
        }
    }
}

fn required_env(name: &str) -> Result<String> {
    std::env::var(name)
        .map_err(|_| NogentError::Config(format!("sandboxed worker is missing {name}")))
}

fn are_phantom_credentials(github: &str, gemini: &str) -> bool {
    github == gemini
        && github.len() == PHANTOM_LEN
        && github.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Minimal environment passed to the trusted nono supervisor. Credential
/// routes consume the two real secrets and replace them with the same random
/// phantom token before the child starts.
fn supervisor_environment(
    github_token: &str,
    gemini_api_key: &str,
    thinking_level: Option<&str>,
    prompts_dir: Option<&Path>,
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
    env.push((OsString::from("GITHUB_TOKEN"), OsString::from(github_token)));
    env.push((
        OsString::from("GEMINI_API_KEY"),
        OsString::from(gemini_api_key),
    ));
    if let Some(level) = thinking_level {
        env.push((
            OsString::from("GEMINI_THINKING_LEVEL"),
            OsString::from(level),
        ));
    }
    if let Some(dir) = prompts_dir {
        env.push((
            OsString::from("NOGENT_PROMPTS_DIR"),
            dir.as_os_str().to_os_string(),
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
    fn supervisor_environment_contains_only_worker_secrets() {
        let env = supervisor_environment("github-real", "gemini-real", Some("high"), None);
        let names = env
            .iter()
            .filter_map(|(name, _)| name.to_str())
            .collect::<Vec<_>>();
        assert!(names.contains(&"GITHUB_TOKEN"));
        assert!(names.contains(&"GEMINI_API_KEY"));
        assert!(!names.contains(&"NOGENT_REQUIRE_PHANTOM_CREDENTIALS"));
        assert!(!names.contains(&"GITHUB_APP_PRIVATE_KEY"));
        assert!(!names.contains(&"GITHUB_WEBHOOK_SECRET"));
    }

    #[tokio::test]
    async fn worker_deadline_fails_instead_of_hanging_forever() {
        let result: Result<()> = with_deadline(Duration::from_millis(1), "test worker", async {
            std::future::pending::<Result<()>>().await
        })
        .await;
        assert!(result.is_err());
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
        assert_eq!(
            network["custom_credentials"]["nogent_github"]["credential_format"],
            "Bearer {}"
        );
        assert!(profile["filesystem"].get("allow").is_none());
        assert!(profile["filesystem"].get("write").is_none());
        assert!(profile["filesystem"].get("allow_file").is_none());
        assert!(profile["filesystem"].get("read_file").is_none());
        assert_eq!(
            profile["filesystem"]["deny"],
            serde_json::json!([
                "/etc/nogent/app-private-key.pem",
                "/etc/nogent/nogent.env",
                "/private/etc/nogent/app-private-key.pem",
                "/private/etc/nogent/nogent.env"
            ])
        );

        let allowed_env = profile["environment"]["allow_vars"]
            .as_array()
            .expect("environment allowlist");
        assert!(!allowed_env.iter().any(|name| name == "GITHUB_TOKEN"));
        assert!(!allowed_env.iter().any(|name| name == "GEMINI_API_KEY"));
        assert!(allowed_env.iter().any(|name| name == "NOGENT_PROMPTS_DIR"));

        let excluded = profile["groups"]["exclude"]
            .as_array()
            .expect("group exclusions");
        assert!(excluded.iter().any(|group| group == "system_write_linux"));
        assert!(excluded.iter().any(|group| group == "system_read_macos"));
        assert_eq!(profile["security"]["capability_elevation"], false);
    }
}
