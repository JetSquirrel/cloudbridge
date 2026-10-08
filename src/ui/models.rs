//! Models View — the token economics of the LLM providers in the ledger:
//! spend, token volume, blended unit price, and cache behavior per model,
//! over the selected range.

use chrono::Utc;
use gpui_kit::component::{button::*, skeleton::Skeleton, Sizable as _, StyledExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::data::Range;
use super::{chart, data, fmt, theme};
use crate::analytics;
use crate::model::{FindingSeverity, ModelTokenSummary, TokenFinding};
use crate::ui::theme::CardOutline as _;

/// Models View
pub struct ModelsView {
    /// Selected range in the header segmented control.
    range: Range,
    /// The loaded page data; `None` until the first load completes.
    data: Option<data::ModelsData>,
    /// A load is in flight.
    loading: bool,
    /// Last load failure, shown under the header when stale data stays on
    /// screen, or as the page when there is nothing to show.
    error: Option<String>,
    /// Bumped on every load; a completion stamped with an older generation
    /// is discarded so a slow first load cannot clobber a newer result.
    generation: u64,
    /// Chart hover state (point under the mouse + the canvas's per-frame
    /// geometry cells); drives the hover guide, dot, and tooltip.
    chart_hover: chart::ChartHover,
}

impl ModelsView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            range: Range::Mtd,
            data: None,
            loading: false,
            error: None,
            generation: 0,
            chart_hover: chart::ChartHover::new(),
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

    /// Reload the page's data. Called by the app shell when this page is
    /// navigated to; a no-op while a load is already in flight. Existing
    /// data stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.load(cx);
    }

    /// Load the page's data off the UI thread; the ledger queries are
    /// blocking.
    fn load(&mut self, cx: &mut Context<Self>) {
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
            // `load_models` never fails — each sub-query degrades on its
            // own and logs — so there is no error arm to land.
            let loaded = smol::unblock(move || data::load_models(range)).await;
            this.update(cx, |this, cx| {
                this.loading = false;
                // A range switch during the flight started a newer load;
                // its data wins over this stale result.
                if this.generation == generation {
                    this.data = Some(loaded);
                    this.error = None;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
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
                    .child(theme::page_title(cx, "Models"))
                    .when_some(self.data.as_ref(), |el, d| {
                        el.child(theme::caption(
                            cx,
                            format!(
                                "{}, reported in {}",
                                self.range.header_caption(Utc::now()),
                                d.currency
                            ),
                        ))
                    }),
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
                            // The raised chip carries the card border.
                            button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                        } else {
                            button
                        }
                    })),
            )
    }

    /// The four KPI cards: AI spend, token volume, the blended per-million
    /// price, and how much of the input the cache served.
    fn render_stats(&self, cx: &Context<Self>, d: &data::ModelsData) -> impl IntoElement {
        let currency = d.currency.as_str();
        let muted = |text: String| div().text_color(theme::text_muted(cx)).child(text);
        div()
            .w_full()
            .h_flex()
            .items_stretch()
            .gap_4()
            .child(theme::stat_card(
                cx,
                "AI SPEND",
                fmt::amount(d.total_cost, currency),
                muted(format!("{} models this range", d.models.len())),
            ))
            .child(theme::stat_card(
                cx,
                "TOKENS",
                fmt::quantity(d.total_tokens),
                muted("input + output + cache".to_string()),
            ))
            .child(theme::stat_card(
                cx,
                "BLENDED $/1M",
                fmt::unit_cost(d.blended, currency),
                muted("cost ÷ tokens, all models".to_string()),
            ))
            .child(theme::stat_card(
                cx,
                "CACHE SHARE",
                d.cache_share
                    .map(|share| format!("{share:.1}%"))
                    .unwrap_or_else(|| "—".to_string()),
                muted("of input tokens read from cache".to_string()),
            ))
    }

    /// One row per model: cost and its move against the previous window,
    /// the token split, the blended unit price, and a share-of-spend bar.
    fn render_models_card(&self, cx: &Context<Self>, d: &data::ModelsData) -> impl IntoElement {
        let currency = d.currency.as_str();
        let total = d.total_cost;
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(theme::section_title(cx, "Per model"))
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .pb_2()
                    .child(theme::header_cell(cx, "MODEL").flex_1().min_w_0())
                    .child(theme::header_cell(cx, "COST").w_24().text_right())
                    .child(theme::header_cell(cx, "Δ VS PREV").w_20().text_right())
                    .child(theme::header_cell(cx, "IN").w_20().text_right())
                    .child(theme::header_cell(cx, "OUT").w_20().text_right())
                    .child(theme::header_cell(cx, "CACHE").w_20().text_right())
                    .child(theme::header_cell(cx, "$/1M").w_20().text_right())
                    .child(theme::header_cell(cx, "SHARE").w_32().px_2()),
            )
            .child(
                div()
                    .v_flex()
                    .children(d.models.iter().map(|m| model_row(cx, m, total, currency))),
            )
    }

    /// The daily token chart with hover interactivity: the canvas
    /// publishes its bounds and point coordinates every frame; the wrapper
    /// maps the mouse position to the nearest point and the shared overlay
    /// draws the guide, dot, and tooltip on top.
    fn render_tokens_chart(
        &self,
        d: &data::ModelsData,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        // The chart's dashed-baseline slot is empty — token volume has no
        // trailing-mean series here.
        let baseline: Vec<data::ChartPoint> = Vec::new();
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(theme::section_title(cx, "Tokens per day"))
            .child(
                div()
                    .id("tokens-chart")
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
                        &d.daily_tokens,
                        &baseline,
                        // 260px at the default 16px rem.
                        rems(16.25),
                        self.chart_hover.points_cell(),
                        self.chart_hover.bounds_cell(),
                    ))
                    .when_some(
                        chart::hover_overlay_with(
                            cx,
                            &self.chart_hover,
                            &d.daily_tokens,
                            &fmt::quantity,
                            window.rem_size(),
                        ),
                        |el, overlay| el.children(overlay),
                    ),
            )
    }

    fn render_findings(&self, cx: &Context<Self>, findings: &[TokenFinding]) -> impl IntoElement {
        div()
            .w_full()
            .v_flex()
            .gap_4()
            .children(findings.iter().map(|finding| finding_card(cx, finding)))
    }

    fn render_empty_state(&self, cx: &Context<Self>) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_primary(cx))
                    .child("No model-provider data yet"),
            )
            .child(div().text_sm().text_color(theme::text_muted(cx)).child(
                "Token counts and per-model costs appear once the ledger has \
                 an LLM provider's usage. Import an OpenAI or Anthropic usage \
                 export from Accounts.",
            ))
            .child(
                div().pt_1().flex().child(
                    Button::new("models-open-accounts")
                        .label("Open Accounts")
                        .small()
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .on_click(|_, _, cx| {
                            crate::app::navigate_to(crate::app::CurrentView::Accounts, cx)
                        }),
                ),
            )
    }

    /// First-load placeholder shaped like the loaded page — the stat-card
    /// row, the per-model table, and the chart at its fixed height — so
    /// the landing content does not jump the layout.
    fn render_loading(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_6()
            .child(div().h_flex().gap_4().children((0..4).map(|_| {
                theme::card(cx)
                    .flex_1()
                    .min_w_0()
                    .p_5()
                    .v_flex()
                    .gap_2()
                    .child(Skeleton::new().w_20().h_3())
                    .child(Skeleton::new().w_24().h_6())
                    .child(Skeleton::new().w_32().h_3())
            })))
            .child(
                theme::card(cx)
                    .w_full()
                    .p_5()
                    .v_flex()
                    .gap_3()
                    .child(Skeleton::new().w_32().h_4())
                    .children((0..5).map(|_| Skeleton::new().w_full().h_4())),
            )
            .child(
                theme::card(cx)
                    .w_full()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(Skeleton::new().w_40().h_4())
                    .child(Skeleton::new().w_full().h(rems(16.25))),
            )
    }

    /// Compact banner shown above stale content when a background reload
    /// fails — the full-page error card is only for when there is nothing
    /// to show at all.
    fn render_error_banner(&self, cx: &Context<Self>, error: &str) -> impl IntoElement {
        div()
            .w_full()
            .p_3()
            .rounded_md()
            .bg(theme::danger_bg(cx))
            .text_sm()
            .text_color(theme::danger(cx))
            .child(error.to_string())
    }

    fn render_error(&self, cx: &Context<Self>, error: &str) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_primary(cx))
                    .child(error.to_string()),
            )
            .child(
                div().h_flex().child(
                    Button::new("retry-load")
                        .label("Retry")
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.load(cx);
                        })),
                ),
            )
    }
}

