//! The Reconciliation page: does each provider's final bill match what was
//! imported? One table across accounts, problems first, with a row opening in
//! place onto why.
//!
//! A row is never blank and never reads as a success by default: a month with
//! charges and no invoice is `Missing evidence`, not an empty line.

use std::collections::HashMap;

use anyhow::Result;
use chrono::{NaiveDate, Utc};
use gpui_kit::component::dialog::{AlertDialog, Cancel, Dialog, DialogButtonProps, DialogFooter};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{dialogs, fmt, theme};
#[cfg(not(target_family = "wasm"))]
use crate::analytics::reconciliation::{self, CsvError};
use crate::analytics::reconciliation::{
    group_invoices, invoice_from, neighbouring_periods, reconcile, Evidence, InvoiceDraft,
    InvoiceError,
};
use crate::analytics::snapshot;
use crate::ledger::invoice::{all_invoices, billed_totals, delete_invoice, save_invoices};
use crate::ledger::snapshot::{current_lines, periods_of, snapshot_lines, snapshots_of};
use crate::model::{
    BillingSnapshot, ChargeCategory, Invoice, InvoiceSource, PeriodKey, ReconcileReason,
    ReconcileStatus, Reconciliation,
};
use crate::ui::theme::CardOutline as _;

/// How many of an account's ended months can show as missing evidence. An
/// older gap is not something anyone is about to act on, and it would bury
/// the rows that are.
const MISSING_MONTHS: usize = 3;

/// How wide the amount columns are, so they share a spine down the page.
const AMOUNT_WIDTH: f32 = 7.5;

/// An account the page can record an invoice against.
#[derive(Clone)]
struct AccountChoice {
    provider: String,
    account_id: String,
    name: String,
}

/// One line of the table: a group of invoices and its comparison with the
/// ledger, or a month with charges and no invoice.
#[derive(Clone)]
struct Row {
    key: PeriodKey,
    account_name: String,
    currency: String,
    invoices: Vec<Invoice>,
    ledger_total: Option<f64>,
    invoice_total: Option<f64>,
    difference: Option<f64>,
    status: ReconcileStatus,
}

impl Row {
    /// What this row is, whatever order it sorts into.
    fn id(&self) -> String {
        let scope = self
            .invoices
            .first()
            .and_then(|invoice| invoice.billing_account_id.as_deref())
            .unwrap_or("");
        format!(
            "{}/{}/{}/{}/{}",
            self.key.provider, self.key.account_id, self.key.billing_period, self.currency, scope
        )
    }

