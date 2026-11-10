use super::*;
use crate::cloud::BillingPeriod;
use crate::ledger::{self, BalanceSnapshot, Channel, Charge, PeriodKey};
use chrono::TimeZone;

/// Both stores, in memory: app state and the ledger.
fn stores() -> (Connection, Connection) {
    let app = Connection::open_in_memory().expect("in-memory duckdb");
    db::prepare_schema(&app).expect("app schema applies");

    let ledger = Connection::open_in_memory().expect("in-memory duckdb");
    ledger::schema::apply(&ledger).expect("ledger schema applies");
    ledger::schema::apply_reporting_currency(&ledger, "USD").expect("view applies");

    (app, ledger)
}

fn day(days_ago: i64) -> DateTime<Utc> {
    (Utc::now() - Duration::days(days_ago))
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc()
}

fn usage(service: &str, amount: f64, at: DateTime<Utc>, tags: Option<&str>) -> Charge {
    Charge {
        service_name: Some(service.to_string()),
        billed_cost: Some(amount),
        tags: tags.map(str::to_string),
        ..Charge::new(at, at + Duration::hours(1), "USD")
    }
}

/// Write a whole period's charges (the ledger replaces periods whole).
fn write_period(ledger: &mut Connection, period: BillingPeriod, charges: &[Charge]) {
    write_period_for(ledger, "acct-1", period, charges);
}

/// [`write_period`] for any account id.
fn write_period_for(
    ledger: &mut Connection,
    account_id: &str,
    period: BillingPeriod,
    charges: &[Charge],
) {
    let key = PeriodKey::new("AWS", account_id, period.label());
    ledger::write_period(
        ledger,
        &key,
        &ledger::new_batch_id(),
        charges,
        None,
        Channel::Api,
    )
    .unwrap();
}

/// Write charges for an account, split by billing period (the ledger
/// replaces periods whole).
fn write_charges(ledger: &mut Connection, account_id: &str, charges: &[Charge]) {
    let current = BillingPeriod::containing(Utc::now());
    for period in [current, current.previous()] {
        let batch: Vec<Charge> = charges
            .iter()
            .filter(|c| BillingPeriod::containing(c.charge_period_start) == period)
            .cloned()
            .collect();
        if !batch.is_empty() {
            write_period_for(ledger, account_id, period, &batch);
        }
    }
}

fn add_aws_account(app: &Connection, id: &str, name: &str) {
    app.execute(
        "INSERT INTO cloud_accounts
         (id, name, source_id, region, created_at, last_synced_at, enabled)
         VALUES (?, ?, 'AWS', 'us-east-1', '2026-08-01T00:00:00+00:00', NULL, true)",
        duckdb::params![id, name],
    )
    .unwrap();
}

fn set_budget(app: &Connection, account_id: &str, monthly_budget: f64, alert_threshold: f64) {
    app.execute(
        "INSERT OR REPLACE INTO budgets
         (account_id, monthly_budget, currency, alert_threshold, created_at, updated_at)
         VALUES (?, ?, 'USD', ?, '2026-08-01T00:00:00+00:00', '2026-08-01T00:00:00+00:00')",
        duckdb::params![account_id, monthly_budget, alert_threshold],
    )
    .unwrap();
}

fn add_deepseek_account(app: &Connection) {
    app.execute(
        "INSERT INTO cloud_accounts
         (id, name, source_id, region, created_at, last_synced_at, enabled)
         VALUES ('acct-ds', 'DeepSeek dev', 'DeepSeek', NULL,
                 '2026-08-01T00:00:00+00:00', NULL, true)",
        [],
    )
    .unwrap();
}

fn snapshot(ledger: &mut Connection, days_ago: i64, balance: f64) {
    ledger::write_balance(
        ledger,
        &BalanceSnapshot {
            provider: "DeepSeek".to_string(),
            account_id: "acct-ds".to_string(),
            observed_at: day(days_ago),
            balance,
            granted_balance: None,
            topped_up_balance: Some(balance),
            currency: "CNY".to_string(),
        },
    )
    .unwrap();
}

