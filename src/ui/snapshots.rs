//! The Month-end snapshots card on Account detail: freeze a month's bill,
//! then see what it held beside what the ledger holds now.
//!
//! It loads apart from the page. Reading a month's lines is a scan of that
//! month, and the rest of the page should not wait on it.

use anyhow::Result;
use chrono::Utc;
use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{fmt, theme};
use crate::analytics::snapshot;
use crate::i18n::bucket_label;
use crate::ledger::snapshot::{
    current_lines, freeze_period, periods_of, snapshot_lines, snapshots_of,
};
use crate::model::{
    AmountDelta, BillingSnapshot, Channel, FreezeOutcome, PeriodKey, SnapshotComparison,
    SnapshotTotal,
};
use crate::ui::theme::CardOutline as _;

/// How many months the switcher offers.
const PERIODS_SHOWN: usize = 6;

/// How many characters of a checksum or batch id identify it on screen.
const SHORT_ID: usize = 12;

/// What the card shows for its selected month.
struct LoadedMonth {
    /// Every month on offer, newest first.
    periods: Vec<PeriodKey>,
    /// The selected month; `None` when the account has no billing data.
    key: Option<PeriodKey>,
    /// Its revisions, newest first.
    revisions: Vec<BillingSnapshot>,
    /// The revision on screen and how it compares with the ledger.
    shown: Option<ShownRevision>,
    /// What the ledger holds for the month now.
    current_totals: Vec<SnapshotTotal>,
    /// The month has not ended yet.
    period_open: bool,
}

struct ShownRevision {
    snapshot: BillingSnapshot,
    comparison: SnapshotComparison,
    /// The stored lines still match the checksum and the recorded totals.
    verified: bool,
}

/// The month to open on: the newest one that has ended, because that is the
/// one a month-end freeze is for.
fn default_period(periods: &[PeriodKey]) -> Option<PeriodKey> {
    let now = Utc::now();
    periods
        .iter()
        .find(|key| !snapshot::period_is_open(&key.billing_period, now).unwrap_or(true))
        .or_else(|| periods.first())
        .cloned()
}

/// Read one month. Blocking; the card runs it in `smol::unblock`.
fn load_month(
    account_id: &str,
    period: Option<&str>,
    revision: Option<u32>,
) -> Result<LoadedMonth> {
    let mut periods = periods_of(account_id)?;
    periods.truncate(PERIODS_SHOWN);
    let key = period
        .and_then(|period| {
            periods
                .iter()
                .find(|key| key.billing_period == period)
                .cloned()
        })
        .or_else(|| default_period(&periods));

    let Some(key) = key else {
        return Ok(LoadedMonth {
            periods,
            key: None,
            revisions: Vec::new(),
            shown: None,
            current_totals: Vec::new(),
            period_open: false,
        });
    };

    let revisions = snapshots_of(&key)?;
    let current = current_lines(&key)?;
    let shown = revisions
        .iter()
        .find(|snapshot| Some(snapshot.revision) == revision)
        .or_else(|| revisions.first())
        .map(|shown| -> Result<ShownRevision> {
            let frozen = snapshot_lines(&shown.snapshot_id)?;
            Ok(ShownRevision {
                snapshot: shown.clone(),
                comparison: snapshot::compare(&frozen, &current),
                verified: snapshot::verify(shown, &frozen),
            })
        })
        .transpose()?;

    Ok(LoadedMonth {
        period_open: snapshot::period_is_open(&key.billing_period, Utc::now())?,
        current_totals: snapshot::summarize(&current),
        periods,
        key: Some(key),
        revisions,
        shown,
    })
}

/// The card's view.
#[derive(Default)]
pub struct SnapshotsCard {
    account_id: Option<String>,
    month: Option<LoadedMonth>,
    /// The selection, kept across reloads.
    period: Option<String>,
    revision: Option<u32>,
    freezing: bool,
    /// Outcome of the last freeze; cleared by the next selection.
    notice: Option<SharedString>,
    error: Option<SharedString>,
    /// Bumped by every load; only the latest flight may write its result.
    generation: u64,
}

