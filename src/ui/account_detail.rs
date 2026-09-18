//! Account Detail View — one account's usage trend, service breakdown, and
//! region / service-category drill-down of the current billing period.

use anyhow::Result;
use chrono::Utc;
use gpui_kit::component::{button::*, scroll::ScrollableElement, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::data::{AccountDetailData, Range, ServiceRow};
use super::{accounts, chart, data, fmt, theme};
use crate::cloud::BillingPeriod;
use crate::ledger::query::{breakdown_by, data_quality_issues, BreakdownDim, DataQualityIssue};
use crate::ui::theme::CardOutline as _;
use crate::{db, ingest};

/// Account Detail View
///
/// The view is created once by the shell and shown with
/// [`AccountDetailView::show`]; until the first `show`, there is nothing
/// to render.
pub struct AccountDetailView {
    /// The account being viewed.
    account_id: Option<String>,
    /// Selected range in the header segmented control.
    range: Range,
    /// The loaded page data; `None` until the first load completes.
    data: Option<AccountDetailData>,
    /// The current period's region / service-category breakdowns; lands in
    /// the same flight as `data`.
    drilldown: Option<AccountDrilldown>,
    /// The account's data-quality findings for its current billing period;
    /// lands in the same flight as `data`. `None` until a load completes
    /// and after a dismissal, which hides the section until the next load.
    health: Option<AccountHealth>,
    /// The dimension the drill-down card's switcher has selected.
    drill_dim: DrillDim,
    /// A load is in flight.
    loading: bool,
    /// Last load failure, shown under the header.
    error: Option<String>,
    /// Bumped by every load; only the latest flight may write its result.
    generation: u64,
    /// Chart hover state; see [`chart::ChartHover`].
    chart_hover: chart::ChartHover,
}

impl AccountDetailView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            account_id: None,
            range: Range::Mtd,
            data: None,
            drilldown: None,
            health: None,
            drill_dim: DrillDim::Region,
            loading: false,
            error: None,
            generation: 0,
            chart_hover: chart::ChartHover::new(),
        }
    }

    /// Point the page at an account and load it. A repeat `show` for the
    /// account already on screen keeps the selected range; a switch resets
    /// to MTD.
    pub fn show(&mut self, account_id: String, cx: &mut Context<Self>) {
        if self.account_id.as_deref() != Some(account_id.as_str()) {
            self.range = Range::Mtd;
            self.data = None;
            self.drilldown = None;
            self.health = None;
        }
        self.account_id = Some(account_id);
        self.load(cx);
    }

    /// Reload the page data. Called by the app shell when this page is
    /// navigated to; a no-op while a load is already in flight.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.account_id.is_none() {
            return;
        }
        self.load(cx);
    }

    /// Load the page data off-thread; the ledger queries are blocking. The
    /// drill-down rides in the same flight, so switching its dimension
    /// later is a re-render, not a reload.
    fn load(&mut self, cx: &mut Context<Self>) {
        let Some(account_id) = self.account_id.clone() else {
            return;
        };
        self.loading = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        // New data may shift the points; a stale hover would tag the
        // wrong day.
        self.chart_hover.clear();
        cx.notify();

        let range = self.range;
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || -> Result<_> {
                let detail = data::load_account_detail(&account_id, range)?;
                let drilldown = load_drilldown(&account_id)?;
                let health = load_health(&account_id)?;
                Ok((detail, drilldown, health))
            })
            .await;
            this.update(cx, |this, cx| {
                this.loading = false;
                if this.generation == generation {
                    match result {
                        Ok((loaded, drilldown, health)) => {
                            this.data = Some(loaded);
                            this.drilldown = Some(drilldown);
                            this.health = Some(health);
                            this.error = None;
                        }
                        Err(e) => {
                            this.error = Some(format!("Could not load the account: {e}"));
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, d: &AccountDetailData, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.range;
        div()
            .w_full()
            .h_flex()
            .items_start()
            .justify_between()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(
                        Button::new("back-to-accounts")
                            .label("← Accounts")
                            .ghost()
                            .small()
                            .on_click(|_, _, cx| {
                                crate::app::navigate_to(crate::app::CurrentView::Accounts, cx)
                            }),
                    )
                    .child(theme::page_title(cx, d.account_name.clone()))
                    .child(theme::caption(
                        cx,
                        format!(
                            "{} · {} · reported in {}",
                            d.provider, d.window_caption, d.currency
                        ),
                    )),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .p_1()
                    .rounded_full()
                    .border_1()
                    .border_color(theme::card_border(cx))
                    .children(Range::ALL.iter().map(|range| {
                        let active = *range == selected;
                        let button = Button::new(range.id())
                            .label(range.label())
                            .small()
                            .rounded_full()
                            .custom(theme::range_pill(cx, active))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.range != *range {
                                    this.range = *range;
                                    this.load(cx);
                                }
                            }));
                        if active {
                            button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                        } else {
                            button
                        }
                    })),
            )
    }

    /// The trend chart with the shared hover overlay. No baseline series:
    /// a single account's trailing mean is noisier than it is informative.
    fn render_chart(
        &self,
        d: &AccountDetailData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let title = match d.range {
            Range::Months12 => "Monthly usage",
            _ => "Daily usage",
        };
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(theme::section_title(cx, title))
            .child(
                div()
                    .id("account-chart")
                    .w_full()
                    .relative()
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                        let x: f32 = event.position.x.into();
                        let nearest = this.chart_hover.nearest(x);
                        if nearest.is_some() && nearest != this.chart_hover.index() {
                            this.chart_hover.set(nearest);
                            cx.notify();
                        }
                    }))
                    .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                        if !*hovered && this.chart_hover.index().is_some() {
                            this.chart_hover.clear();
                            cx.notify();
                        }
                    }))
                    .child(chart::spend_area_chart(
                        cx,
                        &d.chart.actual,
                        &d.chart.baseline,
                        rems(16.25),
                        self.chart_hover.points_cell(),
                        self.chart_hover.bounds_cell(),
                    ))
                    .when_some(
                        chart::hover_overlay(
                            cx,
                            &self.chart_hover,
                            &d.chart.actual,
                            &d.currency,
                            window.rem_size(),
                        ),
                        |el, overlay| el.children(overlay),
                    ),
            )
    }

    fn render_services(&self, d: &AccountDetailData, cx: &App) -> impl IntoElement {
        let currency = d.currency.as_str();
        let vs_prior = match d.range {
            Range::Mtd => "VS LAST MONTH",
            Range::Days30 => "VS PRIOR 30D",
            Range::Months12 => "VS PRIOR 12M",
        };
        let card = theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(theme::section_title(cx, "By service or model"));

        if d.services.is_empty() {
            if !d.is_snapshot {
                return card.child(theme::caption(cx, "No usage in this window."));
            }
            return card.child(
                div()
                    .v_flex()
                    .gap_1()
                    .items_start()
                    .child(theme::caption(
                        cx,
                        "No usage in this window. A balance-reporting source's usage \
                         arrives through its bill file import.",
                    ))
                    .child(
                        Button::new("go-to-accounts")
                            .label("Go to Accounts → Import")
                            .link()
                            .small()
                            .text_color(theme::accent(cx))
                            .on_click(|_, _, cx| {
                                crate::app::navigate_to(crate::app::CurrentView::Accounts, cx)
                            }),
                    ),
            );
        }

        card.child(
            div()
                .h_flex()
                .items_center()
                .pb_2()
                .child(theme::header_cell(cx, "SERVICE / MODEL").flex_1().min_w_0())
                .child(theme::header_cell(cx, "AMOUNT").w_24().text_right())
                .child(theme::header_cell(cx, "SHARE").w_32().px_2())
                .child(theme::header_cell(cx, vs_prior).w_24().text_right()),
        )
        .child(
            div()
                .v_flex()
                .children(d.services.iter().map(|row| service_row(cx, row, currency))),
        )
    }

    /// The drill-down card: the account's region or service-category
    /// breakdown. Unlike the rest of the page it is billing-period keyed —
    /// the ledger groups these dimensions per period — so it always shows
    /// the current period whatever range the header has selected.
    fn render_drilldown(&self, d: &AccountDetailData, cx: &mut Context<Self>) -> AnyElement {
        let Some(drilldown) = &self.drilldown else {
            return div().into_any_element();
        };
        let dim = self.drill_dim;
        let rows: &[(String, f64)] = match dim {
            DrillDim::Region => &drilldown.by_region,
            DrillDim::ServiceCategory => &drilldown.by_category,
        };
        let currency = d.currency.as_str();
        let total: f64 = rows.iter().map(|(_, amount)| amount).sum();
        let has_other = rows.iter().any(|(label, _)| label == "Other");
        let selected = dim;

        let card = theme::card(cx).w_full().p_5().v_flex().gap_4().child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(theme::section_title(cx, dim.title()))
                        .child(theme::caption(cx, "Current billing period")),
                )
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .p_1()
                        .rounded_full()
                        .border_1()
                        .border_color(theme::card_border(cx))
                        .children(DrillDim::ALL.iter().map(|option| {
                            let active = *option == selected;
                            let button = Button::new(option.id())
                                .label(option.label())
                                .small()
                                .rounded_full()
                                .custom(theme::range_pill(cx, active))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.drill_dim != *option {
                                        this.drill_dim = *option;
                                        cx.notify();
                                    }
                                }));
                            if active {
                                button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                            } else {
                                button
                            }
                        })),
                ),
        );

        if rows.is_empty() {
            return card
                .child(theme::caption(cx, "No usage in this billing period."))
                .into_any_element();
        }

        card.child(
            div()
                .h_flex()
                .items_center()
                .pb_2()
                .child(
                    theme::header_cell(cx, dim.bucket_header())
                        .flex_1()
                        .min_w_0(),
                )
                .child(theme::header_cell(cx, "AMOUNT").w_24().text_right())
                .child(theme::header_cell(cx, "SHARE").w_32().px_2()),
        )
        .child(
            div().v_flex().children(
                rows.iter()
                    .map(|(label, amount)| drilldown_row(cx, label, *amount, total, currency)),
            ),
        )
        .when(has_other, |el| {
            el.child(theme::caption(cx, dim.other_caption()))
        })
        .into_any_element()
    }

    /// The compact health section: the account's data-quality findings for
    /// its current billing period, rendered like the accounts page's Data
    /// health card. Billing-period keyed like the drill-down — the ledger's
    /// checks group per period — so it ignores the header's range. Dismiss
    /// persists every shown finding's key and hides the section; the loader
    /// filters dismissed findings out, so only a genuinely new finding
    /// brings the section back.
    fn render_health(&self, d: &AccountDetailData, cx: &Context<Self>) -> AnyElement {
        let Some(health) = &self.health else {
            return div().into_any_element();
        };
        let has_findings = !health.issues.is_empty();
        let dismiss_keys = health.dismiss_keys.clone();
        let card = theme::card(cx).w_full().p_5().v_flex().gap_3().child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .child(
                    div()
                        .v_flex()
                        .gap_1()
                        .child(theme::section_title(cx, "Data health"))
                        .child(theme::caption(cx, "Current billing period")),
                )
                .when(has_findings, |el| {
                    el.child(
                        Button::new("dismiss-health-section")
                            .label("Dismiss")
                            .link()
                            .small()
                            .text_color(theme::text_muted(cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Err(e) = data::dismiss_quality_issues(&dismiss_keys) {
                                    tracing::warn!(
                                        "Could not persist the data-quality dismissal: {}",
                                        e
                                    );
                                }
                                this.health = None;
                                cx.notify();
                            })),
                    )
                }),
        );

        if health.issues.is_empty() {
            return card
                .child(theme::caption(
                    cx,
                    "All good — no data-quality issues this period.",
                ))
                .into_any_element();
        }

        card.children(
            health
                .issues
                .iter()
                .map(|issue| accounts::render_issue_row(issue, &d.currency, cx)),
        )
        .into_any_element()
    }
}