#[test]
fn the_default_rules_seed_once() {
    let (app, _) = stores();

    seed_default_rules_on(&app).unwrap();
    seed_default_rules_on(&app).unwrap();

    let rules = db::get_alert_rules_of(&app).unwrap();
    assert_eq!(rules.len(), 3);
    assert!(rules.iter().all(|rule| rule.enabled));
}

#[test]
fn a_disabled_default_rule_stays_disabled() {
    let (app, _) = stores();

    seed_default_rules_on(&app).unwrap();
    db::set_alert_rule_enabled_to(&app, RULE_BALANCE_FLOOR, false).unwrap();
    seed_default_rules_on(&app).unwrap();

    let rules = db::get_alert_rules_of(&app).unwrap();
    let floor = rules
        .iter()
        .find(|rule| rule.id == RULE_BALANCE_FLOOR)
        .unwrap();
    assert!(!floor.enabled);
}

#[test]
fn a_breach_needs_consecutive_days_over_the_baseline() {
    let daily: BTreeMap<NaiveDate, f64> = (1..=14)
        .map(|d| {
            let day = Utc
                .with_ymd_and_hms(2026, 8, d, 0, 0, 0)
                .unwrap()
                .date_naive();
            // Ten a day, then 40 on the last two: 4× a baseline of 10.
            (day, if d >= 13 { 40.0 } else { 10.0 })
        })
        .collect();

    let breach = detect_breach(&daily, 2.5, 2).expect("two days at 4× is a breach");
    assert_eq!(breach.streak_days(), 2);
    assert!((breach.last_amount - 40.0).abs() < 1e-9);
    // The last day's baseline spans the six flat days and the first
    // breach day: (6×10 + 40) / 7.
    assert!((breach.baseline - 100.0 / 7.0).abs() < 1e-9);

    // One breaching day is not yet a streak.
    let last = Utc
        .with_ymd_and_hms(2026, 8, 14, 0, 0, 0)
        .unwrap()
        .date_naive();
    let one_day: BTreeMap<NaiveDate, f64> = daily
        .keys()
        .map(|day| (*day, if *day == last { 40.0 } else { 10.0 }))
        .collect();
    assert!(detect_breach(&one_day, 2.5, 2).is_none());
}

#[test]
fn a_zero_baseline_never_fires() {
    // A brand-new service: any spend is infinitely above "nothing".
    let daily: BTreeMap<NaiveDate, f64> = [(2, 50.0), (3, 60.0)]
        .iter()
        .map(|(d, amount)| {
            (
                Utc.with_ymd_and_hms(2026, 8, *d, 0, 0, 0)
                    .unwrap()
                    .date_naive(),
                *amount,
            )
        })
        .collect();

    assert!(detect_breach(&daily, 2.5, 2).is_none());
}

