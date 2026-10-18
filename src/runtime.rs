//! Launching agent CLIs and turning their output into log lines and a final answer.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};

use crate::config::{ArgItem, OutputFormat, RuntimeSpec, SessionMode};
use crate::model::Persona;
use crate::session::LogKind;

/// One line for the persona's live log.
#[derive(Debug, Clone)]
pub struct LogLine {
    pub kind: LogKind,
    pub text: String,
}

impl LogLine {
    fn new(kind: LogKind, text: impl Into<String>) -> Self {
        Self { kind, text: text.into() }
    }
}

#[derive(Debug, Default)]
pub struct TurnOutput {
    /// The agent's final textual answer for this turn.
    pub text: String,
    /// Native session id to resume next turn, if the runtime supports it.
    pub native_session: Option<String>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
}

/// Inputs for one invocation of a process runtime.
pub struct Invocation<'a> {
    pub spec: &'a RuntimeSpec,
    pub persona: &'a Persona,
    pub prompt: &'a str,
    pub workdir: &'a Path,
    /// Native session to resume (`Some`) or to create (preassigned id for a fresh session).
    pub native_session: Option<&'a str>,
    pub resume: bool,
    /// Process-guard sandbox to run the CLI in, when the guard is active.
    pub wrapper: Option<crate::guard::Backend>,
}

/// Expands the argument templates for an invocation.
pub fn build_args(inv: &Invocation<'_>) -> (Vec<String>, bool) {
    let spec = inv.spec;
    let effort = inv.persona.effort.as_str();
    let effort = spec.effort_map.get(effort).map(String::as_str).unwrap_or(effort);
    let mut vars: BTreeMap<&str, &str> = BTreeMap::new();
    vars.insert("model", inv.persona.model.as_str());
    vars.insert("effort", effort);
    vars.insert("prompt", inv.prompt);
    let workdir = inv.workdir.to_string_lossy();
    vars.insert("workdir", &workdir);
    vars.insert("native_session", inv.native_session.unwrap_or(""));

    let template = match (&spec.resume_args, inv.resume) {
        (Some(r), true) => r,
        _ => &spec.args,
    };
    let mut prompt_in_args = false;
    let mut out = Vec::new();
    for item in template {
        let parts: Vec<&String> = match item {
            ArgItem::One(s) => vec![s],
            ArgItem::Group(g) => g.iter().collect(),
        };
        let mut expanded = Vec::with_capacity(parts.len());
        let mut drop_group = false;
        for p in parts {
            if p.contains("{prompt}") {
                prompt_in_args = true;
            }
            let (s, had_empty) = substitute(p, &vars);
            if had_empty && matches!(item, ArgItem::Group(_)) {
                drop_group = true;
            }
            expanded.push(s);
        }
        if !drop_group {
            out.extend(expanded);
        }
    }
    out.extend(inv.persona.extra_args.iter().cloned());
    (out, prompt_in_args)
}

/// Replaces `{name}` placeholders. Returns whether any placeholder expanded to "".
fn substitute(template: &str, vars: &BTreeMap<&str, &str>) -> (String, bool) {
    let mut out = String::with_capacity(template.len());
    let mut had_empty = false;
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else { break };
        let name = &rest[start + 1..start + len];
        if let Some(v) = vars.get(name) {
            out.push_str(&rest[..start]);
            out.push_str(v);
            had_empty |= v.is_empty();
        } else {
            out.push_str(&rest[..start + len + 1]);
        }
        rest = &rest[start + len + 1..];
    }
    out.push_str(rest);
    (out, had_empty)
}

/// Human-readable command line for the log (prompt elided).
pub fn describe_command(command: &str, args: &[String], prompt: &str) -> String {
    let shown: Vec<String> = args
        .iter()
        .map(|a| if !prompt.is_empty() && a == prompt { "<prompt>".to_string() } else { a.clone() })
        .map(|a| shlex::try_quote(&a).map(|c| c.into_owned()).unwrap_or(a))
        .collect();
    format!("{command} {}", shown.join(" "))
}