impl SnapshotsCard {
    /// Point the card at an account. A switch forgets the selection.
    pub fn show(&mut self, account_id: String, cx: &mut Context<Self>) {
        if self.account_id.as_deref() != Some(account_id.as_str()) {
            self.month = None;
            self.period = None;
            self.revision = None;
            self.notice = None;
        }
        self.account_id = Some(account_id);
        self.load(cx);
    }

    fn select_period(&mut self, period: String, cx: &mut Context<Self>) {
        self.period = Some(period);
        self.revision = None;
        self.notice = None;
        self.load(cx);
    }

    fn select_revision(&mut self, revision: u32, cx: &mut Context<Self>) {
        self.revision = Some(revision);
        self.notice = None;
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(account_id) = self.account_id.clone() else {
            return;
        };
        self.generation += 1;
        let generation = self.generation;
        let period = self.period.clone();
        let revision = self.revision;
        cx.spawn(async move |this, cx| {
            let result =
                smol::unblock(move || load_month(&account_id, period.as_deref(), revision)).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(month) => {
                        this.period = month.key.as_ref().map(|key| key.billing_period.clone());
                        this.month = Some(month);
                        this.error = None;
                    }
                    Err(e) => {
                        this.error = Some(tr!("snapshots.load_failed", error = e));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn freeze(&mut self, key: PeriodKey, cx: &mut Context<Self>) {
        if self.freezing {
            return;
        }
        self.freezing = true;
        self.notice = None;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || freeze_period(&key)).await;
            this.update(cx, |this, cx| {
                this.freezing = false;
                match result {
                    Ok(FreezeOutcome::Frozen(frozen)) => {
                        this.notice = Some(tr!("snapshots.notice_frozen", n = frozen.revision));
                        this.revision = None;
                    }
                    Ok(FreezeOutcome::Unchanged(latest)) => {
                        this.notice = Some(tr!("snapshots.notice_unchanged", n = latest.revision));
                    }
                    Ok(FreezeOutcome::Empty) => {
                        this.notice = Some(tr!("snapshots.notice_empty"));
                    }
                    Err(e) => {
                        this.error = Some(tr!("snapshots.freeze_failed", error = e));
                    }
                }
                this.load(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Month switcher, in the same segmented shape as the page's range.
    fn render_periods(&self, month: &LoadedMonth, cx: &mut Context<Self>) -> Div {
        let selected = month.key.as_ref().map(|key| key.billing_period.clone());
        div()
            .h_flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .children(month.periods.iter().map(|key| {
                let period = key.billing_period.clone();
                let active = selected.as_deref() == Some(period.as_str());
                let button = Button::new(SharedString::from(format!("snapshot-period-{period}")))
                    .label(period.clone())
                    .small()
                    .rounded_full()
                    .custom(theme::range_pill(cx, active))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.select_period(period.clone(), cx);
                    }));
                if active {
                    button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                } else {
                    button
                }
            }))
    }

    fn render_revisions(&self, month: &LoadedMonth, cx: &mut Context<Self>) -> Option<Div> {
        if month.revisions.len() < 2 {
            return None;
        }
        let shown = month.shown.as_ref().map(|shown| shown.snapshot.revision);
        Some(
            div()
                .h_flex()
                .flex_wrap()
                .items_center()
                .gap_1()
                .child(theme::caption(cx, tr!("snapshots.revisions")))
                .children(month.revisions.iter().map(|snapshot| {
                    let revision = snapshot.revision;
                    let active = shown == Some(revision);
                    let button =
                        Button::new(SharedString::from(format!("snapshot-revision-{revision}")))
                            .label(tr!("snapshots.revision_short", n = revision))
                            .small()
                            .rounded_full()
                            .custom(theme::range_pill(cx, active))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.select_revision(revision, cx);
                            }));
                    if active {
                        button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                    } else {
                        button
                    }
                })),
        )
    }

    fn render_freeze(&self, month: &LoadedMonth, cx: &mut Context<Self>) -> Option<Button> {
        let key = month.key.clone()?;
        let has_rows = month.current_totals.iter().any(|total| total.rows > 0);
        let newest_matches = month
            .shown
            .as_ref()
            .filter(|shown| Some(shown.snapshot.revision) == newest_revision(month))
            .is_some_and(|shown| shown.comparison.is_identical());
        let label = if month.revisions.is_empty() {
            tr!("snapshots.freeze")
        } else {
            tr!("snapshots.freeze_again")
        };
        Some(
            Button::new("snapshot-freeze")
                .label(label)
                .custom(theme::outline_variant(cx))
                .card_outline(cx)
                .loading(self.freezing)
                .disabled(!has_rows || newest_matches || self.freezing)
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.freeze(key.clone(), cx);
                })),
        )
    }
}

