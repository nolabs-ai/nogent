//! Listener process configuration, loaded from the environment.
//!
//! The listener holds the real secrets (App private key, webhook secret, Gemini
//! key) in `Zeroizing` memory and never logs them.

use std::env;

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
    /// Hard cap on simultaneous jobs. The webhook returns 429 when saturated so
    /// GitHub retries instead of letting valid deliveries create unbounded work.
    pub max_concurrent_jobs: usize,
    /// Shared secret for the non-GitHub `POST /api/triage` ingress. `None` (unset
    /// `NOGENT_TRIAGE_SECRET`) disables the endpoint entirely (fail-secure).
    pub triage_ingress_secret: Option<Zeroizing<String>>,
    /// Max serialized bytes of caller-supplied `context` accepted by
    /// `/api/triage`. Bounds untrusted grounding before it reaches the model.
    pub triage_max_context_bytes: usize,
}

#[derive(Debug, Clone)]
pub enum WorkerIsolation {
    /// Backwards-compatible local mode. Production deployments should use
    /// `Nono` so fork-controlled content crosses a process sandbox boundary.
    InProcess,
    Nono {
        binary: String,
        profile: String,
        /// Gemini-only profile for Question jobs (no GitHub egress).
        question_profile: String,
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
            &opt(
                "NOGENT_NONO_QUESTION_PROFILE",
                "/usr/share/nogent/nogent-question.nono.json",
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

        // Fail-secure: only enable /api/triage when a non-empty secret is set.
        let triage_ingress_secret = match env::var("NOGENT_TRIAGE_SECRET") {
            Ok(secret) if !secret.trim().is_empty() => Some(Zeroizing::new(secret)),
            _ => None,
        };
        let triage_max_context_bytes = opt("NOGENT_TRIAGE_MAX_CONTEXT_BYTES", "262144")
            .parse::<usize>()
            .map_err(|e| {
                NogentError::Config(format!("NOGENT_TRIAGE_MAX_CONTEXT_BYTES invalid: {e}"))
            })?;

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
            triage_ingress_secret,
            triage_max_context_bytes,
        })
    }
}

fn parse_worker_isolation(
    mode: &str,
    binary: &str,
    profile: &str,
    question_profile: &str,
) -> Result<WorkerIsolation> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "in-process" => Ok(WorkerIsolation::InProcess),
        "nono" if binary.trim().is_empty() => Err(NogentError::Config(
            "NOGENT_NONO_BIN cannot be empty in nono worker mode".into(),
        )),
        "nono" if profile.trim().is_empty() => Err(NogentError::Config(
            "NOGENT_NONO_PROFILE cannot be empty in nono worker mode".into(),
        )),
        "nono" if question_profile.trim().is_empty() => Err(NogentError::Config(
            "NOGENT_NONO_QUESTION_PROFILE cannot be empty in nono worker mode".into(),
        )),
        "nono" => Ok(WorkerIsolation::Nono {
            binary: binary.to_string(),
            profile: profile.to_string(),
            question_profile: question_profile.to_string(),
        }),
        other => Err(NogentError::Config(format!(
            "NOGENT_WORKER_MODE must be 'in-process' or 'nono', got '{other}'"
        ))),
    }
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
            triage_ingress_secret: None,
            triage_max_context_bytes: 262_144,
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
            parse_worker_isolation("in-process", "", "", ""),
            Ok(WorkerIsolation::InProcess)
        ));
        assert!(matches!(
            parse_worker_isolation(
                "nono",
                "/usr/bin/nono",
                "/etc/nogent/worker.json",
                "/etc/nogent/question.json"
            ),
            Ok(WorkerIsolation::Nono { .. })
        ));
        assert!(parse_worker_isolation("nono", "", "profile", "question").is_err());
        assert!(parse_worker_isolation("nono", "nono", "profile", "").is_err());
        assert!(parse_worker_isolation("unknown", "nono", "profile", "question").is_err());
    }
}
