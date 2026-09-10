//! Cloud Account Management View

use chrono::{DateTime, Utc};
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

use super::{data, theme};

/// Account Management View
pub struct AccountsView {
    /// Account list
    accounts: Vec<CloudAccount>,
    /// Real table/card data, once the background load has landed.
    accounts_data: Option<data::AccountsData>,
    /// A table/card data load is in flight; reloads do not pile on.
    loading_data: bool,
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
    /// What the last "Fill from system" click found, if there has been one.
    fill_status: Option<FillStatus>,
}

/// The outcome of one attempt to fill the form from this machine.
enum FillStatus {
    /// Filled, from the variable or file named.
    Filled(String),
    /// Nothing found, having looked in the places named.
    NotFound(String),
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

        let default_source = registry::default_source();

        let mut view = Self {
            accounts: Vec::new(),
            accounts_data: None,
            loading_data: false,
            show_add_dialog: false,
            fill_status: None,
            error: None,
            success: None,
            name_input,
            ak_input,
            sk_input,
            region_input,
            selected_source: default_source,
        };

        view.load_accounts();
        view.load_data(cx);
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

    /// Reload both the table/card data and the configured-accounts list.
    /// Called by the app shell when this page is navigated to; a no-op for
    /// the table data while a load is already in flight. Existing data
    /// stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_accounts();
        if !self.loading_data {
            self.load_data(cx);
        }
    }

    /// Load the table and card data off the UI thread, like validation and
    /// import do: the loader reads the ledger, which blocks.
    fn load_data(&mut self, cx: &mut Context<Self>) {
        self.loading_data = true;
        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(data::load_accounts)
                .await
                .map_err(|e| e.to_string());

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.loading_data = false;
                    match outcome {
                        Ok(loaded) => {
                            this.accounts_data = Some(loaded);
                        }
                        Err(e) => {
                            this.error = Some(format!("Failed to load account data: {}", e));
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

    /// Re-normalize the raw store without re-fetching, then reload so the
    /// table and the raw-payloads card reflect the new ledger.
    fn replay_normalization(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.success = Some("Replaying normalization from the raw store...".to_string());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(data::replay_normalization)
                .await
                .map(|outcome| {
                    format!(
                        "Re-normalized {} period(s), {} charge(s) written",
                        outcome.periods, outcome.charges
                    )
                })
                .map_err(|e| e.to_string());

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    match outcome {
                        Ok(message) => {
                            this.success = Some(message);
                            this.error = None;
                            this.load_data(cx);
                        }
                        Err(e) => {
                            this.error = Some(e);
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

    fn show_add_dialog(&mut self, cx: &mut Context<Self>) {
        self.show_add_dialog = true;
        self.selected_source = registry::default_source();
        self.fill_status = None;
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
        // Whatever the last fill found was about the source being left.
        self.fill_status = None;

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

    /// Copy what this machine already has into the form.
    ///
    /// Read here, at the click, rather than kept from when the dialog
    /// opened: an app launched from Finder inherits no shell variables, so
    /// what is worth reading is the credentials file — and either of them
    /// can change while the dialog sits open.
    ///
    /// Only what is actually there is written: a missing secret or region
    /// leaves the field as it was, rather than blanking a value the user
    /// just typed. Finding nothing is reported rather than passed over in
    /// silence, since the places looked in are the answer to why.
    fn fill_from_system(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(found) = self.selected_source.credentials_from_system() else {
            self.fill_status = Some(FillStatus::NotFound(
                self.selected_source.credential_places().join(" or "),
            ));
            cx.notify();
            return;
        };

        self.ak_input.update(cx, |state, cx| {
            state.set_value(found.access_key, window, cx);
        });
        if let Some(secret_key) = found.secret_key {
            self.sk_input.update(cx, |state, cx| {
                state.set_value(secret_key, window, cx);
            });
        }
        if let Some(region) = found.region {
            self.region_input.update(cx, |state, cx| {
                state.set_value(region, window, cx);
            });
        }

        self.fill_status = Some(FillStatus::Filled(found.origin));
        cx.notify();
    }

    /// What the last fill found, once there has been one.
    fn render_fill_status(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .text_xs()
            .when_some(self.fill_status.as_ref(), |el, status| match status {
                FillStatus::Filled(origin) => el
                    .text_color(theme::text_muted(cx))
                    .child(format!("Filled from {}", origin)),
                FillStatus::NotFound(places) => el
                    .text_color(gpui::red())
                    .child(format!("No credentials found in {}", places)),
            })
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
        // A source whose bill can be imported from a file is usable with no
        // credentials at all, so an empty key is not an error there — it is
        // the normal case for one that has no billing API in this build.
        if ak.is_empty() && !self.selected_source.credentials_optional() {
            self.error = Some(format!(
                "Please enter the {}",
                self.selected_source.access_key_label
            ));
            cx.notify();
            return;
        }
        if sk.is_empty() && !ak.is_empty() && self.selected_source.needs_secret_key() {
            self.error = Some("Please enter Secret Access Key".to_string());
            cx.notify();
            return;
        }

        let account = CloudAccount {
            id: Uuid::new_v4().to_string(),
            name,
            source_id: self.selected_source.source_id(),
            region: if region.is_empty() {
                None
            } else {
                Some(region)
            },
            created_at: Utc::now(),
            last_synced_at: None,
            enabled: true,
            // Derived by the save, from the key it is given.
            access_key_hint: None,
        };

        match db::save_account(&account, &ak, &sk) {
            Ok(_) => {
                self.success = Some("Account added successfully".to_string());
                self.error = None;
                self.show_add_dialog = false;
                self.load_accounts();
                self.load_data(cx);
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
                self.load_data(cx);
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
        // The one moment a keyring read is warranted: the user asked for a
        // request to be signed.
        let context = match db::account_context(account, descriptor) {
            Ok(context) => context,
            Err(e) => {
                self.error = Some(format!("Cannot validate {}: {}", account_name, e));
                self.success = None;
                cx.notify();
                return;
            }
        };

        // Show validating status
        self.success = Some(format!("Validating account {}...", account_name));
        self.error = None;
        cx.notify();

        // Use standard thread to handle sync HTTP requests
        let (tx, rx) = std::sync::mpsc::channel::<Result<bool, String>>();

        std::thread::spawn(move || {
            let result = descriptor
                .client(context)
                .and_then(|source| source.validate_credentials())
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

    /// Import a bill export the user downloaded from their console.
    ///
    /// The file picker is opened here rather than a path being typed,
    /// because the file is a download whose name the user did not choose.
    /// The import itself is blocking — it reads a file, writes Parquet and
    /// replaces a month in the ledger — so it runs on a thread, like
    /// validation does.
    fn import_bill_file(&mut self, account: &CloudAccount, cx: &mut Context<Self>) {
        let Some(descriptor) = account.descriptor() else {
            self.error = Some(format!(
                "Account {} uses an unknown billing source",
                account.name
            ));
            self.success = None;
            cx.notify();
            return;
        };
        let Some(format) = descriptor.bill_file else {
            self.error = Some(format!(
                "{} publishes no bill export CloudBridge can read",
                descriptor.display_name
            ));
            self.success = None;
            cx.notify();
            return;
        };

        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });

        self.error = None;
        self.success = Some(format!(
            "Choose the {} to import ({})",
            format.display_name,
            format.extension_hint()
        ));
        cx.notify();

        let account = account.clone();
        cx.spawn(async move |this, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                // Cancelled: leave the account list exactly as it was.
                Ok(Ok(None)) | Err(_) => None,
                Ok(Err(e)) => {
                    report(
                        &this,
                        cx,
                        Err(format!("Could not open the file picker: {}", e)),
                    );
                    return;
                }
            };
            let Some(path) = path else {
                report(&this, cx, Ok(String::new()));
                return;
            };

            let name = account.name.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(
                    crate::ingest::import_bill_file(&account, &path).map_err(|e| e.to_string()),
                );
            });

            let outcome = smol::unblock(move || {
                rx.recv_timeout(std::time::Duration::from_secs(300))
                    .unwrap_or_else(|_| Err("The import timed out".to_string()))
            })
            .await;

            report(
                &this,
                cx,
                outcome.map(|outcome| {
                    // Say which months were replaced, not merely that
                    // something was imported: the export supersedes those
                    // months rather than adding to them.
                    format!(
                        "Imported {} charge(s) into {} from its {}, replacing {}",
                        outcome.charges(),
                        name,
                        outcome.format,
                        outcome.period_labels()
                    )
                }),
            );
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
                        el.bg(theme::accent(cx)).text_color(theme::on_accent(cx))
                    })
                    .when(!is_selected, |el| {
                        el.bg(theme::card_border(cx))
                            .text_color(theme::text_primary(cx))
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
        let source_count = self.accounts.len();

        div()
            .w_full()
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, "Accounts"))
                    .child(theme::caption(
                        cx,
                        format!(
                            "{} sources · credentials in the OS keyring, never in the database",
                            source_count
                        ),
                    )),
            )
            .child(
                Button::new("add")
                    .label("Add account")
                    .custom(
                        ButtonCustomVariant::new(cx)
                            .color(theme::accent(cx))
                            .foreground(theme::on_accent(cx))
                            .hover(theme::accent_hover(cx))
                            .active(theme::accent_hover(cx)),
                    )
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_add_dialog(cx);
                    })),
            )
    }

    /// The accounts table: a header row plus one row per configured source.
    /// Validate / Import bill / Delete live on the row, so this table is the
    /// single place an account appears.
    fn render_accounts_table(&self, cx: &Context<Self>) -> impl IntoElement {
        let card = theme::card(cx).w_full().p_5().v_flex().child(
            div()
                .w_full()
                .h_flex()
                .items_center()
                .gap_4()
                .pb_2()
                .child(table_header_cell(cx, "ACCOUNT", COL_ACCOUNT))
                .child(table_header_cell(cx, "SOURCE", COL_SOURCE))
                .child(table_header_cell(cx, "REPORTS", COL_REPORTS))
                .child(table_header_cell(cx, "MTD", COL_MTD))
                .child(table_header_cell(cx, "LAST FETCH", COL_FETCH))
                .child(table_header_cell(cx, "STATE", COL_STATE))
                .child(table_header_cell(cx, "ACTIONS", COL_ACTIONS)),
        );

        match &self.accounts_data {
            None => card.child(
                div()
                    .w_full()
                    .py_6()
                    .flex()
                    .justify_center()
                    .child(theme::caption(cx, "Loading accounts…")),
            ),
            Some(loaded) if loaded.accounts.is_empty() => card.child(
                div()
                    .w_full()
                    .py_6()
                    .flex()
                    .justify_center()
                    .child(theme::caption(
                        cx,
                        "No cloud accounts yet — click Add account above to connect a source.",
                    )),
            ),
            Some(loaded) => card.children(
                loaded
                    .accounts
                    .iter()
                    .map(|row| self.render_table_row(row, &loaded.currency, cx)),
            ),
        }
    }

    fn render_table_row(
        &self,
        row: &data::AccountRowData,
        currency: &str,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // The management actions need the stored account the row came from.
        let account = self.accounts.iter().find(|a| a.id == row.id).cloned();
        let descriptor = account.as_ref().and_then(|a| a.descriptor());
        let account_for_validate = if descriptor.is_some_and(|source| source.fetches_from_api()) {
            account.clone()
        } else {
            None
        };
        let account_for_import = if descriptor.is_some_and(|source| source.imports_bill_file()) {
            account.clone()
        } else {
            None
        };
        let validate_id = row.id.clone();
        let import_id = row.id.clone();
        let delete_id = row.id.clone();

        // Balance-reporting sources have no MTD to show; their number is
        // the balance itself.
        let mtd = if row.source_kind == "Balance only" {
            match &row.balance {
                Some((amount, currency)) => format_balance(*amount, currency),
                None => "—".to_string(),
            }
        } else {
            format_money(row.mtd, currency)
        };

        let last_sync = row
            .last_sync
            .map(format_last_sync)
            .unwrap_or_else(|| "never".to_string());

        div()
            .w_full()
            .h_flex()
            .items_center()
            .gap_4()
            .py_3()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .child(
                div()
                    .w(px(COL_ACCOUNT))
                    .v_flex()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_primary(cx))
                            .child(row.name.clone()),
                    )
                    // Absent for an account stored before the hint was
                    // recorded; it appears the next time the account's
                    // credentials are actually used.
                    .when_some(
                        account
                            .as_ref()
                            .and_then(|a| a.masked_access_key())
                            .map(|masked| format!("AK: {}", masked)),
                        |el, masked| {
                            el.child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted(cx))
                                    .child(masked),
                            )
                        },
                    ),
            )
            .child(
                div()
                    .w(px(COL_SOURCE))
                    .text_color(theme::text_primary(cx))
                    .child(row.provider.clone()),
            )
            .child(
                div()
                    .w(px(COL_REPORTS))
                    .text_sm()
                    .text_color(theme::text_muted(cx))
                    .child(row.source_kind.clone()),
            )
            .child(
                div()
                    .w(px(COL_MTD))
                    .text_color(theme::text_primary(cx))
                    .child(mtd),
            )
            .child(
                div()
                    .w(px(COL_FETCH))
                    .text_sm()
                    .text_color(theme::text_muted(cx))
                    .child(last_sync),
            )
            .child(div().w(px(COL_STATE)).child(render_state(row.state, cx)))
            .child(
                div()
                    .w(px(COL_ACTIONS))
                    .h_flex()
                    .gap_1()
                    .flex_wrap()
                    .when_some(account_for_validate, |el, account| {
                        el.child(
                            Button::new(SharedString::from(format!("validate-{}", validate_id)))
                                .label("Validate")
                                .ghost()
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.validate_account(&account, cx);
                                })),
                        )
                    })
                    .when_some(account_for_import, |el, account| {
                        el.child(
                            Button::new(SharedString::from(format!("import-{}", import_id)))
                                .label("Import bill")
                                .ghost()
                                .small()
                                .tooltip(
                                    "Read a bill export downloaded from this \
                                     provider's console. It replaces every month \
                                     the file covers.",
                                )
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.import_bill_file(&account, cx);
                                })),
                        )
                    })
                    .child(
                        Button::new(SharedString::from(format!("delete-{}", delete_id)))
                            .label("Delete")
                            .danger()
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.delete_account(&delete_id, cx);
                            })),
                    ),
            )
    }

    /// The two cards under the table: paid-API budget and raw payloads.
    fn render_bottom_cards(&self, cx: &Context<Self>) -> impl IntoElement {
        let (budget_used, budget_ceiling, budget_spent) = self
            .accounts_data
            .as_ref()
            .map(|loaded| {
                (
                    loaded.budget.used,
                    loaded.budget.ceiling,
                    loaded.budget.spent,
                )
            })
            .unwrap_or((0, data::API_CALL_CEILING, 0.0));
        let budget_fill = (budget_used as f32 / budget_ceiling.max(1) as f32).clamp(0.0, 1.0);
        let budget_body = format!(
            "AWS Cost Explorer charges $0.01 per request. CloudBridge has spent \
             ${:.2} on fetches this month across {} calls, one per stale period.",
            budget_spent, budget_used
        );

        let raw_body = match &self.accounts_data {
            Some(loaded) => format!(
                "{} of Parquet under {}. A mapping fix replays these instead of \
                 paying for another fetch.",
                format_bytes(loaded.raw.bytes),
                loaded.raw.path.display()
            ),
            None => "Reading the raw store…".to_string(),
        };

        div()
            .w_full()
            .h_flex()
            .gap_4()
            .child(
                theme::card(cx)
                    .flex_1()
                    .p_5()
                    .v_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::text_primary(cx))
                            .child("Paid API budget"),
                    )
                    .child(theme::caption(cx, budget_body))
                    .child(
                        div()
                            .w_full()
                            .h_2()
                            .rounded_full()
                            .bg(theme::card_border(cx))
                            .child(
                                div()
                                    .h_full()
                                    .w(relative(budget_fill))
                                    .rounded_full()
                                    .bg(theme::olive(cx)),
                            ),
                    )
                    .child(theme::caption(
                        cx,
                        format!(
                            "{} of a {}-call monthly ceiling",
                            budget_used, budget_ceiling
                        ),
                    )),
            )
            .child(
                theme::card(cx)
                    .flex_1()
                    .p_5()
                    .v_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::text_primary(cx))
                            .child("Raw payloads on disk"),
                    )
                    .child(theme::caption(cx, raw_body))
                    .child(
                        div().child(
                            Button::new("replay-normalization")
                                .label("Replay normalization")
                                .outline()
                                .custom(
                                    ButtonCustomVariant::new(cx)
                                        .color(theme::accent(cx))
                                        .border(theme::accent(cx))
                                        .hover(theme::alert_tint(cx))
                                        .active(theme::alert_tint(cx)),
                                )
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.replay_normalization(cx);
                                })),
                        ),
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
                    .bg(theme::card_bg(cx))
                    .border_1()
                    .border_color(theme::card_border(cx))
                    .text_color(theme::text_primary(cx))
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
                            // A source with no billing API in this build has
                            // nothing to sign, so the form does not ask for a
                            // secret it would only file away unused. Its bill
                            // arrives through Import bill instead.
                            .when(self.selected_source.fetches_from_api(), |el| {
                                el.child(
                                    div()
                                        .v_flex()
                                        .gap_1()
                                        .child(
                                            div()
                                                .text_sm()
                                                .child(self.selected_source.access_key_label),
                                        )
                                        .child(Input::new(&self.ak_input)),
                                )
                                .when_some(self.selected_source.secret_key_label, |el, label| {
                                    el.child(
                                        div()
                                            .v_flex()
                                            .gap_1()
                                            .child(div().text_sm().child(label))
                                            .child(Input::new(&self.sk_input)),
                                    )
                                })
                                .when(
                                    self.selected_source.default_region.is_some(),
                                    |el| {
                                        el.child(
                                            div()
                                                .v_flex()
                                                .gap_1()
                                                .child(div().text_sm().child("Region"))
                                                .child(Input::new(&self.region_input)),
                                        )
                                    },
                                )
                            })
                            .when_some(
                                self.selected_source
                                    .bill_file
                                    .filter(|_| !self.selected_source.fetches_from_api()),
                                |el, format| {
                                    el.child(
                                        div().text_sm().text_color(theme::text_muted(cx)).child(
                                            format!(
                                                "No credentials needed: this source is read \
                                                 from its {}. Save the account, then use \
                                                 Import bill. ({})",
                                                format.display_name, format.origin_hint
                                            ),
                                        ),
                                    )
                                },
                            )
                            // Offered whether or not this process can see
                            // a key, because it cannot know until it looks:
                            // an app launched from Finder inherits no shell
                            // variables, and the credentials file is read
                            // only when the button is clicked.
                            .when(self.selected_source.local_credentials.is_some(), |el| {
                                el.child(
                                    div()
                                        .h_flex()
                                        .gap_2()
                                        .justify_between()
                                        .items_center()
                                        .child(self.render_fill_status(cx))
                                        .child(
                                            Button::new("fill-from-system")
                                                .label("Fill from system")
                                                .ghost()
                                                .tooltip(
                                                    "Read the credentials this machine \
                                                         already has, from the environment or \
                                                         the provider's credentials file",
                                                )
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.fill_from_system(window, cx);
                                                })),
                                        ),
                                )
                            }),
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

    fn render_messages(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(theme::alert_tint(cx))
                        .text_color(theme::accent(cx))
                        .child(error),
                )
            })
            .when_some(self.success.clone(), |el, success| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(theme::warning_bg(cx))
                        .text_color(theme::warning_text(cx))
                        .child(success),
                )
            })
    }
}

