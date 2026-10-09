//! Schedules persona turns inside a session and routes messages along the team graph.
//!
//! Protocol: at the end of a turn a persona addresses teammates with
//! `<message to="id">…</message>` blocks. The entry persona finishes the session with
//! `<final>…</final>`. Messages are only delivered along edges of the team graph.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::{mpsc, watch};

use crate::config::{RuntimeKind, RuntimeSpec, SessionMode};
use crate::decisions::{DecisionsConfig, Jev, Verdict};
use crate::model::{Persona, Team};
use crate::runtime::{self, Invocation, LogLine, TurnOutput, truncate};
use crate::session::{
    AgentState, AgentStatus, DecisionKind, EventBody, HistoryEntry, InboxMessage, Inner, LogKind, Session, SessionMeta,
    SessionStatus,
};

pub const USER: &str = "user";

pub struct Orchestrator {
    pub runtimes: BTreeMap<String, RuntimeSpec>,
    pub sessions_dir: PathBuf,
    pub workspaces_dir: PathBuf,
    /// Quick decisions with Jev (editable at runtime from the settings page).
    pub decisions: RwLock<DecisionsConfig>,
    pub jev: Jev,
}

/// A message produced by a turn, after recipient resolution and ack filtering.
#[derive(Debug, Clone)]
pub struct Outgoing {
    pub to: String,
    pub content: String,
    pub wake: bool,
}

/// What a finished turn asks the orchestrator to do.
#[derive(Debug, Default)]
pub struct Routed {
    pub messages: Vec<Outgoing>,
    pub final_answer: Option<String>,
}

pub struct NewSession {
    pub team: Team,
    pub task: String,
    pub title: Option<String>,
    pub workdir: Option<PathBuf>,
    pub dry_run: bool,
}

