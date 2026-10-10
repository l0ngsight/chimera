//! Rendering for the terminal UI.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, BorderType, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState, Wrap,
};

use super::{App, Screen, SessionView};
use crate::session::{AgentStatus, Event, EventBody, LogKind, SessionStatus};

const ACCENT: Color = Color::Rgb(124, 156, 255);
const MUTED: Color = Color::Gray;
const DIM: Color = Color::DarkGray;

pub fn draw(f: &mut Frame, app: &App) {
    let [body, footer] = Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(f.area());
    match app.screen {
        Screen::Sessions => draw_sessions(f, app, body),
        Screen::NewSession => draw_new(f, app, body),
        Screen::Session => draw_session(f, app, body),
    }
    draw_footer(f, app, footer);
}

fn agent_color(s: AgentStatus) -> Color {
    match s {
        AgentStatus::Idle => DIM,
        AgentStatus::Queued => Color::Yellow,
        AgentStatus::Running => Color::LightBlue,
        AgentStatus::Waiting => Color::Cyan,
        AgentStatus::Error => Color::Red,
        AgentStatus::Done => Color::Green,
    }
}

fn session_color(s: SessionStatus) -> Color {
    match s {
        SessionStatus::Running => Color::LightBlue,
        SessionStatus::Idle | SessionStatus::Interrupted => Color::Yellow,
        SessionStatus::Completed => Color::Green,
        SessionStatus::Failed | SessionStatus::Cancelled => Color::Red,
    }
}

fn status_name<T: serde::Serialize>(s: T) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// A dot that pulses for running agents.
fn dot(status: AgentStatus, tick: u64) -> Span<'static> {
    let sym = if status == AgentStatus::Running && tick % 4 >= 2 { "○" } else { "●" };
    Span::styled(sym, Style::new().fg(agent_color(status)))
}

fn panel(title: impl AsRef<str>) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .title(Line::from(format!(" {} ", title.as_ref())).style(Style::new().fg(MUTED).add_modifier(Modifier::BOLD)))
}

// ---------------------------------------------------------------- sessions

fn draw_sessions(f: &mut Frame, app: &App, area: Rect) {
    let guard_off = app.settings.as_ref().is_some_and(|s| !s.guard_on);
    let [head, warn, table] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(if guard_off { 2 } else { 0 }),
        Constraint::Min(3),
    ])
    .areas(area);

    let mode = if app.embedded { "embedded server" } else { "server" };
    f.render_widget(
        Line::from(vec![
            " ◐ Chimera ".bold().fg(ACCENT),
            Span::styled(format!(" {mode} {} ", app.server()), Style::new().fg(DIM)),
        ]),
        head,
    );
    if let Some(s) = app.settings.as_ref().filter(|_| guard_off) {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                " ⚠ Process guard is off: ".bold().fg(Color::Yellow),
                Span::raw(s.guard_detail.clone()),
            ]))
            .wrap(Wrap { trim: true }),
            warn,
        );
    }

    let rows: Vec<Row> = app
        .sessions
        .iter()
        .map(|s| {
            let dots: Vec<Span> = s.agents.values().map(|a| dot(a.status, app.tick)).collect();
            Row::new(vec![
                Cell::from(Span::styled(status_name(s.status), Style::new().fg(session_color(s.status)))),
                Cell::from(Line::from(vec![
                    Span::raw(s.title.clone()),
                    if s.dry_run { Span::styled("  dry run", Style::new().fg(DIM)) } else { Span::raw("") },
                ])),
                Cell::from(s.team_name.clone()),
                Cell::from(Line::from(dots)),
                Cell::from(s.total_turns.to_string()),
                Cell::from(s.created_at.with_timezone(&chrono::Local).format("%b %d %H:%M").to_string()),
            ])
        })
        .collect();
    let empty = rows.is_empty();
    let t = Table::new(
        rows,
        [
            Constraint::Length(11),
            Constraint::Fill(3),
            Constraint::Fill(1),
            Constraint::Length(12),
            Constraint::Length(5),
            Constraint::Length(12),
        ],
    )
    .header(Row::new(["Status", "Session", "Team", "Agents", "Turns", "Started"]).style(Style::new().fg(DIM)))
    .row_highlight_style(Style::new().bg(Color::Rgb(36, 48, 90)))
    .highlight_symbol("▶ ")
    .block(panel("Sessions"));
    let mut state = TableState::default().with_selected((!empty).then_some(app.sessions_sel));
    f.render_stateful_widget(t, table, &mut state);
    if empty {
        let inner = table.inner(ratatui::layout::Margin::new(2, 2));
        f.render_widget(
            Paragraph::new("No sessions yet. Press n to start one.").fg(DIM).alignment(Alignment::Center),
            inner,
        );
    }
}