#[test]
fn a_cost_anomaly_fires_once_a_day_while_the_streak_holds() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();

    // Flat 10/day for twelve days, then 40/day for two. Everything is
    // tagged, so the untagged-ratio rule has nothing to say here.
    let tag = Some(r#"{"business_line":"etl"}"#);
    let mut charges = Vec::new();
    for back in (2..=13).rev() {
        charges.push(usage("Claude", 10.0, day(back), tag));
    }
    charges.push(usage("Claude", 40.0, day(1), tag));
    charges.push(usage("Claude", 40.0, day(0), tag));
    // A flat service alongside, which must not fire.
    for back in (0..=13).rev() {
        charges.push(usage("S3", 10.0, day(back), tag));
    }

    // Split by billing period, as the ledger replaces whole periods.
    let current = BillingPeriod::containing(Utc::now());
    let previous = current.previous();
    let this_month: Vec<Charge> = charges
        .iter()
        .filter(|c| BillingPeriod::containing(c.charge_period_start) == current)
        .cloned()
        .collect();
    let last_month: Vec<Charge> = charges
        .iter()
        .filter(|c| BillingPeriod::containing(c.charge_period_start) == previous)
        .cloned()
        .collect();
    write_period(&mut ledger, current, &this_month);
    if !last_month.is_empty() {
        write_period(&mut ledger, previous, &last_month);
    }

    let created = evaluate_with(&app, &ledger, Utc::now()).unwrap();
    assert_eq!(created, 1);

    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.rule_id, RULE_COST_ANOMALY);
    assert_eq!(event.severity, Severity::Critical);
    assert!(event.title.contains("Claude"), "{}", event.title);
    assert!(event.body.contains("2 consecutive days"), "{}", event.body);
    assert!(event.stat_json.is_some());

    // Same day, same condition: no second event.
    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);

    // A disabled rule fires nothing, even on a later day.
    db::set_alert_rule_enabled_to(&app, RULE_COST_ANOMALY, false).unwrap();
    assert_eq!(
        evaluate_with(&app, &ledger, Utc::now() + Duration::days(1)).unwrap(),
        0
    );
}

#[test]
fn a_balance_below_its_floor_projects_days_remaining() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_deepseek_account(&app);

    // ¥10/day of burn, ¥30 left against the default ¥200 floor.
    for (days_ago, balance) in [(6, 90.0), (4, 70.0), (2, 50.0), (0, 30.0)] {
        snapshot(&mut ledger, days_ago, balance);
    }

    let created = evaluate_with(&app, &ledger, Utc::now()).unwrap();
    assert_eq!(created, 1);

    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    let event = &events[0];
    assert_eq!(event.rule_id, RULE_BALANCE_FLOOR);
    assert_eq!(event.severity, Severity::Warning);
    assert!(event.body.contains("¥30.00"), "{}", event.body);
    assert!(event.body.contains("¥200.00 floor"), "{}", event.body);
    assert!(event.body.contains("roughly 3 days"), "{}", event.body);
}

fn balance_events(app: &Connection) -> Vec<AlertEvent> {
    db::get_alert_events_of(app, &[AlertStatus::Open, AlertStatus::Snoozed]).unwrap()
}

#[test]
fn a_low_balance_stays_one_card_with_the_latest_figures() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_deepseek_account(&app);
    snapshot(&mut ledger, 1, 30.0);

    let now = Utc::now();
    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 1);
    let first = balance_events(&app);
    assert_eq!(first.len(), 1);

    // Days later, lower: no new card, and no new notification.
    snapshot(&mut ledger, 0, 5.0);
    let later = now + Duration::days(3);
    assert_eq!(evaluate_with(&app, &ledger, later).unwrap(), 0);
    let events = balance_events(&app);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, first[0].id);
    assert_eq!(events[0].created_at, first[0].created_at);
    assert!(events[0].body.contains("5.00"), "{}", events[0].body);
}

#[test]
fn dated_cards_from_earlier_versions_merge_into_one() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_deepseek_account(&app);
    snapshot(&mut ledger, 0, 30.0);
    let now = Utc::now();
    evaluate_with(&app, &ledger, now).unwrap();
    let mut older = balance_events(&app)[0].clone();
    for days in [1, 2] {
        older.id = format!("old-{days}");
        older.created_at = now - Duration::days(days);
        older.dedupe_key = format!("{}|2026-01-0{days}", older.dedupe_key);
        db::insert_alert_event_to(&app, &older).unwrap();
        older.dedupe_key = older.dedupe_key.split("|2026").next().unwrap().to_string();
    }
    assert_eq!(balance_events(&app).len(), 3);

    evaluate_with(&app, &ledger, now).unwrap();
    assert_eq!(balance_events(&app).len(), 1);
}