fn newest_revision(month: &LoadedMonth) -> Option<u32> {
    month.revisions.first().map(|snapshot| snapshot.revision)
}

impl Render for SnapshotsCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let month = self.month.take();
        let header = div()
            .w_full()
            .h_flex()
            .items_start()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::section_title(cx, tr!("snapshots.title")))
                    .child(theme::caption(cx, tr!("snapshots.subtitle"))),
            )
            .when_some(
                month.as_ref().and_then(|m| self.render_freeze(m, cx)),
                |el, button| el.child(button),
            );

        let card = theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(header)
            .when_some(self.error.clone(), |el, error| {
                el.child(banner(error, theme::danger_bg(cx), theme::danger(cx)))
            })
            .when_some(self.notice.clone(), |el, notice| {
                el.child(theme::caption(cx, notice))
            });

        let card = match &month {
            None => card,
            Some(m) if m.key.is_none() => card.child(theme::caption(cx, tr!("snapshots.no_data"))),
            Some(m) => card
                .child(self.render_periods(m, cx))
                .when_some(self.render_revisions(m, cx), |el, revisions| {
                    el.child(revisions)
                })
                .child(render_month(m, cx)),
        };
        self.month = month;
        card
    }
}

fn render_month(month: &LoadedMonth, cx: &App) -> Div {
    let body = div().v_flex().gap_4();
    let Some(shown) = &month.shown else {
        return body
            .when(month.period_open, |el| {
                el.child(banner(
                    tr!("snapshots.warn_open_now"),
                    theme::warning_bg(cx),
                    theme::warning_text(cx),
                ))
            })
            .children(amount_warnings(&month.current_totals, None, cx))
            .child(theme::caption(cx, tr!("snapshots.not_frozen")))
            .child(unfrozen_table(&month.current_totals, cx));
    };

    let snapshot = &shown.snapshot;
    let comparison = &shown.comparison;
    body.when(!shown.verified, |el| {
        el.child(banner(
            tr!("snapshots.warn_integrity"),
            theme::danger_bg(cx),
            theme::danger(cx),
        ))
    })
    .when(snapshot.period_open, |el| {
        el.child(banner(
            tr!("snapshots.warn_open_frozen"),
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ))
    })
    .children(amount_warnings(
        &snapshot.totals,
        Some(&snapshot.reporting_currency),
        cx,
    ))
    .child(status_line(comparison, cx))
    .child(comparison_table(comparison, cx))
    .when(!comparison.by_service.is_empty(), |el| {
        el.child(delta_section(
            tr!("snapshots.by_service"),
            &comparison.by_service,
            false,
            cx,
        ))
    })
    .when(!comparison.by_category.is_empty(), |el| {
        el.child(delta_section(
            tr!("snapshots.by_category"),
            &comparison.by_category,
            true,
            cx,
        ))
    })
    .child(frozen_categories(&snapshot.totals, cx))
    .child(provenance(snapshot, month.revisions.len(), cx))
}

/// What a set of totals cannot vouch for. `reporting` is the currency a frozen
/// month was converted to; a month not yet frozen has no conversion to report.
fn amount_warnings(totals: &[SnapshotTotal], reporting: Option<&str>, cx: &App) -> Vec<Div> {
    let rows: u64 = totals.iter().map(|total| total.rows).sum();
    let without_amount: u64 = totals.iter().map(|total| total.rows_without_amount).sum();
    let unconverted: u64 = totals.iter().map(|total| total.rows_unconverted).sum();

    let mut warnings = Vec::new();
    if rows > 0 && without_amount == rows {
        warnings.push(banner(
            tr!("snapshots.warn_no_amounts"),
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ));
    } else if without_amount > 0 {
        warnings.push(banner(
            tr!(
                "snapshots.warn_some_absent",
                rows = without_amount,
                total = rows
            ),
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ));
    }
    if let Some(reporting) = reporting.filter(|_| unconverted > 0) {
        warnings.push(banner(
            tr!(
                "snapshots.warn_unconverted",
                rows = unconverted,
                reporting = reporting
            ),
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ));
    }
    warnings
}

