//! Overview View

use gpui_kit::component::{button::*, scroll::ScrollableElement, skeleton::Skeleton, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::data::Range;
use super::data_health::{self, HealthCard};
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
    /// The reporting currency, read once at creation and refreshed from
    /// every landed load; the pre-load header caption reads it here rather
    /// than re-parsing the config file on every frame.
    reporting_currency: String,
    /// Bumped by every load/refresh; only the latest flight may write its
    /// result, so an older load cannot clobber a newer refresh.
    generation: u64,
    /// Chart hover state (point under the mouse + the canvas's per-frame
    /// geometry cells); drives the hover guide, dot, and tooltip.
    chart_hover: chart::ChartHover,
    /// The user dismissed the Data health card; it stays hidden until the
    /// next load. Dismissal is also persisted per finding
    /// (`{kind}:{billing_period}` in the app-state database) and filtered
    /// at the data layer, so a refresh can only bring the card back with
    /// a finding that is genuinely new — never with a dismissed one.
    health_dismissed: bool,
    /// The Data health card lists every finding rather than the worst few.
    health_expanded: bool,
    /// Demo data is being loaded or cleared from this page.
    demo_running: bool,
}

impl OverviewView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            range: Range::Mtd,
            data: None,
            loading: false,
            refreshing: false,
            error: None,
            opened_at: chrono::Utc::now(),
            reporting_currency: data::reporting_currency(),
            generation: 0,
            chart_hover: chart::ChartHover::new(),
            health_dismissed: false,
            health_expanded: false,
            demo_running: false,
        }
    }

    /// Start the first load if none has run. The app shell calls this on
    /// the page's first visit, so construction — and window opening —
    /// stays cheap and the hidden pages do not race the visible one for
    /// the ledger at startup.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.data.is_none() && !self.loading {
            self.load(cx);
        }
    }

    /// Reload the page data. Called by the app shell when this page is
    /// navigated to; a no-op while a load is already in flight. Existing
    /// data stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading || self.refreshing {
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
                            this.reporting_currency = loaded.currency.clone();
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
        if self.refreshing || self.loading {
            return;
        }
        self.refreshing = true;
        self.error = None;
        self.generation += 1;
        let generation = self.generation;
        cx.notify();

        let range = self.range;
        cx.spawn(async move |this, cx| {
            let result = async {
                let accounts = smol::unblock(crate::db::get_all_accounts)
                    .await
                    .map_err(|e| e.to_string())?;
                let mut failures = Vec::new();
                // Publish each success as soon as it lands, even if another
                // account is slow, the window closes, or loading the page fails.
                for account in accounts.into_iter().filter(|account| {
                    account
                        .descriptor()
                        .is_some_and(|source| source.fetches_from_api())
                }) {
                    let id = account.id.clone();
                    let name = account.name.clone();
                    match smol::unblock(move || data::refresh_account(&account, force)).await {
                        Ok(_) => {
                            cx.update(|cx| crate::app::clear_account_failure(&id, cx));
                        }
                        Err(e) => failures.push(format!("{name}: {e}")),
                    }
                }
                let overview = smol::unblock(move || data::load_overview(range))
                    .await
                    .map_err(|e| e.to_string())?;
                Ok::<_, String>((overview, failures))
            }
            .await;

            this.update(cx, |this, cx| {
                this.refreshing = false;
                // A range switch or newer refresh during the flight
                // started a newer load; its data wins over this stale
                // result.
                if this.generation == generation {
                    match result {
                        Ok((loaded, failures)) => {
                            this.reporting_currency = loaded.currency.clone();
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
        let busy = self.loading || refreshing;
        let caption = match &self.data {
            Some(d) => format!("{}, reported in {}", d.window_caption, d.currency),
            None => format!(
                "{}, reported in {}",
                self.range.header_caption(self.opened_at),
                self.reporting_currency
            ),
        };

        div()
            .w_full()
            .flex_shrink_0()
            .h_flex()
            .items_center()
            .flex_wrap()
            .gap_4()
            .px_6()
            .py_5()
            .border_b_1()
            .border_color(theme::card_border(cx))
            .bg(theme::card_bg(cx))
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
                    .flex_wrap()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_1()
                            .p_1()
                            .rounded(cx.theme().radius)
                            .border_1()
                            .border_color(theme::card_border(cx))
                            .children(Range::ALL.iter().map(|range| {
                                let active = *range == selected;
                                let button = Button::new(range.id())
                                    .label(range.label())
                                    .tooltip(match range {
                                        Range::Mtd => "This month to date",
                                        Range::Days30 => "Last 30 days",
                                        Range::Months12 => "Last 12 months",
                                    })
                                    .disabled(busy)
                                    .small()
                                    .rounded(cx.theme().radius)
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
                            .icon(IconName::RefreshCw)
                            .label(if refreshing {
                                "Refreshing…"
                            } else if self.loading {
                                "Loading…"
                            } else if cfg!(target_family = "wasm") {
                                "Reload demo"
                            } else {
                                "Refresh"
                            })
                            .small()
                            .primary()
                            .loading(busy)
                            .tooltip(if cfg!(target_family = "wasm") {
                                "Reload the sample ledger; no live provider data is fetched"
                            } else {
                                "Fetch stale sources and update this overview"
                            })
                            .disabled(busy)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(false, cx);
                            })),
                    )
                    .when(cfg!(not(target_family = "wasm")), |el| {
                        el.child(
                            Button::new("force-refresh")
                                .label("Force refresh")
                                .tooltip(
                                    "Fetch every source again, including recently synced periods",
                                )
                                .small()
                                .custom(theme::outline_variant(cx))
                                .card_outline(cx)
                                .disabled(busy)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.refresh(true, cx);
                                })),
                        )
                    }),
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
        let compact = window.viewport_size().width < rems(80.0).to_pixels(window.rem_size());

        div()
            .w_full()
            .h_flex()
            .when(compact, |el| el.flex_col())
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
                            .flex_wrap()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            .child(theme::section_title(cx, d.chart_title))
                            .child(
                                div()
                                    .h_flex()
                                    .flex_wrap()
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
                                    })
                                    // Only the MTD chart carries the
                                    // typical-day benchmark line.
                                    .when(d.chart_benchmark.is_some(), |el| {
                                        el.child(legend_dotted(
                                            cx,
                                            theme::grey(cx),
                                            "Typical day (6-mo avg)",
                                        ))
                                    }),
                            ),
                    )
                    .child(self.render_chart_with_axis(d, window, cx))
                    // Endpoints make every range readable without hovering.
                    .when(!d.chart.actual.is_empty(), |el| {
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
                                .pl(rems(6.25))
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
                    // A compact breakdown leaves most of the row for the trend.
                    .when(compact, |el| el.w_full())
                    .when(!compact, |el| el.w_64())
                    .flex_shrink_0()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(theme::section_title(cx, "Spend breakdown"))
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
                                            .rounded(cx.theme().radius)
                                            .bg(theme::sidebar_bg(cx))
                                            .child(
                                                div()
                                                    .h_full()
                                                    .w(relative(frac))
                                                    .rounded(cx.theme().radius)
                                                    .bg(line_color(cx, &line.name, index)),
                                            ),
                                    )
                            })),
                    )
                    .child(
                        Button::new("open-attribution")
                            .label("View attribution")
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

    /// Currency ticks use exactly the same range as the canvas and benchmark.
    fn render_chart_with_axis(
        &self,
        d: &data::OverviewData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let (min, max) = chart::padded_y_range(
            d.chart
                .actual
                .iter()
                .chain(&d.chart.baseline)
                .map(|p| p.amount),
        );
        div()
            .h_flex()
            .gap_3()
            .child(
                div()
                    .w(rems(5.5))
                    .flex_shrink_0()
                    .h(rems(16.25))
                    .relative()
                    .text_right()
                    .children((0..=3).map(|i| {
                        div()
                            .absolute()
                            .right_0()
                            .top(relative(i as f32 / 3.0))
                            .mt(rems(-0.5))
                            .line_height(rems(1.0))
                            .text_xs()
                            .text_color(theme::text_muted(cx))
                            .child(fmt::amount(max - (max - min) * i as f64 / 3.0, &d.currency))
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(self.render_chart(d, window, cx)),
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
            // The typical-day benchmark: a flat dotted grey line layered
            // over the chart canvas.
            .when_some(
                d.chart_benchmark.map(|value| benchmark_frac(d, value)),
                |el, frac| el.child(benchmark_overlay(cx, frac)),
            )
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
    /// Load or clear the demo ledger from this page, then have the shell
    /// reload it and the status bar. The same operation as Settings' demo
    /// buttons.
    fn set_demo_data(&mut self, load: bool, cx: &mut Context<Self>) {
        if self.demo_running {
            return;
        }
        self.demo_running = true;
        self.error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || data::set_demo_data(load)).await;
            this.update(cx, |this, cx| {
                this.demo_running = false;
                match result {
                    Ok(_) => crate::app::request_reload(cx),
                    Err(e) => {
                        tracing::error!("Demo data operation failed: {}", e);
                        this.error = Some(format!("Couldn't change the demo data: {e}"));
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The empty page. With no accounts it is the first step — add one, or
    /// look around with demo data; with accounts but no spend it says why
    /// the page is still blank.
    fn render_empty_state(
        &self,
        d: &data::OverviewData,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let no_accounts = d.accounts.real + d.accounts.demo == 0;
        let (title, caption) = if no_accounts {
            (
                "No accounts yet",
                "Add a billing account to see your spend here, or load demo data to look around first.",
            )
        } else {
            (
                "No spend yet",
                "Refresh fetches the current bill. Providers can take up to a day to report \
                 new charges, and a source read from a file shows spend once its bill is \
                 imported on the Accounts page.",
            )
        };

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
                    .child(title),
            )
            .child(
                div()
                    .max_w(rems(30.0))
                    .text_center()
                    .child(theme::caption(cx, caption)),
            )
            .when(no_accounts, |el| {
                el.child(
                    div()
                        .pt_2()
                        .h_flex()
                        .gap_2()
                        .child(
                            Button::new("empty-add-account")
                                .label("Add account…")
                                .primary()
                                .on_click(|_, _, cx| crate::app::open_add_account(cx)),
                        )
                        .child(
                            Button::new("empty-load-demo")
                                .label("Load demo data")
                                .custom(theme::outline_variant(cx))
                                .card_outline(cx)
                                .loading(self.demo_running)
                                .disabled(self.demo_running)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.set_demo_data(true, cx);
                                })),
                        ),
                )
            })
    }

    /// A quiet line over the page while demo accounts are configured, so
    /// the demo bill is not read as a real one, with the way to remove it.
    fn render_demo_notice(
        &self,
        d: &data::OverviewData,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let text = if d.accounts.real == 0 {
            "Showing demo data"
        } else {
            "Demo accounts are included in these totals"
        };
        div()
            .w_full()
            .h_flex()
            .items_center()
            .gap_3()
            .child(theme::pill_outline(cx, "Demo"))
            .child(theme::caption(cx, text))
            .child(
                Button::new("clear-demo")
                    .label("Clear demo data")
                    .small()
                    .ghost()
                    .loading(self.demo_running)
                    .disabled(self.demo_running)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.set_demo_data(false, cx);
                    })),
            )
    }

    /// The Data health card under the header, worst finding first. It
    /// qualifies the figures rather than being one, and stays collapsed
    /// until asked: the spend cards are what the page is for and must not
    /// slide below the fold.
    ///
    /// Dismiss persists every finding's key (`{kind}:{billing_period}`),
    /// shown or not, and hides the card; the data layer filters dismissed
    /// findings out of every later load, so the card only reappears when
    /// a refresh surfaces something new.
    fn render_health_card(
        &self,
        cx: &mut Context<Self>,
        d: &data::OverviewData,
    ) -> impl IntoElement {
        let rows = d
            .data_quality
            .iter()
            .map(|issue| {
                data_health::issue_row(
                    issue.severity.issue_severity(),
                    issue.message.clone(),
                    issue.affected_amount,
                    &d.currency,
                    cx,
                )
                .into_any_element()
            })
            .collect();

        HealthCard::new("overview-health", rows)
            .compact()
            .expanded(self.health_expanded)
            .on_toggle(cx.listener(|this, _, _, cx| {
                this.health_expanded = !this.health_expanded;
                cx.notify();
            }))
            .on_dismiss(cx.listener(|this, _, _, cx| {
                let keys: Vec<String> = this
                    .data
                    .as_ref()
                    .map(|d| d.data_quality.iter().map(|row| row.key.clone()).collect())
                    .unwrap_or_default();
                if let Err(e) = data::dismiss_quality_issues(&keys) {
                    tracing::warn!("Could not persist the data-quality dismissal: {}", e);
                }
                this.health_dismissed = true;
                cx.notify();
            }))
    }
}

impl Render for OverviewView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = window.viewport_size().width / window.rem_size();
        let columns = if width < 56.0 {
            1
        } else if width < 80.0 {
            2
        } else {
            4
        };
        let body: AnyElement = match &self.data {
            // Loading, or not started yet — the desktop holds the first
            // load until its stores open. Only a failed load shows no body;
            // its error sits under the header.
            None if self.loading || self.error.is_none() => {
                render_skeleton(cx, columns).into_any_element()
            }
            None => div().into_any_element(),
            Some(d) if is_empty(d) => self.render_empty_state(d, cx).into_any_element(),
            Some(d) => div()
                .v_flex()
                .gap_4()
                .child(render_stats(cx, d, columns))
                .child(self.render_middle(d, window, cx))
                .child(render_movers(cx, d, columns < 4))
                // MTD only; the rolling ranges carry no month-over-month.
                .when_some(d.month_over_month.as_ref(), |el, mom| {
                    el.child(render_month_over_month(cx, &d.currency, mom, columns < 4))
                })
                .into_any_element(),
        };

        div()
            .size_full()
            .v_flex()
            .bg(theme::app_bg(cx))
            .child(self.render_header(cx))
            .child(
                div()
                    .v_flex()
                    .gap_4()
                    // Full height so the scroll region fills the window and
                    // the scrollbar can actually engage.
                    .flex_1()
                    .min_h_0()
                    .p_6()
                    .overflow_y_scrollbar()
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
                    .when_some(
                        self.data
                            .as_ref()
                            .filter(|d| !d.data_quality.is_empty() && !self.health_dismissed),
                        |el, d| el.child(self.render_health_card(cx, d)),
                    )
                    .when_some(
                        self.data.as_ref().filter(|d| d.accounts.demo > 0),
                        |el, d| el.child(self.render_demo_notice(d, cx)),
                    )
                    .child(body),
            )
    }
}

