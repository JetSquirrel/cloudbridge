//! Export the monthly cost review from Overview: pick a month, pick a file.
//!
//! The figures come from the queries the pages read, gathered here and handed
//! to the pure [`crate::report`]. Desktop only for now: the browser has no
//! file to save to through the same dialog.

use std::collections::BTreeMap;

use anyhow::Result;
use chrono::{Duration, Utc};
use gpui_kit::component::button::*;
use gpui_kit::component::dialog::{Cancel, Dialog, DialogFooter};
use gpui_kit::component::*;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::data::{self, BUSINESS_LINE_TAG};
use super::insights::judge_period;
use super::{dialogs, theme};
use crate::analytics::{midnight, period_of};
use crate::cloud::BillingPeriod;
use crate::ledger::query;
use crate::model::{BreakdownDim, PeriodKey};
use crate::report::{
    service_rows, ForecastLine, InsightsSection, ReportData, SourceRow, TOP_FINDINGS,
};
use crate::ui::theme::CardOutline as _;

/// Whether a `YYYY-MM` month has yet to end at `now`.
fn is_open(period: &str, now: chrono::DateTime<Utc>) -> Result<bool> {
    Ok(now < midnight(period_of(period)?.end_exclusive()))
}

/// How far back the month picker reaches.
const MONTHS_OFFERED: i64 = 24 * 31;

/// Months the ledger has usage for, newest first. Blocking.
fn months_with_data() -> Result<Vec<String>> {
    let since = Utc::now() - Duration::days(MONTHS_OFFERED);
    let mut months: Vec<String> = query::monthly_usage(since)?
        .into_iter()
        .map(|(period, _)| period)
        .collect();
    months.sort_unstable_by(|a, b| b.cmp(a));
    months.dedup();
    Ok(months)
}

/// The month to open on: the newest one that has ended.
fn default_month(months: &[String]) -> Option<String> {
    let now = Utc::now();
    months
        .iter()
        .find(|month| !is_open(month, now).unwrap_or(true))
        .or_else(|| months.first())
        .cloned()
}

/// Gather a month's report. Blocking; it runs in `smol::unblock`. The browser
/// has no export to call it from yet.
#[cfg_attr(target_family = "wasm", allow(dead_code))]
pub fn load_report(period: &str) -> Result<ReportData> {
    let now = Utc::now();
    let billing = period_of(period)?;
    let period_open = is_open(period, now)?;
    let currency = data::reporting_currency();
    let names: BTreeMap<String, String> = data::account_names()?.into_iter().collect();

    let comparison = query::period_over_period(period)?;
    let (usage, credits) = query::usage_and_credits(period)?;

    let mut regions: BTreeMap<String, f64> = BTreeMap::new();
    let mut sources = Vec::new();
    for account in query::ledger_accounts()? {
        let covered = account
            .first_period
            .as_deref()
            .is_some_and(|first| first <= period)
            && account
                .last_period
                .as_deref()
                .is_some_and(|last| period <= last);
        if !covered {
            continue;
        }
        let key = PeriodKey::new(account.provider.clone(), account.account_id.clone(), period);
        let parts = query::breakdown_by(&key, BreakdownDim::Region)?;
        // An account with no rows this month, between its first and last.
        if parts.is_empty() {
            continue;
        }
        for (region, amount) in parts {
            *regions.entry(region).or_default() += amount;
        }
        sources.push(SourceRow {
            account: names
                .get(&account.account_id)
                .cloned()
                .unwrap_or_else(|| account.account_id.clone()),
            provider: account.provider,
            channel: query::channel_of(&key)?,
            last_ingest: query::last_ingest(&key)?,
        });
    }
    let mut regions: Vec<(String, f64)> = regions.into_iter().collect();
    regions.sort_by(|a, b| b.1.abs().total_cmp(&a.1.abs()).then_with(|| a.0.cmp(&b.0)));

    let mut business_lines = query::tag_usage_breakdown(period, BUSINESS_LINE_TAG)?;
    business_lines.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));

    // A forecast and a budget describe a month in progress. For an ended one
    // the report says it skipped them rather than showing today's budgets
    // against last month's bill.
    let forecast = if period_open {
        query::forecast_for_period(period)
            .ok()
            .map(|f| ForecastLine {
                forecast: f.forecast,
                days_elapsed: f.days_elapsed,
                days_in_month: f.days_in_month,
            })
    } else {
        None
    };
    let budgets = if period_open && BillingPeriod::containing(now).label() == period {
        data::load_budget_statuses()?
    } else {
        Vec::new()
    };

    let judged = judge_period(period)?;
    let scope = judged.scope;
    let mut judged = judged.report;
    let insights = match scope {
        Some(scope) if judged.resources > 0 => {
            let total_findings = judged.findings.len();
            judged.findings.truncate(TOP_FINDINGS);
            InsightsSection::Evaluated {
                scanned_at: scope.scanned_at,
                partial: !scope.complete,
                resources: judged.resources,
                total_findings,
                findings: judged.findings,
            }
        }
        _ => InsightsSection::NotScanned,
    };

    let since = midnight(billing.start());
    let until = midnight(billing.end_exclusive());
    Ok(ReportData {
        period: period.to_string(),
        currency,
        generated_at: now,
        period_open,
        has_demo_accounts: data::AccountCounts::load()?.demo > 0,
        net: comparison.current_total,
        previous_net: comparison.previous_total,
        usage,
        credits,
        services: service_rows(
            &comparison.current_by_service,
            &comparison.previous_by_service,
        ),
        regions,
        business_lines,
        forecast,
        budgets,
        data_quality: query::data_quality_issues(period, BUSINESS_LINE_TAG)?,
        insights,
        sources,
        unconverted_charges: query::unconverted_between(since, until)?,
        rows_without_amount: query::rows_without_amount(period)?,
    })
}