    fn reference(&self) -> String {
        self.invoices
            .iter()
            .map(|invoice| invoice.invoice_ref.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A sortable column of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Month,
    Account,
    Invoice,
    Ledger,
    Difference,
}

/// The row a selected row opens onto.
struct OpenRow {
    id: String,
    /// The snapshot compared with; `None` is the current ledger.
    basis: Option<String>,
    revisions: Vec<BillingSnapshot>,
    result: Option<Reconciliation>,
}

struct PageData {
    accounts: Vec<AccountChoice>,
    rows: Vec<Row>,
}

/// Compare a group of invoices with the ledger or one of its snapshots.
/// Blocking.
fn compare(invoices: &[Invoice], basis: Option<&BillingSnapshot>) -> Result<Reconciliation> {
    let first = &invoices[0];
    let (lines, period_open) = match basis {
        Some(frozen) => (snapshot_lines(&frozen.snapshot_id)?, frozen.period_open),
        None => (
            current_lines(&first.key)?,
            snapshot::period_is_open(&first.key.billing_period, Utc::now())?,
        ),
    };
    let mut neighbours = Vec::new();
    for period in neighbouring_periods(&first.key.billing_period) {
        let key = PeriodKey::new(
            first.key.provider.clone(),
            first.key.account_id.clone(),
            period,
        );
        for (currency, total) in billed_totals(&key)? {
            if currency == first.currency {
                neighbours.push((key.billing_period.clone(), total));
            }
        }
    }
    Ok(reconcile(
        invoices,
        &Evidence {
            lines: &lines,
            period_open,
            neighbours: &neighbours,
        },
    ))
}

/// Read the page. Blocking; it runs in `smol::unblock`.
fn load_page() -> Result<PageData> {
    let accounts: Vec<AccountChoice> = crate::db::get_all_accounts()?
        .into_iter()
        .map(|account| AccountChoice {
            provider: account.source_id.as_str().to_string(),
            account_id: account.id,
            name: account.name,
        })
        .collect();
    let names: HashMap<&str, &str> = accounts
        .iter()
        .map(|account| (account.account_id.as_str(), account.name.as_str()))
        .collect();
    let name_of = |id: &str| {
        names
            .get(id)
            .map_or_else(|| id.to_string(), |n| n.to_string())
    };

    let mut rows = Vec::new();
    for group in group_invoices(all_invoices()?) {
        let result = compare(&group, None)?;
        rows.push(Row {
            key: group[0].key.clone(),
            account_name: name_of(&group[0].key.account_id),
            currency: result.currency,
            ledger_total: result.ledger_total,
            invoice_total: Some(result.invoice_total),
            difference: result.difference,
            status: result.status,
            invoices: group,
        });
    }

    let now = Utc::now();
    let mut missing = Vec::new();
    for account in &accounts {
        let ended = periods_of(&account.account_id)?
            .into_iter()
            .filter(|key| !snapshot::period_is_open(&key.billing_period, now).unwrap_or(true))
            .take(MISSING_MONTHS);
        for key in ended {
            for (currency, total) in billed_totals(&key)? {
                let covered = rows
                    .iter()
                    .any(|row: &Row| row.key == key && row.currency == currency);
                if !covered {
                    missing.push(Row {
                        key: key.clone(),
                        account_name: account.name.clone(),
                        currency,
                        invoices: Vec::new(),
                        ledger_total: Some(total),
                        invoice_total: None,
                        difference: None,
                        status: ReconcileStatus::MissingEvidence,
                    });
                }
            }
        }
    }
    rows.extend(missing);
    Ok(PageData { accounts, rows })
}

/// Problems first: what is wrong, then what cannot be checked, then what has
/// not been entered, and only then what is fine.
fn status_rank(status: ReconcileStatus) -> u8 {
    match status {
        ReconcileStatus::Difference => 0,
        ReconcileStatus::CannotVerify => 1,
        ReconcileStatus::MissingEvidence => 2,
        ReconcileStatus::Matched => 3,
    }
}

fn row_matches(row: &Row, text: &str, status: Option<ReconcileStatus>) -> bool {
    if status.is_some_and(|status| status != row.status) {
        return false;
    }
    let text = text.trim().to_lowercase();
    text.is_empty()
        || [
            row.key.provider.as_str(),
            row.key.account_id.as_str(),
            row.account_name.as_str(),
            row.key.billing_period.as_str(),
            row.currency.as_str(),
            row.reference().as_str(),
        ]
        .iter()
        .any(|field| field.to_lowercase().contains(&text))
}

/// Order the table. `None` is the default: status, then newest month. An
/// absent figure sorts last either way, because it is not a small one.
fn sort_rows(rows: &mut [Row], sort: Option<(Column, bool)>) {
    let identity = |a: &Row, b: &Row| a.id().cmp(&b.id());
    let Some((column, descending)) = sort else {
        rows.sort_by(|a, b| {
            status_rank(a.status)
                .cmp(&status_rank(b.status))
                .then_with(|| b.key.billing_period.cmp(&a.key.billing_period))
                .then_with(|| identity(a, b))
        });
        return;
    };
    let direct = |order: std::cmp::Ordering| if descending { order.reverse() } else { order };
    let figure = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(a), Some(b)) => direct(a.total_cmp(&b)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    };
    rows.sort_by(|a, b| {
        let order = match column {
            Column::Month => direct(a.key.billing_period.cmp(&b.key.billing_period)),
            Column::Account => direct(a.account_name.cmp(&b.account_name)),
            Column::Invoice => figure(a.invoice_total, b.invoice_total),
            Column::Ledger => figure(a.ledger_total, b.ledger_total),
            Column::Difference => figure(a.difference, b.difference),
        };
        order.then_with(|| identity(a, b))
    });
}

fn status_label(status: ReconcileStatus) -> SharedString {
    match status {
        ReconcileStatus::Matched => tr!("reconciliation.status_matched"),
        ReconcileStatus::Difference => tr!("reconciliation.status_difference"),
        ReconcileStatus::CannotVerify => tr!("reconciliation.status_cannot_verify"),
        ReconcileStatus::MissingEvidence => tr!("reconciliation.status_missing"),
    }
}

/// Text and icon carry the status; color only repeats it.
fn status_badge(status: ReconcileStatus, cx: &App) -> Div {
    let (icon, bg, fg) = match status {
        ReconcileStatus::Matched => (
            IconName::CircleCheck,
            theme::success_bg(cx),
            theme::success(cx),
        ),
        ReconcileStatus::Difference => (
            IconName::TriangleAlert,
            theme::danger_bg(cx),
            theme::danger(cx),
        ),
        ReconcileStatus::CannotVerify => (
            IconName::CircleX,
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ),
        ReconcileStatus::MissingEvidence => {
            (IconName::Info, theme::sidebar_bg(cx), theme::text_muted(cx))
        }
    };
    div()
        .h_flex()
        .items_center()
        .gap_1()
        .child(Icon::new(icon).small().text_color(fg))
        .child(theme::pill(status_label(status), bg, fg))
}

fn date_text(date: Option<NaiveDate>) -> String {
    date.map_or_else(
        || "—".to_string(),
        |date| date.format("%Y-%m-%d").to_string(),
    )
}

/// What a reason means and what to do about it.
fn reason_text(reason: &ReconcileReason) -> SharedString {
    match reason {
        ReconcileReason::NoLedgerRows => tr!("reconciliation.reason_no_rows"),
        ReconcileReason::CurrencyMismatch => tr!("reconciliation.reason_currency"),
        ReconcileReason::NoAmounts => tr!("reconciliation.reason_no_amounts"),
        ReconcileReason::PeriodOpen => tr!("reconciliation.reason_open"),
        ReconcileReason::NonAuthoritative { rows } => {
            tr!("reconciliation.reason_non_authoritative", rows = rows)
        }
        ReconcileReason::MixedBillingAccounts { accounts } => {
            tr!(
                "reconciliation.reason_billing_accounts",
                accounts = accounts
            )
        }
        ReconcileReason::PartialCoverage { from, through } => tr!(
            "reconciliation.reason_partial",
            from = date_text(*from),
            through = date_text(*through)
        ),
        ReconcileReason::TaxDiffers => tr!("reconciliation.reason_tax"),
        ReconcileReason::CreditsDiffer => tr!("reconciliation.reason_credits"),
        ReconcileReason::AdjustmentsDiffer => tr!("reconciliation.reason_adjustments"),
        ReconcileReason::MonthMismatch { period } => {
            tr!("reconciliation.reason_month", period = period)
        }
        ReconcileReason::UsageWithoutAmount { rows } => {
            tr!("reconciliation.reason_usage", rows = rows)
        }
        ReconcileReason::OtherCurrency { currency, rows } => tr!(
            "reconciliation.reason_other_currency",
            currency = currency,
            rows = rows
        ),
    }
}

fn invoice_error_text(error: &InvoiceError) -> SharedString {
    match error {
        InvoiceError::MissingField(field) => tr!("reconciliation.error_missing", field = field),
        InvoiceError::BadPeriod(value) => tr!("reconciliation.error_period", value = value),
        InvoiceError::BadCurrency(value) => tr!("reconciliation.error_currency", value = value),
        InvoiceError::BadAmount { field, value } => {
            tr!("reconciliation.error_amount", field = field, value = value)
        }
        InvoiceError::PositiveCredit(value) => tr!("reconciliation.error_credit", value = value),
    }
}

#[cfg(not(target_family = "wasm"))]
fn csv_error_text(error: &CsvError) -> SharedString {
    match error {
        CsvError::Empty => tr!("reconciliation.csv_empty"),
        CsvError::MissingColumn(column) => tr!("reconciliation.csv_column", column = column),
        CsvError::WrongFieldCount { line } => tr!("reconciliation.csv_fields", line = line),
        CsvError::Row { line, error } => tr!(
            "reconciliation.csv_row",
            line = line,
            error = invoice_error_text(error)
        ),
    }
}

fn category_label(category: ChargeCategory) -> SharedString {
    match category {
        ChargeCategory::Usage => tr!("snapshots.category_usage"),
        ChargeCategory::Purchase => tr!("snapshots.category_purchase"),
        ChargeCategory::Credit => tr!("snapshots.category_credit"),
        ChargeCategory::Tax => tr!("snapshots.category_tax"),
        ChargeCategory::Adjustment => tr!("snapshots.category_adjustment"),
    }
}

fn amount_text(value: Option<f64>, currency: &str) -> SharedString {
    value.map_or_else(
        || "—".into(),
        |value| fmt::exact_amount(value, currency).into(),
    )
}

fn signed_text(value: Option<f64>, currency: &str) -> SharedString {
    value.map_or_else(
        || "—".into(),
        |value| fmt::signed_exact_amount(value, currency).into(),
    )
}

/// The page.
pub struct ReconciliationView {
    accounts: Vec<AccountChoice>,
    rows: Vec<Row>,
    loaded: bool,
    loading: bool,
    error: Option<SharedString>,
    notice: Option<SharedString>,
    generation: u64,

