//! Overview View

use gpui_kit::component::{button::*, scroll::ScrollableElement, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::data::Range;
use super::{chart, data, fmt, theme};
use crate::ui::theme::CardOutline as _;

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
    /// When the view was created; backs the pre-load header caption so
    /// render output is deterministic given state.
    opened_at: chrono::DateTime<chrono::Utc>,
    /// Bumped by every load/refresh; only the latest flight may write its
    /// result, so an older load cannot clobber a newer refresh.
    generation: u64,
    /// Chart hover state (point under the mouse + the canvas's per-frame
    /// geometry cells); drives the hover guide, dot, and tooltip.
    chart_hover: chart::ChartHover,
}

impl OverviewView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            range: Range::Mtd,
            data: None,
            loading: false,
            refreshing: false,
            error: None,
            opened_at: chrono::Utc::now(),
            generation: 0,
            chart_hover: chart::ChartHover::new(),
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
        self.generation += 1;
        let generation = self.generation;
        // New data may shift the points; a stale hover would tag the
        // wrong month.
        self.chart_hover.clear();
        cx.notify();
        let range = self.range;
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || data::load_overview(range)).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                // A range switch or refresh during the flight started a
                // newer load; its data wins over this stale result.
                if this.generation == generation {
                    match result {
                        Ok(loaded) => {
                            this.data = Some(loaded);
                            this.error = None;
                        }
                        Err(e) => {
                            this.error = Some(format!("Could not load the overview: {e}"));
                        }
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
        self.generation += 1;
        let generation = self.generation;
        cx.notify();

        let range = self.range;
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || -> Result<_, String> {
                let accounts = crate::db::get_all_accounts().map_err(|e| e.to_string())?;
                let mut failures = Vec::new();
                for account in &accounts {
                    if let Err(e) = data::refresh_account(account, force) {
                        failures.push(format!("{}: {}", account.name, e));
                    }
                }
                let overview = data::load_overview(range).map_err(|e| e.to_string())?;
                Ok((overview, failures))
            })
            .await;

            this.update(cx, |this, cx| {
                this.refreshing = false;
                // A range switch or newer refresh during the flight
                // started a newer load; its data wins over this stale
                // result.
                if this.generation == generation {
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
        let caption = match &self.data {
            Some(d) => format!("{} · reported in {}", d.window_caption, d.currency),
            None => format!(
                "{} · reported in {}",
                self.range.header_caption(self.opened_at),
                data::reporting_currency()
            ),
        };

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
                                    // The raised chip carries the card border.
                                    button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                                } else {
                                    button
                                }
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
                            .custom(theme::accent_variant(cx))
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
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .disabled(refreshing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(true, cx);
                            })),
                    ),
            )
    }

    fn render_middle(
        &self,
        d: &data::OverviewData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
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
            .items_stretch()
            .gap_4()
            // Spend chart
            .child(
                theme::card(cx)
                    .flex_1()
                    // min_w_0: a flex child may not shrink below its
                    // content by default; the chart canvas must compress
                    // inside the h_flex on narrow windows.
                    .min_w_0()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .child(theme::section_title(cx, d.chart_title))
                            .child(
                                div()
                                    .h_flex()
                                    .items_center()
                                    .gap_4()
                                    .child(legend_solid(cx, theme::accent(cx), "Actual"))
                                    // The 12-month range has no baseline
                                    // series, so it gets no legend entry.
                                    .when(!d.chart.baseline.is_empty(), |el| {
                                        el.child(legend_dashed(
                                            cx,
                                            theme::olive(cx),
                                            "7-day baseline",
                                        ))
                                    }),
                            ),
                    )
                    .child(self.render_chart(d, window, cx))
                    // The rolling ranges get x-axis endpoints; MTD's
                    // calendar-month context is already in the header.
                    .when(d.range != Range::Mtd, |el| {
                        let first = d
                            .chart
                            .actual
                            .first()
                            .map(|p| p.label.clone())
                            .unwrap_or_default();
                        let last = d
                            .chart
                            .actual
                            .last()
                            .map(|p| p.label.clone())
                            .unwrap_or_default();
                        el.child(
                            div()
                                .h_flex()
                                .justify_between()
                                .child(theme::caption(cx, first))
                                .child(theme::caption(cx, last)),
                        )
                    })
                    // Only the 12-month caption adds information beyond
                    // the legend; MTD/30d just restate it.
                    .when(d.range == Range::Months12, |el| {
                        el.child(theme::caption(cx, d.chart_caption))
                    }),
            )
            // Where it went
            .child(
                theme::card(cx)
                    // 320px ≈ the 20rem step.
                    .w_80()
                    .flex_shrink_0()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(theme::section_title(cx, "Where it went"))
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
                                                    .child(fmt::amount(line.amount, currency)),
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
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .on_click(|_, _, cx| {
                                crate::app::navigate_to(crate::app::CurrentView::Attribution, cx)
                            }),
                    ),
            )
    }

    /// The spend chart with hover interactivity: the canvas publishes its
    /// bounds and point coordinates every frame; the wrapper maps the
    /// mouse position to the nearest point and the shared overlay draws
    /// the guide, dot, and tooltip on top.
    fn render_chart(
        &self,
        d: &data::OverviewData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id("spend-chart")
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
                // 260px at the default 16px rem.
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
            .py_12()
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = match &self.data {
            _ if self.loading && self.data.is_none() => div()
                .w_full()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child("Loading overview…")
                .into_any_element(),
            None => div().into_any_element(),
            Some(d) if is_empty(d) => self.render_empty_state(cx).into_any_element(),
            Some(d) => div()
                .v_flex()
                .gap_6()
                .child(render_stats(cx, d))
                .child(self.render_middle(d, window, cx))
                .child(render_movers(cx, d))
                .into_any_element(),
        };

        div().size_full().bg(theme::app_bg(cx)).child(
            div()
                .v_flex()
                .gap_6()
                // Full height so the scroll region fills the window and
                // the scrollbar can actually engage.
                .h_full()
                .p_8()
                .overflow_y_scrollbar()
                .child(self.render_header(cx))
                .when_some(self.error.clone(), |el, error| {
                    el.child(
                        div()
                            .w_full()
                            .p_3()
                            .rounded_md()
                            .bg(theme::danger_bg(cx))
                            .text_sm()
                            .text_color(theme::text_primary(cx))
                            .child(error),
                    )
                })
                .child(body),
        )
    }
}

