//! What the menu bar shows: this month's spend, where it is heading, and
//! what needs attention.
//!
//! One blocking load feeds both the icon's title and the panel under it, so
//! the two never disagree. The pieces are the Overview's own — the same
//! month-to-date figure, forecast and movers — read again rather than
//! recomputed, so the menu bar cannot drift from the page it summarizes.

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::alerts::{self, Severity};
use crate::ui::data::{self, Range};

/// How many open alerts the panel lists by name; the rest are a count.
pub const LISTED_ALERTS: usize = 3;

/// The menu bar's view of the ledger.
#[derive(Clone, Debug)]
pub struct Summary {
    pub currency: String,
    /// Net spend this month to date.
    pub month_to_date: f64,
    /// Month-end forecast.
    pub forecast: f64,
    /// Usage change against the same day last month, when there is a base
    /// to compare with.
    pub change_pct: Option<f64>,
    /// Open alerts, critical first, newest first within a severity.
    pub alerts: Vec<AlertLine>,
    /// The services whose usage moved most against last month.
    pub movers: Vec<MoverLine>,
    /// Accounts that are not demo ones.
    pub real_accounts: usize,
    /// The freshest ingest across all accounts.
    pub last_synced_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug)]
pub struct AlertLine {
    pub id: String,
    pub severity: Severity,
    pub title: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct MoverLine {
    pub provider: String,
    pub service: String,
    pub amount: f64,
    pub change_pct: Option<f64>,
}

impl Summary {
    /// Read it all. Blocking; run inside `smol::unblock`.
    pub fn load() -> Result<Self> {
        let overview = data::load_overview(Range::Mtd)?;
        let mut open = alerts::open_alerts()?;
        open.sort_by(|a, b| {
            severity_rank(a.severity)
                .cmp(&severity_rank(b.severity))
                .then(b.created_at.cmp(&a.created_at))
        });
        let sync = data::load_sync_status()?;

        Ok(Self {
            currency: overview.currency,
            month_to_date: overview.stats.spend,
            forecast: overview.card2_value,
            change_pct: overview.stats.change_pct,
            alerts: open
                .into_iter()
                .map(|alert| AlertLine {
                    id: alert.id,
                    severity: alert.severity,
                    title: alert.title,
                    created_at: alert.created_at,
                })
                .collect(),
            movers: overview
                .movers
                .into_iter()
                .map(|row| MoverLine {
                    provider: row.provider,
                    service: row.service,
                    amount: row.amount,
                    change_pct: row.change_pct,
                })
                .collect(),
            real_accounts: overview.accounts.real,
            last_synced_at: sync.last_synced_at,
        })
    }

    /// Whether any open alert is critical.
    pub fn has_critical(&self) -> bool {
        self.alerts
            .iter()
            .any(|alert| alert.severity == Severity::Critical)
    }

    /// The text beside the menu bar icon: the month's spend, short enough
    /// to sit among other menu bar items, marked when something is wrong.
    pub fn title(&self) -> String {
        let spend = compact_amount(self.month_to_date, &self.currency);
        if self.alerts.is_empty() {
            spend
        } else {
            format!("{spend} · {}", self.alerts.len())
        }
    }

    /// The icon's tooltip, which is all Windows shows of it.
    pub fn tooltip(&self) -> String {
        let mut text = format!(
            "CloudBridge — {} this month",
            crate::ui::fmt::amount(self.month_to_date, &self.currency)
        );
        match self.alerts.len() {
            0 => {}
            1 => text.push_str(", 1 open alert"),
            n => text.push_str(&format!(", {n} open alerts")),
        }
        text
    }
}

fn severity_rank(severity: Severity) -> u8 {
    match severity {
        Severity::Critical => 0,
        Severity::Warning => 1,
    }
}

/// An amount in four or five characters: `$842`, `$12.4k`, `$1.2M`. The
/// menu bar is shared with every other app's item, and the exact figure is
/// one click away in the panel.
pub fn compact_amount(value: f64, currency: &str) -> String {
    let full = crate::ui::fmt::amount(value, currency);
    let magnitude = value.abs();
    if magnitude < 1_000.0 {
        // Whole units are enough up here; cents are panel detail.
        return match full.split_once('.') {
            Some((whole, _)) if magnitude >= 10.0 => whole.to_string(),
            _ => full,
        };
    }
    // The symbol, or the code and its space, is whatever precedes the
    // first digit of the full rendering.
    let start = full
        .find(|c: char| c.is_ascii_digit())
        .unwrap_or(full.len());
    let prefix = &full[..start];
    let (scaled, suffix) = if magnitude < 1_000_000.0 {
        (magnitude / 1_000.0, "k")
    } else {
        (magnitude / 1_000_000.0, "M")
    };
    let digits = if scaled < 100.0 {
        format!("{scaled:.1}")
    } else {
        format!("{scaled:.0}")
    };
    format!("{prefix}{digits}{suffix}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn summary(month_to_date: f64, alerts: Vec<Severity>) -> Summary {
        Summary {
            currency: "USD".to_string(),
            month_to_date,
            forecast: 0.0,
            change_pct: None,
            alerts: alerts
                .into_iter()
                .enumerate()
                .map(|(i, severity)| AlertLine {
                    id: format!("a{i}"),
                    severity,
                    title: "Spike".to_string(),
                    created_at: Utc::now(),
                })
                .collect(),
            movers: Vec::new(),
            real_accounts: 1,
            last_synced_at: None,
        }
    }

    #[test]
    fn a_compact_amount_fits_in_the_menu_bar() {
        assert_eq!(compact_amount(0.0, "USD"), "$0");
        assert_eq!(compact_amount(4.5, "USD"), "$4.50");
        assert_eq!(compact_amount(842.37, "USD"), "$842");
        assert_eq!(compact_amount(1_234.0, "USD"), "$1.2k");
        assert_eq!(compact_amount(12_400.0, "USD"), "$12.4k");
        assert_eq!(compact_amount(10_811.41, "USD"), "$10.8k");
        assert_eq!(compact_amount(250_000.0, "USD"), "$250k");
        assert_eq!(compact_amount(1_200_000.0, "CNY"), "¥1.2M");
    }

    #[test]
    fn a_currency_without_a_symbol_keeps_its_code() {
        assert_eq!(compact_amount(5_000.0, "HKD"), "HKD 5.0k");
    }

    #[test]
    fn the_title_counts_open_alerts_only_when_there_are_some() {
        assert_eq!(summary(842.0, vec![]).title(), "$842");
        assert_eq!(
            summary(10_811.41, vec![Severity::Critical, Severity::Warning]).title(),
            "$10.8k · 2"
        );
    }

    #[test]
    fn the_tooltip_spells_the_amount_out() {
        assert_eq!(
            summary(10_811.41, vec![Severity::Warning]).tooltip(),
            "CloudBridge — $10,811 this month, 1 open alert"
        );
    }

    #[test]
    fn critical_is_told_apart_from_warning() {
        assert!(!summary(1.0, vec![Severity::Warning]).has_critical());
        assert!(summary(1.0, vec![Severity::Warning, Severity::Critical]).has_critical());
    }
}