    filter: Entity<InputState>,
    status_filter: Option<ReconcileStatus>,
    sort: Option<(Column, bool)>,
    open: Option<OpenRow>,
    detail_generation: u64,

    form_account: Option<String>,
    form_period: Entity<InputState>,
    form_reference: Entity<InputState>,
    form_currency: Entity<InputState>,
    form_amount: Entity<InputState>,
    form_tax: Entity<InputState>,
    form_credits: Entity<InputState>,
    form_adjustments: Entity<InputState>,
    form_billing_account: Entity<InputState>,
    form_error: Option<SharedString>,
    saving: bool,
    pending_delete: Option<Invoice>,
    #[cfg(not(target_family = "wasm"))]
    importing: bool,

    _subscriptions: Vec<Subscription>,
}

impl ReconciliationView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| {
            InputState::new(window, cx).placeholder(tr!("reconciliation.filter_placeholder"))
        });
        let subscription = cx.subscribe(&filter, |_, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let mut input = |placeholder: SharedString| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let form_period = input("YYYY-MM".into());
        let form_reference = input(tr!("reconciliation.ph_reference"));
        let form_currency = input("USD".into());
        let form_amount = input("0.00".into());
        let form_tax = input(tr!("reconciliation.ph_optional"));
        let form_credits = input(tr!("reconciliation.ph_credits"));
        let form_adjustments = input(tr!("reconciliation.ph_optional"));
        let form_billing_account = input(tr!("reconciliation.ph_optional"));
        Self {
            accounts: Vec::new(),
            rows: Vec::new(),
            loaded: false,
            loading: false,
            error: None,
            notice: None,
            generation: 0,
            filter,
            status_filter: None,
            sort: None,
            open: None,
            detail_generation: 0,
            form_account: None,
            form_period,
            form_reference,
            form_currency,
            form_amount,
            form_tax,
            form_credits,
            form_adjustments,
            form_billing_account,
            form_error: None,
            saving: false,
            pending_delete: None,
            #[cfg(not(target_family = "wasm"))]
            importing: false,
            _subscriptions: vec![subscription],
        }
    }

    /// Reload the page. Called by the shell when it is navigated to.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if !self.loading {
            self.load(cx);
        }
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(load_page).await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                this.loaded = true;
                match result {
                    Ok(data) => {
                        this.accounts = data.accounts;
                        this.rows = data.rows;
                        this.error = None;
                        this.refresh_open(cx);
                    }
                    Err(e) => this.error = Some(tr!("reconciliation.load_failed", error = e)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn toggle_sort(&mut self, column: Column, cx: &mut Context<Self>) {
        let first_descending = !matches!(column, Column::Account);
        self.sort = match self.sort {
            Some((current, descending)) if current == column => {
                (descending == first_descending).then_some((column, !descending))
            }
            _ => Some((column, first_descending)),
        };
        cx.notify();
    }

    fn toggle_row(&mut self, id: String, cx: &mut Context<Self>) {
        if self.open.as_ref().is_some_and(|open| open.id == id) {
            self.open = None;
            cx.notify();
            return;
        }
        self.open = Some(OpenRow {
            id,
            basis: None,
            revisions: Vec::new(),
            result: None,
        });
        self.load_open(cx);
    }

    fn select_basis(&mut self, basis: Option<String>, cx: &mut Context<Self>) {
        if let Some(open) = &mut self.open {
            open.basis = basis;
            open.result = None;
        }
        self.load_open(cx);
    }

    /// After the rows reload, the open row reads its figures again, or closes
    /// if its evidence went.
    fn refresh_open(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else {
            return;
        };
        if self.rows.iter().any(|row| row.id() == open.id) {
            self.load_open(cx);
        } else {
            self.open = None;
        }
    }

    fn load_open(&mut self, cx: &mut Context<Self>) {
        let Some(open) = &self.open else {
            return;
        };
        let Some(row) = self.rows.iter().find(|row| row.id() == open.id) else {
            return;
        };
        let id = open.id.clone();
        let basis = open.basis.clone();
        let key = row.key.clone();
        let invoices = row.invoices.clone();
        self.detail_generation += 1;
        let generation = self.detail_generation;
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || -> Result<_> {
                let revisions = snapshots_of(&key)?;
                let chosen = basis
                    .as_ref()
                    .and_then(|id| revisions.iter().find(|s| s.snapshot_id == *id));
                let result = if invoices.is_empty() {
                    None
                } else {
                    Some(compare(&invoices, chosen)?)
                };
                Ok((revisions, result))
            })
            .await;
            this.update(cx, |this, cx| {
                if this.detail_generation != generation {
                    return;
                }
                match result {
                    Ok((revisions, result)) => {
                        if let Some(open) = this.open.as_mut().filter(|open| open.id == id) {
                            open.revisions = revisions;
                            open.result = result;
                        }
                    }
                    Err(e) => this.error = Some(tr!("reconciliation.load_failed", error = e)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open the entry form, already filled in when it comes from a month with
    /// no invoice.
    fn open_form(&mut self, prefill: Option<Row>, window: &mut Window, cx: &mut Context<Self>) {
        self.form_error = None;
        self.form_account = prefill
            .as_ref()
            .map(|row| row.key.account_id.clone())
            .or_else(|| self.accounts.first().map(|a| a.account_id.clone()));
        let period = prefill
            .as_ref()
            .map(|row| row.key.billing_period.clone())
            .unwrap_or_default();
        let currency = prefill
            .as_ref()
            .map(|row| row.currency.clone())
            .unwrap_or_default();
        for (input, value) in [
            (&self.form_period, period),
            (&self.form_currency, currency),
            (&self.form_reference, String::new()),
            (&self.form_amount, String::new()),
            (&self.form_tax, String::new()),
            (&self.form_credits, String::new()),
            (&self.form_adjustments, String::new()),
            (&self.form_billing_account, String::new()),
        ] {
            input.update(cx, |state, cx| state.set_value(value, window, cx));
        }
        dialogs::open(window, cx, |this, dialog, window, cx| {
            this.render_form(dialog, window, cx)
        });
    }

    fn submit_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.saving {
            return;
        }
        let account = self
            .accounts
            .iter()
            .find(|account| Some(&account.account_id) == self.form_account.as_ref());
        let value = |input: &Entity<InputState>| input.read(cx).value().to_string();
        let draft = InvoiceDraft {
            provider: account.map(|a| a.provider.clone()).unwrap_or_default(),
            account_id: account.map(|a| a.account_id.clone()).unwrap_or_default(),
            billing_period: value(&self.form_period),
            invoice_ref: value(&self.form_reference),
            currency: value(&self.form_currency),
            amount: value(&self.form_amount),
            tax: value(&self.form_tax),
            credits: value(&self.form_credits),
            adjustments: value(&self.form_adjustments),
            billing_account_id: value(&self.form_billing_account),
        };
        let invoice = match invoice_from(&draft, InvoiceSource::Manual, None, Utc::now()) {
            Ok(invoice) => invoice,
            Err(error) => {
                self.form_error = Some(invoice_error_text(&error));
                cx.notify();
                return;
            }
        };
        self.saving = true;
        self.form_error = None;
        cx.notify();
        let reference = invoice.invoice_ref.clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = smol::unblock(move || save_invoices(&[invoice])).await;
            this.update_in(cx, |this, window, cx| {
                this.saving = false;
                match result {
                    Ok(_) => {
                        window.close_dialog(cx);
                        this.notice = Some(tr!("reconciliation.saved", reference = reference));
                        this.load(cx);
                    }
                    Err(e) => {
                        this.form_error = Some(tr!("reconciliation.save_failed", error = e));
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    #[cfg(not(target_family = "wasm"))]
    fn import_csv(&mut self, cx: &mut Context<Self>) {
        if self.importing {
            return;
        }
        self.importing = true;
        self.notice = None;
        self.error = None;
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(tr!("reconciliation.import_csv")),
        });
        cx.notify();
        let known: Vec<(String, String)> = self
            .accounts
            .iter()
            .map(|account| (account.provider.clone(), account.account_id.clone()))
            .collect();
        cx.spawn(async move |this, cx| {
            let path = match chosen.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next(),
                _ => None,
            };
            let outcome: Option<Result<usize, SharedString>> = match path {
                None => None,
                Some(path) => Some(
                    smol::unblock(move || -> Result<usize, SharedString> {
                        let text = std::fs::read_to_string(&path)
                            .map_err(|e| tr!("reconciliation.csv_unreadable", error = e))?;
                        let name = path.file_name().map(|name| name.to_string_lossy());
                        let invoices =
                            reconciliation::invoices_from_csv(&text, name.as_deref(), Utc::now())
                                .map_err(|error| csv_error_text(&error))?;
                        if let Some(stray) = invoices.iter().find(|invoice| {
                            !known.iter().any(|(provider, account)| {
                                *provider == invoice.key.provider
                                    && *account == invoice.key.account_id
                            })
                        }) {
                            return Err(tr!(
                                "reconciliation.csv_unknown_account",
                                provider = stray.key.provider,
                                account = stray.key.account_id
                            ));
                        }
                        save_invoices(&invoices)
                            .map_err(|e| tr!("reconciliation.save_failed", error = e))
                    })
                    .await,
                ),
            };
            this.update(cx, |this, cx| {
                this.importing = false;
                match outcome {
                    None => {}
                    Some(Ok(count)) => {
                        this.notice = Some(tr!("reconciliation.imported", n = count));
                        this.load(cx);
                    }
                    Some(Err(error)) => this.error = Some(error),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn confirm_delete(&mut self, invoice: Invoice, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_delete = Some(invoice);
        dialogs::open_alert(window, cx, |this, alert, _, cx| {
            this.render_delete(alert, cx)
        });
    }

    fn render_delete(&mut self, alert: AlertDialog, cx: &mut Context<Self>) -> AlertDialog {
        let view = cx.entity().downgrade();
        let Some(invoice) = self.pending_delete.clone() else {
            return alert;
        };
        alert
            .title(tr!(
                "reconciliation.delete_title",
                reference = invoice.invoice_ref
            ))
            .description(tr!("reconciliation.delete_body"))
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr!("common.delete"))
                    .ok_variant(ButtonVariant::Danger)
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                view.update(cx, |this, cx| {
                    if let Some(invoice) = this.pending_delete.take() {
                        this.delete(invoice, cx);
                    }
                })
                .ok();
                true
            })
            .on_close(cx.listener(|this, _, _, cx| {
                this.pending_delete = None;
                cx.notify();
            }))
    }

    fn delete(&mut self, invoice: Invoice, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || delete_invoice(&invoice.invoice_id)).await;
            this.update(cx, |this, cx| {
                match result {
                    Ok(()) => this.load(cx),
                    Err(e) => this.error = Some(tr!("reconciliation.save_failed", error = e)),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_form(
        &mut self,
        dialog: Dialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        let field = |label: SharedString, input: &Entity<InputState>| {
            div()
                .flex_1()
                .min_w_0()
                .v_flex()
                .gap_1()
                .child(div().text_sm().child(label))
                .child(Input::new(input))
        };
        let accounts = div()
            .h_flex()
            .flex_wrap()
            .gap_2()
            .children(self.accounts.iter().map(|account| {
                let id = account.account_id.clone();
                let selected = self.form_account.as_ref() == Some(&id);
                Button::new(SharedString::from(format!("invoice-account-{id}")))
                    .label(account.name.clone())
                    .when(selected, |button| button.primary())
                    .when(!selected, |button| {
                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.form_account = Some(id.clone());
                        cx.notify();
                    }))
            }));
        dialog
            .title(tr!("reconciliation.form_title"))
            .w(rems(34.).to_pixels(window.rem_size()))
            .child(
                div()
                    .v_flex()
                    .gap_4()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr!("reconciliation.col_account")))
                            .when(self.accounts.is_empty(), |el| {
                                el.child(theme::caption(cx, tr!("reconciliation.no_accounts")))
                            })
                            .child(accounts),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_4()
                            .child(field(tr!("reconciliation.col_month"), &self.form_period))
                            .child(field(
                                tr!("reconciliation.col_invoice_id"),
                                &self.form_reference,
                            )),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_4()
                            .child(field(
                                tr!("reconciliation.col_currency"),
                                &self.form_currency,
                            ))
                            .child(field(tr!("reconciliation.field_total"), &self.form_amount)),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_4()
                            .child(field(tr!("snapshots.category_tax"), &self.form_tax))
                            .child(field(tr!("snapshots.category_credit"), &self.form_credits))
                            .child(field(
                                tr!("snapshots.category_adjustment"),
                                &self.form_adjustments,
                            )),
                    )
                    .child(field(
                        tr!("reconciliation.field_billing_account"),
                        &self.form_billing_account,
                    ))
                    .child(theme::caption(cx, tr!("reconciliation.form_hint"))),
            )
            .footer(
                DialogFooter::new()
                    .when_some(self.form_error.clone(), |el, error| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .text_color(theme::danger(cx))
                                .child(error),
                        )
                    })
                    .child(
                        Button::new("invoice-cancel")
                            .label(tr!("common.cancel"))
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("invoice-save")
                            .label(tr!("reconciliation.save"))
                            .primary()
                            .loading(self.saving)
                            .disabled(self.saving || self.accounts.is_empty())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_form(window, cx);
                            })),
                    ),
            )
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> Div {
        let pills = [
            None,
            Some(ReconcileStatus::Difference),
            Some(ReconcileStatus::CannotVerify),
            Some(ReconcileStatus::MissingEvidence),
            Some(ReconcileStatus::Matched),
        ];
        div()
            .w_full()
            .h_flex()
            .flex_wrap()
            .items_center()
            .gap_3()
            .child(div().w(rems(18.)).child(Input::new(&self.filter)))
            .child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .gap_1()
                    .children(pills.into_iter().enumerate().map(|(index, status)| {
                        let active = self.status_filter == status;
                        let count = self
                            .rows
                            .iter()
                            .filter(|row| status.is_none_or(|status| row.status == status))
                            .count();
                        let label = status.map_or_else(|| tr!("reconciliation.all"), status_label);
                        let button = Button::new(("reconciliation-status", index))
                            .label(SharedString::from(format!("{label} {count}")))
                            .small()
                            .rounded_full()
                            .custom(theme::range_pill(cx, active))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.status_filter = status;
                                cx.notify();
                            }));
                        if active {
                            button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                        } else {
                            button
                        }
                    })),
            )
            .child(div().flex_1())
            .when(cfg!(not(target_family = "wasm")), |el| {
                #[cfg(not(target_family = "wasm"))]
                let el = el.child(
                    Button::new("reconciliation-import")
                        .label(tr!("reconciliation.import_csv"))
                        .tooltip(tr!(
                            "reconciliation.csv_format",
                            columns = reconciliation::CSV_COLUMNS.join(", ")
                        ))
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .loading(self.importing)
                        .disabled(self.importing)
                        .on_click(cx.listener(|this, _, _, cx| this.import_csv(cx))),
                );
                el
            })
            .child(
                Button::new("reconciliation-enter")
                    .label(tr!("reconciliation.enter_total"))
                    .custom(theme::outline_variant(cx))
                    .card_outline(cx)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.open_form(None, window, cx);
                    })),
            )
    }

    fn sort_header(&self, column: Column, label: SharedString, cx: &mut Context<Self>) -> Button {
        let direction = self
            .sort
            .filter(|(sorted, _)| *sorted == column)
            .map(|(_, descending)| descending);
        Button::new(SharedString::from(format!(
            "reconciliation-sort-{column:?}"
        )))
        .label(label)
        .ghost()
        .small()
        .compact()
        .when_some(direction, |button, descending| {
            button.icon(if descending {
                IconName::ChevronDown
            } else {
                IconName::ChevronUp
            })
        })
        .on_click(cx.listener(move |this, _, _, cx| this.toggle_sort(column, cx)))
    }

    fn render_table(&self, cx: &mut Context<Self>) -> Div {
        let text = self.filter.read(cx).value().to_string();
        let mut rows: Vec<Row> = self
            .rows
            .iter()
            .filter(|row| row_matches(row, &text, self.status_filter))
            .cloned()
            .collect();
        sort_rows(&mut rows, self.sort);

        let header = div()
            .w_full()
            .h_flex()
            .gap_3()
            .pb_2()
            .child(div().flex_1().min_w_0().child(self.sort_header(
                Column::Account,
                tr!("reconciliation.col_account"),
                cx,
            )))
            .child(div().w(rems(6.)).flex_shrink_0().child(self.sort_header(
                Column::Month,
                tr!("reconciliation.col_month"),
                cx,
            )))
            .child(
                div()
                    .w(rems(8.))
                    .flex_shrink_0()
                    .child(theme::header_cell(cx, tr!("reconciliation.col_invoice_id"))),
            )
            .child(amount_header(self.sort_header(
                Column::Invoice,
                tr!("reconciliation.col_invoice"),
                cx,
            )))
            .child(amount_header(self.sort_header(
                Column::Ledger,
                tr!("reconciliation.col_ledger"),
                cx,
            )))
            .child(amount_header(self.sort_header(
                Column::Difference,
                tr!("reconciliation.col_difference"),
                cx,
            )))
            .child(
                div()
                    .w(rems(9.))
                    .flex_shrink_0()
                    .child(theme::header_cell(cx, tr!("reconciliation.col_status"))),
            );

        let mut table = div().v_flex().child(header);
        if rows.is_empty() {
            let message = if self.rows.is_empty() {
                tr!("reconciliation.empty")
            } else {
                tr!("reconciliation.no_match")
            };
            return table.child(
                div()
                    .py_6()
                    .border_t_1()
                    .border_color(theme::card_border(cx))
                    .child(theme::caption(cx, message)),
            );
        }
        for row in rows {
            let is_open = self.open.as_ref().is_some_and(|open| open.id == row.id());
            table = table.child(self.render_row(&row, is_open, cx));
            if is_open {
                table = table.child(self.render_detail(&row, cx));
            }
        }
        table
    }

    fn render_row(&self, row: &Row, is_open: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let id = row.id();
        let keyboard_id = id.clone();
        let hover = theme::sidebar_bg(cx);
        let focus_ring = theme::sidebar_bg(cx);
        let accessible_name = SharedString::from(format!(
            "{} {} {}",
            row.account_name,
            row.key.billing_period,
            status_label(row.status)
        ));
        let reference = if row.invoices.is_empty() {
            "—".to_string()
        } else {
            row.reference()
        };
        div()
            .id(SharedString::from(format!("reconciliation-row-{id}")))
            .role(Role::Button)
            .aria_label(accessible_name)
            .aria_expanded(is_open)
            .tab_index(0)
            .focus_visible(move |style| style.bg(focus_ring))
            .w_full()
            .h_flex()
            .items_center()
            .gap_3()
            .py_2()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .cursor_pointer()
            .hover(move |style| style.bg(hover))
            .when(is_open, |el| el.bg(hover))
            .text_sm()
            .text_color(theme::text_primary(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(if is_open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small()
                        .text_color(theme::text_muted(cx)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(format!("{} ({})", row.account_name, row.key.provider)),
                    ),
            )
            .child(
                div()
                    .w(rems(6.))
                    .flex_shrink_0()
                    .child(row.key.billing_period.clone()),
            )
            .child(
                div()
                    .w(rems(8.))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(reference),
            )
            .child(amount_cell(amount_text(row.invoice_total, &row.currency)))
            .child(amount_cell(amount_text(row.ledger_total, &row.currency)))
            .child(amount_cell(signed_text(row.difference, &row.currency)))
            .child(
                div()
                    .w(rems(9.))
                    .flex_shrink_0()
                    .child(status_badge(row.status, cx)),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle_row(id.clone(), cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle_row(keyboard_id.clone(), cx);
                }
            }))
    }

    fn render_detail(&self, row: &Row, cx: &mut Context<Self>) -> Div {
        let details = div()
            .v_flex()
            .gap_3()
            .py_3()
            .pl_6()
            .border_t_1()
            .border_color(theme::card_border(cx));
        if row.invoices.is_empty() {
            let prefill = row.clone();
            return details
                .child(theme::caption(cx, tr!("reconciliation.missing_body")))
                .child(
                    div().h_flex().child(
                        Button::new("reconciliation-enter-for-row")
                            .label(tr!("reconciliation.enter_total"))
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.open_form(Some(prefill.clone()), window, cx);
                            })),
                    ),
                );
        }
        let Some(open) = &self.open else {
            return details;
        };
        let details = details.child(self.render_basis(open, cx));
        let Some(result) = &open.result else {
            return details.child(theme::caption(cx, tr!("common.loading")));
        };
        details
            .child(
                div()
                    .h_flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .child(status_badge(result.status, cx))
                    .child(theme::caption(
                        cx,
                        tr!(
                            "reconciliation.basis_line",
                            currency = result.currency,
                            tolerance = fmt::exact_amount(result.tolerance, &result.currency),
                            authoritative = result.coverage.authoritative_rows,
                            rows = result.coverage.rows,
                            from = date_text(result.coverage.from),
                            through = date_text(result.coverage.through)
                        ),
                    )),
            )
            .when(!result.reasons.is_empty(), |el| {
                el.child(
                    div()
                        .v_flex()
                        .gap_1()
                        .children(result.reasons.iter().map(|reason| {
                            div()
                                .text_sm()
                                .text_color(theme::text_primary(cx))
                                .child(reason_text(reason))
                        })),
                )
            })
            .child(components_table(result, cx))
            .child(self.render_evidence(row, cx))
    }

    /// What the invoice is compared with: the ledger, or a frozen revision.
    fn render_basis(&self, open: &OpenRow, cx: &mut Context<Self>) -> Div {
        let pill = |id: SharedString,
                    label: SharedString,
                    basis: Option<String>,
                    cx: &mut Context<Self>| {
            let active = open.basis == basis;
            let button = Button::new(id)
                .label(label)
                .small()
                .rounded_full()
                .custom(theme::range_pill(cx, active))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_basis(basis.clone(), cx);
                }));
            if active {
                button.card_outline(cx).font_weight(FontWeight::MEDIUM)
            } else {
                button
            }
        };
        let mut basis = div()
            .h_flex()
            .flex_wrap()
            .items_center()
            .gap_1()
            .child(theme::caption(cx, tr!("reconciliation.compare_with")))
            .child(pill(
                "reconciliation-basis-current".into(),
                tr!("reconciliation.basis_current"),
                None,
                cx,
            ));
        for revision in &open.revisions {
            basis = basis.child(pill(
                SharedString::from(format!("reconciliation-basis-{}", revision.snapshot_id)),
                tr!("snapshots.revision_short", n = revision.revision),
                Some(revision.snapshot_id.clone()),
                cx,
            ));
        }
        basis
    }

    /// The invoices themselves: where each came from, and the way to withdraw
    /// one entered by mistake.
    fn render_evidence(&self, row: &Row, cx: &mut Context<Self>) -> Div {
        let mut evidence = div()
            .v_flex()
            .gap_1()
            .child(theme::header_cell(cx, tr!("reconciliation.evidence")));
        for invoice in &row.invoices {
            let source: SharedString = match (invoice.source, &invoice.source_file) {
                (InvoiceSource::Csv, Some(file)) => {
                    tr!("reconciliation.source_csv_file", file = file)
                }
                (InvoiceSource::Csv, None) => tr!("reconciliation.source_csv"),
                (InvoiceSource::Manual, _) => tr!("reconciliation.source_manual"),
            };
            let target = invoice.clone();
            evidence = evidence.child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .child(theme::caption(
                        cx,
                        tr!(
                            "reconciliation.evidence_line",
                            reference = invoice.invoice_ref,
                            amount = fmt::exact_amount(invoice.amount, &invoice.currency),
                            source = source,
                            at = invoice.entered_at.format("%Y-%m-%d %H:%M UTC").to_string()
                        ),
                    ))
                    .child(
                        Button::new(SharedString::from(format!(
                            "reconciliation-delete-{}",
                            invoice.invoice_id
                        )))
                        .label(tr!("common.delete"))
                        .ghost()
                        .small()
                        .on_click(cx.listener(
                            move |this, _, window, cx| {
                                this.confirm_delete(target.clone(), window, cx);
                            },
                        )),
                    ),
            );
        }
        evidence
    }
}

