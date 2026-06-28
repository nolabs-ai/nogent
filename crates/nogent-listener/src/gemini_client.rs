//! Gemini access, direct to `generativelanguage.googleapis.com` with the real
//! API key. Supports single-turn text (triage) and multi-turn tool-calling
//! (agentic review).

use nogent_core::error::{NogentError, Result};
use nogent_core::gemini::{Content, GenerateRequest, GenerateResponse, Part, Tool};

const GEMINI_API: &str = "https://generativelanguage.googleapis.com";

pub struct GeminiClient {
    http: reqwest::Client,
    api_key: String,
    model: String,
    /// Reasoning level (e.g. "high") for thinking-capable models; None to omit.
    thinking_level: Option<String>,
}

impl GeminiClient {
    pub fn new(api_key: &str, model: &str, thinking_level: Option<&str>) -> Result<Self> {
        let http = reqwest::Client::builder().user_agent("nogent").build()?;
        Ok(GeminiClient {
            http,
            api_key: api_key.to_string(),
            model: model.to_string(),
            thinking_level: thinking_level.map(str::to_string),
        })
    }

    async fn post(&self, req: &GenerateRequest) -> Result<GenerateResponse> {
        let url = format!("{GEMINI_API}/v1beta/models/{}:generateContent", self.model);
        let resp = self
            .http
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(req)
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
        serde_json::from_str(&body).map_err(NogentError::from)
    }

    /// Single-turn: system + user prompt, no tools. Returns the response text.
    pub async fn generate(&self, system: &str, user: &str) -> Result<String> {
        self.post(&GenerateRequest::new(
            system,
            user,
            self.thinking_level.as_deref(),
        ))
        .await?
        .text()
    }

    /// One turn of a multi-turn conversation with tools. Returns the model's
    /// parts (text and/or functionCall) so the caller can drive the loop.
    pub async fn generate_turn(
        &self,
        system: &str,
        contents: &[Content],
        tools: &[Tool],
    ) -> Result<Vec<Part>> {
        let req = GenerateRequest::with_contents(
            system,
            contents.to_vec(),
            tools.to_vec(),
            self.thinking_level.as_deref(),
        );
        Ok(self.post(&req).await?.first_parts())
    }
}