/// Runs a process runtime to completion, streaming log lines through `log`.
pub async fn run_process(
    inv: Invocation<'_>,
    log: mpsc::UnboundedSender<LogLine>,
    mut cancel: watch::Receiver<bool>,
) -> TurnOutput {
    let spec = inv.spec;
    let (args, prompt_in_args) = build_args(&inv);
    let mut shown = describe_command(&spec.command, &args, inv.prompt);
    // Under the process guard the CLI runs inside an OS sandbox: `<wrapper> <command> <args…>`.
    let guard = inv.wrapper.as_ref().filter(|_| spec.guard);
    let (program, args) = match guard {
        Some(b) => {
            shown = format!("[process guard: {}] {shown}", b.name);
            (b.program.clone(), b.args.iter().cloned().chain([spec.command.clone()]).chain(args).collect())
        }
        None => (spec.command.clone(), args),
    };
    let _ = log.send(LogLine::new(LogKind::Command, shown));

    let mut cmd = Command::new(&program);
    cmd.args(&args)
        .current_dir(inv.workdir)
        .stdin(if prompt_in_args { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    own_process_group(&mut cmd);
    for var in guard.map(|b| b.hidden_env).unwrap_or_default() {
        cmd.env_remove(var);
    }
    for (k, v) in spec.env.iter().chain(inv.persona.env.iter()) {
        cmd.env(k, v);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let msg = format!("failed to start `{}`: {e}", spec.command);
            let _ = log.send(LogLine::new(LogKind::Error, msg.clone()));
            return TurnOutput { error: Some(msg), ..Default::default() };
        }
    };

    if !prompt_in_args && let Some(mut stdin) = child.stdin.take() {
        let prompt = inv.prompt.to_owned();
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
            let _ = stdin.shutdown().await;
        });
    }

    let stderr = child.stderr.take().expect("stderr piped");
    let stderr_log = log.clone();
    let stderr_task = tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        let mut tail = Vec::new();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            let _ = stderr_log.send(LogLine::new(LogKind::Stderr, line.clone()));
            tail.push(line);
            if tail.len() > 20 {
                tail.remove(0);
            }
        }
        tail.join("\n")
    });

    let stdout = child.stdout.take().expect("stdout piped");
    let mut parser = OutputParser::new(spec.output);
    let mut lines = BufReader::new(stdout).lines();
    let timeout = tokio::time::sleep(Duration::from_secs(spec.timeout_secs.max(1)));
    tokio::pin!(timeout);

    let pgid = child.id();
    let mut aborted: Option<String> = None;
    let mut status = None;
    let mut stdout_open = true;
    // The turn ends when the CLI exits. Processes it left running in the background (a dev
    // server, a watcher) may keep stdout open forever, so we do not wait for end-of-file.
    while status.is_none() {
        tokio::select! {
            line = lines.next_line(), if stdout_open => match line {
                Ok(Some(line)) => {
                    for l in parser.feed(&line) {
                        let _ = log.send(l);
                    }
                }
                _ => stdout_open = false,
            },
            st = child.wait() => status = Some(st.ok()),
            _ = &mut timeout => {
                aborted = Some(format!("timed out after {}s", spec.timeout_secs));
                break;
            }
            _ = cancel.changed() => {
                if *cancel.borrow() {
                    aborted = Some("cancelled".into());
                    break;
                }
            }
        }
    }
    if let Some(reason) = &aborted {
        kill_process_group(pgid);
        let _ = child.start_kill();
        let _ = log.send(LogLine::new(LogKind::Error, format!("process {reason}")));
    }
    // Read what the CLI wrote just before exiting. A process left in the background can keep
    // the pipe open, so read briefly, stop the turn's whole process group (outside the process
    // guard, which already does this), then read the rest (`next_line` is cancellation safe).
    for (round, wait) in [Duration::from_secs(2), Duration::from_secs(2)].into_iter().enumerate() {
        if round == 1 {
            kill_process_group(pgid);
        }
        let until = tokio::time::Instant::now() + wait;
        while stdout_open {
            match tokio::time::timeout_at(until, lines.next_line()).await {
                Ok(Ok(Some(line))) => {
                    for l in parser.feed(&line) {
                        let _ = log.send(l);
                    }
                }
                Ok(_) => stdout_open = false,
                Err(_) => break,
            }
        }
    }
    if stdout_open {
        kill_process_group(pgid);
    }
    let status = match status {
        Some(s) => s,
        None => tokio::time::timeout(Duration::from_secs(5), child.wait()).await.ok().and_then(Result::ok),
    };
    let stderr_tail =
        tokio::time::timeout(Duration::from_secs(2), stderr_task).await.ok().and_then(Result::ok).unwrap_or_default();
    let exit_code = status.and_then(|s| s.code());

    let mut out = parser.finish();
    out.exit_code = exit_code;
    if out.native_session.is_none() && spec.session == SessionMode::Preassign {
        out.native_session = inv.native_session.map(str::to_owned);
    }
    if let Some(reason) = aborted {
        out.error = Some(reason);
    } else if !status.map(|s| s.success()).unwrap_or(false) && out.error.is_none() {
        let mut msg = format!("`{}` exited with {:?}", spec.command, exit_code);
        if !stderr_tail.is_empty() {
            msg.push_str(": ");
            msg.push_str(&truncate(&stderr_tail, 1500));
        }
        out.error = Some(msg);
    }
    out
}

