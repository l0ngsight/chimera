mod access;
mod catalog;
mod config;
mod decisions;
mod guard;
mod limits;
mod model;
mod orchestrator;
mod promptgen;
mod runtime;
mod session;
mod store;
mod tui;
mod web;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use crate::config::Config;
use crate::orchestrator::Orchestrator;
use crate::session::Session;
use crate::store::{SettingsStore, TeamStore};
use crate::web::AppState;

/// Chimera: multi-agent orchestration across Claude Code, Codex, opencode, Hermes and pi.
#[derive(Parser)]
#[command(name = "chimera", version)]
struct Cli {
    #[command(flatten)]
    opts: ServerOpts,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Args, Clone)]
struct ServerOpts {
    /// Path to the configuration file (default: ./chimera.yaml when present).
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// Address to listen on, overrides the config file.
    #[arg(short, long, global = true)]
    bind: Option<String>,
    /// Data directory for teams, sessions and workspaces, overrides the config file.
    #[arg(short, long, global = true)]
    data_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the server with the web UI (the default).
    Serve,
    /// Terminal UI. Connects to a running server, or starts one inside this process.
    Tui {
        /// Server to connect to (default: http://<bind>). With an explicit URL no server is started.
        #[arg(long)]
        url: Option<String>,
        /// Access token of the server (default: $CHIMERA_TOKEN).
        #[arg(long)]
        token: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let cfg = load_config(&cli.opts)?;
    match cli.command {
        None | Some(Command::Serve) => {
            init_tracing(None);
            let listener = bind(&cfg).await?;
            let state = build_state(&cfg).await?;
            tracing::info!(
                "chimera listening on http://{}  (data: {})",
                listener.local_addr()?,
                cfg.data_dir.display()
            );
            if let Some(token) = state.access.token() {
                let addr = listener.local_addr()?;
                let host = if addr.ip().is_unspecified() { "<this-host>".to_string() } else { addr.ip().to_string() };
                tracing::info!("access token required; open http://{host}:{}/?token={token}", addr.port());
            }
            let stopping = state.clone();
            axum::serve(listener, web::router(state))
                .with_graceful_shutdown(async move {
                    let signal = stop_signal().await;
                    tracing::info!("{signal}: stopping agents and saving sessions (Ctrl+C again to force)");
                    force_exit_watchdog();
                    stopping.shutdown().await;
                })
                .await?;
            tracing::info!("chimera stopped");
            // Exit right away: nothing left to do, and dropping the runtime could wait on
            // blocking work (DNS lookups, file I/O) that no longer matters.
            std::process::exit(0);
        }
        Some(Command::Tui { url, token }) => {
            std::fs::create_dir_all(&cfg.data_dir)?;
            // The terminal belongs to the TUI: log to a file instead.
            let log_path = cfg.data_dir.join("chimera.log");
            init_tracing(Some(&log_path));
            let explicit = url.is_some();
            let mut server = None;
            let token = token.or_else(|| std::env::var(access::TOKEN_ENV).ok());
            let base = url.unwrap_or_else(|| format!("http://{}", cfg.bind));
            let mut client = tui::Client::new(&base, token.clone());
            let embedded = if client.ping().await {
                false
            } else if explicit {
                anyhow::bail!("no Chimera server at {}", client.base());
            } else {
                let listener = bind(&cfg).await?;
                let state = build_state(&cfg).await?;
                // The embedded server may require a token (non-loopback bind): use it.
                if token.is_none() && state.access.token().is_some() {
                    client = tui::Client::new(&base, state.access.token().map(str::to_owned));
                }
                server = Some(state.clone());
                tokio::spawn(async move { axum::serve(listener, web::router(state)).await });
                true
            };
            let result = tui::run(client, embedded).await;
            if let Some(state) = server {
                // Stop agent processes and save sessions before the process exits.
                force_exit_watchdog();
                state.shutdown().await;
            }
            if embedded {
                println!("Chimera stopped. Log: {}", log_path.display());
            }
            if let Err(e) = result {
                eprintln!("Error: {e:#}");
                std::process::exit(1);
            }
            std::process::exit(0);
        }
    }
}

fn load_config(opts: &ServerOpts) -> anyhow::Result<Config> {
    let config_path = opts.config.clone().or_else(|| {
        let p = PathBuf::from("chimera.yaml");
        p.exists().then_some(p)
    });
    let mut cfg = Config::load(config_path.as_deref())?;
    if let Some(b) = &opts.bind {
        cfg.bind = b.clone();
    }
    if let Some(d) = &opts.data_dir {
        cfg.data_dir = d.clone();
    }
    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    cfg.data_dir = cfg.data_dir.canonicalize()?;
    Ok(cfg)
}

fn init_tracing(file: Option<&std::path::Path>) {
    let filter =
        tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "chimera=info,tower_http=warn".into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match file.and_then(|p| std::fs::OpenOptions::new().create(true).append(true).open(p).ok()) {
        Some(f) => builder.with_ansi(false).with_writer(std::sync::Mutex::new(f)).init(),
        None => builder.init(),
    }
}

async fn bind(cfg: &Config) -> anyhow::Result<tokio::net::TcpListener> {
    tokio::net::TcpListener::bind(&cfg.bind).await.with_context(|| format!("binding {}", cfg.bind))
}

/// Loads teams, settings and sessions and verifies the process guard.
async fn build_state(cfg: &Config) -> anyhow::Result<Arc<AppState>> {
    let data_dir = &cfg.data_dir;
    let teams = TeamStore::open(data_dir.join("teams"))?;
    let settings = SettingsStore::new(data_dir.join("settings.yaml"));
    let saved = settings.load();
    let prompt_llm = saved.as_ref().map(|s| s.prompt_llm.clone()).unwrap_or_else(|| cfg.prompt_llm.clone());
    let guard_enabled = saved.as_ref().and_then(|s| s.guard_enabled).unwrap_or(true);
    let decisions_cfg = saved.and_then(|s| s.decisions).unwrap_or_else(|| cfg.decisions.clone());

    let sessions_dir = data_dir.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    let mut sessions = BTreeMap::new();
    for entry in std::fs::read_dir(&sessions_dir)? {
        let dir = entry?.path();
        match Session::load(&dir) {
            Ok(s) => {
                sessions.insert(s.id.clone(), s);
            }
            Err(e) => tracing::warn!("skipping session {}: {e:#}", dir.display()),
        }
    }

    let orchestrator = Arc::new(Orchestrator {
        runtimes: cfg.runtimes.clone(),
        sessions_dir,
        workspaces_dir: data_dir.join("workspaces"),
        decisions: RwLock::new(decisions_cfg),
        jev: decisions::Jev::new(),
        guard: guard::Guard::new(guard_enabled),
        limits: Default::default(),
    });
    orchestrator.guard.self_test().await;
    let gs = orchestrator.guard.status();
    if gs.available && gs.enabled {
        tracing::info!("process guard on: {}", gs.detail);
    } else {
        tracing::warn!("process guard OFF ({}): agents could kill or quit your other programs", gs.detail);
    }
    Ok(Arc::new(AppState {
        teams,
        settings,
        prompt_llm: RwLock::new(prompt_llm),
        orchestrator,
        sessions: RwLock::new(sessions),
        shutdown: tokio::sync::watch::channel(false).0,
        catalog: catalog::Catalog::default(),
        access: access::Access::new(
            &cfg.bind,
            std::env::var(access::TOKEN_ENV).ok().or_else(|| cfg.auth_token.clone()),
            &cfg.allowed_hosts,
            data_dir,
        )?,
    }))
}

/// Waits for Ctrl+C, `kill` (SIGTERM) or the terminal closing (SIGHUP) and names it.
async fn stop_signal() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).ok();
        let mut hup = signal(SignalKind::hangup()).ok();
        async fn recv(s: Option<&mut tokio::signal::unix::Signal>) {
            match s {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending().await,
            }
        }
        tokio::select! {
            _ = tokio::signal::ctrl_c() => "Ctrl+C",
            _ = recv(term.as_mut()) => "SIGTERM",
            _ = recv(hup.as_mut()) => "SIGHUP",
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "Ctrl+C"
    }
}

/// Once shutdown starts, guarantees the process ends: a second Ctrl+C exits at once, and after
/// 8 seconds it exits regardless. Runs on its own OS thread so it works even if the async
/// runtime is stuck.
fn force_exit_watchdog() {
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        eprintln!("forced exit");
        std::process::exit(130);
    });
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(8));
        eprintln!("shutdown took too long; exiting");
        std::process::exit(130);
    });
}
