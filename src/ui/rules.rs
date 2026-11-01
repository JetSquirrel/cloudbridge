//! Rules View — alerting rules that run on the local ledger after each ingest.

use chrono::{DateTime, Utc};
use gpui_kit::component::{
    button::*,
    dialog::{AlertDialog, Cancel, Dialog, DialogButtonProps, DialogFooter},
    input::{Input, InputState},
    skeleton::Skeleton,
    switch::*,
    Disableable as _, Sizable as _, StyledExt, WindowExt as _,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::json;

use crate::alerts::{self, RuleView};
use crate::cloud::BudgetStatus;
use crate::ui::theme::CardOutline as _;

use super::{data, dialogs, fmt, theme};

/// One selectable rule kind in the "New rule" dialog.
struct RuleKindSpec {
    kind: &'static str,
    /// `locales/ui.yml` key of the default name, also the selector label.
    name: &'static str,
}

const RULE_KINDS: [RuleKindSpec; 4] = [
    RuleKindSpec {
        kind: alerts::RULE_COST_ANOMALY,
        name: "rules.kind_cost_anomaly",
    },
    RuleKindSpec {
        kind: alerts::RULE_BALANCE_FLOOR,
        name: "rules.kind_balance_floor",
    },
    RuleKindSpec {
        kind: alerts::RULE_UNTAGGED_RATIO,
        name: "rules.kind_untagged_ratio",
    },
    RuleKindSpec {
        kind: alerts::RULE_BUDGET,
        name: "rules.kind_budget",
    },
];

/// What a budget rule compares against its threshold, as config values and
/// the `locales/ui.yml` keys of their selector labels.
const BASED_OPTIONS: [(&str, &str); 2] = [
    ("cost", "rules.based_cost"),
    ("forecast", "rules.based_forecast"),
];

/// A budget rule's threshold type, as config values and label keys.
const THRESHOLD_TYPE_OPTIONS: [(&str, &str); 2] = [
    ("percent", "rules.threshold_percent"),
    ("absolute", "rules.threshold_absolute"),
];

/// Humanized "last fired" line under a rule's switch.
fn last_fired_label(last_fired_at: Option<DateTime<Utc>>) -> String {
    match last_fired_at {
        Some(at) => tr!("rules.fired", when = fmt::relative_time(at)).into(),
        None => tr!("rules.never_fired").into(),
    }
}

/// Rules View
pub struct RulesView {
    /// Loaded rules, once the first load lands.
    data: Option<data::RulesData>,
    /// Per-account budget consumption, loaded alongside the rules.
    budgets: Vec<BudgetStatus>,
    /// `(id, name)` of every account, for the budget editor and the budget
    /// rule's account picker.
    accounts: Vec<(String, String)>,
    /// A load is in flight.
    loading: bool,
    /// Bumped on every load so an overlapping older load discards its
    /// result instead of clobbering fresher state.
    load_generation: u64,
    /// The last load, toggle or delete failure, if any.
    error: Option<String>,
    /// Whether the "New rule" dialog is open, so a create that lands after
    /// the dialog was dismissed does not close some other dialog.
    show_new_dialog: bool,
    /// Validation or creation failure inside the dialog.
    dialog_error: Option<String>,
    /// A create is in flight.
    creating: bool,
    /// The kind the dialog will create.
    selected_kind: &'static str,
    name_input: Entity<InputState>,
    multiplier_input: Entity<InputState>,
    consecutive_input: Entity<InputState>,
    floor_input: Entity<InputState>,
    threshold_input: Entity<InputState>,
    /// The account a budget rule watches (dialog).
    rule_account: Option<String>,
    /// A budget rule's `based` (dialog): `cost` or `forecast`.
    rule_based: &'static str,
    /// A budget rule's threshold type (dialog): `percent` or `absolute`.
    rule_threshold_type: &'static str,
    /// The account the budget editor edits.
    budget_account: Option<String>,
    budget_amount_input: Entity<InputState>,
    budget_threshold_input: Entity<InputState>,
    /// A budget save or delete is in flight.
    saving_budget: bool,
    /// Budget editor validation or save failure.
    budget_error: Option<String>,
    /// The custom rule awaiting delete confirmation, if any.
    pending_delete: Option<String>,
}

impl RulesView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let name_input = cx.new(|cx| InputState::new(window, cx));
        let multiplier_input = cx.new(|cx| InputState::new(window, cx).placeholder("2.5"));
        let consecutive_input = cx.new(|cx| InputState::new(window, cx).placeholder("2"));
        let floor_input = cx.new(|cx| InputState::new(window, cx).placeholder("200"));
        let threshold_input = cx.new(|cx| InputState::new(window, cx).placeholder("15"));
        let budget_amount_input = cx.new(|cx| InputState::new(window, cx).placeholder("500"));
        let budget_threshold_input = cx.new(|cx| InputState::new(window, cx).placeholder("80"));

        Self {
            data: None,
            budgets: Vec::new(),
            accounts: Vec::new(),
            loading: false,
            load_generation: 0,
            error: None,
            show_new_dialog: false,
            dialog_error: None,
            creating: false,
            selected_kind: RULE_KINDS[0].kind,
            name_input,
            multiplier_input,
            consecutive_input,
            floor_input,
            threshold_input,
            rule_account: None,
            rule_based: BASED_OPTIONS[0].0,
            rule_threshold_type: THRESHOLD_TYPE_OPTIONS[0].0,
            budget_account: None,
            budget_amount_input,
            budget_threshold_input,
            saving_budget: false,
            budget_error: None,
            pending_delete: None,
        }
    }

    /// Start the first load if none has run. The app shell calls this on
    /// the page's first visit, so construction — and window opening —
    /// stays cheap and the hidden pages do not race the visible one for
    /// the ledger at startup.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.data.is_none() && !self.loading {
            self.load(cx);
        }
    }

    /// Reload the rules. Called by the app shell when this page is
    /// navigated to; a no-op while a load is already in flight. Existing
    /// data stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.load(cx);
    }

    /// Load the rules from the ledger. The query is blocking SQLite, so it
    /// runs on a worker thread like the Accounts page's loads do. Loads can
    /// overlap — a toggle or delete triggers one mid-flight — so only the
    /// newest generation may write state; an older result is discarded.
    ///
    /// Budgets and accounts load alongside: the budget editor and the
    /// budget rule's account picker both need them.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.load_generation += 1;
        let generation = self.load_generation;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = smol::unblock(|| {
                let rules = data::load_rules().map_err(|e| e.to_string())?;
                let budgets = data::load_budget_statuses().map_err(|e| e.to_string())?;
                let accounts = data::account_names().map_err(|e| e.to_string())?;
                Ok::<_, String>((rules, budgets, accounts))
            })
            .await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if generation != this.load_generation {
                        return;
                    }
                    this.loading = false;
                    match result {
                        Ok((rules, budgets, accounts)) => {
                            this.data = Some(rules);
                            this.budgets = budgets;
                            this.accounts = accounts;
                            // A banner from an earlier failure would mask
                            // the fresh list forever.
                            this.error = None;
                        }
                        Err(e) => {
                            this.error = Some(tr!("rules.load_failed", error = e).into());
                        }
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    /// Flip a rule on or off. The switch reflects the new state immediately;
    /// the write is a single SQLite update, run off-thread, followed by an
    /// evaluation so a condition that already holds fires right away, then a
    /// reload so last-fired catches up. A failure reloads too, landing the
    /// switch back on the truth.
    fn set_enabled(&mut self, id: String, enabled: bool, cx: &mut Context<Self>) {
        if let Some(data) = self.data.as_mut() {
            if let Some(rule) = data.rules.iter_mut().find(|rule| rule.id == id) {
                rule.enabled = enabled;
            }
        }
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                data::set_rule_enabled(&id, enabled).map_err(|e| e.to_string())
            })
            .await;
            if result.is_ok() {
                let _ = smol::unblock(data::evaluate_rules).await;
            }

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if let Err(e) = result {
                        this.error = Some(tr!("rules.update_failed", error = e).into());
                    }
                    this.load(cx);
                })
                .ok();
            });
        })
        .detach();
    }

    // ==================== New rule dialog ====================

    fn show_new_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_new_dialog = true;
        self.dialog_error = None;
        self.error = None;
        self.set_kind(RULE_KINDS[0].kind, window, cx);
        dialogs::open(window, cx, |this, dialog, window, cx| {
            this.new_rule_dialog(dialog, window, cx)
        });
    }

    /// The dialog was dismissed (Escape, Cancel, the close button or the
    /// backdrop), or the page closed it after a create.
    fn new_dialog_closed(&mut self, cx: &mut Context<Self>) {
        self.show_new_dialog = false;
        self.dialog_error = None;
        cx.notify();
    }

    /// Select the kind the dialog creates: the name field is prefilled with
    /// the kind's default name, and the parameter fields with the seeded
    /// rule's values.
    fn set_kind(&mut self, kind: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        self.selected_kind = kind;
        self.dialog_error = None;

        let spec = RULE_KINDS
            .iter()
            .find(|spec| spec.kind == kind)
            .unwrap_or(&RULE_KINDS[0]);
        self.name_input.update(cx, |state, cx| {
            state.set_value(tr!(spec.name), window, cx);
        });
        self.multiplier_input.update(cx, |state, cx| {
            state.set_value("2.5", window, cx);
        });
        self.consecutive_input.update(cx, |state, cx| {
            state.set_value("2", window, cx);
        });
        self.floor_input.update(cx, |state, cx| {
            state.set_value("200", window, cx);
        });
        self.threshold_input.update(cx, |state, cx| {
            // A budget rule's threshold reads as a percent of the budget by
            // default; the untagged rule's reads as a percent of spend.
            let default = if kind == alerts::RULE_BUDGET {
                "80"
            } else {
                "15"
            };
            state.set_value(default, window, cx);
        });
        self.rule_based = BASED_OPTIONS[0].0;
        self.rule_threshold_type = THRESHOLD_TYPE_OPTIONS[0].0;
        if self.rule_account.is_none() {
            self.rule_account = self.accounts.first().map(|(id, _)| id.clone());
        }

        cx.notify();
    }

    /// The config JSON the dialog's parameter fields describe, or the parse
    /// error to show in it.
    fn config_from_inputs(
        &self,
        cx: &Context<Self>,
    ) -> std::result::Result<serde_json::Value, String> {
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();

        match self.selected_kind {
            alerts::RULE_COST_ANOMALY => {
                let multiplier = value(&self.multiplier_input)
                    .parse::<f64>()
                    .map_err(|_| tr!("rules.bad_multiplier").to_string())?;
                let consecutive_days = value(&self.consecutive_input)
                    .parse::<u64>()
                    .map_err(|_| tr!("rules.bad_days").to_string())?;
                Ok(json!({
                    "multiplier": multiplier,
                    "consecutive_days": consecutive_days,
                }))
            }
            alerts::RULE_BALANCE_FLOOR => {
                let floor = value(&self.floor_input)
                    .parse::<f64>()
                    .map_err(|_| tr!("rules.bad_floor").to_string())?;
                Ok(json!({ "floor": floor }))
            }
            alerts::RULE_BUDGET => {
                let account_id = self
                    .rule_account
                    .clone()
                    .ok_or_else(|| tr!("rules.pick_account").to_string())?;
                let threshold = value(&self.threshold_input)
                    .parse::<f64>()
                    .map_err(|_| tr!("rules.bad_threshold_80").to_string())?;
                Ok(json!({
                    "account_id": account_id,
                    "based": self.rule_based,
                    "threshold_type": self.rule_threshold_type,
                    "threshold": threshold,
                }))
            }
            _ => {
                // The dialog asks for a percent; the config is a ratio.
                let percent = value(&self.threshold_input)
                    .parse::<f64>()
                    .map_err(|_| tr!("rules.bad_threshold_15").to_string())?;
                Ok(json!({ "threshold": percent / 100.0 }))
            }
        }
    }

    /// Create the rule off-thread, evaluate right away so a condition that
    /// already holds fires immediately, then reload the list.
    fn submit_new_rule(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Enter confirms the dialog too, so a second submit can arrive
        // while the first is in flight.
        if self.creating {
            return;
        }
        let name = self.name_input.read(cx).value().trim().to_string();
        let config = match self.config_from_inputs(cx) {
            Ok(config) => config,
            Err(e) => {
                self.dialog_error = Some(e);
                cx.notify();
                return;
            }
        };

        self.creating = true;
        self.dialog_error = None;
        cx.notify();

        let kind = self.selected_kind;
        cx.spawn_in(window, async move |this, cx| {
            let result = smol::unblock(move || {
                data::create_rule(kind, &name, config).map_err(|e| e.to_string())
            })
            .await;
            if result.is_ok() {
                let _ = smol::unblock(data::evaluate_rules).await;
            }

            this.update_in(cx, |this, window, cx| {
                this.creating = false;
                match result {
                    Ok(_) => {
                        if this.show_new_dialog {
                            window.close_dialog(cx);
                        }
                        this.new_dialog_closed(cx);
                        this.load(cx);
                    }
                    Err(e) => {
                        this.dialog_error = Some(e);
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    // ==================== Budget editor ====================

    /// Select the account the budget editor edits, prefilling the fields
    /// from its current budget when one exists. (The status carries no
    /// alert threshold, so the percent field falls back to the default.)
    fn select_budget_account(
        &mut self,
        account_id: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let amount = self
            .budgets
            .iter()
            .find(|status| status.account_id == account_id)
            .map(|status| format!("{}", status.monthly_budget))
            .unwrap_or_default();
        self.budget_account = Some(account_id);
        self.budget_error = None;
        self.budget_amount_input.update(cx, |state, cx| {
            state.set_value(amount, window, cx);
        });
        self.budget_threshold_input.update(cx, |state, cx| {
            state.set_value("80", window, cx);
        });
        cx.notify();
    }

    /// Save the editor's budget for the selected account, re-evaluate so a
    /// rule whose line moved fires (or resolves) right away, then reload.
    fn save_budget(&mut self, cx: &mut Context<Self>) {
        let Some(account_id) = self.budget_account.clone() else {
            return;
        };
        let value = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        let monthly_budget = match value(&self.budget_amount_input).parse::<f64>() {
            Ok(amount) if amount > 0.0 => amount,
            _ => {
                self.budget_error = Some(tr!("rules.bad_budget").into());
                cx.notify();
                return;
            }
        };
        let alert_threshold = match value(&self.budget_threshold_input).parse::<f64>() {
            Ok(percent) if percent > 0.0 => percent,
            _ => {
                self.budget_error = Some(tr!("rules.bad_alert_percent").into());
                cx.notify();
                return;
            }
        };

        self.saving_budget = true;
        self.budget_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                data::save_budget(&account_id, monthly_budget, alert_threshold)
                    .map_err(|e| e.to_string())
            })
            .await;
            if result.is_ok() {
                let _ = smol::unblock(data::evaluate_rules).await;
            }

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.saving_budget = false;
                    if let Err(e) = result {
                        this.budget_error = Some(tr!("rules.budget_save_failed", error = e).into());
                        cx.notify();
                    } else {
                        this.load(cx);
                    }
                })
                .ok();
            });
        })
        .detach();
    }

    /// Remove the selected account's budget. Budget rules watching it stop
    /// firing: with no budget there is nothing to measure against.
    fn delete_budget(&mut self, cx: &mut Context<Self>) {
        let Some(account_id) = self.budget_account.clone() else {
            return;
        };

        self.saving_budget = true;
        self.budget_error = None;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result =
                smol::unblock(move || data::delete_budget(&account_id).map_err(|e| e.to_string()))
                    .await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.saving_budget = false;
                    if let Err(e) = result {
                        this.budget_error =
                            Some(tr!("rules.budget_remove_failed", error = e).into());
                        cx.notify();
                    } else {
                        this.load(cx);
                    }
                })
                .ok();
            });
        })
        .detach();
    }

    // ==================== Delete ====================

    fn ask_delete(&mut self, id: String, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_delete = Some(id);
        dialogs::open_alert(window, cx, |this, alert, _, cx| {
            this.delete_confirm(alert, cx)
        });
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_delete.take() else {
            return;
        };

        cx.spawn(async move |this, cx| {
            let result =
                smol::unblock(move || data::delete_rule(&id).map_err(|e| e.to_string())).await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    if let Err(e) = result {
                        this.error = Some(tr!("rules.delete_failed", error = e).into());
                    }
                    this.load(cx);
                })
                .ok();
            });
        })
        .detach();
    }

    // ==================== Rendering ====================

    fn render_rule(&self, rule: &RuleView, cx: &Context<Self>) -> impl IntoElement {
        let name_row = div()
            .h_flex()
            // Baseline (not center) so the scope pill's text sits on the
            // same line as the rule name instead of floating against the
            // name's taller line box.
            .items_baseline()
            .gap_3()
            .flex_wrap()
            .child(
                div()
                    .text_lg()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_primary(cx))
                    .child(rule.name.clone()),
            )
            .child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .border_1()
                    .border_color(theme::accent(cx))
                    .text_xs()
                    .text_color(theme::accent(cx))
                    .child(rule.scope.clone()),
            );

        let description = div()
            .w_full()
            .text_sm()
            .text_color(theme::text_muted(cx))
            .child(rule.description.clone());

        let chips_row = div()
            .w_full()
            .h_flex()
            .items_center()
            .gap_2()
            .flex_wrap()
            .children(
                rule.condition_chips
                    .iter()
                    .map(|text| theme::pill_outline(cx, text.clone())),
            )
            .children(
                rule.delivery_chips
                    .iter()
                    .map(|text| theme::pill_outline(cx, text.clone())),
            );

        let switch_id = SharedString::from(format!("rule-switch-{}", rule.id));
        let rule_id = rule.id.clone();
        let custom = rule.id.starts_with(alerts::CUSTOM_RULE_PREFIX);

        theme::card(cx)
            .w_full()
            .p_5()
            .h_flex()
            .gap_4()
            .child(
                // min_w_0: a flex child may not shrink below its content by
                // default, which is what pushed long descriptions past the
                // card's edge.
                div()
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap_3()
                    .child(name_row)
                    .child(description)
                    .child(chips_row),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .v_flex()
                    .items_end()
                    .gap_2()
                    .child(
                        Switch::new(switch_id)
                            .checked(rule.enabled)
                            .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                this.set_enabled(rule_id.clone(), *checked, cx);
                            })),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted(cx))
                            .child(last_fired_label(rule.last_fired_at)),
                    )
                    .when(custom, |el| {
                        let rule_id = rule.id.clone();
                        el.child(
                            Button::new(SharedString::from(format!("rule-delete-{}", rule.id)))
                                .label(tr!("common.delete"))
                                .ghost()
                                .danger()
                                .small()
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.ask_delete(rule_id.clone(), window, cx);
                                })),
                        )
                    }),
            )
    }

    /// The "Monthly budgets" card: per-account budget editor on top,
    /// consumption lines for every account that has a budget below.
    fn render_budgets(&self, cx: &Context<Self>) -> impl IntoElement {
        let account_selector =
            div()
                .h_flex()
                .gap_2()
                .flex_wrap()
                .children(self.accounts.iter().map(|(id, name)| {
                    let id = id.clone();
                    let selected = self.budget_account.as_deref() == Some(id.as_str());
                    Button::new(SharedString::from(format!("budget-account-{id}")))
                        .label(name.clone())
                        .when(selected, |button| button.primary())
                        .when(!selected, |button| {
                            button.custom(theme::outline_variant(cx)).card_outline(cx)
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.select_budget_account(id.clone(), window, cx);
                        }))
                }));

        let has_budget = self
            .budget_account
            .as_ref()
            .is_some_and(|id| self.budgets.iter().any(|status| &status.account_id == id));

        let editor = div()
            .h_flex()
            .gap_4()
            .flex_wrap()
            .items_end()
            .child(
                div()
                    .w_40()
                    .v_flex()
                    .gap_1()
                    .child(div().text_sm().child(tr!(
                        "rules.monthly_budget",
                        currency = data::reporting_currency()
                    )))
                    .child(Input::new(&self.budget_amount_input)),
            )
            .child(
                div()
                    .w_40()
                    .v_flex()
                    .gap_1()
                    .child(div().text_sm().child(tr!("rules.alert_at")))
                    .child(Input::new(&self.budget_threshold_input)),
            )
            .child(
                Button::new("save-budget")
                    .label(tr!("rules.save_budget"))
                    .primary()
                    .disabled(self.saving_budget || self.budget_account.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_budget(cx);
                    })),
            )
            .when(has_budget, |el| {
                el.child(
                    Button::new("delete-budget")
                        .label(tr!("rules.remove"))
                        .ghost()
                        .danger()
                        .small()
                        .disabled(self.saving_budget)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.delete_budget(cx);
                        })),
                )
            });

        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_3()
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_primary(cx))
                    .child(tr!("rules.monthly_budgets")),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_muted(cx))
                    .child(tr!("rules.budgets_hint")),
            )
            .when(self.accounts.is_empty(), |el| {
                el.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_3()
                        .child(theme::caption(cx, tr!("rules.budget_needs_account")))
                        .child(
                            Button::new("budget-add-account")
                                .label(tr!("palette.add_account"))
                                .custom(theme::outline_variant(cx))
                                .card_outline(cx)
                                .on_click(|_, _, cx| crate::app::open_add_account(cx)),
                        ),
                )
            })
            .when(!self.accounts.is_empty(), |el| {
                el.child(account_selector).child(editor)
            })
            .children(self.budgets.iter().map(|status| {
                let line = tr!(
                    "rules.budget_line",
                    account = status.account_name,
                    used = fmt::amount(status.current_cost, &status.currency),
                    budget = fmt::amount(status.monthly_budget, &status.currency),
                    pct = format!("{:.0}", status.percentage_used)
                );
                div()
                    .text_sm()
                    .text_color(if status.alert_triggered {
                        theme::danger(cx)
                    } else {
                        theme::text_muted(cx)
                    })
                    .child(line)
            }))
            .when_some(self.budget_error.clone(), |el, error| {
                el.child(div().text_sm().text_color(theme::danger(cx)).child(error))
            })
    }

    fn render_kind_selector(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .h_flex()
            .gap_2()
            .flex_wrap()
            .children(RULE_KINDS.iter().map(|spec| {
                let kind = spec.kind;
                Button::new(SharedString::from(format!("rule-kind-{kind}")))
                    .label(tr!(spec.name))
                    .when(kind == self.selected_kind, |button| button.primary())
                    .when(kind != self.selected_kind, |button| {
                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_kind(kind, window, cx);
                    }))
            }))
    }

    /// The "New rule" dialog: kind, name and the kind's parameters. Enter
    /// creates, like the Create rule button; the dialog stays open until the
    /// create lands, so a validation error shows where it was typed.
    fn new_rule_dialog(
        &mut self,
        dialog: Dialog,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Dialog {
        let parameters =
            match self.selected_kind {
                alerts::RULE_COST_ANOMALY => div()
                    .h_flex()
                    .gap_4()
                    .child(
                        div()
                            .flex_1()
                            .v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr!("rules.multiplier")))
                            .child(Input::new(&self.multiplier_input)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr!("rules.consecutive_days")))
                            .child(Input::new(&self.consecutive_input)),
                    ),
                alerts::RULE_BALANCE_FLOOR => div().h_flex().child(
                    div()
                        .flex_1()
                        .v_flex()
                        .gap_1()
                        .child(div().text_sm().child(tr!("rules.floor")))
                        .child(Input::new(&self.floor_input)),
                ),
                alerts::RULE_BUDGET => {
                    let account_picker = div()
                        .v_flex()
                        .gap_1()
                        .child(div().text_sm().child(tr!("rules.account")))
                        .when(self.accounts.is_empty(), |el| {
                            el.child(theme::caption(cx, tr!("rules.no_accounts")))
                        })
                        .child(div().h_flex().gap_2().flex_wrap().children(
                            self.accounts.iter().map(|(id, name)| {
                                let id = id.clone();
                                let selected = self.rule_account.as_deref() == Some(id.as_str());
                                Button::new(SharedString::from(format!("rule-account-{id}")))
                                    .label(name.clone())
                                    .when(selected, |button| button.primary())
                                    .when(!selected, |button| {
                                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.rule_account = Some(id.clone());
                                        cx.notify();
                                    }))
                            }),
                        ));

                    let based_picker = div()
                        .flex_1()
                        .v_flex()
                        .gap_1()
                        .child(div().text_sm().child(tr!("rules.based_on")))
                        .child(div().h_flex().gap_2().children(BASED_OPTIONS.iter().map(
                            |(value, label)| {
                                let value = *value;
                                let selected = self.rule_based == value;
                                Button::new(SharedString::from(format!("rule-based-{value}")))
                                    .label(tr!(*label))
                                    .when(selected, |button| button.primary())
                                    .when(!selected, |button| {
                                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.rule_based = value;
                                        cx.notify();
                                    }))
                            },
                        )));

                    let threshold_type_picker =
                        div()
                            .flex_1()
                            .v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr!("rules.threshold_type")))
                            .child(div().h_flex().gap_2().children(
                                THRESHOLD_TYPE_OPTIONS.iter().map(|(value, label)| {
                                    let value = *value;
                                    let selected = self.rule_threshold_type == value;
                                    Button::new(SharedString::from(format!(
                                        "rule-threshold-type-{value}"
                                    )))
                                    .label(tr!(*label))
                                    .when(selected, |button| button.primary())
                                    .when(!selected, |button| {
                                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        // 80 would silently go from a percent
                                        // to an amount, so the unit change
                                        // clears it.
                                        if this.rule_threshold_type != value {
                                            this.threshold_input.update(cx, |state, cx| {
                                                state.set_value("", window, cx);
                                            });
                                        }
                                        this.rule_threshold_type = value;
                                        cx.notify();
                                    }))
                                }),
                            ));

                    div()
                        .v_flex()
                        .gap_4()
                        .child(account_picker)
                        .child(
                            div()
                                .h_flex()
                                .gap_4()
                                .child(based_picker)
                                .child(threshold_type_picker),
                        )
                        .child(
                            div().h_flex().child(
                                div()
                                    .flex_1()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child(
                                        if self.rule_threshold_type == "absolute" {
                                            tr!(
                                                "rules.amount_reporting",
                                                currency = data::reporting_currency()
                                            )
                                        } else {
                                            tr!("rules.percent_of_budget")
                                        },
                                    ))
                                    .child(Input::new(&self.threshold_input)),
                            ),
                        )
                }
                _ => div().h_flex().child(
                    div()
                        .flex_1()
                        .v_flex()
                        .gap_1()
                        .child(div().text_sm().child(tr!("rules.threshold_pct")))
                        .child(Input::new(&self.threshold_input)),
                ),
            };

        let view = cx.entity().downgrade();
        dialog
            .title(tr!("rules.new_rule_title"))
            .w(rems(32.).to_pixels(window.rem_size()))
            .max_h(rems(37.5))
            .on_close(cx.listener(|this, _, _, cx| this.new_dialog_closed(cx)))
            .on_ok(move |_, window, cx| {
                view.update(cx, |this, cx| this.submit_new_rule(window, cx))
                    .ok();
                false
            })
            .child(
                div()
                    .v_flex()
                    .gap_4()
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr!("rules.kind")))
                            .child(self.render_kind_selector(cx)),
                    )
                    .child(
                        div()
                            .v_flex()
                            .gap_1()
                            .child(div().text_sm().child(tr!("accounts.name")))
                            .child(Input::new(&self.name_input)),
                    )
                    .child(parameters)
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::text_muted(cx))
                            .child(tr!("rules.enabled_on_creation")),
                    ),
            )
            .footer(
                DialogFooter::new()
                    .gap_2()
                    // Beside the buttons rather than at the end of the body,
                    // which scrolls: an error below the fold reads as a
                    // button that did nothing.
                    .when_some(self.dialog_error.clone(), |el, error| {
                        el.child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_sm()
                                .line_height(relative(1.4))
                                .text_color(theme::danger(cx))
                                .child(error),
                        )
                    })
                    .child(
                        Button::new("cancel-new-rule")
                            .label(tr!("common.cancel"))
                            .ghost()
                            .on_click(|_, window, cx| window.dispatch_action(Box::new(Cancel), cx)),
                    )
                    .child(
                        Button::new("save-new-rule")
                            .label(tr!("rules.create_rule"))
                            .primary()
                            .disabled(self.creating)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.submit_new_rule(window, cx);
                            })),
                    ),
            )
    }

    /// Confirm deleting the pending rule, by name. Escape and Cancel leave
    /// it be.
    fn delete_confirm(&mut self, alert: AlertDialog, cx: &mut Context<Self>) -> AlertDialog {
        let name = self
            .pending_delete
            .as_ref()
            .and_then(|id| {
                self.data
                    .as_ref()
                    .and_then(|data| data.rules.iter().find(|rule| &rule.id == id))
            })
            .map(|rule| tr!("rules.quoted", name = rule.name).to_string())
            .unwrap_or_else(|| tr!("rules.this_rule").into());
        let view = cx.entity().downgrade();

        alert
            .title(tr!("rules.delete_title", name = name))
            .description(tr!("rules.delete_body"))
            .button_props(
                DialogButtonProps::default()
                    .ok_text(tr!("common.delete"))
                    .ok_variant(ButtonVariant::Danger)
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                view.update(cx, |this, cx| this.confirm_delete(cx)).ok();
                true
            })
            .on_close(cx.listener(|this, _, _, cx| {
                this.pending_delete = None;
                cx.notify();
            }))
    }
}

