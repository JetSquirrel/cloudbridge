//! Settings View

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{switch::*, *};

use crate::config::{load_config, save_config, AppConfig};

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

    fn toggle_dark_mode(&mut self, cx: &mut Context<Self>) {
        self.config.theme.dark_mode = !self.config.theme.dark_mode;
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
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.toggle_dark_mode(cx);
                                })),
                        ),
                    cx,
                ),
            )
            // Data settings
            .child(
                self.render_section(
                    "Data",
                    div().v_flex().gap_3().child(
                        div().h_flex().justify_between().items_center().child(
                            div()
                                .v_flex()
                                .child(div().child("Data Refresh Interval"))
                                .child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(format!(
                                            "{} minutes",
                                            self.config.refresh_interval_minutes
                                        )),
                                ),
                        ),
                    ),
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
                                .child(div().child("0.1.0")),
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
