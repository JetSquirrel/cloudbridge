use super::*;

// ==================== Resolution ====================

/// Auto-resolve open events whose condition no longer holds — the
/// "Resolved this month" list. Returns how many were resolved.
pub fn resolve_stale_alerts() -> Result<usize> {
    crate::ledger::with_connection_ref(|ledger| {
        db::with_connection(|app| resolve_stale_with(app, ledger, Utc::now()))
    })
}

pub(crate) fn resolve_stale_with(
    app: &Connection,
    ledger: &Connection,
    now: DateTime<Utc>,
) -> Result<usize> {
    let rules = db::get_alert_rules_of(app)?;
    let open = db::get_alert_events_of(app, &[AlertStatus::Open])?;

    // Reads shared by every open event of a kind, filled on first use: the
    // period's untagged share, the anomaly window per account scope, and the
    // accounts list the budget check matches against. Without these caches
    // each event re-ran the full window scan behind its own check.
    let mut untagged_share_now: Option<f64> = None;
    let mut anomaly_windows: HashMap<Option<String>, Vec<query::ServiceDailyTotal>> =
        HashMap::new();
    let mut accounts: Option<Vec<crate::cloud::CloudAccount>> = None;

    let mut resolved = 0;
    for event in open {
        let Some(rule) = rules.iter().find(|rule| rule.id == event.rule_id) else {
            continue;
        };
        let context = event.context().unwrap_or_else(|| json!({}));

        let stale = match rule.kind.as_str() {
            RULE_BALANCE_FLOOR => {
                let provider = context.get("provider").and_then(|v| v.as_str());
                let account_id = context.get("account_id").and_then(|v| v.as_str());
                let floor = context.get("floor").and_then(|v| v.as_f64());
                match (provider, account_id, floor) {
                    (Some(provider), Some(account_id), Some(floor)) => {
                        query::latest_balance_of(ledger, provider, account_id)?
                            .is_some_and(|balance| balance.balance >= floor)
                    }
                    _ => false,
                }
            }
            RULE_UNTAGGED_RATIO => {
                let threshold = config_f64(rule, "threshold", DEFAULT_UNTAGGED_THRESHOLD);
                let share = match untagged_share_now {
                    Some(share) => share,
                    None => {
                        let current = crate::cloud::BillingPeriod::containing(now);
                        let share = untagged_share(&query::tag_breakdown_of(
                            ledger,
                            &current.label(),
                            BUSINESS_LINE_TAG,
                            None,
                        )?);
                        untagged_share_now = Some(share);
                        share
                    }
                };
                share <= threshold
            }
            RULE_COST_ANOMALY => {
                let provider = context.get("provider").and_then(|v| v.as_str());
                let service = context.get("service").and_then(|v| v.as_str());
                match (provider, service) {
                    (Some(provider), Some(service)) => {
                        let multiplier = config_f64(rule, "multiplier", 2.5);
                        let consecutive = config_u64(rule, "consecutive_days", 2);
                        // The event's context remembers the account a scoped
                        // rule fired for; fall back to the rule's own config.
                        let account_id = context
                            .get("account_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                            .or_else(|| config_str(rule, "account_id"));
                        let rows = match anomaly_windows.entry(account_id) {
                            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
                            std::collections::hash_map::Entry::Vacant(entry) => {
                                let since = now - Duration::days(ANOMALY_WINDOW_DAYS);
                                let rows = match entry.key().as_deref() {
                                    Some(account) => {
                                        daily_totals_for_account_of(ledger, account, since)?
                                    }
                                    None => query::daily_totals_by_service_of(ledger, since)?,
                                };
                                entry.insert(rows)
                            }
                        };
                        let daily: BTreeMap<NaiveDate, f64> = rows
                            .iter()
                            .filter(|row| row.provider == provider && row.service == service)
                            .filter_map(|row| {
                                NaiveDate::parse_from_str(&row.day, "%Y-%m-%d")
                                    .ok()
                                    .map(|day| (day, row.amount))
                            })
                            .collect();
                        detect_breach_with_lag(&daily, multiplier, consecutive, now).is_none()
                    }
                    _ => false,
                }
            }
            RULE_BUDGET => {
                if accounts.is_none() {
                    accounts = Some(db::get_all_accounts_of(app)?);
                }
                let accounts = accounts.as_ref().expect("just populated");
                budget_breach_with(app, ledger, rule, accounts, now)?.is_none()
            }
            _ => false,
        };

        if stale {
            db::set_alert_event_status_to(app, &event.id, AlertStatus::Resolved, None)?;
            resolved += 1;
        }
    }

    Ok(resolved)
}
