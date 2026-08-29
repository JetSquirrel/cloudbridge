//! Settings View

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{button::*, switch::*, *};

use crate::config::{load_config, save_config, AppConfig, SUPPORTED_REPORTING_CURRENCIES};

/// Settings View
pub struct SettingsView {
    /// Configuration
    config: AppConfig,
    /// Save status
    save_status: Option<String>,
}

impl SettingsView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        let config = load_config().unwrap_or_default();

        Self {
            config,
            save_status: None,
        }
    }

    fn set_dark_mode(&mut self, dark: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.config.theme.dark_mode = dark;
        Theme::change(
            if dark {
                ThemeMode::Dark
            } else {
                ThemeMode::Light
            },
            Some(window),
            cx,
        );
        self.save_config(cx);
    }

    /// Change the currency every amount is shown in.
    ///
    /// Only the reading view is rebuilt — charges stay in the currency
    /// they were billed in, so this costs nothing and loses nothing.
    fn set_reporting_currency(&mut self, currency: &str, cx: &mut Context<Self>) {
        self.config.reporting_currency = currency.to_string();

        if let Err(e) = crate::ledger::set_reporting_currency(currency) {
            tracing::error!("Failed to switch reporting currency: {}", e);
            self.save_status = Some(format!("Could not switch currency: {}", e));
            cx.notify();
            return;
        }

        self.save_config(cx);
    }

    fn save_config(&mut self, cx: &mut Context<Self>) {
        match save_config(&self.config) {
            Ok(_) => {
                self.save_status = Some("Settings saved".to_string());
            }
            Err(e) => {
                self.save_status = Some(format!("Save failed: {}", e));
            }
        }
        cx.notify();
    }

    fn render_section(
        &self,
        title: &str,
        children: impl IntoElement,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        div()
            .w_full()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .v_flex()
            .gap_4()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .child(title.to_string()),
            )
            .child(children)
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dark_mode = self.config.theme.dark_mode;
        let reporting_currency = self.config.reporting_currency.clone();

        div()
            .size_full()
            .p_6()
            .v_flex()
            .gap_6()
            .bg(cx.theme().background)
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(cx.theme().foreground)
                    .child("Settings"),
            )
            // Appearance settings
            .child(
                self.render_section(
                    "Appearance",
                    div()
                        .h_flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div().v_flex().child(div().child("Dark Mode")).child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Use dark theme"),
                            ),
                        )
                        .child(
                            Switch::new("dark-mode")
                                .checked(dark_mode)
                                .on_click(cx.listener(|this, checked: &bool, window, cx| {
                                    this.set_dark_mode(*checked, window, cx);
                                })),
                        ),
                    cx,
                ),
            )
            // Reporting currency
            .child(
                self.render_section(
                    "Reporting",
                    div()
                        .h_flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div().v_flex().child(div().child("Currency")).child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "Totals are converted to this currency. \
                                         Charges keep the currency they were billed in.",
                                    ),
                            ),
                        )
                        .child(div().h_flex().gap_2().children(
                            SUPPORTED_REPORTING_CURRENCIES.iter().map(|currency| {
                                Button::new(SharedString::from(format!("currency-{currency}")))
                                    .label(*currency)
                                    .when(*currency == reporting_currency, |button| {
                                        button.primary()
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_reporting_currency(currency, cx);
                                    }))
                            }),
                        )),
                    cx,
                ),
            )
            // About
            .child(
                self.render_section(
                    "About",
                    div()
                        .v_flex()
                        .gap_2()
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Version:"),
                                )
                                .child(div().child(env!("CARGO_PKG_VERSION"))),
                        )
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(
                                    div()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Built with:"),
                                )
                                .child(div().child("GPUI + Rust")),
                        ),
                    cx,
                ),
            )
            // Save status
            .when_some(self.save_status.clone(), |el, status| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(gpui::green().opacity(0.1))
                        .text_color(gpui::green())
                        .child(status),
                )
            })
    }
}
