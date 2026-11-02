//! Settings → Insights rules: which script rules Insights runs, and the
//! community catalog to install more from. Desktop only.
//!
//! The folder is the truth: a rule is in effect when its `.js` file is in
//! it, however it got there. The catalog is read only when someone asks
//! (Browse catalog), so opening Settings makes no network request. An
//! install or update shows as the row's new status, so it needs no message
//! of its own; removing someone's own rule, or replacing an edited one,
//! cannot be undone and asks first.

use gpui_kit::component::{
    button::*,
    dialog::{AlertDialog, DialogButtonProps},
    Disableable as _, Sizable as _, StyledExt,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{dialogs, theme};
use crate::insight_catalog::{
    self, CatalogEntry, CatalogIndex, InstallRecord, LocalRule, RuleRow, RuleStatus,
};
use crate::ui::theme::CardOutline as _;

/// A change to the folder that cannot be taken back, waiting on its
/// confirmation.
#[derive(Debug, Clone, PartialEq)]
enum Pending {
    /// Delete a rule the catalog cannot restore.
    Delete(String),
    /// Overwrite an edited rule with the catalog's.
    Replace(CatalogEntry),
}

pub struct InsightRulesView {
    local: Vec<LocalRule>,
    record: InstallRecord,
    /// The catalog, once someone has browsed it.
    index: Option<CatalogIndex>,
    fetching: bool,
    /// The rule an install or removal is running for.
    busy: Option<String>,
    error: Option<String>,
    pending: Option<Pending>,
}

impl InsightRulesView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            local: Vec::new(),
            record: InstallRecord::default(),
            index: None,
            fetching: false,
            busy: None,
            error: None,
            pending: None,
        };
        view.reload_local(cx);
        view
    }

    /// Read the folder again: on opening, and after every change to it.
    fn reload_local(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let read = smol::unblock(insight_catalog::read_local).await;
            this.update(cx, |this, cx| {
                match read {
                    Ok((local, record)) => {
                        this.local = local;
                        this.record = record;
                    }
                    Err(e) => {
                        this.error =
                            Some(tr!("insight_rules.read_failed", error = format!("{e:#}")).into())
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn browse(&mut self, cx: &mut Context<Self>) {
        if self.fetching {
            return;
        }
        self.fetching = true;
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let fetched = smol::unblock(insight_catalog::fetch_index).await;
            this.update(cx, |this, cx| {
                this.fetching = false;
                match fetched {
                    Ok(index) => this.index = Some(index),
                    Err(e) => {
                        this.error =
                            Some(tr!("insight_rules.fetch_failed", error = format!("{e:#}")).into())
                    }
                }
                cx.notify();
            })
            .ok();
            this.update(cx, |this, cx| this.reload_local(cx)).ok();
        })
        .detach();
    }

    /// Run `change` on the folder for rule `id`, then read the folder again.
    fn change(
        &mut self,
        id: String,
        what: &'static str,
        change: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
        cx: &mut Context<Self>,
    ) {
        if self.busy.is_some() {
            return;
        }
        self.busy = Some(id.clone());
        self.error = None;
        cx.notify();
        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(change).await;
            this.update(cx, |this, cx| {
                this.busy = None;
                if let Err(e) = outcome {
                    tracing::error!("Couldn't {what} insight rule {id}: {e:#}");
                    this.error = Some(
                        match what {
                            "install" => tr!(
                                "insight_rules.install_failed",
                                id = id,
                                error = format!("{e:#}")
                            ),
                            _ => tr!(
                                "insight_rules.remove_failed",
                                id = id,
                                error = format!("{e:#}")
                            ),
                        }
                        .into(),
                    );
                }
                this.reload_local(cx);
            })
            .ok();
        })
        .detach();
    }

    fn install(&mut self, entry: CatalogEntry, cx: &mut Context<Self>) {
        let id = entry.id.clone();
        self.change(id, "install", move || insight_catalog::install(&entry), cx);
    }

    fn remove(&mut self, id: String, cx: &mut Context<Self>) {
        let target = id.clone();
        self.change(id, "remove", move || insight_catalog::remove(&target), cx);
    }

    fn open_folder(&mut self, cx: &mut Context<Self>) {
        match insight_catalog::ensure_rules_dir() {
            Ok(dir) => cx.open_with_system(&dir),
            Err(e) => {
                self.error =
                    Some(tr!("insight_rules.open_failed", error = format!("{e:#}")).into());
                cx.notify();
            }
        }
    }

    fn confirm(&mut self, pending: Pending, window: &mut Window, cx: &mut Context<Self>) {
        self.pending = Some(pending);
        dialogs::open_alert(window, cx, |this, alert, _, cx| {
            this.render_confirm(alert, cx)
        });
    }

    fn render_confirm(&mut self, alert: AlertDialog, cx: &mut Context<Self>) -> AlertDialog {
        let view = cx.entity().downgrade();
        let (title, description, ok) = match &self.pending {
            Some(Pending::Delete(id)) => (
                tr!("insight_rules.delete_title", id = id).to_string(),
                tr!("insight_rules.delete_body").to_string(),
                tr!("common.delete"),
            ),
            Some(Pending::Replace(entry)) => (
                tr!("insight_rules.replace_title", id = entry.id).to_string(),
                tr!("insight_rules.replace_body", version = entry.version).to_string(),
                tr!("insight_rules.replace"),
            ),
            None => (String::new(), String::new(), SharedString::from("OK")),
        };
        alert
            .title(title)
            .description(description)
            .button_props(
                DialogButtonProps::default()
                    .ok_text(ok)
                    .ok_variant(ButtonVariant::Danger)
                    .show_cancel(true),
            )
            .on_ok(move |_, _, cx| {
                view.update(cx, |this, cx| match this.pending.take() {
                    Some(Pending::Delete(id)) => this.remove(id, cx),
                    Some(Pending::Replace(entry)) => this.install(entry, cx),
                    None => {}
                })
                .ok();
                true
            })
            .on_close(cx.listener(|this, _, _, cx| {
                this.pending = None;
                cx.notify();
            }))
    }

    fn render_row(&self, row: &RuleRow, cx: &Context<Self>) -> Div {
        let entry = row.entry.as_ref();
        let busy = self.busy.as_deref() == Some(row.id.as_str());
        let idle = self.busy.is_none();
        let title = entry
            .map(|e| e.title.clone())
            .unwrap_or_else(|| format!("{}.js", row.id));
        let meta = entry.map(|e| {
            let providers = if e.providers.is_empty() {
                String::new()
            } else {
                tr!(
                    "insight_rules.for_providers",
                    providers = e.providers.join(", ")
                )
                .to_string()
            };
            tr!(
                "insight_rules.version_by",
                version = e.version,
                author = e.author,
                providers = providers
            )
            .to_string()
        });
        let status: Option<String> = match &row.status {
            RuleStatus::Available => None,
            RuleStatus::UpToDate => Some(tr!("insight_rules.installed").into()),
            RuleStatus::UpdateAvailable { installed } => {
                Some(tr!("insight_rules.update_available", installed = installed).into())
            }
            RuleStatus::EditedLocally => Some(tr!("insight_rules.edited").into()),
            RuleStatus::Local if entry.is_some() => Some(tr!("insight_rules.name_clash").into()),
            RuleStatus::Local => Some(tr!("insight_rules.yours").into()),
            RuleStatus::Withdrawn => Some(tr!("insight_rules.withdrawn").into()),
            RuleStatus::NeedsNewerApp => Some(tr!("insight_rules.needs_newer").into()),
        };

        // `action` names the button's id; the label is its translation.
        let button = |id: &str, action: &'static str| {
            Button::new(SharedString::from(format!("insight-rule-{id}-{action}")))
                .label(match action {
                    "install" => tr!("insight_rules.install"),
                    "update" => tr!("insight_rules.update"),
                    "replace" => tr!("insight_rules.replace_ellipsis"),
                    "remove" => tr!("rules.remove"),
                    _ => tr!("insight_rules.delete_ellipsis"),
                })
                .disabled(!idle)
        };
        let id = row.id.clone();
        let mut actions: Vec<AnyElement> = Vec::new();
        match (&row.status, entry) {
            (RuleStatus::Available, Some(e)) | (RuleStatus::UpdateAvailable { .. }, Some(e)) => {
                let e = e.clone();
                let label = if row.status == RuleStatus::Available {
                    "install"
                } else {
                    "update"
                };
                actions.push(
                    button(&id, label)
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .loading(busy)
                        .on_click(cx.listener(move |this, _, _, cx| this.install(e.clone(), cx)))
                        .into_any_element(),
                );
            }
            (RuleStatus::EditedLocally, Some(e)) => {
                let e = e.clone();
                actions.push(
                    button(&id, "replace")
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .loading(busy)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.confirm(Pending::Replace(e.clone()), window, cx)
                        }))
                        .into_any_element(),
                );
            }
            _ => {}
        }
        if row.installed {
            // The catalog can put back what it installed; anything else
            // is someone's own work, and goes only once confirmed.
            let restorable = matches!(
                row.status,
                RuleStatus::UpToDate | RuleStatus::UpdateAvailable { .. }
            );
            let id = id.clone();
            actions.push(if restorable {
                button(&row.id, "remove")
                    .ghost()
                    .loading(busy)
                    .on_click(cx.listener(move |this, _, _, cx| this.remove(id.clone(), cx)))
                    .into_any_element()
            } else {
                button(&row.id, "delete")
                    .ghost()
                    .loading(busy && !matches!(row.status, RuleStatus::EditedLocally))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.confirm(Pending::Delete(id.clone()), window, cx)
                    }))
                    .into_any_element()
            });
        }

        div()
            .w_full()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_4()
            .py_3()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .v_flex()
                    .gap_0p5()
                    .child(div().child(title))
                    .when_some(entry.filter(|e| !e.explanation.is_empty()), |el, e| {
                        el.child(theme::caption(cx, e.explanation.clone()))
                    })
                    .when_some(meta, |el, meta| el.child(theme::caption(cx, meta))),
            )
            .child(
                div()
                    .flex_none()
                    .h_flex()
                    .items_center()
                    .gap_3()
                    .when_some(status, |el, status| {
                        el.child(
                            div()
                                .text_sm()
                                .text_color(theme::text_muted(cx))
                                .child(status),
                        )
                    })
                    .children(actions),
            )
    }
}

