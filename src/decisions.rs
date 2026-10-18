//! Quick orchestration decisions with Jev, TypeSafe's "System One" decision model.
//!
//! Jev answers typed questions (choice / score / yes-no) about a state in ~100ms for a fraction of
//! an LLM call. Chimera uses it for cheap routing calls between agent turns:
//! - not waking a persona for pure acknowledgements,
//! - repairing messages addressed to an unknown teammate id,
//! - noticing when the entry persona finished but forgot `<final>`,
//! - routing a user's follow-up message to the right persona,
//! - (optional) choosing the reasoning effort a turn needs.
//!
//! Every decision is optional: if Jev is disabled, unconfigured or fails, chimera falls back to its
//! default behaviour.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{Effort, Persona, Team};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct DecisionsConfig {
    pub enabled: bool,
    /// Base URL of a System One API; requests go to `{base_url}/systemone`.
    /// OpenRouter: `https://openrouter.ai/api/v1`; TypeSafe direct: `https://api.typesafe.ai/v1`.
    pub base_url: String,
    pub model: String,
    /// Environment variable with the API key (falls back to the variables in `FALLBACK_KEY_ENVS`).
    /// Empty for servers that need no key, such as a local Ollama.
    pub api_key_env: String,
    pub timeout_ms: u64,
    /// Do not start a turn for messages that are only acknowledgements.
    pub ack_filter: bool,
    /// Minimum confidence for treating a message as an acknowledgement.
    pub ack_min_confidence: f64,
    /// Map messages sent to unknown ids onto the intended teammate.
    pub resolve_recipients: bool,
    pub resolve_min_confidence: f64,
    /// Finish the session when the entry persona's reply says the task is done but lacks `<final>`.
    pub detect_final: bool,
    pub final_threshold: f64,
    /// Allow "auto" as the recipient of user messages.
    pub route_user_messages: bool,
    pub route_min_confidence: f64,
    /// Before each turn, let Jev pick the effort the turn needs (off by default). The persona's
    /// configured effort is the ceiling unless `effort_can_raise` is set.
    pub advise_effort: bool,
    pub effort_min_confidence: f64,
    /// Allow one level above the configured effort for turns Jev judges harder than usual.
    pub effort_can_raise: bool,
}

impl Default for DecisionsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_url: OPENROUTER_BASE_URL.into(),
            model: "jev-latest".into(),
            api_key_env: "JEV_API_KEY".into(),
            timeout_ms: 5000,
            ack_filter: true,
            ack_min_confidence: 0.6,
            resolve_recipients: true,
            resolve_min_confidence: 0.5,
            detect_final: true,
            final_threshold: 0.9,
            route_user_messages: true,
            route_min_confidence: 0.4,
            advise_effort: false,
            effort_min_confidence: 0.5,
            effort_can_raise: false,
        }
    }
}

/// OpenRouter's System One API, which implements TypeSafe's request and response format.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";

const FALLBACK_KEY_ENVS: &[&str] = &["JEV_API_KEY", "OPENROUTER_API_KEY", "TYPESAFE_API_KEY"];

impl DecisionsConfig {
    /// No key is configured on purpose (local servers such as Ollama).
    pub fn keyless(&self) -> bool {
        self.api_key_env.trim().is_empty()
    }

    /// The API key and the variable it came from. Keyless configurations never pick up a
    /// fallback key, so a cloud key is not sent to a local server.
    pub fn api_key(&self) -> Option<(String, String)> {
        if self.keyless() {
            return None;
        }
        std::iter::once(self.api_key_env.as_str())
            .chain(FALLBACK_KEY_ENVS.iter().copied())
            .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()).map(|v| (v, var.to_string())))
    }

    /// Enabled, and either keyless or a key is available.
    pub fn active(&self) -> bool {
        self.enabled && (self.keyless() || self.api_key().is_some())
    }
}

/// A typed question in Jev's request format.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Noul {
        instructions: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<Value>,
    },
    Choice {
        instructions: Value,
        criteria: BTreeMap<String, Value>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        confidence: f64,
    },
}

#[derive(Debug, Deserialize)]
struct SystemOneResponse {
    #[serde(default)]
    model: String,
    answers: BTreeMap<String, Answer>,
    #[serde(default)]
    usage: Usage,
}