#[test]
fn a_dismissed_balance_waits_a_day_and_a_snoozed_one_wakes_once() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_deepseek_account(&app);
    snapshot(&mut ledger, 0, 30.0);
    let now = Utc::now();
    evaluate_with(&app, &ledger, now).unwrap();
    let id = balance_events(&app)[0].id.clone();

    // Snoozed: updated quietly until it runs out, then counted once.
    let until = now + Duration::hours(2);
    db::set_alert_event_status_to(&app, &id, AlertStatus::Snoozed, Some(until)).unwrap();
    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 0);
    assert_eq!(balance_events(&app)[0].status, AlertStatus::Snoozed);
    let after = until + Duration::minutes(1);
    assert_eq!(evaluate_with(&app, &ledger, after).unwrap(), 1);
    assert_eq!(balance_events(&app)[0].status, AlertStatus::Open);
    assert_eq!(evaluate_with(&app, &ledger, after).unwrap(), 0);

    // Dismissed: nothing more today, a new card once the day has passed.
    db::set_alert_event_status_to(&app, &id, AlertStatus::Dismissed, None).unwrap();
    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
    assert!(balance_events(&app).is_empty());
    let tomorrow = Utc::now() + Duration::days(1);
    assert_eq!(evaluate_with(&app, &ledger, tomorrow).unwrap(), 1);
}

#[test]
fn an_accounts_budget_is_its_floor() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_deepseek_account(&app);
    app.execute(
        "INSERT INTO budgets
         (account_id, monthly_budget, currency, alert_threshold, created_at, updated_at)
         VALUES ('acct-ds', 25.0, 'CNY', 80.0,
                 '2026-08-01T00:00:00+00:00', '2026-08-01T00:00:00+00:00')",
        [],
    )
    .unwrap();

    // ¥30: below the default floor of 200, but above this account's 25.
    snapshot(&mut ledger, 0, 30.0);

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
}

#[test]
fn a_balance_back_above_the_floor_resolves_the_event() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_deepseek_account(&app);
    snapshot(&mut ledger, 0, 30.0);

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);

    // A top-up arrives.
    snapshot(&mut ledger, -1, 500.0);
    assert_eq!(resolve_stale_with(&app, &ledger, Utc::now()).unwrap(), 1);
    assert!(db::get_alert_events_of(&app, &[AlertStatus::Open])
        .unwrap()
        .is_empty());
    assert_eq!(
        db::get_alert_events_of(&app, &[AlertStatus::Resolved])
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn a_growing_untagged_share_warns_once_a_day() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();

    let current = BillingPeriod::containing(Utc::now());
    let previous = current.previous();
    let at_in = |period: BillingPeriod| period.start().and_hms_opt(12, 0, 0).unwrap().and_utc();

    // This month: half untagged. Last month: a tenth untagged.
    let this_month = vec![
        usage("EC2", 50.0, at_in(current), None),
        usage(
            "S3",
            50.0,
            at_in(current),
            Some(r#"{"business_line":"etl"}"#),
        ),
    ];
    let last_month = vec![
        usage("EC2", 10.0, at_in(previous), None),
        usage(
            "S3",
            90.0,
            at_in(previous),
            Some(r#"{"business_line":"etl"}"#),
        ),
    ];

    let key = PeriodKey::new("AWS", "acct-1", current.label());
    ledger::write_period(
        &mut ledger,
        &key,
        &ledger::new_batch_id(),
        &this_month,
        None,
        Channel::Api,
    )
    .unwrap();
    let key = PeriodKey::new("AWS", "acct-1", previous.label());
    ledger::write_period(
        &mut ledger,
        &key,
        &ledger::new_batch_id(),
        &last_month,
        None,
        Channel::Api,
    )
    .unwrap();

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    assert_eq!(events[0].rule_id, RULE_UNTAGGED_RATIO);
    assert!(events[0].body.contains("50%"), "{}", events[0].body);

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
}

#[test]
fn a_shrinking_or_small_untagged_share_does_not_warn() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();

    let current = BillingPeriod::containing(Utc::now());
    let at_in = |period: BillingPeriod| period.start().and_hms_opt(12, 0, 0).unwrap().and_utc();

    // 10% untagged: under the threshold.
    let charges = vec![
        usage("EC2", 10.0, at_in(current), None),
        usage(
            "S3",
            90.0,
            at_in(current),
            Some(r#"{"business_line":"etl"}"#),
        ),
    ];
    let key = PeriodKey::new("AWS", "acct-1", current.label());
    ledger::write_period(
        &mut ledger,
        &key,
        &ledger::new_batch_id(),
        &charges,
        None,
        Channel::Api,
    )
    .unwrap();

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
}

#[test]
fn a_snooze_holds_the_condition_quiet_until_it_runs_out() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_deepseek_account(&app);
    snapshot(&mut ledger, 0, 30.0);

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();

    // Snoozed into the future: no refire.
    db::set_alert_event_status_to(
        &app,
        &events[0].id,
        AlertStatus::Snoozed,
        Some(Utc::now() + Duration::hours(24)),
    )
    .unwrap();
    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);

    // Snooze expired: the condition fires again.
    db::set_alert_event_status_to(
        &app,
        &events[0].id,
        AlertStatus::Snoozed,
        Some(Utc::now() - Duration::hours(1)),
    )
    .unwrap();
    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
}

