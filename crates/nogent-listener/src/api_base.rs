//! Validated API base URLs for direct production traffic and nono's loopback
//! credential proxy. Plain HTTP is accepted only for a loopback host.

use nogent_core::error::{NogentError, Result};

#[derive(Debug, Clone)]
pub struct ApiBase {
    url: String,
    loopback_http: bool,
}

impl ApiBase {
    pub fn from_env(name: &str, default: &str) -> Result<Self> {
        let raw = std::env::var(name).unwrap_or_else(|_| default.to_string());
        Self::parse(name, &raw)
    }

    fn parse(name: &str, raw: &str) -> Result<Self> {
        let parsed = reqwest::Url::parse(raw)
            .map_err(|e| NogentError::Config(format!("{name} is not a valid URL: {e}")))?;
        if !parsed.username().is_empty() || parsed.password().is_some() {
            return Err(NogentError::Config(format!(
                "{name} must not contain credentials"
            )));
        }
        if parsed.query().is_some() || parsed.fragment().is_some() {
            return Err(NogentError::Config(format!(
                "{name} must not contain a query or fragment"
            )));
        }

        let loopback = parsed.host_str().is_some_and(is_loopback_host);
        let loopback_http = parsed.scheme() == "http" && loopback;
        if parsed.scheme() != "https" && !loopback_http {
            return Err(NogentError::Config(format!(
                "{name} must use HTTPS or HTTP on a loopback host"
            )));
        }

        Ok(ApiBase {
            url: raw.trim_end_matches('/').to_string(),
            loopback_http,
        })
    }

    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    #[must_use]
    pub fn requires_https(&self) -> bool {
        !self.loopback_http
    }
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_https_and_nono_loopback_proxy() {
        let direct = ApiBase::parse("API", "https://api.github.com").expect("https");
        assert!(direct.requires_https());

        let proxy =
            ApiBase::parse("API", "http://127.0.0.1:4321/nogent_github").expect("loopback proxy");
        assert!(!proxy.requires_https());
        assert_eq!(proxy.url(), "http://127.0.0.1:4321/nogent_github");
    }

    #[test]
    fn rejects_plaintext_remote_and_embedded_credentials() {
        assert!(ApiBase::parse("API", "http://example.com").is_err());
        assert!(ApiBase::parse("API", "https://token@example.com").is_err());
    }
}