/// The empty-state condition: nothing spent and nothing recorded in the
/// window.
fn is_empty(d: &data::OverviewData) -> bool {
    d.stats.spend == 0.0 && d.chart.actual.is_empty() && d.business_lines.is_empty()
}

/// Row of the four headline stat cards.
fn render_stats(cx: &App, d: &data::OverviewData) -> impl IntoElement {
    let stats = &d.stats;
    let currency = d.currency.as_str();
    div()
        .w_full()
        .h_flex()
        .items_stretch()
        .gap_4()
        .child(theme::stat_card(
            cx,
            d.spend_label,
            fmt::amount(stats.spend, currency),
            div()
                .h_flex()
                .gap_1()
                // The card shrinks below this row on narrow windows; let
                // the change span wrap under the usage/credits split
                // instead of being sliced by the next card.
                .min_w_0()
                .flex_wrap()
                // Net stays the big number; the split shows why it differs
                // from the real burn.
                .child(div().text_color(theme::text_muted(cx)).child(format!(
                    "usage {} · credits {}",
                    fmt::amount(stats.usage, currency),
                    fmt::amount(stats.credits, currency)
                )))
                .when_some(stats.change_pct, |el, pct| {
                    el.child(div().text_color(theme::accent(cx)).child(format!(
                        "· {} {}",
                        fmt::change_pct(pct),
                        d.change_caption
                    )))
                }),
        ))
        .child(theme::stat_card(
            cx,
            d.card2_label,
            fmt::amount(d.card2_value, currency),
            div()
                .text_color(theme::text_muted(cx))
                .child(d.card2_caption),
        ))
        .child(theme::stat_card(
            cx,
            "UNALLOCATED",
            format!("{:.1}%", stats.unallocated_pct),
            div().text_color(theme::text_muted(cx)).child(format!(
                "{} with no tag or metric match",
                fmt::amount(stats.unallocated_amount, currency)
            )),
        ))
        .child(
            theme::stat_card(
                cx,
                "OPEN ALERTS",
                stats.open_alerts.to_string(),
                Button::new("open-alerts")
                    .label(format!(
                        "{} critical, {} warning →",
                        stats.critical_alerts, stats.warning_alerts
                    ))
                    .link()
                    .small()
                    .text_color(theme::accent(cx))
                    .on_click(|_, _, cx| {
                        crate::app::navigate_to(crate::app::CurrentView::Alerts, cx)
                    }),
            )
            .when(stats.open_alerts > 0, |el| el.bg(theme::alert_tint(cx))),
        )
}

