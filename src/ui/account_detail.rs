//! Account Detail View — one account's usage trend and service breakdown.

use gpui_kit::component::{button::*, scroll::ScrollableElement, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::data::{AccountDetailData, Range, ServiceRow};
use super::{chart, data, fmt, theme};
use crate::ui::theme::CardOutline as _;

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

    /// Load the page data off-thread; the ledger queries are blocking.
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
            let result = smol::unblock(move || data::load_account_detail(&account_id, range)).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                if this.generation == generation {
                    match result {
                        Ok(loaded) => {
                            this.data = Some(loaded);
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
                .child(self.render_chart(d, window, cx))
                .child(self.render_services(d, cx))
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
