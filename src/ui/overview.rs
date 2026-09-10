//! Overview View

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{button::*, scroll::ScrollableElement, *};

use super::{chart, data, theme};

/// Range selection of the header segmented control. Only MTD is backed by
/// real data; 30d and 12m stay selectable but the page always renders the
/// month to date.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Mtd,
    Days30,
    Months12,
}

impl Range {
    const ALL: [Range; 3] = [Range::Mtd, Range::Days30, Range::Months12];

    fn label(self) -> &'static str {
        match self {
            Range::Mtd => "MTD",
            Range::Days30 => "30d",
            Range::Months12 => "12m",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Range::Mtd => "range-mtd",
            Range::Days30 => "range-30d",
            Range::Months12 => "range-12m",
        }
    }
}

/// Overview View
pub struct OverviewView {
    /// Selected range in the header segmented control.
    range: Range,
    /// The loaded page data; `None` until the first load completes.
    data: Option<data::OverviewData>,
    /// First load in flight.
    loading: bool,
    /// A refresh (normal or forced) is running off-thread.
    refreshing: bool,
    /// Last load/refresh failure, shown under the header.
    error: Option<String>,
}

impl OverviewView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            range: Range::Mtd,
            data: None,
            loading: false,
            refreshing: false,
            error: None,
        };
        view.load(cx);
        view
    }

    /// Reload the page data. Called by the app shell when this page is
    /// navigated to; a no-op while a load is already in flight. Existing
    /// data stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.load(cx);
    }

    /// Load the page data off-thread; the ledger query is blocking.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(data::load_overview).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(loaded) => {
                        this.data = Some(loaded);
                        this.error = None;
                    }
                    Err(e) => {
                        this.error = Some(format!("Could not load the overview: {e}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Refresh every configured account, then reload the page data.
    ///
    /// `force` re-fetches periods that are still fresh (the Force refresh
    /// button); otherwise only stale periods are fetched. All of it is
    /// blocking, so it runs inside `smol::unblock`.
    fn refresh(&mut self, force: bool, cx: &mut Context<Self>) {
        if self.refreshing {
            return;
        }
        self.refreshing = true;
        self.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || -> Result<_, String> {
                let accounts = crate::db::get_all_accounts().map_err(|e| e.to_string())?;
                let mut failures = Vec::new();
                for account in &accounts {
                    if let Err(e) = data::refresh_account(account, force) {
                        failures.push(format!("{}: {}", account.name, e));
                    }
                }
                let overview = data::load_overview().map_err(|e| e.to_string())?;
                Ok((overview, failures))
            })
            .await;

            this.update(cx, |this, cx| {
                this.refreshing = false;
                match result {
                    Ok((loaded, failures)) => {
                        this.data = Some(loaded);
                        this.error = if failures.is_empty() {
                            None
                        } else {
                            Some(format!("Some sources failed: {}", failures.join("; ")))
                        };
                    }
                    Err(e) => {
                        this.error = Some(format!("Refresh failed: {e}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let selected = self.range;
        let refreshing = self.refreshing;
        let currency = self
            .data
            .as_ref()
            .map(|d| d.currency.as_str())
            .unwrap_or(crate::config::DEFAULT_REPORTING_CURRENCY);
        let caption = format!(
            "{}, month to date · reported in {}",
            chrono::Utc::now().format("%B %Y"),
            currency
        );

        div()
            .w_full()
            .h_flex()
            .items_start()
            .justify_between()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, "Overview"))
                    .child(theme::caption(cx, caption)),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_3()
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
                                let mut pill = div()
                                    .id(range.id())
                                    .px_3()
                                    .py_1()
                                    .rounded_full()
                                    .text_sm()
                                    .cursor_pointer();
                                if active {
                                    pill = pill
                                        .bg(theme::card_bg(cx))
                                        .border_1()
                                        .border_color(theme::card_border(cx))
                                        .text_color(theme::text_primary(cx))
                                        .font_weight(FontWeight::MEDIUM);
                                } else {
                                    pill = pill.text_color(theme::text_muted(cx));
                                }
                                pill.child(range.label()).on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.range = *range;
                                        cx.notify();
                                    },
                                ))
                            })),
                    )
                    .child(
                        Button::new("refresh")
                            .label(if refreshing {
                                "Refreshing…"
                            } else {
                                "Refresh"
                            })
                            .small()
                            .custom(outline_button(cx))
                            .disabled(refreshing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(false, cx);
                            })),
                    )
                    .child(
                        Button::new("force-refresh")
                            .label(if refreshing {
                                "Refreshing…"
                            } else {
                                "Force refresh"
                            })
                            .small()
                            .primary()
                            .disabled(refreshing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(true, cx);
                            })),
                    ),
            )
    }

    fn render_middle(&self, d: &data::OverviewData, cx: &mut Context<Self>) -> impl IntoElement {
        let lines = &d.business_lines;
        let max_amount = lines
            .iter()
            .map(|line| line.amount)
            .fold(0.0_f64, f64::max)
            .max(1.0);
        let currency = d.currency.as_str();

        div()
            .w_full()
            .h_flex()
            .gap_4()
            // Daily spend chart
            .child(
                theme::card(cx)
                    .flex_1()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .child(section_title(cx, "Daily spend, all sources"))
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_4()
                                    .child(legend_solid(cx, theme::accent(cx), "Actual"))
                                    .child(legend_dashed(cx, theme::olive(cx), "7-day baseline")),
                            ),
                    )
                    .child(chart::spend_area_chart(
                        cx,
                        &d.daily.actual,
                        &d.daily.baseline,
                        260.0,
                    ))
                    .child(theme::caption(
                        cx,
                        "Actual against the 7-day trailing mean.",
                    )),
            )
            // Where it went
            .child(
                theme::card(cx)
                    .w(px(340.0))
                    .flex_shrink_0()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(section_title(cx, "Where it went"))
                    .child(
                        div()
                            .v_flex()
                            .gap_3()
                            .children(lines.iter().enumerate().map(|(index, line)| {
                                let frac = (line.amount / max_amount) as f32;
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(
                                        div()
                                            .h_flex()
                                            .items_center()
                                            .justify_between()
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .text_color(theme::text_primary(cx))
                                                    .child(line.name.clone()),
                                            )
                                            .child(
                                                div()
                                                    .text_sm()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(theme::text_primary(cx))
                                                    .child(fmt_amount(line.amount, currency)),
                                            ),
                                    )
                                    .child(
                                        div()
                                            .w_full()
                                            .h_2()
                                            .rounded_full()
                                            .bg(theme::sidebar_bg(cx))
                                            .child(
                                                div()
                                                    .h_full()
                                                    .w(relative(frac))
                                                    .rounded_full()
                                                    .bg(line_color(cx, &line.name, index)),
                                            ),
                                    )
                            })),
                    )
                    .child(
                        Button::new("open-attribution")
                            .label("Open attribution")
                            .small()
                            .w_full()
                            .custom(outline_button(cx))
                            .on_click(|_, _, cx| {
                                crate::app::navigate_to(crate::app::CurrentView::Attribution, cx)
                            }),
                    ),
            )
    }

    /// Shown on an empty ledger instead of fake-looking zeros.
    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .items_center()
            .gap_2()
            .py(px(48.0))
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_primary(cx))
                    .child("No data yet"),
            )
            .child(theme::caption(
                cx,
                "Add an account from the Accounts page to see spend here.",
            ))
    }
}