// ---------------------------------------------------------------- new session

fn draw_new(f: &mut Frame, app: &App, area: Rect) {
    let w = area.width.min(90);
    let h = (app.teams.len() as u16 + 12).min(area.height);
    let form = Rect::new(area.x + (area.width - w) / 2, area.y + (area.height.saturating_sub(h)) / 3, w, h);
    f.render_widget(Clear, form);
    let block = panel("New session");
    let inner = block.inner(form);
    f.render_widget(block, form);

    let [teams_label, teams, task_label, task, dry, _, hint] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(app.teams.len().max(1) as u16),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(inner);

    f.render_widget(Line::from(" Team (↑↓)").fg(MUTED), teams_label);
    if app.teams.is_empty() {
        f.render_widget(Line::from("  loading teams…").fg(DIM), teams);
    } else {
        let items: Vec<ListItem> = app
            .teams
            .iter()
            .map(|t| {
                ListItem::new(Line::from(vec![
                    Span::raw(t.name.clone()),
                    Span::styled(format!("  {} personas", t.personas.len()), Style::new().fg(DIM)),
                ]))
            })
            .collect();
        let list = List::new(items).highlight_symbol(" ▶ ").highlight_style(Style::new().fg(ACCENT).bold());
        f.render_stateful_widget(list, teams, &mut ListState::default().with_selected(Some(app.new_team)));
    }

    f.render_widget(Line::from(" Task").fg(MUTED), task_label.offset(ratatui::layout::Offset { x: 0, y: 1 }));
    let field = Rect { x: task.x + 1, width: task.width.saturating_sub(2), ..task };
    let (visible, col) = app.new_task.view(field.width as usize);
    f.render_widget(Paragraph::new(visible).style(Style::new().bg(Color::Rgb(28, 34, 48))), field);
    f.set_cursor_position((field.x + col as u16, field.y));

    let check = if app.new_dry_run { "[x]" } else { "[ ]" };
    f.render_widget(
        Line::from(vec![
            Span::raw(format!(" {check} Dry run")),
            Span::styled("  (Ctrl+D) mock agents, no LLM calls", Style::new().fg(DIM)),
        ])
        .style(Style::new())
        .alignment(Alignment::Left),
        dry.offset(ratatui::layout::Offset { x: 0, y: 1 }),
    );
    f.render_widget(Line::from(" Enter start · Esc back · paste works for long tasks").fg(DIM), hint);
}

// ---------------------------------------------------------------- session