impl Render for AccountDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match (&self.account_id, &self.data) {
            (None, _) => theme::caption(cx, "No account selected.").into_any_element(),
            (Some(_), None) if self.loading => div()
                .w_full()
                .flex()
                .items_center()
                .justify_center()
                .py_16()
                .text_base()
                .text_color(theme::text_muted(cx))
                .child("Loading…")
                .into_any_element(),
            (Some(_), None) => div().into_any_element(),
            (Some(_), Some(d)) => div()
                .v_flex()
                .gap_6()
                .child(self.render_header(d, cx))
                .child(render_stats(d, cx))
                .child(self.render_health(d, cx))
                .child(self.render_chart(d, window, cx))
                .child(self.render_services(d, cx))
                .child(self.render_drilldown(d, cx))
                .into_any_element(),
        };

        div().size_full().bg(theme::app_bg(cx)).child(
            div()
                .v_flex()
                .gap_6()
                .h_full()
                .p_8()
                .overflow_y_scrollbar()
                .when_some(self.error.clone(), |el, error| {
                    el.child(
                        div()
                            .w_full()
                            .p_3()
                            .rounded_md()
                            .bg(theme::danger_bg(cx))
                            .text_sm()
                            .text_color(theme::danger(cx))
                            .child(error),
                    )
                })
                .child(body),
        )
    }
}