impl Orchestrator {
    pub fn create_session(self: &Arc<Self>, req: NewSession) -> anyhow::Result<Arc<Session>> {
        req.team.validate().map_err(anyhow::Error::msg)?;
        if req.task.trim().is_empty() {
            anyhow::bail!("task must not be empty");
        }
        let now = Utc::now();
        let id = format!("{}-{}", now.format("%Y%m%d-%H%M%S"), &uuid::Uuid::new_v4().simple().to_string()[..6]);
        let workdir = match req.workdir {
            Some(w) if !w.as_os_str().is_empty() => w,
            _ => self.workspaces_dir.join(&id),
        };
        std::fs::create_dir_all(&workdir)?;
        let workdir = workdir.canonicalize()?;
        let title = req
            .title
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| truncate(req.task.lines().next().unwrap_or("session"), 80));
        let agents = req
            .team
            .personas
            .iter()
            .map(|p| {
                (
                    p.id.clone(),
                    AgentState {
                        status: AgentStatus::Idle,
                        turns: 0,
                        native_session: None,
                        history: vec![],
                        notes: vec![],
                        inbox: vec![],
                    },
                )
            })
            .collect();
        let meta = SessionMeta {
            id: id.clone(),
            title,
            task: req.task.clone(),
            team: req.team,
            workdir,
            dry_run: req.dry_run,
            status: SessionStatus::Running,
            status_detail: None,
            created_at: now,
            updated_at: now,
            round_turns: 0,
            total_turns: 0,
            final_answer: None,
            agents,
        };
        let session = Session::create(self.sessions_dir.join(&id), meta)?;
        let entry = session.lock().meta.team.entry.clone();
        self.post_message(&session, &entry, &req.task)?;
        Ok(session)
    }

    /// Delivers a user message to a persona and (re)starts the scheduler.
    pub fn post_message(self: &Arc<Self>, session: &Arc<Session>, to: &str, content: &str) -> anyhow::Result<()> {
        if content.trim().is_empty() {
            anyhow::bail!("message must not be empty");
        }
        {
            let mut inner = session.lock();
            let Some(agent) = inner.meta.agents.get_mut(to) else {
                anyhow::bail!("unknown persona '{to}'");
            };
            agent.inbox.push(InboxMessage { from: USER.into(), content: content.into(), wake: true });
            if agent.status != AgentStatus::Running {
                agent.status = AgentStatus::Queued;
            }
            inner.meta.round_turns = 0;
            inner.meta.final_answer = None;
            session.emit_locked(
                &mut inner,
                None,
                EventBody::Message {
                    from: USER.into(),
                    to: to.into(),
                    content: content.into(),
                    rejected: false,
                    deferred: false,
                },
            );
            if inner.meta.agents[to].status == AgentStatus::Queued {
                session.emit_locked(&mut inner, Some(to), EventBody::AgentStatus { status: AgentStatus::Queued });
            }
            // A user message starts a new round for the mock runtime.
            inner.mock.clear();
            inner.mock_claimed.clear();
        }
        self.start(session);
        Ok(())
    }

    /// Picks the persona that should handle a user message (`to: "auto"`); falls back to the entry.
    pub async fn route_user_message(&self, session: &Arc<Session>, content: &str) -> String {
        let (team, task, dry_run) = {
            let inner = session.lock();
            (inner.meta.team.clone(), inner.meta.task.clone(), inner.meta.dry_run)
        };
        let cfg = self.decisions_cfg();
        if dry_run || !cfg.active() || !cfg.route_user_messages {
            return team.entry;
        }
        let res = self.jev.route_user_message(&cfg, &team, &task, content).await;
        let chosen = match &res {
            Ok(v) if v.confidence >= cfg.route_min_confidence && team.persona(&v.value).is_some() => {
                Some(v.value.clone())
            }
            _ => None,
        };
        record(session, None, DecisionKind::RouteUser, "user message".into(), &res, chosen.is_some(), |v| v.clone());
        chosen.unwrap_or(team.entry)
    }

    fn decisions_cfg(&self) -> DecisionsConfig {
        self.decisions.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Turns a persona's reply into deliveries, using Jev for quick routing decisions.
    async fn route_output(
        &self,
        session: &Arc<Session>,
        team: &Team,
        persona: &Persona,
        output: &TurnOutput,
    ) -> Routed {
        let directives = parse_directives(&output.text);
        let mut routed = Routed {
            messages: directives
                .messages
                .into_iter()
                .map(|(to, content)| Outgoing { to, content, wake: true })
                .collect(),
            final_answer: directives.final_answer,
        };
        let cfg = self.decisions_cfg();
        if session.lock().meta.dry_run || !cfg.active() || output.error.is_some() {
            return routed;
        }
        let from = persona.id.as_str();

        // 1. Messages addressed to an id that is not on the team: ask which teammate was meant.
        if cfg.resolve_recipients {
            let candidates = team.recipients_of(from);
            for m in routed.messages.iter_mut().filter(|m| m.to != USER && team.persona(&m.to).is_none()) {
                if candidates.is_empty() {
                    break;
                }
                let res = self.jev.resolve_recipient(&cfg, persona, &m.to, &m.content, &candidates).await;
                let resolved = match &res {
                    Ok(v)
                        if v.value != "none"
                            && v.confidence >= cfg.resolve_min_confidence
                            && team.can_send(from, &v.value) =>
                    {
                        Some(v.value.clone())
                    }
                    _ => None,
                };
                let subject = format!("unknown recipient `{}` from {from}", m.to);
                record(session, Some(from), DecisionKind::ResolveRecipient, subject, &res, resolved.is_some(), |v| {
                    v.clone()
                });
                if let Some(to) = resolved {
                    m.to = to;
                }
            }
        }

        // 2. Acknowledgements should not start a (potentially expensive) turn of the recipient.
        if cfg.ack_filter {
            let cfg = &cfg;
            let checks = routed.messages.iter().enumerate().filter_map(|(i, m)| {
                let to = team.persona(&m.to).filter(|_| team.can_send(from, &m.to))?;
                Some(async move { (i, self.jev.classify_message(cfg, persona, to, &m.content).await) })
            });
            for (i, res) in futures::future::join_all(checks).await {
                let is_ack = matches!(&res, Ok(v) if v.value == "ack" && v.confidence >= cfg.ack_min_confidence);
                let subject = format!("message {from} → {}", routed.messages[i].to);
                record(session, Some(from), DecisionKind::AckFilter, subject, &res, is_ack, |v| v.clone());
                if is_ack {
                    routed.messages[i].wake = false;
                }
            }
        }

        // 3. The entry persona says it is done but forgot <final>, and nothing else is pending.
        let quiet = routed.final_answer.is_none()
            && !routed.messages.iter().any(|m| m.wake)
            && !output.text.trim().is_empty()
            && {
                let inner = session.lock();
                inner.running.iter().all(|id| id == from)
                    && inner.meta.agents.iter().all(|(id, a)| id == from || !a.has_work())
            };
        if cfg.detect_final && from == team.entry && quiet {
            let task = session.lock().meta.task.clone();
            let res = self.jev.task_complete(&cfg, &task, &output.text).await;
            let done = matches!(&res, Ok(v) if v.value >= cfg.final_threshold);
            record(session, Some(from), DecisionKind::DetectFinal, format!("reply of {from}"), &res, done, |p| {
                format!("P(task complete) = {p:.2}")
            });
            if done {
                routed.final_answer = Some(strip_directives(&output.text));
            }
        }
        routed
    }

    pub fn cancel(&self, session: &Arc<Session>) {
        let mut inner = session.lock();
        if !inner.scheduler_active {
            return;
        }
        inner.stopping = Some((SessionStatus::Cancelled, Some("cancelled by user".into())));
        let _ = inner.cancel.send(true);
        drop(inner);
        session.wake.notify_one();
    }

    fn start(self: &Arc<Self>, session: &Arc<Session>) {
        let mut inner = session.lock();
        if inner.scheduler_active {
            drop(inner);
            session.wake.notify_one();
            return;
        }
        inner.scheduler_active = true;
        inner.stopping = None;
        inner.cancel = watch::channel(false).0;
        inner.meta.status = SessionStatus::Running;
        inner.meta.status_detail = None;
        session.emit_locked(
            &mut inner,
            None,
            EventBody::SessionStatus { status: SessionStatus::Running, detail: None },
        );
        inner.save(&session.dir);
        drop(inner);
        let (orch, session) = (self.clone(), session.clone());
        tokio::spawn(async move { orch.run_scheduler(session).await });
    }

    async fn run_scheduler(self: Arc<Self>, session: Arc<Session>) {
        loop {
            {
                let mut inner = session.lock();
                if inner.stopping.is_none() {
                    self.start_runnable(&session, &mut inner);
                }
                if inner.running.is_empty() {
                    finalize(&session, &mut inner);
                    return;
                }
            }
            session.wake.notified().await;
        }
    }

    /// Starts turns for personas with pending messages, respecting parallelism and turn budget.
    fn start_runnable(self: &Arc<Self>, session: &Arc<Session>, inner: &mut Inner) {
        let team = inner.meta.team.clone();
        for persona in &team.personas {
            if inner.running.len() >= team.max_parallel || inner.meta.round_turns >= team.max_turns {
                break;
            }
            let agent = &inner.meta.agents[&persona.id];
            if !agent.has_work() || inner.running.contains(&persona.id) {
                continue;
            }
            self.start_turn(session, inner, persona.clone());
        }
    }

    fn start_turn(self: &Arc<Self>, session: &Arc<Session>, inner: &mut Inner, persona: Persona) {
        let runtime_name = if inner.meta.dry_run { "mock".to_string() } else { persona.runtime.clone() };
        let spec = self.runtimes.get(&runtime_name).cloned();
        inner.meta.round_turns += 1;
        inner.meta.total_turns += 1;
        let turn = inner.meta.total_turns;
        inner.running.insert(persona.id.clone());

        let agent = inner.meta.agents.get_mut(&persona.id).expect("agent exists");
        let incoming = std::mem::take(&mut agent.inbox);
        let notes = std::mem::take(&mut agent.notes);
        agent.status = AgentStatus::Running;
        agent.turns += 1;

        // Decide whether to continue a native CLI session or replay history in the prompt.
        let (native_session, resume) = match spec.as_ref().map(|s| (s.session, s.resume_args.is_some())) {
            Some((SessionMode::Preassign, has_resume)) => match &agent.native_session {
                Some(id) if has_resume => (Some(id.clone()), true),
                _ => (Some(uuid::Uuid::new_v4().to_string()), false),
            },
            Some((SessionMode::Capture, true)) => (agent.native_session.clone(), agent.native_session.is_some()),
            _ => (None, false),
        };

        let prompt = build_prompt(&inner.meta, &persona, &incoming, &notes, resume);
        session.emit_locked(inner, Some(&persona.id), EventBody::AgentStatus { status: AgentStatus::Running });
        session.emit_locked(inner, Some(&persona.id), EventBody::TurnStart { turn });
        session.emit_locked(inner, Some(&persona.id), EventBody::Log { kind: LogKind::Prompt, text: prompt.clone() });

        let cancel = inner.cancel.subscribe();
        let workdir = inner.meta.workdir.clone();
        let team = inner.meta.team.clone();
        let task = inner.meta.task.clone();
        let session = session.clone();
        let orch = self.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let (log_tx, mut log_rx) = mpsc::unbounded_channel::<LogLine>();
            let fwd_session = session.clone();
            let pid = persona.id.clone();
            let forward = tokio::spawn(async move {
                while let Some(l) = log_rx.recv().await {
                    fwd_session.emit(Some(&pid), EventBody::Log { kind: l.kind, text: l.text });
                }
            });
            let output = match spec {
                None => TurnOutput {
                    error: Some(format!("unknown runtime '{runtime_name}' (check chimera.yaml)")),
                    ..Default::default()
                },
                Some(spec) if spec.kind == RuntimeKind::Mock => {
                    run_mock(&session, &team, &persona, &task, &workdir, &incoming, log_tx, cancel).await
                }
                Some(spec) => {
                    runtime::run_process(
                        Invocation {
                            spec: &spec,
                            persona: &persona,
                            prompt: &prompt,
                            workdir: &workdir,
                            native_session: native_session.as_deref(),
                            resume,
                        },
                        log_tx,
                        cancel,
                    )
                    .await
                }
            };
            let _ = forward.await;
            let elapsed = started.elapsed();
            let routed = orch.route_output(&session, &team, &persona, &output).await;
            complete_turn(&session, &persona, turn, incoming, output, routed, elapsed);
        });
    }
}

