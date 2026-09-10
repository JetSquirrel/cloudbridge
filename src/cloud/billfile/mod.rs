//! Bill file import: a provider's own bill export, read from a local file.
//!
//! This is the second channel into the ledger, beside the API fetch in
//! [`crate::cloud::BillingSource`]. An API call asks the provider what a
//! month cost and is billed per request; the provider's own bill export is
//! the same month at instance level, for free, with full history — and for
//! some providers it is the only channel that reports a model at all.
//!
//! Both channels land in `fct_charge` through the same
//! [`Normalized`], so nothing downstream can tell them apart. An import is
//! one [`crate::ledger`] whole-period replacement per period the file
//! covers, exactly like a fetch, which is what lets a bill export *replace*
//! a coarser API reading of the same month instead of being added to it.
//!
//! The file is copied into the raw partition byte for byte before anything
//! interprets it, so a mapping fix replays the import from the copy
//! CloudBridge kept rather than asking the user to find the download again.
//!
//! ## What a parser has to be
//!
//! Every format below reads through [`Sheet`], and every parser is a pure
//! function of one [`RawBatch`]. That is the same contract the API
//! normalizers hold, and it is what makes these testable from a recorded
//! export rather than from a live account.

pub mod aliyun;
pub mod anthropic;
pub mod detail_export;
pub mod openai;
pub mod usage_export;
pub mod volcengine;

use anyhow::{anyhow, Result};
use chrono::NaiveDate;

use super::{BillingPeriod, Normalized, RawBatch};

/// How one provider's bill export is read.
///
/// Registered on a [`crate::cloud::registry::SourceDescriptor`], so a
/// source gains a file channel by naming a format rather than by anything
/// downstream learning it exists.
pub struct BillFileFormat {
    /// What the provider calls this export. Named in the import dialog and
    /// in every error, because the commonest import failure is picking a
    /// different download from the same console.
    pub display_name: &'static str,
    /// Where the export is produced, for the dialog's hint.
    pub origin_hint: &'static str,
    /// Filename extensions the import accepts, lowercase and without the
    /// dot. Advisory: the file is parsed on its contents, not its name.
    pub extensions: &'static [&'static str],
    /// Part name the file's text is stored under in a raw batch.
    pub part: &'static str,
    /// The billing periods a file covers.
    ///
    /// Read before anything is written, because a period is replaced as a
    /// whole and one export routinely spans several months — a date range
    /// picked in a console has no reason to stop at a month boundary.
    pub periods: fn(&str) -> Result<Vec<BillingPeriod>>,
    /// Rows for the one period the batch is for. Pure, and reads only from
    /// the batch, exactly like an API normalizer.
    pub normalize: fn(&RawBatch) -> Result<Normalized>,
}

impl BillFileFormat {
    /// The extensions as a file dialog would list them, for a hint.
    pub fn extension_hint(&self) -> String {
        self.extensions
            .iter()
            .map(|extension| format!(".{}", extension))
            .collect::<Vec<_>>()
            .join(" or ")
    }
}

/// The text of the file a batch was imported from.
///
/// A parser's first line, and the reason both halves of a format can share
/// one error message.
pub fn text_of<'a>(batch: &'a RawBatch, part: &str) -> Result<&'a str> {
    Ok(batch
        .part(part)
        .ok_or_else(|| anyhow!("Raw batch has no '{}' payload", part))?
        .body
        .as_str())
}

// ==================== Reading a delimited export ====================

/// Delimiters an export might use, in the order they are tried.
///
/// A semicolon appears in exports produced for locales where the comma is
/// the decimal separator; a tab, in anything that has been through a
/// spreadsheet.
const DELIMITERS: [char; 3] = [',', '\t', ';'];