fn amount_header(button: Button) -> Div {
    div()
        .w(rems(AMOUNT_WIDTH))
        .flex_shrink_0()
        .h_flex()
        .justify_end()
        .child(button)
}

fn amount_cell(text: SharedString) -> Div {
    div()
        .w(rems(AMOUNT_WIDTH))
        .flex_shrink_0()
        .text_right()
        .child(text)
}

/// The figure by charge category, on both sides.
fn components_table(result: &Reconciliation, cx: &App) -> Div {
    let currency = result.currency.as_str();
    let heading = |text: SharedString| {
        div()
            .w(rems(AMOUNT_WIDTH))
            .flex_shrink_0()
            .text_right()
            .child(theme::header_cell(cx, text))
    };
    div()
        .v_flex()
        .child(
            div()
                .w_full()
                .h_flex()
                .gap_3()
                .pb_1()
                .child(
                    div()
                        .flex_1()
                        .child(theme::header_cell(cx, tr!("reconciliation.col_category"))),
                )
                .child(heading(tr!("reconciliation.col_invoice")))
                .child(heading(tr!("reconciliation.col_ledger")))
                .child(heading(tr!("reconciliation.col_difference"))),
        )
        .children(result.components.iter().map(|component| {
            let difference = component.invoice.map(|invoice| invoice - component.ledger);
            div()
                .w_full()
                .h_flex()
                .gap_3()
                .py_1()
                .border_t_1()
                .border_color(theme::card_border(cx))
                .text_sm()
                .text_color(theme::text_primary(cx))
                .child(div().flex_1().child(tr!(
                    "reconciliation.category_rows",
                    category = category_label(component.category),
                    rows = component.rows
                )))
                .child(amount_cell(amount_text(component.invoice, currency)))
                .child(amount_cell(amount_text(Some(component.ledger), currency)))
                .child(amount_cell(signed_text(difference, currency)))
        }))
        .child(
            div()
                .w_full()
                .h_flex()
                .gap_3()
                .py_1()
                .border_t_1()
                .border_color(theme::card_border(cx))
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme::text_primary(cx))
                .child(div().flex_1().child(tr!("reconciliation.total")))
                .child(amount_cell(amount_text(
                    Some(result.invoice_total),
                    currency,
                )))
                .child(amount_cell(amount_text(result.ledger_total, currency)))
                .child(amount_cell(signed_text(result.difference, currency))),
        )
}