impl Render for OverviewView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match &self.data {
            _ if self.loading && self.data.is_none() => div()
                .w_full()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child("Loading…")
                .into_any_element(),
            None => div().into_any_element(),
            Some(d) if is_empty(d) => self.render_empty_state(cx).into_any_element(),
            Some(d) => div()
                .v_flex()
                .gap_6()
                .child(render_stats(cx, &d.stats, &d.currency))
                .child(self.render_middle(d, cx))
                .child(render_movers(cx, &d.movers, &d.currency))
                .into_any_element(),
        };

        div().size_full().bg(theme::app_bg(cx)).child(
            div()
                .v_flex()
                .gap_6()
                .p(px(32.0))
                .overflow_y_scrollbar()
                .child(self.render_header(cx))
                .when_some(self.error.clone(), |el, error| {
                    el.child(
                        div()
                            .w_full()
                            .p_3()
                            .rounded_md()
                            .bg(theme::alert_tint(cx))
                            .text_sm()
                            .text_color(theme::text_primary(cx))
                            .child(error),
                    )
                })
                .child(body),
        )
    }
}

/// The empty-state condition: nothing spent and nothing recorded this month.
fn is_empty(d: &data::OverviewData) -> bool {
    d.stats.mtd_spend == 0.0 && d.daily.actual.is_empty() && d.business_lines.is_empty()
}

/// Row of the four headline stat cards.
fn render_stats(cx: &App, stats: &data::OverviewStats, currency: &str) -> impl IntoElement {
    div()
        .w_full()
        .h_flex()
        .gap_4()
        .child(stat_card(
            cx,
            "MONTH TO DATE",
            fmt_amount(stats.mtd_spend, currency),
            div().text_color(theme::accent(cx)).child(format!(
                "{:+.1}% vs same day last month",
                stats.mtd_change_pct
            )),
        ))
        .child(stat_card(
            cx,
            "MONTH-END FORECAST",
            fmt_amount(stats.forecast_month_end, currency),
            div()
                .text_color(theme::text_muted(cx))
                .child("MTD plus the mean of the last 7 days of burn"),
        ))
        .child(stat_card(
            cx,
            "UNALLOCATED",
            format!("{:.1}%", stats.unallocated_pct),
            div().text_color(theme::text_muted(cx)).child(format!(
                "{} with no tag or metric match",
                fmt_amount(stats.unallocated_amount, currency)
            )),
        ))
        .child(
            stat_card(
                cx,
                "OPEN ALERTS",
                stats.open_alerts.to_string(),
                div()
                    .text_color(theme::accent(cx))
                    .underline()
                    .child(format!(
                        "{} critical, {} warning →",
                        stats.critical_alerts, stats.warning_alerts
                    )),
            )
            .id("open-alerts-card")
            .cursor_pointer()
            .on_click(|_, _, cx| crate::app::navigate_to(crate::app::CurrentView::Alerts, cx))
            .bg(theme::alert_tint(cx)),
        )
}

