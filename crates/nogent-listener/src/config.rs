//! Listener process configuration, loaded from the environment.
//!
//! The listener holds the real secrets (App private key, webhook secret, Gemini
//! key) in `Zeroizing` memory and never logs them.

use std::env;
use std::path::PathBuf;
use std::time::Duration;

use nogent_core::error::{NogentError, Result};
use zeroize::Zeroizing;

pub struct ListenerConfig {
    pub app_id: String,
    pub private_key_pem: Zeroizing<String>,
    pub webhook_secret: Zeroizing<String>,
    pub gemini_api_key: Zeroizing<String>,
    pub gemini_model: String,
    /// Reasoning level for thinking-capable models (Gemini 3.x): minimal | low |
    /// medium | high. Empty → omitted (use for 2.5 models).
    pub gemini_thinking_level: Option<String>,
    pub bind_addr: String,
    /// Max webhook body size in bytes.
    pub max_body_bytes: usize,
    /// Owner logins (lower-cased) allowed to use this listener. Empty = no
    /// restriction (every installation may call). Populated from
    /// `NOGENT_ALLOWED_OWNERS` (comma-separated).
    pub allowed_owners: Vec<String>,
    /// Installation IDs allowed to use this listener. Empty = no restriction.
    /// Populated from `NOGENT_ALLOWED_INSTALLATIONS` (comma-separated u64s).
    pub allowed_installations: Vec<u64>,
    /// How untrusted review and triage jobs are executed.
    pub worker_isolation: WorkerIsolation,
    /// Hard cap on simultaneous review/triage workers.
    pub max_concurrent_jobs: usize,
    /// Number of actionable, verified deliveries that can wait in memory for a
    /// worker. A delivery is acknowledged only after it enters this queue.
    pub max_queued_jobs: usize,
    /// Overall deadline for one review/triage job, including the nono process.
    pub worker_timeout: Duration,
    /// Optional operator-owned prompt directory, canonicalized at startup.
    pub prompts_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub enum WorkerIsolation {
    /// Backwards-compatible local mode. Production deployments should use
    /// `Nono` so fork-controlled content crosses a process sandbox boundary.
    InProcess,
    Nono {
        binary: PathBuf,
        profile: PathBuf,
    },
}

impl ListenerConfig {
    /// True if this `(installation, owner)` is permitted by the allowlist.
    /// An empty allowlist on a given field means "no restriction" for that
    /// field, so an unset listener accepts every installation (the historical
    /// behaviour). Set either env var to lock the listener down to specific
    /// owners or installations and stop free-Gemini-for-anyone calls.
    #[must_use]
    pub fn allowlist_permits(&self, installation_id: u64, owner: &str) -> bool {
        if !self.allowed_installations.is_empty()
            && !self.allowed_installations.contains(&installation_id)
        {
            return false;
        }
        if !self.allowed_owners.is_empty() {
            let owner_lc = owner.to_ascii_lowercase();
            if !self.allowed_owners.iter().any(|o| o == &owner_lc) {
                return false;
            }
        }
        true
    }
}

fn req(key: &str) -> Result<String> {
    env::var(key).map_err(|_| NogentError::Config(format!("missing required env var {key}")))
}

fn opt(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_string())
}

/// Load the App private key PEM from `GITHUB_APP_PRIVATE_KEY` (inline) or, if
/// that is unset, from the file at `GITHUB_APP_PRIVATE_KEY_FILE`. The file form
/// is preferred for deployment: a multi-line PEM does not fit cleanly in a
/// systemd `EnvironmentFile`, so the deploy writes it to a 0600 file instead.
fn load_private_key() -> Result<Zeroizing<String>> {
    if let Ok(inline) = env::var("GITHUB_APP_PRIVATE_KEY") {
        return Ok(Zeroizing::new(inline));
    }
    let path = env::var("GITHUB_APP_PRIVATE_KEY_FILE").map_err(|_| {
        NogentError::Config(
            "set GITHUB_APP_PRIVATE_KEY (inline PEM) or GITHUB_APP_PRIVATE_KEY_FILE (path)".into(),
        )
    })?;
    let pem = std::fs::read_to_string(&path).map_err(|e| {
        NogentError::Config(format!("reading GITHUB_APP_PRIVATE_KEY_FILE {path}: {e}"))
    })?;
    Ok(Zeroizing::new(pem))
}