#[derive(Debug, Default, Deserialize)]
struct Usage {
    /// Reported by OpenRouter (USD).
    #[serde(default)]
    cost: Option<f64>,
}

/// Facts about one Jev call.
#[derive(Debug, Clone)]
struct CallInfo {
    model: String,
    latency_ms: u64,
    cost: Option<f64>,
}

/// The outcome of one decision, for the event log.
#[derive(Debug, Clone)]
pub struct Verdict<T> {
    pub value: T,
    pub confidence: f64,
    pub latency_ms: u64,
    pub model: String,
    /// Cost in USD when the provider reports it (OpenRouter does).
    pub cost: Option<f64>,
}

impl<T> Verdict<T> {
    fn new(value: T, confidence: f64, info: CallInfo) -> Self {
        Self { value, confidence, latency_ms: info.latency_ms, model: info.model, cost: info.cost }
    }
}

pub struct Jev {
    http: reqwest::Client,
}

impl Default for Jev {
    fn default() -> Self {
        Self::new()
    }
}

impl Jev {
    pub fn new() -> Self {
        Self { http: reqwest::Client::new() }
    }

    /// Sends one request with a single question and returns its answer.
    async fn ask(&self, cfg: &DecisionsConfig, state: Value, question: Question) -> anyhow::Result<(Answer, CallInfo)> {
        let key = match cfg.api_key() {
            Some((key, _)) => Some(key),
            None if cfg.keyless() => None,
            None => bail!("no Jev API key configured"),
        };
        let url = format!("{}/systemone", cfg.base_url.trim_end_matches('/'));
        let started = Instant::now();
        let mut req = self.http.post(&url);
        if let Some(key) = key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .timeout(Duration::from_millis(cfg.timeout_ms.max(100)))
            .json(&json!({ "model": cfg.model, "state": state, "questions": { "q": question } }))
            .send()
            .await
            .with_context(|| format!("calling {url}"))?;
        let status = resp.status();
        let body: Value = resp.json().await.context("decoding Jev response")?;
        if !status.is_success() {
            let msg =
                body["detail"]["message"].as_str().or(body["error"]["message"].as_str()).unwrap_or("request failed");
            bail!("Jev returned {status}: {msg}");
        }
        let mut parsed: SystemOneResponse = serde_json::from_value(body).context("unexpected Jev response shape")?;
        let answer = parsed.answers.remove("q").context("Jev response has no answer")?;
        let info =
            CallInfo { model: parsed.model, latency_ms: started.elapsed().as_millis() as u64, cost: parsed.usage.cost };
        Ok((answer, info))
    }

    async fn noul(
        &self,
        cfg: &DecisionsConfig,
        state: Value,
        instructions: Value,
        criteria: Option<Value>,
    ) -> anyhow::Result<Verdict<f64>> {
        match self.ask(cfg, state, Question::Noul { instructions, criteria }).await? {
            (Answer::Noul { noul }, info) => Ok(Verdict::new(noul, (noul - 0.5).abs() * 2.0, info)),
            (other, ..) => bail!("expected a noul answer, got {other:?}"),
        }
    }

    async fn choice(
        &self,
        cfg: &DecisionsConfig,
        state: Value,
        instructions: Value,
        criteria: BTreeMap<String, Value>,
    ) -> anyhow::Result<Verdict<String>> {
        match self.ask(cfg, state, Question::Choice { instructions, criteria }).await? {
            (Answer::Choice { choice, confidence }, info) => Ok(Verdict::new(choice, confidence, info)),
            (other, ..) => bail!("expected a choice answer, got {other:?}"),
        }
    }

    /// Classifies a teammate message as `request`, `report` or `ack`.
    pub async fn classify_message(
        &self,
        cfg: &DecisionsConfig,
        from: &Persona,
        to: &Persona,
        content: &str,
    ) -> anyhow::Result<Verdict<String>> {
        let state = json!({
            "sender": describe(from),
            "recipient": describe(to),
            "message": clip(content, 6000),
        });
        let criteria = BTreeMap::from([
            (
                "request".to_string(),
                json!(
                    "Asks the recipient to do work, answer a question, make a decision, review something, or fix a problem."
                ),
            ),
            (
                "report".to_string(),
                json!(
                    "Delivers results, completed work, findings, a status the recipient is waiting on, or information it needs to continue."
                ),
            ),
            (
                "ack".to_string(),
                json!(
                    "Only acknowledges, thanks, agrees or signs off; nothing new for the recipient to act on and no reply expected."
                ),
            ),
        ]);
        self.choice(cfg, state, json!("What kind of message is `message` from `sender` to `recipient`?"), criteria)
            .await
    }

