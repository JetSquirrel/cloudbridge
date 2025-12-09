//! Cost Chart Components
//!
//! Uses gpui-component's built-in chart components for cost visualization:
//! - BarChart: Daily cost trend
//! - LineChart: Cost trend comparison
//! - PieChart: Service cost breakdown

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::chart::{BarChart, LineChart, PieChart};
use gpui_component::{ActiveTheme, StyledExt};

use crate::cloud::{DailyCost, ServiceCost};

// ==================== Bar Chart ====================

/// Cost Trend Bar Chart - shows daily costs as bars
/// Kept as backup in case bar chart visualization is preferred
#[allow(dead_code)]
pub struct CostChart {
    /// Daily cost data
    daily_costs: Vec<DailyCost>,
    /// Chart width
    width: f32,
    /// Chart height
    height: f32,
}

#[allow(dead_code)]
impl CostChart {
    pub fn new(daily_costs: Vec<DailyCost>, width: f32, height: f32) -> Self {
        Self {
            daily_costs,
            width,
            height,
        }
    }

    /// Render chart using built-in BarChart
    pub fn render<V: 'static>(&self, cx: &Context<V>) -> AnyElement {
        if self.daily_costs.is_empty() {
            return div()
                .w(px(self.width))
                .h(px(self.height))
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child("No cost trend data available")
                .into_any_element();
        }

        // Get theme color before closures to avoid lifetime issues
        let chart_color = cx.theme().chart_1;

        // Format dates for display (MM-DD)
        let chart_data: Vec<ChartDataPoint> = self
            .daily_costs
            .iter()
            .map(|d| ChartDataPoint {
                date: Self::format_date(&d.date),
                amount: d.amount,
            })
            .collect();

        // Calculate tick_margin based on data points count
        // Show ~5-7 ticks for readability
        let tick_margin = (chart_data.len() / 6).max(1);

        div()
            .w(px(self.width))
            .h(px(self.height))
            .child(
                BarChart::new(chart_data)
                    .x(|d| d.date.clone())
                    .y(|d| d.amount)
                    .fill(move |_| chart_color)
                    .tick_margin(tick_margin),
            )
            .into_any_element()
    }

    /// Format date display (YYYY-MM-DD -> MM-DD)
    fn format_date(date: &str) -> String {
        if date.len() >= 10 {
            date[5..10].to_string()
        } else {
            date.to_string()
        }
    }
}

// ==================== Bar Chart with Labels ====================

/// Cost Trend Bar Chart with labels - shows daily costs as bars with values
pub struct CostBarChart {
    /// Daily cost data
    daily_costs: Vec<DailyCost>,
    /// Chart width
    width: f32,
    /// Chart height
    height: f32,
    /// Show labels on bars
    show_labels: bool,
}

impl CostBarChart {
    pub fn new(daily_costs: Vec<DailyCost>, width: f32, height: f32) -> Self {
        Self {
            daily_costs,
            width,
            height,
            show_labels: false, // Default: no labels (cleaner look)
        }
    }

    /// Enable labels on bars (shows value above each bar)
    #[allow(dead_code)]
    pub fn with_labels(mut self) -> Self {
        self.show_labels = true;
        self
    }

    /// Disable labels on bars
    #[allow(dead_code)]
    pub fn without_labels(mut self) -> Self {
        self.show_labels = false;
        self
    }

    /// Render chart using built-in BarChart with labels
    pub fn render<V: 'static>(&self, cx: &Context<V>) -> AnyElement {
        if self.daily_costs.is_empty() {
            return div()
                .w(px(self.width))
                .h(px(self.height))
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child("No cost trend data available")
                .into_any_element();
        }

        // Get theme color before closures to avoid lifetime issues
        let chart_color = cx.theme().chart_1;

        // Format dates for display (MM-DD)
        let chart_data: Vec<ChartDataPoint> = self
            .daily_costs
            .iter()
            .map(|d| ChartDataPoint {
                date: Self::format_date(&d.date),
                amount: d.amount,
            })
            .collect();

        // Calculate tick_margin based on data points count
        let tick_margin = (chart_data.len() / 6).max(1);

        let show_labels = self.show_labels;

