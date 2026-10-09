//! Generates persona system prompts with a configurable LLM API (DeepSeek by default).

use std::time::Duration;

use anyhow::{Context, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::{LlmApi, PromptLlmConfig};

#[derive(Debug, Deserialize)]
pub struct GenerateRequest {
    pub name: String,
    #[serde(default)]
    pub role: String,
    /// Free-form description of what the persona should be good at.
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub runtime: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub team_name: String,
    #[serde(default)]
    pub team_description: String,
    /// Other team members as "name — role" strings, for context.
    #[serde(default)]
    pub teammates: Vec<String>,
    /// Existing prompt to refine, if any.
    #[serde(default)]
    pub current_prompt: String,
}

const SYSTEM: &str = "You write system prompts for AI coding agents that work together in a multi-agent team. \
A prompt you write is used verbatim as the persona definition of one agent. Write it in second person (\"You are…\"). \
Cover: identity and seniority, core expertise, responsibilities within the team, what to deliver and in which form \
(files in the shared workspace, concise reports), quality bar and working principles, and when to involve which \
teammate. Be concrete and opinionated, 200-450 words, Markdown allowed. Do NOT describe the messaging syntax: \
the orchestrator appends communication instructions itself. Output only the prompt, no preamble.";

pub fn user_message(req: &GenerateRequest) -> String {
    let mut s = format!("Persona name: {}\nRole: {}\n", req.name, req.role);
    if !req.description.trim().is_empty() {
        s.push_str(&format!("What this persona should be/do: {}\n", req.description.trim()));
    }
    if !req.runtime.is_empty() {
        s.push_str(&format!(
            "It runs in the `{}` agent CLI{}.\n",
            req.runtime,
            if req.model.is_empty() { String::new() } else { format!(" with model `{}`", req.model) }
        ));
    }
    if !req.team_name.is_empty() {
        s.push_str(&format!("Team: {}. {}\n", req.team_name, req.team_description));
    }
    if !req.teammates.is_empty() {
        s.push_str("Teammates:\n");
        for t in &req.teammates {
            s.push_str(&format!("- {t}\n"));
        }
    }
    if !req.current_prompt.trim().is_empty() {
        s.push_str(&format!(
            "\nImprove this existing prompt while keeping its intent:\n---\n{}\n---\n",
            req.current_prompt.trim()
        ));
    }
    s
}

pub async fn generate(cfg: &PromptLlmConfig, req: &GenerateRequest) -> anyhow::Result<String> {
    let key = std::env::var(&cfg.api_key_env).ok().filter(|k| !k.is_empty()).with_context(|| {
        format!("environment variable {} is not set (API key for {})", cfg.api_key_env, cfg.base_url)
    })?;
    let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build()?;
    let base = cfg.base_url.trim_end_matches('/');
    let user = user_message(req);

    let (url, body) = match cfg.api {
        LlmApi::Openai => (
            if base.ends_with("/chat/completions") { base.to_string() } else { format!("{base}/chat/completions") },
            json!({
                "model": cfg.model,
                "temperature": cfg.temperature,
                "messages": [
                    {"role": "system", "content": SYSTEM},
                    {"role": "user", "content": user},
                ],
            }),
        ),
        LlmApi::Anthropic => (
            if base.ends_with("/v1/messages") { base.to_string() } else { format!("{base}/v1/messages") },
            json!({
                "model": cfg.model,
                "max_tokens": 2048,
                "temperature": cfg.temperature,
                "system": SYSTEM,
                "messages": [{"role": "user", "content": user}],
            }),
        ),
    };

    let mut request = client.post(&url).json(&body);
    request = match cfg.api {
        LlmApi::Openai => request.bearer_auth(&key),
        LlmApi::Anthropic => request.header("x-api-key", &key).header("anthropic-version", "2023-06-01"),
    };
    let resp = request.send().await.with_context(|| format!("calling {url}"))?;
    let status = resp.status();
    let value: Value = resp.json().await.context("decoding LLM response")?;
    if !status.is_success() {
        bail!("LLM API returned {status}: {}", value["error"]["message"].as_str().unwrap_or(&value.to_string()));
    }
    let text = match cfg.api {
        LlmApi::Openai => value["choices"][0]["message"]["content"].as_str().map(str::to_owned),
        LlmApi::Anthropic => value["content"]
            .as_array()
            .map(|parts| parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("")),
    };
    let text = text.filter(|t| !t.trim().is_empty()).context("LLM response contained no text")?;
    Ok(text.trim().to_string())
}