impl Render for RulesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = div()
            .h_flex()
            .justify_between()
            .items_center()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, tr!("nav.rules")))
                    .child(theme::caption(
                        cx,
                        "Every rule runs on the local ledger after each ingest. \
                         Nothing leaves the machine.",
                    )),
            )
            .child(
                Button::new("new-rule")
                    .primary()
                    .label(tr!("rules.new_rule"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.show_new_dialog(window, cx);
                    })),
            );

        let body: AnyElement = if self.loading && self.data.is_none() {
            // Skeleton shaped like the loaded list: three rule cards at
            // their final padding, so the first load does not jump.
            div()
                .v_flex()
                .gap_4()
                .children((0..3).map(|_| {
                    theme::card(cx)
                        .w_full()
                        .p_5()
                        .v_flex()
                        .gap_3()
                        .child(Skeleton::new().w_56().h_5())
                        .child(Skeleton::new().w_full().h_3())
                        .child(Skeleton::new().w_64().h_3())
                }))
                .into_any_element()
        } else if let Some(data) = &self.data {
            let rules: &[RuleView] = data.rules.as_slice();
            div()
                .id("rules-list")
                .flex_1()
                .min_w_0()
                .min_h_0()
                .v_flex()
                .gap_4()
                .overflow_y_scroll()
                .when_some(self.error.clone(), |el, error| {
                    el.child(
                        div()
                            .w_full()
                            .p_3()
                            .rounded_md()
                            .bg(theme::danger_bg(cx))
                            .h_flex()
                            .items_center()
                            .justify_between()
                            .gap_3()
                            .child(div().text_sm().text_color(theme::danger(cx)).child(error))
                            .child(
                                Button::new("retry-load-rules")
                                    .label(tr!("common.retry"))
                                    .custom(theme::outline_variant(cx))
                                    .card_outline(cx)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.load(cx);
                                    })),
                            ),
                    )
                })
                .when(rules.is_empty(), |el| {
                    el.child(
                        theme::card(cx).p_3().child(
                            div()
                                .text_sm()
                                .text_color(theme::text_muted(cx))
                                .child(tr!("rules.empty")),
                        ),
                    )
                })
                .child(self.render_budgets(cx))
                .children(rules.iter().map(|rule| self.render_rule(rule, cx)))
                .into_any_element()
        } else if let Some(error) = &self.error {
            // First load failed: there is no list to keep on screen.
            theme::card(cx)
                .w_full()
                .p_5()
                .v_flex()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::danger(cx))
                        .child(error.clone()),
                )
                .child(
                    div().h_flex().child(
                        Button::new("retry-first-load-rules")
                            .label(tr!("common.retry"))
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.load(cx);
                            })),
                    ),
                )
                .into_any_element()
        } else {
            div().size_0().into_any_element()
        };

        div()
            .size_full()
            .relative()
            .v_flex()
            .gap_6()
            .p_8()
            .bg(theme::app_bg(cx))
            .child(header)
            .child(body)
    }
}