        div()
            .w(px(self.width))
            .h(px(self.height))
            .child(
                BarChart::new(chart_data)
                    .x(|d| d.date.clone())
                    .y(|d| d.amount)
                    .fill(move |_| chart_color)
                    .tick_margin(tick_margin)
                    .when(show_labels, |chart| {
                        chart.label(|d| format!("${:.2}", d.amount))
                    }),
            )
            .into_any_element()
    }

    /// Format date display (YYYY-MM-DD -> MM-DD)
    fn format_date(date: &str) -> String {
        if date.len() >= 10 {
            date[5..10].to_string()
        } else {
            date.to_string()
        }
    }
}

// ==================== Line Chart ====================

/// Cost Trend Line Chart - shows daily costs as a line with dots
#[allow(dead_code)]
pub struct CostLineChart {
    /// Daily cost data
    daily_costs: Vec<DailyCost>,
    /// Chart width
    width: f32,
    /// Chart height
    height: f32,
}

#[allow(dead_code)]
impl CostLineChart {
    pub fn new(daily_costs: Vec<DailyCost>, width: f32, height: f32) -> Self {
        Self {
            daily_costs,
            width,
            height,
        }
    }

    /// Render chart using built-in LineChart
    pub fn render<V: 'static>(&self, cx: &Context<V>) -> AnyElement {
        if self.daily_costs.is_empty() {
            return div()
                .w(px(self.width))
                .h(px(self.height))
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child("No cost trend data available")
                .into_any_element();
        }

        // Get theme color before closures to avoid lifetime issues
        let chart_color = cx.theme().chart_1;

        // Format dates for display (MM-DD)
        let chart_data: Vec<ChartDataPoint> = self
            .daily_costs
            .iter()
            .map(|d| ChartDataPoint {
                date: Self::format_date(&d.date),
                amount: d.amount,
            })
            .collect();

        // Calculate tick_margin based on data points count
        let tick_margin = (chart_data.len() / 6).max(1);

        div()
            .w(px(self.width))
            .h(px(self.height))
            .child(
                LineChart::new(chart_data)
                    .x(|d| d.date.clone())
                    .y(|d| d.amount)
                    .stroke(chart_color)
                    .dot()
                    .tick_margin(tick_margin),
            )
            .into_any_element()
    }

    /// Format date display (YYYY-MM-DD -> MM-DD)
    fn format_date(date: &str) -> String {
        if date.len() >= 10 {
            date[5..10].to_string()
        } else {
            date.to_string()
        }
    }
}

// ==================== Pie Chart ====================

/// Service Cost Pie Chart - shows cost breakdown by service with legend
pub struct ServicePieChart {
    /// Service cost data
    services: Vec<ServiceCost>,
    /// Outer radius
    outer_radius: f32,
    /// Inner radius (0 for pie, >0 for donut)
    inner_radius: f32,
    /// Show legend with values
    show_legend: bool,
}

impl ServicePieChart {
    /// Create a basic pie chart
    #[allow(dead_code)]
    pub fn new(services: Vec<ServiceCost>, outer_radius: f32) -> Self {
        Self {
            services,
            outer_radius,
            inner_radius: 0.0,
            show_legend: false,
        }
    }

    /// Create a donut chart
    pub fn donut(services: Vec<ServiceCost>, outer_radius: f32, inner_radius: f32) -> Self {
        Self {
            services,
            outer_radius,
            inner_radius,
            show_legend: false,
        }
    }

    /// Enable legend display with values and percentages
    pub fn with_legend(mut self) -> Self {
        self.show_legend = true;
        self
    }

    /// Render chart using built-in PieChart
    pub fn render<V: 'static>(&self, cx: &Context<V>) -> AnyElement {
        if self.services.is_empty() {
            return div()
                .size(px(self.outer_radius * 2.0))
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .text_sm()
                .child("No data")
                .into_any_element();
        }

        // Get chart colors
        let colors = [
            cx.theme().chart_1,
            cx.theme().chart_2,
            cx.theme().chart_3,
            cx.theme().chart_4,
            cx.theme().chart_5,
        ];

        // Calculate total for percentages
        let total: f64 = self.services.iter().map(|s| s.amount).sum();

        // Prepare data with color index
        let chart_data: Vec<PieDataPoint> = self
            .services
            .iter()
            .enumerate()
            .map(|(i, s)| PieDataPoint {
                service: Self::truncate_service_name(&s.service),
                amount: s.amount,
                color_index: i % colors.len(),
            })
            .collect();

