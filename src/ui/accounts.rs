//! Cloud Account Management View

use chrono::Utc;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*,
    input::{Input, InputState},
    *,
};
use uuid::Uuid;

use crate::cloud::registry::{self, SourceDescriptor};
use crate::cloud::CloudAccount;
use crate::db;

/// Account Management View
pub struct AccountsView {
    /// Account list
    accounts: Vec<CloudAccount>,
    /// Whether to show add dialog
    show_add_dialog: bool,
    /// Error message
    error: Option<String>,
    /// Success message
    success: Option<String>,
    /// Input field states
    name_input: Entity<InputState>,
    ak_input: Entity<InputState>,
    sk_input: Entity<InputState>,
    region_input: Entity<InputState>,
    /// Billing source selected in the add dialog
    selected_source: &'static SourceDescriptor,
}

impl AccountsView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx).placeholder("Account Name"));
        let ak_input = cx.new(|cx| InputState::new(window, cx).placeholder("Access Key ID"));
        let sk_input = cx.new(|cx| InputState::new(window, cx).placeholder("Secret Access Key"));
        let region_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Region (optional, default us-east-1)")
                .default_value("us-east-1")
        });

        let mut view = Self {
            accounts: Vec::new(),
            show_add_dialog: false,
            error: None,
            success: None,
            name_input,
            ak_input,
            sk_input,
            region_input,
            selected_source: registry::default_source(),
        };

        view.load_accounts();
        view
    }

    fn load_accounts(&mut self) {
        match db::get_all_accounts() {
            Ok(accounts) => {
                self.accounts = accounts;
                self.error = None;
            }
            Err(e) => {
                self.error = Some(format!("Failed to load accounts: {}", e));
            }
        }
    }

    fn show_add_dialog(&mut self, cx: &mut Context<Self>) {
        self.show_add_dialog = true;
        self.selected_source = registry::default_source();
        self.error = None;
        self.success = None;
        cx.notify();
    }

    fn set_source(
        &mut self,
        source: &'static SourceDescriptor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.selected_source = source;

        // Every label comes from the descriptor, so a new source needs no
        // change here.
        self.ak_input.update(cx, |state, cx| {
            state.set_placeholder(source.access_key_label, window, cx);
        });
        self.sk_input.update(cx, |state, cx| {
            state.set_placeholder(source.secret_key_placeholder(), window, cx);
        });
        self.region_input.update(cx, |state, cx| {
            state.set_placeholder(source.region_placeholder(), window, cx);
            state.set_value(source.default_region.unwrap_or_default(), window, cx);
        });

        cx.notify();
    }

    fn hide_add_dialog(&mut self, cx: &mut Context<Self>) {
        self.show_add_dialog = false;
        cx.notify();
    }

    fn save_account(&mut self, cx: &mut Context<Self>) {
        // Get values from input fields
        let name = self.name_input.read(cx).value().to_string();
        let ak = self.ak_input.read(cx).value().to_string();
        let sk = self.sk_input.read(cx).value().to_string();
        let region = self.region_input.read(cx).value().to_string();

        // Validation
        if name.is_empty() {
            self.error = Some("Please enter account name".to_string());
            cx.notify();
            return;
        }
        if ak.is_empty() {
            self.error = Some("Please enter API Key".to_string());
            cx.notify();
            return;
        }
        if sk.is_empty() && self.selected_source.needs_secret_key() {
            self.error = Some("Please enter Secret Access Key".to_string());
            cx.notify();
            return;
        }

        let account = CloudAccount {
            id: Uuid::new_v4().to_string(),
            name,
            source_id: self.selected_source.source_id(),
            access_key_id: ak,
            secret_access_key: sk,
            region: if region.is_empty() {
                None
            } else {
                Some(region)
            },
            created_at: Utc::now(),
            last_synced_at: None,
            enabled: true,
        };

        match db::save_account(&account) {
            Ok(_) => {
                self.success = Some("Account added successfully".to_string());
                self.error = None;
                self.show_add_dialog = false;
                self.load_accounts();
            }
            Err(e) => {
                self.error = Some(format!("Save failed: {}", e));
            }
        }
        cx.notify();
    }

    fn delete_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        match db::delete_account(account_id) {
            Ok(_) => {
                self.success = Some("Account deleted".to_string());
                self.load_accounts();
            }
            Err(e) => {
                self.error = Some(format!("Delete failed: {}", e));
            }
        }
        cx.notify();
    }

    fn validate_account(&mut self, account: &CloudAccount, cx: &mut Context<Self>) {
        let Some(descriptor) = account.descriptor() else {
            self.error = Some(format!(
                "Account {} uses an unknown billing source",
                account.name
            ));
            self.success = None;
            cx.notify();
            return;
        };

        let account_name = account.name.clone();
        let context = account.context(descriptor);

        // Show validating status
        self.success = Some(format!("Validating account {}...", account_name));
        self.error = None;
        cx.notify();

        // Use standard thread to handle sync HTTP requests
        let (tx, rx) = std::sync::mpsc::channel::<Result<bool, String>>();

        std::thread::spawn(move || {
            let result = (descriptor.build)(context)
                .validate_credentials()
                .map_err(|e| e.to_string());
            let _ = tx.send(result);
        });

        // Use gpui spawn to check results
        cx.spawn(async move |this, cx| {
            // Wait for result in background thread
            let validation_result = smol::unblock(move || {
                rx.recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap_or(Err("Validation timeout".to_string()))
            })
            .await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    match validation_result {
                        Ok(true) => {
                            this.success =
                                Some(format!("Account {} validated successfully!", account_name));
                            this.error = None;
                        }
                        Ok(false) => {
                            this.error =
                                Some(format!("Account {} credentials invalid", account_name));
                            this.success = None;
                        }
                        Err(e) => {
                            this.error = Some(format!("Validation failed: {}", e));
                            this.success = None;
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

    fn render_source_selector(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .h_flex()
            .gap_2()
            .children(registry::all().iter().map(|source| {
                let is_selected = source.id == self.selected_source.id;

                div()
                    .id(SharedString::from(source.id))
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .cursor_pointer()
                    .when(is_selected, |el| {
                        el.bg(cx.theme().accent)
                            .text_color(cx.theme().accent_foreground)
                    })
                    .when(!is_selected, |el| {
                        el.bg(cx.theme().muted)
                            .text_color(cx.theme().muted_foreground)
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            this.set_source(source, window, cx);
                        }),
                    )
                    .child(source.short_name)
            }))
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(cx.theme().foreground)
                    .child("Cloud Account Management"),
            )
            .child(
                Button::new("add")
                    .label("Add Account")
                    .primary()
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_add_dialog(cx);
                    })),
            )
    }

    fn render_accounts_list(&self, cx: &Context<Self>) -> impl IntoElement {
        if self.accounts.is_empty() {
            return div().w_full().p_8().items_center().justify_center().child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No cloud accounts yet, click the button above to add"),
            );
        }

        div().w_full().v_flex().gap_3().children(
            self.accounts
                .iter()
                .map(|account| self.render_account_row(account, cx)),
        )
    }

    fn render_account_row(&self, account: &CloudAccount, cx: &Context<Self>) -> impl IntoElement {
        let account_id = account.id.clone();
        let account_for_validate = account.clone();
        let short_name = account.short_name();

        div()
            .w_full()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .h_flex()
                    .gap_4()
                    .items_center()
                    .child(
                        div()
                            .w(px(80.0))
                            .text_xs()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(cx.theme().accent.opacity(0.1))
                            .text_color(cx.theme().accent)
                            .text_center()
                            .child(short_name),
                    )
                    .child(
                        div()
                            .v_flex()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().foreground)
                                    .child(account.name.clone()),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!(
                                        "AK: {}****",
                                        &account.access_key_id
                                            [..8.min(account.access_key_id.len())]
                                    )),
                            ),
                    ),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("validate-{}", account.id)))
                            .label("Validate")
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.validate_account(&account_for_validate, cx);
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("delete-{}", account.id)))
                            .label("Delete")
                            .danger()
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_account(&account_id, cx);
                            })),
                    ),
            )
    }

    fn render_add_dialog(&self, cx: &Context<Self>) -> impl IntoElement {
        if !self.show_add_dialog {
            return div().size_0();
        }

        // Dialog overlay
        div()
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui::black().opacity(0.5))
            .child(
                // Dialog content
                div()
                    .w(px(480.0))
                    .max_h(px(600.0))
                    .p_6()
                    .rounded_xl()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .shadow_lg()
                    .v_flex()
                    .gap_4()
                    .overflow_y_hidden()
                    .child(
                        div()
                            .h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Add Cloud Account"),
                            )
                            .child(
                                Button::new("close")
                                    .label("×")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.hide_add_dialog(cx);
                                    })),
                            ),
                    )
                    // Form
                    .child(
                        div()
                            .v_flex()
                            .gap_4()
                            .child(
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("Cloud Provider"))
                                    .child(self.render_source_selector(cx)),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("Account Name"))
                                    .child(Input::new(&self.name_input)),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("Access Key ID"))
                                    .child(Input::new(&self.ak_input)),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("Secret Access Key"))
                                    .child(Input::new(&self.sk_input)),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("Region"))
                                    .child(Input::new(&self.region_input)),
                            ),
                    )
                    // Error message
                    .when_some(self.error.clone(), |el, error| {
                        el.child(div().text_sm().text_color(gpui::red()).child(error))
                    })
                    // Buttons
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .justify_end()
                            .child(Button::new("cancel").label("Cancel").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.hide_add_dialog(cx);
                                }),
                            ))
                            .child(Button::new("save").label("Save").primary().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.save_account(cx);
                                }),
                            )),
                    ),
            )
    }

    fn render_messages(&self, _cx: &Context<Self>) -> impl IntoElement {
        div()
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(gpui::red().opacity(0.1))
                        .text_color(gpui::red())
                        .child(error),
                )
            })
            .when_some(self.success.clone(), |el, success| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(gpui::green().opacity(0.1))
                        .text_color(gpui::green())
                        .child(success),
                )
            })
    }
}

impl Render for AccountsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .p_6()
            .v_flex()
            .gap_6()
            .bg(cx.theme().background)
            .child(self.render_header(cx))
            .child(self.render_messages(cx))
            .child(self.render_accounts_list(cx))
            .child(self.render_add_dialog(cx))
    }
}