/// The headline stats: net spend with the usage/credits split inline, and
/// the change against the comparison window.
fn render_stats(d: &AccountDetailData, cx: &App) -> impl IntoElement {
    let currency = d.currency.as_str();
    div()
        .w_full()
        .h_flex()
        .items_stretch()
        .gap_4()
        .child(theme::stat_card(
            cx,
            "SPEND",
            fmt::amount(d.spend, currency),
            div().text_color(theme::text_muted(cx)).child(format!(
                "usage {} · credits {}",
                fmt::amount(d.usage, currency),
                fmt::amount(d.credits, currency)
            )),
        ))
        .child(theme::stat_card(
            cx,
            "CHANGE",
            d.change_pct
                .map(fmt::change_pct)
                .unwrap_or_else(|| "—".to_string()),
            div()
                .text_color(theme::text_muted(cx))
                .child(d.change_caption),
        ))
}

/// One service row: name, amount, a share bar, and the change against the
/// comparison window.
fn service_row(cx: &App, row: &ServiceRow, currency: &str) -> Div {
    let (delta_text, delta_color) = match row.change_pct {
        Some(pct) if pct < 0.0 => (format!("{pct:+.0}%"), theme::text_muted(cx)),
        Some(pct) => (format!("{pct:+.0}%"), theme::accent(cx)),
        None => ("—".to_string(), theme::text_muted(cx)),
    };

    div()
        .h_flex()
        .items_center()
        .py_2()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(
            div()
                .flex_1()
                // min_w_0 so a long service name truncates instead of
                // pushing the amount columns out of the card.
                .min_w_0()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_sm()
                .text_color(theme::text_primary(cx))
                .child(row.name.clone()),
        )
        .child(
            div()
                .w_24()
                .text_right()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_primary(cx))
                .child(fmt::amount(row.amount, currency)),
        )
        .child(
            div().w_32().px_2().child(
                div()
                    .w_full()
                    .h_2()
                    .rounded_full()
                    .bg(theme::sidebar_bg(cx))
                    .child(
                        div()
                            .h_full()
                            .w(relative(row.share as f32))
                            .rounded_full()
                            .bg(theme::accent(cx)),
                    ),
            ),
        )
        .child(
            div()
                .w_24()
                .text_right()
                .text_sm()
                .text_color(delta_color)
                .child(delta_text),
        )
}