/// Records a finished turn and routes the messages it produced.
fn complete_turn(
    session: &Arc<Session>,
    persona: &Persona,
    turn: u32,
    incoming: Vec<InboxMessage>,
    output: TurnOutput,
    routed: Routed,
    elapsed: Duration,
) {
    let mut inner = session.lock();
    let team = inner.meta.team.clone();
    let from = persona.id.as_str();
    let stopping = inner.stopping.is_some();
    let failed = output.error.is_some() && !stopping;

    if let Some(err) = &output.error {
        session.emit_locked(&mut inner, Some(from), EventBody::Log { kind: LogKind::Error, text: err.clone() });
    }
    session.emit_locked(
        &mut inner,
        Some(from),
        EventBody::TurnEnd { turn, duration_ms: elapsed.as_millis() as u64, error: output.error.clone() },
    );

    let mut notes = vec![];
    let mut deliveries: Vec<Outgoing> = vec![];
    for m in routed.messages {
        let allowed = team.persona(&m.to).is_some() && team.can_send(from, &m.to);
        session.emit_locked(
            &mut inner,
            None,
            EventBody::Message {
                from: from.into(),
                to: m.to.clone(),
                content: m.content.clone(),
                rejected: !allowed,
                deferred: allowed && !m.wake,
            },
        );
        let to = m.to.clone();
        if allowed {
            deliveries.push(m);
        } else if to == USER {
            notes.push(
                "Messages to `user` are not delivered. The entry persona reports to the user with <final>.".into(),
            );
        } else {
            notes.push(format!("Your message to `{to}` was NOT delivered: you have no communication channel to it."));
        }
    }

    // Tell the senders when a persona fails so they can react instead of waiting forever.
    if failed {
        let err = output.error.clone().unwrap_or_default();
        for m in incoming.iter().filter(|m| m.from != USER) {
            if team.persona(&m.from).is_some() && !deliveries.iter().any(|d| d.to == m.from) {
                deliveries.push(Outgoing {
                    to: m.from.clone(),
                    content: format!(
                        "[chimera] {} ({from}) failed while handling your message: {}",
                        persona.name,
                        truncate(&err, 500)
                    ),
                    wake: true,
                });
            }
        }
    }

    if let Some(final_answer) = routed.final_answer {
        if from == team.entry {
            inner.meta.final_answer = Some(final_answer.clone());
            session.emit_locked(&mut inner, Some(from), EventBody::Final { content: final_answer });
            if inner.stopping.is_none() {
                inner.stopping = Some((SessionStatus::Completed, None));
                let _ = inner.cancel.send(true);
            }
        } else {
            notes.push("Only the entry persona can emit <final>; report your results to a teammate instead.".into());
        }
    }

    {
        let agent = inner.meta.agents.get_mut(from).expect("agent exists");
        agent.history.push(HistoryEntry { incoming, output: output.text.clone() });
        agent.notes.extend(notes);
        if output.error.is_none() && output.native_session.is_some() {
            agent.native_session = output.native_session;
        }
    }

    for Outgoing { to, content, wake } in deliveries {
        let target = inner.meta.agents.get_mut(&to).expect("validated recipient");
        target.inbox.push(InboxMessage { from: from.into(), content, wake });
        if wake && target.status != AgentStatus::Running && target.status != AgentStatus::Queued {
            target.status = AgentStatus::Queued;
            session.emit_locked(&mut inner, Some(&to), EventBody::AgentStatus { status: AgentStatus::Queued });
        }
    }

    let agent = inner.meta.agents.get_mut(from).expect("agent exists");
    agent.status = if failed {
        AgentStatus::Error
    } else if !agent.has_work() {
        AgentStatus::Waiting
    } else {
        AgentStatus::Queued
    };
    let status = agent.status;
    session.emit_locked(&mut inner, Some(from), EventBody::AgentStatus { status });
    inner.running.remove(from);
    inner.save(&session.dir);
    drop(inner);
    session.wake.notify_one();
}

