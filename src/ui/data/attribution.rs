use super::*;

// ==================== Attribution ====================

/// One step of the attribution path (Source → Service → Business line),
/// matching the Sankey's three columns.
pub struct PathStep {
    pub label: String,
}

/// A node in the Sankey. `column` is 0-based: 0 source, 1 service,
/// 2 business line.
pub struct SankeyNode {
    pub id: String,
    pub label: String,
    pub column: usize,
    /// Throughput in the reporting currency; every column sums to the
    /// period's gross usage.
    pub value: f64,
}

/// A link between two nodes, by node id.
pub struct SankeyLink {
    pub from: String,
    pub to: String,
    pub value: f64,
}

pub struct SankeyData {
    pub nodes: Vec<SankeyNode>,
    pub links: Vec<SankeyLink>,
}

/// One of the largest unattributed services, for the Unallocated card.
pub struct UnallocatedItem {
    pub provider: String,
    pub service: Option<String>,
    /// Free-text charge description; always `None` today, because the
    /// largest rows come from an aggregate by service, not raw charges.
    pub description: Option<String>,
    /// Untagged gross usage.
    pub amount: f64,
}

/// The "Unallocated" explainer card.
pub struct UnallocatedCardData {
    /// Untagged usage.
    pub amount: f64,
    /// Share of the period's usage that is untagged (0–100).
    pub pct: f64,
    /// The largest unattributed services, biggest first.
    pub largest: Vec<UnallocatedItem>,
    pub action: String,
}

/// Everything the Attribution page renders.
pub struct AttributionData {
    pub currency: String,
    pub path: Vec<PathStep>,
    pub sankey: SankeyData,
    pub unallocated: UnallocatedCardData,
}

/// Load the Attribution page's data. Blocking; wrap in `smol::unblock`.
///
/// The Sankey is three levels — source, service, business line —
/// because the ledger holds no API-key hop: charges arrive per account,
/// and the `business_line` tag is the only split below the service. Every
/// flow is gross usage, not net: net flows can be negative, which is
/// meaningless in a Sankey.
///
/// One Sankey row before it becomes nodes and links: the provider, the
/// service label the column shows, its gross usage, and how that usage
/// splits across business lines.
type ServiceFlow = (String, String, f64, Vec<(String, f64)>);

