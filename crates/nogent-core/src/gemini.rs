//! Gemini `generateContent` wire types and response text extraction.
//!
//! The actual HTTP call lives in the worker (`nogent-worker::gemini_client`);
//! this module owns only the (de)serializable shapes so they can be unit
//! tested without a network and reused on both sides.

use serde::{Deserialize, Serialize};

use crate::error::{NogentError, Result};

/// Low temperature + tight sampling: this is a security review, not creative
/// writing. Matches the original TS client's generation config.
#[derive(Debug, Clone, Serialize)]
pub struct GenerationConfig {
    pub temperature: f32,
    #[serde(rename = "topK")]
    pub top_k: u32,
    #[serde(rename = "topP")]
    pub top_p: f32,
    #[serde(rename = "maxOutputTokens")]
    pub max_output_tokens: u32,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        GenerationConfig {
            temperature: 0.1,
            top_k: 20,
            top_p: 0.8,
            max_output_tokens: 1600,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Part {
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Content {
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SystemInstruction {
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenerateRequest {
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<SystemInstruction>,
    pub contents: Vec<Content>,
    #[serde(rename = "generationConfig")]
    pub generation_config: GenerationConfig,
}

impl GenerateRequest {
    /// Build a single-turn request with a system instruction + user prompt.
    #[must_use]
    pub fn new(system: &str, user: &str) -> Self {
        GenerateRequest {
            system_instruction: Some(SystemInstruction {
                parts: vec![Part {
                    text: system.to_string(),
                }],
            }),
            contents: vec![Content {
                role: "user".to_string(),
                parts: vec![Part {
                    text: user.to_string(),
                }],
            }],
            generation_config: GenerationConfig::default(),
        }
    }
}

// ── Response ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct RespPart {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RespContent {
    #[serde(default)]
    parts: Vec<RespPart>,
}

#[derive(Debug, Clone, Deserialize)]
struct Candidate {
    #[serde(default)]
    content: Option<RespContent>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GenerateResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
}

impl GenerateResponse {
    /// Concatenate all text parts of the first candidate, trimmed.
    pub fn text(&self) -> Result<String> {
        let text: String = self
            .candidates
            .first()
            .and_then(|c| c.content.as_ref())
            .map(|c| {
                c.parts
                    .iter()
                    .filter_map(|p| p.text.as_deref())
                    .collect::<String>()
            })
            .unwrap_or_default()
            .trim()
            .to_string();
        if text.is_empty() {
            return Err(NogentError::GeminiApi {
                status: 200,
                body: "response contained no text".to_string(),
            });
        }
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_serializes_with_camel_case_keys() {
        let req = GenerateRequest::new("sys", "usr");
        let v = serde_json::to_value(&req).expect("serialize");
        assert!(v.get("systemInstruction").is_some());
        assert_eq!(v["contents"][0]["role"], "user");
        assert_eq!(v["generationConfig"]["maxOutputTokens"], 1600);
        assert_eq!(v["contents"][0]["parts"][0]["text"], "usr");
    }

    #[test]
    fn response_extracts_concatenated_text() {
        let raw = r#"{"candidates":[{"content":{"parts":[{"text":"a"},{"text":"b"}]}}]}"#;
        let r: GenerateResponse = serde_json::from_str(raw).expect("parse");
        assert_eq!(r.text().expect("text"), "ab");
    }

    #[test]
    fn empty_response_is_error() {
        let raw = r#"{"candidates":[]}"#;
        let r: GenerateResponse = serde_json::from_str(raw).expect("parse");
        assert!(r.text().is_err());
    }
}
