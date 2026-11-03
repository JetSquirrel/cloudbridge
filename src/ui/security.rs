//! The Security configuration card on Insights.
//!
//! A section of its own, because a risk is not an amount: nothing here carries
//! a cost or adds up with the cost findings above it. It reports what was and
//! was not evaluated as plainly as what was found, so a partial scan cannot
//! look like zero risks.

use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::theme;
use crate::analytics::security::{
    NotEvaluatedReason, SecurityCheck, SecurityCoverage, SecurityOutcome, SecurityReport,
    SecurityRule, SecuritySeverity,
};
use crate::ui::theme::CardOutline as _;

/// A row's kind, which the status filter narrows to.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Finding,
    NotEvaluated,
    NoFinding,
}

fn status_of(check: &SecurityCheck) -> Status {
    match check.outcome {
        SecurityOutcome::Finding { .. } => Status::Finding,
        SecurityOutcome::NotEvaluated(_) => Status::NotEvaluated,
        SecurityOutcome::NoFinding { .. } => Status::NoFinding,
    }
}

fn status_label(status: Status) -> SharedString {
    match status {
        Status::Finding => tr!("security.status_finding"),
        Status::NotEvaluated => tr!("security.status_not_evaluated"),
        Status::NoFinding => tr!("security.status_no_finding"),
    }
}

fn rule_label(rule: SecurityRule) -> SharedString {
    match rule {
        SecurityRule::PublicSsh => tr!("security.rule_ssh"),
        SecurityRule::PublicRdp => tr!("security.rule_rdp"),
    }
}

fn severity_label(severity: SecuritySeverity) -> SharedString {
    match severity {
        SecuritySeverity::High => tr!("security.severity_high"),
    }
}

/// A row's identity, whatever the filter shows around it.
fn row_id(check: &SecurityCheck) -> String {
    let rule = match &check.outcome {
        SecurityOutcome::Finding { rule, .. } => rule.id(),
        SecurityOutcome::NotEvaluated(_) => "not-evaluated",
        SecurityOutcome::NoFinding { .. } => "no-finding",
    };
    format!("{}/{rule}", check.resource_id)
}

/// The card's view.
#[derive(Default)]
pub struct SecurityCard {
    report: Option<SecurityReport>,
    /// `None` shows every row.
    filter: Option<Status>,
    open: Option<String>,
}

impl SecurityCard {
    /// Show a fresh report, keeping the filter and the open row.
    pub fn show(&mut self, report: SecurityReport, cx: &mut Context<Self>) {
        self.report = Some(report);
        cx.notify();
    }

    fn toggle(&mut self, id: String, cx: &mut Context<Self>) {
        self.open = if self.open.as_ref() == Some(&id) {
            None
        } else {
            Some(id)
        };
        cx.notify();
    }

    fn render_filter(&self, report: &SecurityReport, cx: &mut Context<Self>) -> Div {
        let options = [
            None,
            Some(Status::Finding),
            Some(Status::NotEvaluated),
            Some(Status::NoFinding),
        ];
        div()
            .h_flex()
            .flex_wrap()
            .gap_1()
            .children(options.into_iter().enumerate().map(|(index, status)| {
                let active = self.filter == status;
                let count = report
                    .checks
                    .iter()
                    .filter(|check| status.is_none_or(|status| status_of(check) == status))
                    .count();
                let label = status.map_or_else(|| tr!("security.all"), status_label);
                let button = Button::new(("security-filter", index))
                    .label(SharedString::from(format!("{label} {count}")))
                    .small()
                    .rounded_full()
                    .custom(theme::range_pill(cx, active))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.filter = status;
                        cx.notify();
                    }));
                if active {
                    button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                } else {
                    button
                }
            }))
    }

    fn render_row(&self, check: &SecurityCheck, cx: &mut Context<Self>) -> impl IntoElement {
        let id = row_id(check);
        let keyboard_id = id.clone();
        let is_open = self.open.as_ref() == Some(&id);
        let hover = theme::sidebar_bg(cx);
        let focus_ring = theme::sidebar_bg(cx);
        let status = status_of(check);
        let what: SharedString = match &check.outcome {
            SecurityOutcome::Finding { rule, .. } => rule_label(*rule),
            SecurityOutcome::NotEvaluated(_) => tr!("security.not_evaluated_row"),
            SecurityOutcome::NoFinding { .. } => tr!("security.no_finding_row"),
        };
        let name = check
            .name
            .clone()
            .unwrap_or_else(|| check.resource_id.clone());
        let place = [check.cloud_account_id.as_deref(), check.region.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(", ");
        div()
            .id(SharedString::from(format!("security-row-{id}")))
            .role(Role::Button)
            .aria_label(SharedString::from(format!("{name}: {what}")))
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
                    .w(rems(10.))
                    .flex_shrink_0()
                    .child(status_badge(check, status, cx)),
            )
            .child(div().flex_1().min_w_0().child(what))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(name),
            )
            .child(
                div()
                    .w(rems(13.))
                    .flex_shrink_0()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(theme::text_muted(cx))
                    .child(place),
            )
            .child(
                Icon::new(if is_open {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                })
                .small()
                .text_color(theme::text_muted(cx)),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(id.clone(), cx)))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space") {
                    this.toggle(keyboard_id.clone(), cx);
                }
            }))
    }
}

