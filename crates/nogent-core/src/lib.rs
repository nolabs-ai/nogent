//! nogent-core — shared types and pure logic for the nogent GitHub App.
//!
//! This crate is deliberately transport-light: it holds the data structures,
//! the prompt templates, the canary output validator and the diff bounding
//! logic that BOTH the trusted listener and the sandboxed worker depend on.
//!
//! Security note: nothing here reads a real credential. The worker that links
//! this crate only ever holds phantom session tokens injected by the nono
//! proxy; the real GitHub installation token and Gemini key live in the
//! trusted listener process and are swapped in by nono at network egress.

pub mod diff_digest;
pub mod error;
pub mod events;
pub mod gemini;
pub mod hmac_verify;
pub mod output_validator;
pub mod prompts;
pub mod repo_config;

pub use error::{NogentError, Result};
