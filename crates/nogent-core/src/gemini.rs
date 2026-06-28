//! Gemini `generateContent` wire types: single-turn text (issue triage) and
//! multi-turn function-calling (agentic PR review).
//!
//! `Part` carries all three shapes Gemini uses (text / functionCall /
//! functionResponse) as optional fields, so the same type serializes requests
//! and deserializes responses.

use serde::{Deserialize, Serialize};

use crate::error::{NogentError, Result};

/// Reasoning effort for thinking-capable models (Gemini 3.x), e.g. "HIGH".
#[derive(Debug, Clone, Serialize)]
pub struct ThinkingConfig {
    #[serde(rename = "thinkingLevel")]
    pub thinking_level: String,
}

/// Generation config. Gemini 3.x removed `temperature`/`topP`/`topK` ("no longer
/// recommended; remove from all requests"), so those are optional and omitted by
/// default; reasoning is governed by `thinkingConfig` instead.
#[derive(Debug, Clone, Serialize)]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(rename = "topK", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    // Reviews (summary + findings) can be long; too low truncates the JSON and
    // validation then fails.
    #[serde(rename = "maxOutputTokens")]
    pub max_output_tokens: u32,
    #[serde(rename = "thinkingConfig", skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
}

impl Default for GenerationConfig {
    fn default() -> Self {
        GenerationConfig {
            temperature: None,
            top_k: None,
            top_p: None,
            max_output_tokens: 8192,
            thinking_config: None,
        }
    }
}

