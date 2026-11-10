use super::*;

/// Midnight UTC of a `YYYY-MM-DD`, for chart tests.
fn at(date: NaiveDate) -> DateTime<Utc> {
    date.and_hms_opt(0, 0, 0)
        .expect("midnight exists")
        .and_utc()
}

#[test]
fn a_history_is_orphaned_only_when_no_account_row_names_it() {
    let history = |id: &str| query::LedgerAccount {
        provider: "AWS".to_string(),
        account_id: id.to_string(),
        first_period: Some("2026-09".to_string()),
        last_period: Some("2026-09".to_string()),
        charges: 1,
        usage_cost: 1.0,
    };
    let account = crate::demo_data::account("AWS", "live", "aws", Utc::now());

    let orphans = orphaned_histories(&[history("live"), history("gone")], &[account]);
    assert_eq!(orphans.len(), 1);
    assert_eq!(orphans[0].account_id, "gone");
}
#[test]
fn daily_series_zero_fills_and_baselines_on_demand() {
    let first = NaiveDate::from_ymd_opt(2026, 9, 1).expect("a real date");
    let last = NaiveDate::from_ymd_opt(2026, 9, 3).expect("a real date");
    let mut by_day = BTreeMap::new();
    by_day.insert(
        NaiveDate::from_ymd_opt(2026, 9, 2).expect("a real date"),
        5.0,
    );

    let chart = daily_series(&by_day, at(first), at(last), false);
    let amounts: Vec<f64> = chart.actual.iter().map(|p| p.amount).collect();
    assert_eq!(amounts, vec![0.0, 5.0, 0.0]);
    assert!(chart.baseline.is_empty());

    let chart = daily_series(&by_day, at(first), at(last), true);
    assert_eq!(chart.baseline.len(), chart.actual.len());
}

#[test]
fn drives_prefers_the_largest_tagged_line() {
    let rows = vec![
        query::ServiceTagUsage {
            provider: "aws".to_string(),
            service: "EC2".to_string(),
            tag_value: UNALLOCATED.to_string(),
            amount: 100.0,
        },
        query::ServiceTagUsage {
            provider: "aws".to_string(),
            service: "EC2".to_string(),
            tag_value: "payments".to_string(),
            amount: 50.0,
        },
        query::ServiceTagUsage {
            provider: "aws".to_string(),
            service: "S3".to_string(),
            tag_value: "analytics".to_string(),
            amount: 10.0,
        },
    ];
    let drives = drives_by_service(rows);
    assert_eq!(
        drives.get(&("aws".to_string(), "EC2".to_string())),
        Some(&"payments".to_string())
    );
    assert_eq!(
        drives.get(&("aws".to_string(), "S3".to_string())),
        Some(&"analytics".to_string())
    );
    assert!(!drives.contains_key(&("gcp".to_string(), "EC2".to_string())));
}

#[test]
fn month_over_month_unions_both_months() {
    let mom = month_over_month(
        query::PeriodOverPeriod {
            current_total: 150.0,
            previous_total: 100.0,
            current_by_service: vec![("EC2".to_string(), 150.0)],
            previous_by_service: vec![("EC2".to_string(), 80.0), ("Retired".to_string(), 20.0)],
        },
        None,
    );
    assert_eq!(mom.change_pct, Some(50.0));

    assert_eq!(mom.services.len(), 2);
    let ec2 = &mom.services[0];
    assert_eq!(ec2.service, "EC2");
    assert_eq!((ec2.current, ec2.previous), (150.0, 80.0));
    assert_eq!(ec2.change_pct, Some(87.5));
    // A service that vanished this month is a −100% mover.
    let retired = &mom.services[1];
    assert_eq!(retired.service, "Retired");
    assert_eq!((retired.current, retired.previous), (0.0, 20.0));
    assert_eq!(retired.change_pct, Some(-100.0));
    assert!(mom.decomposition.is_none());
}

#[test]
fn month_over_month_skips_delta_on_a_dust_base() {
    let mom = month_over_month(
        query::PeriodOverPeriod {
            current_total: 10.0,
            previous_total: 0.0,
            current_by_service: vec![("EC2".to_string(), 10.0)],
            previous_by_service: Vec::new(),
        },
        None,
    );
    assert_eq!(mom.change_pct, None);
    assert_eq!(mom.services[0].change_pct, None);
}

/// A decomposition with every movement kind and more rows than the
/// section lists.
fn sample_decomposition() -> query::CostChangeDecomposition {
    query::CostChangeDecomposition {
        billing_period: "2026-09".to_string(),
        previous_period: "2026-08".to_string(),
        current_total: 150.0,
        previous_total: 100.0,
        total_delta: 50.0,
        by_category: (0..7)
            .map(|i| query::CategoryDelta {
                category: format!("Category{i}"),
                current: 10.0 + f64::from(i),
                previous: 10.0,
                delta: f64::from(i),
            })
            .collect(),
        by_service: vec![
            query::ServiceMovement {
                service: "NewSvc".to_string(),
                kind: query::MovementKind::Appeared,
                current: 40.0,
                previous: 0.0,
                delta: 40.0,
            },
            query::ServiceMovement {
                service: "GoneSvc".to_string(),
                kind: query::MovementKind::Vanished,
                current: 0.0,
                previous: 20.0,
                delta: -20.0,
            },
            query::ServiceMovement {
                service: "BigSvc".to_string(),
                kind: query::MovementKind::Grown,
                current: 100.0,
                previous: 90.0,
                delta: 10.0,
            },
            query::ServiceMovement {
                service: "SmallSvc".to_string(),
                kind: query::MovementKind::Shrunk,
                current: 10.0,
                previous: 15.0,
                delta: -5.0,
            },
        ],
        residual: 2.5,
        reconciled: false,
    }
}

