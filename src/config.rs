//! Application configuration: runtimes (how to invoke each agent CLI) and the
//! LLM provider used to generate persona prompts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    #[serde(default)]
    pub prompt_llm: PromptLlmConfig,
    /// Quick orchestration decisions with Jev.
    #[serde(default)]
    pub decisions: crate::decisions::DecisionsConfig,
    /// Runtime definitions keyed by name. Merged over the built-in defaults.
    #[serde(default)]
    pub runtimes: BTreeMap<String, RuntimeSpec>,
    /// Token clients must present (`CHIMERA_TOKEN` overrides it). Without one, a token is only
    /// required, and then generated, when `bind` is not a loopback address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
    /// Extra names the server may be reached by (the `Host` header), e.g. `chimera.lan`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_hosts: Vec<String>,
}

fn default_bind() -> String {
    "127.0.0.1:7878".into()
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("data")
}

/// Which API the persona-prompt generator talks to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LlmApi {
    /// Any OpenAI-compatible `/chat/completions` endpoint (DeepSeek, OpenAI, OpenRouter, Ollama, ...).
    #[default]
    Openai,
    /// Anthropic Messages API.
    Anthropic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PromptLlmConfig {
    #[serde(default)]
    pub api: LlmApi,
    pub base_url: String,
    pub model: String,
    /// Name of the environment variable holding the API key (the key itself is never stored).
    pub api_key_env: String,
    #[serde(default = "default_temperature")]
    pub temperature: f32,
}

fn default_temperature() -> f32 {
    0.7
}

impl Default for PromptLlmConfig {
    fn default() -> Self {
        Self {
            api: LlmApi::Openai,
            base_url: "https://api.deepseek.com".into(),
            model: "deepseek-chat".into(),
            api_key_env: "DEEPSEEK_API_KEY".into(),
            temperature: default_temperature(),
        }
    }
}

/// How an agent runtime is launched. Arguments are templates; supported placeholders:
/// `{model}`, `{effort}`, `{prompt}`, `{workdir}`, `{native_session}`.
///
/// An argument may be a group (a YAML list). A group is dropped entirely when any of its
/// placeholders resolves to an empty string, e.g. `["--model", "{model}"]`.
///
/// If no argument references `{prompt}`, the prompt is written to the process's stdin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSpec {
    #[serde(default)]
    pub label: String,
    /// `process` (spawn a CLI) or `mock` (built-in simulator for dry runs and demos).
    #[serde(default)]
    pub kind: RuntimeKind,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<ArgItem>,
    /// Arguments used instead of `args` when continuing a native CLI session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_args: Option<Vec<ArgItem>>,
    /// How native session ids are obtained (enables `resume_args`).
    #[serde(default)]
    pub session: SessionMode,
    /// How stdout is interpreted.
    #[serde(default)]
    pub output: OutputFormat,
    /// Maps chimera effort levels to runtime-specific values; missing entries pass through,
    /// an empty string drops the effort argument group.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effort_map: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Kill the process after this many seconds.
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
    /// Run under the process guard (OS sandbox that blocks killing/quitting outside programs).
    /// Off for CLIs that apply their own macOS sandbox, which cannot be nested.
    #[serde(default = "default_true")]
    pub guard: bool,
    /// Case-insensitive phrases that mark a failed turn as a usage/rate limit, which switches
    /// the persona to its next fallback (see `Persona::fallbacks`).
    #[serde(default = "default_limit_patterns")]
    pub limit_patterns: Vec<String>,
    /// Where the persona editor gets this runtime's model list from.
    #[serde(default)]
    pub models_source: ModelsSource,
    /// Model suggestions for the UI (also the fallback when `models_source` fails).
    #[serde(default)]
    pub models: Vec<String>,
}

/// Phrases Claude Code, Codex, opencode and pi print when a plan, session or rate limit is hit,
/// e.g. "Usage limit reached · limit resets 3pm", "You've hit your usage limit. Try again in 4 days",
/// "Quota exceeded", "429 Too Many Requests", "Credit balance is too low".
pub const DEFAULT_LIMIT_PATTERNS: &[&str] = &[
    "usage limit",
    "limit reached",
    "hit your limit",
    "out of extra usage",
    "rate limit",
    "rate_limit",
    "quota",
    "too many requests",
    "429",
    "credit balance",
    "out of credits",
    "insufficient credits",
    "insufficient_quota",
];