// ==================== Drill-down data ====================
//
// The page loader lives in `data.rs`; the region / service-category
// breakdowns are small enough that their loader lives here, next to the
// card that renders them.

/// Which stored dimension the drill-down card groups the account's current
/// period by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DrillDim {
    Region,
    ServiceCategory,
}

impl DrillDim {
    const ALL: [DrillDim; 2] = [DrillDim::Region, DrillDim::ServiceCategory];

    fn label(self) -> &'static str {
        match self {
            DrillDim::Region => "Region",
            DrillDim::ServiceCategory => "Category",
        }
    }

    fn id(self) -> &'static str {
        match self {
            DrillDim::Region => "drill-region",
            DrillDim::ServiceCategory => "drill-category",
        }
    }

    /// The card's title in this dimension.
    fn title(self) -> &'static str {
        match self {
            DrillDim::Region => "By region",
            DrillDim::ServiceCategory => "By service category",
        }
    }

    /// The bucket column's table header.
    fn bucket_header(self) -> &'static str {
        match self {
            DrillDim::Region => "REGION",
            DrillDim::ServiceCategory => "SERVICE CATEGORY",
        }
    }

    /// Footnote under a breakdown that has an 'Other' bucket, naming what
    /// landed there.
    fn other_caption(self) -> &'static str {
        match self {
            DrillDim::Region => "Charges with no region read as 'Other'.",
            DrillDim::ServiceCategory => "Charges with no service category read as 'Other'.",
        }
    }
}

