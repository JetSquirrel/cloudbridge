//! Alerts View — open alerts evaluated locally against the ledger after each ingest.

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{button::*, Icon, IconName, StyledExt};

use super::{data, theme};
use crate::alerts::{AlertField, AlertKind, AlertStat, AlertView, Severity};

fn kind_icon(kind: AlertKind) -> IconName {
    match kind {
        AlertKind::CostAnomaly => IconName::ChartPie,
        AlertKind::Balance => IconName::TriangleAlert,
        AlertKind::UntaggedRatio => IconName::Info,
    }
}

/// Terracotta primary button in the warm palette (matches overview.rs / accounts.rs).
fn primary_variant(cx: &App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(theme::accent(cx))
        .foreground(theme::on_accent(cx))
        .hover(theme::accent_hover(cx))
        .active(theme::accent_hover(cx))
}

/// Outline-style button in the warm palette.
fn outline_variant(cx: &App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(theme::card_bg(cx))
        .foreground(theme::text_primary(cx))
        .border(theme::card_border(cx))
        .hover(theme::sidebar_bg(cx))
        .active(theme::sidebar_bg(cx))
}

/// The tinted circle at the left of an alert card.
fn icon_badge(alert: &AlertView, cx: &App) -> Div {
    let (bg, fg) = match alert.severity {
        Severity::Critical => (theme::alert_tint(cx), theme::accent(cx)),
        Severity::Warning => (theme::warning_bg(cx), theme::warning_text(cx)),
    };

    div()
        .flex_shrink_0()
        .size(px(48.0))
        .rounded_full()
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .child(
            Icon::new(kind_icon(alert.kind))
                .size(px(20.0))
                .text_color(fg),
        )
}

/// The severity marker next to an alert title.
fn severity_badge(severity: Severity, cx: &App) -> Div {
    match severity {
        Severity::Critical => div()
            .text_xs()
            .font_weight(FontWeight::BOLD)
            .text_color(theme::accent(cx))
            .child("Critical"),
        Severity::Warning => theme::pill("Warning", theme::warning_bg(cx), theme::warning_text(cx)),
    }
}

/// The right-side highlight stat on an alert card.
fn stat_block(stat: &AlertStat, cx: &App) -> Div {
    div()
        .flex_shrink_0()
        .v_flex()
        .items_end()
        .gap_1()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::BOLD)
                .text_color(theme::accent(cx))
                .child(stat.value.clone()),
        )
        .child(
            div()
                .text_xs()
                .text_color(theme::text_muted(cx))
                .child(stat.label.clone()),
        )
}

/// One label/value column of the fields row.
fn field_column(field: &AlertField, cx: &App) -> Div {
    div()
        .flex_1()
        .v_flex()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(theme::text_muted(cx))
                .child(field.label.to_uppercase()),
        )
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_primary(cx))
                .child(field.value.clone()),
        )
}

fn divider(cx: &App) -> Div {
    div().h(px(1.0)).w_full().bg(theme::card_border(cx))
}

/// Alerts View
pub struct AlertsView {
    /// Index into the loaded `filters`; 0 is "All".
    selected_filter: usize,
    /// The loaded page data, once the first load lands.
    data: Option<data::AlertsData>,
    /// A load is in flight (initial or a reload after an action).
    loading: bool,
    /// The last load or action failure, if any.
    error: Option<String>,
}