/// The empty-state condition: nothing spent and nothing recorded in the
/// window.
fn is_empty(d: &data::OverviewData) -> bool {
    d.stats.spend == 0.0 && d.chart.actual.is_empty() && d.business_lines.is_empty()
}

/// First-load placeholder shaped like the loaded page — the four stat
/// cards, then the chart card beside the "Spend breakdown" card at their
/// final paddings — so the landing content does not jump the layout.
fn render_skeleton(cx: &App, columns: u16) -> impl IntoElement {
    let stat = || {
        theme::card(cx)
            .flex_1()
            .min_w_0()
            .p_5()
            .v_flex()
            .gap_2()
            .child(Skeleton::new().w_24().h_3())
            .child(Skeleton::new().w_32().h_6())
            .child(Skeleton::new().w_full().h_3())
    };

    div()
        .v_flex()
        .gap_4()
        .child(
            div()
                .w_full()
                .grid()
                .grid_cols(columns)
                .items_stretch()
                .gap_4()
                .children((0..4).map(|_| stat())),
        )
        .child(
            div()
                .w_full()
                .h_flex()
                .when(columns < 4, |el| el.flex_col())
                .items_stretch()
                .gap_4()
                .child(
                    theme::card(cx)
                        .flex_1()
                        .min_w_0()
                        .p_5()
                        .v_flex()
                        .gap_4()
                        .child(Skeleton::new().w_40().h_4())
                        // Matches the spend chart's rems(16.25) canvas.
                        .child(Skeleton::new().w_full().h(rems(16.25))),
                )
                .child(
                    theme::card(cx)
                        .when(columns < 4, |el| el.w_full())
                        .when(columns == 4, |el| el.w_64())
                        .flex_shrink_0()
                        .p_5()
                        .v_flex()
                        .gap_4()
                        .child(Skeleton::new().w_32().h_4())
                        .children((0..4).map(|_| Skeleton::new().w_full().h_3())),
                ),
        )
}