pub fn load_attribution() -> Result<AttributionData> {
    let period = BillingPeriod::containing(Utc::now()).label();
    let currency = reporting_currency();

    let path = [
        tr!("attribution.step_source"),
        tr!("attribution.step_service"),
        tr!("attribution.step_business_line"),
    ]
    .into_iter()
    .map(|label| PathStep {
        label: label.to_string(),
    })
    .collect();

    // (provider, service) → tag rows, assembled link by link so every
    // column sums to the same usage total. One period-wide tag query
    // covers every service, merged "Other" tails included — filtering its
    // rows to one service gives exactly what a per-service breakdown
    // query would return.
    let services = query::provider_service_usage(&period)?;
    let mut tags_by_service: BTreeMap<(String, String), Vec<(String, f64)>> = BTreeMap::new();
    for row in query::tag_usage_breakdown_by_service(&period, BUSINESS_LINE_TAG)? {
        tags_by_service
            .entry((row.provider, row.service))
            .or_default()
            .push((row.tag_value, row.amount));
    }
    let tags_of = |provider: &str, service: &str| {
        tags_by_service
            .get(&(provider.to_string(), service.to_string()))
            .cloned()
            .unwrap_or_default()
    };

    // Per provider, keep the top services and merge the tail into one
    // "Other" node — tag breakdown included, so every column still sums to
    // the same usage total. Beyond a handful of nodes per column the
    // ribbons cross into mush and the thin ones are illegible anyway.
    const TOP_SERVICES_PER_PROVIDER: usize = 5;
    let mut by_provider: BTreeMap<String, Vec<(String, f64)>> = BTreeMap::new();
    for (provider, service, amount) in services {
        by_provider
            .entry(provider)
            .or_default()
            .push((service, amount));
    }

    let mut services: Vec<ServiceFlow> = Vec::new();
    for (provider, mut rows) in by_provider {
        // Largest first: nodes stack bottom-up, so the biggest flows sit
        // low and parallel instead of crossing.
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        let tail = rows.split_off(TOP_SERVICES_PER_PROVIDER.min(rows.len()));
        let mut tail_sum = 0.0;
        let mut tail_tags: BTreeMap<String, f64> = BTreeMap::new();
        for (service, amount) in tail {
            tail_sum += amount;
            for (tag, tag_amount) in tags_of(&provider, &service) {
                *tail_tags.entry(tag).or_insert(0.0) += tag_amount;
            }
        }
        for (service, amount) in rows {
            services.push((
                provider.clone(),
                service.clone(),
                amount,
                tags_of(&provider, &service),
            ));
        }
        if tail_sum > 0.0 {
            services.push((
                provider,
                "Other".to_string(),
                tail_sum,
                tail_tags.into_iter().collect(),
            ));
        }
    }
    // Global largest-first so all three columns stack in matching orders.
    services.sort_by(|a, b| b.2.total_cmp(&a.2));

    let mut nodes: Vec<SankeyNode> = Vec::new();
    let mut links: Vec<SankeyLink> = Vec::new();
    let mut provider_totals: Vec<(String, f64)> = Vec::new();
    let mut line_totals: Vec<(String, f64)> = Vec::new();

    let add_total = |totals: &mut Vec<(String, f64)>, id: String, amount: f64| match totals
        .iter_mut()
        .find(|(existing, _)| *existing == id)
    {
        Some((_, total)) => *total += amount,
        None => totals.push((id, amount)),
    };

    for (provider, service, amount, tags) in &services {
        let src = format!("src-{provider}");
        let svc = format!("svc-{provider}-{service}");
        add_total(&mut provider_totals, src.clone(), *amount);

        // Thick ribbons attach first (lowest) on both ends.
        let mut tags = tags.clone();
        tags.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (tag, tag_amount) in tags {
            let line = format!("line-{tag}");
            add_total(&mut line_totals, line.clone(), tag_amount);
            links.push(SankeyLink {
                from: svc.clone(),
                to: line,
                value: tag_amount,
            });
        }
        // The service node's own throughput is its total, tagged or not.
        links.push(SankeyLink {
            from: src,
            to: svc.clone(),
            value: *amount,
        });
        nodes.push(SankeyNode {
            id: svc,
            label: service.clone(),
            column: 1,
            value: *amount,
        });
    }

    // Largest at the bottom in every column, matching the service column.
    provider_totals.sort_by(|a, b| b.1.total_cmp(&a.1));
    line_totals.sort_by(|a, b| b.1.total_cmp(&a.1));

    for (id, value) in provider_totals {
        let provider = id.trim_start_matches("src-");
        nodes.push(SankeyNode {
            label: registry::get(provider)
                .map(|descriptor| descriptor.display_name.to_string())
                .unwrap_or_else(|| provider.to_string()),
            id,
            column: 0,
            value,
        });
    }
    for (id, value) in line_totals {
        nodes.push(SankeyNode {
            label: id.trim_start_matches("line-").to_string(),
            id,
            column: 2,
            value,
        });
    }

    let (usage_total, _) = query::usage_and_credits(&period)?;
    let unallocated_amount = query::tag_usage_breakdown(&period, BUSINESS_LINE_TAG)?
        .into_iter()
        .find(|(value, _)| value == UNALLOCATED)
        .map(|(_, amount)| amount)
        .unwrap_or(0.0);
    const TOP_UNALLOCATED_SERVICES: usize = 3;
    let largest =
        query::untagged_usage_by_service(&period, BUSINESS_LINE_TAG, TOP_UNALLOCATED_SERVICES)?
            .into_iter()
            .map(|row| UnallocatedItem {
                provider: row.provider,
                service: row.service,
                description: None,
                amount: row.amount,
            })
            .collect();

    Ok(AttributionData {
        currency,
        path,
        sankey: SankeyData { nodes, links },
        unallocated: UnallocatedCardData {
            amount: unallocated_amount,
            pct: if usage_total > 0.0 {
                unallocated_amount / usage_total * 100.0
            } else {
                0.0
            },
            largest,
            action: tr!("attribution.see_untagged").into(),
        },
    })
}