impl Render for ModelsView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = if let Some(d) = &self.data {
            let content: AnyElement = if d.models.is_empty() {
                self.render_empty_state(cx).into_any_element()
            } else {
                div()
                    .v_flex()
                    .gap_6()
                    .child(self.render_stats(cx, d))
                    .child(self.render_models_card(cx, d))
                    .child(self.render_tokens_chart(d, window, cx))
                    .child(self.render_findings(cx, &d.findings))
                    .into_any_element()
            };
            div()
                .v_flex()
                .gap_6()
                // A failed background reload keeps the last good data on
                // screen; the error rides above it as a banner.
                .when_some(self.error.clone(), |el, error| {
                    el.child(self.render_error_banner(cx, &error))
                })
                .child(content)
                .into_any_element()
        } else if let Some(error) = &self.error {
            self.render_error(cx, error).into_any_element()
        } else if self.loading {
            self.render_loading(cx).into_any_element()
        } else {
            self.render_empty_state(cx).into_any_element()
        };

        div()
            .id("models")
            .size_full()
            .v_flex()
            .gap_6()
            .p_8()
            .bg(theme::app_bg(cx))
            .overflow_y_scroll()
            .child(self.render_header(cx))
            .child(body)
    }
}

/// One per-model table row. A model the import priced per request (or not
/// by tokens at all) has `has_token_data == false` and reads as dashes in
/// the token columns rather than invented zeros.
fn model_row(cx: &App, m: &ModelTokenSummary, total: f64, currency: &str) -> Div {
    let share = if total > 0.0 {
        m.usage_cost / total
    } else {
        0.0
    };

    // A ratio on a sub-cent base is noise, the overview's dust rule.
    let delta: AnyElement = if m.previous_cost >= fmt::DUST_THRESHOLD {
        let pct = (m.usage_cost - m.previous_cost) / m.previous_cost * 100.0;
        // Cost up takes the accent — the movers table's attention color;
        // cost down takes the olive, the theme's positive.
        let color = if pct > 0.0 {
            theme::accent(cx)
        } else if pct < 0.0 {
            theme::olive(cx)
        } else {
            theme::text_muted(cx)
        };
        div().text_color(color).child(fmt::change_pct(pct))
    } else {
        div()
            .text_color(theme::text_muted(cx))
            .child("—".to_string())
    }
    .into_any_element();

    let token_cell = |value: f64| -> AnyElement {
        if m.has_token_data {
            div()
                .text_color(theme::text_primary(cx))
                .child(fmt::quantity(value))
        } else {
            div()
                .text_color(theme::text_muted(cx))
                .child("—".to_string())
        }
        .into_any_element()
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
                // min_w_0 so a long model name truncates instead of
                // pushing the number columns out of the card.
                .min_w_0()
                .v_flex()
                .child(
                    div()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_sm()
                        .text_color(theme::text_primary(cx))
                        .child(m.model.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_muted(cx))
                        .child(m.provider.clone()),
                ),
        )
        .child(
            div()
                .w_24()
                .text_right()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_primary(cx))
                .child(fmt::amount(m.usage_cost, currency)),
        )
        .child(div().w_20().text_right().text_sm().child(delta))
        .child(
            div()
                .w_20()
                .text_right()
                .text_sm()
                .child(token_cell(m.tokens_in)),
        )
        .child(
            div()
                .w_20()
                .text_right()
                .text_sm()
                .child(token_cell(m.tokens_out)),
        )
        .child(
            div()
                .w_20()
                .text_right()
                .text_sm()
                .child(token_cell(m.tokens_cache)),
        )
        .child(
            div()
                .w_20()
                .text_right()
                .text_sm()
                .text_color(theme::text_primary(cx))
                .child(fmt::unit_cost(
                    analytics::blended_unit_cost(
                        m.usage_cost,
                        m.tokens_in + m.tokens_out + m.tokens_cache,
                    ),
                    currency,
                )),
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

/// One finding from the token analytics: a severity-colored dot ahead of
/// the title, the detail muted under it.
fn finding_card(cx: &App, finding: &TokenFinding) -> Div {
    let color = match finding.severity {
        FindingSeverity::Warning => theme::warning_text(cx),
        FindingSeverity::Notice => theme::accent(cx),
        FindingSeverity::Info => theme::grey(cx),
    };
    theme::card(cx)
        .w_full()
        .p_5()
        .v_flex()
        .gap_1()
        .child(
            div()
                .h_flex()
                .items_center()
                .gap_2()
                .child(theme::dot(color))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_primary(cx))
                        .child(finding.title.clone()),
                ),
        )
        .child(
            div()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child(finding.detail.clone()),
        )
}
