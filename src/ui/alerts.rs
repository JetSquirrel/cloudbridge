//! Alerts View — open alerts evaluated locally against the ledger after each ingest.

use gpui_kit::component::{button::*, skeleton::Skeleton, Disableable, Icon, IconName, StyledExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{data, theme};
use crate::alerts::{AlertField, AlertKind, AlertStat, AlertView, Severity};
use crate::ui::theme::CardOutline as _;

fn kind_icon(kind: AlertKind) -> Icon {
    match kind {
        AlertKind::CostAnomaly => Icon::new(IconName::ChartPie),
        AlertKind::Balance => Icon::new(IconName::TriangleAlert),
        AlertKind::UntaggedRatio => Icon::new(IconName::Info),
        // The component icon subset has no money icon; the assets crate's
        // full catalog does, and Icon accepts any IconNamed.
        AlertKind::Budget => Icon::new(gpui_kit::assets::IconName::Wallet),
    }
}

/// An unselected filter chip: a bordered card-colored pill. The selected
/// chip is a primary Button.
fn inactive_chip_variant(cx: &App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(theme::card_bg(cx))
        .foreground(theme::text_muted(cx))
        .hover(theme::sidebar_bg(cx))
        .active(theme::surface_pressed(cx))
}

/// The tinted circle at the left of an alert card.
fn icon_badge(alert: &AlertView, cx: &App) -> Div {
    let (bg, fg) = match alert.severity {
        Severity::Critical => (theme::alert_tint(cx), theme::accent(cx)),
        Severity::Warning => (theme::warning_bg(cx), theme::warning_text(cx)),
    };

    div()
        .flex_shrink_0()
        .size_12()
        .rounded_full()
        .bg(bg)
        .flex()
        .items_center()
        .justify_center()
        .child(kind_icon(alert.kind).size_5().text_color(fg))
}

/// The severity marker next to an alert title.
fn severity_badge(severity: Severity, cx: &App) -> Div {
    match severity {
        Severity::Critical => theme::pill("Critical", theme::alert_tint(cx), theme::danger(cx)),
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
    /// Label of the selected filter chip — stable across reloads, unlike
    /// an index into `filters`, which silently re-targets when the
    /// backend reorders or drops a chip.
    selected_filter: String,
    /// The loaded page data, once the first load lands.
    data: Option<data::AlertsData>,
    /// A load is in flight (initial or a reload after an action).
    loading: bool,
    /// The last load or action failure, if any.
    error: Option<String>,
}

impl AlertsView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            selected_filter: "All".to_string(),
            data: None,
            loading: false,
            error: None,
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

    fn select_filter(&mut self, label: String, cx: &mut Context<Self>) {
        self.selected_filter = label;
        cx.notify();
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

    /// Load (or reload) the page's data off the UI thread.
    ///
    /// `load_alerts` also auto-resolves events whose condition stopped
    /// holding, so a reload after Snooze or Dismiss returns the truth.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();
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
            });
        })
        .detach();
    }

    /// Run a blocking alert mutation off the UI thread, then reload.
    ///
    /// Only one action (or load) runs at a time: a second click while one
    /// is in flight is dropped here, and the buttons are disabled for the
    /// duration, so two rapid Snooze clicks can't race the ledger.
    fn run_action(
        &mut self,
        cx: &mut Context<Self>,
        action: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        verb: &'static str,
    ) {
        if self.loading {
            return;
        }
        self.loading = true;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(action).await;
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if let Err(e) = result {
                        this.error = Some(format!("Could not {verb} the alert: {e}"));
                    }
                    this.load(cx);
                })
                .ok();
            });
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
        filter: &data::AlertFilterData,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let active = filter.label == self.selected_filter;
        let label = filter.label.clone();

        Button::new(SharedString::from(format!("alert-filter-{label}")))
            .label(filter.label.clone())
            .child(div().text_xs().opacity(0.7).child(filter.count.to_string()))
            .when(active, |chip| chip.primary())
            .when(!active, |chip| chip.custom(inactive_chip_variant(cx)))
            // Only the unselected chip is outlined; the selected one is
            // solid accent.
            .when(!active, |chip| chip.card_outline(cx))
            .rounded_full()
            .h_auto()
            .px_3()
            .py_1()
            .text_sm()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.select_filter(label.clone(), cx);
            }))
    }

    /// One action of an alert card. Chrome follows the action's semantics:
    /// Dismiss is always a text button, the first non-Dismiss action is the
    /// primary, and any remaining ones are outlined. Behaviour follows the
    /// label the backend emitted — only the lifecycle actions do anything
    /// yet. All actions are disabled while an action or reload is in flight
    /// so rapid clicks can't race.
    fn render_action(
        &self,
        primary: bool,
        alert_id: &str,
        action: &str,
        cx: &Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("alert-{alert_id}-action-{action}"));
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

        if action == "Dismiss" {
            Button::new(id)
                .label(action.to_string())
                .text()
                .text_sm()
                .text_color(theme::accent(cx))
                .disabled(self.loading)
                .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
                .into_any_element()
        } else if primary {
            Button::new(id)
                .label(action.to_string())
                .primary()
                .disabled(self.loading)
                .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
                .into_any_element()
        } else {
            Button::new(id)
                .label(action.to_string())
                .custom(theme::outline_variant(cx))
                .card_outline(cx)
                .disabled(self.loading)
                .on_click(cx.listener(move |this, _, _, cx| on_click(this, cx)))
                .into_any_element()
        }
    }

    fn render_alert(&self, alert: &AlertView, cx: &Context<Self>) -> impl IntoElement {
        let primary_action = alert.actions.iter().position(|action| action != "Dismiss");
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
                                    .text_base()
                                    .font_weight(FontWeight::SEMIBOLD)
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
                        .items_start()
                        .gap_6()
                        .children(alert.fields.iter().map(|field| field_column(field, cx))),
                )
            })
            .child(divider(cx))
            .child(div().h_flex().items_center().gap_3().children(
                alert.actions.iter().enumerate().map(|(i, action)| {
                    self.render_action(Some(i) == primary_action, &alert.id, action, cx)
                }),
            ))
    }

    /// One row of the resolved list: the alert's title and when it resolved.
    fn render_resolved_row(&self, alert: &AlertView, cx: &Context<Self>) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("resolved-{}", alert.id)))
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
                    .child(
                        alert
                            .resolved_at
                            .unwrap_or(alert.created_at)
                            .format("%b %d")
                            .to_string(),
                    ),
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
                    .map(|alert| self.render_resolved_row(alert, cx)),
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
            // The rules editor is a desktop page: a rule cannot be tuned in a
            // tab that forgets the change when it reloads, and the browser
            // demo has no way to evaluate one against anything but its own
            // seeded ledger.
            .when(cfg!(not(target_family = "wasm")), |el| {
                el.child(
                    Button::new("edit-rules")
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .label("Edit rules")
                        .on_click(|_, _, cx| {
                            crate::app::navigate_to(crate::app::CurrentView::Rules, cx)
                        }),
                )
            });

        let Some(data) = self.data.as_ref() else {
            let body: AnyElement = if self.loading {
                // Skeleton shaped like the loaded list: alert cards at
                // their final padding, so the first load does not jump.
                div()
                    .v_flex()
                    .gap_4()
                    .children((0..3).map(|_| {
                        theme::card(cx)
                            .w_full()
                            .p_5()
                            .v_flex()
                            .gap_3()
                            .child(
                                div()
                                    .h_flex()
                                    .gap_4()
                                    .items_center()
                                    .child(Skeleton::new().size_12().rounded_full().flex_shrink_0())
                                    .child(
                                        div()
                                            .flex_1()
                                            .v_flex()
                                            .gap_2()
                                            .child(Skeleton::new().w_48().h_4())
                                            .child(Skeleton::new().w_full().h_3()),
                                    ),
                            )
                            .child(Skeleton::new().w_full().h_3())
                    }))
                    .into_any_element()
            } else if let Some(error) = self.error.clone() {
                theme::card(cx)
                    .w_full()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(div().text_sm().text_color(theme::danger(cx)).child(error))
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
                    .into_any_element()
            } else {
                // The first load has not been kicked off yet; render
                // nothing rather than an empty text node.
                div().into_any_element()
            };

            return div()
                .size_full()
                .v_flex()
                .gap_6()
                .p_8()
                .bg(theme::app_bg(cx))
                .child(header)
                .child(body);
        };

        let filters = &data.filters;
        // Keep the selection across reloads; fall back to the first chip
        // ("All") when the selected filter no longer exists.
        let selected_label = filters
            .iter()
            .find(|filter| filter.label == self.selected_filter)
            .or_else(|| filters.first())
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
                .map(|filter| self.render_filter_chip(filter, cx)),
        );

        let error_banner = self.error.clone().map(|error| {
            theme::card(cx)
                .p_3()
                .child(div().text_sm().text_color(theme::danger(cx)).child(error))
        });

        let list = div()
            .id("alerts-list")
            .flex_1()
            .min_h_0()
            .v_flex()
            .gap_4()
            .overflow_y_scroll()
            .when(alerts.is_empty(), |el| {
                // Slim banner, not a hero card: an empty list page should
                // not burn vertical space announcing that it is empty.
                el.child(
                    theme::card(cx).p_3().child(
                        div()
                            .text_sm()
                            .text_color(theme::text_muted(cx))
                            .child("All clear"),
                    ),
                )
            })
            .children(alerts.iter().map(|alert| self.render_alert(alert, cx)))
            .child(self.render_resolved_section(cx));

        div()
            .size_full()
            .v_flex()
            .gap_6()
            .p_8()
            .bg(theme::app_bg(cx))
            .child(header)
            .child(chips)
            .when_some(error_banner, |el, banner| el.child(banner))
            .child(list)
    }
}