impl Render for ReconciliationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body = if !self.loaded && self.loading {
            theme::caption(cx, tr!("common.loading"))
        } else {
            self.render_table(cx)
        };
        div()
            .id("reconciliation")
            .size_full()
            .v_flex()
            .gap_4()
            .p_8()
            .bg(theme::app_bg(cx))
            .overflow_y_scroll()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, tr!("reconciliation.title")))
                    .child(theme::caption(cx, tr!("reconciliation.subtitle"))),
            )
            .child(self.render_toolbar(cx))
            .when_some(self.error.clone(), |el, error| {
                el.child(theme::caption(cx, error).text_color(theme::danger(cx)))
            })
            .when_some(self.notice.clone(), |el, notice| {
                el.child(theme::caption(cx, notice))
            })
            .child(theme::card(cx).w_full().p_5().child(body))
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that brings gpui_kit's own `test` attribute, which
    // shadows the standard one.
    use super::{row_matches, sort_rows, Column, Row};
    use crate::model::{PeriodKey, ReconcileStatus};

    fn row(period: &str, status: ReconcileStatus, difference: Option<f64>) -> Row {
        Row {
            key: PeriodKey::new("AWS", "acct", period),
            account_name: "Prod".to_string(),
            currency: "USD".to_string(),
            invoices: Vec::new(),
            ledger_total: None,
            invoice_total: None,
            difference,
            status,
        }
    }

    fn months(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.key.billing_period.as_str()).collect()
    }

    #[test]
    fn the_default_order_puts_problems_first_and_new_months_above_old() {
        let mut rows = vec![
            row("2026-09", ReconcileStatus::Matched, Some(0.0)),
            row("2026-08", ReconcileStatus::MissingEvidence, None),
            row("2026-07", ReconcileStatus::Difference, Some(5.0)),
            row("2026-09", ReconcileStatus::Difference, Some(1.0)),
            row("2026-06", ReconcileStatus::CannotVerify, None),
        ];
        sort_rows(&mut rows, None);
        assert_eq!(
            months(&rows),
            vec!["2026-09", "2026-07", "2026-06", "2026-08", "2026-09"]
        );
        assert_eq!(rows[4].status, ReconcileStatus::Matched);
    }

    #[test]
    fn an_absent_figure_sorts_last_in_either_direction() {
        let mut rows = vec![
            row("2026-01", ReconcileStatus::CannotVerify, None),
            row("2026-02", ReconcileStatus::Difference, Some(5.0)),
            row("2026-03", ReconcileStatus::Difference, Some(9.0)),
        ];
        sort_rows(&mut rows, Some((Column::Difference, true)));
        assert_eq!(months(&rows), vec!["2026-03", "2026-02", "2026-01"]);
        sort_rows(&mut rows, Some((Column::Difference, false)));
        assert_eq!(months(&rows), vec!["2026-02", "2026-03", "2026-01"]);
    }

    #[test]
    fn the_filter_reads_text_and_status_together() {
        let r = row("2026-09", ReconcileStatus::Difference, Some(1.0));
        assert!(row_matches(&r, "", None));
        assert!(row_matches(&r, "aws", None));
        assert!(row_matches(&r, "PROD", None));
        assert!(row_matches(
            &r,
            "2026-09",
            Some(ReconcileStatus::Difference)
        ));
        assert!(!row_matches(&r, "2026-08", None));
        assert!(!row_matches(&r, "", Some(ReconcileStatus::Matched)));
    }
}