#[test]
fn an_empty_ledger_and_no_accounts_fire_nothing() {
    let (app, ledger) = stores();
    seed_default_rules_on(&app).unwrap();

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
    assert_eq!(resolve_stale_with(&app, &ledger, Utc::now()).unwrap(), 0);
}

#[test]
fn a_custom_rule_is_created_enabled() {
    let (app, _) = stores();

    let view = create_rule_on(
        &app,
        RULE_UNTAGGED_RATIO,
        "Watch unallocated",
        json!({ "threshold": 0.25 }),
    )
    .unwrap();

    assert!(view.id.starts_with(CUSTOM_RULE_PREFIX), "{}", view.id);
    assert!(view.enabled);
    assert_eq!(view.scope, "All sources");
    assert!(
        view.condition_chips[0].contains("25%"),
        "{:?}",
        view.condition_chips
    );

    let rules = db::get_alert_rules_of(&app).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "Watch unallocated");
}

#[test]
fn invalid_configs_are_refused() {
    let (app, _) = stores();

    assert!(create_rule_on(
        &app,
        RULE_COST_ANOMALY,
        "x",
        json!({ "multiplier": 1.0, "consecutive_days": 2 })
    )
    .is_err());
    assert!(create_rule_on(
        &app,
        RULE_COST_ANOMALY,
        "x",
        json!({ "multiplier": 2.5, "consecutive_days": 0 })
    )
    .is_err());
    assert!(create_rule_on(&app, RULE_BALANCE_FLOOR, "x", json!({ "floor": 0.0 })).is_err());
    assert!(create_rule_on(&app, RULE_UNTAGGED_RATIO, "x", json!({ "threshold": 1.5 })).is_err());
    // Budget rules.
    assert!(create_rule_on(&app, RULE_BUDGET, "x", json!({ "based": "cost" })).is_err());
    assert!(create_rule_on(&app, RULE_BUDGET, "x", json!({ "account_id": "  " })).is_err());
    assert!(create_rule_on(
        &app,
        RULE_BUDGET,
        "x",
        json!({ "account_id": "acct-1", "based": "sideways" })
    )
    .is_err());
    assert!(create_rule_on(
        &app,
        RULE_BUDGET,
        "x",
        json!({ "account_id": "acct-1", "threshold_type": "absolute" })
    )
    .is_err());
    assert!(create_rule_on(
        &app,
        RULE_BUDGET,
        "x",
        json!({ "account_id": "acct-1", "threshold_type": "percent", "threshold": -5.0 })
    )
    .is_err());
    // A valid config still fails when the account does not exist.
    assert!(create_rule_on(
        &app,
        RULE_BUDGET,
        "x",
        json!({ "account_id": "ghost", "based": "cost", "threshold_type": "percent" })
    )
    .is_err());
    assert!(create_rule_on(&app, "nonsense", "x", json!({})).is_err());
    assert!(create_rule_on(&app, RULE_BALANCE_FLOOR, "   ", json!({ "floor": 50.0 })).is_err());

    // Nothing was stored along the way.
    assert!(db::get_alert_rules_of(&app).unwrap().is_empty());
}