/// Put the outcome of a background action on screen.
///
/// An `Ok` with an empty message is a cancellation: the messages are
/// cleared and nothing is claimed to have happened.
fn report(view: &WeakEntity<AccountsView>, cx: &mut AsyncApp, outcome: Result<String, String>) {
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            match outcome {
                Ok(message) if message.is_empty() => {
                    view.success = None;
                    view.error = None;
                }
                Ok(message) => {
                    view.success = Some(message);
                    view.error = None;
                    // The ledger moved, and the row shows a sync time.
                    view.load_accounts();
                    view.load_data(cx);
                }
                Err(e) => {
                    view.error = Some(e);
                    view.success = None;
                }
            }
            cx.notify();
        })
        .ok();
    })
    .ok();
}

/// Column widths of the accounts table, shared by the header and the rows.
const COL_ACCOUNT: f32 = 170.0;
const COL_SOURCE: f32 = 100.0;
const COL_REPORTS: f32 = 120.0;
const COL_MTD: f32 = 80.0;
const COL_FETCH: f32 = 90.0;
const COL_STATE: f32 = 110.0;
const COL_ACTIONS: f32 = 170.0;

/// Symbol prefix for the currencies the sources bill in.
fn currency_symbol(currency: &str) -> &str {
    match currency {
        "USD" => "$",
        "CNY" => "¥",
        _ => "",
    }
}