impl ListenerConfig {
    /// Load from environment, failing if any required secret is absent
    /// (fail-secure: we never start without the webhook secret + app key).
    pub fn from_env() -> Result<Self> {
        let private_key_pem = load_private_key()?;
        let webhook_secret = Zeroizing::new(req("GITHUB_WEBHOOK_SECRET")?);
        let gemini_api_key = Zeroizing::new(req("GEMINI_API_KEY")?);

        let max_body_bytes = opt("NOGENT_MAX_BODY_BYTES", "2097152")
            .parse::<usize>()
            .map_err(|e| NogentError::Config(format!("NOGENT_MAX_BODY_BYTES invalid: {e}")))?;

        let allowed_owners = parse_csv_lower(&opt("NOGENT_ALLOWED_OWNERS", ""));
        let allowed_installations = parse_csv_u64(&opt("NOGENT_ALLOWED_INSTALLATIONS", ""))
            .map_err(|e| NogentError::Config(format!("NOGENT_ALLOWED_INSTALLATIONS: {e}")))?;
        let worker_isolation = parse_worker_isolation(
            &opt("NOGENT_WORKER_MODE", "in-process"),
            &opt("NOGENT_NONO_BIN", "/usr/bin/nono"),
            &opt(
                "NOGENT_NONO_PROFILE",
                "/usr/share/nogent/nogent-worker.nono.json",
            ),
        )?;
        let max_concurrent_jobs = opt("NOGENT_MAX_CONCURRENT_JOBS", "2")
            .parse::<usize>()
            .map_err(|e| NogentError::Config(format!("NOGENT_MAX_CONCURRENT_JOBS invalid: {e}")))?;
        if !(1..=32).contains(&max_concurrent_jobs) {
            return Err(NogentError::Config(
                "NOGENT_MAX_CONCURRENT_JOBS must be between 1 and 32".into(),
            ));
        }
        let max_queued_jobs = opt("NOGENT_MAX_QUEUED_JOBS", "100")
            .parse::<usize>()
            .map_err(|e| NogentError::Config(format!("NOGENT_MAX_QUEUED_JOBS invalid: {e}")))?;
        if !(1..=10_000).contains(&max_queued_jobs) {
            return Err(NogentError::Config(
                "NOGENT_MAX_QUEUED_JOBS must be between 1 and 10000".into(),
            ));
        }
        let worker_timeout_secs = opt("NOGENT_WORKER_TIMEOUT_SECS", "900")
            .parse::<u64>()
            .map_err(|e| NogentError::Config(format!("NOGENT_WORKER_TIMEOUT_SECS invalid: {e}")))?;
        if !(30..=3_600).contains(&worker_timeout_secs) {
            return Err(NogentError::Config(
                "NOGENT_WORKER_TIMEOUT_SECS must be between 30 and 3600".into(),
            ));
        }
        let prompts_dir = load_prompts_dir()?;

        Ok(ListenerConfig {
            app_id: req("GITHUB_APP_ID")?,
            private_key_pem,
            webhook_secret,
            gemini_api_key,
            gemini_model: opt("GEMINI_MODEL", "gemini-3.5-flash"),
            gemini_thinking_level: match opt("GEMINI_THINKING_LEVEL", "high").trim() {
                "" => None,
                level => Some(level.to_string()),
            },
            bind_addr: opt("NOGENT_BIND_ADDR", "127.0.0.1:8080"),
            max_body_bytes,
            allowed_owners,
            allowed_installations,
            worker_isolation,
            max_concurrent_jobs,
            max_queued_jobs,
            worker_timeout: Duration::from_secs(worker_timeout_secs),
            prompts_dir,
        })
    }
}

fn parse_worker_isolation(mode: &str, binary: &str, profile: &str) -> Result<WorkerIsolation> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "in-process" => Ok(WorkerIsolation::InProcess),
        "nono" if binary.trim().is_empty() => Err(NogentError::Config(
            "NOGENT_NONO_BIN cannot be empty in nono worker mode".into(),
        )),
        "nono" if profile.trim().is_empty() => Err(NogentError::Config(
            "NOGENT_NONO_PROFILE cannot be empty in nono worker mode".into(),
        )),
        "nono" => Ok(WorkerIsolation::Nono {
            binary: PathBuf::from(binary.trim()),
            profile: PathBuf::from(profile.trim()),
        }),
        other => Err(NogentError::Config(format!(
            "NOGENT_WORKER_MODE must be 'in-process' or 'nono', got '{other}'"
        ))),
    }
}