/// "Biggest movers" table card.
fn render_movers(cx: &App, d: &data::OverviewData) -> impl IntoElement {
    let movers = &d.movers;
    let currency = d.currency.as_str();
    theme::card(cx)
        .w_full()
        .p_5()
        .v_flex()
        .gap_4()
        .child(theme::section_title(cx, d.movers_title))
        .child(
            div()
                .v_flex()
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .pb_2()
                        .child(theme::header_cell(cx, "SOURCE").w_32())
                        .child(
                            theme::header_cell(cx, "MODEL OR SERVICE")
                                .flex_1()
                                .min_w_0(),
                        )
                        .child(
                            theme::header_cell(cx, d.movers_amount_header.clone())
                                .w_24()
                                .text_right(),
                        )
                        .child(
                            theme::header_cell(cx, d.movers_delta_header)
                                .w_40()
                                .text_right(),
                        )
                        .child(theme::header_cell(cx, "DRIVES").w_32()),
                )
                .children(movers.iter().map(|mover| {
                    let (delta_text, delta_color) = match mover.change_pct {
                        Some(pct) if pct < 0.0 => (fmt::change_pct(pct), theme::text_muted(cx)),
                        Some(pct) => (fmt::change_pct(pct), theme::accent(cx)),
                        // No meaningful previous-period base to compare
                        // against.
                        None => ("—".to_string(), theme::text_muted(cx)),
                    };
                    let drives: AnyElement = if mover.drives == data::UNALLOCATED {
                        div()
                            .text_sm()
                            .text_color(theme::text_muted(cx))
                            .child(mover.drives.clone())
                            .into_any_element()
                    } else {
                        theme::pill(
                            mover.drives.clone(),
                            theme::sidebar_bg(cx),
                            theme::text_primary(cx),
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
                                .w_32()
                                .text_sm()
                                .text_color(theme::text_muted(cx))
                                .child(mover.provider.clone()),
                        )
                        .child(
                            div()
                                .flex_1()
                                // min_w_0 so a long service name truncates
                                // instead of pushing the amount columns
                                // out of the card.
                                .min_w_0()
                                .text_sm()
                                .text_color(theme::text_primary(cx))
                                .child(mover.service.clone()),
                        )
                        .child(
                            div()
                                .w_24()
                                .text_right()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text_primary(cx))
                                .child(fmt::amount(mover.amount, currency)),
                        )
                        .child(
                            div()
                                .w_40()
                                .text_right()
                                .text_sm()
                                .text_color(delta_color)
                                .child(delta_text),
                        )
                        .child(div().w_32().child(drives))
                })),
        )
}

fn legend_solid(cx: &App, color: Hsla, label: &'static str) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_2()
        // Decorative 16×2 swatch at the default rem; rem-based so it
        // zooms with the base font.
        .child(div().w_4().h_0p5().rounded_full().bg(color))
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
                // Decorative 4×2 dashes at the default rem; rem-based so
                // they zoom with the base font.
                .children((0..3).map(|_| div().w_1().h_0p5().rounded_full().bg(color))),
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
    } else if index.is_multiple_of(2) {
        theme::accent(cx)
    } else {
        theme::olive(cx)
    }
}
