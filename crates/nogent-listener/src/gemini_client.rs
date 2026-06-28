//! Gemini access, direct to `generativelanguage.googleapis.com` with the real
//! API key.

use nogent_core::error::{NogentError, Result};
use nogent_core::gemini::{GenerateRequest, GenerateResponse};

const GEMINI_API: &str = "https://generativelanguage.googleapis.com";

pub struct GeminiClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
}

impl GeminiClient {
    pub fn new(api_key: &str, model: &str) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent("nogent").build()?;
        Ok(GeminiClient {
            http,
            api_key: api_key.to_string(),
            model: model.to_string(),
        })
    }

    /// Run a single-turn generateContent and return the raw response text.
    pub async fn generate(&self, system: &str, user: &str) -> Result<String> {
        let url = format!("{GEMINI_API}/v1beta/models/{}:generateContent", self.model);
        let req = GenerateRequest::new(system, user);
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&req)
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await?;
        if !status.is_success() {
            return Err(NogentError::GeminiApi {
                status: status.as_u16(),
                body,
            });
        }
        let parsed: GenerateResponse = serde_json::from_str(&body)?;
        parsed.text()
    }
}
