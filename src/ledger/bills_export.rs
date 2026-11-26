//! The Bills page's line list written out as CSV.
//!
//! The page shows one page of charges at a time; this writes every charge
//! the same filter selects, so a spreadsheet can hold what the screen
//! cannot. Amounts are in the reporting currency, converted the way the
//! view converts them, and a line no rate covers keeps an empty amount
//! rather than a zero nobody was charged.

use std::path::Path;

use anyhow::{anyhow, Result};

use crate::model::{BillFilter, BillLine};

/// The columns written, in order. English on purpose, like the FOCUS
/// export: a spreadsheet's headers outlive the interface language.
const HEADERS: [&str; 15] = [
    "Day",
    "Provider",
    "Account",
    "Service",
    "Description",
    "Model",
    "Category",
    "Quantity",
    "Unit",
    "Billed",
    "List",
    "Effective",
    "CostBasis",
    "Credential",
    "Principal",
];

/// Write `lines` to `path` as CSV and return how many were written.
pub fn export_csv(path: &Path, lines: &[BillLine]) -> Result<usize> {
    let mut out = String::new();
    out.push_str(&HEADERS.map(quote).join(","));
    out.push('\n');

    for line in lines {
        let row = [
            quote(line.day.as_str()),
            quote(&line.provider),
            quote(&line.account_id),
            quote(&line.service),
            quote(line.description.as_deref().unwrap_or("")),
            quote(line.model.as_deref().unwrap_or("")),
            quote(line.category.as_str()),
            number(line.quantity),
            quote(line.unit.as_deref().unwrap_or("")),
            number(line.billed),
            number(line.list),
            number(line.effective),
            quote(line.basis.as_str()),
            quote(line.credential.as_deref().unwrap_or("")),
            quote(line.principal.as_deref().unwrap_or("")),
        ];
        out.push_str(&row.join(","));
        out.push('\n');
    }

    std::fs::write(path, out).map_err(|e| anyhow!("Could not write {}: {}", path.display(), e))?;
    Ok(lines.len())
}

/// A field as CSV wants it: quoted when it holds a comma, a quote or a line
/// break, with its own quotes doubled.
fn quote(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

/// A number with at most six decimals and no trailing zeros; empty for
/// `None`, which is not the same as zero.
fn number(value: Option<f64>) -> String {
    match value {
        Some(value) => {
            let text = format!("{value:.6}");
            let trimmed = text.trim_end_matches('0').trim_end_matches('.');
            if trimmed.is_empty() || trimmed == "-" {
                "0".to_string()
            } else {
                trimmed.to_string()
            }
        }
        None => String::new(),
    }
}

/// Write every charge `filter` selects in `[since, until)` to `path`.
/// Blocking.
pub fn export_between(
    path: &Path,
    since: chrono::DateTime<chrono::Utc>,
    until: chrono::DateTime<chrono::Utc>,
    filter: &BillFilter,
) -> Result<usize> {
    let lines = crate::ledger::query::bill_lines_between(since, until, filter, usize::MAX, 0)?;
    export_csv(path, &lines)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ChargeCategory, CostBasis};

    fn line(description: &str) -> BillLine {
        BillLine {
            day: "2026-11-01".to_string(),
            provider: "AWS".to_string(),
            account_id: "123".to_string(),
            service: "EC2".to_string(),
            description: Some(description.to_string()),
            model: None,
            category: ChargeCategory::Usage,
            quantity: Some(24.0),
            unit: Some("Hrs".to_string()),
            billed: Some(1.5),
            list: None,
            effective: None,
            basis: CostBasis::Authoritative,
            credential: None,
            principal: None,
        }
    }

    #[test]
    fn a_field_with_a_comma_or_a_quote_is_quoted_and_its_quotes_doubled() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("a,b"), "\"a,b\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(quote("one\ntwo"), "\"one\ntwo\"");
    }

    #[test]
    fn an_absent_amount_is_empty_rather_than_zero() {
        assert_eq!(number(None), "");
        assert_eq!(number(Some(0.0)), "0");
        assert_eq!(number(Some(1.25)), "1.25");
        assert_eq!(number(Some(1.0 / 3.0)), "0.333333");
    }

    #[test]
    fn a_written_file_has_a_header_and_one_row_per_line() {
        let dir = std::env::temp_dir().join(format!("cloudbridge-bills-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("bills.csv");

        let rows = export_csv(&path, &[line("plain"), line("a,b")]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_dir_all(&dir).ok();

        assert_eq!(rows, 2);
        assert_eq!(text.lines().count(), 3);
        assert!(text.starts_with("Day,Provider,Account,Service,Description,"));
        assert!(text.contains("\"a,b\""));
    }
}
