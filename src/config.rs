use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub refresh_seconds: u64,
    pub command_timeout_seconds: u64,
    pub max_output_mb: usize,
    pub page_size: usize,
    pub history_page_size: usize,
    /// Legacy configuration key, retained for compatibility. ID windows are unused.
    pub history_change_window: u64,
    pub history_max_scan_rows: u64,
    pub theme: String,
    /// Key to action, e.g. `quit = "Q"`. See README for supported actions.
    pub keys: BTreeMap<String, String>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            refresh_seconds: 10,
            command_timeout_seconds: 120,
            max_output_mb: 16,
            page_size: 200,
            history_page_size: 100,
            history_change_window: 1000,
            history_max_scan_rows: 0,
            theme: "dark".into(),
            keys: BTreeMap::new(),
        }
    }
}
impl Config {
    pub fn path() -> PathBuf {
        if let Some(p) = std::env::var_os("XDG_CONFIG_HOME") {
            return PathBuf::from(p).join("p4ui/config.toml");
        }
        #[cfg(windows)]
        if let Some(p) = std::env::var_os("APPDATA") {
            return PathBuf::from(p).join("p4ui/config.toml");
        }
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/p4ui/config.toml")
    }
    pub fn load(path: Option<PathBuf>) -> Result<Self> {
        let explicit = path.is_some();
        let path = path.unwrap_or_else(Self::path);
        let config: Self = match std::fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .with_context(|| format!("Invalid config {}", path.display()))?,
            Err(e) if !explicit && e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
        };
        if !(1..=3600).contains(&config.command_timeout_seconds) {
            bail!("command_timeout_seconds must be 1..=3600");
        }
        if !(1..=256).contains(&config.max_output_mb) {
            bail!("max_output_mb must be 1..=256");
        }
        if !(10..=10000).contains(&config.page_size) {
            bail!("page_size must be 10..=10000");
        }
        if !(1..=200).contains(&config.history_page_size) {
            bail!("history_page_size must be 1..=200");
        }
        if !(1..=10000).contains(&config.history_change_window) {
            bail!("history_change_window must be 1..=10000");
        }
        if config.history_max_scan_rows > 1000000 {
            bail!("history_max_scan_rows must be 0..=1000000 (0 uses server limits)");
        }
        if !["dark", "light", "mono"].contains(&config.theme.as_str()) {
            bail!("theme must be dark, light, or mono");
        }
        let actions = [
            "quit",
            "refresh",
            "help",
            "search",
            "up",
            "down",
            "open",
            "reconcile",
            "submit",
            "sync",
            "new_change",
            "revert",
            "edit",
            "shelve",
            "unshelve",
            "delete",
            "move_change",
            "annotate",
            "filelog",
            "resolve",
            "rename",
            "lock",
            "revision",
            "edit_change",
            "edit_client",
        ];
        for (action, key) in &config.keys {
            if !actions.contains(&action.as_str()) {
                bail!("Unknown key action {action}");
            }
            if key.chars().count() != 1 {
                bail!("Key for {action} must be one character");
            }
        }
        let mut used = std::collections::HashSet::new();
        for action in actions {
            let key = config.key(action);
            if key.is_control() || key.is_whitespace() || "0123456789Agpl+".contains(key) {
                bail!("Key {key:?} for {action} is reserved for navigation or a fixed command");
            }
            if !used.insert(key) {
                bail!("Duplicate effective key binding {key}; remap both conflicting actions");
            }
        }
        Ok(config)
    }
    pub fn key(&self, action: &str) -> char {
        self.keys
            .get(action)
            .and_then(|s| s.chars().next())
            .unwrap_or(match action {
                "quit" => 'q',
                "refresh" => 'r',
                "help" => '?',
                "search" => '/',
                "up" => 'k',
                "down" => 'j',
                "open" => 'o',
                "reconcile" => 'a',
                "submit" => 'c',
                "sync" => 'f',
                "new_change" => 'n',
                "revert" => 'U',
                "edit" => 'e',
                "shelve" => 's',
                "unshelve" => 'u',
                "delete" => 'D',
                "move_change" => 'm',
                "annotate" => 'b',
                "filelog" => 'h',
                "resolve" => 'R',
                "rename" => 'M',
                "lock" => 'L',
                "revision" => 'v',
                "edit_change" => 'E',
                "edit_client" => 'C',
                _ => '\0',
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn invalid_and_colliding_config_is_rejected() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("config.toml");
        for text in [
            "page_size = 0",
            "history_page_size = 0",
            "history_change_window = 10001",
            "history_max_scan_rows = 1000001",
            "theme = 'typo'",
            "unknown = 1",
            "[keys]\nquit = 'r'",
            "[keys]\nquit = '1'",
            "[keys]\nquit = 'A'",
            "[keys]\nquitt = 'Q'",
        ] {
            std::fs::write(&path, text)?;
            assert!(Config::load(Some(path.clone())).is_err(), "accepted {text}");
        }
        std::fs::write(&path, "theme = 'light'\n[keys]\nquit = 'r'\nrefresh = 'q'")?;
        let c = Config::load(Some(path))?;
        assert_eq!(c.key("refresh"), 'q');
        Ok(())
    }
}