/// The account's current-period breakdowns behind the drill-down card.
struct AccountDrilldown {
    by_region: Vec<(String, f64)>,
    by_category: Vec<(String, f64)>,
}

/// Load the account's drill-down breakdowns. Blocking; the view wraps it
/// in the same `smol::unblock` as the page load.
fn load_drilldown(account_id: &str) -> Result<AccountDrilldown> {
    let account = db::get_all_accounts()?
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| anyhow::anyhow!("No account {account_id}"))?;
    let key = ingest::period_key(&account, &BillingPeriod::containing(Utc::now()));
    Ok(AccountDrilldown {
        by_region: breakdown_by(&key, BreakdownDim::Region)?,
        by_category: breakdown_by(&key, BreakdownDim::ServiceCategory)?,
    })
}

/// Load the account's data-quality findings for its current billing period,
/// worst severity first. Blocking; rides in the page load's `smol::unblock`.
///
/// The ledger's health checks are scoped to a billing period, not to one
/// account, so with several accounts in the same period the findings cover
/// them all. For the same reason a dismissal here is period-and-kind
/// scoped, not account scoped: dismissing a finding also hides it on the
/// other surfaces that show the same period and kind (the Overview strip
/// and the Accounts page's Data health card).
fn load_health(account_id: &str) -> Result<AccountHealth> {
    let account = db::get_all_accounts()?
        .into_iter()
        .find(|account| account.id == account_id)
        .ok_or_else(|| anyhow::anyhow!("No account {account_id}"))?;
    let key = ingest::period_key(&account, &BillingPeriod::containing(Utc::now()));
    let dismissed = db::dismissed_quality_issue_keys().unwrap_or_else(|e| {
        tracing::warn!("Could not read the dismissed data-quality issues: {}", e);
        Default::default()
    });
    let mut issues = Vec::new();
    let mut dismiss_keys = Vec::new();
    for issue in data_quality_issues(&key.billing_period, data::BUSINESS_LINE_TAG)? {
        let dismissal_key = issue.dismissal_key(&key.billing_period);
        if dismissed.contains(&dismissal_key) {
            continue;
        }
        dismiss_keys.push(dismissal_key);
        issues.push(issue);
    }
    issues.sort_by_key(|issue| accounts::severity_rank(issue.severity));
    Ok(AccountHealth {
        issues,
        dismiss_keys,
    })
}

/// The health section's data: the period's non-dismissed findings plus
/// their dismissal keys, so the section's Dismiss button can persist them
/// all in one click.
struct AccountHealth {
    issues: Vec<DataQualityIssue>,
    dismiss_keys: Vec<String>,
}

/// One drill-down row: bucket, amount, and a share-of-period bar. The
/// 'Other' bucket — charges with no value for the dimension — reads muted
/// so it is not mistaken for a real one.
fn drilldown_row(cx: &App, label: &str, amount: f64, total: f64, currency: &str) -> Div {
    let share = if total > 0.0 { amount / total } else { 0.0 };
    let label_color = if label == "Other" {
        theme::text_muted(cx)
    } else {
        theme::text_primary(cx)
    };
    div()
        .h_flex()
        .items_center()
        .py_2()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(
            div()
                .flex_1()
                // min_w_0 so a long bucket name truncates instead of
                // pushing the amount columns out of the card.
                .min_w_0()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_sm()
                .text_color(label_color)
                .child(label.to_string()),
        )
        .child(
            div()
                .w_24()
                .text_right()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_primary(cx))
                .child(fmt::amount(amount, currency)),
        )
        .child(
            div().w_32().px_2().child(
                div()
                    .w_full()
                    .h_2()
                    .rounded_full()
                    .bg(theme::sidebar_bg(cx))
                    .child(
                        div()
                            .h_full()
                            .w(relative(share as f32))
                            .rounded_full()
                            .bg(theme::accent(cx)),
                    ),
            ),
        )
}
