//! Configuration management module
//!
//! The shapes here are shared; only where a config is kept differs. The
//! desktop writes `config.json` under the OS's application-data directory,
//! and the web demo holds the same struct in memory for the life of the tab.

use anyhow::Result;
#[cfg(not(target_family = "wasm"))]
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

#[cfg(not(target_family = "wasm"))]
use std::fs;
#[cfg(not(target_family = "wasm"))]
use std::path::PathBuf;

/// How long a billing period stays fresh after it is ingested, until the
/// user picks otherwise.
///
/// A day, rather than the few hours this used to be: a provider's bill does
/// not move faster than that in any way worth paying for. Cost Explorer
/// bills per request, the Alibaba Cloud bill settles a few times a day, and
/// a mid-month figure that is a few hours stale is not one anybody acts on.
pub const DEFAULT_REFRESH_INTERVAL_HOURS: u32 = 24;

/// Intervals offered in Settings, in hours.
pub const REFRESH_INTERVAL_CHOICES_HOURS: &[u32] = &[6, 12, 24, 48];

/// Currency every amount is shown in until the user picks another.
pub const DEFAULT_REPORTING_CURRENCY: &str = "USD";

/// Currencies the built-in rate table can convert between.
pub const SUPPORTED_REPORTING_CURRENCIES: &[&str] = &["USD", "CNY"];

/// Application configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Encryption key (for encrypting AK/SK)
    pub encryption_key: Option<String>,
    /// Theme settings
    pub theme: ThemeConfig,
    /// How long a billing period stays fresh after it is ingested, in
    /// hours.
    ///
    /// Replaces `refresh_interval_minutes`, which was persisted and never
    /// acted on. The rename is deliberate rather than a change of unit:
    /// that field defaulted to 60, so reading a stored 60 as a *minute*
    /// window would have quietly moved every existing install to refreshing
    /// hourly — which costs money on Cost Explorer. Serde ignores the
    /// unknown key, so an old config simply starts at the new default.
    #[serde(default = "default_refresh_interval_hours")]
    pub refresh_interval_hours: u32,
    /// Currency every amount is converted to for display. Charges are
    /// stored in the currency they were billed in; this only changes the
    /// view they are read through.
    #[serde(default = "default_reporting_currency")]
    pub reporting_currency: String,
}

fn default_reporting_currency() -> String {
    DEFAULT_REPORTING_CURRENCY.to_string()
}

fn default_refresh_interval_hours() -> u32 {
    DEFAULT_REFRESH_INTERVAL_HOURS
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            encryption_key: None,
            theme: ThemeConfig::default(),
            refresh_interval_hours: default_refresh_interval_hours(),
            reporting_currency: default_reporting_currency(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ThemeConfig {
    /// Whether to use dark mode. Defaults to light.
    ///
    /// Derived from `name` whenever a theme is picked; kept because it is
    /// what builds before the theme picker knew how to persist.
    pub dark_mode: bool,
    /// Name of the selected theme, as it appears in the theme registry
    /// (e.g. "CloudBridge Light", "Ayu Dark"). `None` means no theme was
    /// ever picked: choose between the CloudBridge pair by `dark_mode`.
    #[serde(default)]
    pub name: Option<String>,
}

/// Get application data directory
#[cfg(not(target_family = "wasm"))]
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
#[cfg(not(target_family = "wasm"))]
pub fn get_config_path() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("config.json"))
}

/// Path of the application-state database: accounts, budgets and the
/// response caches the dashboard reads.
#[cfg(not(target_family = "wasm"))]
pub fn get_database_path() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("cloudbridge.duckdb"))
}

/// Path of the billing ledger. A separate file from the application state:
/// the ledger is the durable record, everything in `cloudbridge.duckdb` is
/// either user-entered or re-fetchable.
#[cfg(not(target_family = "wasm"))]
pub fn get_ledger_database_path() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("billing.duckdb"))
}

/// Root of the raw payload store. Laid out so the same path semantics
/// work for a local directory and for an object store; see [`crate::cloud::raw`].
#[cfg(not(target_family = "wasm"))]
pub fn get_raw_data_dir() -> Result<PathBuf> {
    let data_dir = get_app_data_dir()?;
    Ok(data_dir.join("raw"))
}

