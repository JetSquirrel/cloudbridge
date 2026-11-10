//! The two ledgers, held to the same answers.
//!
//! The desktop reads the ledger with SQL (`src/ledger/`); the browser demo
//! reads the same rows with folds over vectors (`src/web/ledger/`). The wasm
//! build only proves the two have the same signatures. This compiles the
//! browser's ledger into the desktop's tests, writes one demo bill into
//! both, and asks each read the pages make of both.
//!
//! A read is compared by its `Debug` output, numbers within a rounding
//! tolerance — the two sum in different orders — and everything else
//! exactly: which rows, in which order, under which names. A new read
//! belongs in [`compare`]; a difference that is meant is narrowed there in
//! the open, with the reason, rather than left out.

// The browser's ledger and the store under it. Much of each is there for
// the pages, which these tests do not build.
#[allow(dead_code, unused_imports)]
#[path = "web/ledger/mod.rs"]
mod ledger;
#[allow(dead_code)]
#[path = "web/memory.rs"]
mod memory;

use std::fmt::Debug;

use anyhow::Result;
use chrono::{DateTime, Duration, TimeZone, Utc};

use crate::demo_data::{self, DEMO_SOURCES};
use crate::ledger as native;
use crate::model::{BillingPeriod, BreakdownDim, Channel, Charge, ChargeCategory, PeriodKey};
use ledger as web;
use memory::Connection as WebConnection;

/// The tag the demo bill allocates by, and the pages read.
const TAG: &str = "business_line";

/// The account [`edge_charges`] are written under, beside the demo's own.
const EDGE_ACCOUNT: (&str, &str) = ("AWS", "parity-edge");