/// Called with no turns running: decides the session's resting status.
fn finalize(session: &Arc<Session>, inner: &mut Inner) {
    let (status, detail) = match inner.stopping.take() {
        Some(s) => s,
        None if inner.meta.agents.values().any(|a| a.has_work()) => (
            SessionStatus::Idle,
            Some(format!("turn limit reached ({} turns); send a message to continue", inner.meta.team.max_turns)),
        ),
        None => (
            SessionStatus::Idle,
            Some("no pending messages and no final answer yet; send a message to continue".into()),
        ),
    };
    let clear_inboxes = matches!(status, SessionStatus::Completed | SessionStatus::Cancelled);
    let ids: Vec<String> = inner.meta.agents.keys().cloned().collect();
    for id in ids {
        let agent = inner.meta.agents.get_mut(&id).expect("agent exists");
        if clear_inboxes {
            agent.inbox.clear();
        }
        let new_status = match (status, agent.status) {
            (_, AgentStatus::Error) => AgentStatus::Error,
            (SessionStatus::Completed, AgentStatus::Idle) => AgentStatus::Idle,
            (SessionStatus::Completed, _) => AgentStatus::Done,
            (_, AgentStatus::Running) => AgentStatus::Waiting,
            (_, s) if agent.has_work() => s,
            (_, AgentStatus::Queued) => AgentStatus::Waiting,
            (_, s) => s,
        };
        if new_status != agent.status {
            agent.status = new_status;
            session.emit_locked(inner, Some(&id), EventBody::AgentStatus { status: new_status });
        }
    }
    inner.meta.status = status;
    inner.meta.status_detail = detail.clone();
    inner.scheduler_active = false;
    session.emit_locked(inner, None, EventBody::SessionStatus { status, detail });
    inner.save(&session.dir);
}

#[derive(Debug, Default, PartialEq)]
pub struct Directives {
    pub messages: Vec<(String, String)>,
    pub final_answer: Option<String>,
}

/// Extracts `<message to="id">…</message>` and `<final>…</final>` blocks.
pub fn parse_directives(text: &str) -> Directives {
    let mut d = Directives::default();
    let mut rest = text;
    while let Some(start) = rest.find("<message") {
        let after = &rest[start + "<message".len()..];
        let Some(tag_end) = after.find('>') else { break };
        let attrs = &after[..tag_end];
        let body_start = &after[tag_end + 1..];
        let Some(close) = body_start.find("</message>") else { break };
        if let Some(to) = attr(attrs, "to") {
            let to = to.trim().trim_start_matches('@').to_lowercase();
            let content = body_start[..close].trim().to_string();
            if !content.is_empty() {
                d.messages.push((to, content));
            }
        }
        rest = &body_start[close + "</message>".len()..];
    }
    if let Some(start) = text.rfind("<final>") {
        let body = &text[start + "<final>".len()..];
        let body = body.find("</final>").map(|end| &body[..end]).unwrap_or(body);
        let body = body.trim();
        if !body.is_empty() {
            d.final_answer = Some(body.to_string());
        }
    }
    d
}

