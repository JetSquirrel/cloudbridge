//! Cloud Account Management View

use chrono::Utc;
use gpui_kit::component::{
    button::*,
    input::{Input, InputState},
    scroll::ScrollableElement,
    *,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use std::collections::HashSet;
use uuid::Uuid;

use crate::cloud::registry::{self, SourceDescriptor};
use crate::cloud::CloudAccount;
use crate::db;
use crate::ui::theme::CardOutline as _;

use super::{data, fmt, theme};

actions!(accounts, [CloseAccountDialog]);

/// Key context for the add-account dialog, so Escape closes it.
const ACCOUNT_DIALOG_CONTEXT: &str = "AccountDialog";

/// Column widths (rem) shared by the accounts table header and its rows,
/// so the two cannot drift apart.
const COLUMN_REMS: [f32; 7] = [10.0, 10.0, 8.0, 5.0, 6.0, 8.0, 12.0];

/// Account Management View
pub struct AccountsView {
    /// Account list
    accounts: Vec<CloudAccount>,
    /// Real table/card data, once the background load has landed.
    accounts_data: Option<data::AccountsData>,
    /// A table/card data load is in flight; reloads do not pile on.
    loading_data: bool,
    /// A save is in flight; the dialog's Save button is disabled.
    saving: bool,
    /// Per-account actions in flight, by account id: the row's button is
    /// disabled and a second click is a no-op.
    validating_ids: HashSet<String>,
    importing_ids: HashSet<String>,
    deleting_ids: HashSet<String>,
    /// Whether to show add dialog
    show_add_dialog: bool,
    /// Error message
    error: Option<String>,
    /// Success message
    success: Option<String>,
    /// Neutral in-progress message, shown while an action runs
    info: Option<String>,
    /// Focus anchor the add dialog tracks, so Escape reaches it
    dialog_focus: FocusHandle,
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

        static BIND_KEYS: std::sync::Once = std::sync::Once::new();
        BIND_KEYS.call_once(|| {
            cx.bind_keys([KeyBinding::new(
                "escape",
                CloseAccountDialog,
                Some(ACCOUNT_DIALOG_CONTEXT),
            )]);
        });

        let mut view = Self {
            accounts: Vec::new(),
            accounts_data: None,
            loading_data: false,
            saving: false,
            validating_ids: HashSet::new(),
            importing_ids: HashSet::new(),
            deleting_ids: HashSet::new(),
            show_add_dialog: false,
            fill_status: None,
            error: None,
            success: None,
            info: None,
            dialog_focus: cx.focus_handle(),
            name_input,
            ak_input,
            sk_input,
            region_input,
            selected_source: default_source,
        };

        view.load_accounts(cx);
        view.load_data(cx);
        view
    }

    /// Load the configured-accounts list off the UI thread, like load_data
    /// does: the read opens the ledger database, which blocks.
    fn load_accounts(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(db::get_all_accounts)
                .await
                .map_err(|e| e.to_string());

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    match outcome {
                        Ok(accounts) => {
                            this.accounts = accounts;
                            this.error = None;
                        }
                        Err(e) => {
                            this.error = Some(format!("Failed to load accounts: {}", e));
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    /// Reload both the table/card data and the configured-accounts list.
    /// Called by the app shell when this page is navigated to; a no-op for
    /// the table data while a load is already in flight. Existing data
    /// stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.load_accounts(cx);
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
            });
        })
        .detach();
    }

    /// Re-normalize the raw store without re-fetching, then reload so the
    /// table and the raw-payloads card reflect the new ledger.
    fn replay_normalization(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        self.success = None;
        self.info = Some("Replaying normalization from the raw store...".to_string());
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
                            this.info = None;
                            this.load_data(cx);
                        }
                        Err(e) => {
                            this.error = Some(e);
                            this.success = None;
                            this.info = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    fn show_add_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_add_dialog = true;
        self.selected_source = registry::default_source();
        self.fill_status = None;
        self.error = None;
        self.success = None;
        self.info = None;
        self.dialog_focus.focus(window, cx);
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
                    .text_color(theme::danger(cx))
                    .child(format!("No credentials found in {}", places)),
            })
    }

    fn hide_add_dialog(&mut self, cx: &mut Context<Self>) {
        self.show_add_dialog = false;
        cx.notify();
    }

    fn save_account(&mut self, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }

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

        // The save writes the secret to the OS keyring, which blocks, so
        // it runs off the UI thread with the Save button disabled.
        self.saving = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(move || db::save_account(&account, &ak, &sk))
                .await
                .map_err(|e| e.to_string());

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.saving = false;
                    match outcome {
                        Ok(_) => {
                            this.success = Some("Account added successfully".to_string());
                            this.error = None;
                            this.info = None;
                            this.show_add_dialog = false;
                            this.load_accounts(cx);
                            this.load_data(cx);
                        }
                        Err(e) => {
                            this.error = Some(format!("Save failed: {}", e));
                            this.info = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    fn delete_account(&mut self, account_id: &str, cx: &mut Context<Self>) {
        // One delete per account at a time: the row's button is disabled
        // while this is in flight.
        let account_id = account_id.to_string();
        if !self.deleting_ids.insert(account_id.clone()) {
            return;
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let id = account_id.clone();
            let outcome = smol::unblock(move || db::delete_account(&id))
                .await
                .map_err(|e| e.to_string());

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.deleting_ids.remove(&account_id);
                    match outcome {
                        Ok(_) => {
                            this.success = Some("Account deleted".to_string());
                            this.info = None;
                            this.load_accounts(cx);
                            this.load_data(cx);
                        }
                        Err(e) => {
                            this.error = Some(format!("Delete failed: {}", e));
                            this.info = None;
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    fn validate_account(&mut self, account: &CloudAccount, cx: &mut Context<Self>) {
        let Some(descriptor) = account.descriptor() else {
            self.error = Some(format!(
                "Account {} uses an unknown billing source",
                account.name
            ));
            self.success = None;
            self.info = None;
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
                self.info = None;
                cx.notify();
                return;
            }
        };

        // One validation per account at a time: the row's button is
        // disabled while this is in flight.
        let account_id = account.id.clone();
        if !self.validating_ids.insert(account_id.clone()) {
            return;
        }

        // Show validating status
        self.info = Some(format!("Validating account {}...", account_name));
        self.error = None;
        self.success = None;
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
                    this.validating_ids.remove(&account_id);
                    // The account may have been deleted while the request
                    // was out; its result has no row to report against.
                    if this.accounts.iter().any(|a| a.id == account_id) {
                        this.info = None;
                        match validation_result {
                            Ok(true) => {
                                this.success = Some(format!(
                                    "Account {} validated successfully!",
                                    account_name
                                ));
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
                    }
                    cx.notify();
                })
                .ok();
            });
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
            self.info = None;
            cx.notify();
            return;
        };
        let Some(format) = descriptor.bill_file else {
            self.error = Some(format!(
                "{} publishes no bill export CloudBridge can read",
                descriptor.display_name
            ));
            self.success = None;
            self.info = None;
            cx.notify();
            return;
        };

        // One import per account at a time: the row's button is disabled
        // while the picker and the import are in flight.
        let account_id = account.id.clone();
        if !self.importing_ids.insert(account_id.clone()) {
            return;
        }

        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some("Import".into()),
        });

        self.error = None;
        self.success = None;
        self.info = Some(format!(
            "Choose the {} to import ({})",
            format.display_name,
            format.extension_hint()
        ));
        cx.notify();

        let account = account.clone();
        cx.spawn(async move |this, cx| {
            let outcome: Result<String, String> = async {
                let path = match chosen.await {
                    Ok(Ok(Some(paths))) => paths.into_iter().next(),
                    // Cancelled: leave the account list exactly as it was.
                    Ok(Ok(None)) | Err(_) => return Ok(String::new()),
                    Ok(Err(e)) => {
                        return Err(format!("Could not open the file picker: {}", e));
                    }
                };
                let Some(path) = path else {
                    return Ok(String::new());
                };

                let name = account.name.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(
                        crate::ingest::import_bill_file(&account, &path).map_err(|e| e.to_string()),
                    );
                });

                smol::unblock(move || {
                    rx.recv_timeout(std::time::Duration::from_secs(300))
                        .unwrap_or_else(|_| Err("The import timed out".to_string()))
                })
                .await
                .map(|outcome| {
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
                })
            }
            .await;

            let still_exists = cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.importing_ids.remove(&account_id);
                    cx.notify();
                    // The account may have been deleted while the
                    // picker or the import was out; its result has no
                    // row to report against.
                    this.accounts.iter().any(|a| a.id == account_id)
                })
                .unwrap_or(false)
            });

            if still_exists {
                report(&this, cx, outcome);
            }
        })
        .detach();
    }

    /// The billing-source picker: real buttons, so the choice is keyboard
    /// reachable and focus visible, with the selected source still
    /// controlled by the view.
    fn render_source_selector(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .h_flex()
            .gap_2()
            .flex_wrap()
            .children(registry::all().iter().map(|source| {
                let is_selected = source.id == self.selected_source.id;

                Button::new(SharedString::from(format!("source-{}", source.id)))
                    .label(source.short_name)
                    .small()
                    .when(is_selected, |button| {
                        button.custom(theme::accent_variant(cx))
                    })
                    .when(!is_selected, |button| {
                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_source(source, window, cx);
                    }))
            }))
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let account_count = self.accounts.len();

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
                            "{} accounts · credentials in the OS keyring, never in the database",
                            account_count
                        ),
                    )),
            )
            .child(
                Button::new("add")
                    .label("Add account")
                    .custom(theme::accent_variant(cx))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_add_dialog(window, cx);
                    })),
            )
    }

    /// The accounts table: a header row plus one row per configured source.
    /// Validate / Import / Delete live on the row, so this table is the
    /// single place an account appears.
    fn render_accounts_table(&self, cx: &Context<Self>) -> impl IntoElement {
        let card = theme::card(cx).w_full().p_5().v_flex().child(
            div()
                .w_full()
                .h_flex()
                .items_center()
                .gap_4()
                .pb_2()
                .child(theme::header_cell(cx, "ACCOUNT").w(rems(COLUMN_REMS[0])))
                .child(theme::header_cell(cx, "SOURCE").w(rems(COLUMN_REMS[1])))
                .child(theme::header_cell(cx, "REPORTS").w(rems(COLUMN_REMS[2])))
                .child(
                    theme::header_cell(cx, "MTD / BAL")
                        .w(rems(COLUMN_REMS[3]))
                        .text_right(),
                )
                .child(theme::header_cell(cx, "LAST FETCH").w(rems(COLUMN_REMS[4])))
                .child(theme::header_cell(cx, "STATE").w(rems(COLUMN_REMS[5])))
                .child(theme::header_cell(cx, "ACTIONS").w(rems(COLUMN_REMS[6]))),
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
            fmt::amount(row.mtd, currency)
        };

        let last_sync = row
            .last_sync
            .map(fmt::relative_time)
            .unwrap_or_else(|| "never".to_string());

        let detail_id = row.id.clone();

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
                    .w(rems(COLUMN_REMS[0]))
                    // The account name is the drill-down affordance: it
                    // opens the Account detail page.
                    .id(SharedString::from(format!("account-detail-{}", row.id)))
                    .cursor_pointer()
                    .v_flex()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::accent(cx))
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
                    )
                    .on_click(move |_, _, cx| {
                        crate::app::navigate_to_account(detail_id.clone(), cx)
                    }),
            )
            .child(
                div()
                    // Wide enough for "Amazon Web Services" on one line;
                    // nowrap + ellipsis instead of a 3-line wrap.
                    .w(rems(COLUMN_REMS[1]))
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(theme::text_primary(cx))
                    .child(row.provider.clone()),
            )
            .child(
                div()
                    .w(rems(COLUMN_REMS[2]))
                    .text_sm()
                    .text_color(theme::text_muted(cx))
                    .child(row.source_kind.clone()),
            )
            .child(
                div()
                    .w(rems(COLUMN_REMS[3]))
                    .text_right()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_primary(cx))
                    .child(mtd),
            )
            .child(
                div()
                    .w(rems(COLUMN_REMS[4]))
                    .text_sm()
                    .text_color(theme::text_muted(cx))
                    .child(last_sync),
            )
            .child(
                div()
                    .w(rems(COLUMN_REMS[5]))
                    .child(render_state(row.state, cx)),
            )
            .child(
                div()
                    .w(rems(COLUMN_REMS[6]))
                    .h_flex()
                    .gap_1()
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .when_some(account_for_validate, |el, account| {
                        el.child(
                            Button::new(SharedString::from(format!("validate-{}", validate_id)))
                                .label("Validate")
                                .ghost()
                                .small()
                                .disabled(self.validating_ids.contains(&row.id))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.validate_account(&account, cx);
                                })),
                        )
                    })
                    .when_some(account_for_import, |el, account| {
                        el.child(
                            Button::new(SharedString::from(format!("import-{}", import_id)))
                                .label("Import")
                                .ghost()
                                .small()
                                .disabled(self.importing_ids.contains(&row.id))
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
                            .disabled(self.deleting_ids.contains(&row.id))
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
            "CloudBridge has spent {} on paid API fetches this month, one call \
             per stale period.",
            fmt::amount(budget_spent, "USD")
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
            // Plain flex, not h_flex: h_flex centres the cards on the
            // cross axis, while the default stretch keeps both cards the
            // same height.
            .flex()
            .flex_row()
            .gap_4()
            .child(
                theme::card(cx)
                    .flex_1()
                    // min_w_0: the card must shrink below its content
                    // instead of pushing the row past the viewport.
                    .min_w_0()
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
                    // min_w_0: the raw-store path below is long and has
                    // unbreakable segments; without this the card forces
                    // the row wider than the viewport and is clipped.
                    .min_w_0()
                    .p_5()
                    .v_flex()
                    .gap_3()
                    .child(
                        div()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::text_primary(cx))
                            .child("Raw payloads on disk"),
                    )
                    // Middle ellipsis keeps the tail of the path visible no
                    // matter how deep the data directory is.
                    .child(theme::caption(cx, raw_body).text_ellipsis_middle())
                    .child(
                        div().child(
                            Button::new("replay-normalization")
                                .label("Replay normalization")
                                .outline()
                                .custom(
                                    ButtonCustomVariant::new(cx)
                                        .color(theme::accent(cx))
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
            .bg(theme::scrim(cx))
            .child(
                // Dialog content
                div()
                    .id("add-account-dialog")
                    .key_context(ACCOUNT_DIALOG_CONTEXT)
                    .track_focus(&self.dialog_focus)
                    .on_action(cx.listener(|this, _: &CloseAccountDialog, _, cx| {
                        this.hide_add_dialog(cx);
                        cx.stop_propagation();
                    }))
                    // When an input inside is focused, its own Escape
                    // binding wins the keystroke but re-propagates, so the
                    // raw key is caught here on the bubble.
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.hide_add_dialog(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .w_128()
                    .max_h(rems(37.5))
                    .p_6()
                    .rounded_xl()
                    .bg(theme::card_bg(cx))
                    .border_1()
                    .border_color(theme::card_border(cx))
                    .text_color(theme::text_primary(cx))
                    .shadow_lg()
                    .v_flex()
                    .gap_4()
                    .child(
                        div()
                            .flex_shrink_0()
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
                    // The body scrolls so Save and Cancel below stay
                    // reachable however short the window is.
                    .child(
                        div()
                            .id("add-account-body")
                            .flex_1()
                            .min_h_0()
                            .v_flex()
                            .gap_4()
                            .overflow_y_scroll()
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
                            // arrives through Import instead.
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
                                                 Import. ({})",
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
                        el.child(
                            div()
                                .flex_shrink_0()
                                .text_sm()
                                .text_color(theme::danger(cx))
                                .child(error),
                        )
                    })
                    // Buttons
                    .child(
                        div()
                            .flex_shrink_0()
                            .h_flex()
                            .gap_2()
                            .justify_end()
                            .child(Button::new("cancel").label("Cancel").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.hide_add_dialog(cx);
                                }),
                            ))
                            .child(
                                Button::new("save")
                                    .label("Save")
                                    .custom(theme::accent_variant(cx))
                                    .disabled(self.saving)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_account(cx);
                                    })),
                            ),
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
                        .bg(theme::danger_bg(cx))
                        .text_color(theme::danger(cx))
                        .child(error),
                )
            })
            .when_some(self.success.clone(), |el, success| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(theme::success_bg(cx))
                        .text_color(theme::success(cx))
                        .child(success),
                )
            })
            .when_some(self.info.clone(), |el, info| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(theme::card_bg(cx))
                        .text_color(theme::text_muted(cx))
                        .child(info),
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
                    view.info = None;
                }
                Ok(message) => {
                    view.success = Some(message);
                    view.error = None;
                    view.info = None;
                    // The ledger moved, and the row shows a sync time.
                    view.load_accounts(cx);
                    view.load_data(cx);
                }
                Err(e) => {
                    view.error = Some(e);
                    view.success = None;
                    view.info = None;
                }
            }
            cx.notify();
        })
        .ok();
    });
}