/// Row of the four headline stat cards.
fn render_stats(cx: &App, d: &data::OverviewData, columns: u16) -> impl IntoElement {
    let stats = &d.stats;
    let currency = d.currency.as_str();
    div()
        .w_full()
        .grid()
        .grid_cols(columns)
        .items_stretch()
        .gap_4()
        .child(theme::stat_card(
            cx,
            d.spend_label,
            fmt::amount(stats.spend, currency),
            // Two lines, not one wrapping row: at a card's width the change
            // wrapped under the split anyway, and a "·" separator then led
            // the second line with nothing before it.
            div()
                .v_flex()
                .gap_1()
                .min_w_0()
                // Net stays the big number; the split shows why it differs
                // from the real burn.
                .child(div().text_color(theme::text_muted(cx)).child(format!(
                    "usage {}, credits {}",
                    fmt::amount(stats.usage, currency),
                    fmt::amount(stats.credits, currency)
                )))
                .when_some(stats.change_pct, |el, pct| {
                    el.child(div().text_color(theme::accent(cx)).child(format!(
                        "{} {}",
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
                .child(match &d.forecast_band {
                    // MTD: the confidence band under the point forecast.
                    Some(band) => format!(
                        "range {}–{}",
                        fmt::amount(band.pessimistic, currency),
                        fmt::amount(band.optimistic, currency)
                    ),
                    None => d.card2_caption.to_string(),
                }),
        ))
        .child(
            theme::stat_card(
                cx,
                "UNALLOCATED",
                format!("{:.1}%", stats.unallocated_pct),
                div().text_color(theme::text_muted(cx)).child(format!(
                    "{} with no tag or metric match",
                    fmt::amount(stats.unallocated_amount, currency)
                )),
            )
            .child(
                Button::new("review-allocation")
                    .label("Review allocation")
                    .ghost()
                    .small()
                    .justify_start()
                    .text_color(theme::accent(cx))
                    .on_click(|_, _, cx| {
                        crate::app::navigate_to(crate::app::CurrentView::Attribution, cx)
                    }),
            ),
        )
        .child(
            theme::stat_card(
                cx,
                "OPEN ALERTS",
                stats.open_alerts.to_string(),
                div().text_color(theme::text_muted(cx)).child(format!(
                    "{} critical, {} warning",
                    stats.critical_alerts, stats.warning_alerts
                )),
            )
            .when(stats.open_alerts > 0, |el| {
                el.border_color(theme::danger(cx).opacity(0.35))
            })
            .child(
                Button::new("review-alerts")
                    .label("View alerts")
                    .ghost()
                    .small()
                    .justify_start()
                    .text_color(theme::accent(cx))
                    .on_click(|_, _, cx| {
                        crate::app::navigate_to(crate::app::CurrentView::Alerts, cx)
                    }),
            ),
        )
}

/// "Biggest movers" table card.
fn render_movers(cx: &App, d: &data::OverviewData, compact: bool) -> impl IntoElement {
    let movers = &d.movers;
    let currency = d.currency.as_str();
    theme::card(cx)
        .w_full()
        .p_5()
        .v_flex()
        .gap_4()
        .child(theme::section_title(cx, d.movers_title))
        .when(compact, |el| {
            el.child(theme::caption(cx, "Scroll horizontally to see all columns"))
        })
        .child(
            div().w_full().overflow_x_scrollbar().child(
                div()
                    .min_w(rems(52.0))
                    .v_flex()
                    .child(
                        // The same column gap on the header and every row: the
                        // right-aligned change column otherwise runs straight
                        // into the left-aligned DRIVES column beside it.
                        div()
                            .h_flex()
                            .items_center()
                            .gap_4()
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
                            .gap_4()
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
            ),
        )
}

/// "Month over month" card (MTD range only): net this month to date
/// against the whole of last month, total and per service.
fn render_month_over_month(
    cx: &App,
    currency: &str,
    mom: &data::MonthOverMonth,
    compact: bool,
) -> impl IntoElement {
    let delta = mom.change_pct.map(|pct| {
        let color = if pct < 0.0 {
            theme::text_muted(cx)
        } else {
            theme::accent(cx)
        };
        div()
            .text_sm()
            .text_color(color)
            .child(fmt::change_pct(pct))
    });
    theme::card(cx)
        .w_full()
        .p_5()
        .v_flex()
        .gap_4()
        .child(theme::section_title(cx, "Month over month"))
        .when(compact, |el| {
            el.child(theme::caption(cx, "Scroll horizontally to see all columns"))
        })
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::text_muted(cx))
                        .child(format!(
                            "{} so far vs {} in all of last month",
                            fmt::amount(mom.current_total, currency),
                            fmt::amount(mom.previous_total, currency)
                        )),
                )
                .when_some(delta, |el, delta| el.child(delta)),
        )
        .child(
            div().w_full().overflow_x_scrollbar().child(
                div()
                    .min_w(rems(44.0))
                    .v_flex()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .pb_2()
                            .child(theme::header_cell(cx, "SERVICE").flex_1().min_w_0())
                            .child(theme::header_cell(cx, "THIS MONTH").w_24().text_right())
                            .child(theme::header_cell(cx, "LAST MONTH").w_24().text_right())
                            .child(theme::header_cell(cx, "CHANGE").w_40().text_right()),
                    )
                    .children(mom.services.iter().map(|row| {
                        let (delta_text, delta_color) = match row.change_pct {
                            Some(pct) if pct < 0.0 => (fmt::change_pct(pct), theme::text_muted(cx)),
                            Some(pct) => (fmt::change_pct(pct), theme::accent(cx)),
                            // No meaningful previous-month base to compare
                            // against.
                            None => ("—".to_string(), theme::text_muted(cx)),
                        };
                        div()
                            .h_flex()
                            .items_center()
                            .py_3()
                            .border_t_1()
                            .border_color(theme::card_border(cx))
                            .child(
                                div()
                                    .flex_1()
                                    // min_w_0 so a long service name truncates
                                    // instead of pushing the amount columns
                                    // out of the card.
                                    .min_w_0()
                                    .text_sm()
                                    .text_color(theme::text_primary(cx))
                                    .child(row.service.clone()),
                            )
                            .child(
                                div()
                                    .w_24()
                                    .text_right()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text_primary(cx))
                                    .child(fmt::amount(row.current, currency)),
                            )
                            .child(
                                div()
                                    .w_24()
                                    .text_right()
                                    .text_sm()
                                    .text_color(theme::text_muted(cx))
                                    .child(fmt::amount(row.previous, currency)),
                            )
                            .child(
                                div()
                                    .w_40()
                                    .text_right()
                                    .text_sm()
                                    .text_color(delta_color)
                                    .child(delta_text),
                            )
                    })),
            ),
        )
        // The "why it changed" decomposition; absent when its query
        // failed, the card then shows totals only.
        .when_some(mom.decomposition.as_ref(), |el, why| {
            el.child(render_why_it_changed(cx, currency, why))
        })
}

/// The "why it changed" section of the month-over-month card: the top
/// charge-category deltas and the top service movements with their
/// Appeared/Vanished/Grown/Shrunk badge, plus a warning when the
/// components do not add back up to the delta they claim to explain.
fn render_why_it_changed(
    cx: &App,
    currency: &str,
    why: &data::ChangeDecomposition,
) -> impl IntoElement {
    div()
        .v_flex()
        .gap_3()
        .pt_4()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(theme::section_title(cx, "Why it changed"))
        .child(
            div()
                .h_flex()
                .items_start()
                .gap_4()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .v_flex()
                        .gap_1()
                        .child(theme::header_cell(cx, "BY CATEGORY"))
                        .children(why.categories.iter().map(|row| {
                            div()
                                .h_flex()
                                .items_center()
                                .justify_between()
                                .py_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(theme::text_muted(cx))
                                        .child(row.category.clone()),
                                )
                                .child(delta_amount(cx, row.delta, currency))
                        })),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .v_flex()
                        .gap_1()
                        .child(theme::header_cell(cx, "BY SERVICE"))
                        .children(why.movements.iter().map(|row| {
                            let badge_fg = match row.badge {
                                data::MovementBadge::Appeared | data::MovementBadge::Grown => {
                                    theme::accent(cx)
                                }
                                data::MovementBadge::Vanished | data::MovementBadge::Shrunk => {
                                    theme::text_muted(cx)
                                }
                            };
                            div()
                                .h_flex()
                                .items_center()
                                .gap_2()
                                .py_1()
                                .child(
                                    div()
                                        .flex_1()
                                        // min_w_0 so a long service name
                                        // truncates instead of pushing the
                                        // badge and delta out.
                                        .min_w_0()
                                        .text_sm()
                                        .text_color(theme::text_primary(cx))
                                        .child(row.service.clone()),
                                )
                                .child(theme::pill(
                                    row.badge.label(),
                                    theme::sidebar_bg(cx),
                                    badge_fg,
                                ))
                                .child(delta_amount(cx, row.delta, currency))
                        })),
                ),
        )
        // Wealthfolio's data-quality-as-UI rule: a decomposition that
        // does not reconcile says so, with the unexplained amount.
        .when(!why.reconciled, |el| {
            el.child(
                div()
                    .text_xs()
                    .text_color(theme::warning_text(cx))
                    .child(format!(
                    "This breakdown leaves {} unexplained — the components don't fully reconcile.",
                    fmt::amount(why.residual.abs(), currency)
                )),
            )
        })
}