/// Starts the agent in a process group of its own, so the whole group can be stopped at the end
/// of the turn and a Ctrl+C in Chimera's terminal reaches Chimera only (which then stops the
/// agents itself). On Linux the agent is also killed if Chimera dies without cleaning up.
fn own_process_group(cmd: &mut Command) {
    #[cfg(unix)]
    {
        cmd.process_group(0);
        #[cfg(target_os = "linux")]
        // SAFETY: prctl is async-signal-safe and touches only the calling (child) process.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = cmd;
}

/// Kills the process group led by the agent CLI (the agent and anything it left running).
fn kill_process_group(pgid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pgid) = pgid.and_then(|p| libc::pid_t::try_from(p).ok()).filter(|p| *p > 1) {
        // SAFETY: plain syscall; a group that no longer exists just returns ESRCH.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pgid;
}

/// Incremental parser for a runtime's stdout.
pub struct OutputParser {
    format: OutputFormat,
    text: String,
    last_message: String,
    native_session: Option<String>,
    error: Option<String>,
    /// Accumulated cost reported by the runtime (USD), when it reports one per message.
    cost: f64,
}

impl OutputParser {
    pub fn new(format: OutputFormat) -> Self {
        Self { format, text: String::new(), last_message: String::new(), native_session: None, error: None, cost: 0.0 }
    }

    pub fn feed(&mut self, line: &str) -> Vec<LogLine> {
        if self.format == OutputFormat::Text {
            self.text.push_str(line);
            self.text.push('\n');
            return vec![LogLine::new(LogKind::Text, line)];
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            if line.trim().is_empty() {
                return vec![];
            }
            return vec![LogLine::new(LogKind::Text, line)];
        };
        match self.format {
            OutputFormat::ClaudeStreamJson => self.feed_claude(&v),
            OutputFormat::CodexJson => self.feed_codex(&v),
            OutputFormat::OpencodeJson => self.feed_opencode(&v),
            OutputFormat::PiJson => self.feed_pi(&v),
            OutputFormat::CopilotJson => self.feed_copilot(&v),
            OutputFormat::Text => unreachable!(),
        }
    }

    pub fn finish(self) -> TurnOutput {
        let text = match self.format {
            OutputFormat::Text => self.text.trim().to_string(),
            OutputFormat::OpencodeJson => self.text.trim().to_string(),
            _ if !self.text.trim().is_empty() => self.text.trim().to_string(),
            _ => self.last_message.trim().to_string(),
        };
        TurnOutput { text, native_session: self.native_session, exit_code: None, error: self.error }
    }

    fn feed_claude(&mut self, v: &Value) -> Vec<LogLine> {
        let mut out = vec![];
        match v["type"].as_str().unwrap_or_default() {
            "system" if v["subtype"] == "init" => {
                if let Some(id) = v["session_id"].as_str() {
                    self.native_session = Some(id.to_string());
                }
                out.push(LogLine::new(
                    LogKind::Info,
                    format!("session started (model: {})", v["model"].as_str().unwrap_or("?")),
                ));
            }
            "assistant" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str().unwrap_or_default() {
                        "text" => {
                            let t = c["text"].as_str().unwrap_or_default();
                            self.last_message = t.to_string();
                            out.push(LogLine::new(LogKind::Text, t));
                        }
                        "thinking" => {
                            let t = c["thinking"].as_str().unwrap_or_default();
                            if !t.is_empty() {
                                out.push(LogLine::new(LogKind::Thinking, t));
                            }
                        }
                        "tool_use" => out.push(LogLine::new(
                            LogKind::Tool,
                            format!("{} {}", c["name"].as_str().unwrap_or("tool"), summarize_json(&c["input"])),
                        )),
                        _ => {}
                    }
                }
            }
            "user" => {
                for c in v["message"]["content"].as_array().into_iter().flatten() {
                    if c["type"] == "tool_result" {
                        let body = match &c["content"] {
                            Value::String(s) => s.clone(),
                            Value::Array(parts) => {
                                parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
                            }
                            other => other.to_string(),
                        };
                        let kind = if c["is_error"] == true { LogKind::Error } else { LogKind::ToolResult };
                        out.push(LogLine::new(kind, truncate(&body, 2000)));
                    }
                }
            }
            "result" => {
                if let Some(r) = v["result"].as_str() {
                    self.text = r.to_string();
                }
                if v["is_error"] == true {
                    self.error = Some(v["result"].as_str().unwrap_or("claude reported an error").to_string());
                }
                let cost = v["total_cost_usd"].as_f64().map(|c| format!(", cost ${c:.4}")).unwrap_or_default();
                let turns = v["num_turns"].as_u64().map(|n| format!(", {n} turns")).unwrap_or_default();
                out.push(LogLine::new(
                    LogKind::Info,
                    format!("finished: {}{turns}{cost}", v["subtype"].as_str().unwrap_or("done")),
                ));
            }
            _ => {}
        }
        out
    }

    fn feed_codex(&mut self, v: &Value) -> Vec<LogLine> {
        let mut out = vec![];
        match v["type"].as_str().unwrap_or_default() {
            "thread.started" => {
                if let Some(id) = v["thread_id"].as_str() {
                    self.native_session = Some(id.to_string());
                }
                out.push(LogLine::new(LogKind::Info, "thread started"));
            }
            "item.started" => {
                let item = &v["item"];
                if item["type"] == "command_execution" {
                    out.push(LogLine::new(LogKind::Tool, format!("$ {}", item["command"].as_str().unwrap_or(""))));
                }
            }
            "item.completed" => {
                let item = &v["item"];
                match item["type"].as_str().unwrap_or_default() {
                    "agent_message" => {
                        let t = item["text"].as_str().unwrap_or_default();
                        self.last_message = t.to_string();
                        out.push(LogLine::new(LogKind::Text, t));
                    }
                    "reasoning" => out.push(LogLine::new(LogKind::Thinking, item["text"].as_str().unwrap_or_default())),
                    "command_execution" => out.push(LogLine::new(
                        LogKind::ToolResult,
                        format!(
                            "exit {}: {}",
                            item["exit_code"],
                            truncate(item["aggregated_output"].as_str().unwrap_or_default(), 2000)
                        ),
                    )),
                    "file_change" => {
                        let files: Vec<String> = item["changes"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|c| {
                                format!("{} {}", c["kind"].as_str().unwrap_or("edit"), c["path"].as_str().unwrap_or(""))
                            })
                            .collect();
                        out.push(LogLine::new(LogKind::Tool, format!("files: {}", files.join(", "))));
                    }
                    "error" => out.push(LogLine::new(LogKind::Error, item["message"].as_str().unwrap_or_default())),
                    other => out.push(LogLine::new(LogKind::Tool, format!("{other} {}", summarize_json(item)))),
                }
            }
            "turn.completed" => {
                out.push(LogLine::new(LogKind::Info, format!("turn completed {}", summarize_json(&v["usage"]))))
            }
            "turn.failed" | "error" => {
                let msg = v["error"]["message"].as_str().or(v["message"].as_str()).unwrap_or("codex error");
                self.error = Some(msg.to_string());
                out.push(LogLine::new(LogKind::Error, msg));
            }
            _ => {}
        }
        out
    }

    /// pi `--mode json`: completed messages arrive in `message_end`, tool runs in
    /// `tool_execution_*`; streaming `message_update` deltas are skipped.
    fn feed_pi(&mut self, v: &Value) -> Vec<LogLine> {
        let mut out = vec![];
        match v["type"].as_str().unwrap_or_default() {
            "session" => {
                if let Some(id) = v["id"].as_str() {
                    self.native_session = Some(id.to_string());
                }
                out.push(LogLine::new(LogKind::Info, "session started"));
            }
            "message_end" if v["message"]["role"] == "assistant" => {
                let m = &v["message"];
                let mut text = vec![];
                for c in m["content"].as_array().into_iter().flatten() {
                    match c["type"].as_str().unwrap_or_default() {
                        "text" => text.push(c["text"].as_str().unwrap_or_default().to_string()),
                        "thinking" => {
                            let t = c["thinking"].as_str().unwrap_or_default();
                            if !t.is_empty() {
                                out.push(LogLine::new(LogKind::Thinking, t));
                            }
                        }
                        _ => {}
                    }
                }
                let text = text.join("\n");
                if !text.trim().is_empty() {
                    out.push(LogLine::new(LogKind::Text, text.clone()));
                    self.last_message = text;
                }
                self.cost += m["usage"]["cost"]["total"].as_f64().unwrap_or(0.0);
                if m["stopReason"] == "error" {
                    let msg = m["errorMessage"].as_str().unwrap_or("pi reported an error").to_string();
                    out.push(LogLine::new(LogKind::Error, msg.clone()));
                    self.error = Some(msg);
                }
            }
            "tool_execution_start" => out.push(LogLine::new(
                LogKind::Tool,
                format!("{} {}", v["toolName"].as_str().unwrap_or("tool"), summarize_json(&v["args"])),
            )),
            "tool_execution_end" => {
                let body: Vec<&str> = v["result"]["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .collect();
                let kind = if v["isError"] == true { LogKind::Error } else { LogKind::ToolResult };
                out.push(LogLine::new(kind, truncate(&body.join("\n"), 2000)));
            }
            "auto_retry_start" => out.push(LogLine::new(
                LogKind::Info,
                format!("retrying ({})", v["errorMessage"].as_str().unwrap_or("provider error")),
            )),
            "auto_retry_end" if v["success"] == false => {
                let msg = v["finalError"].as_str().unwrap_or("pi gave up after retries").to_string();
                out.push(LogLine::new(LogKind::Error, msg.clone()));
                self.error = Some(msg);
            }
            "agent_settled" => {
                let cost = if self.cost > 0.0 { format!(", cost ${:.4}", self.cost) } else { String::new() };
                let how = if v["aborted"] == true { "aborted" } else { "finished" };
                out.push(LogLine::new(LogKind::Info, format!("{how}{cost}")));
            }
            _ => {}
        }
        out
    }

    /// GitHub Copilot CLI `--output-format json`: one session event per line (`{type, data, …}`,
    /// see `session-events.schema.json` in `@github/copilot-sdk`). Streaming deltas are skipped.
    /// Every top-level assistant message is part of the reply, so `<message>` blocks written
    /// before a closing remark are not lost.
    fn feed_copilot(&mut self, v: &Value) -> Vec<LogLine> {
        let mut out = vec![];
        let d = &v["data"];
        let nested = d["parentToolCallId"].is_string(); // a subagent's events
        match v["type"].as_str().unwrap_or_default() {
            "session.start" => {
                if let Some(id) = d["sessionId"].as_str() {
                    self.native_session = Some(id.to_string());
                }
                out.push(LogLine::new(LogKind::Info, "session started"));
            }
            "assistant.message" if !nested => {
                let t = d["content"].as_str().unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(LogLine::new(LogKind::Text, t));
                    if !self.text.is_empty() {
                        self.text.push_str("\n\n");
                    }
                    self.text.push_str(t);
                    self.last_message = t.to_string();
                }
            }
            "assistant.reasoning" if !nested => {
                let t = d["content"].as_str().unwrap_or_default();
                if !t.trim().is_empty() {
                    out.push(LogLine::new(LogKind::Thinking, t));
                }
            }
            "tool.execution_start" => {
                let name = d["toolTitle"].as_str().or(d["toolName"].as_str()).unwrap_or("tool");
                out.push(LogLine::new(LogKind::Tool, format!("{name} {}", summarize_json(&d["arguments"]))));
            }
            "tool.execution_complete" => {
                let (kind, body) = if d["success"] == false {
                    (LogKind::Error, d["error"]["message"].as_str().unwrap_or("tool failed"))
                } else {
                    (LogKind::ToolResult, d["result"]["content"].as_str().unwrap_or_default())
                };
                if !body.trim().is_empty() {
                    out.push(LogLine::new(kind, truncate(body, 2000)));
                }
            }
            "assistant.usage" => self.cost += d["cost"].as_f64().unwrap_or(0.0),
            "model.call_failure" => out.push(LogLine::new(
                LogKind::Info,
                format!("model call failed ({}), retrying", d["errorMessage"].as_str().unwrap_or("provider error")),
            )),
            "session.warning" => out.push(LogLine::new(LogKind::Info, d["message"].as_str().unwrap_or_default())),
            // e.g. errorType "rate_limit" / errorCode "user_weekly_rate_limited", or "quota".
            "session.error" => {
                let kind = [d["errorType"].as_str(), d["errorCode"].as_str()].into_iter().flatten().collect::<Vec<_>>();
                let msg = d["message"].as_str().unwrap_or("copilot reported an error");
                let msg = if kind.is_empty() { msg.to_string() } else { format!("{msg} [{}]", kind.join("/")) };
                out.push(LogLine::new(LogKind::Error, msg.clone()));
                self.error = Some(msg);
            }
            _ => {}
        }
        out
    }

    fn feed_opencode(&mut self, v: &Value) -> Vec<LogLine> {
        let part = &v["part"];
        match v["type"].as_str().unwrap_or_default() {
            "text" => {
                let t = part["text"].as_str().unwrap_or_default();
                self.text.push_str(t);
                self.text.push('\n');
                vec![LogLine::new(LogKind::Text, t)]
            }
            "reasoning" => vec![LogLine::new(LogKind::Thinking, part["text"].as_str().unwrap_or_default())],
            "tool_use" => {
                let state = &part["state"];
                let mut lines = vec![LogLine::new(
                    LogKind::Tool,
                    format!("{} {}", part["tool"].as_str().unwrap_or("tool"), summarize_json(&state["input"])),
                )];
                if let Some(o) = state["output"].as_str() {
                    lines.push(LogLine::new(LogKind::ToolResult, truncate(o, 2000)));
                }
                lines
            }
            "error" => {
                let msg = v["error"]["data"]["message"]
                    .as_str()
                    .or(v["error"]["message"].as_str())
                    .map(str::to_owned)
                    .unwrap_or_else(|| v["error"].to_string());
                self.error = Some(msg.clone());
                vec![LogLine::new(LogKind::Error, msg)]
            }
            "step_start" | "step_finish" => vec![],
            _ => vec![LogLine::new(LogKind::Info, summarize_json(v))],
        }
    }
}