#[test]
fn only_custom_rules_can_be_deleted() {
    let (app, _) = stores();
    seed_default_rules_on(&app).unwrap();

    assert!(delete_rule_on(&app, RULE_BALANCE_FLOOR).is_err());

    let view = create_rule_on(
        &app,
        RULE_BALANCE_FLOOR,
        "Low floor",
        json!({ "floor": 50.0 }),
    )
    .unwrap();
    assert_eq!(db::get_alert_rules_of(&app).unwrap().len(), 4);

    delete_rule_on(&app, &view.id).unwrap();
    let rules = db::get_alert_rules_of(&app).unwrap();
    assert_eq!(rules.len(), 3);
    assert!(rules.iter().all(|rule| rule.id != view.id));
}

#[test]
fn a_custom_rule_evaluates_like_a_seeded_one() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    // The seeded floor of 200 would not fire on a balance of 300; the
    // custom floor of 500 must.
    db::set_alert_rule_enabled_to(&app, RULE_BALANCE_FLOOR, false).unwrap();
    add_deepseek_account(&app);

    let view = create_rule_on(
        &app,
        RULE_BALANCE_FLOOR,
        "High floor",
        json!({ "floor": 500.0 }),
    )
    .unwrap();
    snapshot(&mut ledger, 0, 300.0);

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    assert_eq!(events[0].rule_id, view.id);
}

#[test]
fn a_budget_rule_fires_on_cost_over_an_absolute_threshold() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_aws_account(&app, "acct-1", "Prod");
    set_budget(&app, "acct-1", 100.0, 80.0);

    // Pinned mid-month; everything tagged so the other rules stay quiet.
    let now = Utc.with_ymd_and_hms(2026, 8, 16, 12, 0, 0).unwrap();
    let period = BillingPeriod::containing(now);
    let charges = vec![usage(
        "EC2",
        120.0,
        period.start().and_hms_opt(2, 0, 0).unwrap().and_utc(),
        Some(r#"{"business_line":"etl"}"#),
    )];
    write_period(&mut ledger, period, &charges);

    let view = create_rule_on(
        &app,
        RULE_BUDGET,
        "Prod budget",
        json!({
            "account_id": "acct-1",
            "based": "cost",
            "threshold_type": "absolute",
            "threshold": 100.0,
        }),
    )
    .unwrap();
    // The scope chip names the watched account.
    assert_eq!(view.scope, "Prod");

    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.rule_id, view.id);
    // Over the budget itself, not just the threshold: critical.
    assert_eq!(event.severity, Severity::Critical);
    assert!(event.title.contains("Prod"), "{}", event.title);
    assert!(event.body.contains("month-to-date cost"), "{}", event.body);
    assert!(event.body.contains("$120.00"), "{}", event.body);

    // Same day, same condition: no second event (the day-grained
    // debounce every rule here shares).
    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 0);
    // The next day the still-holding condition fires again.
    assert_eq!(
        evaluate_with(&app, &ledger, now + Duration::days(1)).unwrap(),
        1
    );
}

