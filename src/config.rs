//! Configuration management module

use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    /// Encryption key (for encrypting AK/SK)
    pub encryption_key: Option<String>,
    /// Theme settings
    pub theme: ThemeConfig,
    /// Data refresh interval (minutes)
    pub refresh_interval_minutes: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThemeConfig {
    /// Whether to use dark mode
    pub dark_mode: bool,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self { dark_mode: true }
    }
}

/// Get application data directory
pub fn get_app_data_dir() -> Result<PathBuf> {
    // Use simpler path: AppData/Roaming/CloudBridge/ on Windows
    // "" for qualifier and organization to avoid nested folders
    let proj_dirs = ProjectDirs::from("", "", "CloudBridge")
        .ok_or_else(|| anyhow::anyhow!("Unable to determine app data directory"))?;

    let data_dir = proj_dirs.data_dir().to_path_buf();

    // Ensure directory exists
    if !data_dir.exists() {
        fs::create_dir_all(&data_dir)?;
    }

    Ok(data_dir)
}

/// Get config file path
pub fn get_config_path() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("config.json"))
}

/// Get database path
pub fn get_database_path() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("cloudbridge.duckdb"))
}

/// Load configuration
pub fn load_config() -> Result<AppConfig> {
    let config_path = get_config_path()?;

    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        let config: AppConfig = serde_json::from_str(&content)?;
        Ok(config)
    } else {
        // Return default config
        let config = AppConfig::default();
        save_config(&config)?;
        Ok(config)
    }
}

/// Save configuration
pub fn save_config(config: &AppConfig) -> Result<()> {
    let config_path = get_config_path()?;
    let content = serde_json::to_string_pretty(config)?;
    fs::write(&config_path, content)?;
    Ok(())
}
