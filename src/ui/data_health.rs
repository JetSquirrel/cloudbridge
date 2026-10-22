//! The Data health card: the ledger's data-quality findings for the current
//! billing period, as Overview, Accounts and the account detail page show
//! them.
//!
//! Each page loads and dismisses its own findings, because each scopes them
//! differently: the whole ledger, every account, or one account. Only the
//! card is shared: its heading, the worst [`COLLAPSED_ROWS`] findings with
//! Show all for the rest, Dismiss, and the finding row itself.

use gpui_kit::component::{button::*, *};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{fmt, theme};
use crate::ledger::query::{DataQualityIssue, IssueSeverity};

/// Findings the card lists before "Show all": enough for the worst of them
/// to be read, few enough that what the page is for stays in view.
pub(super) const COLLAPSED_ROWS: usize = 3;

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App) + 'static>;

/// The card. `rows` are already drawn and sorted worst first; the card
/// shows the first [`COLLAPSED_ROWS`] unless `expanded`. With no rows it
/// still renders, saying the period is clean, for the pages that keep it
/// in place; a page that would rather hide it does not build it.
///
/// Dismiss names its scope ("Dismiss all") when it dismisses rows the
/// collapsed card is not showing. What it dismisses is the page's call:
/// each page persists its own findings' keys.
#[derive(IntoElement)]
pub(super) struct HealthCard {
    id: SharedString,
    rows: Vec<AnyElement>,
    expanded: bool,
    compact: bool,
    on_toggle: Option<ClickHandler>,
    on_dismiss: Option<ClickHandler>,
}

impl HealthCard {
    /// `id` keeps the card's buttons distinct from the page's own.
    pub(super) fn new(id: impl Into<SharedString>, rows: Vec<AnyElement>) -> Self {
        Self {
            id: id.into(),
            rows,
            expanded: false,
            compact: false,
            on_toggle: None,
            on_dismiss: None,
        }
    }

    pub(super) fn expanded(mut self, expanded: bool) -> Self {
        self.expanded = expanded;
        self
    }

    /// Keep the dashboard's figures in view; findings remain one click away.
    pub(super) fn compact(mut self) -> Self {
        self.compact = true;
        self
    }

    /// Show all / Show fewer; the page flips its own `expanded` flag.
    pub(super) fn on_toggle(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_toggle = Some(Box::new(handler));
        self
    }

    pub(super) fn on_dismiss(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_dismiss = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for HealthCard {
    fn render(self, _: &mut Window, cx: &mut App) -> impl IntoElement {
        let total = self.rows.len();
        let preview_rows = if self.compact { 0 } else { COLLAPSED_ROWS };
        let hidden = total.saturating_sub(preview_rows);
        let shown = if self.expanded {
            total
        } else {
            total.min(preview_rows)
        };
        let summary = match total {
            0 => "No data-quality issues this billing period".to_string(),
            1 => "1 finding this billing period".to_string(),
            n => format!("{n} findings this billing period"),
        };

        let actions = div()
            .h_flex()
            .items_center()
            .gap_2()
            .when_some(self.on_toggle.filter(|_| hidden > 0), |el, on_toggle| {
                el.child(
                    Button::new(SharedString::from(format!("{}-toggle", self.id)))
                        .label(if self.expanded {
                            "Collapse".to_string()
                        } else if self.compact {
                            format!("Review {total} findings")
                        } else {
                            format!("Show all {total}")
                        })
                        .ghost()
                        .small()
                        .on_click(on_toggle),
                )
            })
            .when_some(self.on_dismiss.filter(|_| total > 0), |el, on_dismiss| {
                el.child(
                    Button::new(SharedString::from(format!("{}-dismiss", self.id)))
                        .label(if hidden > 0 { "Dismiss all" } else { "Dismiss" })
                        .ghost()
                        .small()
                        .on_click(on_dismiss),
                )
            });

        theme::card(cx)
            .w_full()
            .px_5()
            .py_3()
            .v_flex()
            .gap_3()
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .justify_between()
                    .flex_wrap()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .when(self.compact, |el| el.items_center().flex_wrap().gap_3())
                            .when(!self.compact, |el| el.flex_col().gap_1())
                            .child(theme::section_title(cx, "Data health"))
                            .child(theme::caption(cx, summary)),
                    )
                    .child(actions),
            )
            .when(shown > 0, |el| {
                el.child(
                    // One group, so the rows sit closer to one another than
                    // to the heading; each row draws the hairline above itself.
                    div().v_flex().children(self.rows.into_iter().take(shown)),
                )
            })
    }
}

/// Worst-first ordering key for a severity.
pub(super) fn severity_rank(severity: IssueSeverity) -> u8 {
    match severity {
        IssueSeverity::Critical => 0,
        IssueSeverity::Warning => 1,
        IssueSeverity::Info => 2,
    }
}

/// A severity's icon and tint/ink pair, mirroring the alerts page's badges:
/// critical in the alert tint, warning in the warning colours, info quiet.
pub(super) fn issue_severity_style(severity: IssueSeverity, cx: &App) -> (IconName, Hsla, Hsla) {
    match severity {
        IssueSeverity::Critical => (IconName::CircleX, theme::alert_tint(cx), theme::danger(cx)),
        IssueSeverity::Warning => (
            IconName::TriangleAlert,
            theme::warning_bg(cx),
            theme::warning_text(cx),
        ),
        IssueSeverity::Info => (IconName::Info, theme::sidebar_bg(cx), theme::text_muted(cx)),
    }
}

/// One data-quality issue: a severity icon badge, the check's message, and
/// the reporting-currency amount behind it when the check could sum one
/// (unconverted charges cannot — their currencies do not mix).
pub(super) fn render_issue_row(issue: &DataQualityIssue, currency: &str, cx: &App) -> Div {
    issue_row(
        issue.severity,
        issue.message.clone(),
        issue.affected_amount,
        currency,
        cx,
    )
}

/// [`render_issue_row`] from its parts, for a page that holds findings in
/// its own row type, as Overview does, so every page draws a finding the
/// same way.
pub(super) fn issue_row(
    severity: IssueSeverity,
    message: impl Into<SharedString>,
    affected_amount: Option<f64>,
    currency: &str,
    cx: &App,
) -> Div {
    let (icon, bg, fg) = issue_severity_style(severity, cx);
    let message: SharedString = message.into();
    div()
        .h_flex()
        .items_center()
        .gap_3()
        .py_2()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(
            div()
                .flex_shrink_0()
                .size_8()
                .rounded_full()
                .bg(bg)
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(icon).size_4().text_color(fg)),
        )
        .child(
            div()
                .flex_1()
                // min_w_0 so a long message wraps instead of pushing the
                // amount out of the card.
                .min_w_0()
                .text_sm()
                .text_color(theme::text_primary(cx))
                .child(message),
        )
        .when_some(affected_amount, |el, amount| {
            el.child(
                div()
                    .flex_shrink_0()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_primary(cx))
                    .child(fmt::amount(amount, currency)),
            )
        })
}