#[test]
fn a_budget_rule_defaults_to_the_budgets_own_alert_threshold() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_aws_account(&app, "acct-1", "Prod");
    set_budget(&app, "acct-1", 100.0, 80.0);

    let now = Utc.with_ymd_and_hms(2026, 8, 16, 12, 0, 0).unwrap();
    let period = BillingPeriod::containing(now);
    let charges = vec![usage(
        "EC2",
        85.0,
        period.start().and_hms_opt(2, 0, 0).unwrap().and_utc(),
        Some(r#"{"business_line":"etl"}"#),
    )];
    write_period(&mut ledger, period, &charges);

    // A percent rule with no number of its own reads the budget's
    // alert_threshold: 80% of 100 = 80, and 85 crosses it.
    let view = create_rule_on(
        &app,
        RULE_BUDGET,
        "Prod budget",
        json!({ "account_id": "acct-1", "based": "cost", "threshold_type": "percent" }),
    )
    .unwrap();

    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    let event = &events[0];
    assert_eq!(event.rule_id, view.id);
    // Past the threshold but under the budget: a warning.
    assert_eq!(event.severity, Severity::Warning);
    assert!(event.body.contains("85% of the $100.00"), "{}", event.body);
    assert!(
        event.stat_json.as_ref().unwrap().contains("85%"),
        "{:?}",
        event.stat_json
    );
}

#[test]
fn a_forecast_budget_rule_fires_before_the_money_is_spent() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_aws_account(&app, "acct-1", "Prod");
    set_budget(&app, "acct-1", 1000.0, 80.0);

    // Day 16 of 31, $60 spent so far: a daily rate of $3.75 projects
    // $116.25 — over 100 on the projection, under it on the actuals.
    let now = Utc.with_ymd_and_hms(2026, 8, 16, 12, 0, 0).unwrap();
    let period = BillingPeriod::containing(now);
    let charges = vec![usage(
        "EC2",
        60.0,
        period.start().and_hms_opt(2, 0, 0).unwrap().and_utc(),
        Some(r#"{"business_line":"etl"}"#),
    )];
    write_period(&mut ledger, period, &charges);

    let forecast_rule = create_rule_on(
        &app,
        RULE_BUDGET,
        "Prod forecast",
        json!({
            "account_id": "acct-1",
            "based": "forecast",
            "threshold_type": "absolute",
            "threshold": 100.0,
        }),
    )
    .unwrap();
    let cost_rule = create_rule_on(
        &app,
        RULE_BUDGET,
        "Prod cost",
        json!({
            "account_id": "acct-1",
            "based": "cost",
            "threshold_type": "absolute",
            "threshold": 100.0,
        }),
    )
    .unwrap();

    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.rule_id, forecast_rule.id);
    assert!(
        event.body.contains("month-end projection"),
        "{}",
        event.body
    );
    assert!(event.body.contains("$116.25"), "{}", event.body);
    assert!(event.rule_id != cost_rule.id);
}

#[test]
fn raising_the_budget_resolves_the_event() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_aws_account(&app, "acct-1", "Prod");
    set_budget(&app, "acct-1", 100.0, 80.0);

    let now = Utc.with_ymd_and_hms(2026, 8, 16, 12, 0, 0).unwrap();
    let period = BillingPeriod::containing(now);
    let charges = vec![usage(
        "EC2",
        90.0,
        period.start().and_hms_opt(2, 0, 0).unwrap().and_utc(),
        Some(r#"{"business_line":"etl"}"#),
    )];
    write_period(&mut ledger, period, &charges);

    create_rule_on(
        &app,
        RULE_BUDGET,
        "Prod budget",
        json!({ "account_id": "acct-1", "based": "cost", "threshold_type": "percent" }),
    )
    .unwrap();
    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 1);

    // 90 of a 200 budget is 45% — under the 80% line now.
    set_budget(&app, "acct-1", 200.0, 80.0);
    assert_eq!(resolve_stale_with(&app, &ledger, now).unwrap(), 1);
    assert!(db::get_alert_events_of(&app, &[AlertStatus::Open])
        .unwrap()
        .is_empty());
}

