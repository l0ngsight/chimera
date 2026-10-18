//! HTTP API, server-sent events and the embedded web UI.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

use crate::config::{ModelsSource, PromptLlmConfig, RuntimeSpec};
use crate::decisions::DecisionsConfig;
use crate::model::Team;
use crate::orchestrator::{NewSession, Orchestrator};
use crate::promptgen;
use crate::session::Session;
use crate::store::{Settings, SettingsStore, TeamStore};

pub struct AppState {
    pub teams: TeamStore,
    pub settings: SettingsStore,
    pub prompt_llm: RwLock<PromptLlmConfig>,
    pub orchestrator: Arc<Orchestrator>,
    pub catalog: crate::catalog::Catalog,
    pub sessions: RwLock<BTreeMap<String, Arc<Session>>>,
    /// Flips to true when the server shuts down; ends open event streams.
    pub shutdown: tokio::sync::watch::Sender<bool>,
}

impl AppState {
    /// Stops the server's work: closes event streams and interrupts running sessions (their
    /// agent processes are stopped; a message resumes them after a restart). Waits up to 5 s.
    pub async fn shutdown(&self) {
        let _ = self.shutdown.send(true);
        let sessions: Vec<Arc<Session>> = self.sessions.read().unwrap().values().cloned().collect();
        for s in &sessions {
            self.orchestrator.interrupt(s);
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while sessions.iter().any(|s| s.lock().scheduler_active) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

type AppResult<T> = Result<T, ApiError>;

pub struct ApiError(StatusCode, String);

impl ApiError {
    fn bad(msg: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, msg.into())
    }
    fn not_found(what: &str) -> Self {
        Self(StatusCode::NOT_FOUND, format!("{what} not found"))
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
        .route("/api/runtimes", get(list_runtimes))
        .route("/api/runtimes/{name}/models", get(runtime_models))
        .route("/api/teams", get(list_teams).post(create_team))
        .route("/api/teams/{id}", get(get_team).put(update_team).delete(delete_team))
        .route("/api/personas/generate", post(generate_persona))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/decisions/test", post(test_decisions))
        .route("/api/limits", get(list_limits).delete(clear_limits))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route("/api/sessions/{id}", get(get_session).delete(delete_session))
        .route("/api/sessions/{id}/events", get(session_events))
        .route("/api/sessions/{id}/stream", get(session_stream))
        .route("/api/sessions/{id}/messages", post(post_message))
        .route("/api/sessions/{id}/cancel", post(cancel_session))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn app_js() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8")], include_str!("../web/app.js"))
}

async fn app_css() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], include_str!("../web/app.css"))
}

#[derive(Serialize)]
struct RuntimeInfo {
    name: String,
    label: String,
    command: String,
    available: bool,
    models: Vec<String>,
    /// How chimera efforts translate to this CLI's values ("" = flag omitted).
    effort_map: BTreeMap<String, String>,
    /// Where the model list comes from, for display.
    models_via: String,
}

async fn list_runtimes(State(st): State<Arc<AppState>>) -> Json<Vec<RuntimeInfo>> {
    let infos = st
        .orchestrator
        .runtimes
        .iter()
        .map(|(name, spec): (&String, &RuntimeSpec)| RuntimeInfo {
            name: name.clone(),
            label: spec.label.clone(),
            command: spec.command.clone(),
            available: spec.kind == crate::config::RuntimeKind::Mock || command_exists(&spec.command),
            models: spec.models.clone(),
            effort_map: spec.effort_map.clone(),
            models_via: match &spec.models_source {
                ModelsSource::Static => "chimera.yaml".into(),
                ModelsSource::Command { args, .. } => format!("{} {}", spec.command, args.join(" ")),
                ModelsSource::AnthropicApi { api_key_env } => format!("aliases + Anthropic API (${api_key_env})"),
            },
        })
        .collect();
    Json(infos)
}

#[derive(Deserialize)]
struct ModelsQuery {
    #[serde(default)]
    refresh: bool,
}

/// Models the runtime can use, asked from its CLI (or API) and cached for a few minutes.
async fn runtime_models(
    State(st): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(q): Query<ModelsQuery>,
) -> AppResult<Json<crate::catalog::ModelList>> {
    let spec = st.orchestrator.runtimes.get(&name).ok_or_else(|| ApiError::not_found("runtime"))?;
    Ok(Json(st.catalog.list(&name, spec, q.refresh).await))
}

fn command_exists(cmd: &str) -> bool {
    if cmd.is_empty() {
        return false;
    }
    let p = std::path::Path::new(cmd);
    if p.components().count() > 1 {
        return p.is_file();
    }
    std::env::var_os("PATH").map(|paths| std::env::split_paths(&paths).any(|d| d.join(cmd).is_file())).unwrap_or(false)
}

async fn list_teams(State(st): State<Arc<AppState>>) -> AppResult<Json<Vec<Team>>> {
    Ok(Json(st.teams.list()?))
}