fn draw_session(f: &mut Frame, app: &App, area: Rect) {
    let Some(v) = &app.view else { return };
    let composing = v.composer.is_some();
    let [head, main, composer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(if composing { 3 } else { 0 }),
    ])
    .areas(area);

    // Header: title, status, team, turns.
    let s = &v.summary;
    let mut l1 = vec![
        " ◐ ".fg(ACCENT),
        Span::styled(s.title.clone(), Style::new().bold()),
        Span::raw("  "),
        Span::styled(format!(" {} ", status_name(s.status)), Style::new().fg(Color::Black).bg(session_color(s.status))),
    ];
    if let Some(d) = &s.status_detail {
        l1.push(Span::styled(format!("  {d}"), Style::new().fg(DIM)));
    }
    let l2 = Line::from(vec![
        Span::styled(format!("   {} · {} turns", s.team_name, s.total_turns), Style::new().fg(MUTED)),
        if s.dry_run { Span::styled(" · dry run", Style::new().fg(Color::Yellow)) } else { Span::raw("") },
        Span::styled(format!(" · {}", s.workdir), Style::new().fg(DIM)),
    ]);
    f.render_widget(Paragraph::new(vec![Line::from(l1), l2]), head);

    let agents_w = (main.width / 3).clamp(30, 48);
    let [agents, log] = Layout::horizontal([Constraint::Length(agents_w), Constraint::Min(20)]).areas(main);
    draw_agents(f, app, v, agents);
    draw_log(f, v, log);

    if let Some(c) = &v.composer {
        let targets = app.targets();
        let label = targets.get(c.target).map(|(_, l)| l.clone()).unwrap_or_default();
        let block = panel(format!("Message to {label}  (Tab: change recipient · Enter send · Esc cancel)"))
            .border_style(Style::new().fg(ACCENT));
        let inner = block.inner(composer);
        f.render_widget(block, composer);
        let (visible, col) = c.input.view(inner.width as usize);
        f.render_widget(Paragraph::new(visible), inner);
        f.set_cursor_position((inner.x + col as u16, inner.y));
    }
}

fn draw_agents(f: &mut Frame, app: &App, v: &SessionView, area: Rect) {
    let mut items = vec![ListItem::new(vec![
        Line::from(vec![Span::raw("☰ "), "Timeline".bold()]),
        Line::from("  messages between personas").fg(DIM),
    ])];
    for p in &v.team.personas {
        let st = v.agent_status.get(&p.id).copied().unwrap_or(AgentStatus::Idle);
        let turns = v.turns.get(&p.id).copied().unwrap_or(0);
        let mut l1 = vec![dot(st, app.tick), Span::raw(" "), Span::styled(p.name.clone(), Style::new().bold())];
        if p.id == v.team.entry {
            l1.push(Span::styled(" ENTRY", Style::new().fg(ACCENT)));
        }
        let model = if p.model.is_empty() { String::new() } else { format!("/{}", p.model) };
        let l2 = Line::from(vec![
            Span::styled(format!("  {} ", status_name(st)), Style::new().fg(agent_color(st))),
            Span::styled(format!("· {}{model} · {} · {turns}t", p.runtime, p.effort.as_str()), Style::new().fg(DIM)),
        ]);
        let recipients: Vec<String> = v.team.recipients_of(&p.id).iter().map(|r| r.id.clone()).collect();
        let l3 = Line::from(format!("  → {}", if recipients.is_empty() { "—".into() } else { recipients.join(", ") }))
            .fg(DIM);
        items.push(ListItem::new(vec![Line::from(l1), l2, l3]));
    }
    let list = List::new(items)
        .block(panel("Agents"))
        .highlight_style(Style::new().bg(Color::Rgb(36, 48, 90)))
        .highlight_symbol("▌");
    f.render_stateful_widget(list, area, &mut ListState::default().with_selected(Some(v.selected)));
}

fn draw_log(f: &mut Frame, v: &SessionView, area: Rect) {
    let who = v.selected_persona();
    let title = match who.and_then(|id| v.team.persona(id)) {
        Some(p) => format!("{} — log", p.name),
        None => "Timeline".to_string(),
    };
    let block = panel(title);
    let inner = block.inner(area);
    let lines = log_lines(v, who);
    let text = if lines.is_empty() {
        Text::from(Line::from(if who.is_some() { "No activity yet." } else { "No messages yet." }).fg(DIM))
    } else {
        Text::from(lines)
    };
    let para = Paragraph::new(text).wrap(Wrap { trim: false });
    let total = para.line_count(inner.width) as u16;
    let max_scroll = total.saturating_sub(inner.height);
    let from_top = max_scroll.saturating_sub(v.scroll.min(max_scroll));
    let title_suffix = if v.scroll > 0 && max_scroll > 0 { " ↑ scrolled — End to follow " } else { "" };
    f.render_widget(
        para.scroll((from_top, 0)).block(block.title_bottom(Line::from(title_suffix).fg(Color::Yellow))),
        area,
    );
}

