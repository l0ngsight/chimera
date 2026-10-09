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
    /// Where the persona editor gets this runtime's model list from.
    #[serde(default)]
    pub models_source: ModelsSource,
    /// Model suggestions for the UI (also the fallback when `models_source` fails).
    #[serde(default)]
    pub models: Vec<String>,
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
            // Codex sandboxes its own commands (seatbelt on macOS); sandboxes cannot be nested.
            guard: false,
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
            models_source: ModelsSource::Static,
            models: vec!["anthropic/claude-opus-5-5".into(), "nousresearch/hermes-4-405b".into()],
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
        }
    }
}

impl Config {
    /// Loads `path` if it exists and merges its runtimes over the built-in ones.
    pub fn load(path: Option<&Path>) -> anyhow::Result<Self> {
        let mut cfg = match path {
            Some(p) if p.exists() => {
                let text = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
                let mut user: Config =
                    serde_yaml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
                let mut runtimes = builtin_runtimes();
                runtimes.append(&mut user.runtimes);
                user.runtimes = runtimes;
                user
            }
            Some(p) => anyhow::bail!("config file {} not found", p.display()),
            None => Config::default(),
        };
        if cfg.runtimes.is_empty() {
            cfg.runtimes = builtin_runtimes();
        }
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