async fn get_team(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<Json<Team>> {
    st.teams.get(&id)?.map(Json).ok_or_else(|| ApiError::not_found("team"))
}

async fn create_team(State(st): State<Arc<AppState>>, Json(team): Json<Team>) -> AppResult<Json<Team>> {
    team.validate().map_err(ApiError::bad)?;
    if st.teams.get(&team.id)?.is_some() {
        return Err(ApiError(StatusCode::CONFLICT, format!("team '{}' already exists", team.id)));
    }
    st.teams.save(&team)?;
    Ok(Json(team))
}

#[derive(Deserialize)]
struct UpdateTeamQuery {
    /// Also apply the edit to existing sessions of this team (from each persona's next turn).
    #[serde(default = "yes")]
    apply_to_sessions: bool,
}

fn yes() -> bool {
    true
}

async fn update_team(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<UpdateTeamQuery>,
    Json(team): Json<Team>,
) -> AppResult<Json<serde_json::Value>> {
    if team.id != id {
        return Err(ApiError::bad("team id in body does not match the URL"));
    }
    team.validate().map_err(ApiError::bad)?;
    st.teams.save(&team)?;
    let mut updated = 0;
    if q.apply_to_sessions {
        let sessions: Vec<Arc<Session>> = st.sessions.read().unwrap().values().cloned().collect();
        for s in sessions.iter().filter(|s| s.lock().meta.team.id == id) {
            if !st.orchestrator.apply_team(s, &team).is_empty() {
                updated += 1;
            }
        }
    }
    let mut body = serde_json::to_value(&team).map_err(anyhow::Error::from)?;
    body["sessions_updated"] = json!(updated);
    Ok(Json(body))
}

async fn delete_team(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<StatusCode> {
    if st.teams.delete(&id)? { Ok(StatusCode::NO_CONTENT) } else { Err(ApiError::not_found("team")) }
}

async fn generate_persona(
    State(st): State<Arc<AppState>>,
    Json(req): Json<promptgen::GenerateRequest>,
) -> AppResult<Json<serde_json::Value>> {
    let cfg = st.prompt_llm.read().unwrap().clone();
    let prompt =
        promptgen::generate(&cfg, &req).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({ "prompt": prompt })))
}

#[derive(Serialize)]
struct SettingsView {
    prompt_llm: PromptLlmConfig,
    api_key_present: bool,
    decisions: DecisionsConfig,
    /// Name of the environment variable the Jev key is read from (never the key itself).
    decisions_key_env: Option<String>,
    decisions_active: bool,
    guard: crate::guard::GuardStatus,
}

async fn get_settings(State(st): State<Arc<AppState>>) -> Json<SettingsView> {
    let cfg = st.prompt_llm.read().unwrap().clone();
    let api_key_present = std::env::var(&cfg.api_key_env).map(|v| !v.is_empty()).unwrap_or(false);
    let decisions = st.orchestrator.decisions.read().unwrap().clone();
    Json(SettingsView {
        prompt_llm: cfg,
        api_key_present,
        decisions_key_env: decisions.api_key().map(|(_, var)| var),
        decisions_active: decisions.active(),
        guard: st.orchestrator.guard.status(),
        decisions,
    })
}

#[derive(Deserialize)]
struct SettingsUpdate {
    #[serde(default)]
    prompt_llm: Option<PromptLlmConfig>,
    #[serde(default)]
    decisions: Option<DecisionsConfig>,
    #[serde(default)]
    guard_enabled: Option<bool>,
}

async fn put_settings(
    State(st): State<Arc<AppState>>,
    Json(update): Json<SettingsUpdate>,
) -> AppResult<Json<SettingsView>> {
    if let Some(cfg) = &update.prompt_llm
        && (cfg.base_url.trim().is_empty() || cfg.model.trim().is_empty() || cfg.api_key_env.trim().is_empty())
    {
        return Err(ApiError::bad("base_url, model and api_key_env are required"));
    }
    if let Some(d) = &update.decisions
        && (d.base_url.trim().is_empty() || d.model.trim().is_empty())
    {
        return Err(ApiError::bad("Jev base_url and model are required"));
    }
    let settings = Settings {
        prompt_llm: update.prompt_llm.unwrap_or_else(|| st.prompt_llm.read().unwrap().clone()),
        decisions: Some(update.decisions.unwrap_or_else(|| st.orchestrator.decisions.read().unwrap().clone())),
        guard_enabled: Some(update.guard_enabled.unwrap_or_else(|| st.orchestrator.guard.status().enabled)),
    };
    st.settings.save(&settings)?;
    *st.prompt_llm.write().unwrap() = settings.prompt_llm;
    *st.orchestrator.decisions.write().unwrap() = settings.decisions.unwrap_or_default();
    st.orchestrator.guard.set_enabled(settings.guard_enabled.unwrap_or(true));
    Ok(get_settings(State(st)).await)
}

/// Sends a tiny request to Jev to check the configuration.
async fn test_decisions(State(st): State<Arc<AppState>>) -> AppResult<Json<serde_json::Value>> {
    let cfg = st.orchestrator.decisions.read().unwrap().clone();
    let v = st.orchestrator.jev.ping(&cfg).await.map_err(|e| ApiError(StatusCode::BAD_GATEWAY, format!("{e:#}")))?;
    Ok(Json(json!({ "model": v.model, "latency_ms": v.latency_ms, "cost": v.cost, "answer": v.value })))
}

