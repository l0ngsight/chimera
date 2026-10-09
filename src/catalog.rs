//! Model catalogs: asks each agent CLI (or its provider's API) which models it can run, so the
//! persona editor can offer a real list instead of free text.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use crate::config::{ModelsFormat, ModelsSource, RuntimeSpec};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelInfo {
    /// The value passed to the CLI's `--model`.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Reasoning efforts the model supports, when the source reports them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<u64>,
    /// Heading to group the model under in the UI (e.g. the opencode provider).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl ModelInfo {
    fn id(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            name: None,
            description: None,
            efforts: vec![],
            default_effort: None,
            context: None,
            group: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelList {
    pub runtime: String,
    /// `cli`, `api` or `static`.
    pub source: &'static str,
    /// What was queried, e.g. "codex debug models".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    pub models: Vec<ModelInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Set when fetching failed; `models` then holds the static suggestions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub fetched_at: DateTime<Utc>,
}

const CACHE_TTL: Duration = Duration::from_secs(600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Catalog {
    cache: Mutex<HashMap<String, (Instant, ModelList)>>,
    http: reqwest::Client,
}

impl Catalog {
    /// Models for a runtime; served from a 10-minute cache unless `refresh` is set.
    pub async fn list(&self, name: &str, spec: &RuntimeSpec, refresh: bool) -> ModelList {
        if !refresh {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((at, list)) = cache.get(name)
                && at.elapsed() < CACHE_TTL
            {
                return list.clone();
            }
        }
        let list = self.fetch(name, spec).await;
        if list.error.is_none() {
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(name.to_string(), (Instant::now(), list.clone()));
        }
        list
    }

    async fn fetch(&self, name: &str, spec: &RuntimeSpec) -> ModelList {
        let static_models: Vec<ModelInfo> = spec.models.iter().map(ModelInfo::id).collect();
        let mut list = ModelList {
            runtime: name.to_string(),
            source: "static",
            via: None,
            models: static_models.clone(),
            note: None,
            error: None,
            fetched_at: Utc::now(),
        };
        match &spec.models_source {
            ModelsSource::Static => {
                list.note = Some(format!(
                    "Suggestions from chimera.yaml; `{}` has no non-interactive model list.",
                    spec.command
                ));
            }
            ModelsSource::Command { args, format } => {
                let via = format!("{} {}", spec.command, args.join(" "));
                match run_command(spec, args).await.and_then(|out| parse(*format, &out)) {
                    Ok(models) if !models.is_empty() => {
                        list.source = "cli";
                        list.models = models;
                    }
                    Ok(_) => list.error = Some(format!("`{via}` listed no models")),
                    Err(e) => list.error = Some(format!("`{via}` failed: {e:#}")),
                }
                list.via = Some(via);
            }
            ModelsSource::AnthropicApi { api_key_env } => {
                // Claude Code has no list command: offer its aliases, plus every model id the
                // Anthropic API reports when a key is available.
                list.models =
                    static_models.into_iter().map(|m| ModelInfo { group: Some("Aliases".into()), ..m }).collect();
                match std::env::var(api_key_env).ok().filter(|k| !k.is_empty()) {
                    None => {
                        list.note = Some(format!(
                            "Claude Code has no model-list command; showing its aliases. Set {api_key_env} on the server to list every model id from the Anthropic API."
                        ));
                    }
                    Some(key) => match self.anthropic_models(&key).await {
                        Ok(models) => {
                            list.source = "api";
                            list.via = Some("Anthropic Models API".into());
                            list.models.extend(models);
                        }
                        Err(e) => list.error = Some(format!("Anthropic Models API failed: {e:#}")),
                    },
                }
            }
        }
        list
    }

    async fn anthropic_models(&self, key: &str) -> anyhow::Result<Vec<ModelInfo>> {
        let mut models = vec![];
        let mut after: Option<String> = None;
        loop {
            // Model ids are URL-safe ([a-z0-9-]), so the cursor needs no escaping.
            let url = match &after {
                Some(a) => format!("https://api.anthropic.com/v1/models?limit=1000&after_id={a}"),
                None => "https://api.anthropic.com/v1/models?limit=1000".to_string(),
            };
            let resp = self
                .http
                .get(url)
                .header("x-api-key", key)
                .header("anthropic-version", "2023-06-01")
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .context("calling the Anthropic Models API")?;
            let status = resp.status();
            let body: Value = resp.json().await.context("decoding the Anthropic Models API response")?;
            if !status.is_success() {
                bail!("{status}: {}", body["error"]["message"].as_str().unwrap_or("request failed"));
            }
            models.extend(parse_anthropic_page(&body));
            match (body["has_more"].as_bool(), body["last_id"].as_str()) {
                (Some(true), Some(last)) => after = Some(last.to_string()),
                _ => return Ok(models),
            }
        }
    }
}

async fn run_command(spec: &RuntimeSpec, args: &[String]) -> anyhow::Result<String> {
    let mut cmd = tokio::process::Command::new(&spec.command);
    cmd.args(args)
        .envs(&spec.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let out = tokio::time::timeout(FETCH_TIMEOUT, cmd.output())
        .await
        .context("timed out")?
        .with_context(|| format!("could not run `{}`", spec.command))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("exit {:?}: {}", out.status.code(), stderr.lines().last().unwrap_or("").trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn parse(format: ModelsFormat, out: &str) -> anyhow::Result<Vec<ModelInfo>> {
    Ok(match format {
        ModelsFormat::Lines => parse_lines(out),
        ModelsFormat::CodexCatalog => parse_codex(out)?,
        ModelsFormat::OpencodeVerbose => parse_opencode_verbose(out),
        ModelsFormat::PiTable => parse_pi_table(out),
    })
}

/// One model per line; the first whitespace-separated token is the id.
fn parse_lines(out: &str) -> Vec<ModelInfo> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_whitespace().next())
        .map(|id| ModelInfo { group: provider_of(id), ..ModelInfo::id(id) })
        .collect()
}

/// `codex debug models`: `{"models": [{slug, display_name, description, supported_reasoning_levels, ...}]}`.
fn parse_codex(out: &str) -> anyhow::Result<Vec<ModelInfo>> {
    let v: Value = serde_json::from_str(out.trim()).context("codex model catalog is not JSON")?;
    let models = v["models"].as_array().context("codex model catalog has no `models` array")?;
    Ok(models
        .iter()
        .filter(|m| m["visibility"].as_str() != Some("hide"))
        .filter_map(|m| {
            Some(ModelInfo {
                id: m["slug"].as_str()?.to_string(),
                name: m["display_name"].as_str().map(str::to_owned),
                description: m["description"].as_str().map(str::to_owned),
                efforts: m["supported_reasoning_levels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|l| l["effort"].as_str().map(str::to_owned))
                    .collect(),
                default_effort: m["default_reasoning_level"].as_str().map(str::to_owned),
                context: m["context_window"].as_u64(),
                group: None,
            })
        })
        .collect())
}

/// `opencode models --verbose`: a `provider/model` line followed by a pretty-printed JSON object.
/// Falls back to plain lines when there is no JSON (e.g. an older opencode).
fn parse_opencode_verbose(out: &str) -> Vec<ModelInfo> {
    let mut models = vec![];
    let mut header: Option<&str> = None;
    let mut json = String::new();
    for line in out.lines() {
        if !json.is_empty() || line.starts_with('{') {
            json.push_str(line);
            json.push('\n');
            if line.starts_with('}') {
                if let (Some(id), Ok(v)) = (header.take(), serde_json::from_str::<Value>(&json)) {
                    models.push(ModelInfo {
                        id: id.to_string(),
                        name: v["name"].as_str().map(str::to_owned),
                        description: None,
                        efforts: v["variants"].as_object().map(|o| o.keys().cloned().collect()).unwrap_or_default(),
                        default_effort: None,
                        context: v["limit"]["context"].as_u64(),
                        group: v["providerID"].as_str().map(str::to_owned).or_else(|| provider_of(id)),
                    });
                }
                json.clear();
            }
        } else if !line.trim().is_empty()
            && let Some(id) = header.replace(line.trim())
        {
            // A header without a JSON body (non-verbose output).
            models.push(ModelInfo { group: provider_of(id), ..ModelInfo::id(id) });
        }
    }
    if let Some(id) = header {
        models.push(ModelInfo { group: provider_of(id), ..ModelInfo::id(id) });
    }
    models
}

/// `pi --list-models`: a header row, then `provider model context max-out thinking images`.
fn parse_pi_table(out: &str) -> Vec<ModelInfo> {
    out.lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with("provider "))
        .filter_map(|l| {
            let cols: Vec<&str> = l.split_whitespace().collect();
            let [provider, model, context, ..] = cols.as_slice() else { return None };
            Some(ModelInfo {
                id: format!("{provider}/{model}"),
                name: None,
                description: None,
                efforts: vec![],
                default_effort: None,
                context: parse_size(context),
                group: Some(provider.to_string()),
            })
        })
        .collect()
}

/// "1M", "300K", "65.5K", "128000" → tokens.
fn parse_size(s: &str) -> Option<u64> {
    let (num, mult) = match s.chars().last()? {
        'K' | 'k' => (&s[..s.len() - 1], 1_000.0),
        'M' | 'm' => (&s[..s.len() - 1], 1_000_000.0),
        _ => (s, 1.0),
    };
    num.parse::<f64>().ok().map(|n| (n * mult).round() as u64)
}

/// One page of `GET /v1/models`.
fn parse_anthropic_page(body: &Value) -> Vec<ModelInfo> {
    body["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let efforts = m["capabilities"]["effort"]
                .as_object()
                .map(|o| {
                    o.iter()
                        .filter(|(k, v)| *k != "supported" && v["supported"].as_bool() == Some(true))
                        .map(|(k, _)| k.clone())
                        .collect()
                })
                .unwrap_or_default();
            Some(ModelInfo {
                id: m["id"].as_str()?.to_string(),
                name: m["display_name"].as_str().map(str::to_owned),
                description: None,
                efforts,
                default_effort: None,
                context: m["max_input_tokens"].as_u64(),
                group: Some("Anthropic API".into()),
            })
        })
        .collect()
}

fn provider_of(id: &str) -> Option<String> {
    id.split_once('/').map(|(p, _)| p.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_codex_catalog_and_skips_hidden_models() {
        let out = json!({"models": [
            {"slug": "gpt-5.6-sol", "display_name": "GPT-5.6-Sol", "description": "Fast", "visibility": "list",
             "default_reasoning_level": "medium", "context_window": 272000,
             "supported_reasoning_levels": [{"effort": "low"}, {"effort": "high"}, {"effort": "max"}]},
            {"slug": "internal", "visibility": "hide"}
        ]})
        .to_string();
        let models = parse(ModelsFormat::CodexCatalog, &out).unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert_eq!(models[0].efforts, ["low", "high", "max"]);
        assert_eq!(models[0].default_effort.as_deref(), Some("medium"));
    }

    #[test]
    fn parses_opencode_verbose_output() {
        let out = "deepseek/deepseek-v4-flash\n{\n  \"id\": \"deepseek-v4-flash\",\n  \"providerID\": \"deepseek\",\n  \"name\": \"DeepSeek V4 Flash\",\n  \"limit\": {\n    \"context\": 1000000\n  },\n  \"variants\": {\n    \"high\": {}\n  }\n}\nopencode/big-pickle\n{\n  \"name\": \"Big Pickle\",\n  \"variants\": {}\n}\n";
        let models = parse(ModelsFormat::OpencodeVerbose, out).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "deepseek/deepseek-v4-flash");
        assert_eq!(models[0].name.as_deref(), Some("DeepSeek V4 Flash"));
        assert_eq!(models[0].efforts, ["high"]);
        assert_eq!(models[0].context, Some(1_000_000));
        assert_eq!(models[1].group.as_deref(), Some("opencode"));
    }

    #[test]
    fn opencode_plain_output_still_parses() {
        let models = parse(ModelsFormat::OpencodeVerbose, "a/x\nb/y\n").unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a/x", "b/y"]);
    }

    #[test]
    fn parses_pi_model_table() {
        let models = parse(ModelsFormat::PiTable, include_str!("../testdata/pi-list-models.txt")).unwrap();
        assert_eq!(models.len(), 4);
        assert_eq!(models[0].id, "amazon-bedrock/amazon.nova-2-lite-v1:0");
        assert_eq!(models[0].context, Some(1_000_000));
        assert_eq!(models[0].group.as_deref(), Some("amazon-bedrock"));
        assert_eq!(models[3].id, "fake/fake-1");
        assert_eq!(models[3].context, Some(128_000));
    }

    #[test]
    fn parses_anthropic_models_page() {
        let page = json!({"data": [{"id": "claude-opus-5-5", "display_name": "Claude Opus 5.5", "max_input_tokens": 1000000,
            "capabilities": {"effort": {"supported": true, "low": {"supported": true}, "max": {"supported": true}, "minimal": {"supported": false}}}}],
            "has_more": false});
        let models = parse_anthropic_page(&page);
        assert_eq!(models[0].id, "claude-opus-5-5");
        let mut efforts = models[0].efforts.clone();
        efforts.sort();
        assert_eq!(efforts, ["low", "max"]);
    }

    #[tokio::test]
    async fn command_source_runs_the_cli_and_falls_back_on_failure() {
        let mut spec = crate::config::builtin_runtimes()["hermes"].clone();
        spec.command = "printf".into();
        spec.models_source = ModelsSource::Command { args: vec!["m1\\nm2\\n".into()], format: ModelsFormat::Lines };
        let catalog = Catalog::default();
        let list = catalog.list("t", &spec, false).await;
        assert_eq!(list.source, "cli");
        assert_eq!(list.models.len(), 2);

        spec.command = "definitely-not-a-command".into();
        let list = catalog.list("t2", &spec, false).await;
        assert_eq!(list.source, "static");
        assert!(list.error.is_some());
        assert!(!list.models.is_empty(), "static suggestions are the fallback");
    }
}
