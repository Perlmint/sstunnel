//! Runtime enable/disable overrides, optionally persisted across restarts.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::warn;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    #[serde(default)]
    overrides: BTreeMap<String, bool>,
}

/// Decisions made at runtime that take precedence over `enabled` in the config.
#[derive(Debug, Default)]
pub struct Overrides {
    map: BTreeMap<String, bool>,
    path: Option<PathBuf>,
}

impl Overrides {
    /// Load from `path`. A missing or unreadable file starts empty rather than
    /// failing: stale state must never stop the daemon from coming up.
    pub fn load(path: Option<PathBuf>) -> Self {
        let map = match &path {
            Some(path) if path.exists() => match std::fs::read_to_string(path)
                .map_err(anyhow::Error::from)
                .and_then(|text| Ok(serde_json::from_str::<Persisted>(&text)?))
            {
                Ok(state) => state.overrides,
                Err(err) => {
                    warn!(path = %path.display(), %err, "ignoring unreadable state file");
                    BTreeMap::new()
                }
            },
            _ => BTreeMap::new(),
        };
        Self { map, path }
    }

    pub fn get(&self, name: &str) -> Option<bool> {
        self.map.get(name).copied()
    }

    pub fn set(&mut self, name: &str, enabled: bool) {
        self.map.insert(name.to_owned(), enabled);
    }

    /// Drop overrides for forwards that no longer exist in the config.
    pub fn retain_known<'a>(&mut self, known: impl Iterator<Item = &'a str>) {
        let known: Vec<&str> = known.collect();
        self.map.retain(|name, _| known.contains(&name.as_str()));
    }

    pub fn save(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        crate::paths::ensure_parent_dir(path)?;
        let text = serde_json::to_string_pretty(&Persisted {
            overrides: self.map.clone(),
        })?;
        // Write via a temp file so a crash mid-write cannot leave a truncated state.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    /// Save, logging rather than propagating: losing persistence is not fatal.
    pub fn save_best_effort(&self) {
        if let Err(err) = self.save() {
            warn!(%err, "could not persist runtime state");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("state.json");

        let mut overrides = Overrides::load(Some(path.clone()));
        assert_eq!(overrides.get("db"), None);
        overrides.set("db", false);
        overrides.save().unwrap();

        let reloaded = Overrides::load(Some(path));
        assert_eq!(reloaded.get("db"), Some(false));
    }

    #[test]
    fn ignores_a_corrupt_state_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(Overrides::load(Some(path)).get("db"), None);
    }

    #[test]
    fn forgets_forwards_that_left_the_config() {
        let mut overrides = Overrides::default();
        overrides.set("db", false);
        overrides.set("gone", true);
        overrides.retain_known(["db"].into_iter());
        assert_eq!(overrides.get("db"), Some(false));
        assert_eq!(overrides.get("gone"), None);
    }
}