/// "Biggest movers this month" table card.
fn render_movers(cx: &App, movers: &[data::MoverRow], currency: &str) -> impl IntoElement {
    theme::card(cx)
        .w_full()
        .p_5()
        .v_flex()
        .gap_4()
        .child(section_title(cx, "Biggest movers this month"))
        .child(
            div()
                .v_flex()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .pb_2()
                        .child(header_cell(cx, "SOURCE").w(px(120.0)))
                        .child(header_cell(cx, "MODEL OR SERVICE").flex_1())
                        .child(header_cell(cx, "MTD").w(px(110.0)).text_right())
                        .child(header_cell(cx, "Δ VS LAST MONTH").w(px(150.0)).text_right())
                        .child(header_cell(cx, "DRIVES").w(px(140.0))),
                )
                .children(movers.iter().map(|mover| {
                    let delta_color = if mover.change_pct < 0.0 {
                        theme::text_muted(cx)
                    } else {
                        theme::accent(cx)
                    };
                    let drives: AnyElement = if mover.drives == "Untagged" {
                        div()
                            .text_sm()
                            .text_color(theme::text_muted(cx))
                            .child(mover.drives.clone())
                            .into_any_element()
                    } else {
                        theme::pill(
                            mover.drives.clone(),
                            theme::warning_bg(cx),
                            theme::warning_text(cx),
                        )
                        .into_any_element()
                    };

                    div()
                        .h_flex()
                        .items_center()
                        .py_3()
                        .border_t_1()
                        .border_color(theme::card_border(cx))
                        .child(
                            div()
                                .w(px(120.0))
                                .text_sm()
                                .text_color(theme::text_muted(cx))
                                .child(mover.provider.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                .text_sm()
                                .text_color(theme::text_primary(cx))
                                .child(mover.service.clone()),
                        )
                        .child(
                            div()
                                .w(px(110.0))
                                .text_right()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_primary(cx))
                                .child(fmt_amount(mover.amount, currency)),
                        )
                        .child(
                            div()
                                .w(px(150.0))
                                .text_right()
                                .text_sm()
                                .text_color(delta_color)
                                .child(format!("{:+.0}%", mover.change_pct)),
                        )
                        .child(div().w(px(140.0)).child(drives))
                })),
        )
}

fn stat_card(cx: &App, label: &'static str, value: String, sub: Div) -> Div {
    theme::card(cx)
        .flex_1()
        .p_5()
        .v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme::text_muted(cx))
                .child(label),
        )
        .child(
            div()
                .text_2xl()
                .font_weight(FontWeight::BOLD)
                .text_color(theme::text_primary(cx))
                .child(value),
        )
        .child(div().text_sm().child(sub))
}

fn section_title(cx: &App, text: &'static str) -> Div {
    div()
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_primary(cx))
        .child(text)
}

fn header_cell(cx: &App, text: &'static str) -> Div {
    div()
        .text_xs()
        .text_color(theme::text_muted(cx))
        .child(text)
}

fn legend_solid(cx: &App, color: Hsla, label: &'static str) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(div().w(px(16.0)).h(px(2.0)).rounded_full().bg(color))
        .child(
            div()
                .text_xs()
                .text_color(theme::text_muted(cx))
                .child(label),
        )
}

fn legend_dashed(cx: &App, color: Hsla, label: &'static str) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_0p5()
                .children((0..3).map(|_| div().w(px(4.0)).h(px(2.0)).rounded_full().bg(color))),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_muted(cx))
                .child(label),
        )
}

/// Bar color for a business line: "Unallocated" is grey, everything else
/// alternates accent/olive by row position (tag names are user data now,
/// so the mock's fixed name-to-color map no longer applies).
fn line_color(cx: &App, name: &str, index: usize) -> Hsla {
    if name == "Unallocated" {
        theme::grey(cx)
    } else if index % 2 == 0 {
        theme::accent(cx)
    } else {
        theme::olive(cx)
    }
}

/// Outline-style button in the warm palette.
fn outline_button(cx: &App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(theme::card_bg(cx))
        .foreground(theme::text_primary(cx))
        .border(theme::card_border(cx))
        .hover(theme::sidebar_bg(cx))
        .active(theme::sidebar_bg(cx))
}

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

/// Whole-unit amount with thousands separators, e.g. `$51,080`.
fn fmt_amount(amount: f64, currency: &str) -> String {
    let symbol = currency_symbol(currency);
    let prefix = if symbol.is_empty() {
        format!("{currency} ")
    } else {
        symbol.to_string()
    };

    let rounded = amount.round() as i64;
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
