//! Terminal UI: the same workflow as the web UI (sessions, new session, live session view with
//! per-agent logs, messaging, cancel), talking to a Chimera server over its HTTP API.

mod client;
mod input;
mod ui;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use crossterm::execute;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

pub use client::Client;
use client::{SessionDetail, SessionRow, SettingsInfo};
use input::LineInput;

use crate::model::Team;
use crate::session::{AgentStatus, Event, EventBody, SessionStatus};

/// Results of background work, delivered to the UI loop.
enum Msg {
    Sessions(Vec<SessionRow>),
    Teams(Vec<Team>),
    Settings(SettingsInfo),
    Opened(Box<SessionDetail>),
    /// The open session's team was edited.
    TeamRefreshed(String, Box<Team>),
    Created(String),
    Event(String, Box<Event>),
    Notice(String),
    Error(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Sessions,
    NewSession,
    Session,
}

/// Live state of the open session.
pub struct SessionView {
    pub id: String,
    pub team: Team,
    pub summary: SessionRow,
    pub events: Vec<Event>,
    last_seq: u64,
    pub agent_status: HashMap<String, AgentStatus>,
    /// Turns per persona, counted from the (replayed) event stream.
    pub turns: HashMap<String, u32>,
    /// Personas running on a fallback after a usage limit: "runtime · model · effort".
    pub fallback: HashMap<String, String>,
    /// 0 = timeline, 1.. = personas in team order.
    pub selected: usize,
    /// Lines scrolled up from the bottom; 0 follows new output.
    pub scroll: u16,
    pub show_prompts: bool,
    pub composer: Option<Composer>,
    pub confirm_cancel: bool,
    stream: Option<JoinHandle<()>>,
}

impl SessionView {
    pub fn selected_persona(&self) -> Option<&str> {
        self.selected.checked_sub(1).and_then(|i| self.team.personas.get(i)).map(|p| p.id.as_str())
    }

    fn apply(&mut self, ev: Event) {
        if ev.seq <= self.last_seq {
            return; // replayed after a reconnect
        }
        self.last_seq = ev.seq;
        match &ev.body {
            EventBody::AgentStatus { status } => {
                if let Some(a) = &ev.agent {
                    self.agent_status.insert(a.clone(), *status);
                }
            }
            EventBody::SessionStatus { status, detail } => {
                self.summary.status = *status;
                self.summary.status_detail = detail.clone();
            }
            EventBody::TurnStart { turn } => self.summary.total_turns = self.summary.total_turns.max(*turn),
            EventBody::Final { content } => self.summary.final_answer = Some(content.clone()),
            EventBody::Fallback { to, level, .. } => {
                if let Some(a) = &ev.agent {
                    if *level == 0 {
                        self.fallback.remove(a);
                    } else {
                        self.fallback.insert(a.clone(), to.clone());
                    }
                }
            }
            _ => {}
        }
        if let (Some(a), EventBody::TurnStart { .. }) = (&ev.agent, &ev.body) {
            *self.turns.entry(a.clone()).or_default() += 1;
        }
        self.events.push(ev);
    }
}

pub struct Composer {
    pub input: LineInput,
    /// Index into `targets()`.
    pub target: usize,
}

pub struct App {
    client: Client,
    /// The server runs inside this process and stops when the TUI exits.
    pub embedded: bool,
    pub screen: Screen,
    pub sessions: Vec<SessionRow>,
    pub sessions_sel: usize,
    pub teams: Vec<Team>,
    pub settings: Option<SettingsInfo>,
    pub new_team: usize,
    pub new_task: LineInput,
    pub new_dry_run: bool,
    pub view: Option<SessionView>,
    pub status: Option<(String, bool, Instant)>,
    pub quit_armed: bool,
    pub tick: u64,
    should_quit: bool,
    tx: mpsc::UnboundedSender<Msg>,
    last_poll: Instant,
}

impl App {
    fn new(client: Client, embedded: bool, tx: mpsc::UnboundedSender<Msg>) -> Self {
        Self {
            client,
            embedded,
            screen: Screen::Sessions,
            sessions: vec![],
            sessions_sel: 0,
            teams: vec![],
            settings: None,
            new_team: 0,
            new_task: LineInput::default(),
            new_dry_run: false,
            view: None,
            status: None,
            quit_armed: false,
            tick: 0,
            should_quit: false,
            tx,
            last_poll: Instant::now() - Duration::from_secs(60),
        }
    }

