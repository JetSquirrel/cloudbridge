//! Dashboard View

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{button::*, scroll::ScrollableElement, *};
use std::collections::HashMap;

use super::chart::{CostBarChart, CostStats, ServicePieChart};
use crate::report::{self, AccountReport, DailyCost, DashboardReport};

/// Dashboard View
pub struct DashboardView {
    /// Everything on screen, read out of the ledger
    report: Option<DashboardReport>,
    /// Whether loading is in progress
    loading: bool,
    /// Error message
    error: Option<String>,
    /// Currently expanded account ID (for drill-down)
    expanded_account: Option<String>,
    /// Daily charges per account, loaded when a card is expanded
    cost_trends: HashMap<String, Vec<DailyCost>>,
    /// Accounts currently loading trends
    loading_trends: HashMap<String, bool>,
}

impl DashboardView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Auto-trigger refresh on initialization
        cx.spawn(async move |this, cx| {
            // Small delay to ensure view is fully initialized
            smol::Timer::after(std::time::Duration::from_millis(100)).await;
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.refresh(cx);
                })
                .ok();
            })
            .ok();
        })
        .detach();

        Self {
            report: None,
            loading: true, // Initial state is loading
            error: None,
            expanded_account: None,
            cost_trends: HashMap::new(),
            loading_trends: HashMap::new(),
        }
    }

    /// Refresh data
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.load(false, cx);
    }

    /// Refresh past the freshness window, paying for another fetch.
    fn force_refresh(&mut self, cx: &mut Context<Self>) {
        self.cost_trends.clear();
        self.load(true, cx);
    }

    /// Ingest what is stale, then read the ledger.
    ///
    /// A provider that fails is logged and skipped rather than failing the
    /// whole render: what is already in the ledger is still worth showing,
    /// and it is the last thing that was true.
    fn load(&mut self, force: bool, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();

        // Use channel to fetch data in background thread
        let (tx, rx) = std::sync::mpsc::channel::<Result<DashboardReport, String>>();

        std::thread::spawn(move || {
            let reporting_currency = crate::config::load_config()
                .unwrap_or_default()
                .reporting_currency;

            let accounts = match crate::db::get_all_accounts() {
                Ok(accounts) => accounts,
                Err(e) => {
                    tracing::error!("Failed to get account list: {}", e);
                    let _ = tx.send(Err(format!("Failed to load data: {}", e)));
                    return;
                }
            };

            let now = chrono::Utc::now();
            for account in accounts.iter().filter(|account| account.enabled) {
                if let Err(e) = crate::ingest::refresh_account(account, now, force) {
                    tracing::error!("Failed to refresh {}: {}", account.name, e);
                }
            }

            let _ = tx.send(
                crate::report::build(&accounts, now, &reporting_currency)
                    .map_err(|e| format!("Failed to read the ledger: {}", e)),
            );
        });

        // Use gpui spawn to wait for results
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                rx.recv_timeout(std::time::Duration::from_secs(120))
                    .unwrap_or(Err("Data retrieval timeout".to_string()))
            })
            .await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    match result {
                        Ok(report) => {
                            this.report = Some(report);
                            this.loading = false;
                            this.error = None;
                        }
                        Err(e) => {
                            this.error = Some(e);
                            this.loading = false;
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
                    .child("Dashboard"),
            )
            .child(
                div()
                    .h_flex()
                    .gap_2()
                    .child(
                        Button::new("refresh")
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.refresh(cx);
                            })),
                    )
                    .child(
                        Button::new("force-refresh")
                            .label("Force Refresh")
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.force_refresh(cx);
                            })),
                    ),
            )
    }

    fn render_summary_cards(&self, cx: &Context<Self>) -> impl IntoElement {
        let Some(report) = self.report.as_ref() else {
            return div().w_full().p_8().items_center().justify_center().child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No data available, please add a cloud account first"),
            );
        };

        if report.accounts.is_empty() {
            return div().w_full().p_8().items_center().justify_center().child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child("No data available, please add a cloud account first"),
            );
        }

        let symbol = report::symbol(&report.reporting_currency);
        let money = |amount: f64| format!("{}{:.2}", symbol, amount);

        div()
            .w_full()
            .v_flex()
            .gap_4()
            // Overview cards. Every figure here is in the reporting
            // currency, converted per charge at a rate from its own time.
            .child(
                div()
                    .w_full()
                    .h_flex()
                    .gap_4()
                    .child(self.render_stat_card(
                        "Current Month",
                        &money(report.current_month),
                        None,
                        cx,
                    ))
                    .child(self.render_stat_card("Last Month", &money(report.last_month), None, cx))
                    .child(self.render_stat_card(
                        "Month-over-Month",
                        &format!("{:+.1}%", report.month_over_month_change),
                        Some(report.month_over_month_change >= 0.0),
                        cx,
                    ))
                    .child(self.render_stat_card(
                        "Active Accounts",
                        &report.accounts.len().to_string(),
                        None,
                        cx,
                    )),
            )
            // A charge in a currency no rate covers is missing from the
            // totals above; say so rather than quietly under-reporting.
            .when(report.unconverted_charges > 0, |el| {
                el.child(
                    div()
                        .w_full()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(format!(
                            "{} charge(s) are not included: no exchange rate to {}",
                            report.unconverted_charges, report.reporting_currency
                        )),
                )
            })
            // Per-account costs (split into cost accounts and balance accounts)
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .mt_4()
                    .child("Cost Details by Account"),
            )
            // Sources that report a period cost
            .child({
                let cost_accounts: Vec<&AccountReport> = report
                    .accounts
                    .iter()
                    .filter(|account| !account.is_snapshot())
                    .collect();

                div()
                    .w_full()
                    .v_flex()
                    .gap_4()
                    .children(
                        cost_accounts
                            .into_iter()
                            .enumerate()
                            .map(|(index, account)| {
                                let is_expanded =
                                    self.expanded_account.as_ref() == Some(&account.account_id);
                                self.render_account_card(account, is_expanded, index, cx)
                            }),
                    )
            })
            // Sources that report a point-in-time balance instead
            .child({
                let balance_accounts: Vec<&AccountReport> = report
                    .accounts
                    .iter()
                    .filter(|account| account.is_snapshot())
                    .collect();

                if balance_accounts.is_empty() {
                    div()
                } else {
                    div()
                        .w_full()
                        .mt_6()
                        .child(
                            div()
                                .text_xl()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(cx.theme().foreground)
                                .child("Balance Details by Account"),
                        )
                        .child(
                            div().w_full().v_flex().gap_4().children(
                                balance_accounts
                                    .into_iter()
                                    .enumerate()
                                    .map(|(index, account)| {
                                        let is_expanded = self.expanded_account.as_ref()
                                            == Some(&account.account_id);
                                        self.render_account_card(account, is_expanded, index, cx)
                                    }),
                            ),
                        )
                }
            })
    }

    fn render_stat_card(
        &self,
        title: &str,
        value: &str,
        is_positive: Option<bool>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let value_color = match is_positive {
            Some(true) => gpui::red(),
            Some(false) => gpui::green(),
            None => cx.theme().foreground,
        };

        div()
            .flex_1()
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(title.to_string()),
            )
            .child(
                div()
                    .text_2xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(value_color)
                    .child(value.to_string()),
            )
    }

    fn render_account_card(
        &self,
        account: &AccountReport,
        is_expanded: bool,
        index: usize,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let change = account.month_over_month_change;
        let change_color = match change {
            Some(change) if change < 0.0 => gpui::green(),
            Some(_) => gpui::red(),
            None => cx.theme().muted_foreground,
        };

        let account_id = account.account_id.clone();
        let details = account.services.clone();

        // Pre-render trend chart (render outside closure to avoid borrow issues)
        let trend_chart = if is_expanded {
            Some(self.render_trend_chart(&account.account_id, cx))
        } else {
            None
        };

        div()
            .id(ElementId::Name(format!("account-card-{}", index).into()))
            // Expanded card takes full width, collapsed card has fixed width
            .when(is_expanded, |el| el.w_full())
            .when(!is_expanded, |el| el.w(px(280.0)))
            .p_4()
            .rounded_lg()
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .cursor_pointer()
            .hover(|s| s.bg(cx.theme().secondary))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.toggle_account_expand(&account_id, cx);
            }))
            .v_flex()
            .gap_3()
            // Header: account name and labels
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(cx.theme().foreground)
                                    .child(account.account_name.clone()),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(if is_expanded { "▼" } else { "▶" }),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .px_2()
                            .py_1()
                            .rounded_md()
                            .bg(cx.theme().accent.opacity(0.1))
                            .text_color(cx.theme().accent)
                            .child(account.short_name()),
                    ),
            )
            // Cost overview
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .child({
                        // A balance stays in the currency the source keeps
                        // it in; a period cost is in the reporting one.
                        let label = if account.is_snapshot() {
                            "Balance"
                        } else {
                            "This Month"
                        };

                        div()
                            .v_flex()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(label),
                            )
                            .child(div().text_lg().font_weight(FontWeight::BOLD).child(format!(
                                "{}{:.2}",
                                report::symbol(&account.currency),
                                account.amount
                            )))
                    })
                    .child(
                        div()
                            .v_flex()
                            .items_end()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("MoM Change"),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(change_color)
                                    // Nothing to compare against reads as
                                    // an em dash, not as a flat 0%.
                                    .child(change.map_or_else(
                                        || "—".to_string(),
                                        |change| format!("{:+.1}%", change),
                                    )),
                            ),
                    ),
            )
            // Show service details when expanded
            .when(is_expanded, |el| {
                el.child(div().w_full().h_px().bg(cx.theme().border).my_2())
                    // Service breakdown section: pie chart with legend
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .mb_2()
                            .child("Service Cost Breakdown (This Month)"),
                    )
                    .child(
                        div()
                            .w_full()
                            // Pie chart with integrated legend (shows values + percentages)
                            .child(
                                ServicePieChart::donut(details.clone(), 80.0, 50.0)
                                    .with_legend()
                                    .render(cx),
                            ),
                    )
                    // Cost trend chart section
                    .child(div().w_full().h_px().bg(cx.theme().border).my_3())
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .mb_2()
                            .child("Cost Trend"),
                    )
                    .children(trend_chart)
            })
    }

    /// Render cost trend chart
    fn render_trend_chart(&self, account_id: &str, cx: &Context<Self>) -> AnyElement {
        // Check if loading
        if self
            .loading_trends
            .get(account_id)
            .copied()
            .unwrap_or(false)
        {
            return div()
                .w_full()
                .h(px(120.0))
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child("Loading trend data...")
                .into_any_element();
        }

        // Charges are already in the ledger; the chart just reads them.
        if let Some(daily_costs) = self.cost_trends.get(account_id) {
            let currency = self
                .report
                .as_ref()
                .map(|report| report.reporting_currency.clone())
                .unwrap_or_default();

            // Use BarChart with labels for daily cost visualization
            let bar_chart = CostBarChart::new(daily_costs.clone(), 550.0, 150.0, currency.clone());

            let total: f64 = daily_costs.iter().map(|d| d.amount).sum();
            let count = daily_costs.len() as f64;
            let average = if count > 0.0 { total / count } else { 0.0 };
            let max = daily_costs.iter().map(|d| d.amount).fold(0.0_f64, f64::max);
            let min = daily_costs
                .iter()
                .map(|d| d.amount)
                .fold(f64::MAX, f64::min);
            let min = if min == f64::MAX { 0.0 } else { min };

            let stats = CostStats::new(total, average, max, min, currency);

            return div()
                .w_full()
                .v_flex()
                .gap_2()
                .child(bar_chart.render(cx))
                .child(stats.render(cx))
                .into_any_element();
        }

        // Show prompt when no data
        div()
            .w_full()
            .h(px(80.0))
            .flex()
            .items_center()
            .justify_center()
            .text_color(cx.theme().muted_foreground)
            .child("Trend data will load automatically when expanded")
            .into_any_element()
    }

    /// Toggle account expand state
    fn toggle_account_expand(&mut self, account_id: &str, cx: &mut Context<Self>) {
        if self.expanded_account.as_ref() == Some(&account_id.to_string()) {
            self.expanded_account = None;
        } else {
            self.expanded_account = Some(account_id.to_string());
            // Check if need to load trend data when expanded
            if !self.cost_trends.contains_key(account_id)
                && !self
                    .loading_trends
                    .get(account_id)
                    .copied()
                    .unwrap_or(false)
            {
                self.load_cost_trend(account_id, cx);
            }
        }
        cx.notify();
    }

    /// Load cost trend data (lazy loading)
    ///
    /// A read of the ledger, not a fetch: the daily rows are already there
    /// from the refresh that filled the cards.
    fn load_cost_trend(&mut self, account_id: &str, cx: &mut Context<Self>) {
        let account_id_clone = account_id.to_string();
        self.loading_trends.insert(account_id.to_string(), true);

        // Get account info
        let account = match crate::db::get_all_accounts() {
            Ok(accounts) => accounts.into_iter().find(|a| a.id == account_id_clone),
            Err(_) => None,
        };

        let Some(account) = account else {
            self.loading_trends.insert(account_id.to_string(), false);
            return;
        };

        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<DailyCost>, String>>();

        std::thread::spawn(move || {
            let Some(descriptor) = account.descriptor() else {
                let _ = tx.send(Err("Unknown billing source".to_string()));
                return;
            };

            let Some(days) = descriptor.trend_window_days() else {
                let _ = tx.send(Err(format!(
                    "{} does not provide usage history",
                    descriptor.display_name
                )));
                return;
            };

            let _ = tx.send(
                report::trend(&account, days, chrono::Utc::now()).map_err(|e| {
                    format!("Failed to read {} trend data: {}", descriptor.short_name, e)
                }),
            );
        });

        let account_id_for_update = account_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                rx.recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap_or(Err("Trend data retrieval timeout".to_string()))
            })
            .await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.loading_trends
                        .insert(account_id_for_update.clone(), false);

                    match result {
                        Ok(daily_costs) => {
                            this.cost_trends.insert(account_id_for_update, daily_costs);
                        }
                        Err(e) => tracing::warn!("{}", e),
                    }
                    cx.notify();
                })
                .ok();
            })
            .ok();
        })
        .detach();
    }
}

impl Render for DashboardView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("dashboard-root")
            .size_full()
            .v_flex()
            .bg(cx.theme().background)
            // Fixed header area
            .child(
                div()
                    .w_full()
                    .flex_shrink_0()
                    .p_6()
                    .pb_0()
                    .child(self.render_header(cx)),
            )
            // Scrollable content area
            .child(
                div()
                    .id("dashboard-scroll-container")
                    .flex_1()
                    .w_full()
                    .overflow_y_scrollbar()
                    .p_6()
                    .pt_4()
                    .child(if self.loading {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child("Loading...")
                            .into_any_element()
                    } else if let Some(ref error) = self.error {
                        div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(gpui::red())
                            .child(error.clone())
                            .into_any_element()
                    } else {
                        self.render_summary_cards(cx).into_any_element()
                    }),
            )
    }
}