        let outer_radius = self.outer_radius;
        let inner_radius = self.inner_radius;

        // Chart element
        let chart = div()
            .flex_shrink_0()
            .size(px(outer_radius * 2.0 + 20.0))
            .flex()
            .items_center()
            .justify_center()
            .child(
                PieChart::new(chart_data.clone())
                    .value(|d| d.amount as f32)
                    .outer_radius(outer_radius)
                    .inner_radius(inner_radius)
                    .color(move |d| colors[d.color_index])
                    .pad_angle(0.02),
            );

        if !self.show_legend {
            return chart.into_any_element();
        }

        // Build legend items
        let legend_items: Vec<_> = self
            .services
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let color = colors[i % colors.len()];
                let percentage = if total > 0.0 {
                    (s.amount / total * 100.0) as f32
                } else {
                    0.0
                };
                // Use full service name for legend (truncate only if very long)
                let name = Self::truncate_legend_name(&s.service);
                let amount = s.amount;
                (color, name, amount, percentage)
            })
            .collect();

        // Render with legend - use vertical layout for better readability
        div()
            .v_flex()
            .gap_3()
            .child(chart)
            .child(
                div()
                    .w_full()
                    .v_flex()
                    .gap_2()
                    .children(legend_items.into_iter().map(|(color, name, amount, pct)| {
                        div()
                            .w_full()
                            .h_flex()
                            .gap_2()
                            .items_center()
                            .justify_between()
                            // Left: color + name
                            .child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_1()
                                    .child(
                                        div()
                                            .size(px(12.0))
                                            .rounded(px(2.0))
                                            .bg(color)
                                            .flex_shrink_0(),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().foreground)
                                            .overflow_hidden()
                                            .text_ellipsis()
                                            .child(name),
                                    ),
                            )
                            // Right: amount + percentage
                            .child(
                                div()
                                    .h_flex()
                                    .gap_2()
                                    .items_center()
                                    .flex_shrink_0()
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(cx.theme().foreground)
                                            .child(format!("${:.2}", amount)),
                                    )
                                    .child(
                                        div()
                                            .text_sm()
                                            .text_color(cx.theme().muted_foreground)
                                            .min_w(px(50.0))
                                            .text_right()
                                            .child(format!("{:.1}%", pct)),
                                    ),
                            )
                    })),
            )
            .into_any_element()
    }

    /// Truncate long service names for legend display
    fn truncate_legend_name(name: &str) -> String {
        if name.len() > 35 {
            format!("{}...", &name[..32])
        } else {
            name.to_string()
        }
    }

    /// Truncate long service names
    fn truncate_service_name(name: &str) -> String {
        if name.len() > 20 {
            format!("{}...", &name[..17])
        } else {
            name.to_string()
        }
    }
}

// ==================== Data Structures ====================

/// Internal data structure for bar/line chart
#[derive(Clone)]
struct ChartDataPoint {
    date: String,
    amount: f64,
}

/// Internal data structure for pie chart
#[derive(Clone)]
struct PieDataPoint {
    #[allow(dead_code)]
    service: String,
    amount: f64,
    color_index: usize,
}

/// Statistics summary component
pub struct CostStats {
    pub total: f64,
    pub average: f64,
    pub max: f64,
    pub min: f64,
    #[allow(dead_code)]
    pub currency: String,
}

impl CostStats {
    pub fn new(total: f64, average: f64, max: f64, min: f64, currency: String) -> Self {
        Self {
            total,
            average,
            max,
            min,
            currency,
        }
    }

    pub fn render<V: 'static>(&self, cx: &Context<V>) -> AnyElement {
        div()
            .w_full()
            .h_flex()
            .gap_4()
            .justify_between()
            .child(self.render_stat_item("Total", self.total, cx))
            .child(self.render_stat_item("Daily Avg", self.average, cx))
            .child(self.render_stat_item("Highest", self.max, cx))
            .child(self.render_stat_item("Lowest", self.min, cx))
            .into_any_element()
    }

    fn render_stat_item<V: 'static>(&self, label: &str, value: f64, cx: &Context<V>) -> Div {
        div()
            .v_flex()
            .items_center()
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(label.to_string()),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(cx.theme().foreground)
                    .child(format!("${:.2}", value)),
            )
    }
}
