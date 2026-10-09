mod catalog;
mod config;
mod decisions;
mod model;
mod orchestrator;
mod promptgen;
mod runtime;
mod session;
mod store;
mod web;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::Context;
use clap::Parser;

use crate::config::Config;
use crate::orchestrator::Orchestrator;
use crate::session::Session;
use crate::store::{SettingsStore, TeamStore};
use crate::web::AppState;

/// Chimera: multi-agent orchestration across Claude Code, Codex, opencode and Hermes.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to the configuration file (default: ./chimera.yaml when present).
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Address to listen on, overrides the config file.
    #[arg(short, long)]
    bind: Option<String>,
    /// Data directory for teams, sessions and workspaces, overrides the config file.
    #[arg(short, long)]
    data_dir: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "chimera=info,tower_http=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let config_path = cli.config.or_else(|| {
        let p = PathBuf::from("chimera.yaml");
        p.exists().then_some(p)
    });
    let mut cfg = Config::load(config_path.as_deref())?;
    if let Some(b) = cli.bind {
        cfg.bind = b;
    }
    if let Some(d) = cli.data_dir {
        cfg.data_dir = d;
    }

    std::fs::create_dir_all(&cfg.data_dir).with_context(|| format!("creating {}", cfg.data_dir.display()))?;
    let data_dir = cfg.data_dir.canonicalize()?;
    let teams = TeamStore::open(data_dir.join("teams"))?;
    let settings = SettingsStore::new(data_dir.join("settings.yaml"));
    let saved = settings.load();
    let prompt_llm = saved.as_ref().map(|s| s.prompt_llm.clone()).unwrap_or_else(|| cfg.prompt_llm.clone());
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
    });
    let state = Arc::new(AppState {
        teams,
        settings,
        prompt_llm: RwLock::new(prompt_llm),
        orchestrator,
        sessions: RwLock::new(sessions),
        catalog: catalog::Catalog::default(),
    });

    let app = web::router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.bind).await.with_context(|| format!("binding {}", cfg.bind))?;
    tracing::info!("chimera listening on http://{}  (data: {})", listener.local_addr()?, data_dir.display());
    axum::serve(listener, app).with_graceful_shutdown(shutdown()).await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
}
