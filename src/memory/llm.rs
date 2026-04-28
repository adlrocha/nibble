//! OpenAI-compatible LLM client for memory extraction and embeddings.
//!
//! Uses `ureq` for HTTP requests. Works with local LLM servers
//! (llama.cpp, Ollama, LM Studio, etc.) via the OpenAI-compatible API.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Configuration for the LLM client.
#[derive(Debug, Clone)]
pub struct LlmClient {
    base_url: String,
    api_key: String,
    model: String,
    #[allow(dead_code)]
    embedding_model: String,
    #[allow(dead_code)]
    embedding_dims: usize,
}

impl LlmClient {
    pub fn from_config(cfg: &crate::config::MemoryLlmConfig) -> Self {
        Self {
            base_url: cfg.base_url.clone(),
            api_key: cfg.api_key.clone(),
            model: cfg.model.clone(),
            embedding_model: cfg.embedding_model.clone(),
            embedding_dims: cfg.embedding_dims,
        }
    }

    /// Check if the LLM endpoint is reachable.
    pub fn is_available(&self) -> bool {
        let url = format!("{}/models", self.base_url.trim_end_matches('/'));
        let req = ureq::get(&url).timeout(std::time::Duration::from_secs(3));
        let req = if !self.api_key.is_empty() {
            req.set("Authorization", &format!("Bearer {}", self.api_key))
        } else {
            req
        };
        req.call().is_ok()
    }

    /// Send a chat completion request and return the assistant's text response.
    pub fn chat_completion(&self, messages: Vec<Message>, temperature: f32) -> Result<String> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));

        let body = ChatRequest {
            model: self.model.clone(),
            messages,
            temperature: Some(temperature),
            max_tokens: Some(4096),
        };

        let req = ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(120));

        let req = if !self.api_key.is_empty() {
            req.set("Authorization", &format!("Bearer {}", self.api_key))
        } else {
            req
        };

        let resp: ChatResponse = req
            .send_json(&body)
            .with_context(|| format!("LLM chat request failed to {}", url))?
            .into_json()
            .with_context(|| "Failed to parse LLM chat response")?;

        let content = resp
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .unwrap_or_default();

        Ok(content)
    }

    /// Compute an embedding vector for the given text.
    #[allow(dead_code)]
    pub fn embedding(&self, text: &str) -> Result<Vec<f32>> {
        let url = format!("{}/embeddings", self.base_url.trim_end_matches('/'));

        let body = EmbeddingRequest {
            model: self.embedding_model.clone(),
            input: text.to_string(),
        };

        let req = ureq::post(&url)
            .set("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(60));

        let req = if !self.api_key.is_empty() {
            req.set("Authorization", &format!("Bearer {}", self.api_key))
        } else {
            req
        };

        let resp: EmbeddingResponse = req
            .send_json(&body)
            .with_context(|| format!("LLM embedding request failed to {}", url))?
            .into_json()
            .with_context(|| "Failed to parse LLM embedding response")?;

        let embedding = resp
            .data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .unwrap_or_default();

        // Validate dimensions
        if embedding.len() != self.embedding_dims {
            anyhow::bail!(
                "Embedding dimension mismatch: expected {}, got {}. \
                 The configured embedding_dims ({}) does not match the model output.",
                self.embedding_dims,
                embedding.len(),
                self.embedding_dims
            );
        }

        Ok(embedding)
    }
}

// ── Data types for OpenAI-compatible API ─────────────────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<i32>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: AssistantMessage,
}

#[derive(Debug, Deserialize)]
struct AssistantMessage {
    content: Option<String>,
}

#[derive(Debug, Serialize)]
#[allow(dead_code)]
struct EmbeddingRequest {
    model: String,
    input: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
}
