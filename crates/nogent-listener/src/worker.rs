//! Boundary between the credential-bearing webhook listener and untrusted
//! review/triage processing.
//!
//! Production mode launches this same binary under `nono run`. The nono
//! supervisor consumes the real per-repository GitHub token and Gemini key,
//! while the sandboxed child receives only a shared random phantom token plus
//! loopback proxy URLs. Fork-controlled repository data is parsed and exposed
//! to model tools only on the child side of that boundary.

use std::ffi::OsString;
use std::io::Read;
use std::process::Stdio;

use nogent_core::error::{NogentError, Result};
use nogent_core::events::{EventJob, JobKind};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use zeroize::Zeroizing;

use crate::config::{ListenerConfig, WorkerIsolation};
use crate::{review, triage};

const MAX_JOB_BYTES: u64 = 4 * 1024 * 1024;
const PHANTOM_LEN: usize = 64;

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
        WorkerIsolation::Nono { binary, profile } => {
            run_sandboxed(cfg, token, job, binary, profile).await
        }
    }
}

async fn run_sandboxed(
    cfg: &ListenerConfig,
    token: &str,
    job: &EventJob,
    nono_binary: &str,
    profile: &str,
) -> Result<()> {
    let current_exe = std::env::current_exe()
        .map_err(|e| NogentError::Io(format!("resolving worker executable: {e}")))?;
    let payload = serde_json::to_vec(job)?;
    if payload.len() as u64 > MAX_JOB_BYTES {
        return Err(NogentError::Payload(format!(
            "worker job exceeds {MAX_JOB_BYTES} bytes"
        )));
    }

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
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        // Never leak the listener's App private key, webhook secret, or any
        // unrelated service credentials into the nono supervisor environment.
        .env_clear()
        .envs(supervisor_environment(
            token,
            &cfg.gemini_api_key,
            cfg.gemini_thinking_level.as_deref(),
        ));

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

    let github_token = Zeroizing::new(required_env("GITHUB_TOKEN")?);
    let gemini_key = Zeroizing::new(required_env("GEMINI_API_KEY")?);
    if std::env::var("NOGENT_REQUIRE_PHANTOM_CREDENTIALS").as_deref() == Ok("1")
        && !are_phantom_credentials(&github_token, &gemini_key)
    {
        return Err(Box::new(NogentError::Auth(
            "sandboxed worker did not receive matching nono phantom credentials".into(),
        )));
    }
    let thinking = std::env::var("GEMINI_THINKING_LEVEL")
        .ok()
        .filter(|value| !value.trim().is_empty());

    run_job(&gemini_key, thinking.as_deref(), &github_token, &job).await?;
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