/// Rows the demo bill has few or none of, for one period: a charge with no
/// service and one with an empty service name, an empty tag value, tags
/// with no allocation key, a negative adjustment, a row with no amount,
/// a credit larger than the usage it offsets, and exact ties in amount
/// across services and resources.
fn edge_charges(period: BillingPeriod, now: DateTime<Utc>) -> Vec<Charge> {
    let start = Utc.from_utc_datetime(&period.start().and_hms_opt(0, 0, 0).unwrap());
    let day = Duration::days(1);
    let row = |offset: i64, service: Option<&str>, amount: Option<f64>, tags: Option<&str>| {
        let from = start + day * offset as i32;
        let mut charge = Charge::new(from, from + day, "USD");
        charge.service_name = service.map(str::to_string);
        charge.billed_cost = amount;
        charge.effective_cost = amount;
        charge.tags = tags.map(str::to_string);
        charge
    };

    let mut charges = vec![
        row(0, None, Some(12.5), Some(r#"{"business_line":"core"}"#)),
        row(0, Some(""), Some(4.0), None),
        row(0, Some("EC2"), Some(25.0), Some(r#"{"business_line":""}"#)),
        row(
            1,
            Some("S3"),
            Some(25.0),
            Some(r#"{"business_line":"growth"}"#),
        ),
        row(
            1,
            Some("Lambda"),
            None,
            Some(r#"{"business_line":"growth"}"#),
        ),
        row(1, Some("RDS"), Some(6.0), Some(r#"{"team":"data"}"#)),
        row(1, Some("RDS"), Some(3.0), Some("{}")),
    ];

    let mut adjustment = row(1, Some("EC2"), Some(-7.0), None);
    adjustment.charge_category = ChargeCategory::Adjustment;
    charges.push(adjustment);

    for (name, region) in [("i-edge-a", Some("us-east-1")), ("i-edge-b", None)] {
        let mut resource = row(
            0,
            Some("EC2"),
            Some(9.0),
            Some(r#"{"business_line":"core"}"#),
        );
        resource.resource_id = Some(name.to_string());
        resource.region_id = region.map(str::to_string);
        charges.push(resource);
    }

    // A refund that outweighs the period's usage.
    let mut credit = row(0, None, Some(-500.0), None);
    credit.charge_category = ChargeCategory::Credit;
    credit.charge_description = Some("Refund".to_string());
    charges.push(credit);

    charges.retain(|charge| charge.charge_period_start <= now);
    charges
}

/// Write the demo bill as of `now` into both ledgers, the way each target's
/// `ledger::demo` does, less the accounts: those are application state.
fn seed(now: DateTime<Utc>, reporting_currency: &str, edge: bool) -> Vec<BillingPeriod> {
    native::open_test_ledger(reporting_currency).expect("test ledger opens");
    web::init_ledger(reporting_currency).expect("memory ledger opens");

    let periods = demo_data::periods(now);
    for (provider, account_id, _, currency) in DEMO_SOURCES {
        let services = demo_data::services_of(provider);
        for (index, period) in periods.iter().enumerate() {
            let mut charges =
                demo_data::period_charges(provider, services, currency, *period, index, now);
            if *provider == "AWS" && index + 1 == periods.len() {
                charges.extend(demo_data::inventory::resource_charges(now));
            }
            let key = PeriodKey::new(*provider, *account_id, period.label());
            let batch_id = demo_data::batch_id(provider, *period);
            native::replace_period(&key, &batch_id, &charges, None, Channel::Api)
                .expect("native period written");
            web::replace_period(&key, &batch_id, &charges, None, Channel::Api)
                .expect("memory period written");
        }
    }

    if edge {
        let (provider, account_id) = EDGE_ACCOUNT;
        let current = *periods.last().expect("the demo has periods");
        for period in [current.previous(), current] {
            let key = PeriodKey::new(provider, account_id, period.label());
            let batch_id = format!("edge-{}", period.label());
            let charges = edge_charges(period, now);
            native::replace_period(&key, &batch_id, &charges, None, Channel::Api)
                .expect("native edge period written");
            web::replace_period(&key, &batch_id, &charges, None, Channel::Api)
                .expect("memory edge period written");
        }
    }

    for snapshot in demo_data::balance_ladder(&periods) {
        native::record_balance(&snapshot).expect("native balance written");
        web::record_balance(&snapshot).expect("memory balance written");
    }

    let invoices = demo_data::invoices(&periods, now);
    native::invoice::save_invoices(&invoices).expect("native invoices written");
    web::invoice::save_invoices(&invoices).expect("memory invoices written");

    let (scope, resources) = demo_data::inventory::inventory(now);
    native::with_connection(|conn| native::inventory::write_inventory(conn, &scope, &resources))
        .expect("native inventory written");
    web::inventory::write_inventory(&scope, &resources);

    // As the desktop's seed does: `replace_period` bypasses the ingest hook
    // that keeps the day rollup, and the daily reads prefer the rollup.
    native::rollup::rebuild_all().expect("rollup rebuilt");

    periods
}

/// The differences found, so one run names every read that disagrees.
struct Report {
    differences: Vec<String>,
}

impl Report {
    fn same<T: Debug>(&mut self, read: &str, native: Result<T>, web: Result<T>) {
        let difference = match (native, web) {
            (Ok(native), Ok(web)) => first_difference(&format!("{native:?}"), &format!("{web:?}")),
            (Err(_), Err(_)) => None,
            (Ok(_), Err(e)) => Some(format!("only the browser failed: {e:#}")),
            (Err(e), Ok(_)) => Some(format!("only the desktop failed: {e:#}")),
        };
        if let Some(difference) = difference {
            self.differences.push(format!("{read}: {difference}"));
        }
    }
}

/// A piece of `Debug` output: a number, or the text between two numbers.
#[derive(Debug, PartialEq)]
enum Token<'a> {
    Number(&'a str),
    Text(&'a str),
}

/// Split `Debug` output into numbers and the text around them.
///
/// A number starts at a digit that does not continue a word, so the `2` of
/// `EC2` stays text. Anything else that looks like a number — the parts of
/// a date, say — is compared as one, which is still an exact comparison for
/// integers.
fn tokens(text: &str) -> Vec<Token<'_>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut text_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let in_word = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        if !bytes[i].is_ascii_digit() || in_word {
            i += 1;
            continue;
        }
        let start = if i > 0 && bytes[i - 1] == b'-' {
            i - 1
        } else {
            i
        };
        let mut end = i;
        while end < bytes.len()
            && (bytes[end].is_ascii_digit()
                || bytes[end] == b'.'
                || bytes[end] == b'e'
                || (bytes[end] == b'-' && bytes[end - 1] == b'e'))
        {
            end += 1;
        }
        if text_start < start {
            tokens.push(Token::Text(&text[text_start..start]));
        }
        tokens.push(Token::Number(&text[start..end]));
        text_start = end;
        i = end;
    }
    if text_start < text.len() {
        tokens.push(Token::Text(&text[text_start..]));
    }

    tokens
}

/// Whether two numbers are the same figure. Integers must match exactly;
/// a float may differ by what summing in another order costs.
fn same_number(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let float = |s: &str| s.contains(['.', 'e']);
    if !float(a) || !float(b) {
        return false;
    }
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(a), Ok(b)) => (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0),
        _ => false,
    }
}

/// Where two `Debug` outputs first part ways, with what leads up to it.
fn first_difference(native: &str, web: &str) -> Option<String> {
    let (native_tokens, web_tokens) = (tokens(native), tokens(web));
    let agree = |(a, b): (&Token, &Token)| match (a, b) {
        (Token::Number(a), Token::Number(b)) => same_number(a, b),
        (a, b) => a == b,
    };
    let at = native_tokens
        .iter()
        .zip(&web_tokens)
        .position(|pair| !agree(pair))
        .or_else(|| {
            (native_tokens.len() != web_tokens.len())
                .then(|| native_tokens.len().min(web_tokens.len()))
        })?;

    let around = |tokens: &[Token]| {
        let from = at.saturating_sub(6);
        let to = (at + 4).min(tokens.len());
        tokens[from..to]
            .iter()
            .map(|token| match token {
                Token::Number(s) | Token::Text(s) => *s,
            })
            .collect::<String>()
    };
    Some(format!(
        "\n    desktop: …{}…\n    browser: …{}…",
        around(&native_tokens),
        around(&web_tokens)
    ))
}

/// Ask both ledgers every read the pages make, as of `now`, over the demo
/// bill and, with `edge`, [`edge_charges`] beside it.
fn compare(now: DateTime<Utc>, reporting_currency: &str, edge: bool) {
    use native::query as n;
    use web::query as w;

    let periods = seed(now, reporting_currency, edge);
    let mut sources: Vec<(&str, &str)> = DEMO_SOURCES
        .iter()
        .map(|(provider, account_id, _, _)| (*provider, *account_id))
        .collect();
    if edge {
        sources.push(EDGE_ACCOUNT);
    }
    let mut report = Report {
        differences: Vec::new(),
    };

    // The current and previous periods, and the oldest: on a bill a year
    // long that one predates the first built-in exchange rate.
    let current = *periods.last().expect("the demo has periods");
    let labels = [
        current.label(),
        current.previous().label(),
        periods[0].label(),
    ];

    // The windows the range switch asks for, and the one before each.
    let windows = [30, 365].map(|days| {
        let since = now - Duration::days(days);
        (since - Duration::days(days), since, now)
    });

    for label in &labels {
        let read = |name: &str| format!("{name}({label})");

        report.same(
            &read("total_for_period"),
            n::total_for_period(label),
            w::total_for_period(label),
        );
        report.same(
            &read("usage_and_credits"),
            n::usage_and_credits(label),
            w::usage_and_credits(label),
        );
        report.same(
            &read("rows_without_amount"),
            n::rows_without_amount(label),
            w::rows_without_amount(label),
        );
        report.same(
            &read("provider_service_usage"),
            n::provider_service_usage(label),
            w::provider_service_usage(label),
        );
        report.same(
            &read("tag_usage_breakdown"),
            n::tag_usage_breakdown(label, TAG),
            w::tag_usage_breakdown(label, TAG),
        );
        report.same(
            &read("tag_usage_breakdown_by_service"),
            n::tag_usage_breakdown_by_service(label, TAG),
            w::tag_usage_breakdown_by_service(label, TAG),
        );
        report.same(
            &read("untagged_totals_by_account"),
            n::untagged_totals_by_account(label, TAG),
            w::untagged_totals_by_account(label, TAG),
        );
        report.same(
            &read("untagged_totals_by_provider"),
            n::untagged_totals_by_provider(label, TAG),
            w::untagged_totals_by_provider(label, TAG),
        );
        report.same(
            &read("untagged_usage_by_service"),
            n::untagged_usage_by_service(label, TAG, 5),
            w::untagged_usage_by_service(label, TAG, 5),
        );
        report.same(
            &read("resource_usage_quantities"),
            n::resource_usage_quantities(label),
            w::resource_usage_quantities(label),
        );
        report.same(
            &read("resource_usage_costs"),
            n::resource_usage_costs(label),
            w::resource_usage_costs(label),
        );
        report.same(
            &read("period_over_period"),
            n::period_over_period(label),
            w::period_over_period(label),
        );
        report.same(
            &read("cost_change_decomposition"),
            n::cost_change_decomposition(label),
            w::cost_change_decomposition(label),
        );
        report.same(
            &read("data_quality_issues"),
            n::data_quality_issues(label, TAG),
            w::data_quality_issues(label, TAG),
        );
        report.same(
            &read("forecast_for_period"),
            native::with_connection_ref(|conn| n::forecast_for_period_of(conn, label, now)),
            w::forecast_at(label, now),
        );
        report.same(
            &read("forecast_bands_for_period"),
            native::with_connection_ref(|conn| n::forecast_bands_for_period_of(conn, label, now)),
            w::forecast_bands_at(label, now),
        );
        for scope in [None, Some(("AWS", "EC2"))] {
            report.same(
                &format!("tag_breakdown_of({label}, {scope:?})"),
                native::with_connection_ref(|conn| n::tag_breakdown_of(conn, label, TAG, scope)),
                w::tag_breakdown_of(&WebConnection, label, TAG, scope),
            );
        }

        for &(provider, account_id) in &sources {
            let key = PeriodKey::new(provider, account_id, label.clone());
            let read = |name: &str| format!("{name}({account_id}, {label})");

            report.same(
                &read("period_total"),
                n::period_total(&key),
                w::period_total(&key),
            );
            report.same(
                &read("channel_of"),
                n::channel_of(&key),
                w::channel_of(&key),
            );
            for dim in [
                BreakdownDim::Service,
                BreakdownDim::Region,
                BreakdownDim::ServiceCategory,
            ] {
                report.same(
                    &read(&format!("breakdown_by[{dim:?}]")),
                    n::breakdown_by(&key, dim),
                    w::breakdown_by(&key, dim),
                );
            }
            for (outer, inner) in [
                (BreakdownDim::Service, BreakdownDim::Resource),
                (BreakdownDim::ServiceCategory, BreakdownDim::Service),
                (BreakdownDim::Region, BreakdownDim::Service),
            ] {
                report.same(
                    &read(&format!("breakdown_within[{outer:?}, {inner:?}]")),
                    n::breakdown_within(&key, outer, inner),
                    w::breakdown_within(&key, outer, inner),
                );
            }
            report.same(
                &read("top_resources"),
                n::top_resources(&key, 10),
                w::top_resources(&key, 10),
            );
            report.same(
                &read("account_data_quality_issues"),
                n::account_data_quality_issues(label, TAG, account_id),
                w::account_data_quality_issues(label, TAG, account_id),
            );
            report.same(
                &read("account_forecast"),
                native::with_connection_ref(|conn| {
                    n::account_forecast_of(conn, provider, account_id, current, now)
                }),
                w::account_forecast_of(&WebConnection, provider, account_id, current, now),
            );
            report.same(
                &read("invoice::billed_totals"),
                native::invoice::billed_totals(&key),
                web::invoice::billed_totals(&key),
            );
            report.same(
                &read("snapshot::current_lines"),
                native::snapshot::current_lines(&key),
                web::snapshot::current_lines(&key),
            );
        }
    }

    for (prev_since, since, until) in windows {
        let days = (until - since).num_days();
        let read = |name: &str| format!("{name}(last {days} days)");

        report.same(
            &read("daily_usage_all"),
            n::daily_usage_all(since),
            w::daily_usage_all(since),
        );
        report.same(
            &read("monthly_usage"),
            n::monthly_usage(since),
            w::monthly_usage(since),
        );
        report.same(
            &read("total_between"),
            n::total_between(since, until),
            w::total_between(since, until),
        );
        report.same(
            &read("unconverted_between"),
            n::unconverted_between(since, until),
            w::unconverted_between(since, until),
        );
        report.same(
            &read("usage_and_credits_between"),
            n::usage_and_credits_between(since, until),
            w::usage_and_credits_between(since, until),
        );
        report.same(
            &read("provider_service_usage_between"),
            n::provider_service_usage_between(since, until),
            w::provider_service_usage_between(since, until),
        );
        report.same(
            &read("tag_usage_breakdown_between"),
            n::tag_usage_breakdown_between(since, until, TAG),
            w::tag_usage_breakdown_between(since, until, TAG),
        );
        report.same(
            &read("service_tag_usage_breakdown_between"),
            n::service_tag_usage_breakdown_between(since, until, "AWS", "EC2", TAG),
            w::service_tag_usage_breakdown_between(since, until, "AWS", "EC2", TAG),
        );
        report.same(
            &read("model_token_summary"),
            n::model_token_summary(since, until, prev_since, since),
            w::model_token_summary(since, until, prev_since, since),
        );
        report.same(
            &read("daily_model_tokens"),
            n::daily_model_tokens(since, until),
            w::daily_model_tokens(since, until),
        );
        report.same(
            &read("daily_totals_by_service"),
            native::with_connection_ref(|conn| n::daily_totals_by_service_of(conn, since)),
            w::daily_totals_by_service_of(&WebConnection, since),
        );

        for &(provider, account_id) in &sources {
            let read = |name: &str| format!("{name}({account_id}, last {days} days)");

            report.same(
                &read("daily_usage_of"),
                n::daily_usage_of(provider, account_id, since),
                w::daily_usage_of(provider, account_id, since),
            );
            report.same(
                &read("monthly_usage_of"),
                n::monthly_usage_of(provider, account_id, since),
                w::monthly_usage_of(provider, account_id, since),
            );
            report.same(
                &read("service_usage_of_between"),
                n::service_usage_of_between(provider, account_id, since, until),
                w::service_usage_of_between(provider, account_id, since, until),
            );
            report.same(
                &read("usage_and_credits_of_between"),
                n::usage_and_credits_of_between(provider, account_id, since, until),
                w::usage_and_credits_of_between(provider, account_id, since, until),
            );
            report.same(
                &read("daily_totals_for_account"),
                native::with_connection_ref(|conn| {
                    n::daily_totals_for_account_of(conn, account_id, since)
                }),
                w::daily_totals_for_account_of(&WebConnection, account_id, since),
            );
        }
    }

    // The Overview's baseline: the same days of each of the last months.
    for (day_count, months) in [(31, 3), (i64::from(chrono::Datelike::day(&now)), 1), (0, 3)] {
        report.same(
            &format!("trailing_daily_average({day_count}, {months})"),
            native::with_connection_ref(|conn| {
                n::trailing_daily_average_of(conn, day_count, months, now)
            }),
            w::trailing_daily_average_at(day_count, months, now),
        );
    }

    for &(provider, account_id) in &sources {
        report.same(
            &format!("latest_balance({account_id})"),
            n::latest_balance(provider, account_id),
            w::latest_balance(provider, account_id),
        );
        report.same(
            &format!("snapshot::periods_of({account_id})"),
            native::snapshot::periods_of(account_id),
            web::snapshot::periods_of(account_id),
        );
        for days in [7, 30, 400] {
            report.same(
                &format!("balance_burn({account_id}, {days} days)"),
                native::with_connection_ref(|conn| {
                    n::balance_burn_at(conn, provider, account_id, days, now)
                }),
                w::balance_burn_at(provider, account_id, days, now),
            );
        }
    }

    report.same(
        "api_fetches_this_month",
        native::with_connection_ref(|conn| n::api_fetches_in_month_of(conn, now)),
        w::api_fetches_in_month_at(now),
    );

    report.same(
        "inventory_resources",
        n::inventory_resources(),
        w::inventory_resources(),
    );
    report.same(
        "inventory_scope",
        n::inventory_scope(),
        w::inventory_scope(),
    );
    report.same(
        "invoice::all_invoices",
        native::invoice::all_invoices(),
        web::invoice::all_invoices(),
    );

    assert!(
        report.differences.is_empty(),
        "the desktop and browser ledgers disagree on {} read(s), as of {now} in \
         {reporting_currency}:\n\n{}\n",
        report.differences.len(),
        report.differences.join("\n\n")
    );
}

fn at(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(year, month, day, hour, 30, 0).unwrap()
}

#[test]
fn the_ledgers_agree_mid_month() {
    compare(at(2026, 10, 17, 12), "USD", false);
}

/// The first hours of a period: month to date is almost nothing, and the
/// previous period is the one with the figures.
#[test]
fn the_ledgers_agree_on_the_first_of_a_month() {
    compare(at(2026, 11, 1, 0), "USD", false);
}

/// A 31st, after a February: the same days of last month run out early.
#[test]
fn the_ledgers_agree_on_the_last_of_a_long_month() {
    compare(at(2027, 3, 31, 23), "USD", false);
}

/// Converting the other way, so the dollar accounts need the rate.
#[test]
fn the_ledgers_agree_in_another_reporting_currency() {
    compare(at(2026, 10, 17, 12), "CNY", false);
}

/// A reporting currency no built-in rate reaches: every charge is
/// unconverted, and no total may count one at par.
#[test]
fn the_ledgers_agree_when_nothing_converts() {
    compare(at(2026, 10, 17, 12), "EUR", false);
}

/// Rows the demo bill lacks, beside it: see [`edge_charges`].
#[test]
fn the_ledgers_agree_on_an_awkward_bill() {
    compare(at(2026, 10, 17, 12), "USD", true);
}

/// The same rows on the first of a month, where only the refund and the
/// first day's charges are in the current period yet.
#[test]
fn the_ledgers_agree_on_an_awkward_bill_converted() {
    compare(at(2026, 11, 1, 0), "CNY", true);
}

#[test]
fn a_number_is_compared_as_a_figure_and_the_rest_exactly() {
    assert_eq!(
        first_difference("(\"EC2\", 380.00000000001)", "(\"EC2\", 380.0)"),
        None
    );
    assert!(first_difference("(\"EC2\", 380.1)", "(\"EC2\", 380.0)").is_some());
    assert!(first_difference("(\"EC2\", 3)", "(\"EC2\", 4)").is_some());
    assert!(first_difference("(\"EC2\", 3)", "(\"EC3\", 3)").is_some());
    assert!(first_difference("[1.0]", "[1.0, 2.0]").is_some());
}
