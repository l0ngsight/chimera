//! Core domain types: teams, personas and the communication graph.

use serde::{Deserialize, Serialize};

/// A team of expert personas plus the directed graph describing who may talk to whom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Team {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Persona that receives the user's task and is allowed to declare it finished.
    pub entry: String,
    pub personas: Vec<Persona>,
    #[serde(default)]
    pub edges: Vec<Edge>,
    /// Upper bound on agent turns per session (guards against runaway loops).
    #[serde(default = "default_max_turns")]
    pub max_turns: u32,
    /// How many agents may run at the same time inside one session.
    #[serde(default = "default_max_parallel")]
    pub max_parallel: usize,
}

fn default_max_turns() -> u32 {
    60
}

fn default_max_parallel() -> usize {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Persona {
    /// Stable identifier used in the message protocol (`<message to="id">`).
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub role: String,
    /// Name of a runtime defined in the configuration (claude, codex, opencode, hermes, mock, ...).
    pub runtime: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: Effort,
    /// The persona's system prompt: expertise, responsibilities, working style.
    #[serde(default)]
    pub prompt: String,
    /// Extra CLI arguments appended verbatim for this persona.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
    /// Extra environment variables for this persona's process.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub env: std::collections::BTreeMap<String, String>,
    /// Alternatives tried in order when the current runtime hits a usage, session or rate
    /// limit (e.g. Claude Code's "usage limit reached"). The persona returns to its primary
    /// runtime once the limit resets.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<Fallback>,
    /// Position in the graph editor; auto-laid-out when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<Position>,
}

/// A harness, model and effort a persona switches to when its runtime is rate limited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fallback {
    pub runtime: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: Effort,
    /// Replaces the persona's `extra_args` while the fallback is active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_args: Vec<String>,
}

impl Persona {
    /// The persona as it runs at a fallback level: 0 is the primary configuration, `n` is
    /// `fallbacks[n - 1]`. Levels past the end resolve to the primary.
    pub fn at_level(&self, level: usize) -> Persona {
        let mut p = self.clone();
        if let Some(f) = level.checked_sub(1).and_then(|i| self.fallbacks.get(i)) {
            p.runtime = f.runtime.clone();
            p.model = f.model.clone();
            p.effort = f.effort;
            p.extra_args = f.extra_args.clone();
        }
        p
    }

    /// The primary configuration followed by each fallback.
    pub fn at_levels(&self) -> impl Iterator<Item = Persona> + '_ {
        (0..self.levels()).map(|l| self.at_level(l))
    }

    /// Number of configurations (primary plus fallbacks).
    pub fn levels(&self) -> usize {
        1 + self.fallbacks.len()
    }

    /// "runtime · model · effort", for timelines and status lines.
    pub fn runtime_label(&self) -> String {
        let model = if self.model.is_empty() { "default model" } else { &self.model };
        format!("{} · {} · {}", self.runtime, model, self.effort.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Position {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    Xhigh,
    Max,
    /// Above `max`; offered by some Codex models.
    Ultra,
}

impl Effort {
    /// Every level, lowest first.
    pub const ALL: [Effort; 7] =
        [Effort::Minimal, Effort::Low, Effort::Medium, Effort::High, Effort::Xhigh, Effort::Max, Effort::Ultra];

    pub fn parse(s: &str) -> Option<Effort> {
        Effort::ALL.into_iter().find(|e| e.as_str() == s)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
            Effort::Ultra => "ultra",
        }
    }
}

/// A directed communication channel. `bidirectional` also allows `to -> from`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    #[serde(default)]
    pub bidirectional: bool,
}

impl Team {
    pub fn persona(&self, id: &str) -> Option<&Persona> {
        self.personas.iter().find(|p| p.id == id)
    }

    /// Whether `from` is allowed to send a message to `to`.
    pub fn can_send(&self, from: &str, to: &str) -> bool {
        self.edges.iter().any(|e| (e.from == from && e.to == to) || (e.bidirectional && e.to == from && e.from == to))
    }

    /// Persona ids `from` may message, in persona order.
    pub fn recipients_of(&self, from: &str) -> Vec<&Persona> {
        self.personas.iter().filter(|p| p.id != from && self.can_send(from, &p.id)).collect()
    }

    /// Checks structural invariants; returns a human readable error.
    pub fn validate(&self) -> Result<(), String> {
        if !is_valid_id(&self.id) {
            return Err(format!("team id '{}' must be non-empty and contain only [a-z0-9_-]", self.id));
        }
        if self.personas.is_empty() {
            return Err("team needs at least one persona".into());
        }
        let mut seen = std::collections::HashSet::new();
        for p in &self.personas {
            if !is_valid_id(&p.id) {
                return Err(format!("persona id '{}' must be non-empty and contain only [a-z0-9_-]", p.id));
            }
            if p.id == "user" {
                return Err("persona id 'user' is reserved".into());
            }
            if !seen.insert(p.id.as_str()) {
                return Err(format!("duplicate persona id '{}'", p.id));
            }
            if p.fallbacks.iter().any(|f| f.runtime.trim().is_empty()) {
                return Err(format!("persona '{}' has a fallback without a runtime", p.id));
            }
        }
        if self.persona(&self.entry).is_none() {
            return Err(format!("entry persona '{}' does not exist", self.entry));
        }
        for e in &self.edges {
            for end in [&e.from, &e.to] {
                if self.persona(end).is_none() {
                    return Err(format!("edge references unknown persona '{end}'"));
                }
            }
            if e.from == e.to {
                return Err(format!("self-loop on '{}' is not allowed", e.from));
            }
        }
        if self.max_parallel == 0 {
            return Err("max_parallel must be at least 1".into());
        }
        Ok(())
    }
}

pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persona(id: &str) -> Persona {
        Persona {
            id: id.into(),
            name: id.into(),
            role: String::new(),
            runtime: "mock".into(),
            model: String::new(),
            effort: Effort::Medium,
            prompt: String::new(),
            extra_args: vec![],
            env: Default::default(),
            fallbacks: vec![],
            position: None,
        }
    }

    #[test]
    fn edges_are_directional_unless_bidirectional() {
        let team = Team {
            id: "t".into(),
            name: "t".into(),
            description: String::new(),
            entry: "a".into(),
            personas: vec![persona("a"), persona("b"), persona("c")],
            edges: vec![
                Edge { from: "a".into(), to: "b".into(), bidirectional: false },
                Edge { from: "a".into(), to: "c".into(), bidirectional: true },
            ],
            max_turns: 10,
            max_parallel: 2,
        };
        assert!(team.validate().is_ok());
        assert!(team.can_send("a", "b"));
        assert!(!team.can_send("b", "a"));
        assert!(team.can_send("c", "a"));
        assert_eq!(team.recipients_of("a").len(), 2);
    }
}
