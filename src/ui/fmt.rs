//! Amount formatting shared by the pages and the chart tooltip.

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

/// Amount with the currency's symbol, e.g. `$51,080`. Precision adapts to
/// the size so sub-dollar spend does not round to `$0`: whole units from
/// 100 up, two decimals from a cent up, and `<$0.01` below a cent.
pub fn amount(value: f64, currency: &str) -> String {
    let symbol = currency_symbol(currency);
    let prefix = if symbol.is_empty() {
        format!("{currency} ")
    } else {
        symbol.to_string()
    };

    let magnitude = value.abs();
    // Below half a cent is netting round-off, not an amount.
    if magnitude < 0.005 {
        return format!("{prefix}0");
    }
    let sign = if value < 0.0 { "-" } else { "" };
    if magnitude < 0.01 {
        // A sub-cent amount has no honest rounding; say so instead.
        return format!("{sign}<{prefix}0.01");
    }
    if magnitude < 100.0 {
        return format!("{sign}{prefix}{magnitude:.2}");
    }

    let rounded = value.round() as i64;
    let sign = if rounded < 0 { "-" } else { "" };
    let digits = rounded.unsigned_abs().to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(c);
    }
    format!("{}{}{}", sign, prefix, grouped)
}
