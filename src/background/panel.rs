//! The panel under the menu bar icon.
//!
//! It answers one question — is my spend running away? — so it shows, in
//! order: this month's spend and where it is heading, the open alerts, and
//! the services that moved most against last month. Everything else is one
//! click away in the window. It closes when it loses focus or on Escape,
//! like any menu bar panel.

use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::summary::{Summary, LISTED_ALERTS};
use super::{GlobalStatus, Status};
use crate::alerts::Severity;
use crate::app::CurrentView;
use crate::ui::{fmt, theme};

actions!(menu_bar_panel, [ClosePanel]);

const CONTEXT: &str = "MenuBarPanel";
const WIDTH: f32 = 340.;
const HEIGHT: f32 = 460.;
/// Space between the panel and the menu bar, and the screen edge.
const GAP: f32 = 6.;

/// Where the panel opens, in points from the top-left of the primary
/// display.
#[derive(Debug)]
pub(super) enum Anchor {
    /// Hanging under a menu bar icon, kept on the icon's screen.
    Below {
        icon: Bounds<Pixels>,
        screen: Bounds<Pixels>,
    },
    /// In the corner of a work area, above the taskbar.
    Corner { work_area: Bounds<Pixels> },
}

/// The panel's top-left corner for an anchor.
fn origin(anchor: &Anchor, panel: gpui_kit::Size<Pixels>) -> Point<Pixels> {
    match anchor {
        Anchor::Below { icon, screen } => {
            let centred = icon.origin.x + icon.size.width / 2. - panel.width / 2.;
            let left = screen.origin.x + px(GAP);
            let right = screen.origin.x + screen.size.width - panel.width - px(GAP);
            point(
                centred.max(left).min(right),
                icon.origin.y + icon.size.height + px(GAP),
            )
        }
        Anchor::Corner { work_area } => point(
            work_area.origin.x + work_area.size.width - panel.width - px(GAP),
            work_area.origin.y + work_area.size.height - panel.height - px(GAP),
        ),
    }
}

pub(super) fn open(anchor: Anchor, cx: &mut App) -> anyhow::Result<WindowHandle<Root>> {
    let status = cx.global::<GlobalStatus>().0.clone();
    let panel_size = size(px(WIDTH), px(HEIGHT));
    cx.bind_keys([KeyBinding::new("escape", ClosePanel, Some(CONTEXT))]);
    let handle = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                origin(&anchor, panel_size),
                panel_size,
            ))),
            titlebar: None,
            kind: WindowKind::PopUp,
            focus: true,
            show: true,
            is_movable: false,
            is_resizable: false,
            is_minimizable: false,
            window_background: if cfg!(target_os = "macos") {
                WindowBackgroundAppearance::Transparent
            } else {
                WindowBackgroundAppearance::Opaque
            },
            ..Default::default()
        },
        |window, cx| {
            let panel = cx.new(|cx| MenuBarPanel::new(status, window, cx));
            cx.new(|cx| Root::new(panel, window, cx))
        },
    )?;
    // Take focus from whatever app had it, so a click anywhere else is a
    // focus change the panel can close on.
    cx.activate(true);
    Ok(handle)
}

