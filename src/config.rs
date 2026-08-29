//! Configuration management module

use anyhow::Result;
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Default data refresh interval, in minutes
pub const DEFAULT_REFRESH_INTERVAL_MINUTES: u32 = 60;

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Encryption key (for encrypting AK/SK)
    pub encryption_key: Option<String>,
    /// Theme settings
    pub theme: ThemeConfig,
    /// Data refresh interval (minutes).
    ///
    /// Persisted but not acted on yet: nothing schedules a refresh from it,
    /// so it has no Settings UI either. Wire both up together.
    pub refresh_interval_minutes: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            encryption_key: None,
            theme: ThemeConfig::default(),
            refresh_interval_minutes: DEFAULT_REFRESH_INTERVAL_MINUTES,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThemeConfig {
    /// Whether to use dark mode. Defaults to light.
    pub dark_mode: bool,
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

/// Path of the application-state database: accounts, budgets and the
/// response caches the dashboard reads.
pub fn get_database_path() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("cloudbridge.duckdb"))
}

/// Path of the billing ledger. A separate file from the application state:
/// the ledger is the durable record, everything in `cloudbridge.duckdb` is
/// either user-entered or re-fetchable.
pub fn get_ledger_database_path() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("billing.duckdb"))
}

/// Root of the raw payload store. Laid out so the same path semantics
/// work for a local directory and for an object store; see [`crate::cloud::raw`].
pub fn get_raw_data_dir() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("raw"))
}

/// Load configuration
pub fn load_config() -> Result<AppConfig> {
    let config_path = get_config_path()?;

    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        let mut config: AppConfig = serde_json::from_str(&content)?;
        // Configs written before AppConfig had a real Default stored 0 here,
        // which is not a usable interval.
        if config.refresh_interval_minutes == 0 {
            config.refresh_interval_minutes = DEFAULT_REFRESH_INTERVAL_MINUTES;
        }
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