impl AlertsView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = Self {
            selected_filter: 0,
            data: None,
            loading: false,
            error: None,
        };
        view.load(cx);
        view
    }

    fn select_filter(&mut self, index: usize, cx: &mut Context<Self>) {
        self.selected_filter = index;
        cx.notify();
    }

    /// Reload the page data. Called by the app shell when this page is
    /// navigated to; a no-op while a load is already in flight. Existing
    /// data stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        self.load(cx);
    }

    /// Load (or reload) the page's data off the UI thread.
    ///
    /// `load_alerts` also auto-resolves events whose condition stopped
    /// holding, so a reload after Snooze or Dismiss returns the truth.
    fn load(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(data::load_alerts).await;
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.loading = false;
                    match result {
                        Ok(data) => {
                            this.data = Some(data);
                            this.error = None;
                        }
                        Err(e) => {
                            this.error = Some(format!("Could not load alerts: {e}"));
                        }
                    }
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    /// Run a blocking alert mutation off the UI thread, then reload.
    fn run_action(
        &mut self,
        cx: &mut Context<Self>,
        action: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        verb: &'static str,
    ) {
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(action).await;
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if let Err(e) = result {
                        this.error = Some(format!("Could not {verb} the alert: {e}"));
                    }
                    this.loading = true;
                    cx.notify();
                    this.load(cx);
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }

    fn snooze_alert(&mut self, id: String, cx: &mut Context<Self>) {
        self.run_action(cx, move || data::snooze_alert(&id, 24), "snooze");
    }

    fn dismiss_alert(&mut self, id: String, cx: &mut Context<Self>) {
        self.run_action(cx, move || data::dismiss_alert(&id), "dismiss");
    }

    fn render_filter_chip(
        &self,
        index: usize,
        filter: &data::AlertFilterData,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = index == self.selected_filter;

        div()
            .id(SharedString::from(format!("alert-filter-{index}")))
            .px_3()
            .py_1()
            .rounded_full()
            .cursor_pointer()
            .text_sm()
            .when(active, |el| {
                el.bg(theme::text_primary(cx))
                    .text_color(theme::on_accent(cx))
            })
            .when(!active, |el| {
                el.bg(theme::card_bg(cx))
                    .text_color(theme::text_muted(cx))
                    .border_1()
                    .border_color(theme::card_border(cx))
            })
            .child(format!("{} {}", filter.label, filter.count))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_filter(index, cx);
            }))
    }

    /// One action of an alert card. Chrome follows position (first is the
    /// primary); behaviour follows the label the backend emitted — only the
    /// lifecycle actions do anything yet.
    fn render_action(
        &self,
        index: usize,
        action_index: usize,
        alert_id: &str,
        action: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("alert-{index}-action-{action_index}"));
        let alert_id = alert_id.to_string();
        let action_owned = action.to_string();

        let on_click = move |this: &mut Self, cx: &mut Context<Self>| match action_owned.as_str() {
            "Snooze 24h" => this.snooze_alert(alert_id.clone(), cx),
            "Dismiss" => this.dismiss_alert(alert_id.clone(), cx),
            "Trace in attribution" => {
                crate::app::navigate_to(crate::app::CurrentView::Attribution, cx)
            }
            _ => {}
        };

        match action_index {
            0 => Button::new(id)
                .label(action.to_string())
                .custom(primary_variant(cx))
                .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
                .into_any_element(),
            1 => Button::new(id)
                .label(action.to_string())
                .custom(outline_variant(cx))
                .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
                .into_any_element(),
            _ => div()
                .id(id)
                .cursor_pointer()
                .text_sm()
                .text_color(theme::accent(cx))
                .child(action.to_string())
                .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
                .into_any_element(),
        }
    }

    fn render_alert(
        &self,
        index: usize,
        alert: &AlertView,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let top_row = div()
            .h_flex()
            .gap_4()
            .items_center()
            .child(icon_badge(alert, cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap_2()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(theme::text_primary(cx))
                                    .child(alert.title.clone()),
                            )
                            .child(severity_badge(alert.severity, cx)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::text_muted(cx))
                            .line_clamp(3)
                            .child(alert.body.clone()),
                    ),
            )
            .when_some(alert.stat.as_ref(), |el, stat| {
                el.child(stat_block(stat, cx))
            });

        theme::card(cx)
            .p_5()
            .v_flex()
            .gap_4()
            .child(top_row)
            .when(!alert.fields.is_empty(), |el| {
                el.child(divider(cx)).child(
                    div()
                        .h_flex()
                        .gap_6()
                        .children(alert.fields.iter().map(|field| field_column(field, cx))),
                )
            })
            .child(divider(cx))
            .child(
                div().h_flex().items_center().gap_3().children(
                    alert
                        .actions
                        .iter()
                        .enumerate()
                        .map(|(i, action)| self.render_action(index, i, &alert.id, action, cx)),
                ),
            )
    }

    /// One row of the resolved list: the alert's title and when it fired.
    fn render_resolved_row(
        &self,
        index: usize,
        alert: &AlertView,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("resolved-{index}")))
            .h_flex()
            .justify_between()
            .items_center()
            .gap_4()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .text_color(theme::text_primary(cx))
                    .child(alert.title.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(theme::text_muted(cx))
                    .child(alert.created_at.format("%b %d").to_string()),
            )
    }

    fn render_resolved_section(&self, cx: &Context<Self>) -> impl IntoElement {
        let resolved: &[AlertView] = self
            .data
            .as_ref()
            .map(|data| data.resolved_this_month.as_slice())
            .unwrap_or(&[]);

        div()
            .pt_2()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_muted(cx))
                    .child(data::RESOLVED_SECTION_TITLE),
            )
            .when(resolved.is_empty(), |el| {
                el.child(theme::caption(cx, "Nothing resolved yet."))
            })
            .children(
                resolved
                    .iter()
                    .enumerate()
                    .map(|(index, alert)| self.render_resolved_row(index, alert, cx)),
            )
    }
}

