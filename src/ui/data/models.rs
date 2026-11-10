use super::*;

// ==================== Models ====================

/// Everything the Models page renders: per-model token economics for the
/// selected range's window, the daily token chart, and the headline
/// ratios derived from the same rows.
pub struct ModelsData {
    /// One row per (provider, model), most expensive first. Models that
    /// only carry cost (`has_token_data == false`) stay in the list.
    pub models: Vec<crate::model::ModelTokenSummary>,
    /// Tokens per day of the window, summed across models.
    pub daily_tokens: Vec<ChartPoint>,
    /// Window usage cost across all models.
    pub total_cost: f64,
    /// Window tokens across all models (input + output + cache).
    pub total_tokens: f64,
    /// Blended $/1M tokens over the window; `None` without token data.
    pub blended: Option<f64>,
    /// Cache-read share of input tokens (0–100); `None` without input
    /// token data.
    pub cache_share: Option<f64>,
    /// Data-quality-style findings over the model rows; empty when the
    /// window is clean.
    pub findings: Vec<crate::model::TokenFinding>,
    pub currency: String,
}

/// Load the Models page's data. Blocking; wrap in `smol::unblock`.
///
/// Never fails: an empty ledger yields zero totals and empty vectors, and
/// each sub-query degrades on its own — a failed read is logged and its
/// piece of the page comes back empty.
pub fn load_models(range: Range) -> ModelsData {
    let now = Utc::now();
    let ((since, until), (prior_since, prior_until)) = range.windows(now);
    let prior_until = match range {
        Range::Mtd => analytics::same_days_end(prior_since, prior_until, now),
        _ => prior_until,
    };

    let mut models = query::model_token_summary(since, until, prior_since, prior_until)
        .unwrap_or_else(|e| {
            tracing::warn!("Could not read the per-model token summary: {}", e);
            Vec::new()
        });
    models.sort_by(|a, b| b.usage_cost.total_cmp(&a.usage_cost));

    let daily = query::daily_model_tokens(since, until).unwrap_or_else(|e| {
        tracing::warn!("Could not read the daily token series: {}", e);
        Vec::new()
    });
    // One point per day, models summed together; the map keys are
    // `YYYY-MM-DD`, so iteration order is the chart's day order.
    let mut by_day: BTreeMap<String, f64> = BTreeMap::new();
    for row in daily {
        *by_day.entry(row.day).or_insert(0.0) += row.tokens;
    }
    let daily_tokens = by_day
        .into_iter()
        .map(|(day, tokens)| ChartPoint {
            label: day,
            amount: tokens,
        })
        .collect();

    let total_cost: f64 = models.iter().map(|m| m.usage_cost).sum();
    let input_side: f64 = models.iter().map(|m| m.input_side()).sum();
    let cache_read: f64 = models.iter().map(|m| m.tokens_cache_read).sum();
    let total_tokens: f64 = models.iter().map(|m| m.tokens()).sum();

    // A ratio on a dust base is noise — the same rule the change percents
    // follow.
    let blended = (total_tokens >= fmt::DUST_THRESHOLD)
        .then(|| analytics::blended_unit_cost(total_cost, total_tokens))
        .flatten();
    // cache_read_share is a 0–1 fraction; the page renders a percent.
    let cache_share = (input_side >= fmt::DUST_THRESHOLD)
        .then(|| {
            analytics::cache_read_share(cache_read, input_side - cache_read)
                .map(|share| share * 100.0)
        })
        .flatten();
    let findings = analytics::token_findings(&models);

    ModelsData {
        models,
        daily_tokens,
        total_cost,
        total_tokens,
        blended,
        cache_share,
        findings,
        currency: reporting_currency(),
    }
}