    /// Picks which allowed recipient an unknown id was meant to be; `none` when unclear.
    pub async fn resolve_recipient(
        &self,
        cfg: &DecisionsConfig,
        from: &Persona,
        addressed_to: &str,
        content: &str,
        candidates: &[&Persona],
    ) -> anyhow::Result<Verdict<String>> {
        let mut criteria: BTreeMap<String, Value> =
            candidates.iter().map(|p| (p.id.clone(), json!(describe(p)))).collect();
        criteria.insert("none".into(), json!("None of these teammates is a plausible match."));
        let state = json!({
            "sender": describe(from),
            "addressed_to": addressed_to,
            "message": clip(content, 4000),
        });
        self.choice(
            cfg,
            state,
            json!("The sender addressed the message to `addressed_to`, which is not a valid teammate id. Which teammate did it mean?"),
            criteria,
        )
        .await
    }

    /// Probability that the entry persona's reply declares the whole task finished.
    pub async fn task_complete(&self, cfg: &DecisionsConfig, task: &str, reply: &str) -> anyhow::Result<Verdict<f64>> {
        let state = json!({ "task": clip(task, 4000), "lead_reply": clip(reply, 8000) });
        self.noul(
            cfg,
            state,
            json!("Does `lead_reply` state that the whole `task` is finished and present the final result to the user, with no work still pending from teammates?"),
            Some(json!({
                "true": "The task is complete and the reply hands the result to the user.",
                "false": "Work is still in progress, delegated, blocked, or waiting on someone.",
            })),
        )
        .await
    }

    /// Picks the persona best suited to handle a user's message.
    pub async fn route_user_message(
        &self,
        cfg: &DecisionsConfig,
        team: &Team,
        task: &str,
        content: &str,
    ) -> anyhow::Result<Verdict<String>> {
        let criteria = team
            .personas
            .iter()
            .map(|p| {
                let mut d = json!({ "who": describe(p) });
                if p.id == team.entry {
                    d["scope"] =
                        json!("Team lead and entry point: handles anything general, cross-cutting or unclear.");
                }
                // The persona's own prompt is the best description of its expertise.
                if !p.prompt.trim().is_empty() {
                    d["expertise"] = json!(clip(p.prompt.trim(), 600));
                }
                (p.id.clone(), d)
            })
            .collect();
        let state = json!({ "original_task": clip(task, 3000), "user_message": clip(content, 4000) });
        self.choice(cfg, state, json!("Which team member should handle `user_message`?"), criteria).await
    }

    /// Picks the reasoning effort a persona's next turn needs, among `candidates`.
    pub async fn advise_effort(
        &self,
        cfg: &DecisionsConfig,
        persona: &Persona,
        task: &str,
        incoming: &[crate::session::InboxMessage],
        candidates: &[Effort],
    ) -> anyhow::Result<Verdict<String>> {
        let criteria = candidates.iter().map(|e| (e.as_str().to_string(), json!(effort_meaning(*e)))).collect();
        let mut messages = String::new();
        for m in incoming {
            messages.push_str(&format!("From {}:\n{}\n\n", m.from, clip(m.content.trim(), 3000)));
        }
        let mut agent = json!({ "who": describe(persona), "usual_effort": persona.effort.as_str() });
        if !persona.prompt.trim().is_empty() {
            agent["expertise"] = json!(clip(persona.prompt.trim(), 600));
        }
        let state = json!({
            "agent": agent,
            "original_task": clip(task, 3000),
            "new_messages": clip(&messages, 8000),
        });
        self.choice(
            cfg,
            state,
            json!("How much reasoning effort does `agent` need for the work `new_messages` ask of it? Pick the lowest level that will still do the work well."),
            criteria,
        )
        .await
    }

    /// Minimal request to check that the configuration works.
    pub async fn ping(&self, cfg: &DecisionsConfig) -> anyhow::Result<Verdict<f64>> {
        self.noul(cfg, json!("Hello, can you hear me?"), json!("Is this a greeting or a check-in?"), None).await
    }
}