impl Render for AlertsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, "Alerts"))
                    .child(theme::caption(
                        cx,
                        "Evaluated locally against the ledger every ingest",
                    )),
            )
            .child(
                Button::new("edit-rules")
                    .custom(outline_variant(cx))
                    .label("Edit rules")
                    .on_click(|_, _, cx| {
                        crate::app::navigate_to(crate::app::CurrentView::Rules, cx)
                    }),
            );

        let Some(data) = self.data.as_ref() else {
            return div()
                .size_full()
                .v_flex()
                .gap_6()
                .p(px(32.0))
                .bg(theme::app_bg(cx))
                .child(header)
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::text_muted(cx))
                        .child(if self.loading {
                            "Loading alerts…".to_string()
                        } else {
                            self.error.clone().unwrap_or_default()
                        }),
                );
        };

        let filters = &data.filters;
        let selected_label = filters
            .get(self.selected_filter)
            .map(|filter| filter.label.as_str())
            .unwrap_or("All");

        let alerts: Vec<&AlertView> = data
            .open
            .iter()
            .filter(|alert| selected_label == "All" || alert.kind.label() == selected_label)
            .collect();

        let chips = div().h_flex().gap_2().children(
            filters
                .iter()
                .enumerate()
                .map(|(index, filter)| self.render_filter_chip(index, filter, cx)),
        );

        let list = div()
            .id("alerts-list")
            .flex_1()
            .v_flex()
            .gap_4()
            .overflow_y_scroll()
            .when_some(self.error.clone(), |el, error| {
                el.child(div().text_sm().text_color(theme::accent(cx)).child(error))
            })
            .when(alerts.is_empty(), |el| {
                el.child(
                    theme::card(cx).p_5().child(
                        div()
                            .text_sm()
                            .text_color(theme::text_muted(cx))
                            .child("All clear — rules evaluate after every ingest."),
                    ),
                )
            })
            .children(
                alerts
                    .iter()
                    .enumerate()
                    .map(|(index, alert)| self.render_alert(index, alert, cx)),
            )
            .child(self.render_resolved_section(cx));

        div()
            .size_full()
            .v_flex()
            .gap_6()
            .p(px(32.0))
            .bg(theme::app_bg(cx))
            .child(header)
            .child(chips)
            .child(list)
    }
}
