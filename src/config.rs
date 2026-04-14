use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Per-pane settings (name, accent color) keyed by the pane's initial cwd.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PaneSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Accent color as a string like "cyan" / "green". See `color_from_str`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

impl PaneSettings {
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.color.is_none()
    }
}

/// On-disk shape of tscope's config (`~/.config/tscope/config.toml`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Map from "user@host" (or plain "host") to a user-friendly name.
    #[serde(default)]
    pub ssh_aliases: BTreeMap<String, String>,
    /// Map from absolute cwd path to per-pane overrides.
    #[serde(default)]
    pub pane_aliases: BTreeMap<String, PaneSettings>,
}

impl Config {
    pub fn path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("tscope")
            .join("config.toml")
    }

    pub fn load() -> Self {
        let path = Self::path();
        match std::fs::read_to_string(&path) {
            Ok(s) => toml::from_str(&s).unwrap_or_default(),
            Err(_) => Config::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = toml::to_string_pretty(self)?;
        std::fs::write(&path, s)?;
        Ok(())
    }

    /// Try `user@host`, then `host` alone, returning the first alias found.
    pub fn lookup_ssh_alias(&self, user: Option<&str>, host: &str) -> Option<String> {
        if let Some(u) = user {
            let key = format!("{}@{}", u, host);
            if let Some(v) = self.ssh_aliases.get(&key) {
                return Some(v.clone());
            }
        }
        self.ssh_aliases.get(host).cloned()
    }

    pub fn set_ssh_alias(&mut self, user: Option<&str>, host: &str, name: String) {
        let key = alias_key(user, host);
        if name.trim().is_empty() {
            self.ssh_aliases.remove(&key);
        } else {
            self.ssh_aliases.insert(key, name);
        }
    }

    pub fn lookup_pane_settings(&self, cwd: &std::path::Path) -> PaneSettings {
        self.pane_aliases
            .get(&cwd.to_string_lossy().to_string())
            .cloned()
            .unwrap_or_default()
    }

    pub fn set_pane_settings(&mut self, cwd: &std::path::Path, settings: PaneSettings) {
        let key = cwd.to_string_lossy().to_string();
        if settings.is_empty() {
            self.pane_aliases.remove(&key);
        } else {
            self.pane_aliases.insert(key, settings);
        }
    }
}

fn alias_key(user: Option<&str>, host: &str) -> String {
    match user {
        Some(u) => format!("{}@{}", u, host),
        None => host.to_string(),
    }
}

