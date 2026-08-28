use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(not(windows))]
use directories::ProjectDirs;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub enabled: bool,
    pub bind_address: String,
    pub port: u16,
    pub fps: u32,
    pub max_clients: u32,
    pub allow_control: bool,
    pub allowed_users: Vec<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            bind_address: "0.0.0.0".to_string(),
            port: 3389,
            fps: 30,
            max_clients: 1,
            allow_control: true,
            allowed_users: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn normalize(&mut self) {
        self.bind_address = self.bind_address.trim().to_string();
        self.fps = self.fps.clamp(1, 120);
        self.max_clients = self.max_clients.clamp(1, 16);
        self.allowed_users = self
            .allowed_users
            .iter()
            .map(|user| user.trim().to_lowercase())
            .filter(|user| !user.is_empty())
            .collect();
        self.allowed_users.sort();
        self.allowed_users.dedup();
    }

    pub fn allows_user(&self, candidates: &[String]) -> bool {
        candidates.iter().any(|candidate| {
            let candidate = candidate.trim().to_lowercase();
            !candidate.is_empty()
                && self
                    .allowed_users
                    .iter()
                    .any(|allowed| allowed == &candidate)
        })
    }
}

pub fn data_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
            .join("RdpDesktopHost")
    }

    #[cfg(not(windows))]
    {
        ProjectDirs::from("com", "RdpDesktopHost", "RdpDesktopHost")
            .map(|dirs| dirs.data_dir().to_path_buf())
            .unwrap_or_else(|| PathBuf::from(".rdp-desktop-host"))
    }
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn certificate_path() -> PathBuf {
    data_dir().join("server-cert.pem")
}

pub fn private_key_path() -> PathBuf {
    data_dir().join("server-key.pem")
}

pub fn load_from(path: &Path) -> Result<AppConfig> {
    if !path.exists() {
        let mut config = AppConfig::default();
        config.normalize();
        save_to(path, &config)?;
        return Ok(config);
    }

    let text =
        fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
    let mut config: AppConfig =
        toml::from_str(&text).with_context(|| format!("parse config {}", path.display()))?;
    config.normalize();
    Ok(config)
}

pub fn save_to(path: &Path, config: &AppConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create config directory {}", parent.display()))?;
    }

    let mut normalized = config.clone();
    normalized.normalize();
    let text = toml::to_string_pretty(&normalized).context("serialize config")?;
    let temp_path = path.with_extension("toml.tmp");
    fs::write(&temp_path, text).with_context(|| format!("write config {}", temp_path.display()))?;
    fs::rename(&temp_path, path).with_context(|| format!("replace config {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_deduplicates_and_clamps_values() {
        let mut config = AppConfig {
            fps: 999,
            max_clients: 0,
            allowed_users: vec![" Alice ".into(), "alice".into(), "".into()],
            ..Default::default()
        };

        config.normalize();

        assert_eq!(config.fps, 120);
        assert_eq!(config.max_clients, 1);
        assert_eq!(config.allowed_users, vec!["alice"]);
    }

    #[test]
    fn round_trip_preserves_policy() {
        let path = std::env::temp_dir().join(format!(
            "rdp-desktop-host-config-{}.toml",
            std::process::id()
        ));
        let config = AppConfig {
            allowed_users: vec![".\\alice".into()],
            allow_control: false,
            ..Default::default()
        };

        save_to(&path, &config).expect("write test config");
        let loaded = load_from(&path).expect("read test config");
        let _ = fs::remove_file(path);

        assert!(!loaded.allow_control);
        assert!(loaded.allows_user(&[".\\alice".into()]));
    }
}