/// The reply without `<message>` blocks, used as the final answer when `<final>` was missing.
fn strip_directives(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("<message") {
        out.push_str(&rest[..start]);
        match rest[start..].find("</message>") {
            Some(end) => rest = &rest[start + end + "</message>".len()..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// Emits a `Decision` event for a Jev call.
fn record<T>(
    session: &Session,
    agent: Option<&str>,
    kind: DecisionKind,
    subject: String,
    res: &anyhow::Result<Verdict<T>>,
    applied: bool,
    outcome: impl Fn(&T) -> String,
) {
    let body = match res {
        Ok(v) => EventBody::Decision {
            kind,
            subject,
            outcome: outcome(&v.value),
            confidence: Some(v.confidence),
            applied,
            latency_ms: v.latency_ms,
            cost: v.cost,
            error: None,
        },
        Err(e) => {
            tracing::warn!("jev decision {kind:?} failed: {e:#}");
            EventBody::Decision {
                kind,
                subject,
                outcome: "skipped".into(),
                confidence: None,
                applied: false,
                latency_ms: 0,
                cost: None,
                error: Some(format!("{e:#}")),
            }
        }
    };
    session.emit(agent, body);
}

fn attr(attrs: &str, name: &str) -> Option<String> {
    let idx = attrs.find(&format!("{name}="))?;
    let v = &attrs[idx + name.len() + 1..];
    let quote = v.chars().next()?;
    if quote == '"' || quote == '\'' {
        let v = &v[1..];
        v.find(quote).map(|end| v[..end].to_string())
    } else {
        Some(v.split_whitespace().next()?.to_string())
    }
}

const HISTORY_BUDGET: usize = 24_000;

/// Included in every turn: agents run unattended on a computer someone is using.
const GROUND_RULES: &str = "# Ground rules for this computer\n\n\
You run unattended on a computer that a person is using at the same time.\n\
- Only stop processes you started yourself in this session. Never kill, quit or restart other programs: \
not the user's web browser, editors, terminals, chat apps, nor the Chimera server that runs you.\n\
- Never use pkill/killall, never kill processes by name or by port (e.g. `lsof -ti:PORT | xargs kill`). \
If a port is busy, use another port.\n\
- For browser testing, use a headless browser with its own temporary profile (e.g. Playwright's bundled \
Chromium). Never launch, restart, control or attach to the user's own Chrome, Safari or Firefox or their profiles.\n\
- Do not change system settings, global configuration or files outside the shared directory unless the task \
explicitly requires it.\n\n";

/// Builds the prompt for one persona turn.
pub fn build_prompt(
    meta: &SessionMeta,
    persona: &Persona,
    incoming: &[InboxMessage],
    notes: &[String],
    resume: bool,
) -> String {
    let team = &meta.team;
    let is_entry = persona.id == team.entry;
    let display = |id: &str| -> String {
        if id == USER {
            "the user".into()
        } else {
            team.persona(id).map(|p| format!("{} (`{id}`)", p.name)).unwrap_or_else(|| format!("`{id}`"))
        }
    };
    let mut p = String::new();

    if !resume {
        let _ = writeln!(p, "# Your role\n");
        if persona.prompt.trim().is_empty() {
            let _ = writeln!(p, "You are {}, {}.\n", persona.name, persona.role);
        } else {
            let _ = writeln!(p, "{}\n", persona.prompt.trim());
        }
        let _ = writeln!(p, "# Team context\n");
        let _ = writeln!(
            p,
            "You are **{}** (id `{}`){} in the team \"{}\", orchestrated by Chimera. \
             Every team member works in the same shared directory `{}`; files you create there are visible to the others.",
            persona.name,
            persona.id,
            if persona.role.is_empty() { String::new() } else { format!(", the {}", persona.role) },
            team.name,
            meta.workdir.display()
        );
        if is_entry {
            let _ = writeln!(
                p,
                "You are the team's entry point: the user gave the task to you and you are accountable for delivering the final result."
            );
        }
        let _ = writeln!(p, "\nOriginal task from the user:\n\"\"\"\n{}\n\"\"\"\n", meta.task.trim());
        p.push_str(&roster(team, persona));

        let history = &meta.agents[&persona.id].history;
        if !history.is_empty() {
            let _ = writeln!(p, "# Your previous turns (oldest first, may be truncated)\n");
            let mut entries = vec![];
            let mut used = 0;
            for (i, h) in history.iter().enumerate().rev() {
                let from: Vec<String> = h.incoming.iter().map(|m| display(&m.from)).collect();
                let entry = format!(
                    "## Turn {}\nReceived messages from: {}\nYour reply:\n{}\n",
                    i + 1,
                    from.join(", "),
                    truncate(&h.output, 4000)
                );
                used += entry.len();
                if used > HISTORY_BUDGET {
                    entries.push(format!("(… {} earlier turns omitted)\n", i + 1));
                    break;
                }
                entries.push(entry);
            }
            for e in entries.into_iter().rev() {
                p.push_str(&e);
                p.push('\n');
            }
        }
    } else {
        p.push_str("(Chimera: new turn in the same session.)\n\n");
    }

    if !notes.is_empty() {
        let _ = writeln!(p, "# Notes from the orchestrator\n");
        for n in notes {
            let _ = writeln!(p, "- {n}");
        }
        p.push('\n');
    }

    let _ = writeln!(p, "# New messages for you\n");
    for m in incoming {
        let _ = writeln!(p, "## From {}\n\n{}\n", display(&m.from), m.content.trim());
    }

    p.push_str(GROUND_RULES);
    let _ = writeln!(p, "# How to communicate\n");
    if resume {
        p.push_str(&roster(team, persona));
    }
    let _ = writeln!(
        p,
        "Do the work your role calls for, using your tools and the shared directory. Your teammates only see what \
         you send them explicitly. To message a teammate, put this in your final reply:\n\n\
         <message to=\"teammate_id\">\nself-contained message: what you did, what you need, where the files are\n</message>\n\n\
         You may include several message blocks. They are delivered when your turn ends; replies arrive in a later turn, so do not wait for them."
    );
    if is_entry {
        let _ = writeln!(
            p,
            "\nWhen the whole task is complete and verified, finish the session with:\n\n<final>\nthe result for the user\n</final>\n\n\
             Do not emit <final> while you still expect work from teammates."
        );
    } else {
        let _ = writeln!(
            p,
            "\nWhen you finish what was asked of you, report back to whoever asked (if you have a channel to them)."
        );
    }
    p
}

fn roster(team: &Team, persona: &Persona) -> String {
    let recipients = team.recipients_of(&persona.id);
    let mut s = String::new();
    if recipients.is_empty() {
        s.push_str("You cannot message any teammate; put your results in your reply and in the shared directory.\n\n");
    } else {
        s.push_str("Teammates you can message:\n");
        for r in recipients {
            let _ = writeln!(
                s,
                "- `{}` — {}{}",
                r.id,
                r.name,
                if r.role.is_empty() { String::new() } else { format!(": {}", r.role) }
            );
        }
        s.push('\n');
    }
    s
}

/// Simulated agent for dry runs: delegates once along the graph, waits for replies,
/// then reports back. Writes a small note into the shared workspace.
#[allow(clippy::too_many_arguments)]
async fn run_mock(
    session: &Arc<Session>,
    team: &Team,
    persona: &Persona,
    task: &str,
    workdir: &std::path::Path,
    incoming: &[InboxMessage],
    log: mpsc::UnboundedSender<LogLine>,
    mut cancel: watch::Receiver<bool>,
) -> TurnOutput {
    let say = |kind, text: String| {
        let _ = log.send(LogLine { kind, text });
    };
    let jitter = (uuid::Uuid::new_v4().as_u128() % 900) as u64;
    say(LogKind::Info, format!("mock runtime simulating {} ({})", persona.runtime, persona.model));
    let senders: Vec<&str> = incoming.iter().map(|m| m.from.as_str()).collect();
    say(LogKind::Thinking, format!("Reading {} message(s) from {}…", incoming.len(), senders.join(", ")));
    for step in 0..3 {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(300 + jitter + step * 200)) => {}
            _ = cancel.changed() => return TurnOutput { error: Some("cancelled".into()), ..Default::default() },
        }
        match step {
            0 => say(LogKind::Tool, format!("Read {}", workdir.display())),
            1 => {
                let dir = workdir.join("chimera-mock");
                let file = dir.join(format!("{}.md", persona.id));
                let note = format!("# {} notes\n\nTurn at {}\n", persona.name, Utc::now());
                let res = tokio::fs::create_dir_all(&dir).await.and(tokio::fs::write(&file, note).await);
                say(LogKind::Tool, format!("Write {}", file.display()));
                say(LogKind::ToolResult, if res.is_ok() { "ok".into() } else { "write failed".into() });
            }
            _ => say(LogKind::Text, format!("Working as {}.", persona.role)),
        }
    }

    let mut inner = session.lock();
    let first = !inner.mock.contains_key(&persona.id);
    let claimed: Vec<String> = inner.mock_claimed.iter().cloned().collect();
    let st = inner.mock.entry(persona.id.clone()).or_default();
    let mut got_reply = false;
    for m in incoming {
        got_reply |= st.pending.remove(&m.from);
    }
    let mut out = String::new();
    let mut new_claims = vec![];
    if first {
        st.requester = incoming.first().map(|m| m.from.clone());
        for r in team.recipients_of(&persona.id) {
            if r.id == team.entry || Some(&r.id) == st.requester.as_ref() || claimed.contains(&r.id) {
                continue;
            }
            new_claims.push(r.id.clone());
            if team.can_send(&r.id, &persona.id) {
                st.pending.insert(r.id.clone());
            }
            let _ = writeln!(
                out,
                "<message to=\"{}\">\nPlease handle the {} part of the task: {}\n</message>",
                r.id,
                if r.role.is_empty() { &r.name } else { &r.role },
                truncate(task, 200)
            );
        }
    }
    if (first || got_reply) && st.pending.is_empty() {
        match st.requester.as_deref() {
            Some(USER) if persona.id == team.entry => {
                let _ = writeln!(
                    out,
                    "<final>\n[mock] {} coordinated the team; all delegated work reported back.\n</final>",
                    persona.name
                );
            }
            Some(req) if team.can_send(&persona.id, req) => {
                let _ = writeln!(out, "<message to=\"{req}\">\n[mock] {} work is complete.\n</message>", persona.role);
            }
            _ => out.push_str("[mock] Work complete; nobody to report to.\n"),
        }
    } else if !first && !got_reply {
        out.push_str("[mock] Acknowledged.\n");
    } else {
        let waiting: Vec<String> = st.pending.iter().cloned().collect();
        let _ = writeln!(out, "[mock] Waiting for: {}", waiting.join(", "));
    }
    inner.mock_claimed.extend(new_claims);
    drop(inner);
    say(LogKind::Text, out.clone());
    TurnOutput { text: out, ..Default::default() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_prompt_carries_the_ground_rules() {
        let team: Team =
            serde_yaml::from_str("id: t\nname: T\nentry: a\npersonas:\n  - {id: a, name: A, runtime: mock}\n").unwrap();
        let meta = SessionMeta {
            id: "s".into(),
            title: "t".into(),
            task: "do it".into(),
            agents: team
                .personas
                .iter()
                .map(|p| {
                    (
                        p.id.clone(),
                        AgentState {
                            status: AgentStatus::Idle,
                            turns: 0,
                            native_session: None,
                            history: vec![],
                            notes: vec![],
                            inbox: vec![],
                        },
                    )
                })
                .collect(),
            team: team.clone(),
            workdir: PathBuf::from("/tmp"),
            dry_run: false,
            status: SessionStatus::Running,
            status_detail: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            round_turns: 0,
            total_turns: 0,
            final_answer: None,
        };
        let msg = [InboxMessage { from: USER.into(), content: "go".into(), wake: true }];
        for resume in [false, true] {
            let prompt = build_prompt(&meta, &team.personas[0], &msg, &[], resume);
            assert!(prompt.contains("Never use pkill/killall"), "resume={resume}");
        }
    }

    #[test]
    fn parses_messages_and_final() {
        let d = parse_directives(
            "work done\n<message to=\"backend\">build the API</message>\n<message to='qa'>\n test it \n</message>\n<final>shipped</final>",
        );
        assert_eq!(
            d.messages,
            vec![("backend".to_string(), "build the API".to_string()), ("qa".to_string(), "test it".to_string())]
        );
        assert_eq!(d.final_answer.as_deref(), Some("shipped"));
    }

    #[test]
    fn ignores_unterminated_blocks() {
        let d = parse_directives("<message to=\"x\">never closed");
        assert!(d.messages.is_empty());
        assert!(d.final_answer.is_none());
    }
}

#[cfg(test)]
mod e2e_tests {
    use super::*;
    use crate::config::{ArgItem, OutputFormat, builtin_runtimes};

    fn temp_dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("chimera-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn orchestrator(runtimes: BTreeMap<String, RuntimeSpec>) -> Arc<Orchestrator> {
        orchestrator_with(runtimes, DecisionsConfig { enabled: false, ..Default::default() })
    }

    fn orchestrator_with(runtimes: BTreeMap<String, RuntimeSpec>, decisions: DecisionsConfig) -> Arc<Orchestrator> {
        let d = temp_dir();
        Arc::new(Orchestrator {
            runtimes,
            sessions_dir: d.join("sessions"),
            workspaces_dir: d.join("ws"),
            decisions: RwLock::new(decisions),
            jev: Jev::new(),
        })
    }

    async fn wait_until_settled(session: &Arc<Session>) -> SessionStatus {
        for _ in 0..600 {
            {
                let inner = session.lock();
                if !inner.scheduler_active {
                    return inner.meta.status;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("session did not settle");
    }

    fn sample_team() -> Team {
        serde_yaml::from_str(include_str!("../teams/software-dev.yaml")).unwrap()
    }

    #[tokio::test]
    async fn dry_run_of_sample_team_completes() {
        let orch = orchestrator(builtin_runtimes());
        let session = orch
            .create_session(NewSession {
                team: sample_team(),
                task: "Build a todo app".into(),
                title: None,
                workdir: None,
                dry_run: true,
            })
            .unwrap();
        assert_eq!(wait_until_settled(&session).await, SessionStatus::Completed);
        let inner = session.lock();
        assert!(inner.meta.final_answer.is_some());
        // Every persona reachable from the lead took part.
        for (id, agent) in &inner.meta.agents {
            assert!(agent.turns > 0, "{id} never ran");
        }
        assert!(inner.meta.workdir.join("chimera-mock/qa.md").exists());
        // Session state is persisted and reloadable.
        drop(inner);
        let reloaded = Session::load(&session.dir).unwrap();
        assert_eq!(reloaded.lock().meta.status, SessionStatus::Completed);
    }

    #[tokio::test]
    async fn process_runtime_routes_messages_and_rejects_missing_edges() {
        // A fake CLI: the lead delegates to `worker` and to `stranger` (no edge); the worker
        // replies; the lead then finishes.
        let script = r#"
            prompt=$(cat)
            case "$prompt" in
              *"id \`lead\`"*"From the user"*) printf 'plan\n<message to="worker">do it</message>\n<message to="stranger">hi</message>\n' ;;
              *"id \`worker\`"*) printf 'done\n<message to="lead">finished</message>\n' ;;
              *"id \`lead\`"*) printf '<final>all good</final>\n' ;;
            esac
        "#;
        let mut rts = builtin_runtimes();
        rts.insert(
            "sh".into(),
            RuntimeSpec {
                label: "sh".into(),
                command: "sh".into(),
                args: vec![ArgItem::One("-c".into()), ArgItem::One(script.into())],
                output: OutputFormat::Text,
                ..rts["hermes"].clone()
            },
        );
        let mut team: Team = serde_yaml::from_str(
            "id: t\nname: T\nentry: lead\npersonas:\n  - {id: lead, name: Lead, runtime: sh}\n  - {id: worker, name: Worker, runtime: sh}\n  - {id: stranger, name: Stranger, runtime: sh}\nedges:\n  - {from: lead, to: worker, bidirectional: true}\n",
        )
        .unwrap();
        team.max_parallel = 2;
        let orch = orchestrator(rts);
        let session = orch
            .create_session(NewSession { team, task: "go".into(), title: None, workdir: None, dry_run: false })
            .unwrap();
        assert_eq!(wait_until_settled(&session).await, SessionStatus::Completed);
        let inner = session.lock();
        assert_eq!(inner.meta.final_answer.as_deref(), Some("all good"));
        assert_eq!(inner.meta.agents["stranger"].turns, 0);
        assert_eq!(inner.meta.agents["worker"].turns, 1);
        assert_eq!(inner.meta.agents["lead"].turns, 2);
        assert!(
            inner
                .events
                .iter()
                .any(|e| matches!(&e.body, EventBody::Message { to, rejected: true, .. } if to == "stranger"))
        );
    }

    #[tokio::test]
    async fn cancel_stops_running_processes() {
        let mut rts = builtin_runtimes();
        rts.insert(
            "sleepy".into(),
            RuntimeSpec {
                label: "sleepy".into(),
                command: "sleep".into(),
                args: vec![ArgItem::One("30".into())],
                output: OutputFormat::Text,
                ..rts["hermes"].clone()
            },
        );
        let team: Team =
            serde_yaml::from_str("id: t\nname: T\nentry: a\npersonas:\n  - {id: a, name: A, runtime: sleepy}\n")
                .unwrap();
        let orch = orchestrator(rts);
        let session = orch
            .create_session(NewSession { team, task: "wait".into(), title: None, workdir: None, dry_run: false })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        orch.cancel(&session);
        assert_eq!(wait_until_settled(&session).await, SessionStatus::Cancelled);
        // The session can be resumed with a follow-up message afterwards.
        orch.post_message(&session, "a", "again").unwrap();
        assert!(session.lock().scheduler_active);
        orch.cancel(&session);
        assert_eq!(wait_until_settled(&session).await, SessionStatus::Cancelled);
    }

    /// A stand-in for the Jev API that answers like the documented examples.
    async fn fake_jev() -> String {
        use axum::{Json, Router, http::HeaderMap, routing::post};
        use serde_json::{Value, json};
        async fn systemone(headers: HeaderMap, Json(req): Json<Value>) -> Json<Value> {
            assert_eq!(headers["authorization"], "Bearer test-jev-key");
            assert_eq!(req["model"], "jev-latest");
            let q = &req["questions"]["q"];
            let answer = match q["type"].as_str().unwrap() {
                "noul" => json!({"type": "noul", "noul": 0.97}),
                _ if q["criteria"].get("none").is_some() => {
                    json!({"type": "choice", "choice": "worker", "probabilities": {}, "confidence": 0.9})
                }
                _ if q["criteria"].get("ack").is_some() => {
                    let msg = req["state"]["message"].as_str().unwrap().to_lowercase();
                    let kind = if msg.contains("thanks") { "ack" } else { "report" };
                    json!({"type": "choice", "choice": kind, "probabilities": {}, "confidence": 0.85})
                }
                _ => json!({"type": "choice", "choice": "worker", "probabilities": {}, "confidence": 0.8}),
            };
            Json(
                json!({"id": "gen-dec-1", "model": "typesafe/jev-1.13-20260917", "provider": "TypeSafe", "answers": {"q": answer}, "usage": {"input_tokens": 1, "output_tokens": 1, "cost": 0.00003}}),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, Router::new().route("/v1/systemone", post(systemone))).await.unwrap();
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn jev_resolves_recipients_filters_acks_and_detects_final() {
        // Lead misaddresses the worker; the worker reports to the lead and thanks the helper;
        // the lead then says it is done without <final>.
        let script = r#"
            prompt=$(cat)
            case "$prompt" in
              *"id \`lead\`"*"From the user"*) printf '<message to="the_worker">compute it</message>\n' ;;
              *"id \`worker\`"*) printf '<message to="lead">result: 42</message>\n<message to="helper">thanks for the help</message>\n' ;;
              *"id \`lead\`"*) printf 'All done. The answer is 42.\n' ;;
              *) printf 'helper should never run\n' ;;
            esac
        "#;
        let mut rts = builtin_runtimes();
        rts.insert(
            "sh".into(),
            RuntimeSpec {
                label: "sh".into(),
                command: "sh".into(),
                args: vec![ArgItem::One("-c".into()), ArgItem::One(script.into())],
                output: OutputFormat::Text,
                ..rts["hermes"].clone()
            },
        );
        let team: Team = serde_yaml::from_str(
            "id: t\nname: T\nentry: lead\npersonas:\n  - {id: lead, name: Lead, runtime: sh}\n  - {id: worker, name: Worker, runtime: sh}\n  - {id: helper, name: Helper, runtime: sh}\nedges:\n  - {from: lead, to: worker, bidirectional: true}\n  - {from: worker, to: helper, bidirectional: true}\n",
        )
        .unwrap();
        // SAFETY: test-only variable with a unique name, not read by other tests.
        unsafe { std::env::set_var("CHIMERA_TEST_JEV_KEY", "test-jev-key") };
        let cfg = DecisionsConfig {
            base_url: fake_jev().await,
            api_key_env: "CHIMERA_TEST_JEV_KEY".into(),
            ..Default::default()
        };
        let orch = orchestrator_with(rts, cfg);
        let session = orch
            .create_session(NewSession { team, task: "compute".into(), title: None, workdir: None, dry_run: false })
            .unwrap();
        assert_eq!(wait_until_settled(&session).await, SessionStatus::Completed);

        {
            let inner = session.lock();
            assert_eq!(inner.meta.final_answer.as_deref(), Some("All done. The answer is 42."));
            assert_eq!(inner.meta.agents["worker"].turns, 1, "misaddressed message was resolved");
            assert_eq!(inner.meta.agents["lead"].turns, 2);
            assert_eq!(inner.meta.agents["helper"].turns, 0, "acknowledgement did not wake the helper");
            let deferred = inner
                .events
                .iter()
                .any(|e| matches!(&e.body, EventBody::Message { to, deferred: true, .. } if to == "helper"));
            assert!(deferred);
            let kinds: Vec<DecisionKind> = inner
                .events
                .iter()
                .filter_map(|e| match &e.body {
                    EventBody::Decision { kind, applied: true, .. } => Some(*kind),
                    _ => None,
                })
                .collect();
            assert!(kinds.contains(&DecisionKind::ResolveRecipient));
            assert!(kinds.contains(&DecisionKind::AckFilter));
            assert!(kinds.contains(&DecisionKind::DetectFinal));
            // OpenRouter reports the cost of each call.
            assert!(inner.events.iter().any(|e| matches!(&e.body, EventBody::Decision { cost: Some(_), .. })));
        }

        // "auto" routing of a follow-up picks the persona Jev chose.
        assert_eq!(orch.route_user_message(&session, "one more thing").await, "worker");
    }

    #[tokio::test]
    async fn jev_failures_fall_back_to_default_behaviour() {
        unsafe { std::env::set_var("CHIMERA_TEST_JEV_KEY2", "k") };
        let cfg = DecisionsConfig {
            base_url: "http://127.0.0.1:9".into(), // nothing listens here
            api_key_env: "CHIMERA_TEST_JEV_KEY2".into(),
            timeout_ms: 500,
            ..Default::default()
        };
        let orch = orchestrator_with(builtin_runtimes(), cfg);
        let mut team = sample_team();
        team.personas.iter_mut().for_each(|p| p.runtime = "mock".into());
        let session = orch
            .create_session(NewSession { team, task: "x".into(), title: None, workdir: None, dry_run: false })
            .unwrap();
        assert_eq!(wait_until_settled(&session).await, SessionStatus::Completed);
        assert_eq!(orch.route_user_message(&session, "hi").await, "dev_lead");
        let inner = session.lock();
        assert!(inner.events.iter().any(|e| matches!(&e.body, EventBody::Decision { error: Some(_), .. })));
    }
}