/// A prepaid balance in its own currency, e.g. `¥8.14 left`.
fn format_balance(amount: f64, currency: &str) -> String {
    format!("{} left", fmt::amount(amount, currency))
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

/// The STATE column: problems get a pill in their severity's colours,
/// untagged spend an outline pill, healthy plain muted text.
fn render_state(state: data::AccountState, cx: &App) -> Div {
    match state {
        data::AccountState::Anomaly => {
            theme::pill(state.label(), theme::alert_tint(cx), theme::danger(cx))
        }
        data::AccountState::LowBalance => theme::pill(
            state.label(),
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ),
        data::AccountState::UntaggedSpend => theme::pill_outline(cx, state.label()),
        data::AccountState::Healthy => div()
            .text_sm()
            .text_color(theme::text_muted(cx))
            .child(state.label()),
    }
}

impl Render for AccountsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .relative()
            .bg(theme::app_bg(cx))
            // The table grows one row per account, so the page owns a
            // scroll region. The dialog overlays the page and therefore
            // stays outside it.
            .child(
                div()
                    .size_full()
                    .v_flex()
                    .gap_6()
                    .p_8()
                    .overflow_y_scrollbar()
                    .child(self.render_header(cx))
                    .child(self.render_messages(cx))
                    .child(self.render_accounts_table(cx))
                    .child(self.render_bottom_cards(cx)),
            )
            .child(self.render_add_dialog(cx))
    }
}