pub fn default_limit_patterns() -> Vec<String> {
    DEFAULT_LIMIT_PATTERNS.iter().map(|s| s.to_string()).collect()
}

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgItem {
    One(String),
    Group(Vec<String>),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeKind {
    #[default]
    Process,
    Mock,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMode {
    /// Every turn is stateless; chimera replays the persona's history in the prompt.
    #[default]
    None,
    /// Chimera generates a UUID up front and passes it as `{native_session}`.
    Preassign,
    /// The id is captured from the runtime's JSON output (e.g. codex `thread.started`).
    Capture,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    /// Plain text; the whole stdout is the agent's answer.
    #[default]
    Text,
    /// `claude -p --output-format stream-json --verbose`.
    ClaudeStreamJson,
    /// `codex exec --json`.
    CodexJson,
    /// `opencode run --format json`.
    OpencodeJson,
    /// `pi -p --mode json` (JSONL session events).
    PiJson,
    /// `copilot --output-format json` (GitHub Copilot CLI session events, JSONL).
    CopilotJson,
}

/// How to discover the models a runtime can use.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ModelsSource {
    /// Only the `models` suggestions.
    #[default]
    Static,
    /// Run `command` with these arguments and parse its stdout.
    Command {
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        format: ModelsFormat,
    },
    /// Anthropic Models API (Claude Code itself cannot list models).
    AnthropicApi {
        #[serde(default = "default_anthropic_key_env")]
        api_key_env: String,
    },
}

fn default_anthropic_key_env() -> String {
    "ANTHROPIC_API_KEY".into()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelsFormat {
    /// One model id per line.
    #[default]
    Lines,
    /// `codex debug models` JSON.
    CodexCatalog,
    /// `opencode models --verbose`.
    OpencodeVerbose,
    /// `pi --list-models` (table: provider, model, context, max-out, thinking, images).
    PiTable,
    /// The `model` choices listed by `copilot help config`.
    CopilotConfig,
}

fn one(s: &str) -> ArgItem {
    ArgItem::One(s.into())
}

fn group(items: &[&str]) -> ArgItem {
    ArgItem::Group(items.iter().map(|s| s.to_string()).collect())
}

/// Shell commands Claude agents may not run: they can take down programs the user is running
/// (`pkill chrome`, `lsof -ti:PORT | xargs kill`, AppleScript `quit`). Deny rules hold even with
/// `bypassPermissions`, including inside pipes and `&&` chains.
pub const CLAUDE_DENIED_TOOLS: &str =
    "Bash(pkill *),Bash(killall *),Bash(kill *),Bash(xargs kill *),Bash(osascript *),Bash(shutdown *),Bash(reboot *)";

/// The same protection for GitHub Copilot CLI agents (`--deny-tool` wins over `--allow-all-tools`).
const COPILOT_DENIED_TOOLS: &[&str] = &[
    "--deny-tool=shell(pkill)",
    "--deny-tool=shell(killall)",
    "--deny-tool=shell(kill)",
    "--deny-tool=shell(xargs kill)",
    "--deny-tool=shell(osascript)",
    "--deny-tool=shell(shutdown)",
    "--deny-tool=shell(reboot)",
];

/// Built-in runtime definitions. Users can override any of them in `chimera.yaml`.
pub fn builtin_runtimes() -> BTreeMap<String, RuntimeSpec> {
    let mut m = BTreeMap::new();
    m.insert(
        "claude".into(),
        RuntimeSpec {
            label: "Claude Code".into(),
            kind: RuntimeKind::Process,
            command: "claude".into(),
            args: vec![
                one("-p"),
                group(&["--model", "{model}"]),
                group(&["--effort", "{effort}"]),
                group(&["--session-id", "{native_session}"]),
                one("--output-format"),
                one("stream-json"),
                one("--verbose"),
                one("--permission-mode"),
                one("bypassPermissions"),
                // Unattended agents must never drive the user's real browser.
                one("--no-chrome"),
                // ...nor kill or quit programs they did not start (enforced even in bypass mode).
                one("--disallowedTools"),
                one(CLAUDE_DENIED_TOOLS),
            ],
            resume_args: Some(vec![
                one("-p"),
                group(&["--resume", "{native_session}"]),
                group(&["--model", "{model}"]),
                group(&["--effort", "{effort}"]),
                one("--output-format"),
                one("stream-json"),
                one("--verbose"),
                one("--permission-mode"),
                one("bypassPermissions"),
                // Unattended agents must never drive the user's real browser.
                one("--no-chrome"),
                // ...nor kill or quit programs they did not start (enforced even in bypass mode).
                one("--disallowedTools"),
                one(CLAUDE_DENIED_TOOLS),
            ]),
            session: SessionMode::Preassign,
            output: OutputFormat::ClaudeStreamJson,
            effort_map: BTreeMap::from([("minimal".into(), "low".into()), ("ultra".into(), "max".into())]),
            env: BTreeMap::new(),
            timeout_secs: default_timeout(),
            guard: true,
            limit_patterns: default_limit_patterns(),
            models_source: ModelsSource::AnthropicApi { api_key_env: default_anthropic_key_env() },
            models: vec!["opus".into(), "sonnet".into(), "haiku".into(), "fable".into()],
        },
    );
    m.insert(
        "codex".into(),
        RuntimeSpec {
            label: "OpenAI Codex CLI".into(),
            kind: RuntimeKind::Process,
            command: "codex".into(),
            args: vec![
                one("exec"),
                group(&["--model", "{model}"]),
                group(&["-c", "model_reasoning_effort=\"{effort}\""]),
                one("--json"),
                one("--skip-git-repo-check"),
                // Sandboxed: writes confined to the workspace, network allowed for installs/tests.
                one("--sandbox"),
                one("workspace-write"),
                one("-c"),
                one("sandbox_workspace_write.network_access=true"),
                one("-c"),
                one("approval_policy=\"never\""),
                one("-"),
            ],
            resume_args: None,
            session: SessionMode::None,
            output: OutputFormat::CodexJson,
            effort_map: BTreeMap::from([("minimal".into(), "low".into())]),
            env: BTreeMap::new(),
            timeout_secs: default_timeout(),
            // On macOS Codex applies its own seatbelt sandbox (which already blocks outside
            // signals) and seatbelt sandboxes cannot be nested. On Linux nesting works.
            guard: !cfg!(target_os = "macos"),
            limit_patterns: default_limit_patterns(),
            models_source: ModelsSource::Command {
                args: vec!["debug".into(), "models".into()],
                format: ModelsFormat::CodexCatalog,
            },
            models: vec!["gpt-5.6-sol".into(), "gpt-5.5".into()],
        },
    );
    m.insert(
        "opencode".into(),
        RuntimeSpec {
            label: "opencode".into(),
            kind: RuntimeKind::Process,
            command: "opencode".into(),
            args: vec![
                one("run"),
                // Unattended: without --auto, opencode auto-rejects every "ask" permission (e.g.
                // `external_directory` for /tmp) and the agent gets stuck. Explicit denies still apply.
                one("--auto"),
                group(&["--model", "{model}"]),
                group(&["--variant", "{effort}"]),
                one("--format"),
                one("json"),
                one("{prompt}"),
            ],
            resume_args: None,
            session: SessionMode::None,
            output: OutputFormat::OpencodeJson,
            effort_map: BTreeMap::new(),
            env: BTreeMap::new(),
            timeout_secs: default_timeout(),
            guard: true,
            limit_patterns: default_limit_patterns(),
            models_source: ModelsSource::Command {
                args: vec!["models".into(), "--verbose".into()],
                format: ModelsFormat::OpencodeVerbose,
            },
            models: vec![
                "deepseek/deepseek-v4-flash".into(),
                "deepseek/deepseek-v4-pro".into(),
                "openrouter/qwen/qwen3-coder".into(),
            ],
        },
    );
    m.insert(
        "hermes".into(),
        RuntimeSpec {
            label: "Hermes Agent (Nous Research)".into(),
            kind: RuntimeKind::Process,
            command: "hermes".into(),
            args: vec![one("chat"), group(&["--model", "{model}"]), one("-q"), one("{prompt}")],
            resume_args: None,
            session: SessionMode::None,
            output: OutputFormat::Text,
            effort_map: BTreeMap::new(),
            env: BTreeMap::new(),
            timeout_secs: default_timeout(),
            guard: true,
            limit_patterns: default_limit_patterns(),
            models_source: ModelsSource::Static,
            models: vec!["anthropic/claude-opus-5-5".into(), "nousresearch/hermes-4-405b".into()],
        },
    );
    m.insert(
        "copilot".into(),
        RuntimeSpec {
            label: "GitHub Copilot CLI".into(),
            kind: RuntimeKind::Process,
            command: "copilot".into(),
            // Prompt on stdin (non-interactive run). The same --session-id creates the session on
            // the first turn and continues it afterwards.
            args: [
                vec![
                    group(&["--model", "{model}"]),
                    group(&["--reasoning-effort", "{effort}"]),
                    group(&["--session-id", "{native_session}"]),
                    one("--output-format"),
                    one("json"),
                    // Unattended: approve tool use, never stop to ask the user a question.
                    one("--allow-all-tools"),
                    one("--no-ask-user"),
                    one("--no-auto-update"),
                    one("--no-color"),
                ],
                COPILOT_DENIED_TOOLS.iter().map(|d| one(d)).collect(),
            ]
            .concat(),
            resume_args: None,
            session: SessionMode::Preassign,
            output: OutputFormat::CopilotJson,
            effort_map: BTreeMap::from([("ultra".into(), "max".into())]),
            env: BTreeMap::new(),
            timeout_secs: default_timeout(),
            guard: true,
            limit_patterns: default_limit_patterns(),
            models_source: ModelsSource::Command {
                args: vec!["help".into(), "config".into()],
                format: ModelsFormat::CopilotConfig,
            },
            models: vec![
                "claude-sonnet-5".into(),
                "gpt-5.6-luna".into(),
                "gpt-5.4".into(),
                "gpt-5.4-mini".into(),
                "gpt-5.3-codex".into(),
                "gpt-5-mini".into(),
            ],
        },
    );
    m.insert(
        "pi".into(),
        RuntimeSpec {
            label: "pi coding agent".into(),
            kind: RuntimeKind::Process,
            command: "pi".into(),
            // Prompt on stdin. The same --session-id creates the session on the first turn and
            // continues it afterwards, so no separate resume arguments are needed.
            args: vec![
                one("-p"),
                one("--mode"),
                one("json"),
                group(&["--model", "{model}"]),
                group(&["--thinking", "{effort}"]),
                group(&["--session-id", "{native_session}"]),
            ],
            resume_args: None,
            session: SessionMode::Preassign,
            output: OutputFormat::PiJson,
            effort_map: BTreeMap::from([("ultra".into(), "max".into())]),
            env: BTreeMap::new(),
            timeout_secs: default_timeout(),
            guard: true,
            limit_patterns: default_limit_patterns(),
            models_source: ModelsSource::Command { args: vec!["--list-models".into()], format: ModelsFormat::PiTable },
            models: vec!["anthropic/claude-sonnet-5-5".into(), "openai/gpt-5.5".into()],
        },
    );
    m.insert(
        "mock".into(),
        RuntimeSpec {
            label: "Mock (dry run, no LLM calls)".into(),
            kind: RuntimeKind::Mock,
            command: String::new(),
            args: vec![],
            resume_args: None,
            session: SessionMode::None,
            output: OutputFormat::Text,
            effort_map: BTreeMap::new(),
            env: BTreeMap::new(),
            timeout_secs: 60,
            guard: false,
            limit_patterns: default_limit_patterns(),
            models_source: ModelsSource::Static,
            models: vec!["mock".into()],
        },
    );
    m
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            data_dir: default_data_dir(),
            prompt_llm: PromptLlmConfig::default(),
            decisions: Default::default(),
            runtimes: builtin_runtimes(),
            auth_token: None,
            allowed_hosts: vec![],
        }
    }
}

