//! Settings View

#[cfg(not(target_family = "wasm"))]
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{button::*, scroll::ScrollableElement, select::*, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::theme;
use crate::config::{
    load_config, save_config, AppConfig, REFRESH_INTERVAL_CHOICES_HOURS,
    SUPPORTED_REPORTING_CURRENCIES,
};
use crate::i18n::Language;
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
            tr!("settings.theme_dark", name = self.name)
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
    /// Whether the OS starts CloudBridge at login, as last read from it;
    /// `None` when it could not be read.
    #[cfg(not(target_family = "wasm"))]
    open_at_login: Option<bool>,
    /// Bumped on every Copy system info; the button reads "Copied" while
    /// `copied` holds the latest one, until its timer clears it.
    #[cfg(not(target_family = "wasm"))]
    copy_generation: u64,
    #[cfg(not(target_family = "wasm"))]
    copied: Option<u64>,
    /// A FOCUS export is being chosen or written.
    #[cfg(not(target_family = "wasm"))]
    exporting: bool,
    /// The Insights rules section, which keeps its own state.
    #[cfg(not(target_family = "wasm"))]
    insight_rules: Entity<super::insight_rules::InsightRulesView>,
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
            #[cfg(not(target_family = "wasm"))]
            open_at_login: Self::read_open_at_login(),
            #[cfg(not(target_family = "wasm"))]
            copy_generation: 0,
            #[cfg(not(target_family = "wasm"))]
            copied: None,
            #[cfg(not(target_family = "wasm"))]
            exporting: false,
            #[cfg(not(target_family = "wasm"))]
            insight_rules: cx.new(super::insight_rules::InsightRulesView::new),
            _subscriptions: vec![subscription],
        }
    }

    #[cfg(not(target_family = "wasm"))]
    fn read_open_at_login() -> Option<bool> {
        crate::background::login_item::is_enabled()
            .map_err(|e| tracing::warn!("Could not read the login item: {}", e))
            .ok()
    }

    /// Add or remove the login item. The switch shows what the OS reports
    /// afterwards, so a refusal leaves it where it was.
    #[cfg(not(target_family = "wasm"))]
    fn set_open_at_login(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.save_status = None;
        self.status_generation += 1;
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                crate::background::login_item::set_enabled(enabled)?;
                crate::background::login_item::is_enabled()
            })
            .await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(now) => this.open_at_login = Some(now),
                    Err(e) => {
                        tracing::error!("Could not change the login item: {}", e);
                        this.save_status = Some(StatusMessage::Error(
                            tr!("settings.open_at_login_failed", error = e).into(),
                        ));
                        this.status_generation += 1;
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    #[cfg(not(target_family = "wasm"))]
    fn set_background_refresh(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.save_status = None;
        self.status_generation += 1;
        self.config.background_refresh = enabled;
        self.save_config(cx);
    }

    #[cfg(not(target_family = "wasm"))]
    fn set_keep_in_menu_bar(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.save_status = None;
        self.status_generation += 1;
        self.config.keep_in_menu_bar = enabled;
        self.save_config(cx);
        crate::background::set_menu_bar(enabled, cx);
    }

    #[cfg(not(target_family = "wasm"))]
    fn set_alert_notifications(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.save_status = None;
        self.status_generation += 1;
        self.config.alert_notifications = enabled;
        self.save_config(cx);
    }

    /// The Background section: what keeps running once the window is
    /// closed. Desktop only — a browser tab stops with its page.
    #[cfg(not(target_family = "wasm"))]
    fn render_background(&self, cx: &Context<Self>) -> impl IntoElement {
        let windowless = crate::background::RUNS_WINDOWLESS;
        let open_at_login_description = if windowless && self.config.keep_in_menu_bar {
            if cfg!(target_os = "windows") {
                tr!("settings.open_at_login_hint_tray")
            } else {
                tr!("settings.open_at_login_hint_menu_bar")
            }
        } else {
            tr!("settings.open_at_login_hint")
        };
        let refresh_description = if windowless && self.config.keep_in_menu_bar {
            tr!("settings.background_refresh_hint_menu_bar")
        } else {
            tr!("settings.background_refresh_hint")
        };

        div()
            .v_flex()
            .gap_4()
            .child(Self::setting_row(
                tr!("settings.open_at_login"),
                open_at_login_description,
                Switch::new("open-at-login")
                    .checked(self.open_at_login.unwrap_or(false))
                    .disabled(self.open_at_login.is_none())
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_open_at_login(*checked, cx);
                    })),
                cx,
            ))
            .when(windowless, |el| {
                el.child(Self::setting_row(
                    if cfg!(target_os = "windows") {
                        tr!("settings.keep_in_tray")
                    } else {
                        tr!("settings.keep_in_menu_bar")
                    },
                    if cfg!(target_os = "windows") {
                        tr!("settings.keep_in_tray_hint")
                    } else {
                        tr!("settings.keep_in_menu_bar_hint")
                    },
                    Switch::new("keep-in-menu-bar")
                        .checked(self.config.keep_in_menu_bar)
                        .disabled(self.saving)
                        .on_click(cx.listener(|this, checked: &bool, _, cx| {
                            this.set_keep_in_menu_bar(*checked, cx);
                        })),
                    cx,
                ))
            })
            .child(Self::setting_row(
                tr!("settings.background_refresh"),
                refresh_description,
                Switch::new("background-refresh")
                    .checked(self.config.background_refresh)
                    .disabled(self.saving)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_background_refresh(*checked, cx);
                    })),
                cx,
            ))
            .child(Self::setting_row(
                tr!("settings.alert_notifications"),
                tr!("settings.alert_notifications_hint"),
                Switch::new("alert-notifications")
                    .checked(self.config.alert_notifications)
                    .disabled(self.saving)
                    .on_click(cx.listener(|this, checked: &bool, _, cx| {
                        this.set_alert_notifications(*checked, cx);
                    })),
                cx,
            ))
    }

    /// The language buttons: System, then each translation by its own
    /// name. Applies at once; the save follows.
    fn render_language_choices(&self, cx: &Context<Self>) -> impl IntoElement {
        let chosen = self.config.language.clone();
        let choice = |id: &'static str,
                      label: SharedString,
                      value: Option<&'static str>,
                      cx: &Context<Self>| {
            let selected = chosen.as_deref() == value;
            Button::new(id)
                .label(label)
                .when(selected, |button| button.primary())
                .when(!selected, |button| {
                    button.custom(theme::outline_variant(cx)).card_outline(cx)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.set_language(value, cx);
                }))
        };
        div()
            .h_flex()
            .gap_2()
            .child(choice(
                "language-system",
                tr!("settings.language_system"),
                None,
                cx,
            ))
            .children(Language::ALL.iter().map(|language| {
                choice(
                    language.code(),
                    language.native_name().into(),
                    Some(language.code()),
                    cx,
                )
            }))
    }

    /// Switch the interface language and remember the choice; `None`
    /// follows the system.
    fn set_language(&mut self, code: Option<&'static str>, cx: &mut Context<Self>) {
        self.save_status = None;
        self.status_generation += 1;
        self.config.language = code.map(str::to_string);
        crate::i18n::set_language(Language::resolve(code), cx);
        self.save_config(cx);
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
                crate::ledger::set_reporting_currency(&currency).map_err(|e| {
                    Failure::Switch(tr!("settings.currency_failed", error = e).into())
                })?;
                save_config(&config)
                    .map_err(|e| Failure::Save(tr!("settings.save_failed", error = e).into()))?;
                Ok(())
            })
            .await;
            this.update(cx, |this, cx| {
                this.saving = false;
                this.save_status = Some(match result {
                    Ok(()) => StatusMessage::Success(tr!("settings.saved").into()),
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
                    this.save_status = Some(StatusMessage::Error(
                        tr!("settings.save_failed", error = e).into(),
                    ));
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
            let result = smol::unblock(move || super::data::set_demo_data(load)).await;
            this.update(cx, |this, cx| {
                this.demo_running = false;
                this.save_status = Some(match result {
                    Ok(summary) => {
                        crate::app::request_reload(cx);
                        StatusMessage::Success(summary)
                    }
                    Err(e) => {
                        tracing::error!("Demo data operation failed: {}", e);
                        StatusMessage::Error(tr!("settings.demo_failed", error = e).into())
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

    /// Put [`system_info`] on the clipboard, for a bug report. The account
    /// read is a store query, so it runs off the UI thread; the button says
    /// "Copied" for two seconds once the text is there.
    #[cfg(not(target_family = "wasm"))]
    fn copy_system_info(&mut self, cx: &mut Context<Self>) {
        let config = self.config.clone();
        self.copy_generation += 1;
        let generation = self.copy_generation;
        cx.spawn(async move |this, cx| {
            let info = smol::unblock(move || system_info(&config)).await;
            let copied = this.update(cx, |this, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(info));
                this.copied = Some(generation);
                cx.notify();
            });
            if copied.is_err() {
                return;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_secs(2))
                .await;
            this.update(cx, |this, cx| {
                if this.copied == Some(generation) {
                    this.copied = None;
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Ask where to save, then write the ledger there as FOCUS. The button
    /// stays busy from the dialog until the file is written.
    #[cfg(not(target_family = "wasm"))]
    fn export_focus(&mut self, cx: &mut Context<Self>) {
        if self.exporting {
            return;
        }
        self.exporting = true;
        self.save_status = None;
        self.status_generation += 1;
        cx.notify();

        let directory = directories::UserDirs::new()
            .and_then(|dirs| dirs.document_dir().map(std::path::Path::to_path_buf))
            .or_else(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_path_buf()))
            .unwrap_or_default();
        let name = format!(
            "cloudbridge-focus-{}.csv",
            chrono::Local::now().format("%Y-%m-%d")
        );
        let chosen = cx.prompt_for_new_path(&directory, Some(&name));

        cx.spawn(async move |this, cx| {
            let outcome: Option<Result<String, String>> = match chosen.await {
                Ok(Ok(Some(path))) => Some(
                    smol::unblock(move || {
                        crate::ledger::focus_export::export_csv(&path)
                            .map(|rows| {
                                tr!("settings.exported", rows = rows, path = path.display()).into()
                            })
                            .map_err(|e| tr!("settings.export_failed", error = e).into())
                    })
                    .await,
                ),
                // Cancelled: nothing to say.
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(e)) => Some(Err(tr!("settings.save_dialog_failed", error = e).into())),
            };
            this.update(cx, |this, cx| {
                this.exporting = false;
                match outcome {
                    Some(Ok(message)) => {
                        this.save_status = Some(StatusMessage::Success(message));
                    }
                    Some(Err(message)) => {
                        tracing::error!("{}", message);
                        this.save_status = Some(StatusMessage::Error(message));
                    }
                    None => {}
                }
                this.status_generation += 1;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The Data section: the ledger, out of the app. Desktop only — the
    /// browser demo has no file to write.
    #[cfg(not(target_family = "wasm"))]
    fn render_data(&self, cx: &Context<Self>) -> impl IntoElement {
        Self::setting_row(
            tr!("settings.export_focus"),
            tr!("settings.export_focus_hint"),
            Button::new("export-focus")
                .label(tr!("settings.export"))
                .custom(theme::outline_variant(cx))
                .card_outline(cx)
                .loading(self.exporting)
                .disabled(self.exporting)
                .on_click(cx.listener(|this, _, _, cx| {
                    this.export_focus(cx);
                })),
            cx,
        )
    }

    fn render_section(
        &self,
        title: SharedString,
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
        label: SharedString,
        description: SharedString,
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
                    .child(theme::page_title(cx, tr!("settings.title")))
                    .child(theme::caption(cx, tr!("settings.subtitle"))),
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
            .child(
                self.render_section(
                    tr!("settings.appearance"),
                    div()
                        .v_flex()
                        .gap_4()
                        .child(Self::setting_row(
                            tr!("settings.theme"),
                            tr!("settings.theme_hint"),
                            Select::new(&self.theme_select).w_72(),
                            cx,
                        ))
                        .child(Self::setting_row(
                            tr!("settings.language"),
                            tr!("settings.language_hint"),
                            self.render_language_choices(cx),
                            cx,
                        )),
                    cx,
                ),
            )
            // Reporting currency
            .child(
                self.render_section(
                    tr!("settings.reporting"),
                    Self::setting_row(
                        tr!("settings.currency"),
                        tr!("settings.currency_hint"),
                        div()
                            .h_flex()
                            .gap_2()
                            .children(SUPPORTED_REPORTING_CURRENCIES.iter().map(|currency| {
                                let selected = *currency == reporting_currency;
                                Button::new(SharedString::from(format!("currency-{currency}")))
                                    .label(*currency)
                                    .when(selected, |button| button.primary())
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
                    tr!("settings.refreshing"),
                    Self::setting_row(
                        tr!("settings.refresh_interval"),
                        tr!("settings.refresh_interval_hint"),
                        div()
                            .h_flex()
                            .gap_2()
                            .children(REFRESH_INTERVAL_CHOICES_HOURS.iter().map(|hours| {
                                let selected = *hours == refresh_interval_hours;
                                Button::new(SharedString::from(format!("refresh-{hours}h")))
                                    .label(tr!("settings.hours", n = hours))
                                    .when(selected, |button| button.primary())
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
            // Background
            .map(|el| {
                #[cfg(not(target_family = "wasm"))]
                let el = el
                    .child(self.render_section(
                        tr!("settings.background"),
                        self.render_background(cx),
                        cx,
                    ))
                    .child(self.render_section(tr!("settings.data"), self.render_data(cx), cx))
                    .child(self.render_section(
                        tr!("settings.insights_rules"),
                        self.insight_rules.clone(),
                        cx,
                    ));
                el
            })
            // Demo data
            .child(
                self.render_section(
                    tr!("settings.demo_data"),
                    div()
                        .h_flex()
                        .justify_between()
                        .items_center()
                        .child(
                            div()
                                .min_w_0()
                                .child(theme::caption(cx, tr!("settings.demo_data_hint"))),
                        )
                        .child(
                            div()
                                .h_flex()
                                .gap_2()
                                .child(
                                    Button::new("load-demo-data")
                                        .label(tr!("settings.load_demo"))
                                        .primary()
                                        .loading(self.demo_running)
                                        .disabled(self.demo_running)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.set_demo_data(true, cx);
                                        })),
                                )
                                .child(
                                    Button::new("clear-demo-data")
                                        .label(tr!("settings.clear_demo"))
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
                    tr!("settings.about"),
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
                                        .child(tr!("settings.version")),
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
                                        .child(tr!("settings.built_with")),
                                )
                                .child(div().child("GPUI + Rust")),
                        )
                        .map(|el| {
                            #[cfg(not(target_family = "wasm"))]
                            let el = el.child(
                                div().pt_2().child(
                                    Button::new("copy-system-info")
                                        .label(if self.copied.is_some() {
                                            tr!("settings.copied")
                                        } else {
                                            tr!("settings.copy_system_info")
                                        })
                                        .icon(if self.copied.is_some() {
                                            IconName::Check
                                        } else {
                                            IconName::Copy
                                        })
                                        .custom(theme::outline_variant(cx))
                                        .card_outline(cx)
                                        .tooltip(tr!("settings.copy_system_info_tooltip"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.copy_system_info(cx);
                                        })),
                                ),
                            );
                            el
                        }),
                    cx,
                ),
            )
    }
}

/// What a bug report needs to know about this install: version, platform,
/// the settings that change what the app fetches and shows, and which
/// kinds of source it reads. Nothing that identifies an account — no
/// names, ids, keys or amounts — since the text is meant to be pasted
/// into a public issue. Blocking: it reads the account list.
#[cfg(not(target_family = "wasm"))]
fn system_info(config: &AppConfig) -> String {
    use std::collections::BTreeMap;

    let mut lines = vec![
        format!("CloudBridge {}", env!("CARGO_PKG_VERSION")),
        format!(
            "Platform: {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ),
        format!(
            "Theme: {}",
            config
                .theme
                .name
                .as_deref()
                .unwrap_or(if config.theme.dark_mode {
                    theme::DARK_THEME_NAME
                } else {
                    theme::LIGHT_THEME_NAME
                })
        ),
        format!("Reporting currency: {}", config.reporting_currency),
        format!("Refresh interval: {}h", config.refresh_interval_hours),
        format!(
            "Background refresh: {}",
            if config.background_refresh {
                "on"
            } else {
                "off"
            }
        ),
    ];
    match crate::db::get_all_accounts() {
        Ok(accounts) => {
            // Source id to (accounts, of which disabled).
            let mut sources: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
            for account in &accounts {
                let entry = sources.entry(account.source_id.as_str()).or_default();
                entry.0 += 1;
                if !account.enabled {
                    entry.1 += 1;
                }
            }
            let listed: Vec<String> = sources
                .iter()
                .map(|(source, (count, disabled))| match disabled {
                    0 => format!("{source} x{count}"),
                    disabled => format!("{source} x{count} ({disabled} disabled)"),
                })
                .collect();
            lines.push(if listed.is_empty() {
                "Sources: none".to_string()
            } else {
                format!("Sources: {}", listed.join(", "))
            });
        }
        Err(e) => lines.push(format!("Sources: couldn't read ({e})")),
    }
    lines.push(match crate::config::get_app_data_dir() {
        Ok(dir) => format!("Data folder: {}", dir.display()),
        Err(e) => format!("Data folder: unknown ({e})"),
    });
    lines.join("\n")
}
