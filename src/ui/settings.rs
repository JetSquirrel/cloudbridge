//! Settings View

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{button::*, select::*, *};

use crate::config::{
    load_config, save_config, AppConfig, REFRESH_INTERVAL_CHOICES_HOURS,
    SUPPORTED_REPORTING_CURRENCIES,
};

/// One entry in the theme picker.
#[derive(Clone)]
struct ThemeItem {
    name: SharedString,
    dark: bool,
}

impl SelectItem for ThemeItem {
    type Value = SharedString;

    fn title(&self) -> SharedString {
        if self.dark {
            format!("{} (dark)", self.name).into()
        } else {
            self.name.clone()
        }
    }

    fn value(&self) -> &Self::Value {
        &self.name
    }
}

/// Settings View
pub struct SettingsView {
    /// Configuration
    config: AppConfig,
    /// Save status
    save_status: Option<String>,
    /// Theme picker state
    theme_select: Entity<SelectState<SearchableVec<ThemeItem>>>,
    _subscriptions: Vec<Subscription>,
}

impl SettingsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let config = load_config().unwrap_or_default();

        // Every theme the registry holds, sorted default-first / light-first.
        let items: Vec<ThemeItem> = ThemeRegistry::global(cx)
            .sorted_themes()
            .iter()
            .map(|theme| ThemeItem {
                name: theme.name.clone(),
                dark: theme.mode.is_dark(),
            })
            .collect();

        // What the picker shows as chosen: the persisted name, or the
        // CloudBridge theme the dark-mode flag implies for old configs.
        let current = config.theme.name.clone().unwrap_or_else(|| {
            if config.theme.dark_mode {
                super::theme::DARK_THEME_NAME.to_string()
            } else {
                super::theme::LIGHT_THEME_NAME.to_string()
            }
        });
        let selected = items.iter().position(|item| item.name.as_str() == current);

        let theme_select = cx.new(|cx| {
            SelectState::new(
                SearchableVec::new(items),
                selected.map(|ix| IndexPath::default().row(ix)),
                window,
                cx,
            )
            .searchable(true)
        });

        let subscription = cx.subscribe(&theme_select, |this, _, event, cx| {
            if let SelectEvent::Confirm(Some(name)) = event {
                this.set_theme(name, cx);
            }
        });

        Self {
            config,
            save_status: None,
            theme_select,
            _subscriptions: vec![subscription],
        }
    }

    /// Apply a theme picked in the selector and remember it.
    ///
    /// `dark_mode` is persisted alongside as a derived hint for old readers
    /// that only know the flag. An unknown name (theme file vanished between
    /// listing and picking) is not persisted.
    fn set_theme(&mut self, name: &SharedString, cx: &mut Context<Self>) {
        super::theme::apply_theme_by_name(name, cx);

        if let Some(theme) = ThemeRegistry::global(cx).themes().get(name.as_str()) {
            self.config.theme.dark_mode = theme.mode.is_dark();
            self.config.theme.name = Some(name.to_string());
            self.save_config(cx);
        }
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

    /// Change how long a billing period stays fresh before a refresh will
    /// fetch it again.
    ///
    /// Takes effect on the next refresh; nothing is re-fetched here. A
    /// longer interval is the cheaper one — Cost Explorer bills per
    /// request — which is why the default is a day.
    fn set_refresh_interval(&mut self, hours: u32, cx: &mut Context<Self>) {
        self.config.refresh_interval_hours = hours;
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
        let reporting_currency = self.config.reporting_currency.clone();
        let refresh_interval_hours = self.config.refresh_interval_hours;

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
                            div().v_flex().child(div().child("Theme")).child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("Applies immediately and is remembered across launches"),
                            ),
                        )
                        .child(Select::new(&self.theme_select).w(px(280.0))),
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
            // Refreshing
            .child(
                self.render_section(
                    "Refreshing",
                    div()
                        .h_flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div().v_flex().child(div().child("Refresh interval")).child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(
                                        "A billing period is fetched again only once \
                                             this has passed. Longer is cheaper: AWS Cost \
                                             Explorer bills per request. Force Refresh \
                                             ignores it; an imported bill file is never \
                                             re-fetched.",
                                    ),
                            ),
                        )
                        .child(div().h_flex().gap_2().children(
                            REFRESH_INTERVAL_CHOICES_HOURS.iter().map(|hours| {
                                Button::new(SharedString::from(format!("refresh-{hours}h")))
                                    .label(format!("{hours}h"))
                                    .when(*hours == refresh_interval_hours, |button| {
                                        button.primary()
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_refresh_interval(*hours, cx);
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