/// A delimited bill export, parsed into its header row and the rows below.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sheet {
    pub headers: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Sheet {
    /// Parse `text`, taking as the header the first row carrying one of
    /// `anchors`.
    ///
    /// The anchor is there because these exports are not always a bare
    /// table: Alibaba Cloud writes a title and the account the bill belongs
    /// to above the header, and a spreadsheet round trip can add more.
    /// Skipping to a row that names a column we know is how the table is
    /// found without hard-coding how tall the preamble is.
    ///
    /// Fails naming what it did see, because "this is not that export" is
    /// the single most likely thing to have gone wrong and the row it
    /// stopped on is the answer.
    pub fn parse(text: &str, anchors: &[&str]) -> Result<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        if text.trim().is_empty() {
            return Err(anyhow!("The file is empty"));
        }

        // Whichever delimiter yields the widest header row is the one in
        // use: a comma-delimited file contains no tabs to be split on, and
        // a tab-delimited one splits into a single column on commas.
        //
        // The first row is the tie-break, and it is what makes the failure
        // below readable: when no delimiter finds an anchor at all, the file
        // is still split the way it most plausibly wants to be, so the error
        // can list the columns it actually has instead of one long line.
        let rows = DELIMITERS
            .iter()
            .map(|delimiter| split_rows(text, *delimiter))
            .max_by_key(|rows| {
                (
                    header_row(rows, anchors).map(|(_, row)| row.len()),
                    rows.first().map_or(0, Vec::len),
                )
            })
            .unwrap_or_default();

        let (index, headers) = header_row(&rows, anchors).ok_or_else(|| {
            anyhow!(
                "No column named {} in the file. Its first line is: {}",
                or_list(anchors),
                rows.first()
                    .map(|row| row.join(" | "))
                    .unwrap_or_else(|| "(blank)".to_string())
            )
        })?;

        Ok(Self {
            headers: headers.clone(),
            // Trailing blank rows are what a spreadsheet leaves behind, and
            // a row of empty cells is not a charge.
            rows: rows[index + 1..]
                .iter()
                .filter(|row| row.iter().any(|cell| !cell.trim().is_empty()))
                .cloned()
                .collect(),
        })
    }

    /// Index of the first column whose header is one of `aliases`.
    pub fn column(&self, aliases: &[&str]) -> Option<usize> {
        let wanted: Vec<String> = aliases.iter().map(|alias| header_key(alias)).collect();
        self.headers
            .iter()
            .position(|header| wanted.contains(&header_key(header)))
    }

    /// [`Self::column`], but an error naming every header actually present
    /// when none of the aliases match.
    ///
    /// These formats are not specified anywhere and their headers differ by
    /// console locale and by export version, so a missing column is a
    /// routine event that has to be diagnosable from the message alone —
    /// the fix is to add an alias, and the message says which.
    pub fn require(&self, what: &str, aliases: &[&str]) -> Result<usize> {
        self.column(aliases).ok_or_else(|| {
            anyhow!(
                "This export has no {} column (looked for {}). \
                 The columns it does have are: {}",
                what,
                or_list(aliases),
                self.headers.join(", ")
            )
        })
    }

    /// Every row, as a lookup that tolerates a short row.
    ///
    /// A trailing empty cell is routinely omitted rather than written as an
    /// empty field, so indexing a row directly would panic on exactly the
    /// files that are hardest to get hold of.
    pub fn records(&self) -> impl Iterator<Item = Record<'_>> {
        self.rows.iter().map(|cells| Record { cells })
    }
}

/// One row of a [`Sheet`].
#[derive(Debug, Clone, Copy)]
pub struct Record<'a> {
    cells: &'a [String],
}

impl<'a> Record<'a> {
    /// The cell at `column`, trimmed, or `""` when the row stops short of
    /// it.
    pub fn get(&self, column: usize) -> &'a str {
        self.cells.get(column).map_or("", |cell| cell.trim())
    }

    /// The cell at `column`, or `None` when it is blank or absent.
    pub fn optional(&self, column: Option<usize>) -> Option<&'a str> {
        let cell = self.get(column?);
        (!cell.is_empty()).then_some(cell)
    }

    /// The cell at `column` as an owned string, or `None` when blank.
    pub fn text(&self, column: Option<usize>) -> Option<String> {
        self.optional(column).map(str::to_string)
    }

    /// The cell at an optional column as money — `None` for a column this
    /// export does not have, and for one it leaves blank.
    pub fn amount(&self, column: Option<usize>) -> Result<Option<f64>> {
        match self.optional(column) {
            Some(cell) => amount(cell),
            None => Ok(None),
        }
    }

    /// The cell at an optional column as a quantity — a token count, a
    /// request count. Read the same way as money; named apart so a parser
    /// says at the point of use which of the two a column holds.
    pub fn quantity(&self, column: Option<usize>) -> Result<Option<f64>> {
        self.amount(column)
    }
}

