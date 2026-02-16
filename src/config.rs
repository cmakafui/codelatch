use std::{env, fs, path::PathBuf};

use directories::BaseDirs;
use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize};

use crate::errors::{AppError, Result};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub telegram_bot_token: Option<String>,
    #[serde(default)]
    pub telegram_chat_id: Option<i64>,
    #[serde(default = "default_auto_deny_seconds")]
    pub auto_deny_seconds: u64,
    #[serde(default = "default_hook_timeout_seconds")]
    pub hook_timeout_seconds: u64,
    #[serde(default = "default_context_lines")]
    pub context_lines: usize,
    #[serde(default = "default_max_inline_length")]
    pub max_inline_length: usize,
    #[serde(default = "default_socket_path")]
    pub socket_path: String,
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            telegram_bot_token: None,
            telegram_chat_id: None,
            auto_deny_seconds: default_auto_deny_seconds(),
            hook_timeout_seconds: default_hook_timeout_seconds(),
            context_lines: default_context_lines(),
            max_inline_length: default_max_inline_length(),
            socket_path: default_socket_path(),
            db_path: default_db_path(),
        }
    }
}

impl Config {
    pub fn is_configured(&self) -> bool {
        self.telegram_bot_token
            .as_deref()
            .map(str::trim)
            .is_some_and(|token| !token.is_empty())
            && self.telegram_chat_id.is_some_and(|chat_id| chat_id != 0)
    }

    pub fn token(&self) -> Result<&str> {
        self.telegram_bot_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .ok_or(AppError::NotConfigured)
    }

    pub fn chat_id(&self) -> Result<i64> {
        self.telegram_chat_id
            .filter(|chat_id| *chat_id != 0)
            .ok_or(AppError::NotConfigured)
    }
}

pub fn load() -> Result<Config> {
    let path = config_path()?;
    let mut figment =
        Figment::from(Serialized::defaults(Config::default())).merge(Env::prefixed("CODELATCH_"));

    if path.exists() {
        figment = figment.merge(Toml::file(&path));
    }

    figment.extract().map_err(|_| AppError::ConfigLoad)
}

pub fn save(config: &Config) -> Result<()> {
    let path = config_path()?;
    let Some(parent) = path.parent() else {
        return Err(AppError::HomeDirUnavailable);
    };

    fs::create_dir_all(parent)
        .map_err(|_| AppError::CreateConfigDir(parent.display().to_string()))?;

    let toml_text = toml::to_string_pretty(config).map_err(|_| AppError::ConfigSerialize)?;
    fs::write(&path, toml_text).map_err(|_| AppError::WriteConfig(path.display().to_string()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

pub fn config_path() -> Result<PathBuf> {
    let Some(base_dirs) = BaseDirs::new() else {
        return Err(AppError::HomeDirUnavailable);
    };
    Ok(base_dirs.config_dir().join("codelatch").join("config.toml"))
}

pub fn claude_settings_path() -> Result<PathBuf> {
    let Some(base_dirs) = BaseDirs::new() else {
        return Err(AppError::HomeDirUnavailable);
    };
    Ok(base_dirs.home_dir().join(".claude").join("settings.json"))
}

pub fn data_dir() -> Result<PathBuf> {
    let Some(base_dirs) = BaseDirs::new() else {
        return Err(AppError::HomeDirUnavailable);
    };
    Ok(base_dirs.data_dir().join("codelatch"))
}

pub fn pid_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("codelatchd.pid"))
}

fn default_auto_deny_seconds() -> u64 {
    600
}

fn default_hook_timeout_seconds() -> u64 {
    3600
}

fn default_context_lines() -> usize {
    15
}

fn default_max_inline_length() -> usize {
    4096
}

fn default_socket_path() -> String {
    if let Ok(runtime_dir) = env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir)
            .join("codelatch.sock")
            .to_string_lossy()
            .into_owned();
    }
    "/tmp/codelatch.sock".to_string()
}

fn default_db_path() -> String {
    BaseDirs::new()
        .map(|dirs| dirs.data_dir().join("codelatch").join("codelatch.db"))
        .unwrap_or_else(|| PathBuf::from("/tmp/codelatch.db"))
        .to_string_lossy()
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::Config;

    #[test]
    fn defaults_are_safe() {
        let config = Config::default();
        assert!(!config.is_configured());
        assert_eq!(config.auto_deny_seconds, 600);
        assert_eq!(config.hook_timeout_seconds, 3600);
    }
}