pub(super) struct MenuBarPanel {
    status: Entity<Status>,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl MenuBarPanel {
    fn new(status: Entity<Status>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let observe = cx.observe(&status, |_, _, cx| cx.notify());
        let deactivated = cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active() {
                Self::dismiss(window, cx);
            }
        });
        Self {
            status,
            focus_handle,
            _subscriptions: vec![observe, deactivated],
        }
    }

    /// Close this panel from inside it, and forget it was open.
    fn dismiss(window: &mut Window, cx: &mut App) {
        tracing::debug!("Closing the menu bar panel");
        window.remove_window();
        cx.defer(|cx| {
            if cx.has_global::<super::tray::MenuBar>() {
                cx.global_mut::<super::tray::MenuBar>().panel = None;
            }
        });
    }

    /// Close the panel and bring the window up on `view`.
    fn open_window_on(view: Option<CurrentView>, window: &mut Window, cx: &mut App) {
        Self::dismiss(window, cx);
        cx.defer(move |cx| crate::desktop::show_main_window(view, cx));
    }

    fn render_header(&self, summary: &Summary, cx: &App) -> impl IntoElement {
        let now = chrono::Utc::now();
        div()
            .p_4()
            .v_flex()
            .gap_1()
            .child(theme::caption(
                cx,
                format!("{}, month to date · {}", now.format("%B"), summary.currency),
            ))
            .child(
                div()
                    .text_3xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_primary(cx))
                    .child(fmt::amount(summary.month_to_date, &summary.currency)),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .text_sm()
                    .child(div().text_color(theme::text_muted(cx)).child(format!(
                        "Forecast {} by month end",
                        fmt::amount(summary.forecast, &summary.currency)
                    )))
                    .when_some(summary.change_pct, |el, pct| {
                        el.child(
                            div()
                                .text_color(theme::accent(cx))
                                .child(format!("{} vs last month", fmt::change_pct(pct))),
                        )
                    }),
            )
    }

    fn render_alerts(&self, summary: &Summary, cx: &App) -> impl IntoElement {
        let count = summary.alerts.len();
        let heading = if count == 0 {
            div()
                .h_flex()
                .gap_2()
                .items_center()
                .text_color(theme::text_muted(cx))
                .child(Icon::new(IconName::CircleCheck).small())
                .child("No open alerts")
        } else {
            let color = if summary.has_critical() {
                theme::danger(cx)
            } else {
                theme::warning_text(cx)
            };
            div()
                .h_flex()
                .gap_2()
                .items_center()
                .text_color(color)
                .font_weight(FontWeight::SEMIBOLD)
                .child(Icon::new(IconName::TriangleAlert).small())
                .child(if count == 1 {
                    "1 open alert".to_string()
                } else {
                    format!("{count} open alerts")
                })
        };

        div()
            .px_4()
            .py_3()
            .v_flex()
            .gap_1()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .child(heading)
            .children(summary.alerts.iter().take(LISTED_ALERTS).map(|alert| {
                let color = match alert.severity {
                    Severity::Critical => theme::danger(cx),
                    Severity::Warning => theme::warning_text(cx),
                };
                div()
                    .id(SharedString::from(format!("panel-alert-{}", alert.id)))
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::sidebar_bg(cx)))
                    .child(theme::dot(color).flex_shrink_0())
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_sm()
                            .child(alert.title.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_xs()
                            .text_color(theme::text_muted(cx))
                            .child(fmt::relative_time(alert.created_at)),
                    )
                    .on_click(|_, window, cx| {
                        Self::open_window_on(Some(CurrentView::Alerts), window, cx)
                    })
            }))
            .when(count > LISTED_ALERTS, |el| {
                el.child(
                    div()
                        .px_2()
                        .text_xs()
                        .text_color(theme::text_muted(cx))
                        .child(format!("and {} more", count - LISTED_ALERTS)),
                )
            })
    }

    fn render_movers(&self, summary: &Summary, cx: &App) -> impl IntoElement {
        div()
            .px_4()
            .py_3()
            .v_flex()
            .gap_2()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .child(theme::section_title(cx, "Biggest movers this month"))
            .when(summary.movers.is_empty(), |el| {
                el.child(theme::caption(cx, "Nothing billed yet this month."))
            })
            .children(summary.movers.iter().map(|mover| {
                let (delta, delta_color) = match mover.change_pct {
                    Some(pct) if pct < 0.0 => (fmt::change_pct(pct), theme::text_muted(cx)),
                    Some(pct) => (fmt::change_pct(pct), theme::accent(cx)),
                    None => ("new".to_string(), theme::text_muted(cx)),
                };
                div()
                    .h_flex()
                    .gap_2()
                    .items_center()
                    .text_sm()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(mover.service.clone())
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted(cx))
                                    .child(mover.provider.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_right()
                            .child(fmt::amount(mover.amount, &summary.currency)),
                    )
                    .child(
                        div()
                            .w_16()
                            .flex_shrink_0()
                            .text_right()
                            .text_xs()
                            .text_color(delta_color)
                            .child(delta),
                    )
            }))
    }

    fn render_footer(&self, status: &Status, cx: &App) -> impl IntoElement {
        let line = if status.refreshing {
            "Refreshing…".to_string()
        } else if !status.failures.is_empty() {
            match status.failures.len() {
                1 => "1 account failed to refresh".to_string(),
                n => format!("{n} accounts failed to refresh"),
            }
        } else {
            match status.summary.as_ref().and_then(|s| s.last_synced_at) {
                Some(at) => format!("Synced {}", fmt::relative_time(at)),
                None => "Not synced yet".to_string(),
            }
        };
        let line_color = if !status.refreshing && !status.failures.is_empty() {
            theme::danger(cx)
        } else {
            theme::text_muted(cx)
        };

        div()
            .px_3()
            .py_2()
            .h_flex()
            .gap_2()
            .items_center()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .child(
                Button::new("panel-quit")
                    .label("Quit")
                    .ghost()
                    .xsmall()
                    .on_click(|_, _, cx| cx.quit()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_xs()
                    .text_color(line_color)
                    .child(line),
            )
            .child(
                Button::new("panel-refresh")
                    .icon(IconName::RotateCw)
                    .ghost()
                    .small()
                    .loading(status.refreshing)
                    .disabled(status.refreshing)
                    .tooltip("Fetch accounts that are due")
                    .on_click(|_, _, cx| super::refresh_now(cx)),
            )
            .child(
                Button::new("panel-open")
                    .label("Open CloudBridge")
                    .primary()
                    .small()
                    .on_click(|_, window, cx| Self::open_window_on(None, window, cx)),
            )
    }
}