#[test]
fn change_decomposition_maps_badges_and_truncates() {
    let why = change_decomposition(Some(sample_decomposition())).expect("Some in, Some out");
    // Seven category rows in, five out.
    assert_eq!(why.categories.len(), TOP_DECOMPOSITION_ROWS);
    assert_eq!(why.movements.len(), 4);
    let badges: Vec<MovementBadge> = why.movements.iter().map(|row| row.badge).collect();
    assert_eq!(
        badges,
        vec![
            MovementBadge::Appeared,
            MovementBadge::Vanished,
            MovementBadge::Grown,
            MovementBadge::Shrunk,
        ]
    );
    // The reconciliation flags pass through untouched.
    assert!(!why.reconciled);
    assert_eq!(why.residual, 2.5);
}

#[test]
fn change_decomposition_passes_none_through() {
    assert!(change_decomposition(None).is_none());
}

#[test]
fn benchmark_value_skips_empty_and_dust_series() {
    assert_eq!(benchmark_value(Vec::new()), None);
    // A fresh ledger averages to a flat zero line: no real history,
    // no overlay.
    let zeros = (1..=5)
        .map(|day| (format!("2026-09-{day:02}"), 0.0))
        .collect();
    assert_eq!(benchmark_value(zeros), None);
}

#[test]
fn benchmark_value_takes_the_flat_value() {
    let series = (1..=5)
        .map(|day| (format!("2026-09-{day:02}"), 42.0))
        .collect();
    assert_eq!(benchmark_value(series), Some(42.0));
}

#[test]
fn data_quality_rows_sort_critical_first() {
    let issue = |severity: query::IssueSeverity| query::DataQualityIssue {
        kind: query::DataQualityKind::UntaggedUsage,
        severity,
        message: format!("{severity:?}"),
        affected_amount: None,
        affected_count: 1,
    };
    let rows = data_quality_rows(
        "2026-09",
        vec![
            issue(query::IssueSeverity::Info),
            issue(query::IssueSeverity::Critical),
            issue(query::IssueSeverity::Warning),
        ],
        &std::collections::HashSet::new(),
    );
    let severities: Vec<DataQualitySeverity> = rows.iter().map(|row| row.severity).collect();
    assert_eq!(
        severities,
        vec![
            DataQualitySeverity::Critical,
            DataQualitySeverity::Warning,
            DataQualitySeverity::Info,
        ]
    );
}

#[test]
fn data_quality_rows_drop_dismissed_keys_and_keep_new_ones() {
    let issue = |kind: query::DataQualityKind| query::DataQualityIssue {
        kind,
        severity: query::IssueSeverity::Warning,
        message: format!("{kind:?}"),
        affected_amount: Some(10.0),
        affected_count: 1,
    };
    // Untagged usage was dismissed for the period; a different kind and
    // a different period are not covered by that dismissal.
    let dismissed: std::collections::HashSet<String> =
        ["untagged_usage:2026-09".to_string()].into_iter().collect();
    let rows = data_quality_rows(
        "2026-09",
        vec![
            issue(query::DataQualityKind::UntaggedUsage),
            issue(query::DataQualityKind::MissingRegion),
        ],
        &dismissed,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "missing_region:2026-09");

    // The same dismissal does not touch the next period.
    let rows = data_quality_rows(
        "2026-10",
        vec![issue(query::DataQualityKind::UntaggedUsage)],
        &dismissed,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].key, "untagged_usage:2026-10");
}

const TEMPLATE_CATEGORIES: [&str; 4] = [
    "Spend overview",
    "Forecast & trends",
    "Composition",
    "Data health",
];

#[test]
fn an_alert_trace_query_quotes_its_names() {
    let fired = at(NaiveDate::from_ymd_opt(2026, 10, 31).expect("a real date"));
    let sql = alert_trace_query("OpenAI", "it's; DROP TABLE t", fired);
    assert!(sql.contains("service_name = 'it''s; DROP TABLE t'"));
    assert!(sql.contains("DATE '2026-10-31'"));
}

#[test]
fn a_resource_trace_query_matches_literally() {
    let sql = resource_trace_query("AWS", "arn:aws:ec2:i_1%'", "i_1%'");
    assert!(sql.contains(r"resource_id = 'arn:aws:ec2:i_1%''' OR"));
    assert!(sql.contains(r"LIKE '%i\_1\%''' ESCAPE"));
}

#[test]
fn the_unallocated_query_is_a_real_template() {
    assert!(unallocated_query().is_some());
}

#[test]
fn templates_exist() {
    assert!(!query_templates().is_empty());
}

#[test]
fn templates_are_selects() {
    for template in query_templates() {
        let sql = template.sql.trim_start();
        assert!(!sql.is_empty(), "{}: SQL must not be empty", template.title);
        let starts_select = sql
            .get(..7)
            .is_some_and(|s| s.eq_ignore_ascii_case("select "));
        let starts_with = sql.get(..4).is_some_and(|s| s.eq_ignore_ascii_case("with"));
        assert!(
            starts_select || starts_with,
            "{}: SQL must start with SELECT or WITH",
            template.title
        );
    }
}

#[test]
fn template_categories_are_known() {
    for template in query_templates() {
        assert!(
            TEMPLATE_CATEGORIES.contains(&template.category),
            "{}: unknown category {:?}",
            template.title,
            template.category
        );
    }
}
