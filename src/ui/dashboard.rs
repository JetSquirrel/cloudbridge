//! Dashboard View

use std::collections::HashMap;
use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{button::*, *};

use crate::cloud::{CostSummary, CostTrend, ServiceCost};
use super::chart::{CostChart, CostStats};

/// Dashboard View
pub struct DashboardView {
    /// Cost summary data
    summaries: Vec<CostSummary>,
    /// Whether loading is in progress
    loading: bool,
    /// Error message
    error: Option<String>,
    /// Currently expanded account ID (for drill-down)
    expanded_account: Option<String>,
    /// Cost trend cache (account_id -> CostTrend)
    cost_trends: HashMap<String, CostTrend>,
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
                }).ok();
            }).ok();
        }).detach();
        
        Self {
            summaries: Vec::new(),
            loading: true, // Initial state is loading
            error: None,
            expanded_account: None,
            cost_trends: HashMap::new(),
            loading_trends: HashMap::new(),
        }
    }

    /// Refresh data
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.error = None;
        cx.notify();

        // Use channel to fetch data in background thread
        let (tx, rx) = std::sync::mpsc::channel::<Result<Vec<CostSummary>, String>>();
        
        std::thread::spawn(move || {
            tracing::info!("Dashboard starting to load account data...");
            match crate::db::get_all_accounts() {
                Ok(accounts) => {
                    tracing::info!("Dashboard retrieved {} accounts", accounts.len());
                    let mut summaries = Vec::new();

                    for account in accounts {
                        if !account.enabled {
                            tracing::info!("Skipping disabled account: {}", account.name);
                            continue;
                        }
                        
                        tracing::info!("Processing account: {} ({})", account.name, account.provider.short_name());

                        // Try to get from cache first
                        tracing::info!("Checking cache for account {}...", account.name);
                        match crate::db::get_cached_cost_summary_with_account(
                            &account.id, 
                            &account.name, 
                            &account.provider
                        ) {
                            Ok(Some(cached)) => {
                                tracing::info!("Account {} using cached data", account.name);
                                summaries.push(cached);
                                continue;
                            }
                            Ok(None) => {
                                tracing::info!("Account {} has no cache, fetching from API", account.name);
                            }
                            Err(e) => {
                                tracing::warn!("Account {} cache query failed: {}, trying API", account.name, e);
                            }
                        }

                        match account.provider {
                            crate::cloud::CloudProvider::AWS => {
                                tracing::info!("Calling AWS Cost Explorer API...");
                                let service = crate::cloud::aws::AwsCloudService::new(
                                    account.id.clone(),
                                    account.name.clone(),
                                    account.access_key_id.clone(),
                                    account.secret_access_key.clone(),
                                    account.region.clone(),
                                );

                                use crate::cloud::CloudService;
                                match service.get_cost_summary() {
                                    Ok(summary) => {
                                        tracing::info!("AWS account {} cost retrieval successful", account.name);
                                        // Save to cache
                                        if let Err(e) = crate::db::save_cost_summary_cache(&summary) {
                                            tracing::warn!("Failed to save cost cache: {}", e);
                                        }
                                        summaries.push(summary);
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to get cost for {}: {}", account.name, e);
                                    }
                                }
                            }
                            crate::cloud::CloudProvider::Aliyun => {
                                tracing::info!("Calling Aliyun BSS API...");
                                let service = crate::cloud::aliyun::AliyunCloudService::new(
                                    account.id.clone(),
                                    account.name.clone(),
                                    account.access_key_id.clone(),
                                    account.secret_access_key.clone(),
                                    account.region.clone(),
                                );

                                use crate::cloud::CloudService;
                                match service.get_cost_summary() {
                                    Ok(summary) => {
                                        tracing::info!("Aliyun account {} cost retrieval successful", account.name);
                                        // Save to cache
                                        if let Err(e) = crate::db::save_cost_summary_cache(&summary) {
                                            tracing::warn!("Failed to save cost cache: {}", e);
                                        }
                                        summaries.push(summary);
                                    }
                                    Err(e) => {
                                        tracing::error!("Failed to get Aliyun {} cost: {}", account.name, e);
                                    }
                                }
                            }
                            _ => {
                                tracing::warn!("Account {} uses unsupported cloud provider", account.name);
                            }
                        }
                    }
                    tracing::info!("Dashboard preparing to send {} cost summary entries", summaries.len());
                    if let Err(e) = tx.send(Ok(summaries)) {
                        tracing::error!("Failed to send data: {:?}", e);
                    }
                }
                Err(e) => {
                    tracing::error!("Failed to get account list: {}", e);
                    let _ = tx.send(Err(format!("Failed to load data: {}", e)));
                }
            }
            tracing::info!("Dashboard background thread completed");
        });

        // Use gpui spawn to wait for results
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                rx.recv_timeout(std::time::Duration::from_secs(60))
                    .unwrap_or(Err("Data retrieval timeout".to_string()))
            }).await;

            tracing::info!("Dashboard received data result");
            
            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    match result {
                        Ok(summaries) => {
                            tracing::info!("Dashboard updating {} cost summaries", summaries.len());
                            for s in &summaries {
                                tracing::info!("Account {}: current month {:.2} {}, last month {:.2} {}", 
                                    s.account_name, s.current_month_cost, s.currency,
                                    s.last_month_cost, s.currency);
                            }
                            this.summaries = summaries;
                            this.loading = false;
                            this.error = None;
                        }
                        Err(e) => {
                            tracing::error!("Dashboard update failed: {}", e);
                            this.error = Some(e);
                            this.loading = false;
                        }
                    }
                    cx.notify();
                }).ok();
            }).ok();
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

    /// Force refresh (clear cache and refetch)
    fn force_refresh(&mut self, cx: &mut Context<Self>) {
        // Clear all cache
        if let Err(e) = crate::db::clear_all_cache() {
            tracing::warn!("Failed to clear cache: {}", e);
        }
        // Clear trend cache in memory
        self.cost_trends.clear();
        // Then refresh
        self.refresh(cx);
    }

    fn render_summary_cards(&self, cx: &Context<Self>) -> impl IntoElement {
        if self.summaries.is_empty() {
            return div()
                .w_full()
                .p_8()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("No data available, please add a cloud account first"),
                );
        }

        let total_current: f64 = self.summaries.iter().map(|s| s.current_month_cost).sum();
        let total_last: f64 = self.summaries.iter().map(|s| s.last_month_cost).sum();
        let total_change = if total_last > 0.0 {
            ((total_current - total_last) / total_last) * 100.0
        } else {
            0.0
        };

        div()
            .w_full()
            .v_flex()
            .gap_4()
            // Overview cards
            .child(
                div()
                    .w_full()
                    .h_flex()
                    .gap_4()
                    .child(self.render_stat_card("Current Month", &format!("${:.2}", total_current), None, cx))
                    .child(self.render_stat_card("Last Month", &format!("${:.2}", total_last), None, cx))
                    .child(self.render_stat_card(
                        "Month-over-Month",
                        &format!("{:+.1}%", total_change),
                        Some(total_change >= 0.0),
                        cx,
                    ))
                    .child(self.render_stat_card("Active Accounts", &self.summaries.len().to_string(), None, cx)),
            )
            // Per-account costs
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().foreground)
                    .mt_4()
                    .child("Cost Details by Account"),
            )
            .child(
                div()
                    .w_full()
                    .h_flex()
                    .flex_wrap()
                    .gap_4()
                    .children(self.summaries.iter().enumerate().map(|(index, summary)| {
                        let is_expanded = self.expanded_account.as_ref() == Some(&summary.account_id);
                        self.render_account_card(summary, is_expanded, index, cx)
                    })),
            )
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

    fn render_account_card(&self, summary: &CostSummary, is_expanded: bool, index: usize, cx: &Context<Self>) -> impl IntoElement {
        let change_color = if summary.month_over_month_change >= 0.0 {
            gpui::red()
        } else {
            gpui::green()
        };

        let card_width = if is_expanded { px(600.0) } else { px(280.0) };
        let account_id = summary.account_id.clone();
        let details = summary.current_month_details.clone();
        
        // Pre-render trend chart (render outside closure to avoid borrow issues)
        let trend_chart = if is_expanded {
            Some(self.render_trend_chart(&summary.account_id, cx))
        } else {
            None
        };

        div()
            .id(ElementId::Name(format!("account-card-{}", index).into()))
            .w(card_width)
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
                                    .child(summary.account_name.clone()),
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
                            .child(summary.provider.short_name()),
                    ),
            )
            // Cost overview
            .child(
                div()
                    .h_flex()
                    .justify_between()
                    .child(
                        div()
                            .v_flex()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child("This Month"),
                            )
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::BOLD)
                                    .child(format!("${:.2}", summary.current_month_cost)),
                            ),
                    )
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
                                    .child(format!("{:+.1}%", summary.month_over_month_change)),
                            ),
                    ),
            )
            // Show service details when expanded
            .when(is_expanded, |el| {
                el
                    .child(
                        div()
                            .w_full()
                            .h_px()
                            .bg(cx.theme().border)
                            .my_2(),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .mb_2()
                            .child("Service Cost Details (This Month)"),
                    )
                    .child(Self::render_service_details_static(&details, cx))
                    // Add cost trend chart
                    .child(
                        div()
                            .w_full()
                            .h_px()
                            .bg(cx.theme().border)
                            .my_3(),
                    )
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(cx.theme().foreground)
                            .mb_2()
                            .child("Cost Trend (This Month)"),
                    )
                    .children(trend_chart)
            })
    }

    /// Render cost trend chart
    fn render_trend_chart(&self, account_id: &str, cx: &Context<Self>) -> AnyElement {
        // Check if loading
        if self.loading_trends.get(account_id).copied().unwrap_or(false) {
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

        // Check for cached data
        if let Some(trend) = self.cost_trends.get(account_id) {
            let chart = CostChart::new(trend.daily_costs.clone(), 550.0, 150.0);
            
            // Calculate statistics from daily_costs
            let total: f64 = trend.daily_costs.iter().map(|d| d.amount).sum();
            let count = trend.daily_costs.len() as f64;
            let average = if count > 0.0 { total / count } else { 0.0 };
            let max = trend.daily_costs.iter().map(|d| d.amount).fold(0.0_f64, f64::max);
            let min = trend.daily_costs.iter().map(|d| d.amount).fold(f64::MAX, f64::min);
            let min = if min == f64::MAX { 0.0 } else { min };
            
            let stats = CostStats::new(
                total,
                average,
                max,
                min,
                trend.currency.clone(),
            );
            
            return div()
                .w_full()
                .v_flex()
                .gap_2()
                .child(chart.render(cx))
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
            if !self.cost_trends.contains_key(account_id) && !self.loading_trends.get(account_id).copied().unwrap_or(false) {
                self.load_cost_trend(account_id, cx);
            }
        }
        cx.notify();
    }

    /// Load cost trend data (lazy loading)
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

        let (tx, rx) = std::sync::mpsc::channel::<Result<CostTrend, String>>();
        
        std::thread::spawn(move || {
            use chrono::{Datelike, Duration, Utc};
            
            let now = Utc::now();
            // Get past 30 days data, ensure enough data points for trend display
            let start = now - Duration::days(30);
            let start_date = format!("{}-{:02}-{:02}", start.year(), start.month(), start.day());
            let end_date = format!("{}-{:02}-{:02}", now.year(), now.month(), now.day());
            
            // Try to get from cache first
            if let Ok(Some(cached)) = crate::db::get_cached_cost_trend(&account.id, &start_date, &end_date) {
                tracing::info!("Account {} cost trend using cached data", account.name);
                let _ = tx.send(Ok(cached));
                return;
            }
            
            match account.provider {
                crate::cloud::CloudProvider::AWS => {
                    let service = crate::cloud::aws::AwsCloudService::new(
                        account.id.clone(),
                        account.name.clone(),
                        account.access_key_id.clone(),
                        account.secret_access_key.clone(),
                        account.region.clone(),
                    );

                    use crate::cloud::CloudService;
                    match service.get_cost_trend(&start_date, &end_date) {
                        Ok(trend) => {
                            // Save to cache
                            if let Err(e) = crate::db::save_cost_trend_cache(&trend) {
                                tracing::warn!("Failed to save trend cache: {}", e);
                            }
                            let _ = tx.send(Ok(trend));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("Failed to get trend data: {}", e)));
                        }
                    }
                }
                crate::cloud::CloudProvider::Aliyun => {
                    let service = crate::cloud::aliyun::AliyunCloudService::new(
                        account.id.clone(),
                        account.name.clone(),
                        account.access_key_id.clone(),
                        account.secret_access_key.clone(),
                        account.region.clone(),
                    );

                    use crate::cloud::CloudService;
                    match service.get_cost_trend(&start_date, &end_date) {
                        Ok(trend) => {
                            // Save to cache
                            if let Err(e) = crate::db::save_cost_trend_cache(&trend) {
                                tracing::warn!("Failed to save trend cache: {}", e);
                            }
                            let _ = tx.send(Ok(trend));
                        }
                        Err(e) => {
                            let _ = tx.send(Err(format!("Failed to get Aliyun trend data: {}", e)));
                        }
                    }
                }
                _ => {
                    let _ = tx.send(Err("This cloud provider is not supported".to_string()));
                }
            }
        });

        let account_id_for_update = account_id.to_string();
        cx.spawn(async move |this, cx| {
            let result = smol::unblock(move || {
                rx.recv_timeout(std::time::Duration::from_secs(30))
                    .unwrap_or(Err("Trend data retrieval timeout".to_string()))
            }).await;

            cx.update(|cx| {
                this.update(cx, |this, cx| {
                    this.loading_trends.insert(account_id_for_update.clone(), false);
                    
                    match result {
                        Ok(trend) => {
                            tracing::info!("Successfully loaded cost trend for account {}: {} days", account_id_for_update, trend.daily_costs.len());
                            this.cost_trends.insert(account_id_for_update, trend);
                        }
                        Err(e) => {
                            tracing::error!("Failed to load trend: {}", e);
                        }
                    }
                    cx.notify();
                }).ok();
            }).ok();
        })
        .detach();
    }

    /// Render service cost details list (static version, for closures)
    fn render_service_details_static(details: &[ServiceCost], cx: &Context<Self>) -> AnyElement {
        if details.is_empty() {
            return div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child("No cost data available")
                .into_any_element();
        }

        div()
            .w_full()
            .v_flex()
            .gap_1()
            .children(details.iter().take(10).map(|service| {
                Self::render_service_row_static(service, cx)
            }))
            .when(details.len() > 10, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .mt_2()
                        .child(format!("... and {} more services", details.len() - 10)),
                )
            })
            .into_any_element()
    }

    /// Render single service cost row (static version)
    fn render_service_row_static(service: &ServiceCost, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h_flex()
            .justify_between()
            .items_center()
            .py_1()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .overflow_hidden()
                    .max_w(px(400.0))
                    .child(service.service.clone()),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child(format!("${:.4}", service.amount)),
            )
    }
}

impl Render for DashboardView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .p_6()
            .v_flex()
            .gap_6()
            .bg(cx.theme().background)
            .child(self.render_header(cx))
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
            })
    }
}