/// Thousands separators on a whole-unit amount.
fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        format!("-{}", out)
    } else {
        out
    }
}

/// Whole-unit amount with the currency's symbol, e.g. `$18,420`.
fn format_money(amount: f64, currency: &str) -> String {
    let symbol = currency_symbol(currency);
    if symbol.is_empty() {
        format!("{} {}", thousands(amount.round() as i64), currency)
    } else {
        format!("{}{}", symbol, thousands(amount.round() as i64))
    }
}

/// A prepaid balance in its own currency, e.g. `¥8.14 left`.
fn format_balance(amount: f64, currency: &str) -> String {
    let symbol = currency_symbol(currency);
    if symbol.is_empty() {
        format!("{:.2} {} left", amount, currency)
    } else {
        format!("{}{:.2} left", symbol, amount)
    }
}

/// A sync time as an age, e.g. `14 min ago`.
fn format_last_sync(at: DateTime<Utc>) -> String {
    let secs = (Utc::now() - at).num_seconds().max(0);
    if secs < 60 {
        "just now".to_string()
    } else if secs < 3_600 {
        format!("{} min ago", secs / 60)
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3_600)
    } else {
        format!("{} d ago", secs / 86_400)
    }
}

/// A byte size as GB, MB, KB, or B, whichever reads whole-ish.
fn format_bytes(bytes: u64) -> String {
    const GB: u64 = 1 << 30;
    const MB: u64 = 1 << 20;
    const KB: u64 = 1 << 10;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{} KB", bytes / KB)
    } else {
        format!("{} B", bytes)
    }
}

/// One header cell of the accounts table: small, muted, semibold caps.
fn table_header_cell(cx: &App, text: &'static str, width: f32) -> Div {
    div()
        .w(px(width))
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::text_muted(cx))
        .child(text)
}

/// The STATE column: a tint pill for healthy, an accent outline pill for
/// untagged spend, plain text otherwise.
fn render_state(state: data::AccountState, cx: &App) -> Div {
    match state {
        data::AccountState::Healthy => theme::pill(
            state.label(),
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ),
        data::AccountState::UntaggedSpend => div()
            .px_2()
            .py_0p5()
            .rounded_full()
            .border_1()
            .border_color(theme::accent(cx))
            .text_xs()
            .text_color(theme::accent(cx))
            .child(state.label()),
        _ => div()
            .text_color(theme::text_primary(cx))
            .child(state.label()),
    }
}

impl Render for AccountsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .p_8()
            .v_flex()
            .gap_6()
            .bg(theme::app_bg(cx))
            .child(self.render_header(cx))
            .child(self.render_messages(cx))
            .child(self.render_accounts_table(cx))
            .child(self.render_bottom_cards(cx))
            .child(self.render_add_dialog(cx))
    }
}
