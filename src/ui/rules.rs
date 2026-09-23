//! Rules View — alerting rules that run on the local ledger after each ingest.

use chrono::{DateTime, Utc};
use gpui_kit::component::{
    button::*,
    input::{Input, InputState},
    skeleton::Skeleton,
    switch::*,
    Disableable as _, IconName, Sizable as _, StyledExt,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::json;

use crate::alerts::{self, RuleView};
use crate::cloud::BudgetStatus;
use crate::ui::theme::CardOutline as _;

use super::{data, fmt, theme};

actions!(rules, [CloseRulesDialog]);

/// Key context both dialogs share, so Escape closes whichever is open.
const RULES_DIALOG_CONTEXT: &str = "RulesDialog";

/// One selectable rule kind in the "New rule" dialog.
struct RuleKindSpec {
    kind: &'static str,
    /// Default name, also the selector label.
    name: &'static str,
}

const RULE_KINDS: [RuleKindSpec; 4] = [
    RuleKindSpec {
        kind: alerts::RULE_COST_ANOMALY,
        name: "Model cost growth anomaly",
    },
    RuleKindSpec {
        kind: alerts::RULE_BALANCE_FLOOR,
        name: "Balance floor",
    },
    RuleKindSpec {
        kind: alerts::RULE_UNTAGGED_RATIO,
        name: "Untagged spend ratio",
    },
    RuleKindSpec {
        kind: alerts::RULE_BUDGET,
        name: "Account budget",
    },
];

/// What a budget rule compares against its threshold, as selector labels
/// and config values.
const BASED_OPTIONS: [(&str, &str); 2] =
    [("cost", "Cost to date"), ("forecast", "Month-end forecast")];

/// A budget rule's threshold type, as selector labels and config values.
const THRESHOLD_TYPE_OPTIONS: [(&str, &str); 2] =
    [("percent", "% of budget"), ("absolute", "Amount")];

/// Humanized "last fired" line under a rule's switch.
fn last_fired_label(last_fired_at: Option<DateTime<Utc>>) -> String {
    match last_fired_at {
        Some(at) => format!("Fired {}", fmt::relative_time(at)),
        None => "Never fired".to_string(),
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
    /// Focus anchor both dialogs track, so Escape reaches them.
    dialog_focus: FocusHandle,
    /// Whether the "New rule" dialog is open.
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
    /// A delete is in flight.
    deleting: bool,
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

        // Escape-to-close for the dialogs. gpui-component's own Cancel
        // action is crate-private, so the dialogs get their own action and
        // context. Registered once: bind_keys appends, and the view may be
        // rebuilt on every navigation.
        static BIND_KEYS: std::sync::Once = std::sync::Once::new();
        BIND_KEYS.call_once(|| {
            cx.bind_keys([KeyBinding::new(
                "escape",
                CloseRulesDialog,
                Some(RULES_DIALOG_CONTEXT),
            )]);
        });

        Self {
            data: None,
            budgets: Vec::new(),
            accounts: Vec::new(),
            loading: false,
            load_generation: 0,
            error: None,
            dialog_focus: cx.focus_handle(),
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
            deleting: false,
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
                            this.error = Some(format!("Could not load rules: {e}"));
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
                        this.error = Some(format!("Could not update the rule: {e}"));
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
        self.dialog_focus.focus(window, cx);
        self.set_kind(RULE_KINDS[0].kind, window, cx);
    }

    fn hide_new_dialog(&mut self, cx: &mut Context<Self>) {
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
            state.set_value(spec.name, window, cx);
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
                    .map_err(|_| "The multiplier must be a number, e.g. 2.5".to_string())?;
                let consecutive_days = value(&self.consecutive_input)
                    .parse::<u64>()
                    .map_err(|_| "Consecutive days must be a whole number, e.g. 2".to_string())?;
                Ok(json!({
                    "multiplier": multiplier,
                    "consecutive_days": consecutive_days,
                }))
            }
            alerts::RULE_BALANCE_FLOOR => {
                let floor = value(&self.floor_input)
                    .parse::<f64>()
                    .map_err(|_| "The floor must be a number, e.g. 200".to_string())?;
                Ok(json!({ "floor": floor }))
            }
            alerts::RULE_BUDGET => {
                let account_id = self
                    .rule_account
                    .clone()
                    .ok_or_else(|| "Pick the account the rule watches".to_string())?;
                let threshold = value(&self.threshold_input)
                    .parse::<f64>()
                    .map_err(|_| "The threshold must be a number, e.g. 80".to_string())?;
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
                    .map_err(|_| "The threshold must be a number, e.g. 15".to_string())?;
                Ok(json!({ "threshold": percent / 100.0 }))
            }
        }
    }

    /// Create the rule off-thread, evaluate right away so a condition that
    /// already holds fires immediately, then reload the list.
    fn submit_new_rule(&mut self, cx: &mut Context<Self>) {
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
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                data::create_rule(kind, &name, config).map_err(|e| e.to_string())
            })
            .await;
            if result.is_ok() {
                let _ = smol::unblock(data::evaluate_rules).await;
            }

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.creating = false;
                    match result {
                        Ok(_) => {
                            this.show_new_dialog = false;
                            this.dialog_error = None;
                            this.load(cx);
                        }
                        Err(e) => {
                            this.dialog_error = Some(e);
                            cx.notify();
                        }
                    }
                })
                .ok();
            });
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
                self.budget_error =
                    Some("The budget must be a number above zero, e.g. 500".to_string());
                cx.notify();
                return;
            }
        };
        let alert_threshold = match value(&self.budget_threshold_input).parse::<f64>() {
            Ok(percent) if percent > 0.0 => percent,
            _ => {
                self.budget_error =
                    Some("The alert threshold must be a percent above zero, e.g. 80".to_string());
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
                        this.budget_error = Some(format!("Could not save the budget: {e}"));
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
                        this.budget_error = Some(format!("Could not remove the budget: {e}"));
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
        self.dialog_focus.focus(window, cx);
        cx.notify();
    }

    fn cancel_delete(&mut self, cx: &mut Context<Self>) {
        self.pending_delete = None;
        cx.notify();
    }

    fn confirm_delete(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.pending_delete.take() else {
            return;
        };

        self.deleting = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result =
                smol::unblock(move || data::delete_rule(&id).map_err(|e| e.to_string())).await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.deleting = false;
                    if let Err(e) = result {
                        this.error = Some(format!("Could not delete the rule: {e}"));
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
                                .label("Delete")
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
                        .small()
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
                    .child(div().text_sm().child("Monthly budget"))
                    .child(Input::new(&self.budget_amount_input)),
            )
            .child(
                div()
                    .w_40()
                    .v_flex()
                    .gap_1()
                    .child(div().text_sm().child("Alert at (% of budget)"))
                    .child(Input::new(&self.budget_threshold_input)),
            )
            .child(
                Button::new("save-budget")
                    .label("Save budget")
                    .primary()
                    .small()
                    .disabled(self.saving_budget || self.budget_account.is_none())
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_budget(cx);
                    })),
            )
            .when(has_budget, |el| {
                el.child(
                    Button::new("delete-budget")
                        .label("Remove")
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
                    .child("Monthly budgets"),
            )
            .child(div().text_sm().text_color(theme::text_muted(cx)).child(
                "What an account may cost in a month. Budget rules measure the \
                         month-to-date cost or the month-end projection against it.",
            ))
            .when(self.accounts.is_empty(), |el| {
                el.child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_3()
                        .child(theme::caption(cx, "A budget belongs to an account."))
                        .child(
                            Button::new("budget-add-account")
                                .label("Add account…")
                                .small()
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
                let line = format!(
                    "{} — {} of {} used ({:.0}%)",
                    status.account_name,
                    fmt::amount(status.current_cost, &status.currency),
                    fmt::amount(status.monthly_budget, &status.currency),
                    status.percentage_used,
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
                    .label(spec.name)
                    .small()
                    .when(kind == self.selected_kind, |button| button.primary())
                    .when(kind != self.selected_kind, |button| {
                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_kind(kind, window, cx);
                    }))
            }))
    }

    fn render_new_dialog(&self, cx: &Context<Self>) -> AnyElement {
        if !self.show_new_dialog {
            return div().size_0().into_any_element();
        }

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
                            .child(div().text_sm().child("Multiplier (× 7-day baseline)"))
                            .child(Input::new(&self.multiplier_input)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .v_flex()
                            .gap_1()
                            .child(div().text_sm().child("Consecutive days"))
                            .child(Input::new(&self.consecutive_input)),
                    ),
                alerts::RULE_BALANCE_FLOOR => div().h_flex().child(
                    div()
                        .flex_1()
                        .v_flex()
                        .gap_1()
                        .child(div().text_sm().child("Floor (account's currency)"))
                        .child(Input::new(&self.floor_input)),
                ),
                alerts::RULE_BUDGET => {
                    let account_picker = div()
                        .v_flex()
                        .gap_1()
                        .child(div().text_sm().child("Account"))
                        .when(self.accounts.is_empty(), |el| {
                            el.child(theme::caption(cx, "No accounts yet — add one first."))
                        })
                        .child(div().h_flex().gap_2().flex_wrap().children(
                            self.accounts.iter().map(|(id, name)| {
                                let id = id.clone();
                                let selected = self.rule_account.as_deref() == Some(id.as_str());
                                Button::new(SharedString::from(format!("rule-account-{id}")))
                                    .label(name.clone())
                                    .small()
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
                        .child(div().text_sm().child("Based on"))
                        .child(div().h_flex().gap_2().children(BASED_OPTIONS.iter().map(
                            |(value, label)| {
                                let value = *value;
                                let selected = self.rule_based == value;
                                Button::new(SharedString::from(format!("rule-based-{value}")))
                                    .label(*label)
                                    .small()
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
                            .child(div().text_sm().child("Threshold type"))
                            .child(div().h_flex().gap_2().children(
                                THRESHOLD_TYPE_OPTIONS.iter().map(|(value, label)| {
                                    let value = *value;
                                    let selected = self.rule_threshold_type == value;
                                    Button::new(SharedString::from(format!(
                                        "rule-threshold-type-{value}"
                                    )))
                                    .label(*label)
                                    .small()
                                    .when(selected, |button| button.primary())
                                    .when(!selected, |button| {
                                        button.custom(theme::outline_variant(cx)).card_outline(cx)
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
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
                                            "Amount (reporting currency)"
                                        } else {
                                            "Percent of budget (%)"
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
                        .child(div().text_sm().child("Threshold (%)"))
                        .child(Input::new(&self.threshold_input)),
                ),
            };

        div()
            .id("new-rule-scrim")
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim(cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.hide_new_dialog(cx);
                }),
            )
            .child(
                div()
                    .id("new-rule-dialog")
                    // occlude: clicks on the panel must not reach the
                    // dismiss-on-click scrim behind it.
                    .occlude()
                    .key_context(RULES_DIALOG_CONTEXT)
                    .track_focus(&self.dialog_focus)
                    .on_action(cx.listener(|this, _: &CloseRulesDialog, _, cx| {
                        this.hide_new_dialog(cx);
                        cx.stop_propagation();
                    }))
                    // When an input inside is focused, its own Escape
                    // binding wins the keystroke but re-propagates, so the
                    // raw key is caught here on the bubble.
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.hide_new_dialog(cx);
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
                                    .child("New rule"),
                            )
                            .child(
                                Button::new("close-new-rule")
                                    .icon(IconName::Close)
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.hide_new_dialog(cx);
                                    })),
                            ),
                    )
                    // The body scrolls so the footer buttons below stay
                    // reachable no matter how tall the parameters grow.
                    .child(
                        div()
                            .id("new-rule-body")
                            .flex_1()
                            .min_h_0()
                            .v_flex()
                            .gap_4()
                            .overflow_y_scroll()
                            .child(
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("Kind"))
                                    .child(self.render_kind_selector(cx)),
                            )
                            .child(
                                div()
                                    .v_flex()
                                    .gap_1()
                                    .child(div().text_sm().child("Name"))
                                    .child(Input::new(&self.name_input)),
                            )
                            .child(parameters)
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::text_muted(cx))
                                    .child("The rule is enabled on creation."),
                            )
                            .when_some(self.dialog_error.clone(), |el, error| {
                                el.child(div().text_sm().text_color(theme::danger(cx)).child(error))
                            }),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("cancel-new-rule")
                                    .label("Cancel")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.hide_new_dialog(cx);
                                    })),
                            )
                            .child(
                                Button::new("save-new-rule")
                                    .label("Create rule")
                                    .primary()
                                    .disabled(self.creating)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.submit_new_rule(cx);
                                    })),
                            ),
                    )
                    .with_animation(
                        "new-rule-dialog-enter",
                        theme::dialog_enter_animation(),
                        |this, delta| this.opacity(delta).mt(px(10.0 * (1.0 - delta))),
                    ),
            )
            .into_any_element()
    }

    fn render_delete_confirm(&self, cx: &Context<Self>) -> AnyElement {
        let Some(id) = &self.pending_delete else {
            return div().size_0().into_any_element();
        };

        let name = self
            .data
            .as_ref()
            .and_then(|data| data.rules.iter().find(|rule| &rule.id == id))
            .map(|rule| rule.name.clone())
            .unwrap_or_else(|| "this rule".to_string());

        div()
            .id("delete-rule-scrim")
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim(cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cancel_delete(cx);
                }),
            )
            .child(
                div()
                    .id("delete-rule-dialog")
                    // occlude: clicks on the panel must not reach the
                    // dismiss-on-click scrim behind it.
                    .occlude()
                    .key_context(RULES_DIALOG_CONTEXT)
                    .track_focus(&self.dialog_focus)
                    .on_action(cx.listener(|this, _: &CloseRulesDialog, _, cx| {
                        this.cancel_delete(cx);
                        cx.stop_propagation();
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.cancel_delete(cx);
                            cx.stop_propagation();
                        }
                    }))
                    .w_112()
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
                            .h_flex()
                            .justify_between()
                            .items_center()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child("Delete rule"),
                            )
                            .child(
                                Button::new("close-delete-rule")
                                    .icon(IconName::Close)
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_delete(cx);
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(theme::text_muted(cx))
                            .child(format!(
                                "Delete \"{name}\"? Its past alerts stay; the rule stops running."
                            )),
                    )
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("cancel-delete-rule")
                                    .label("Cancel")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.cancel_delete(cx);
                                    })),
                            )
                            .child(
                                Button::new("confirm-delete-rule")
                                    .label("Delete")
                                    .danger()
                                    .disabled(self.deleting)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_delete(cx);
                                    })),
                            ),
                    )
                    .with_animation(
                        "delete-rule-dialog-enter",
                        theme::dialog_enter_animation(),
                        |this, delta| this.opacity(delta).mt(px(10.0 * (1.0 - delta))),
                    ),
            )
            .into_any_element()
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
                    .child(theme::page_title(cx, "Rules"))
                    .child(theme::caption(
                        cx,
                        "Every rule runs on the local ledger after each ingest. \
                         Nothing leaves the machine.",
                    )),
            )
            .child(
                Button::new("new-rule")
                    .primary()
                    .label("New rule")
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
                                    .label("Retry")
                                    .small()
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
                                .child("No rules yet. Create one with New rule in the top right."),
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
                            .label("Retry")
                            .small()
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
            .child(self.render_new_dialog(cx))
            .child(self.render_delete_confirm(cx))
    }
}