fn load_prompts_dir() -> Result<Option<PathBuf>> {
    let Ok(raw) = env::var("NOGENT_PROMPTS_DIR") else {
        return Ok(None);
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let dir = std::fs::canonicalize(raw.trim())
        .map_err(|e| NogentError::Config(format!("canonicalizing NOGENT_PROMPTS_DIR: {e}")))?;
    if !dir.is_dir() {
        return Err(NogentError::Config(
            "NOGENT_PROMPTS_DIR must name a directory".into(),
        ));
    }
    let protected_files = [
        PathBuf::from("/etc/nogent/app-private-key.pem"),
        PathBuf::from("/etc/nogent/nogent.env"),
        PathBuf::from("/private/etc/nogent/app-private-key.pem"),
        PathBuf::from("/private/etc/nogent/nogent.env"),
    ];
    if protected_files
        .iter()
        .any(|secret| secret.starts_with(&dir))
    {
        return Err(NogentError::Config(
            "NOGENT_PROMPTS_DIR must not contain listener secret files".into(),
        ));
    }
    for file in [
        nogent_core::prompts::PR_REVIEW_SYSTEM_FILE,
        nogent_core::prompts::ISSUE_TRIAGE_SYSTEM_FILE,
    ] {
        std::fs::read_to_string(dir.join(file)).map_err(|e| {
            NogentError::Config(format!(
                "reading NOGENT_PROMPTS_DIR/{file} during startup validation: {e}"
            ))
        })?;
    }
    Ok(Some(dir))
}

fn parse_csv_lower(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_csv_u64(raw: &str) -> std::result::Result<Vec<u64>, String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            s.parse::<u64>()
                .map_err(|e| format!("'{s}' is not a u64: {e}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(owners: &[&str], installs: &[u64]) -> ListenerConfig {
        ListenerConfig {
            app_id: "1".into(),
            private_key_pem: Zeroizing::new(String::new()),
            webhook_secret: Zeroizing::new(String::new()),
            gemini_api_key: Zeroizing::new(String::new()),
            gemini_model: String::new(),
            gemini_thinking_level: None,
            bind_addr: String::new(),
            max_body_bytes: 0,
            allowed_owners: owners.iter().map(|s| s.to_ascii_lowercase()).collect(),
            allowed_installations: installs.to_vec(),
            worker_isolation: WorkerIsolation::InProcess,
            max_concurrent_jobs: 2,
            max_queued_jobs: 100,
            worker_timeout: Duration::from_secs(900),
            prompts_dir: None,
        }
    }

    #[test]
    fn empty_allowlist_permits_everything() {
        let c = cfg_with(&[], &[]);
        assert!(c.allowlist_permits(42, "anyone"));
    }

    #[test]
    fn owner_allowlist_is_case_insensitive() {
        let c = cfg_with(&["NoLabs-AI"], &[]);
        assert!(c.allowlist_permits(1, "nolabs-ai"));
        assert!(c.allowlist_permits(1, "NOLABS-AI"));
        assert!(!c.allowlist_permits(1, "someone-else"));
    }

    #[test]
    fn installation_allowlist_blocks_others() {
        let c = cfg_with(&[], &[100, 200]);
        assert!(c.allowlist_permits(100, "x"));
        assert!(!c.allowlist_permits(99, "x"));
    }

    #[test]
    fn both_lists_must_pass_when_set() {
        let c = cfg_with(&["luke"], &[7]);
        assert!(c.allowlist_permits(7, "luke"));
        assert!(!c.allowlist_permits(7, "other")); // owner fails
        assert!(!c.allowlist_permits(8, "luke")); // installation fails
    }

    #[test]
    fn parse_csv_handles_whitespace_and_empties() {
        assert_eq!(parse_csv_lower(" a , B ,  ,c "), vec!["a", "b", "c"]);
        assert_eq!(parse_csv_u64(" 1 , 2,3 ").unwrap(), vec![1, 2, 3]);
        assert!(parse_csv_u64("1,foo").is_err());
    }

    #[test]
    fn worker_isolation_is_explicit_and_fail_secure() {
        assert!(matches!(
            parse_worker_isolation("in-process", "", ""),
            Ok(WorkerIsolation::InProcess)
        ));
        assert!(matches!(
            parse_worker_isolation("nono", "/usr/bin/nono", "/etc/nogent/worker.json"),
            Ok(WorkerIsolation::Nono { .. })
        ));
        assert!(parse_worker_isolation("nono", "", "profile").is_err());
        assert!(parse_worker_isolation("unknown", "nono", "profile").is_err());
    }
}
