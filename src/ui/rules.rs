//! Rules View — alerting rules that run on the local ledger after each ingest.

use chrono::{DateTime, Utc};
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{
    button::*,
    input::{Input, InputState},
    switch::*,
    ActiveTheme as _, Disableable as _, Sizable as _, StyledExt,
};
use serde_json::json;

use crate::alerts::{self, RuleView};

use super::{data, theme};

/// One selectable rule kind in the "New rule" dialog.
struct RuleKindSpec {
    kind: &'static str,
    /// Default name, also the selector label.
    name: &'static str,
}

const RULE_KINDS: [RuleKindSpec; 3] = [
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
];

/// Background for the condition / delivery chips.
fn chip_bg(cx: &App) -> Hsla {
    cx.theme().secondary
}

/// A rounded condition or delivery chip.
fn chip(cx: &App, text: &str, mono: bool) -> Div {
    let el = div()
        .px_2()
        .py_1()
        .rounded_md()
        .bg(chip_bg(cx))
        .text_sm()
        .text_color(theme::text_primary(cx))
        .child(text.to_string());
    if mono {
        el.font_family("monospace")
    } else {
        el
    }
}

/// Humanized "last fired" line under a rule's switch.
fn last_fired_label(last_fired_at: Option<DateTime<Utc>>) -> String {
    let Some(at) = last_fired_at else {
        return "Never fired".to_string();
    };
    let elapsed = (Utc::now() - at).num_minutes().max(0);
    if elapsed < 60 {
        format!("Fired {elapsed} min ago")
    } else if elapsed < 48 * 60 {
        format!("Fired {} h ago", elapsed / 60)
    } else if elapsed < 7 * 24 * 60 {
        format!("Fired {} d ago", elapsed / (24 * 60))
    } else {
        format!("Fired {}", at.format("%Y-%m-%d"))
    }
}

/// Rules View
pub struct RulesView {
    /// Loaded rules, once the first load lands.
    data: Option<data::RulesData>,
    /// A load is in flight.
    loading: bool,
    /// The last load, toggle or delete failure, if any.
    error: Option<String>,
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

        let mut view = Self {
            data: None,
            loading: false,
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
            pending_delete: None,
            deleting: false,
        };
        view.load(cx);
        view
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
    /// runs on a worker thread like the Accounts page's loads do.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        cx.notify();

        cx.spawn(async move |this, cx| {
            let result = smol::unblock(|| data::load_rules().map_err(|e| e.to_string())).await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.loading = false;
                    match result {
                        Ok(rules) => {
                            this.data = Some(rules);
                        }
                        Err(e) => {
                            this.error = Some(format!("Could not load rules: {e}"));
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
            })
            .ok();
        })
        .detach();
    }

    // ==================== New rule dialog ====================

    fn show_new_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_new_dialog = true;
        self.dialog_error = None;
        self.error = None;
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
            state.set_value("15", window, cx);
        });

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
            })
            .ok();
        })
        .detach();
    }

    // ==================== Delete ====================

    fn ask_delete(&mut self, id: String, cx: &mut Context<Self>) {
        self.pending_delete = Some(id);
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
            })
            .ok();
        })
        .detach();
    }

    // ==================== Rendering ====================

    fn render_rule(&self, rule: &RuleView, cx: &Context<Self>) -> impl IntoElement {
        let name_row = div()
            .h_flex()
            .items_center()
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
            .gap_2()
            .flex_wrap()
            .children(
                rule.condition_chips
                    .iter()
                    .enumerate()
                    .map(|(i, text)| chip(cx, text, i == 0)),
            )
            .children(rule.delivery_chips.iter().map(|text| chip(cx, text, false)));

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
                                .small()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.ask_delete(rule_id.clone(), cx);
                                })),
                        )
                    }),
            )
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
                    .when(kind == self.selected_kind, |button| button.primary())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.set_kind(kind, window, cx);
                    }))
            }))
    }

    fn render_new_dialog(&self, cx: &Context<Self>) -> impl IntoElement {
        if !self.show_new_dialog {
            return div().size_0();
        }

        let parameters = match self.selected_kind {
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
                div()
                    .w(px(520.0))
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
                                    .child("New rule"),
                            )
                            .child(Button::new("close-new-rule").label("×").ghost().on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.hide_new_dialog(cx);
                                }),
                            )),
                    )
                    .child(
                        div()
                            .v_flex()
                            .gap_4()
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
                            .child(parameters),
                    )
                    .child(div().text_xs().text_color(theme::text_muted(cx)).child(
                        "The rule is enabled on creation and runs on the next \
                                    evaluation, right after it is saved.",
                    ))
                    .when_some(self.dialog_error.clone(), |el, error| {
                        el.child(div().text_sm().text_color(gpui::red()).child(error))
                    })
                    .child(
                        div()
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
                    ),
            )
    }

    fn render_delete_confirm(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(id) = &self.pending_delete else {
            return div().size_0();
        };

        let name = self
            .data
            .as_ref()
            .and_then(|data| data.rules.iter().find(|rule| &rule.id == id))
            .map(|rule| rule.name.clone())
            .unwrap_or_else(|| "this rule".to_string());

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
                div()
                    .w(px(420.0))
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
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child("Delete rule"),
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
                                    .primary()
                                    .disabled(self.deleting)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_delete(cx);
                                    })),
                            ),
                    ),
            )
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
            theme::caption(cx, "Loading rules…").into_any_element()
        } else if let Some(error) = &self.error {
            div()
                .w_full()
                .p_3()
                .rounded_md()
                .bg(theme::alert_tint(cx))
                .text_sm()
                .text_color(theme::accent(cx))
                .child(error.clone())
                .into_any_element()
        } else {
            let rules: &[RuleView] = self
                .data
                .as_ref()
                .map(|data| data.rules.as_slice())
                .unwrap_or(&[]);
            if rules.is_empty() {
                theme::caption(cx, "No rules yet.").into_any_element()
            } else {
                div()
                    .id("rules-list")
                    .flex_1()
                    .min_w_0()
                    .v_flex()
                    .gap_4()
                    .overflow_y_scroll()
                    .children(rules.iter().map(|rule| self.render_rule(rule, cx)))
                    .into_any_element()
            }
        };

        div()
            .size_full()
            .relative()
            .v_flex()
            .gap_6()
            .p(px(32.0))
            .bg(theme::app_bg(cx))
            .child(header)
            .child(body)
            .child(self.render_new_dialog(cx))
            .child(self.render_delete_confirm(cx))
    }
}
