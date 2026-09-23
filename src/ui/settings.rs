//! Settings View

use gpui_kit::component::{button::*, scroll::ScrollableElement, select::*, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::theme;
use crate::config::{
    load_config, save_config, AppConfig, REFRESH_INTERVAL_CHOICES_HOURS,
    SUPPORTED_REPORTING_CURRENCIES,
};
use crate::ui::theme::CardOutline as _;

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

/// Outcome of the last settings action, shown in the banner under the page
/// title. Success and failure render in different colors, so a failed
/// save can't be mistaken for a saved one.
#[derive(Clone)]
enum StatusMessage {
    Success(String),
    Error(String),
}

/// Settings View
pub struct SettingsView {
    /// Configuration
    config: AppConfig,
    /// Save status
    save_status: Option<StatusMessage>,
    /// Bumped whenever the status is (re)assigned or cleared; an auto-fade
    /// timer only clears the banner while its own generation is current.
    status_generation: u64,
    /// A ledger rebuild / config write is running off the UI thread; the
    /// controls that would race it stay disabled until it lands.
    saving: bool,
    /// An edit landed while `saving` was set; written once that flight
    /// finishes so no change is silently dropped.
    save_pending: bool,
    /// A demo-data load or clear is running off the UI thread.
    demo_running: bool,
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
                theme::DARK_THEME_NAME.to_string()
            } else {
                theme::LIGHT_THEME_NAME.to_string()
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
            status_generation: 0,
            saving: false,
            save_pending: false,
            demo_running: false,
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
        // A new edit supersedes the last outcome; clear it so a stale
        // "Settings saved" doesn't drift beside unsaved changes.
        self.save_status = None;
        self.status_generation += 1;
        theme::apply_theme_by_name(name, cx);

        if let Some(theme) = ThemeRegistry::global(cx).themes().get(name.as_str()) {
            self.config.theme.dark_mode = theme.mode.is_dark();
            self.config.theme.name = Some(name.to_string());
            self.save_config(cx);
        }
    }

    /// Fade a success banner five seconds after it was set; a banner
    /// reassigned in the meantime (newer generation) is left alone. Error
    /// banners persist until the next action answers them.
    fn schedule_status_fade(&mut self, cx: &mut Context<Self>) {
        self.status_generation += 1;
        let generation = self.status_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_secs(5))
                .await;
            this.update(cx, |this, cx| {
                if this.status_generation == generation
                    && matches!(this.save_status, Some(StatusMessage::Success(_)))
                {
                    this.save_status = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Change the currency every amount is shown in.
    ///
    /// Only the reading view is rebuilt — charges stay in the currency
    /// they were billed in, so this costs nothing and loses nothing. The
    /// rebuild and the config write are blocking, so they run off the UI
    /// thread; a second switch while one is in flight is ignored rather
    /// than raced.
    fn set_reporting_currency(&mut self, currency: &str, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        self.save_status = None;
        self.status_generation += 1;
        let previous = std::mem::replace(&mut self.config.reporting_currency, currency.to_string());
        self.saving = true;
        cx.notify();

        let config = self.config.clone();
        let currency = currency.to_string();
        cx.spawn(async move |this, cx| {
            // The two steps fail differently: if the rebuild fails the
            // ledger still shows the old currency, so the selection is put
            // back to match; if only the write fails the switch did apply.
            enum Failure {
                Switch(String),
                Save(String),
            }
            let result = smol::unblock(move || -> Result<(), Failure> {
                crate::ledger::set_reporting_currency(&currency)
                    .map_err(|e| Failure::Switch(format!("Could not switch currency: {e}")))?;
                save_config(&config).map_err(|e| Failure::Save(format!("Save failed: {e}")))?;
                Ok(())
            })
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                this.save_status = Some(match result {
                    Ok(()) => StatusMessage::Success("Settings saved".to_string()),
                    Err(Failure::Switch(e)) => {
                        tracing::error!("Failed to switch reporting currency: {}", e);
                        this.config.reporting_currency = previous;
                        StatusMessage::Error(e)
                    }
                    Err(Failure::Save(e)) => {
                        tracing::error!("Failed to save settings: {}", e);
                        StatusMessage::Error(e)
                    }
                });
                if matches!(this.save_status, Some(StatusMessage::Success(_))) {
                    this.schedule_status_fade(cx);
                } else {
                    this.status_generation += 1;
                }
                if this.save_pending {
                    this.save_pending = false;
                    this.save_config(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Change how long a billing period stays fresh before a refresh will
    /// fetch it again.
    ///
    /// Takes effect on the next refresh; nothing is re-fetched here. A
    /// longer interval is the cheaper one — Cost Explorer bills per
    /// request — which is why the default is a day.
    fn set_refresh_interval(&mut self, hours: u32, cx: &mut Context<Self>) {
        self.save_status = None;
        self.status_generation += 1;
        self.config.refresh_interval_hours = hours;
        self.save_config(cx);
    }

    /// Persist the config off the UI thread. If a write is already in
    /// flight, remember that and re-run with the latest config when it
    /// lands, so edits made during a slow currency switch still stick.
    ///
    /// These saves back settings that apply the moment they change, so a
    /// success stays silent; only a failure is surfaced in the banner.
    fn save_config(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            self.save_pending = true;
            return;
        }
        self.saving = true;
        cx.notify();

        let config = self.config.clone();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || save_config(&config)).await;
            this.update(cx, |this, cx| {
                this.saving = false;
                if let Err(e) = result {
                    tracing::error!("Failed to save settings: {}", e);
                    this.save_status = Some(StatusMessage::Error(format!("Save failed: {e}")));
                    this.status_generation += 1;
                }
                if this.save_pending {
                    this.save_pending = false;
                    this.save_config(cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Load or clear the demo ledger off the UI thread, then re-evaluate
    /// the alert rules so the demo's spike and low balance show up, and
    /// ask the shell to reload whatever page is showing.
    fn set_demo_data(&mut self, load: bool, cx: &mut Context<Self>) {
        if self.demo_running {
            return;
        }
        self.demo_running = true;
        self.save_status = None;
        self.status_generation += 1;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                let summary = if load {
                    crate::ledger::demo::seed_demo()
                } else {
                    crate::ledger::demo::clear_demo()
                }?;
                crate::alerts::evaluate()?;
                Ok::<_, anyhow::Error>(summary)
            })
            .await;
            this.update(cx, |this, cx| {
                this.demo_running = false;
                this.save_status = Some(match result {
                    Ok(summary) => {
                        crate::app::request_reload(cx);
                        StatusMessage::Success(summary)
                    }
                    Err(e) => {
                        tracing::error!("Demo data operation failed: {}", e);
                        StatusMessage::Error(format!("Demo data failed: {e}"))
                    }
                });
                if matches!(this.save_status, Some(StatusMessage::Success(_))) {
                    this.schedule_status_fade(cx);
                } else {
                    this.status_generation += 1;
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_section(
        &self,
        title: &'static str,
        children: impl IntoElement,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(theme::section_title(cx, title))
            .child(children)
    }

    /// A labelled setting: name and muted description on the left, the
    /// control pinned to the right.
    fn setting_row(
        label: &'static str,
        description: &'static str,
        control: impl IntoElement,
        cx: &Context<Self>,
    ) -> Div {
        div()
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .min_w_0()
                    .v_flex()
                    .child(div().child(label))
                    .child(theme::caption(cx, description)),
            )
            .child(control)
    }
}

impl Render for SettingsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let reporting_currency = self.config.reporting_currency.clone();
        let refresh_interval_hours = self.config.refresh_interval_hours;
        let saving = self.saving;

        div()
            .size_full()
            .p_8()
            .v_flex()
            .gap_6()
            .bg(cx.theme().background)
            .overflow_y_scrollbar()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, "Settings"))
                    .child(theme::caption(
                        cx,
                        "Saved locally and applied across every page",
                    )),
            )
            // Save status
            .when_some(self.save_status.clone(), |el, status| {
                let (message, bg, fg) = match status {
                    StatusMessage::Success(message) => {
                        (message, theme::success_bg(cx), theme::success(cx))
                    }
                    StatusMessage::Error(message) => {
                        (message, theme::danger_bg(cx), theme::danger(cx))
                    }
                };
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(bg)
                        .text_color(fg)
                        .child(message),
                )
            })
            // Appearance settings
            .child(self.render_section(
                "Appearance",
                Self::setting_row(
                    "Theme",
                    "Applies immediately and is remembered across launches",
                    Select::new(&self.theme_select).w_72(),
                    cx,
                ),
                cx,
            ))
            // Reporting currency
            .child(
                self.render_section(
                    "Reporting",
                    Self::setting_row(
                        "Currency",
                        "Totals are converted to this currency. \
                     Charges keep the currency they were billed in.",
                        div()
                            .h_flex()
                            .gap_2()
                            .children(SUPPORTED_REPORTING_CURRENCIES.iter().map(|currency| {
                                let selected = *currency == reporting_currency;
                                Button::new(SharedString::from(format!("currency-{currency}")))
                                    .label(*currency)
                                    .small()
                                    .when(selected, |button| {
                                        button.custom(theme::accent_variant(cx))
                                    })
                                    .when(!selected, |button| {
                                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                                    })
                                    .loading(saving && selected)
                                    .disabled(saving)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_reporting_currency(currency, cx);
                                    }))
                            })),
                        cx,
                    ),
                    cx,
                ),
            )
            // Refreshing
            .child(
                self.render_section(
                    "Refreshing",
                    Self::setting_row(
                        "Refresh interval",
                        "A billing period is fetched again only once \
                     this has passed. Longer is cheaper: AWS Cost \
                     Explorer bills per request. Force Refresh \
                     ignores it; an imported bill file is never \
                     re-fetched.",
                        div()
                            .h_flex()
                            .gap_2()
                            .children(REFRESH_INTERVAL_CHOICES_HOURS.iter().map(|hours| {
                                let selected = *hours == refresh_interval_hours;
                                Button::new(SharedString::from(format!("refresh-{hours}h")))
                                    .label(format!("{hours}h"))
                                    .small()
                                    .when(selected, |button| {
                                        button.custom(theme::accent_variant(cx))
                                    })
                                    .when(!selected, |button| {
                                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                                    })
                                    .disabled(saving)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.set_refresh_interval(*hours, cx);
                                    }))
                            })),
                        cx,
                    ),
                    cx,
                ),
            )
            // Demo data
            .child(
                self.render_section(
                    "Demo data",
                    div()
                        .h_flex()
                        .justify_between()
                        .items_center()
                        .child(div().min_w_0().child(theme::caption(
                            cx,
                            "Twelve months of fake but realistic spend across \
                         three demo accounts, for design review. Demo rows \
                         never touch a real API.",
                        )))
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(
                                    Button::new("load-demo-data")
                                        .label("Load demo data")
                                        .primary()
                                        .loading(self.demo_running)
                                        .disabled(self.demo_running)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_demo_data(true, cx);
                                        })),
                                )
                                .child(
                                    Button::new("clear-demo-data")
                                        .label("Clear demo data")
                                        .small()
                                        .custom(theme::outline_variant(cx))
                                        .card_outline(cx)
                                        .disabled(self.demo_running)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_demo_data(false, cx);
                                        })),
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
                                        .w_24()
                                        .flex_shrink_0()
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
                                        .w_24()
                                        .flex_shrink_0()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Built with:"),
                                )
                                .child(div().child("GPUI + Rust")),
                        ),
                    cx,
                ),
            )
    }
}
