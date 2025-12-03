//! Cost Trend Chart Component

use gpui::prelude::FluentBuilder;
use gpui::*;
use gpui_component::{ActiveTheme, StyledExt};

use crate::cloud::DailyCost;

/// Cost Trend Chart
pub struct CostChart {
    /// Daily cost data
    daily_costs: Vec<DailyCost>,
    /// Chart width
    width: f32,
    /// Chart height
    height: f32,
}

impl CostChart {
    pub fn new(daily_costs: Vec<DailyCost>, width: f32, height: f32) -> Self {
        Self {
            daily_costs,
            width,
            height,
        }
    }

    /// Render chart
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

        let max_amount = self
            .daily_costs
            .iter()
            .map(|d| d.amount)
            .fold(0.0_f64, f64::max);
        let chart_height = self.height - 40.0; // Leave space for bottom date labels
        let bar_width = (self.width - 40.0) / self.daily_costs.len() as f32; // Leave space for left Y-axis

        // Pre-calculate each bar
        let bars: Vec<_> = self
            .daily_costs
            .iter()
            .enumerate()
            .map(|(i, daily)| self.render_bar(daily, max_amount, bar_width, chart_height, i, cx))
            .collect();

        // Format first and last dates
        let first_date = self.daily_costs.first().map(|d| self.format_date(&d.date));
        let last_date = self.daily_costs.last().map(|d| self.format_date(&d.date));

        div()
            .w(px(self.width))
            .h(px(self.height))
            .v_flex()
            .gap_1()
            // Chart area
            .child(
                div()
                    .w_full()
                    .h(px(chart_height))
                    .h_flex()
                    .items_end()
                    .gap_px()
                    .px_4()
                    .children(bars),
            )
            // Bottom date labels (show only first and last)
            .child(
                div()
                    .w_full()
                    .h(px(20.0))
                    .h_flex()
                    .justify_between()
                    .px_4()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .when_some(first_date, |el, date| el.child(date))
                    .when_some(last_date, |el, date| el.child(date)),
            )
            .into_any_element()
    }

    /// Render single bar
    fn render_bar<V: 'static>(
        &self,
        daily: &DailyCost,
        max_amount: f64,
        bar_width: f32,
        chart_height: f32,
        _index: usize,
        cx: &Context<V>,
    ) -> Div {
        let bar_height = if max_amount > 0.0 {
            ((daily.amount / max_amount) * chart_height as f64) as f32
        } else {
            0.0
        };
        // Minimum height 2px to remain visible
        let bar_height = bar_height.max(2.0);

        // Choose color based on cost level (red gradient, higher cost = darker)
        let intensity = if max_amount > 0.0 {
            (daily.amount / max_amount) as f32
        } else {
            0.0
        };
        let bar_color = self.get_bar_color(intensity, cx);

        div()
            .w(px(bar_width - 2.0))
            .h(px(bar_height))
            .bg(bar_color)
            .rounded_t_sm()
            .hover(|s| s.opacity(0.8))
    }

    /// Get bar color based on intensity
    fn get_bar_color<V: 'static>(&self, intensity: f32, cx: &Context<V>) -> Hsla {
        // Use theme color and adjust brightness based on intensity
        let accent = cx.theme().accent;
        Hsla {
            h: accent.h,
            s: accent.s,
            l: 0.3 + (1.0 - intensity) * 0.4, // Higher intensity = darker color
            a: 0.8 + intensity * 0.2,
        }
    }

    /// Format date display
    fn format_date(&self, date: &str) -> String {
        // Input format: YYYY-MM-DD, Output: MM-DD
        if date.len() >= 10 {
            date[5..10].to_string()
        } else {
            date.to_string()
        }
    }
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