fn summarize_json(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Object(map) => {
            // Prefer the most telling field of common tool inputs.
            for key in ["command", "file_path", "path", "pattern", "url", "query", "description"] {
                if let Some(s) = map.get(key).and_then(Value::as_str) {
                    return truncate(s, 300);
                }
            }
            truncate(&v.to_string(), 300)
        }
        _ => truncate(&v.to_string(), 300),
    }
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}… [truncated]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::builtin_runtimes;
    use crate::model::Effort;

    fn persona(model: &str, effort: Effort) -> Persona {
        Persona {
            id: "p".into(),
            name: "P".into(),
            role: String::new(),
            runtime: "claude".into(),
            model: model.into(),
            effort,
            prompt: String::new(),
            extra_args: vec!["--foo".into()],
            env: Default::default(),
            fallbacks: vec![],
            position: None,
        }
    }

    #[test]
    fn agents_are_kept_away_from_the_users_programs() {
        let rts = builtin_runtimes();
        let p = persona("opus", Effort::High);
        for resume in [false, true] {
            let inv = Invocation {
                spec: &rts["claude"],
                persona: &p,
                prompt: "hi",
                workdir: Path::new("/tmp"),
                native_session: Some("s"),
                resume,
                wrapper: None,
            };
            let (args, _) = build_args(&inv);
            assert!(args.contains(&"--no-chrome".to_string()), "resume={resume}");
            let denied = args.windows(2).find(|w| w[0] == "--disallowedTools").map(|w| w[1].clone());
            assert!(denied.is_some_and(|d| d.contains("Bash(pkill *)") && d.contains("Bash(xargs kill *)")));
        }
        let inv = Invocation {
            spec: &rts["codex"],
            persona: &p,
            prompt: "hi",
            workdir: Path::new("/tmp"),
            native_session: None,
            resume: false,
            wrapper: None,
        };
        let (args, _) = build_args(&inv);
        assert!(args.windows(2).any(|w| w == ["--sandbox", "workspace-write"]));
        assert!(!args.iter().any(|a| a.contains("dangerously")));
        let p = persona("gpt-5.3-codex", Effort::Ultra);
        let inv = Invocation {
            spec: &rts["copilot"],
            persona: &p,
            prompt: "hi",
            workdir: Path::new("/tmp"),
            native_session: Some("0cb916db-26aa-40f2-86b5-1ba81b225fd2"),
            resume: true,
            wrapper: None,
        };
        let (args, prompt_in_args) = build_args(&inv);
        assert!(!prompt_in_args, "the prompt goes to stdin");
        assert!(args.windows(2).any(|w| w == ["--model", "gpt-5.3-codex"]));
        assert!(args.windows(2).any(|w| w == ["--reasoning-effort", "max"]));
        assert!(args.windows(2).any(|w| w == ["--session-id", "0cb916db-26aa-40f2-86b5-1ba81b225fd2"]));
        assert!(args.contains(&"--deny-tool=shell(pkill)".to_string()));
        assert!(args.contains(&"--no-ask-user".to_string()));
    }

    #[test]
    fn groups_with_empty_placeholders_are_dropped() {
        let rts = builtin_runtimes();
        let p = persona("", Effort::High);
        let inv = Invocation {
            spec: &rts["claude"],
            persona: &p,
            prompt: "hi",
            workdir: Path::new("/tmp"),
            native_session: None,
            resume: false,
            wrapper: None,
        };
        let (args, in_args) = build_args(&inv);
        assert!(!in_args);
        assert!(!args.contains(&"--model".to_string()));
        assert!(!args.contains(&"--session-id".to_string()));
        assert!(args.windows(2).any(|w| w == ["--effort", "high"]));
        assert_eq!(args.last().unwrap(), "--foo");
    }

    #[test]
    fn resume_args_and_effort_map() {
        let rts = builtin_runtimes();
        let p = persona("opus", Effort::Minimal);
        let inv = Invocation {
            spec: &rts["claude"],
            persona: &p,
            prompt: "hi",
            workdir: Path::new("/tmp"),
            native_session: Some("abc"),
            resume: true,
            wrapper: None,
        };
        let (args, _) = build_args(&inv);
        assert!(args.windows(2).any(|w| w == ["--resume", "abc"]));
        assert!(args.windows(2).any(|w| w == ["--effort", "low"]));
    }

    #[test]
    fn prompt_placeholder_goes_in_args() {
        let rts = builtin_runtimes();
        let p = persona("deepseek/deepseek-v4-flash", Effort::Medium);
        let inv = Invocation {
            spec: &rts["opencode"],
            persona: &p,
            prompt: "do it",
            workdir: Path::new("/tmp"),
            native_session: None,
            resume: false,
            wrapper: None,
        };
        let (args, in_args) = build_args(&inv);
        assert!(in_args);
        assert!(args.contains(&"do it".to_string()));
        assert!(args.contains(&"--auto".to_string()), "opencode must not auto-reject permissions");
        assert!(describe_command("opencode", &args, "do it").contains("<prompt>"));
    }

    #[test]
    fn parses_claude_stream_json() {
        let mut p = OutputParser::new(OutputFormat::ClaudeStreamJson);
        p.feed(r#"{"type":"system","subtype":"init","session_id":"s1","model":"opus"}"#);
        let l = p.feed(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#);
        assert_eq!(l[0].text, "Bash ls");
        p.feed(r#"{"type":"result","subtype":"success","result":"all done","is_error":false}"#);
        let out = p.finish();
        assert_eq!(out.text, "all done");
        assert_eq!(out.native_session.as_deref(), Some("s1"));
    }

    /// Real `pi -p --mode json` output: a bash tool call, its result, then the final answer.
    #[test]
    fn parses_pi_json() {
        let mut p = OutputParser::new(OutputFormat::PiJson);
        let mut lines = vec![];
        for l in include_str!("../testdata/pi-json.jsonl").lines() {
            lines.extend(p.feed(l));
        }
        assert!(lines.iter().any(|l| l.kind == LogKind::Tool && l.text == "bash echo tool-ran-ok"));
        assert!(lines.iter().any(|l| l.kind == LogKind::ToolResult && l.text.trim() == "tool-ran-ok"));
        assert!(lines.iter().any(|l| l.kind == LogKind::Info && l.text.starts_with("finished")));
        let out = p.finish();
        assert_eq!(out.text, "Done. <message to=\"lead\">result ready</message>");
        assert_eq!(out.native_session.as_deref(), Some("bc66e7da-dfaa-4677-852a-1535c50f75fe"));
        assert!(out.error.is_none());
    }

    /// Events follow the Copilot SDK's `session-events.schema.json` (hand-written sample).
    #[test]
    fn parses_copilot_json() {
        let mut p = OutputParser::new(OutputFormat::CopilotJson);
        let mut logs = vec![];
        for l in include_str!("../testdata/copilot-json.jsonl").lines() {
            logs.extend(p.feed(l));
        }
        let kinds: Vec<LogKind> = logs.iter().map(|l| l.kind).collect();
        assert!(kinds.contains(&LogKind::Thinking));
        assert!(logs.iter().any(|l| l.kind == LogKind::Tool && l.text.contains("cargo test")));
        assert!(logs.iter().any(|l| l.kind == LogKind::ToolResult && l.text.contains("12 passed")));
        assert!(logs.iter().any(|l| l.kind == LogKind::Error && l.text.contains("pkill")));
        let out = p.finish();
        assert!(out.error.is_none());
        assert_eq!(out.native_session.as_deref(), Some("0cb916db-26aa-40f2-86b5-1ba81b225fd2"));
        assert!(out.text.starts_with("I'll add the endpoint."));
        assert!(!out.text.contains("subagent chatter"));
        let d = crate::orchestrator::parse_directives(&out.text);
        assert_eq!(d.messages, vec![("qa".to_string(), "POST /links is ready, tests pass.".to_string())]);
    }

    #[test]
    fn copilot_limits_switch_to_fallbacks() {
        let mut p = OutputParser::new(OutputFormat::CopilotJson);
        p.feed(r#"{"type":"session.error","data":{"errorType":"rate_limit","errorCode":"user_weekly_rate_limited","message":"You've reached your weekly rate limit for gpt-5.4. Try again in 2 days 3 hours."}}"#);
        let out = p.finish();
        let spec = &crate::config::builtin_runtimes()["copilot"];
        let reason = crate::limits::detect(spec, &out).expect("recognised as a limit");
        assert!(reason.contains("user_weekly_rate_limited"));
    }

    #[test]
    fn pi_errors_are_reported() {
        let mut p = OutputParser::new(OutputFormat::PiJson);
        p.feed(r#"{"type":"message_end","message":{"role":"assistant","content":[],"stopReason":"error","errorMessage":"401 invalid key"}}"#);
        assert_eq!(p.finish().error.as_deref(), Some("401 invalid key"));
    }

    #[test]
    fn parses_codex_json() {
        let mut p = OutputParser::new(OutputFormat::CodexJson);
        p.feed(r#"{"type":"thread.started","thread_id":"t1"}"#);
        p.feed(r#"{"type":"item.completed","item":{"id":"1","type":"agent_message","text":"first"}}"#);
        p.feed(r#"{"type":"item.completed","item":{"id":"2","type":"agent_message","text":"final"}}"#);
        let out = p.finish();
        assert_eq!(out.text, "final");
        assert_eq!(out.native_session.as_deref(), Some("t1"));
    }

    #[tokio::test]
    async fn runs_a_text_process_via_stdin() {
        let mut spec = builtin_runtimes()["hermes"].clone();
        spec.command = "cat".into();
        spec.args = vec![];
        let p = persona("", Effort::Medium);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (_ctx, crx) = watch::channel(false);
        let out = run_process(
            Invocation {
                spec: &spec,
                persona: &Persona { extra_args: vec![], ..p },
                prompt: "hello\nworld",
                workdir: Path::new("."),
                native_session: None,
                resume: false,
                wrapper: None,
            },
            tx,
            crx,
        )
        .await;
        assert_eq!(out.text, "hello\nworld");
        assert!(out.error.is_none());
        assert_eq!(rx.recv().await.unwrap().kind, LogKind::Command);
    }

    #[tokio::test]
    async fn guarded_runtimes_run_inside_the_wrapper() {
        // `env CMD ARGS…` behaves like a pass-through sandbox, so the wrapping is observable on any OS.
        let mut spec = builtin_runtimes()["hermes"].clone();
        spec.command = "cat".into();
        spec.args = vec![];
        let p = Persona { extra_args: vec![], ..persona("", Effort::Medium) };
        for (guard, expect_prefix) in [(true, true), (false, false)] {
            spec.guard = guard;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let (_ctx, crx) = watch::channel(false);
            let out = run_process(
                Invocation {
                    spec: &spec,
                    persona: &p,
                    prompt: "guarded",
                    workdir: Path::new("."),
                    native_session: None,
                    resume: false,
                    wrapper: Some(crate::guard::Backend {
                        name: "env",
                        program: "env".into(),
                        args: vec![],
                        hidden_env: &[],
                        covers: "test",
                    }),
                },
                tx,
                crx,
            )
            .await;
            assert_eq!(out.text, "guarded");
            let cmd = rx.recv().await.unwrap().text;
            assert_eq!(cmd.starts_with("[process guard: env]"), expect_prefix, "{cmd}");
        }
    }
}
