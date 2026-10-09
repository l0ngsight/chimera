//! A running (or finished) orchestration session: its event log, agent states and inboxes.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, broadcast, watch};

use crate::model::Team;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    /// Agents are working.
    Running,
    /// Nothing left to do but no final answer was produced; the user can send more input.
    Idle,
    /// The entry persona delivered a final answer.
    Completed,
    Failed,
    Cancelled,
    /// The server stopped while the session was running.
    Interrupted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Idle,
    Queued,
    Running,
    Waiting,
    Error,
    Done,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogKind {
    Command,
    Prompt,
    Text,
    Thinking,
    Tool,
    ToolResult,
    Stderr,
    Info,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub ts: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(flatten)]
    pub body: EventBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventBody {
    SessionStatus {
        status: SessionStatus,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    AgentStatus {
        status: AgentStatus,
    },
    Log {
        kind: LogKind,
        text: String,
    },
    Message {
        from: String,
        to: String,
        content: String,
        #[serde(default)]
        rejected: bool,
        /// Delivered without starting a turn (e.g. an acknowledgement); read on the next turn.
        #[serde(default, skip_serializing_if = "is_false")]
        deferred: bool,
    },
    /// A quick orchestration decision made with Jev.
    Decision {
        kind: DecisionKind,
        /// What was decided about, e.g. "message dev_lead → qa".
        subject: String,
        outcome: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<f64>,
        /// Whether chimera acted on the outcome.
        applied: bool,
        latency_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    TurnStart {
        turn: u32,
    },
    TurnEnd {
        turn: u32,
        duration_ms: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    Final {
        content: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    /// Is a teammate message only an acknowledgement?
    AckFilter,
    /// Which teammate did an unknown recipient id mean?
    ResolveRecipient,
    /// Did the entry persona finish without emitting `<final>`?
    DetectFinal,
    /// Which persona should handle a user message?
    RouteUser,
}

fn is_false(b: &bool) -> bool {
    !*b
}

fn yes() -> bool {
    true
}

/// A message waiting in a persona's inbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxMessage {
    pub from: String,
    pub content: String,
    /// Whether this message should start a turn. Non-waking messages are read on the next turn.
    #[serde(default = "yes")]
    pub wake: bool,
}

/// One completed turn, kept to rebuild context for stateless runtimes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub incoming: Vec<InboxMessage>,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentState {
    pub status: AgentStatus,
    pub turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_session: Option<String>,
    #[serde(default)]
    pub history: Vec<HistoryEntry>,
    /// System notes to include in the persona's next prompt (e.g. rejected messages).
    #[serde(default)]
    pub notes: Vec<String>,
    #[serde(default)]
    pub inbox: Vec<InboxMessage>,
}

impl AgentState {
    /// Has messages that should start a turn.
    pub fn has_work(&self) -> bool {
        self.inbox.iter().any(|m| m.wake)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub id: String,
    pub title: String,
    pub task: String,
    pub team: Team,
    pub workdir: PathBuf,
    pub dry_run: bool,
    pub status: SessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_detail: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// Turns used since the last user message (bounded by `team.max_turns`).
    pub round_turns: u32,
    pub total_turns: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub final_answer: Option<String>,
    pub agents: BTreeMap<String, AgentState>,
}

/// Compact view for listings and the UI.
#[derive(Debug, Clone, Serialize)]
pub struct SessionSummary {
    pub id: String,
    pub title: String,
    pub task: String,
    pub team_id: String,
    pub team_name: String,
    pub workdir: PathBuf,
    pub dry_run: bool,
    pub status: SessionStatus,
    pub status_detail: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub total_turns: u32,
    pub final_answer: Option<String>,
    pub agents: BTreeMap<String, AgentSummary>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentSummary {
    pub status: AgentStatus,
    pub turns: u32,
    pub queued: usize,
}

/// State used only by the mock runtime to simulate delegation.
#[derive(Debug, Default)]
pub struct MockState {
    pub requester: Option<String>,
    pub pending: HashSet<String>,
}

pub struct Inner {
    pub meta: SessionMeta,
    pub events: Vec<Event>,
    pub running: HashSet<String>,
    pub scheduler_active: bool,
    /// Set when the session should wind down once running turns finish.
    pub stopping: Option<(SessionStatus, Option<String>)>,
    pub cancel: watch::Sender<bool>,
    pub mock: HashMap<String, MockState>,
    pub mock_claimed: HashSet<String>,
    log_file: Option<File>,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Event>,
    pub wake: Notify,
}

impl Session {
    pub fn create(dir: PathBuf, meta: SessionMeta) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&dir)?;
        let session = Self::from_parts(dir, meta, vec![])?;
        session.save();
        Ok(session)
    }

    /// Loads a persisted session; sessions that were running become `Interrupted`.
    pub fn load(dir: &Path) -> anyhow::Result<Arc<Self>> {
        let mut meta: SessionMeta = serde_json::from_slice(&std::fs::read(dir.join("session.json"))?)?;
        let mut events = vec![];
        if let Ok(f) = File::open(dir.join("events.jsonl")) {
            for line in BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(ev) = serde_json::from_str::<Event>(&line) {
                    events.push(ev);
                }
            }
        }
        let interrupted = meta.status == SessionStatus::Running;
        if interrupted {
            meta.status = SessionStatus::Interrupted;
            meta.status_detail = Some("server restarted while the session was running".into());
            for a in meta.agents.values_mut() {
                if matches!(a.status, AgentStatus::Running | AgentStatus::Queued) {
                    a.status = AgentStatus::Waiting;
                }
            }
        }
        let session = Self::from_parts(dir.to_path_buf(), meta, events)?;
        if interrupted {
            session.emit(
                None,
                EventBody::SessionStatus {
                    status: SessionStatus::Interrupted,
                    detail: Some("server restarted while the session was running".into()),
                },
            );
            session.save();
        }
        Ok(session)
    }

    fn from_parts(dir: PathBuf, meta: SessionMeta, events: Vec<Event>) -> anyhow::Result<Arc<Self>> {
        let log_file = OpenOptions::new().create(true).append(true).open(dir.join("events.jsonl"))?;
        let (tx, _) = broadcast::channel(4096);
        let (cancel, _) = watch::channel(false);
        Ok(Arc::new(Self {
            id: meta.id.clone(),
            dir,
            inner: Mutex::new(Inner {
                meta,
                events,
                running: HashSet::new(),
                scheduler_active: false,
                stopping: None,
                cancel,
                mock: HashMap::new(),
                mock_claimed: HashSet::new(),
                log_file: Some(log_file),
            }),
            tx,
            wake: Notify::new(),
        }))
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records and broadcasts an event.
    pub fn emit(&self, agent: Option<&str>, body: EventBody) {
        let mut inner = self.lock();
        inner.emit(&self.tx, agent, body);
    }

    pub fn emit_locked(&self, inner: &mut Inner, agent: Option<&str>, body: EventBody) {
        inner.emit(&self.tx, agent, body);
    }

    /// Snapshot of past events plus a receiver for future ones, without gaps.
    pub fn subscribe(&self) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let inner = self.lock();
        (inner.events.clone(), self.tx.subscribe())
    }

    pub fn events_for(&self, agent: Option<&str>) -> Vec<Event> {
        let inner = self.lock();
        inner
            .events
            .iter()
            .filter(|e| agent.is_none() || e.agent.as_deref() == agent || is_message_involving(e, agent))
            .cloned()
            .collect()
    }

    pub fn summary(&self) -> SessionSummary {
        self.lock().summary()
    }

    pub fn save(&self) {
        let inner = self.lock();
        inner.save(&self.dir);
    }
}

fn is_message_involving(e: &Event, agent: Option<&str>) -> bool {
    match (&e.body, agent) {
        (EventBody::Message { from, to, .. }, Some(a)) => from == a || to == a,
        _ => false,
    }
}

impl Inner {
    fn emit(&mut self, tx: &broadcast::Sender<Event>, agent: Option<&str>, body: EventBody) {
        let ev = Event {
            seq: self.events.last().map(|e| e.seq + 1).unwrap_or(1),
            ts: Utc::now(),
            agent: agent.map(str::to_owned),
            body,
        };
        if let Some(f) = self.log_file.as_mut()
            && let Ok(line) = serde_json::to_string(&ev)
        {
            let _ = writeln!(f, "{line}");
        }
        self.events.push(ev.clone());
        self.meta.updated_at = ev.ts;
        let _ = tx.send(ev);
    }

    pub fn save(&self, dir: &Path) {
        let tmp = dir.join("session.json.tmp");
        match serde_json::to_vec_pretty(&self.meta) {
            Ok(bytes) => {
                if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, dir.join("session.json"))).is_err() {
                    tracing::warn!(session = %self.meta.id, "failed to persist session");
                }
            }
            Err(e) => tracing::warn!(session = %self.meta.id, "failed to serialize session: {e}"),
        }
    }

    pub fn summary(&self) -> SessionSummary {
        let m = &self.meta;
        SessionSummary {
            id: m.id.clone(),
            title: m.title.clone(),
            task: m.task.clone(),
            team_id: m.team.id.clone(),
            team_name: m.team.name.clone(),
            workdir: m.workdir.clone(),
            dry_run: m.dry_run,
            status: m.status,
            status_detail: m.status_detail.clone(),
            created_at: m.created_at,
            updated_at: m.updated_at,
            total_turns: m.total_turns,
            final_answer: m.final_answer.clone(),
            agents: m
                .agents
                .iter()
                .map(|(k, a)| {
                    (
                        k.clone(),
                        AgentSummary {
                            status: a.status,
                            turns: a.turns,
                            queued: a.inbox.iter().filter(|m| m.wake).count(),
                        },
                    )
                })
                .collect(),
        }
    }
}