fn name_of(v: &SessionView, id: &str) -> String {
    if id == "user" {
        return "You".into();
    }
    v.team.persona(id).map(|p| p.name.clone()).unwrap_or_else(|| id.to_string())
}

/// Lines for the timeline (`who = None`) or one persona's log.
fn log_lines<'a>(v: &'a SessionView, who: Option<&str>) -> Vec<Line<'a>> {
    let mut out = vec![];
    if who.is_none() {
        out.push(Line::from("Task").fg(MUTED).bold());
        block_lines(&mut out, &v.summary.task, Style::new(), 8);
    }
    for ev in &v.events {
        let relevant = match (who, &ev.body) {
            (
                None,
                EventBody::Message { .. }
                | EventBody::Final { .. }
                | EventBody::SessionStatus { .. }
                | EventBody::Decision { .. },
            ) => true,
            (None, _) => false,
            (Some(a), EventBody::Message { from, to, .. }) => from == a || to == a,
            (Some(_), EventBody::AgentStatus { .. }) => false,
            (Some(a), _) => ev.agent.as_deref() == Some(a),
        };
        if relevant {
            event_lines(v, who, ev, &mut out);
        }
    }
    out
}

fn time(ev: &Event) -> String {
    ev.ts.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()
}

fn block_lines<'a>(out: &mut Vec<Line<'a>>, text: &str, style: Style, max_lines: usize) {
    let all: Vec<&str> = text.lines().collect();
    for l in all.iter().take(max_lines) {
        out.push(Line::from(vec![Span::styled(" ▏", Style::new().fg(DIM)), Span::styled(l.to_string(), style)]));
    }
    if all.len() > max_lines {
        out.push(Line::from(format!(" ▏… {} more lines", all.len() - max_lines)).fg(DIM));
    }
}