impl Render for InsightRulesView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = insight_catalog::rows(self.index.as_ref(), &self.local, &self.record);
        let browsed = self.index.is_some();

        div()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .gap_4()
                    .child(div().min_w_0().v_flex().child(theme::caption(
                        cx,
                        "Each rule adds a section to Insights. Rules run on this computer, \
                             reading only this period's bill and scan, with no network or file \
                             access. Add your own by putting a .js file in the folder.",
                    )))
                    .child(
                        div()
                            .flex_none()
                            .h_flex()
                            .gap_2()
                            .child(
                                Button::new("insight-rules-browse")
                                    .label(if browsed {
                                        tr!("insight_rules.check_updates")
                                    } else {
                                        tr!("insight_rules.browse")
                                    })
                                    .custom(theme::outline_variant(cx))
                                    .card_outline(cx)
                                    .loading(self.fetching)
                                    .disabled(self.fetching)
                                    .on_click(cx.listener(|this, _, _, cx| this.browse(cx))),
                            )
                            .child(
                                Button::new("insight-rules-folder")
                                    .label(tr!("insight_rules.open_folder"))
                                    .custom(theme::outline_variant(cx))
                                    .card_outline(cx)
                                    .on_click(cx.listener(|this, _, _, cx| this.open_folder(cx))),
                            ),
                    ),
            )
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .w_full()
                        .p_3()
                        .rounded_md()
                        .bg(theme::danger_bg(cx))
                        .text_sm()
                        .text_color(theme::danger(cx))
                        .child(error),
                )
            })
            .map(|el| {
                if rows.is_empty() {
                    el.child(theme::caption(
                        cx,
                        if browsed {
                            tr!("insight_rules.empty_browsed")
                        } else {
                            tr!("insight_rules.empty")
                        },
                    ))
                } else {
                    el.child(
                        div()
                            .v_flex()
                            .children(rows.iter().map(|row| self.render_row(row, cx))),
                    )
                }
            })
            .child(
                div().child(
                    Button::new("insight-rules-contribute")
                        .label(tr!("insight_rules.contribute"))
                        .link()
                        .small()
                        .on_click(|_, _, cx| cx.open_url(insight_catalog::CATALOG_HOME)),
                ),
            )
    }
}