/// A signed money delta, colored like the page's change percents: rises
/// in the accent, drops muted.
fn delta_amount(cx: &App, delta: f64, currency: &str) -> Div {
    let color = if delta < 0.0 {
        theme::text_muted(cx)
    } else {
        theme::accent(cx)
    };
    let text = if delta >= 0.0 {
        format!("+{}", fmt::amount(delta, currency))
    } else {
        fmt::amount(delta, currency)
    };
    div()
        .text_sm()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(color)
        .child(text)
}

fn legend_solid(cx: &App, color: Hsla, label: &'static str) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_2()
        // Decorative 16×2 swatch at the default rem; rem-based so it
        // zooms with the base font.
        .child(div().w_4().h_0p5().rounded(cx.theme().radius).bg(color))
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
                .children((0..3).map(|_| div().w_1().h_0p5().rounded(cx.theme().radius).bg(color))),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_muted(cx))
                .child(label),
        )
}

fn legend_dotted(cx: &App, color: Hsla, label: &'static str) -> Div {
    div()
        .h_flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_0p5()
                // Decorative 2×2 dots at the default rem; rem-based so
                // they zoom with the base font.
                .children(
                    (0..4).map(|_| div().w_0p5().h_0p5().rounded(cx.theme().radius).bg(color)),
                ),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_muted(cx))
                .child(label),
        )
}