fn status_line(comparison: &SnapshotComparison, cx: &App) -> Div {
    if comparison.is_identical() {
        return div().h_flex().child(theme::pill(
            tr!("snapshots.status_matches"),
            theme::success_bg(cx),
            theme::success(cx),
        ));
    }
    div()
        .h_flex()
        .flex_wrap()
        .items_center()
        .gap_2()
        .child(theme::pill(
            tr!("snapshots.status_differs"),
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ))
        .child(theme::caption(
            cx,
            tr!(
                "snapshots.lines_summary",
                new = comparison.new_lines,
                removed = comparison.removed_lines,
                changed = comparison.changed_lines,
                unchanged = comparison.unchanged_lines
            ),
        ))
}

fn cell(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .flex_1()
        .min_w_0()
        .text_sm()
        .text_color(theme::text_primary(cx))
        .child(text.into())
}

fn number_cell(cx: &App, text: impl Into<SharedString>) -> Div {
    cell(cx, text).text_right()
}

fn header_row(cx: &App, first: SharedString) -> Div {
    div()
        .w_full()
        .h_flex()
        .gap_4()
        .pb_2()
        .child(div().flex_1().child(theme::header_cell(cx, first)))
        .child(
            div()
                .flex_1()
                .text_right()
                .child(theme::header_cell(cx, tr!("snapshots.col_frozen"))),
        )
        .child(
            div()
                .flex_1()
                .text_right()
                .child(theme::header_cell(cx, tr!("snapshots.col_current"))),
        )
        .child(
            div()
                .flex_1()
                .text_right()
                .child(theme::header_cell(cx, tr!("snapshots.col_change"))),
        )
}

fn row(cx: &App) -> Div {
    div()
        .w_full()
        .h_flex()
        .gap_4()
        .py_1()
        .border_t_1()
        .border_color(theme::card_border(cx))
}

fn total_text(total: Option<f64>, currency: &str) -> SharedString {
    match total {
        Some(total) => fmt::exact_amount(total, currency).into(),
        None => tr!("snapshots.no_amount"),
    }
}

fn comparison_table(comparison: &SnapshotComparison, cx: &App) -> Div {
    div()
        .v_flex()
        .child(header_row(cx, tr!("snapshots.col_currency")))
        .children(comparison.by_currency.iter().map(|delta| {
            row(cx)
                .child(cell(cx, delta.currency.clone()))
                .child(number_cell(
                    cx,
                    total_text(delta.frozen, &delta.currency),
                ))
                .child(number_cell(
                    cx,
                    total_text(delta.current, &delta.currency),
                ))
                .child(number_cell(
                    cx,
                    fmt::signed_exact_amount(delta.delta, &delta.currency),
                ))
        }))
}

fn unfrozen_table(totals: &[SnapshotTotal], cx: &App) -> Div {
    div()
        .v_flex()
        .child(header_row(cx, tr!("snapshots.col_currency")))
        .children(
            snapshot::currency_totals(totals)
                .into_iter()
                .map(|(currency, total)| {
                    row(cx)
                        .child(cell(cx, currency.clone()))
                        .child(number_cell(cx, "—"))
                        .child(number_cell(cx, total_text(total, &currency)))
                        .child(number_cell(cx, "—"))
                }),
        )
}

fn delta_section(title: SharedString, deltas: &[AmountDelta], categories: bool, cx: &App) -> Div {
    div()
        .v_flex()
        .child(header_row(cx, title))
        .children(deltas.iter().map(|delta| {
            let label = if categories {
                category_label(&delta.label)
            } else {
                bucket_label(&delta.label)
            };
            row(cx)
                .child(cell(cx, label))
                .child(number_cell(
                    cx,
                    fmt::exact_amount(delta.frozen, &delta.currency),
                ))
                .child(number_cell(
                    cx,
                    fmt::exact_amount(delta.current, &delta.currency),
                ))
                .child(number_cell(
                    cx,
                    fmt::signed_exact_amount(delta.delta, &delta.currency),
                ))
        }))
}