/// The first row carrying one of `anchors`, with its index.
fn header_row<'a>(rows: &'a [Vec<String>], anchors: &[&str]) -> Option<(usize, &'a Vec<String>)> {
    let wanted: Vec<String> = anchors.iter().map(|anchor| header_key(anchor)).collect();
    rows.iter()
        .enumerate()
        .find(|(_, row)| row.iter().any(|cell| wanted.contains(&header_key(cell))))
}

/// Split `text` on `delimiter`, following RFC 4180 quoting.
///
/// Deliberately minimal, in the manner of the profile-file reader in
/// [`crate::cloud::registry`]: quoted fields, doubled quotes inside them,
/// embedded delimiters and newlines, and CRLF. That is everything a
/// billing console emits.
fn split_rows(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                // A doubled quote is one literal quote; a single one ends
                // the field.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }

        match c {
            // Only an opening quote quotes: one in the middle of a field is
            // a character the provider meant literally.
            '"' if field.is_empty() => quoted = true,
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            '\r' => {}
            c => field.push(c),
        }
    }

    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// A header as it is compared.
///
/// Case, spacing, underscores and the bracket and colon forms — half-width
/// and full-width both — are noise: the same column is written `Product
/// Name`, `product_name` and `产品名称（明细）` across three exports of the
/// same bill.
fn header_key(header: &str) -> String {
    header
        .chars()
        .filter(|c| {
            !c.is_whitespace()
                && !matches!(
                    c,
                    '_' | '-' | '.' | ':' | '(' | ')' | '[' | ']' | '"' | '\''
                )
                && !matches!(c, '：' | '（' | '）' | '【' | '】' | '\u{3000}')
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// `a`, `b` or `c`, for a message listing what was looked for.
fn or_list(names: &[&str]) -> String {
    match names {
        [] => "nothing".to_string(),
        [only] => format!("'{}'", only),
        [rest @ .., last] => format!(
            "{} or '{}'",
            rest.iter()
                .map(|name| format!("'{}'", name))
                .collect::<Vec<_>>()
                .join(", "),
            last
        ),
    }
}

// ==================== Reading a cell ====================

/// A money cell as a number, or `None` when the cell is blank.
///
/// Errors rather than defaulting to zero. A column that has been mapped to
/// the wrong field parses as garbage, and a silent zero would leave a bill
/// that looks complete and totals wrong — the one failure this whole module
/// exists to avoid.
///
/// Handles what these exports actually contain: thousands separators, a
/// leading currency symbol, a parenthesised or full-width negative, and the
/// `-` some consoles write for "nothing".
pub fn amount(cell: &str) -> Result<Option<f64>> {
    let cell = cell.trim();
    if cell.is_empty() || cell == "-" || cell == "--" || cell == "N/A" {
        return Ok(None);
    }

    let negated = cell.starts_with('(') && cell.ends_with(')');
    let digits: String = cell
        .trim_start_matches('(')
        .trim_end_matches(')')
        .chars()
        .filter(|c| !matches!(c, ',' | ' ' | '¥' | '￥' | '$' | '€' | '£' | '\u{a0}'))
        .map(|c| match c {
            // Full-width minus and hyphen, as a Chinese console writes them.
            '\u{2212}' | '\u{ff0d}' => '-',
            c => c,
        })
        .collect();

    let value: f64 = digits
        .parse()
        .map_err(|_| anyhow!("{:?} is not an amount", cell))?;

    Ok(Some(if negated { -value } else { value }))
}

/// The billing period a date cell falls in, or `None` when it is blank.
///
/// Accepts every shape these exports use for a month or a day:
/// `2026-08`, `2026/08`, `202608`, `2026-08-01`, `2026-08-01 13:00:00`
/// and `2026-08-01T13:00:00Z`.
pub fn period(cell: &str) -> Option<BillingPeriod> {
    let (year, month, _) = year_month_day(cell)?;
    Some(BillingPeriod::new(year, month))
}

/// The day a date cell names, or `None` when it names only a month.
pub fn date(cell: &str) -> Option<NaiveDate> {
    let (year, month, day) = year_month_day(cell)?;
    NaiveDate::from_ymd_opt(year, month, day?)
}

/// Pull a year, a month and possibly a day out of a date cell.
fn year_month_day(cell: &str) -> Option<(i32, u32, Option<u32>)> {
    let cell = cell.trim();
    // Everything before the time, if there is one, then the digit groups.
    let groups: Vec<&str> = cell
        .split(['T', ' '])
        .next()?
        .split(['-', '/', '.'])
        .collect();

    let (year, month, day) = match groups.as_slice() {
        // A bare YYYYMM or YYYYMMDD, with no separators at all.
        [single] if single.len() == 6 => (&single[..4], &single[4..6], None),
        [single] if single.len() == 8 => (&single[..4], &single[4..6], Some(&single[6..8])),
        [year, month] => (*year, *month, None),
        [year, month, day, ..] => (*year, *month, Some(*day)),
        _ => return None,
    };

    let year: i32 = year.parse().ok()?;
    let month: u32 = month.parse().ok()?;
    if !(1..=12).contains(&month) {
        return None;
    }

    Some((year, month, day.and_then(|day| day.parse().ok())))
}

/// A tag cell as the JSON object `fct_charge.tags` holds.
///
/// Alibaba Cloud and Volcengine both write tags as one cell of `key:value`
/// pairs separated by semicolons. A pair with no value is kept with an
/// empty one — the key having been applied at all is the fact a tag view
/// needs, and dropping it would understate what is allocated.
///
/// `None` when the cell is blank or holds nothing that parses, so a source
/// with no tags writes NULL rather than an empty object.
pub fn tags_json(cell: &str) -> Option<String> {
    let pairs: Vec<(String, String)> = cell
        .split([';', '；', ','])
        .filter_map(|pair| {
            let pair = pair.trim();
            if pair.is_empty() {
                return None;
            }
            let (key, value) = pair.split_once([':', '：', '=']).unwrap_or((pair, ""));
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_string(), value.trim().to_string()))
        })
        .collect();

    if pairs.is_empty() {
        return None;
    }

    let object: serde_json::Map<String, serde_json::Value> = pairs
        .into_iter()
        .map(|(key, value)| (key, serde_json::Value::String(value)))
        .collect();
    Some(serde_json::Value::Object(object).to_string())
}

/// Every period a column of date cells covers, oldest first.
///
/// The shape of [`BillFileFormat::periods`] for a format whose period is
/// one column: read the column, keep one of each.
pub fn periods_in_column(sheet: &Sheet, column: usize) -> Vec<BillingPeriod> {
    let mut periods: Vec<BillingPeriod> = Vec::new();
    for record in sheet.records() {
        if let Some(found) = period(record.get(column)) {
            if !periods.contains(&found) {
                periods.push(found);
            }
        }
    }
    periods.sort_by_key(|period| (period.year, period.month));
    periods
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_field_can_hold_the_delimiter_and_a_newline() {
        let sheet = Sheet::parse(
            "BillingCycle,ProductName\n2026-08,\"Model Studio, \"\"Bailian\"\"\nsecond line\"\n",
            &["BillingCycle"],
        )
        .unwrap();

        assert_eq!(sheet.rows.len(), 1);
        assert_eq!(sheet.rows[0][1], "Model Studio, \"Bailian\"\nsecond line");
    }

    #[test]
    fn a_tab_delimited_export_is_read_as_one() {
        let sheet = Sheet::parse(
            "BillingCycle\tProductName\n2026-08\tBailian, Model Studio\n",
            &["BillingCycle"],
        )
        .unwrap();

        assert_eq!(sheet.headers, vec!["BillingCycle", "ProductName"]);
        // The comma inside the cell is not a delimiter in this file.
        assert_eq!(sheet.rows[0][1], "Bailian, Model Studio");
    }

    #[test]
    fn the_table_is_found_under_a_preamble() {
        let sheet = Sheet::parse(
            "Alibaba Cloud bill detail\nAccount,1234567890\n\n\
             BillingCycle,ProductName\n2026-08,Model Studio\n",
            &["BillingCycle"],
        )
        .unwrap();

        assert_eq!(sheet.headers, vec!["BillingCycle", "ProductName"]);
        assert_eq!(sheet.rows.len(), 1);
    }

    #[test]
    fn a_byte_order_mark_is_not_part_of_the_first_header() {
        let sheet = Sheet::parse("\u{feff}账期,产品名称\n2026-08,百炼\n", &["账期"]).unwrap();
        assert_eq!(sheet.column(&["账期"]), Some(0));
    }

    #[test]
    fn a_header_matches_however_it_is_punctuated() {
        let sheet = Sheet::parse(
            "Billing Cycle,product_name,应付金额（元）\n2026-08,Bailian,1.00\n",
            &["BillingCycle"],
        )
        .unwrap();

        assert_eq!(sheet.column(&["billingcycle"]), Some(0));
        assert_eq!(sheet.column(&["ProductName"]), Some(1));
        assert_eq!(sheet.column(&["应付金额(元)"]), Some(2));
    }

    #[test]
    fn a_missing_column_names_the_ones_that_are_there() {
        let sheet = Sheet::parse("账期,产品名称\n2026-08,百炼\n", &["账期"]).unwrap();

        let error = sheet
            .require("net amount", &["应付金额", "PretaxAmount"])
            .unwrap_err()
            .to_string();
        // Both halves matter: what it wanted, and what the file holds.
        assert!(error.contains("'应付金额'"), "{}", error);
        assert!(error.contains("账期, 产品名称"), "{}", error);
    }

    #[test]
    fn a_file_that_is_not_this_export_says_what_it_saw() {
        let error = Sheet::parse("id,name\n1,ecs\n", &["账期", "BillingCycle"])
            .unwrap_err()
            .to_string();
        assert!(error.contains("id | name"), "{}", error);
    }

    #[test]
    fn a_row_that_stops_short_is_read_as_blank_not_a_panic() {
        let sheet = Sheet::parse("账期,产品名称,应付金额\n2026-08,百炼\n", &["账期"]).unwrap();
        let record = sheet.records().next().unwrap();

        assert_eq!(record.get(2), "");
        assert_eq!(record.optional(Some(2)), None);
    }

    #[test]
    fn blank_rows_are_not_charges() {
        let sheet = Sheet::parse(
            "账期,产品名称\n2026-08,百炼\n,\n\n2026-08,方舟\n",
            &["账期"],
        )
        .unwrap();
        assert_eq!(sheet.rows.len(), 2);
    }

    #[test]
    fn an_empty_file_is_an_error_rather_than_an_empty_bill() {
        assert!(Sheet::parse("   \n", &["账期"]).is_err());
    }

    #[test]
    fn an_amount_is_read_through_whatever_decoration_it_carries() {
        assert_eq!(amount("1234.56").unwrap(), Some(1234.56));
        assert_eq!(amount("¥1,234.56").unwrap(), Some(1234.56));
        assert_eq!(amount("$0.0021").unwrap(), Some(0.0021));
        assert_eq!(amount("(12.50)").unwrap(), Some(-12.50));
        assert_eq!(amount("\u{2212}12.50").unwrap(), Some(-12.50));
        assert_eq!(amount("").unwrap(), None);
        assert_eq!(amount("-").unwrap(), None);
    }

    /// A column mapped to the wrong field must not total as zero.
    #[test]
    fn a_cell_that_is_not_an_amount_is_an_error() {
        let error = amount("Model Studio").unwrap_err().to_string();
        assert!(error.contains("not an amount"), "{}", error);
    }

    #[test]
    fn a_date_cell_is_read_in_every_shape_these_exports_use() {
        for cell in [
            "2026-08",
            "2026/08",
            "202608",
            "2026-08-01",
            "2026-08-01 13:45:00",
            "2026-08-01T13:45:00Z",
            "20260801",
        ] {
            assert_eq!(period(cell), Some(BillingPeriod::new(2026, 8)), "{}", cell);
        }

        assert_eq!(date("2026-08-09"), NaiveDate::from_ymd_opt(2026, 8, 9));
        // A month alone names no day.
        assert_eq!(date("2026-08"), None);
        assert_eq!(period("not a date"), None);
        assert_eq!(period("2026-13"), None);
    }

    #[test]
    fn a_tag_cell_becomes_the_json_object_the_ledger_stores() {
        assert_eq!(
            tags_json("env:prod; owner:platform"),
            Some(r#"{"env":"prod","owner":"platform"}"#.to_string())
        );
        // A key applied with no value is still an applied key.
        assert_eq!(tags_json("env"), Some(r#"{"env":""}"#.to_string()));
        assert_eq!(tags_json("  "), None);
    }

    #[test]
    fn the_periods_a_file_covers_come_back_oldest_first_without_repeats() {
        let sheet = Sheet::parse(
            "账期,应付金额\n2026-09-03,1\n2026-08-31,2\n2026-09-01,3\n",
            &["账期"],
        )
        .unwrap();

        assert_eq!(
            periods_in_column(&sheet, 0),
            vec![BillingPeriod::new(2026, 8), BillingPeriod::new(2026, 9)]
        );
    }
}
