//! File-backed storage for teams (`<data>/teams/<id>.yaml`) and UI-editable settings.

use std::path::PathBuf;

use anyhow::Context;

use crate::config::PromptLlmConfig;
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

impl SettingsStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn load(&self) -> Option<PromptLlmConfig> {
        let text = std::fs::read_to_string(&self.path).ok()?;
        serde_yaml::from_str(&text).ok()
    }

    pub fn save(&self, cfg: &PromptLlmConfig) -> anyhow::Result<()> {
        std::fs::write(&self.path, serde_yaml::to_string(cfg)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sample_teams_are_valid() {
        for (id, yaml) in super::SAMPLE_TEAMS {
            let team: crate::model::Team = serde_yaml::from_str(yaml).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert_eq!(&team.id, id);
            team.validate().unwrap_or_else(|e| panic!("{id}: {e}"));
        }
    }
}