/// Load configuration.
///
/// The browser has no file to read, so the settings the demo runs with are
/// the defaults the desktop would write on a first launch. Keeping them in
/// memory is enough for the Settings page to be exercised; nothing survives
/// a reload, which is what a demo wants.
#[cfg(target_family = "wasm")]
pub fn load_config() -> Result<AppConfig> {
    CONFIG.with(|config| Ok(config.borrow().clone()))
}

#[cfg(target_family = "wasm")]
pub fn save_config(config: &AppConfig) -> Result<()> {
    CONFIG.with(|stored| *stored.borrow_mut() = config.clone());
    Ok(())
}

#[cfg(target_family = "wasm")]
thread_local! {
    static CONFIG: std::cell::RefCell<AppConfig> = std::cell::RefCell::new(AppConfig::default());
}

/// Load configuration
#[cfg(not(target_family = "wasm"))]
pub fn load_config() -> Result<AppConfig> {
    let config_path = get_config_path()?;

    if config_path.exists() {
        let content = fs::read_to_string(&config_path)?;
        let mut config: AppConfig = serde_json::from_str(&content)?;
        // Zero is not a usable interval: it would re-fetch every period on
        // every load, which is exactly what this window exists to prevent.
        if config.refresh_interval_hours == 0 {
            config.refresh_interval_hours = default_refresh_interval_hours();
        }
        if config.reporting_currency.is_empty() {
            config.reporting_currency = default_reporting_currency();
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
#[cfg(not(target_family = "wasm"))]
pub fn save_config(config: &AppConfig) -> Result<()> {
    let config_path = get_config_path()?;
    let content = serde_json::to_string_pretty(config)?;
    fs::write(&config_path, content)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The important one. A config written by a build where the interval was
    /// dead holds `refresh_interval_minutes: 60`. Reading that as the new
    /// setting — under either name — would move every existing install to
    /// refreshing hourly, and AWS Cost Explorer bills per request.
    #[test]
    fn a_config_from_before_the_setting_existed_starts_at_the_default() {
        let old = r#"{
            "encryption_key": null,
            "theme": { "dark_mode": false },
            "refresh_interval_minutes": 60,
            "reporting_currency": "CNY"
        }"#;

        let config: AppConfig = serde_json::from_str(old).expect("an old config still parses");

        assert_eq!(
            config.refresh_interval_hours,
            DEFAULT_REFRESH_INTERVAL_HOURS
        );
        // Everything the user did choose is kept.
        assert_eq!(config.reporting_currency, "CNY");
    }

    #[test]
    fn an_interval_the_user_chose_is_the_one_read_back() {
        let config: AppConfig = serde_json::from_str(
            r#"{"encryption_key":null,"theme":{"dark_mode":false},
                "refresh_interval_hours":6,"reporting_currency":"USD"}"#,
        )
        .unwrap();

        assert_eq!(config.refresh_interval_hours, 6);
    }

    #[test]
    fn the_default_is_a_day_and_is_offered_in_settings() {
        assert_eq!(
            AppConfig::default().refresh_interval_hours,
            DEFAULT_REFRESH_INTERVAL_HOURS
        );
        assert!(
            REFRESH_INTERVAL_CHOICES_HOURS.contains(&DEFAULT_REFRESH_INTERVAL_HOURS),
            "the default has to be selectable, or Settings shows nothing chosen"
        );
    }

    /// A config written before the theme picker holds
    /// `theme: { "dark_mode": ... }` and nothing else.
    #[test]
    fn a_config_from_before_the_theme_picker_has_no_theme_name() {
        let config: AppConfig = serde_json::from_str(
            r#"{"encryption_key":null,"theme":{"dark_mode":true},
                "refresh_interval_hours":24,"reporting_currency":"USD"}"#,
        )
        .expect("an old config still parses");

        assert_eq!(config.theme.name, None);
        // The dark-mode flag survives as the fallback for choosing a theme.
        assert!(config.theme.dark_mode);
    }

    #[test]
    fn a_picked_theme_name_round_trips() {
        let mut config = AppConfig::default();
        config.theme.name = Some("Ayu Dark".to_string());
        config.theme.dark_mode = true;

        let read_back: AppConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();

        assert_eq!(read_back.theme.name.as_deref(), Some("Ayu Dark"));
        assert!(read_back.theme.dark_mode);
    }
}