/// Where the flat benchmark value falls on the chart's y-axis (0.0 =
/// bottom, 1.0 = top), using the chart canvas's own padded range over the
/// same two series (actual + baseline).
fn benchmark_frac(d: &data::OverviewData, value: f64) -> f32 {
    let (min, max) = chart::padded_y_range(
        d.chart
            .actual
            .iter()
            .chain(d.chart.baseline.iter())
            .map(|point| point.amount),
    );
    (((value - min) / (max - min)) as f32).clamp(0.0, 1.0)
}

/// The typical-day benchmark line, layered absolutely over the chart
/// canvas: a dotted grey horizontal line at the benchmark's y, visually
/// distinct from the dashed olive 7-day baseline.
fn benchmark_overlay(cx: &App, frac: f32) -> impl IntoElement {
    let color = theme::grey(cx);
    canvas(
        move |bounds, _window, _cx| {
            let left: f32 = bounds.origin.x.into();
            let top: f32 = bounds.origin.y.into();
            let width: f32 = bounds.size.width.into();
            let height: f32 = bounds.size.height.into();
            (left, top + height * (1.0 - frac), left + width)
        },
        move |_bounds, (left, y, right), window, _cx| {
            // 1px stroke, 2px dash / 4px gap at the default rem.
            let rem = window.rem_size();
            let mut line =
                PathBuilder::stroke(rems(0.0625).to_pixels(rem)).dash_array(&[px(2.0), px(4.0)]);
            line.move_to(point(px(left), px(y)));
            line.line_to(point(px(right), px(y)));
            if let Ok(path) = line.build() {
                window.paint_path(path, color);
            }
        },
    )
    .absolute()
    .inset_0()
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