    pub fn server(&self) -> &str {
        self.client.base()
    }

    /// Recipients offered by the composer: auto (when Jev routing is on) or the entry, then everyone.
    pub fn targets(&self) -> Vec<(String, String)> {
        let Some(v) = &self.view else { return vec![] };
        let mut t = vec![];
        if self.settings.as_ref().is_some_and(|s| s.auto_routing) && !v.summary.dry_run {
            t.push(("auto".to_string(), "⚡ auto (Jev routes)".to_string()));
        }
        for p in &v.team.personas {
            let entry = if p.id == v.team.entry { " (entry)" } else { "" };
            t.push((p.id.clone(), format!("{}{entry}", p.name)));
        }
        t
    }

    fn notify(&mut self, text: impl Into<String>, error: bool) {
        self.status = Some((text.into(), error, Instant::now()));
    }

    fn spawn<F>(&self, fut: F)
    where
        F: std::future::Future<Output = Msg> + Send + 'static,
    {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let _ = tx.send(fut.await);
        });
    }

    fn refresh_sessions(&mut self) {
        self.last_poll = Instant::now();
        let c = self.client.clone();
        self.spawn(async move {
            match c.sessions().await {
                Ok(s) => Msg::Sessions(s),
                Err(e) => Msg::Error(format!("cannot reach {}: {e:#}", c.base())),
            }
        });
        let c = self.client.clone();
        self.spawn(
            async move { c.settings().await.map(Msg::Settings).unwrap_or_else(|e| Msg::Error(format!("{e:#}"))) },
        );
    }

    fn open_session(&mut self, id: String) {
        let c = self.client.clone();
        self.spawn(async move {
            match c.session(&id).await {
                Ok(d) => Msg::Opened(Box::new(d)),
                Err(e) => Msg::Error(format!("{e:#}")),
            }
        });
    }

    fn close_session(&mut self) {
        if let Some(v) = self.view.take()
            && let Some(h) = v.stream
        {
            h.abort();
        }
        self.screen = Screen::Sessions;
        self.refresh_sessions();
    }

    fn running_sessions(&self) -> usize {
        self.sessions.iter().filter(|s| s.status == SessionStatus::Running).count()
    }

    fn request_quit(&mut self) {
        let running = self.running_sessions();
        if self.embedded && running > 0 && !self.quit_armed {
            self.quit_armed = true;
            self.notify(
                format!(
                    "{running} session(s) are running in this process and will stop. Press q again to quit, any other key to stay."
                ),
                true,
            );
        } else {
            self.should_quit = true;
        }
    }

    fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Sessions(s) => {
                self.sessions = s;
                self.sessions_sel = self.sessions_sel.min(self.sessions.len().saturating_sub(1));
            }
            Msg::Teams(t) => {
                self.teams = t;
                self.new_team = self.new_team.min(self.teams.len().saturating_sub(1));
            }
            Msg::Settings(s) => self.settings = Some(s),
            Msg::Opened(d) => {
                let d = *d;
                let (tx, mut rx) = mpsc::unbounded_channel();
                let stream = tokio::spawn(self.client.clone().follow(d.summary.id.clone(), tx));
                let fwd = self.tx.clone();
                let id = d.summary.id.clone();
                tokio::spawn(async move {
                    while let Some(ev) = rx.recv().await {
                        if fwd.send(Msg::Event(id.clone(), Box::new(ev))).is_err() {
                            break;
                        }
                    }
                });
                if let Some(old) = self.view.take().and_then(|v| v.stream) {
                    old.abort();
                }
                self.view = Some(SessionView {
                    id: d.summary.id.clone(),
                    agent_status: d.summary.agents.iter().map(|(k, a)| (k.clone(), a.status)).collect(),
                    turns: HashMap::new(),
                    fallback: HashMap::new(),
                    team: d.team,
                    summary: d.summary,
                    events: vec![],
                    last_seq: 0,
                    selected: 0,
                    scroll: 0,
                    show_prompts: false,
                    composer: None,
                    confirm_cancel: false,
                    stream: Some(stream),
                });
                self.screen = Screen::Session;
            }
            Msg::Created(id) => {
                self.new_task.clear();
                self.notify("session started", false);
                self.open_session(id);
            }
            Msg::Event(id, ev) => {
                let team_changed = matches!(ev.body, EventBody::TeamUpdated { .. });
                if let Some(v) = self.view.as_mut().filter(|v| v.id == id) {
                    v.apply(*ev);
                    if team_changed {
                        let c = self.client.clone();
                        self.spawn(async move {
                            match c.session(&id).await {
                                Ok(d) => Msg::TeamRefreshed(id, Box::new(d.team)),
                                Err(e) => Msg::Error(format!("{e:#}")),
                            }
                        });
                    }
                }
            }
            Msg::TeamRefreshed(id, team) => {
                if let Some(v) = self.view.as_mut().filter(|v| v.id == id) {
                    v.team = *team;
                    v.selected = v.selected.min(v.team.personas.len());
                }
            }
            Msg::Notice(t) => self.notify(t, false),
            Msg::Error(e) => self.notify(e, true),
        }
    }

    fn on_tick(&mut self) {
        self.tick += 1;
        if self.screen != Screen::Session && self.last_poll.elapsed() > Duration::from_secs(2) {
            self.refresh_sessions();
        }
        if self.status.as_ref().is_some_and(|(_, err, at)| at.elapsed() > Duration::from_secs(if *err { 8 } else { 4 }))
        {
            self.status = None;
        }
    }

    fn on_paste(&mut self, text: &str) {
        match self.screen {
            Screen::NewSession => self.new_task.insert_str(text),
            Screen::Session => {
                if let Some(c) = self.view.as_mut().and_then(|v| v.composer.as_mut()) {
                    c.input.insert_str(text);
                }
            }
            Screen::Sessions => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.request_quit();
            return;
        }
        if self.quit_armed && key.code != KeyCode::Char('q') {
            self.quit_armed = false;
            self.status = None;
        }
        match self.screen {
            Screen::Sessions => self.key_sessions(key),
            Screen::NewSession => self.key_new(key),
            Screen::Session => self.key_session(key),
        }
    }

    fn key_sessions(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') => self.request_quit(),
            KeyCode::Down | KeyCode::Char('j') => {
                self.sessions_sel = (self.sessions_sel + 1).min(self.sessions.len().saturating_sub(1))
            }
            KeyCode::Up | KeyCode::Char('k') => self.sessions_sel = self.sessions_sel.saturating_sub(1),
            KeyCode::Enter => {
                if let Some(s) = self.sessions.get(self.sessions_sel) {
                    self.open_session(s.id.clone());
                }
            }
            KeyCode::Char('n') => {
                self.screen = Screen::NewSession;
                let c = self.client.clone();
                self.spawn(
                    async move { c.teams().await.map(Msg::Teams).unwrap_or_else(|e| Msg::Error(format!("{e:#}"))) },
                );
            }
            KeyCode::Char('r') => self.refresh_sessions(),
            _ => {}
        }
    }

    fn key_new(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.screen = Screen::Sessions,
            KeyCode::Up => self.new_team = self.new_team.saturating_sub(1),
            KeyCode::Down => self.new_team = (self.new_team + 1).min(self.teams.len().saturating_sub(1)),
            KeyCode::Char('d') if ctrl => self.new_dry_run = !self.new_dry_run,
            KeyCode::Enter => {
                let Some(team) = self.teams.get(self.new_team) else {
                    return self.notify("no team selected", true);
                };
                let task = self.new_task.text().trim().to_string();
                if task.is_empty() {
                    return self.notify("type a task first", true);
                }
                let (c, team_id, dry) = (self.client.clone(), team.id.clone(), self.new_dry_run);
                self.notify("starting…", false);
                self.spawn(async move {
                    match c.create_session(&team_id, &task, dry).await {
                        Ok(s) => Msg::Created(s.id),
                        Err(e) => Msg::Error(format!("{e:#}")),
                    }
                });
            }
            _ => self.new_task.handle_key(key),
        }
    }

    fn key_session(&mut self, key: KeyEvent) {
        let targets = self.targets();
        let Some(v) = self.view.as_mut() else { return };

        if let Some(c) = v.composer.as_mut() {
            match key.code {
                KeyCode::Esc => v.composer = None,
                KeyCode::Tab => c.target = (c.target + 1) % targets.len().max(1),
                KeyCode::BackTab => c.target = (c.target + targets.len().max(1) - 1) % targets.len().max(1),
                KeyCode::Enter => {
                    let text = c.input.text().trim().to_string();
                    let Some((to, label)) = targets.get(c.target).cloned() else { return };
                    if text.is_empty() {
                        return;
                    }
                    v.composer = None;
                    let (cl, id) = (self.client.clone(), v.id.clone());
                    self.spawn(async move {
                        match cl.post_message(&id, &to, &text).await {
                            Ok(()) => Msg::Notice(format!("sent to {label}")),
                            Err(e) => Msg::Error(format!("{e:#}")),
                        }
                    });
                }
                _ => c.input.handle_key(key),
            }
            return;
        }

        if v.confirm_cancel {
            v.confirm_cancel = false;
            if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('c')) {
                let (cl, id) = (self.client.clone(), v.id.clone());
                self.spawn(async move {
                    match cl.cancel(&id).await {
                        Ok(()) => Msg::Notice("cancelling…".into()),
                        Err(e) => Msg::Error(format!("{e:#}")),
                    }
                });
            } else {
                self.status = None;
            }
            return;
        }

        let personas = v.team.personas.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => self.close_session(),
            KeyCode::Down | KeyCode::Char('j') => {
                v.selected = (v.selected + 1).min(personas);
                v.scroll = 0;
            }
            KeyCode::Up | KeyCode::Char('k') => {
                v.selected = v.selected.saturating_sub(1);
                v.scroll = 0;
            }
            KeyCode::PageUp => v.scroll = v.scroll.saturating_add(10),
            KeyCode::PageDown => v.scroll = v.scroll.saturating_sub(10),
            KeyCode::Home => v.scroll = u16::MAX,
            KeyCode::End => v.scroll = 0,
            KeyCode::Char('p') => v.show_prompts = !v.show_prompts,
            KeyCode::Char('m') | KeyCode::Char('i') => {
                // Default recipient: the selected persona, otherwise auto routing or the entry.
                let target = v
                    .selected_persona()
                    .and_then(|p| targets.iter().position(|(id, _)| id == p))
                    .or_else(|| targets.iter().position(|(id, _)| id == "auto"))
                    .or_else(|| targets.iter().position(|(id, _)| *id == v.team.entry))
                    .unwrap_or(0);
                v.composer = Some(Composer { input: LineInput::default(), target });
            }
            KeyCode::Char('c') if v.summary.status == SessionStatus::Running => {
                v.confirm_cancel = true;
                self.notify("Cancel this session? press y to confirm", true);
            }
            _ => {}
        }
    }
}

