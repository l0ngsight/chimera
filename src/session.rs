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
        /// USD, when the provider reports it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
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
    /// The team was edited and the changes were applied to this session (from the next turn).
    TeamUpdated {
        changes: Vec<String>,
    },
    /// A persona switched configuration: to a fallback after hitting a usage/rate limit, or
    /// back towards its primary once that limit reset. `from`/`to` are "runtime · model · effort".
    Fallback {
        from: String,
        to: String,
        /// Fallback level now in use; 0 is the primary configuration.
        level: usize,
        reason: String,
        /// When the limit that caused the switch is expected to reset.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        until: Option<DateTime<Utc>>,
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
    /// How much reasoning effort does the next turn need?
    AdviseEffort,
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
    /// Fallback level in use (0 = the persona's primary runtime/model/effort).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub fallback: usize,
    /// Messages taken from the inbox by the turn that is running. Saved so that a crash or
    /// restart mid-turn puts them back into the inbox instead of losing them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub in_flight: Vec<InboxMessage>,
    /// Every configuration of the persona is rate limited: its messages wait until then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_until: Option<DateTime<Utc>>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl AgentState {
    pub fn new() -> Self {
        Self {
            status: AgentStatus::Idle,
            turns: 0,
            native_session: None,
            history: vec![],
            notes: vec![],
            inbox: vec![],
            fallback: 0,
            in_flight: vec![],
            parked_until: None,
        }
    }

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
    /// Personas removed from the team after the session started; kept so history stays valid.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub retired: std::collections::BTreeSet<String>,
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
    /// Fallback level in use (0 = primary).
    pub fallback: usize,
    /// "runtime · model · effort" currently used, when that is a fallback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
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
    /// User messages that arrived while the session was winding down (after `<final>` or a
    /// cancel, while turns still finish): delivered once it has stopped, starting a new round.
    pub held_back: Vec<(String, InboxMessage)>,
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
        // A turn that was running when the server stopped never finished: its messages go back
        // to the front of the inbox.
        for a in meta.agents.values_mut() {
            // Known limits are not kept across restarts, so the primary is simply tried again.
            a.parked_until = None;
            if !a.in_flight.is_empty() {
                let mut inbox = std::mem::take(&mut a.in_flight);
                inbox.append(&mut a.inbox);
                a.inbox = inbox;
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
                held_back: vec![],
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
    /// Events with `seq > after` plus a receiver for future ones, without gaps. A client that
    /// reconnects with the last seq it saw only gets what it missed.
    pub fn subscribe_after(&self, after: u64) -> (Vec<Event>, broadcast::Receiver<Event>) {
        let inner = self.lock();
        (Self::tail(&inner.events, after), self.tx.subscribe())
    }

    /// Events with `seq > after`.
    pub fn events_after(&self, after: u64) -> Vec<Event> {
        Self::tail(&self.lock().events, after)
    }

    fn tail(events: &[Event], after: u64) -> Vec<Event> {
        // Sequence numbers only grow, so the events after `after` are a suffix.
        let start = events.partition_point(|e| e.seq <= after);
        events[start..].to_vec()
    }

    /// Past events, then live ones, for one subscriber. Never skips an event: when the subscriber
    /// falls behind the broadcast channel (it lagged), the missed events are re-read from the
    /// session. After the past events comes one [`StreamItem::ReplayDone`].
    pub fn event_stream(self: &Arc<Self>, after: u64) -> impl futures::Stream<Item = StreamItem> + use<> {
        struct St {
            session: Arc<Session>,
            rx: broadcast::Receiver<Event>,
            pending: std::collections::VecDeque<Event>,
            last: u64,
            replay_done: bool,
        }
        let (history, rx) = self.subscribe_after(after);
        let last = history.last().map(|e| e.seq).unwrap_or(after);
        let st = St { session: self.clone(), rx, pending: history.into(), last, replay_done: false };
        futures::stream::unfold(st, |mut st| async move {
            loop {
                if let Some(e) = st.pending.pop_front() {
                    st.last = e.seq;
                    return Some((StreamItem::Event(e), st));
                }
                if !st.replay_done {
                    st.replay_done = true;
                    return Some((StreamItem::ReplayDone, st));
                }
                match st.rx.recv().await {
                    Ok(e) if e.seq > st.last => {
                        st.last = e.seq;
                        return Some((StreamItem::Event(e), st));
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        st.pending = st.session.events_after(st.last).into();
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        })
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

/// An item of [`Session::event_stream`].
#[derive(Debug)]
pub enum StreamItem {
    Event(Event),
    /// All events that existed when the stream started have been sent; what follows is live.
    ReplayDone,
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
                            fallback: a.fallback,
                            active: m
                                .team
                                .persona(k)
                                .filter(|p| a.fallback > 0 && a.fallback < p.levels())
                                .map(|p| p.at_level(a.fallback).runtime_label()),
                        },
                    )
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn session() -> Arc<Session> {
        let team: Team =
            serde_yaml::from_str("id: t\nname: T\nentry: a\npersonas:\n  - {id: a, name: A, runtime: mock}\n").unwrap();
        let dir = std::env::temp_dir().join(format!("chimera-session-{}", uuid::Uuid::new_v4()));
        let meta = SessionMeta {
            id: "s".into(),
            title: "t".into(),
            task: "t".into(),
            agents: [("a".to_string(), AgentState::new())].into(),
            team,
            workdir: dir.clone(),
            dry_run: true,
            status: SessionStatus::Idle,
            status_detail: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            round_turns: 0,
            total_turns: 0,
            final_answer: None,
            retired: Default::default(),
        };
        Session::create(dir, meta).unwrap()
    }

    fn log(s: &Session, i: usize) {
        s.emit(None, EventBody::Log { kind: LogKind::Info, text: i.to_string() });
    }

    async fn next_seqs(stream: &mut (impl futures::Stream<Item = StreamItem> + Unpin), n: usize) -> Vec<u64> {
        let mut out = vec![];
        while out.len() < n {
            match tokio::time::timeout(std::time::Duration::from_secs(5), stream.next()).await {
                Ok(Some(StreamItem::Event(e))) => out.push(e.seq),
                Ok(Some(StreamItem::ReplayDone)) => {}
                other => panic!("stream ended early: {other:?}"),
            }
        }
        out
    }

    /// The stream replays history, marks the end of the replay, resumes after a given seq, and
    /// never loses events when the subscriber falls behind the broadcast channel.
    #[tokio::test]
    async fn event_stream_resumes_and_survives_lag() {
        let s = session();
        for i in 0..10 {
            log(&s, i);
        }
        let mut stream = Box::pin(s.event_stream(0));
        assert_eq!(next_seqs(&mut stream, 10).await, (1..=10).collect::<Vec<_>>());
        assert!(matches!(stream.next().await, Some(StreamItem::ReplayDone)));

        // Far more events than the broadcast channel holds, before the subscriber reads any.
        for i in 0..6000 {
            log(&s, i);
        }
        assert_eq!(next_seqs(&mut stream, 6000).await, (11..=6010).collect::<Vec<_>>());

        // A reconnecting client only gets what came after the last seq it saw.
        let mut resumed = Box::pin(s.event_stream(6005));
        assert_eq!(next_seqs(&mut resumed, 5).await, (6006..=6010).collect::<Vec<_>>());
        assert!(matches!(resumed.next().await, Some(StreamItem::ReplayDone)));
    }
}
