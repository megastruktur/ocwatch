use anyhow::{Context, Result};
use dirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u16,
    #[serde(default)]
    pub hosts: Vec<HostConfig>,
}

fn default_poll_interval() -> u16 {
    5
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HostConfig {
    pub name: String,
    /// None = local machine. Some("megaserver") = SSH target
    pub ssh_target: Option<String>,
    pub ssh_identity: Option<PathBuf>,
    pub ssh_port: Option<u16>,
    /// If set, only scan this tmux session name on remote
    pub tmux_session_filter: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            poll_interval_secs: 5,
            hosts: vec![],
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();

        if !config_path.exists() {
            tracing::info!("No config file found at {:?}, using defaults", config_path);
            return Ok(Config::default());
        }

        let contents = std::fs::read_to_string(&config_path)
            .with_context(|| format!("Failed to read config file: {:?}", config_path))?;

        let config: Config = toml::from_str(&contents)
            .with_context(|| format!("Failed to parse config file: {:?}", config_path))?;

        config.validate()?;
        Ok(config)
    }

    pub fn config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("~/.config"))
            .join("ocwatch")
            .join("config.toml")
    }

    fn validate(&self) -> Result<()> {
        if self.poll_interval_secs < 1 {
            anyhow::bail!("poll_interval_secs must be >= 1");
        }
        Ok(())
    }
}