impl Focusable for MenuBarPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for MenuBarPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let status = self.status.read(cx);

        let body: AnyElement = match &status.summary {
            None => div()
                .flex_1()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(theme::text_muted(cx))
                .child(Icon::new(IconName::LoaderCircle))
                .child("Reading the ledger…")
                .into_any_element(),
            Some(summary) if summary.real_accounts == 0 => div()
                .flex_1()
                .p_6()
                .v_flex()
                .items_center()
                .justify_center()
                .gap_3()
                .child(theme::section_title(cx, "No accounts yet"))
                .child(
                    div()
                        .text_center()
                        .text_sm()
                        .text_color(theme::text_muted(cx))
                        .child(
                            "Add a cloud account and CloudBridge watches its spend \
                             from here, window open or not.",
                        ),
                )
                .child(
                    Button::new("panel-add-account")
                        .label("Add an account")
                        .primary()
                        .small()
                        .on_click(|_, window, cx| {
                            Self::open_window_on(Some(CurrentView::Accounts), window, cx);
                            cx.defer(crate::app::open_add_account);
                        }),
                )
                .into_any_element(),
            Some(summary) => div()
                .flex_1()
                .min_h_0()
                .v_flex()
                .child(self.render_header(summary, cx))
                .child(self.render_alerts(summary, cx))
                .child(self.render_movers(summary, cx))
                .into_any_element(),
        };

        div()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &ClosePanel, window, cx| {
                Self::dismiss(window, cx);
            }))
            .size_full()
            .v_flex()
            .overflow_hidden()
            .bg(theme::card_bg(cx))
            .border_1()
            .border_color(theme::card_border(cx))
            .rounded_lg()
            .text_color(theme::text_primary(cx))
            .child(body)
            .child(self.render_footer(status, cx))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings GPUI's `test` attribute in over the
    // standard one.
    use super::{origin, Anchor, GAP, HEIGHT, WIDTH};
    use gpui_kit::{point, px, size, Bounds, Pixels};

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    fn panel() -> gpui_kit::Size<Pixels> {
        size(px(WIDTH), px(HEIGHT))
    }

    #[test]
    fn the_panel_hangs_centred_under_its_icon() {
        let at = origin(
            &Anchor::Below {
                icon: bounds(1000., 0., 40., 24.),
                screen: bounds(0., 0., 1728., 1117.),
            },
            panel(),
        );
        assert_eq!(at, point(px(1020. - WIDTH / 2.), px(24. + GAP)));
    }

    #[test]
    fn an_icon_at_the_screen_edge_keeps_the_panel_on_screen() {
        let at = origin(
            &Anchor::Below {
                icon: bounds(1700., 0., 24., 24.),
                screen: bounds(0., 0., 1728., 1117.),
            },
            panel(),
        );
        assert_eq!(at.x, px(1728. - WIDTH - GAP));
    }

    /// A second display to the right of the primary one starts at its own
    /// x; the panel stays on that display, not the primary.
    #[test]
    fn an_icon_on_another_display_keeps_the_panel_on_that_display() {
        let at = origin(
            &Anchor::Below {
                icon: bounds(1730., 0., 24., 24.),
                screen: bounds(1728., 0., 2560., 1440.),
            },
            panel(),
        );
        assert_eq!(at.x, px(1728. + GAP));
    }

    #[test]
    fn without_an_icon_the_panel_takes_the_work_area_corner() {
        let at = origin(
            &Anchor::Corner {
                work_area: bounds(0., 0., 1920., 1032.),
            },
            panel(),
        );
        assert_eq!(at, point(px(1920. - WIDTH - GAP), px(1032. - HEIGHT - GAP)));
    }
}