impl Config {
    /// Loads `path` if it exists and merges its runtimes over the built-in ones.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut cfg = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                Self::parse(&text).with_context(|| format!("parsing {}", p.display()))?
            }
            Some(p) => anyhow::bail!("config file {} not found", p.display()),
            None => Config::default(),
        };
        if cfg.runtimes.is_empty() {
            cfg.runtimes = builtin_runtimes();
        }
        Ok(cfg)
    }

    /// Parses a config file. A runtime with a built-in name is merged key by key over the
    /// built-in definition: `claude: {models: [...]}` changes only the model list and keeps the
    /// built-in arguments (including `--no-chrome` and the deny list). A key that is given
    /// replaces the built-in value as a whole, e.g. `args` replaces all arguments.
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        use serde_yaml::Value;
        let mut doc: Value = serde_yaml::from_str(text)?;
        if doc.is_null() {
            doc = Value::Mapping(Default::default());
        }
        let user_runtimes = doc.as_mapping_mut().and_then(|m| m.remove("runtimes"));
        let mut cfg: Config = serde_yaml::from_value(doc)?;
        let mut runtimes = builtin_runtimes();
        if let Some(Value::Mapping(user)) = user_runtimes {
            for (name, spec) in user {
                let name = name.as_str().context("runtime names must be strings")?.to_string();
                let merged = match (runtimes.get(&name), spec) {
                    (Some(builtin), Value::Mapping(over)) => {
                        let mut base = serde_yaml::to_value(builtin)?;
                        let base_map = base.as_mapping_mut().context("built-in runtime is not a mapping")?;
                        for (k, v) in over {
                            base_map.insert(k, v);
                        }
                        base
                    }
                    (_, spec) => spec,
                };
                let spec: RuntimeSpec = serde_yaml::from_value(merged).with_context(|| format!("runtime `{name}`"))?;
                runtimes.insert(name, spec);
            }
        }
        cfg.runtimes = runtimes;
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let cfg: Config = serde_yaml::from_str(include_str!("../chimera.example.yaml")).unwrap();
        assert_eq!(cfg.decisions.model, "jev-latest");
        assert!(cfg.runtimes.contains_key("hermes"));
    }

    /// Overriding one key of a built-in runtime keeps the rest, including the security flags.
    #[test]
    fn user_runtimes_merge_over_builtins() {
        let cfg = Config::parse(
            "runtimes:\n  claude:\n    models: [opus]\n    timeout_secs: 60\n  extra:\n    command: aider\n    args: [--yes]\n",
        )
        .unwrap();
        let claude = &cfg.runtimes["claude"];
        assert_eq!(claude.models, vec!["opus".to_string()]);
        assert_eq!(claude.timeout_secs, 60);
        assert_eq!(claude.command, "claude", "untouched keys come from the built-in");
        let args = format!("{:?}", claude.args);
        assert!(args.contains("--no-chrome") && args.contains("--disallowedTools"), "{args}");
        assert!(claude.resume_args.is_some());
        assert_eq!(cfg.runtimes["extra"].command, "aider");
        assert!(cfg.runtimes.contains_key("codex"));
        // An empty file is a valid config.
        assert!(Config::parse("").unwrap().runtimes.contains_key("claude"));
        // So is a partial prompt_llm section.
        let partial = Config::parse("prompt_llm:\n  model: deepseek-reasoner\n").unwrap();
        assert_eq!(partial.prompt_llm.model, "deepseek-reasoner");
        assert_eq!(partial.prompt_llm.api_key_env, "DEEPSEEK_API_KEY");
    }

    #[test]
    fn user_runtimes_override_builtins() {
        let dir = std::env::temp_dir().join(format!("chimera-cfg-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chimera.yaml");
        std::fs::write(
            &path,
            "runtimes:\n  hermes:\n    command: /opt/hermes\n    args: [chat, [\"--model\", \"{model}\"], \"{prompt}\"]\n",
        )
        .unwrap();
        let cfg = Config::load(Some(&path)).unwrap();
        assert_eq!(cfg.runtimes["hermes"].command, "/opt/hermes");
        assert!(cfg.runtimes.contains_key("claude"));
        assert_eq!(cfg.prompt_llm.model, "deepseek-chat");
    }
}
