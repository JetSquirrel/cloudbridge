//! Amount formatting shared by the pages and the chart tooltip.

use chrono::{DateTime, Utc};

/// Below this, an amount is netting round-off rather than spend; the
/// formatter says `<$0.01` instead of inventing a rounding.
pub const DUST_THRESHOLD: f64 = 0.01;

/// Currency symbol for a reporting-currency code.
fn currency_symbol(currency: &str) -> &str {
    match currency {
        "USD" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" | "CNY" => "¥",
        _ => "",
    }
}

/// Decimal places a currency amounts to; JPY has no minor unit.
fn currency_decimals(currency: &str) -> usize {
    match currency {
        "JPY" => 0,
        _ => 2,
    }
}

/// Amount with the currency's symbol, e.g. `$51,080`. Precision adapts to
/// the size so sub-dollar spend does not round to `$0`: whole units from
/// 100 up, two decimals from a cent up, and `<$0.01` below a cent. JPY has
/// no minor unit, so it always rounds to whole yen.
pub fn amount(value: f64, currency: &str) -> String {
    let symbol = currency_symbol(currency);
    let prefix = if symbol.is_empty() {
        format!("{currency} ")
    } else {
        symbol.to_string()
    };
    let decimals = currency_decimals(currency);

    let magnitude = value.abs();
    // Below half a cent is netting round-off, not an amount.
    if magnitude < 0.005 {
        return format!("{prefix}0");
    }
    let sign = if value < 0.0 { "-" } else { "" };
    if decimals == 2 && magnitude < DUST_THRESHOLD {
        // A sub-cent amount has no honest rounding; say so instead.
        return format!("{sign}<{prefix}{DUST_THRESHOLD}");
    }
    if decimals == 2 && magnitude < 100.0 {
        return format!("{sign}{prefix}{magnitude:.2}");
    }

    let rounded = value.round() as i64;
    let digits = rounded.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{sign}{prefix}{grouped}")
}