/// Runtime/model pairs currently known to be rate limited (personas use their fallbacks).
async fn list_limits(State(st): State<Arc<AppState>>) -> Json<Vec<crate::limits::LimitEntry>> {
    Json(st.orchestrator.limits.list(chrono::Utc::now()))
}

#[derive(Deserialize)]
struct ClearLimitsQuery {
    runtime: Option<String>,
}

/// Forgets limits so personas try their primary runtime again on their next turn.
async fn clear_limits(State(st): State<Arc<AppState>>, Query(q): Query<ClearLimitsQuery>) -> StatusCode {
    st.orchestrator.limits.clear(q.runtime.as_deref());
    StatusCode::NO_CONTENT
}

async fn list_sessions(State(st): State<Arc<AppState>>) -> Json<Vec<crate::session::SessionSummary>> {
    let mut list: Vec<_> = st.sessions.read().unwrap().values().map(|s| s.summary()).collect();
    list.sort_by_key(|s| std::cmp::Reverse(s.created_at));
    Json(list)
}

#[derive(Deserialize)]
struct CreateSession {
    team_id: String,
    task: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

async fn create_session(
    State(st): State<Arc<AppState>>,
    Json(req): Json<CreateSession>,
) -> AppResult<Json<crate::session::SessionSummary>> {
    let team = st.teams.get(&req.team_id)?.ok_or_else(|| ApiError::not_found("team"))?;
    let workdir = req.workdir.filter(|w| !w.trim().is_empty()).map(|w| PathBuf::from(shellexpand(&w)));
    let session = st
        .orchestrator
        .create_session(NewSession { team, task: req.task, title: req.title, workdir, dry_run: req.dry_run })
        .map_err(|e| ApiError::bad(format!("{e:#}")))?;
    let summary = session.summary();
    st.sessions.write().unwrap().insert(session.id.clone(), session);
    Ok(Json(summary))
}

fn shellexpand(p: &str) -> String {
    match (p.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => format!("{home}/{rest}"),
        _ => p.to_string(),
    }
}

fn find_session(st: &AppState, id: &str) -> AppResult<Arc<Session>> {
    st.sessions.read().unwrap().get(id).cloned().ok_or_else(|| ApiError::not_found("session"))
}

async fn get_session(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<Json<serde_json::Value>> {
    let session = find_session(&st, &id)?;
    let inner = session.lock();
    Ok(Json(json!({ "summary": inner.summary(), "team": inner.meta.team })))
}

async fn delete_session(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<StatusCode> {
    let session = find_session(&st, &id)?;
    if session.lock().scheduler_active {
        return Err(ApiError(StatusCode::CONFLICT, "session is running; cancel it first".into()));
    }
    st.sessions.write().unwrap().remove(&id);
    std::fs::remove_dir_all(&session.dir).map_err(anyhow::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct EventsQuery {
    agent: Option<String>,
}

async fn session_events(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<EventsQuery>,
) -> AppResult<Json<Vec<crate::session::Event>>> {
    let session = find_session(&st, &id)?;
    Ok(Json(session.events_for(q.agent.as_deref())))
}

/// Server-sent events: replays the whole history, then streams live events.
async fn session_stream(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> AppResult<Sse<impl Stream<Item = Result<SseEvent, Infallible>>>> {
    let session = find_session(&st, &id)?;
    let (history, rx) = session.subscribe();
    let last_seq = history.last().map(|e| e.seq).unwrap_or(0);
    let replay = tokio_stream::iter(history);
    let live = BroadcastStream::new(rx).filter_map(move |r| r.ok().filter(|e| e.seq > last_seq));
    let stream =
        replay.chain(live).map(|e| Ok(SseEvent::default().data(serde_json::to_string(&e).unwrap_or_default())));
    // End the stream when the server shuts down, otherwise graceful shutdown waits on it forever.
    let mut stop = st.shutdown.subscribe();
    let stream = futures::StreamExt::take_until(stream, async move {
        let _ = stop.wait_for(|down| *down).await;
    });
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

#[derive(Deserialize)]
struct PostMessage {
    #[serde(default)]
    to: Option<String>,
    content: String,
}

async fn post_message(
    State(st): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<PostMessage>,
) -> AppResult<StatusCode> {
    let session = find_session(&st, &id)?;
    if req.content.trim().is_empty() {
        return Err(ApiError::bad("message must not be empty"));
    }
    let to = match req.to.filter(|t| !t.is_empty()) {
        Some(t) if t == "auto" => st.orchestrator.route_user_message(&session, &req.content).await,
        Some(t) => t,
        None => session.lock().meta.team.entry.clone(),
    };
    st.orchestrator.post_message(&session, &to, &req.content).map_err(|e| ApiError::bad(format!("{e:#}")))?;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel_session(State(st): State<Arc<AppState>>, Path(id): Path<String>) -> AppResult<StatusCode> {
    let session = find_session(&st, &id)?;
    st.orchestrator.cancel(&session);
    Ok(StatusCode::ACCEPTED)
}