fn event_lines<'a>(v: &'a SessionView, who: Option<&str>, ev: &'a Event, out: &mut Vec<Line<'a>>) {
    match &ev.body {
        EventBody::Message { from, to, content, rejected, deferred } => {
            let mut head = match who {
                Some(a) if to == a => vec!["← from ".fg(MUTED), Span::styled(name_of(v, from), Style::new().bold())],
                Some(_) => vec!["→ to ".fg(MUTED), Span::styled(name_of(v, to), Style::new().bold())],
                None => vec![
                    Span::styled(name_of(v, from), Style::new().bold()),
                    " → ".fg(MUTED),
                    Span::styled(name_of(v, to), Style::new().bold()),
                ],
            };
            if *rejected {
                head.push("  not delivered: no channel".fg(Color::Red));
            }
            if *deferred {
                head.push("  ⚡ deferred".fg(Color::Yellow));
            }
            head.push(Span::styled(format!("  {}", time(ev)), Style::new().fg(DIM)));
            out.push(Line::default());
            out.push(Line::from(head));
            block_lines(out, content, Style::new(), 30);
        }
        EventBody::Final { content } => {
            out.push(Line::default());
            out.push(
                Line::from(format!("✔ Final answer from {}", name_of(v, ev.agent.as_deref().unwrap_or("?"))))
                    .fg(Color::Green)
                    .bold(),
            );
            block_lines(out, content, Style::new(), 200);
        }
        EventBody::SessionStatus { status, detail } => out.push(
            Line::from(format!(
                "· session {}{} · {}",
                status_name(*status),
                detail.as_ref().map(|d| format!(" — {d}")).unwrap_or_default(),
                time(ev)
            ))
            .fg(DIM),
        ),
        EventBody::Decision { kind, subject, outcome, confidence, applied, latency_ms, error, .. } => {
            let mut spans = vec![
                "⚡ Jev ".fg(Color::Yellow).bold(),
                Span::styled(status_name(*kind).replace('_', " "), Style::new().bold()),
                Span::styled(format!(" · {subject}: "), Style::new().fg(MUTED)),
            ];
            match error {
                Some(e) => spans.push(Span::styled(format!("skipped ({e})"), Style::new().fg(Color::Red))),
                None => {
                    spans.push(Span::styled(outcome.clone(), Style::new().bold()));
                    if let Some(c) = confidence {
                        spans.push(Span::styled(format!(" ({:.0}%, {latency_ms}ms)", c * 100.0), Style::new().fg(DIM)));
                    }
                    if *applied {
                        spans.push(" applied".fg(Color::Yellow));
                    }
                }
            }
            out.push(Line::from(spans));
        }
        EventBody::TurnStart { turn } => {
            out.push(Line::default());
            out.push(Line::from(format!("──── turn #{turn} · {} ────", time(ev))).fg(DIM));
        }
        EventBody::TurnEnd { turn, duration_ms, error } => out.push(Line::from(vec![
            Span::styled(
                format!("turn #{turn} finished in {:.1}s", *duration_ms as f64 / 1000.0),
                Style::new().fg(DIM),
            ),
            if error.is_some() { " with error".fg(Color::Red) } else { Span::raw("") },
        ])),
        EventBody::Log { kind, text } => match kind {
            LogKind::Prompt => {
                if v.show_prompts {
                    out.push(Line::from("▾ prompt sent to the agent (p to hide)").fg(DIM));
                    block_lines(out, text, Style::new().fg(MUTED), usize::MAX);
                } else {
                    out.push(Line::from(format!("▸ prompt ({} chars) — p to show", text.chars().count())).fg(DIM));
                }
            }
            LogKind::Command => out.push(Line::from(format!("$ {text}")).fg(DIM)),
            LogKind::Text => out.extend(text.lines().map(|l| Line::from(l.to_string()))),
            LogKind::Thinking => out.extend(text.lines().take(12).map(|l| Line::from(l.to_string()).fg(DIM).italic())),
            LogKind::Tool => out.push(Line::from(format!("⚙ {text}")).fg(Color::Magenta)),
            LogKind::ToolResult => block_lines(out, text, Style::new().fg(DIM), 6),
            LogKind::Stderr => out.push(Line::from(text.to_string()).fg(DIM)),
            LogKind::Info => out.push(Line::from(text.to_string()).fg(Color::Cyan)),
            LogKind::Error => out.push(Line::from(format!("✖ {text}")).fg(Color::Red)),
        },
        EventBody::AgentStatus { .. } => {}
    }
}

// ---------------------------------------------------------------- footer

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    if let Some((text, err, _)) = &app.status {
        let style = if *err {
            Style::new().fg(Color::Black).bg(Color::Yellow)
        } else {
            Style::new().fg(Color::Black).bg(Color::Cyan)
        };
        f.render_widget(Line::from(format!(" {text} ")).style(style), area);
        return;
    }
    let keys: &[(&str, &str)] = match app.screen {
        Screen::Sessions => {
            &[("↑↓", "select"), ("Enter", "open"), ("n", "new session"), ("r", "refresh"), ("q", "quit")]
        }
        Screen::NewSession => &[("↑↓", "team"), ("Ctrl+D", "dry run"), ("Enter", "start"), ("Esc", "back")],
        Screen::Session => {
            if app.view.as_ref().is_some_and(|v| v.composer.is_some()) {
                &[("Tab", "recipient"), ("Enter", "send"), ("Esc", "cancel")]
            } else {
                &[
                    ("↑↓", "agent"),
                    ("PgUp/PgDn", "scroll"),
                    ("End", "follow"),
                    ("m", "message"),
                    ("p", "prompts"),
                    ("c", "cancel"),
                    ("Esc", "back"),
                ]
            }
        }
    };
    let mut spans = vec![];
    for (k, d) in keys {
        spans.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(MUTED)));
        spans.push(Span::styled(format!(" {d}  "), Style::new().fg(MUTED)));
    }
    f.render_widget(Line::from(spans), area);
}