/// An amount to the currency's full precision, e.g. `$12,345.67`. For figures
/// that are checked against a bill, where [`amount`]'s rounding would hide the
/// difference.
pub fn exact_amount(value: f64, currency: &str) -> String {
    let symbol = currency_symbol(currency);
    let prefix = if symbol.is_empty() {
        format!("{currency} ")
    } else {
        symbol.to_string()
    };
    let decimals = currency_decimals(currency);
    let text = format!("{:.*}", decimals, value.abs());
    let (whole, fraction) = text.split_once('.').unwrap_or((&text, ""));
    let mut grouped = String::new();
    for (i, c) in whole.chars().enumerate() {
        if i > 0 && (whole.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    let zero = text.chars().all(|c| c == '0' || c == '.');
    let sign = if value < 0.0 && !zero { "-" } else { "" };
    if fraction.is_empty() {
        format!("{sign}{prefix}{grouped}")
    } else {
        format!("{sign}{prefix}{grouped}.{fraction}")
    }
}

/// [`exact_amount`] with a `+` on a rise, for a change between two figures.
pub fn signed_exact_amount(value: f64, currency: &str) -> String {
    let text = exact_amount(value, currency);
    let is_zero = !text.chars().any(|c| c.is_ascii_digit() && c != '0');
    if value > 0.0 && !is_zero {
        format!("+{text}")
    } else {
        text
    }
}

/// A large count with an adaptive magnitude suffix: whole units under a
/// thousand (`843`), one decimal with a k/M/B suffix above it (`12.4k`,
/// `3.4M`, `5.6B`). Token counts are what this is for — a raw `84123841`
/// is unreadable in a table cell. Sign and zero pass through the same path.
pub fn quantity(value: f64) -> String {
    let sign = if value < 0.0 { "-" } else { "" };
    let magnitude = value.abs();
    if magnitude < 1_000.0 {
        format!("{sign}{}", magnitude.round() as i64)
    } else if magnitude < 1_000_000.0 {
        format!("{sign}{:.1}k", magnitude / 1_000.0)
    } else if magnitude < 1_000_000_000.0 {
        format!("{sign}{:.1}M", magnitude / 1_000_000.0)
    } else {
        format!("{sign}{:.1}B", magnitude / 1_000_000_000.0)
    }
}

/// A per-million-token price with the currency's symbol, e.g. `$4.20/1M`.
/// `None` — no token base to divide by — reads as a dash rather than an
/// invented number.
pub fn unit_cost(cost: Option<f64>, currency: &str) -> String {
    match cost {
        Some(cost) => format!("{}/1M", amount(cost, currency)),
        None => "—".to_string(),
    }
}

/// A timestamp as an age: `just now`, `N min ago`, `N h ago`, `N d ago`,
/// or the calendar date once it is more than a week out.
pub fn relative_time(at: DateTime<Utc>) -> String {
    let elapsed = (Utc::now() - at).num_minutes().max(0);
    if elapsed < 1 {
        tr!("time.just_now").into()
    } else if elapsed < 60 {
        tr!("time.minutes_ago", n = elapsed).into()
    } else if elapsed < 48 * 60 {
        tr!("time.hours_ago", n = elapsed / 60).into()
    } else if elapsed < 7 * 24 * 60 {
        tr!("time.days_ago", n = elapsed / (24 * 60)).into()
    } else {
        at.format(&rust_i18n::t!("time.short_date")).to_string()
    }
}

/// A percentage change with its sign, e.g. `+12.5%`.
pub fn change_pct(pct: f64) -> String {
    format!("{pct:+.1}%")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_amounts_keep_the_minor_units_at_any_size() {
        assert_eq!(exact_amount(12345.678, "USD"), "$12,345.68");
        assert_eq!(exact_amount(-0.3, "USD"), "-$0.30");
        assert_eq!(exact_amount(-0.001, "USD"), "$0.00");
        assert_eq!(exact_amount(1500.4, "JPY"), exact_amount(1500.0, "JPY"));
        assert_eq!(signed_exact_amount(2.5, "USD"), "+$2.50");
        assert_eq!(signed_exact_amount(-2.5, "USD"), "-$2.50");
        assert_eq!(signed_exact_amount(0.0, "USD"), "$0.00");
        assert_eq!(signed_exact_amount(0.001, "USD"), "$0.00");
    }

    #[test]
    fn small_amounts_keep_two_decimals() {
        assert_eq!(amount(51.5, "USD"), "$51.50");
        assert_eq!(amount(0.99, "USD"), "$0.99");
        assert_eq!(amount(42.75, "CNY"), "¥42.75");
    }

    #[test]
    fn large_amounts_group_thousands_and_round_to_whole_units() {
        assert_eq!(amount(51080.4, "USD"), "$51,080");
        assert_eq!(amount(1_234_567.89, "USD"), "$1,234,568");
        assert_eq!(amount(100.0, "USD"), "$100");
    }

    #[test]
    fn a_negative_amount_keeps_its_sign_ahead_of_the_symbol() {
        assert_eq!(amount(-12.5, "USD"), "-$12.50");
        assert_eq!(amount(-51080.0, "USD"), "-$51,080");
    }

    #[test]
    fn sub_cent_amounts_are_dust_not_invented_roundings() {
        assert_eq!(amount(0.007, "USD"), "<$0.01");
        assert_eq!(amount(-0.007, "USD"), "-<$0.01");
        // Half a cent is round-off, not an amount at all.
        assert_eq!(amount(0.004, "USD"), "$0");
        assert_eq!(amount(0.0, "USD"), "$0");
    }

    #[test]
    fn yen_has_no_minor_unit() {
        assert_eq!(amount(500.6, "JPY"), "¥501");
        assert_eq!(amount(49.0, "JPY"), "¥49");
    }

    #[test]
    fn a_currency_with_no_known_symbol_is_named_instead() {
        assert_eq!(amount(12.5, "BTC"), "BTC 12.50");
    }

    #[test]
    fn a_change_percentage_carries_its_sign() {
        assert_eq!(change_pct(12.5), "+12.5%");
        assert_eq!(change_pct(-3.26), "-3.3%");
    }

    #[test]
    fn quantities_pick_a_readable_magnitude() {
        assert_eq!(quantity(0.0), "0");
        assert_eq!(quantity(843.0), "843");
        assert_eq!(quantity(999.4), "999");
        assert_eq!(quantity(12_400.0), "12.4k");
        assert_eq!(quantity(3_400_000.0), "3.4M");
        assert_eq!(quantity(5_600_000_000.0), "5.6B");
    }

    #[test]
    fn a_negative_quantity_keeps_its_sign() {
        assert_eq!(quantity(-843.0), "-843");
        assert_eq!(quantity(-12_400.0), "-12.4k");
    }

    #[test]
    fn a_unit_cost_prices_a_million_tokens() {
        assert_eq!(unit_cost(Some(4.2), "USD"), "$4.20/1M");
        assert_eq!(unit_cost(Some(140.0), "USD"), "$140/1M");
        assert_eq!(unit_cost(None, "USD"), "—");
    }
}
