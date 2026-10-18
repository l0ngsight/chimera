//! File-backed storage for teams (`<data>/teams/<id>.yaml`) and UI-editable settings.

use std::path::PathBuf;

use anyhow::Context;

use serde::{Deserialize, Serialize};

use crate::config::PromptLlmConfig;
use crate::decisions::DecisionsConfig;
use crate::model::Team;

/// Teams shipped with chimera. Each is written to the data dir once (also on existing installs
/// when a new sample ships); `.samples` remembers which were offered, so deleting one sticks.
pub const SAMPLE_TEAMS: &[(&str, &str)] = &[
    ("software-dev", include_str!("../teams/software-dev.yaml")),
    ("copilot-engineering-team", include_str!("../teams/copilot-engineering-team.yaml")),
];

/// Sample teams that were renamed: (old id, new id, old name, new name). An install that has the
/// old sample gets its copy moved to the new id, edits included, instead of a second copy.
const RENAMED_SAMPLES: &[(&str, &str, &str, &str)] = &[(
    "scb-software-engineering",
    "copilot-engineering-team",
    "SCB Software Engineering Team",
    "Copilot Engineering Team",
)];

pub struct TeamStore {
    dir: PathBuf,
}

impl TeamStore {
    pub fn open(dir: PathBuf) -> anyhow::Result<Self> {
        let fresh = !dir.exists();
        std::fs::create_dir_all(&dir)?;
        let store = Self { dir };
        let marker = store.dir.join(".samples");
        let mut offered: Vec<String> = match std::fs::read_to_string(&marker) {
            Ok(text) => text.lines().map(str::to_owned).collect(),
            // Installs from before the marker existed were given the first sample team.
            Err(_) if !fresh => vec!["software-dev".into()],
            Err(_) => vec![],
        };
        let mut changed = false;
        for (old, new, old_name, new_name) in RENAMED_SAMPLES {
            if !offered.iter().any(|o| o == old) || offered.iter().any(|o| o == new) {
                continue;
            }
            if let Ok(Some(mut team)) = store.get(old)
                && !store.path(new).exists()
            {
                team.id = new.to_string();
                if team.name == *old_name {
                    team.name = new_name.to_string();
                }
                store.save(&team)?;
                std::fs::remove_file(store.path(old))?;
            }
            offered.push(new.to_string());
            changed = true;
        }
        for (id, yaml) in SAMPLE_TEAMS {
            if offered.iter().any(|o| o == id) {
                continue;
            }
            if !store.path(id).exists() {
                std::fs::write(store.path(id), yaml)?;
            }
            offered.push(id.to_string());
            changed = true;
        }
        if changed || !marker.exists() {
            std::fs::write(&marker, offered.join("\n") + "\n")?;
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

    /// The Copilot team may only use Copilot CLI and its approved models.
    #[test]
    fn copilot_team_uses_only_approved_copilot_models() {
        const APPROVED: &[&str] =
            &["claude-sonnet-5", "gpt-5-mini", "gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini", "gpt-5.6-luna"];
        let (_, yaml) = super::SAMPLE_TEAMS.iter().find(|(id, _)| *id == "copilot-engineering-team").unwrap();
        let team: crate::model::Team = serde_yaml::from_str(yaml).unwrap();
        for p in &team.personas {
            for c in p.at_levels() {
                assert_eq!(c.runtime, "copilot", "{}", p.id);
                assert!(APPROVED.contains(&c.model.as_str()), "{}: {}", p.id, c.model);
            }
        }
    }

    #[test]
    fn renamed_sample_teams_keep_the_users_copy() {
        let dir = std::env::temp_dir().join(format!("chimera-teams-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // An install that was given the SCB sample and edited it.
        std::fs::write(dir.join(".samples"), "software-dev\nscb-software-engineering\n").unwrap();
        let (_, yaml) = super::SAMPLE_TEAMS.iter().find(|(id, _)| *id == "copilot-engineering-team").unwrap();
        let old = yaml
            .replacen("id: copilot-engineering-team", "id: scb-software-engineering", 1)
            .replacen("name: Copilot Engineering Team", "name: SCB Software Engineering Team", 1)
            .replacen("max_parallel: 3", "max_parallel: 2", 1);
        std::fs::write(dir.join("scb-software-engineering.yaml"), old).unwrap();

        let store = super::TeamStore::open(dir.clone()).unwrap();
        let teams = store.list().unwrap();
        assert_eq!(teams.len(), 1, "no duplicate: {:?}", teams.iter().map(|t| &t.id).collect::<Vec<_>>());
        assert_eq!(teams[0].id, "copilot-engineering-team");
        assert_eq!(teams[0].name, "Copilot Engineering Team");
        assert_eq!(teams[0].max_parallel, 2, "the user's edit is kept");
        assert!(!dir.join("scb-software-engineering.yaml").exists());
        // Stable on the next start.
        assert_eq!(super::TeamStore::open(dir).unwrap().list().unwrap().len(), 1);
    }

    #[test]
    fn new_sample_teams_reach_existing_installs_once() {
        let dir = std::env::temp_dir().join(format!("chimera-teams-{}", uuid::Uuid::new_v4()));
        // An install from before `.samples` existed, whose user deleted the first sample.
        std::fs::create_dir_all(&dir).unwrap();
        let store = super::TeamStore::open(dir.clone()).unwrap();
        let ids: Vec<String> = store.list().unwrap().into_iter().map(|t| t.id).collect();
        assert_eq!(ids, vec!["copilot-engineering-team"]);
        // Deleting a sample sticks across restarts.
        std::fs::remove_file(dir.join("copilot-engineering-team.yaml")).unwrap();
        assert!(super::TeamStore::open(dir.clone()).unwrap().list().unwrap().is_empty());
        // A fresh install gets every sample.
        let fresh = dir.join("fresh");
        assert_eq!(super::TeamStore::open(fresh).unwrap().list().unwrap().len(), super::SAMPLE_TEAMS.len());
    }
}