#[test]
fn a_budget_rule_without_a_budget_never_fires() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    add_aws_account(&app, "acct-1", "Prod");

    let now = Utc.with_ymd_and_hms(2026, 8, 16, 12, 0, 0).unwrap();
    let period = BillingPeriod::containing(now);
    let charges = vec![usage(
        "EC2",
        500.0,
        period.start().and_hms_opt(2, 0, 0).unwrap().and_utc(),
        Some(r#"{"business_line":"etl"}"#),
    )];
    write_period(&mut ledger, period, &charges);

    create_rule_on(
        &app,
        RULE_BUDGET,
        "Prod budget",
        json!({
            "account_id": "acct-1",
            "based": "cost",
            "threshold_type": "absolute",
            "threshold": 100.0,
        }),
    )
    .unwrap();

    // No budgets row for the account: nothing to measure against.
    assert_eq!(evaluate_with(&app, &ledger, now).unwrap(), 0);
}

#[test]
fn a_part_imported_today_does_not_hide_yesterdays_breach() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();

    // Flat 10/day, then 40 on the two days before today — a two-day
    // breach that completed yesterday. Today holds only a part-imported
    // 2.0, which would break the streak if it were the anchor.
    let tag = Some(r#"{"business_line":"etl"}"#);
    let mut charges = Vec::new();
    for back in (3..=13).rev() {
        charges.push(usage("Claude", 10.0, day(back), tag));
    }
    charges.push(usage("Claude", 40.0, day(2), tag));
    charges.push(usage("Claude", 40.0, day(1), tag));
    charges.push(usage("Claude", 2.0, day(0), tag));
    write_charges(&mut ledger, "acct-1", &charges);

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    let event = &events[0];
    assert_eq!(event.rule_id, RULE_COST_ANOMALY);
    assert!(event.title.contains("Claude"), "{}", event.title);
    // The breach is reported against yesterday, the last complete day.
    let yesterday = day(1).date_naive().to_string();
    assert!(
        event.stat_json.as_ref().unwrap().contains(&yesterday),
        "{:?}",
        event.stat_json
    );

    // And it debounces like any other firing.
    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
}

#[test]
fn an_account_scoped_anomaly_rule_ignores_other_accounts() {
    let (app, mut ledger) = stores();
    seed_default_rules_on(&app).unwrap();
    // The seeded unscoped rule would fire for both accounts; only the
    // scoped one is under test.
    db::set_alert_rule_enabled_to(&app, RULE_COST_ANOMALY, false).unwrap();

    // The same two-day breach on two accounts of one provider.
    let tag = Some(r#"{"business_line":"etl"}"#);
    let mut charges = Vec::new();
    for back in (2..=13).rev() {
        charges.push(usage("Claude", 10.0, day(back), tag));
    }
    charges.push(usage("Claude", 40.0, day(1), tag));
    charges.push(usage("Claude", 40.0, day(0), tag));
    write_charges(&mut ledger, "acct-1", &charges);
    write_charges(&mut ledger, "acct-2", &charges);

    let view = create_rule_on(
        &app,
        RULE_COST_ANOMALY,
        "Prod anomaly",
        json!({ "multiplier": 2.5, "consecutive_days": 2, "account_id": "acct-1" }),
    )
    .unwrap();

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 1);
    let events = db::get_alert_events_of(&app, &[AlertStatus::Open]).unwrap();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.rule_id, view.id);
    assert!(
        event.dedupe_key.starts_with("cost|acct-1|"),
        "{}",
        event.dedupe_key
    );
    assert!(
        event.fields_json.contains("acct-1"),
        "{}",
        event.fields_json
    );

    assert_eq!(evaluate_with(&app, &ledger, Utc::now()).unwrap(), 0);
}