/// The Export report button and the month picker it opens.
#[derive(Default)]
pub struct ReportExport {
    months: Vec<String>,
    selected: Option<String>,
    exporting: bool,
    status: Option<Result<SharedString, SharedString>>,
}

impl ReportExport {
    fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.status = None;
        cx.spawn_in(window, async move |this, cx| {
            let months = smol::unblock(months_with_data).await;
            this.update_in(cx, |this, window, cx| match months {
                Ok(months) => {
                    this.selected = default_month(&months);
                    this.months = months;
                    dialogs::open(window, cx, |this, dialog, window, cx| {
                        this.render_picker(dialog, window, cx)
                    });
                }
                Err(e) => {
                    this.status = Some(Err(tr!("report_ui.failed", error = e)));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn render_picker(
        &mut self,
        dialog: Dialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        let selected = self.selected.clone();
        dialog
            .title(tr!("report_ui.title"))
            .w(rems(30.).to_pixels(window.rem_size()))
            .child(
                div()
                    .v_flex()
                    .gap_3()
                    .child(theme::caption(cx, tr!("report_ui.hint")))
                    .when(self.months.is_empty(), |el| {
                        el.child(theme::caption(cx, tr!("report_ui.no_months")))
                    })
                    .child(
                        div()
                            .h_flex()
                            .flex_wrap()
                            .gap_1()
                            .children(self.months.iter().map(|month| {
                                let value = month.clone();
                                let active = selected.as_ref() == Some(month);
                                let button = Button::new(SharedString::from(format!(
                                    "report-month-{month}"
                                )))
                                .label(month.clone())
                                .small()
                                .rounded_full()
                                .custom(theme::range_pill(cx, active))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.selected = Some(value.clone());
                                    cx.notify();
                                }));
                                if active {
                                    button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                                } else {
                                    button
                                }
                            })),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("report-cancel")
                            .label(tr!("common.cancel"))
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("report-export")
                            .label(tr!("report_ui.export"))
                            .primary()
                            .disabled(selected.is_none())
                            .on_click(cx.listener(|this, _, window, cx| {
                                window.close_dialog(cx);
                                this.export(cx);
                            })),
                    ),
            )
    }

    /// Ask where to save, then gather and write the report.
    #[cfg(not(target_family = "wasm"))]
    fn export(&mut self, cx: &mut Context<Self>) {
        let Some(period) = self.selected.clone() else {
            return;
        };
        if self.exporting {
            return;
        }
        self.exporting = true;
        self.status = None;
        cx.notify();

        let directory = directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(std::path::Path::to_path_buf))
            .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
            .unwrap_or_default();
        let name = format!("cloudbridge-cost-review-{period}.html");
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));

        cx.spawn(async move |this, cx| {
            let outcome: Option<Result<SharedString, SharedString>> = match chosen.await {
                Ok(Ok(Some(path))) => Some(
                    smol::unblock(move || -> Result<SharedString, SharedString> {
                        let report =
                            load_report(&period).map_err(|e| tr!("report_ui.failed", error = e))?;
                        std::fs::write(&path, crate::report::render_html(&report))
                            .map_err(|e| tr!("report_ui.failed", error = e))?;
                        Ok(tr!("report_ui.saved", path = path.display()))
                    })
                    .await,
                ),
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(e)) => Some(Err(tr!("report_ui.failed", error = e))),
            };
            this.update(cx, |this, cx| {
                this.exporting = false;
                this.status = outcome;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(target_family = "wasm")]
    fn export(&mut self, _cx: &mut Context<Self>) {}
}

impl Render for ReportExport {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h_flex()
            .items_center()
            .gap_3()
            .when_some(self.status.clone(), |el, status| {
                let (text, color) = match status {
                    Ok(text) => (text, theme::text_muted(cx)),
                    Err(text) => (text, theme::danger(cx)),
                };
                el.child(div().min_w_0().text_sm().text_color(color).child(text))
            })
            .child(
                Button::new("report-open")
                    .label(tr!("report_ui.button"))
                    .tooltip(tr!("report_ui.tooltip"))
                    .custom(theme::outline_variant(cx))
                    .card_outline(cx)
                    .loading(self.exporting)
                    .disabled(self.exporting)
                    .on_click(cx.listener(|this, _, window, cx| this.open(window, cx))),
            )
    }
}