/// Runs the TUI until the user quits.
pub async fn run(client: Client, embedded: bool) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = event_loop(&mut terminal, client, embedded).await;
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn event_loop(terminal: &mut ratatui::DefaultTerminal, client: Client, embedded: bool) -> anyhow::Result<()> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut app = App::new(client, embedded, tx);
    app.refresh_sessions();
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    loop {
        terminal.draw(|f| ui::draw(f, &app))?;
        tokio::select! {
            ev = keys.next() => match ev {
                Some(Ok(TermEvent::Key(k))) => app.on_key(k),
                Some(Ok(TermEvent::Paste(text))) => app.on_paste(&text),
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(msg) = rx.recv() => {
                app.on_msg(msg);
                // Apply everything already queued (e.g. a replayed session history) before redrawing.
                while let Ok(msg) = rx.try_recv() {
                    app.on_msg(msg);
                }
            }
            _ = tick.tick() => app.on_tick(),
        }
        if app.should_quit {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn sample_view() -> SessionView {
        let team: Team = serde_yaml::from_str(include_str!("../../teams/software-dev.yaml")).unwrap();
        let summary: SessionRow = serde_json::from_value(serde_json::json!({
            "id": "s1", "title": "Build a URL shortener", "task": "Build a URL shortener", "team_name": team.name,
            "workdir": "/tmp/ws", "dry_run": false, "status": "running", "created_at": "2026-10-10T08:00:00Z",
            "total_turns": 3, "agents": {"dev_lead": {"status": "running", "turns": 2}}
        }))
        .unwrap();
        let mut v = SessionView {
            id: "s1".into(),
            team,
            summary,
            events: vec![],
            last_seq: 0,
            agent_status: HashMap::from([("dev_lead".to_string(), AgentStatus::Running)]),
            turns: HashMap::new(),
            fallback: HashMap::new(),
            selected: 0,
            scroll: 0,
            show_prompts: false,
            composer: None,
            confirm_cancel: false,
            stream: None,
        };
        let ev = |seq, agent: Option<&str>, body| Event {
            seq,
            ts: chrono::Utc::now(),
            agent: agent.map(str::to_owned),
            body,
        };
        v.apply(ev(
            1,
            None,
            EventBody::Message {
                from: "user".into(),
                to: "dev_lead".into(),
                content: "Build a URL shortener".into(),
                rejected: false,
                deferred: false,
            },
        ));
        v.apply(ev(2, Some("dev_lead"), EventBody::TurnStart { turn: 1 }));
        v.apply(ev(
            3,
            Some("dev_lead"),
            EventBody::Log { kind: crate::session::LogKind::Tool, text: "Write docs/PLAN.md".into() },
        ));
        v.apply(ev(
            4,
            None,
            EventBody::Message {
                from: "dev_lead".into(),
                to: "backend".into(),
                content: "Implement POST /links".into(),
                rejected: false,
                deferred: false,
            },
        ));
        v.apply(ev(4, None, EventBody::Final { content: "duplicate seq is ignored".into() }));
        v
    }

    fn render(app: &App, w: u16, h: u16) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| ui::draw(f, app)).unwrap();
        let buf = term.backend().buffer();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n").collect()
    }

    fn app() -> App {
        let (tx, _rx) = mpsc::unbounded_channel();
        App::new(Client::new("http://127.0.0.1:7878", None), false, tx)
    }

    #[tokio::test]
    async fn session_view_shows_agents_timeline_and_agent_log() {
        let mut app = app();
        app.view = Some(sample_view());
        app.screen = Screen::Session;
        let screen = render(&app, 140, 40);
        assert!(screen.contains("Build a URL shortener"), "{screen}");
        assert!(screen.contains("Dana (Dev Lead)"), "{screen}");
        assert!(screen.contains("running"), "{screen}");
        assert!(screen.contains("Implement POST /links"), "{screen}");
        assert!(!screen.contains("duplicate seq"), "{screen}");
        assert_eq!(app.view.as_ref().unwrap().turns["dev_lead"], 1, "turns are counted once from events");

        // Select the dev lead: its log shows the tool call.
        let v = app.view.as_mut().unwrap();
        v.selected = 1 + v.team.personas.iter().position(|p| p.id == "dev_lead").unwrap();
        let screen = render(&app, 140, 40);
        assert!(screen.contains("Write docs/PLAN.md"), "{screen}");
    }

    #[tokio::test]
    async fn composer_targets_and_sessions_screen_render() {
        let mut app = app();
        app.view = Some(sample_view());
        app.screen = Screen::Session;
        app.settings = Some(SettingsInfo { guard_on: false, guard_detail: "bwrap missing".into(), auto_routing: true });
        assert_eq!(app.targets()[0].0, "auto");
        app.on_key(KeyEvent::from(KeyCode::Char('m')));
        for ch in "hello".chars() {
            app.on_key(KeyEvent::from(KeyCode::Char(ch)));
        }
        let screen = render(&app, 120, 30);
        assert!(screen.contains("hello"), "{screen}");
        assert!(screen.contains("auto"), "{screen}");

        app.screen = Screen::Sessions;
        app.sessions = vec![app.view.as_ref().unwrap().summary.clone()];
        let screen = render(&app, 120, 30);
        assert!(screen.contains("Process guard is off"), "{screen}");
        assert!(screen.contains("Build a URL shortener"), "{screen}");
    }
}
