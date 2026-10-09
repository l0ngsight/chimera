//! File-backed storage for teams (`<data>/teams/<id>.yaml`) and UI-editable settings.

use std::path::PathBuf;

use anyhow::Context;

use serde::{Deserialize, Serialize};

use crate::config::PromptLlmConfig;
use crate::decisions::DecisionsConfig;
use crate::model::Team;

/// Teams shipped with chimera, written to the data dir on first start.
const SAMPLE_TEAMS: &[(&str, &str)] = &[("software-dev", include_str!("../teams/software-dev.yaml"))];

pub struct TeamStore {
    dir: PathBuf,
}

impl TeamStore {
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        let fresh = !dir.exists();
        std::fs::create_dir_all(&dir)?;
        let store = Self { dir };
        if fresh {
            for (id, yaml) in SAMPLE_TEAMS {
                std::fs::write(store.path(id), yaml)?;
            }
        }
        Ok(store)
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.yaml"))
    }

    pub fn list(&self) -> anyhow::Result<Vec<Team>> {
        let mut teams = vec![];
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            match std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|t| Ok(serde_yaml::from_str::<Team>(&t)?))
            {
                Ok(team) => teams.push(team),
                Err(e) => tracing::warn!("skipping {}: {e}", path.display()),
            }
        }
        teams.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(teams)
    }

    pub fn get(&self, id: &str) -> anyhow::Result<Option<Team>> {
        if !crate::model::is_valid_id(id) {
            return Ok(None);
        }
        let path = self.path(id);
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path)?;
        Ok(Some(serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?))
    }

    pub fn save(&self, team: &Team) -> anyhow::Result<()> {
        team.validate().map_err(anyhow::Error::msg)?;
        std::fs::write(self.path(&team.id), serde_yaml::to_string(team)?)?;
        Ok(())
    }

    pub fn delete(&self, id: &str) -> anyhow::Result<bool> {
        if !crate::model::is_valid_id(id) {
            return Ok(false);
        }
        let path = self.path(id);
        if !path.exists() {
            return Ok(false);
        }
        std::fs::remove_file(path)?;
        Ok(true)
    }
}

/// Settings editable from the UI, persisted to `<data>/settings.yaml`.
pub struct SettingsStore {
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub prompt_llm: PromptLlmConfig,
    /// `None` when written by a version without Jev support.
    #[serde(default)]
    pub decisions: Option<DecisionsConfig>,
    /// Process guard switch; `None` means the default (on).
    #[serde(default)]
    pub guard_enabled: Option<bool>,
}

/// Accepts the current format and the original one (a bare `PromptLlmConfig`).
#[derive(Deserialize)]
#[serde(untagged)]
enum StoredSettings {
    Current(Settings),
    Legacy(PromptLlmConfig),
}

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Option<Settings> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        match serde_yaml::from_str(&text) {
            Ok(StoredSettings::Current(s)) => Some(s),
            Ok(StoredSettings::Legacy(prompt_llm)) => {
                Some(Settings { prompt_llm, decisions: None, guard_enabled: None })
            }
            Err(e) => {
                tracing::warn!("ignoring unreadable {}: {e}", self.path.display());
                None
            }
        }
    }

    pub fn save(&self, settings: &Settings) -> anyhow::Result<()> {
        std::fs::write(&self.path, serde_yaml::to_string(settings)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn loads_legacy_and_current_settings() {
        let dir = std::env::temp_dir().join(format!("chimera-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = super::SettingsStore::new(dir.join("settings.yaml"));
        std::fs::write(dir.join("settings.yaml"), "api: openai\nbase_url: http://x\nmodel: m\napi_key_env: K\n")
            .unwrap();
        let s = store.load().unwrap();
        assert_eq!(s.prompt_llm.model, "m");
        assert!(s.decisions.is_none());
        store
            .save(&super::Settings {
                prompt_llm: s.prompt_llm,
                decisions: Some(Default::default()),
                guard_enabled: None,
            })
            .unwrap();
        assert_eq!(store.load().unwrap().decisions.unwrap().model, "jev-latest");
    }

    #[test]
    fn sample_teams_are_valid() {
        for (id, yaml) in super::SAMPLE_TEAMS {
            let team: crate::model::Team = serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(&team.id, id);
            team.validate().unwrap_or_else(|e| panic!("{id}: {e}"));
        }
    }
}