/// Text and icon carry the status; color only repeats it.
fn status_badge(check: &SecurityCheck, status: Status, cx: &App) -> Div {
    let (icon, bg, fg) = match status {
        Status::Finding => (
            IconName::TriangleAlert,
            theme::danger_bg(cx),
            theme::danger(cx),
        ),
        Status::NotEvaluated => (
            IconName::CircleX,
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ),
        Status::NoFinding => (
            IconName::Check,
            theme::sidebar_bg(cx),
            theme::text_muted(cx),
        ),
    };
    let label = match &check.outcome {
        SecurityOutcome::Finding { severity, .. } => severity_label(*severity),
        _ => status_label(status),
    };
    div()
        .h_flex()
        .items_center()
        .gap_1()
        .child(Icon::new(icon).small().text_color(fg))
        .child(theme::pill(label, bg, fg))
}

/// What a row opens onto: what was seen, why it matters, what to do by hand.
fn details(check: &SecurityCheck, coverage: &SecurityCoverage, cx: &App) -> Div {
    let fact = |label: SharedString, value: SharedString| {
        div()
            .h_flex()
            .gap_4()
            .text_sm()
            .child(
                div()
                    .w(rems(9.))
                    .flex_shrink_0()
                    .text_color(theme::text_muted(cx))
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(theme::text_primary(cx))
                    .child(value),
            )
    };
    let body = div()
        .v_flex()
        .gap_2()
        .py_3()
        .pl_4()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(fact(
            tr!("security.fact_resource"),
            check.resource_id.clone().into(),
        ));
    let body = match &check.outcome {
        SecurityOutcome::Finding { rule, evidence, .. } => body
            .child(fact(tr!("security.fact_rule"), rule.id().into()))
            .child(fact(
                tr!("security.fact_observed"),
                evidence.join("; ").into(),
            ))
            .child(fact(tr!("security.fact_why"), tr!("security.why_remote")))
            .child(fact(
                tr!("security.fact_check"),
                match rule {
                    SecurityRule::PublicSsh => tr!("security.check_ssh"),
                    SecurityRule::PublicRdp => tr!("security.check_rdp"),
                },
            )),
        SecurityOutcome::NoFinding { rules_read } => body.child(fact(
            tr!("security.fact_observed"),
            tr!("security.rules_read", rules = rules_read),
        )),
        SecurityOutcome::NotEvaluated(reason) => body.child(fact(
            tr!("security.fact_reason"),
            match reason {
                NotEvaluatedReason::NoConfiguration => tr!("security.reason_no_configuration"),
                NotEvaluatedReason::NoIngressData => tr!("security.reason_no_ingress"),
                NotEvaluatedReason::Undecodable => tr!("security.reason_undecodable"),
            },
        )),
    };
    body.when_some(scan_line(coverage), |el, line| {
        el.child(fact(tr!("security.fact_scan"), line))
    })
}

fn scan_line(coverage: &SecurityCoverage) -> Option<SharedString> {
    match coverage {
        SecurityCoverage::NotScanned => None,
        SecurityCoverage::Scanned {
            evaluated_at,
            complete,
            regions,
        } => {
            let at = evaluated_at.format("%Y-%m-%d %H:%M UTC").to_string();
            let regions = regions.join(", ");
            Some(if *complete {
                tr!("security.scan_complete", at = at, regions = regions)
            } else {
                tr!("security.scan_partial", at = at, regions = regions)
            })
        }
    }
}

impl Render for SecurityCard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(report) = self.report.clone() else {
            return div();
        };
        let card = theme::card(cx).w_full().p_5().v_flex().gap_3().child(
            div()
                .v_flex()
                .gap_1()
                .child(theme::section_title(cx, tr!("security.title")))
                .child(theme::caption(cx, tr!("security.subtitle"))),
        );
        let SecurityCoverage::Scanned { complete, .. } = &report.coverage else {
            return div()
                .w_full()
                .child(card.child(notice(tr!("security.not_scanned"), cx)));
        };
        let card = card
            .when(!complete, |el| {
                el.child(notice(tr!("security.partial"), cx))
            })
            .child(theme::caption(
                cx,
                tr!(
                    "security.coverage",
                    groups = report.groups,
                    evaluated = report.evaluated,
                    not_evaluated = report.not_evaluated
                ),
            ));
        if report.groups == 0 {
            return div()
                .w_full()
                .child(card.child(notice(tr!("security.no_groups"), cx)));
        }
        let mut list = div().v_flex();
        let shown: Vec<&SecurityCheck> = report
            .checks
            .iter()
            .filter(|check| self.filter.is_none_or(|status| status_of(check) == status))
            .collect();
        for check in &shown {
            list = list.child(self.render_row(check, cx));
            if self.open.as_deref() == Some(row_id(check).as_str()) {
                list = list.child(details(check, &report.coverage, cx));
            }
        }
        if shown.is_empty() {
            list = list.child(theme::caption(cx, tr!("security.no_match")));
        }
        div()
            .w_full()
            .child(card.child(self.render_filter(&report, cx)).child(list))
    }
}

fn notice(text: SharedString, cx: &App) -> Div {
    div()
        .w_full()
        .p_3()
        .rounded_md()
        .bg(theme::warning_bg(cx))
        .text_sm()
        .text_color(theme::warning_text(cx))
        .child(text)
}