/// The frozen month by charge category, each in its billing currency.
fn frozen_categories(totals: &[SnapshotTotal], cx: &App) -> Div {
    div()
        .v_flex()
        .gap_1()
        .child(theme::header_cell(cx, tr!("snapshots.frozen_by_category")))
        .children(totals.iter().map(|total| {
            let amount: SharedString = if total.rows == total.rows_without_amount {
                tr!("snapshots.no_amount")
            } else {
                fmt::exact_amount(total.billed_cost, &total.currency).into()
            };
            row(cx)
                .child(cell(cx, category_label(&total.category)))
                .child(cell(cx, total.currency.clone()))
                .child(number_cell(
                    cx,
                    tr!("snapshots.charge_count", n = total.rows),
                ))
                .child(number_cell(cx, amount))
        }))
}

/// Where the revision on screen came from, and what it converted at.
fn provenance(snapshot: &BillingSnapshot, revisions: usize, cx: &App) -> Div {
    let frozen_at = snapshot.frozen_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let source: SharedString = match snapshot.channel {
        Channel::Api => tr!("snapshots.source_api"),
        Channel::File => tr!("snapshots.source_file"),
    };
    let batch = snapshot
        .batch_id
        .as_deref()
        .map(|id| id.chars().take(SHORT_ID).collect::<String>())
        .unwrap_or_else(|| "—".to_string());
    let checksum: String = snapshot.checksum.chars().take(SHORT_ID).collect();
    let reporting_total: f64 = snapshot
        .totals
        .iter()
        .map(|total| total.billed_cost_base)
        .sum();
    let has_amounts = snapshot
        .totals
        .iter()
        .any(|total| total.rows > total.rows_without_amount);

    div()
        .v_flex()
        .gap_1()
        .child(theme::header_cell(cx, tr!("snapshots.provenance")))
        .child(fact(
            cx,
            tr!("snapshots.fact_revision"),
            tr!(
                "snapshots.revision_of",
                n = snapshot.revision,
                total = revisions
            ),
        ))
        .child(fact(cx, tr!("snapshots.fact_frozen_at"), frozen_at))
        .child(fact(cx, tr!("snapshots.fact_source"), source))
        .child(fact(cx, tr!("snapshots.fact_batch"), batch))
        .child(fact(
            cx,
            tr!("snapshots.fact_rows"),
            snapshot.row_count.to_string(),
        ))
        .child(fact(cx, tr!("snapshots.fact_checksum"), checksum))
        .when(has_amounts, |el| {
            el.child(fact(
                cx,
                tr!("snapshots.fact_reporting_total"),
                fmt::exact_amount(reporting_total, &snapshot.reporting_currency),
            ))
        })
        .children(snapshot.rates.iter().map(|rate| {
            let text: SharedString = match rate.rate {
                Some(_) if rate.currency == snapshot.reporting_currency => {
                    tr!("snapshots.rate_same", rows = rate.rows)
                }
                Some(value) => tr!(
                    "snapshots.rate_used",
                    rate = value,
                    reporting = snapshot.reporting_currency,
                    rows = rate.rows
                ),
                None => tr!("snapshots.rate_missing", rows = rate.rows),
            };
            fact(
                cx,
                tr!("snapshots.fact_rate", currency = rate.currency),
                text,
            )
        }))
}

fn fact(cx: &App, label: SharedString, value: impl Into<SharedString>) -> Div {
    div()
        .h_flex()
        .gap_4()
        .text_sm()
        .child(
            div()
                .w(rems(11.))
                .flex_shrink_0()
                .text_color(theme::text_muted(cx))
                .child(label),
        )
        .child(
            div()
                .min_w_0()
                .text_color(theme::text_primary(cx))
                .child(value.into()),
        )
}

fn banner(text: SharedString, bg: Hsla, fg: Hsla) -> Div {
    div()
        .w_full()
        .p_3()
        .rounded_md()
        .bg(bg)
        .text_sm()
        .text_color(fg)
        .child(text)
}

fn category_label(category: &str) -> SharedString {
    match category {
        "Usage" => tr!("snapshots.category_usage"),
        "Purchase" => tr!("snapshots.category_purchase"),
        "Credit" => tr!("snapshots.category_credit"),
        "Tax" => tr!("snapshots.category_tax"),
        "Adjustment" => tr!("snapshots.category_adjustment"),
        other => SharedString::from(other.to_string()),
    }
}
