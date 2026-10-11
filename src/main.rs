mod catalog;
mod config;
mod decisions;
mod guard;
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
#[command(version)]
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
            let stopping = state.clone();
            axum::serve(listener, web::router(state))
                .with_graceful_shutdown(async move {
                    ctrl_c().await;
                    force_exit_on_second_signal();
                    stopping.shutdown().await;
                })
                .await?;
        }
        Some(Command::Tui { url }) => {
            std::fs::create_dir_all(&cfg.data_dir)?;
            // The terminal belongs to the TUI: log to a file instead.
            let log_path = cfg.data_dir.join("chimera.log");
            init_tracing(Some(&log_path));
            let explicit = url.is_some();
            let mut server = None;
            let client = tui::Client::new(&url.unwrap_or_else(|| format!("http://{}", cfg.bind)));
            let embedded = if client.ping().await {
                false
            } else if explicit {
                anyhow::bail!("no Chimera server at {}", client.base());
            } else {
                let listener = bind(&cfg).await?;
                let state = build_state(&cfg).await?;
                server = Some(state.clone());
                tokio::spawn(async move { axum::serve(listener, web::router(state)).await });
                true
            };
            tui::run(client, embedded).await?;
            if let Some(state) = server {
                // Stop agent processes and save sessions before the process exits.
                state.shutdown().await;
            }
            if embedded {
                println!("Chimera stopped. Log: {}", log_path.display());
            }
        }
    }
    Ok(())
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
    }))
}

async fn ctrl_c() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down (press Ctrl+C again to force)");
}

/// After the first Ctrl+C: a second one, or 10 seconds without finishing, exits immediately.
fn force_exit_on_second_signal() {
    tokio::spawn(async {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::warn!("forced exit"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => tracing::warn!("shutdown took too long; exiting"),
        }
        std::process::exit(130);
    });
}