fn effort_meaning(e: Effort) -> &'static str {
    match e {
        Effort::Minimal => "Trivial: acknowledge, relay or answer from what is already known; no real reasoning.",
        Effort::Low => "Simple and well defined: a small edit, a routine command, a straightforward answer or report.",
        Effort::Medium => "Ordinary work: a feature or fix of moderate size that needs some planning and reasoning.",
        Effort::High => "Hard: design decisions, tricky debugging, reviews for subtle bugs, many interacting parts.",
        Effort::Xhigh => "Very hard: deep architecture, algorithmic or security work where mistakes are costly.",
        Effort::Max | Effort::Ultra => {
            "Exceptionally hard, open-ended problems that need the most thorough reasoning available."
        }
    }
}

/// Efforts the advisor may choose for a persona configured with `configured`: from `low` up to
/// the configured level (one above it when raising is allowed, but never into `ultra` unless it
/// is configured). `minimal` is offered only to personas configured with it.
pub fn effort_candidates(configured: Effort, can_raise: bool) -> Vec<Effort> {
    let mut top = configured;
    if can_raise && configured < Effort::Max {
        top = Effort::ALL[Effort::ALL.iter().position(|e| *e == configured).unwrap_or(0) + 1];
    }
    let bottom = if configured == Effort::Minimal { Effort::Minimal } else { Effort::Low };
    Effort::ALL.into_iter().filter(|e| *e >= bottom && *e <= top).collect()
}

#[cfg(test)]
#[test]
fn effort_candidates_are_capped() {
    use Effort::*;
    assert_eq!(effort_candidates(High, false), vec![Low, Medium, High]);
    assert_eq!(effort_candidates(High, true), vec![Low, Medium, High, Xhigh]);
    assert_eq!(effort_candidates(Low, false), vec![Low]);
    assert_eq!(effort_candidates(Minimal, true), vec![Minimal, Low]);
    assert_eq!(effort_candidates(Max, true), vec![Low, Medium, High, Xhigh, Max]);
}

fn describe(p: &Persona) -> String {
    if p.role.is_empty() { p.name.clone() } else { format!("{} — {}", p.name, p.role) }
}

fn clip(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_serialize_in_jev_format() {
        let q = Question::Choice {
            instructions: json!("Which?"),
            criteria: BTreeMap::from([("a".to_string(), json!("A")), ("b".to_string(), Value::Null)]),
        };
        assert_eq!(
            serde_json::to_value(&q).unwrap(),
            json!({"type": "choice", "instructions": "Which?", "criteria": {"a": "A", "b": null}})
        );
        let q = Question::Noul { instructions: json!("Yes?"), criteria: None };
        assert_eq!(serde_json::to_value(&q).unwrap(), json!({"type": "noul", "instructions": "Yes?"}));
    }

    #[test]
    fn answers_parse_from_documented_examples() {
        let r: SystemOneResponse = serde_json::from_value(json!({
            "model": "jev-1.13.0",
            "answers": {
                "a": {"type": "noul", "noul": 0.95},
                "b": {"type": "choice", "choice": "billing", "probabilities": {"billing": 0.88, "technical": 0.12}, "confidence": 0.81}
            },
            "usage": {"input_tokens": 318, "output_tokens": 34}
        }))
        .unwrap();
        assert!(matches!(r.answers["a"], Answer::Noul { noul } if noul == 0.95));
        assert!(matches!(&r.answers["b"], Answer::Choice { choice, .. } if choice == "billing"));
    }

    #[test]
    fn keyless_config_is_active_and_never_borrows_a_cloud_key() {
        // SAFETY: test-only variable with a unique name.
        unsafe { std::env::set_var("JEV_API_KEY", "cloud-key-must-not-leak") };
        let local = DecisionsConfig {
            base_url: "http://localhost:11434/v1".into(),
            model: "nimble".into(),
            api_key_env: String::new(),
            ..Default::default()
        };
        assert!(local.keyless());
        assert!(local.active());
        assert!(local.api_key().is_none(), "a local server must not receive the cloud key");
        let cloud = DecisionsConfig::default();
        assert_eq!(cloud.api_key().map(|(_, var)| var).as_deref(), Some("JEV_API_KEY"));
    }

    #[test]
    fn clip_respects_char_boundaries() {
        assert_eq!(clip("héllo", 2), "h…");
        assert_eq!(clip("abc", 10), "abc");
    }
}