impl GenerationConfig {
    /// Default config plus an optional reasoning level (e.g. "HIGH"). An empty
    /// level is treated as unset.
    #[must_use]
    pub fn with_thinking(thinking_level: Option<&str>) -> Self {
        let thinking_config = match thinking_level {
            Some(level) if !level.trim().is_empty() => Some(ThinkingConfig {
                thinking_level: level.to_string(),
            }),
            _ => None,
        };
        GenerationConfig {
            thinking_config,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Gemini 3.x assigns each call an id; the matching response must echo it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub name: String,
    pub response: serde_json::Value,
}

/// A content part. `text` / `function_call` / `function_response` are the
/// payload; `thought_signature` is an opaque token Gemini 3.x attaches to a
/// model `functionCall` part and which MUST be echoed back verbatim when the
/// turn is replayed in history (otherwise the API rejects the next request).
/// Capturing it here means `Part` clones preserve it on the round trip.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Part {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(rename = "functionCall", skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(rename = "functionResponse", skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    #[serde(rename = "thoughtSignature", skip_serializing_if = "Option::is_none")]
    pub thought_signature: Option<String>,
}

impl Part {
    #[must_use]
    pub fn text(s: impl Into<String>) -> Self {
        Part {
            text: Some(s.into()),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn function_response(id: Option<&str>, name: &str, response: serde_json::Value) -> Self {
        Part {
            function_response: Some(FunctionResponse {
                id: id.map(str::to_string),
                name: name.to_string(),
                response,
            }),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Content {
    pub role: String,
    pub parts: Vec<Part>,
}

impl Content {
    #[must_use]
    pub fn user_text(s: impl Into<String>) -> Self {
        Content {
            role: "user".to_string(),
            parts: vec![Part::text(s)],
        }
    }

    #[must_use]
    pub fn model(parts: Vec<Part>) -> Self {
        Content {
            role: "model".to_string(),
            parts,
        }
    }

    /// Tool results are sent back in a `user` content (the REST API's role enum
    /// is user/model; functionResponse parts ride in a user turn).
    #[must_use]
    pub fn tool_results(parts: Vec<Part>) -> Self {
        Content {
            role: "user".to_string(),
            parts,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FunctionDeclaration {
    pub name: String,
    pub description: String,
    /// OpenAPI-subset JSON schema for the args.
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    #[serde(rename = "functionDeclarations")]
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Clone, Serialize)]
struct SystemInstruction {
    parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GenerateRequest {
    #[serde(rename = "systemInstruction", skip_serializing_if = "Option::is_none")]
    system_instruction: Option<SystemInstruction>,
    contents: Vec<Content>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<Tool>,
    #[serde(rename = "generationConfig")]
    generation_config: GenerationConfig,
}

impl GenerateRequest {
    /// Single-turn text request (issue triage; no tools).
    #[must_use]
    pub fn new(system: &str, user: &str, thinking_level: Option<&str>) -> Self {
        GenerateRequest {
            system_instruction: Some(SystemInstruction {
                parts: vec![Part::text(system)],
            }),
            contents: vec![Content::user_text(user)],
            generation_config: GenerationConfig::with_thinking(thinking_level),
            tools: Vec::new(),
        }
    }

    /// Multi-turn request with a running conversation + (optionally) tools.
    #[must_use]
    pub fn with_contents(
        system: &str,
        contents: Vec<Content>,
        tools: Vec<Tool>,
        thinking_level: Option<&str>,
    ) -> Self {
        GenerateRequest {
            system_instruction: Some(SystemInstruction {
                parts: vec![Part::text(system)],
            }),
            contents,
            tools,
            generation_config: GenerationConfig::with_thinking(thinking_level),
        }
    }
}

// ── Response ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
struct RespContent {
    #[serde(default)]
    parts: Vec<Part>,
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
    /// Parts of the first candidate (text and/or functionCall).
    #[must_use]
    pub fn first_parts(&self) -> Vec<Part> {
        self.candidates
            .first()
            .and_then(|c| c.content.as_ref())
            .map(|c| c.parts.clone())
            .unwrap_or_default()
    }

    /// Concatenated text of the first candidate, trimmed. Errors if empty.
    pub fn text(&self) -> Result<String> {
        let text: String = self
            .first_parts()
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<String>()
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
    fn single_turn_serializes_without_tools_or_sampling_params() {
        let req = GenerateRequest::new("sys", "usr", Some("high"));
        let v = serde_json::to_value(&req).expect("serialize");
        assert!(v.get("systemInstruction").is_some());
        assert_eq!(v["contents"][0]["role"], "user");
        assert_eq!(v["contents"][0]["parts"][0]["text"], "usr");
        // tools omitted when empty.
        assert!(v.get("tools").is_none());
        // Gemini 3.x: temperature/topP/topK must NOT be sent.
        assert!(v["generationConfig"].get("temperature").is_none());
        assert!(v["generationConfig"].get("topP").is_none());
        assert_eq!(
            v["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    #[test]
    fn thinking_omitted_when_level_empty() {
        let req = GenerateRequest::new("sys", "usr", None);
        let v = serde_json::to_value(&req).expect("serialize");
        assert!(v["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn tool_request_serializes_function_declarations() {
        let tools = vec![Tool {
            function_declarations: vec![FunctionDeclaration {
                name: "grep".into(),
                description: "search".into(),
                parameters: serde_json::json!({"type":"object"}),
            }],
        }];
        let req =
            GenerateRequest::with_contents("sys", vec![Content::user_text("hi")], tools, None);
        let v = serde_json::to_value(&req).expect("serialize");
        assert_eq!(v["tools"][0]["functionDeclarations"][0]["name"], "grep");
    }

    #[test]
    fn response_extracts_function_call_with_id() {
        let raw = r#"{"candidates":[{"content":{"parts":[
            {"functionCall":{"id":"call_1","name":"grep","args":{"pattern":"fn add"}}},
            {"text":"ignored-while-calling"}
        ]}}]}"#;
        let r: GenerateResponse = serde_json::from_str(raw).expect("parse");
        let parts = r.first_parts();
        let call = parts
            .iter()
            .find_map(|p| p.function_call.clone())
            .expect("call");
        assert_eq!(call.name, "grep");
        assert_eq!(call.id.as_deref(), Some("call_1"));
        assert_eq!(call.args["pattern"], "fn add");
    }

    #[test]
    fn thought_signature_round_trips_when_echoing_model_turn() {
        // Gemini 3.x attaches a thoughtSignature to the functionCall part; it
        // must survive deserialize → re-serialize when we replay the turn.
        let raw = r#"{"candidates":[{"content":{"parts":[
            {"functionCall":{"id":"c1","name":"grep","args":{}},"thoughtSignature":"SIG123"}
        ]}}]}"#;
        let r: GenerateResponse = serde_json::from_str(raw).expect("parse");
        let parts = r.first_parts();
        let echoed = serde_json::to_value(Content::model(parts)).expect("serialize");
        assert_eq!(echoed["parts"][0]["thoughtSignature"], "SIG123");
        assert_eq!(echoed["parts"][0]["functionCall"]["name"], "grep");
    }

    #[test]
    fn function_response_part_echoes_id() {
        let p = Part::function_response(Some("call_1"), "grep", serde_json::json!({"matches": []}));
        let v = serde_json::to_value(&p).expect("serialize");
        assert_eq!(v["functionResponse"]["id"], "call_1");
        assert_eq!(v["functionResponse"]["name"], "grep");
        assert!(v["functionResponse"]["response"]["matches"].is_array());
    }
}
