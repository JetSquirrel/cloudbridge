//! The demo inventory: a handful of resources and the current period's
//! charges for them, shaped so the Insights page has one of each finding
//! to show — a stopped instance still paying for its volume, an idle public
//! address, an unclaimed bucket, a billed volume the scan no longer finds —
//! beside resources that are owned, running and cheap.
//!
//! The charges join the demo AWS account's current period, so they also
//! show on every page that reads it, as resource-level rows would.

use chrono::{DateTime, Duration, Utc};

use super::day_start;
use crate::model::{BillingPeriod, Charge, InventoryResource, InventoryScope};

/// The demo AWS account's provider-side id.
pub const DEMO_CLOUD_ACCOUNT: &str = "123456789012";
const REGION: &str = "us-east-1";

/// (resource id, scanner type, region, name, tags, properties).
type DemoResource = (
    &'static str,
    &'static str,
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<&'static str>,
);

const RESOURCES: &[DemoResource] = &[
    (
        "arn:aws:ec2:us-east-1:123456789012:instance/i-0d3m0build",
        "ec2:instance",
        REGION,
        Some("build-runner"),
        Some(r#"{"team":"platform","Name":"build-runner"}"#),
        Some(
            r#"{"InstanceType":"m5.xlarge","State":{"Code":80,"Name":"stopped"},"Volumes":[{"VolumeId":"vol-0d3m0build","Device":"/dev/sdf"}]}"#,
        ),
    ),
    (
        "arn:aws:ec2:us-east-1:123456789012:volume/vol-0d3m0build",
        "ec2:volume",
        REGION,
        None,
        Some(r#"{"team":"platform"}"#),
        None,
    ),
    (
        "arn:aws:ec2:us-east-1:123456789012:instance/i-0d3m0web",
        "ec2:instance",
        REGION,
        Some("web"),
        Some(r#"{"owner":"web","Name":"web"}"#),
        Some(r#"{"InstanceType":"t3.medium","State":{"Code":16,"Name":"running"}}"#),
    ),
    (
        "arn:aws:ec2:us-east-1:123456789012:elastic-ip/eipalloc-0d3m0idle",
        "ec2:elastic-ip",
        REGION,
        None,
        Some(r#"{"team":"platform"}"#),
        None,
    ),
    (
        "arn:aws:s3:::analytics-scratch-2024",
        "s3:bucket",
        "global",
        Some("analytics-scratch-2024"),
        None,
        None,
    ),
    (
        "arn:aws:s3:::site-assets",
        "s3:bucket",
        "global",
        Some("site-assets"),
        Some(r#"{"aws:cloudformation:stack-name":"site"}"#),
        None,
    ),
    (
        "arn:aws:ec2:us-east-1:123456789012:security-group/sg-0d3m0",
        "ec2:security-group",
        REGION,
        None,
        None,
        None,
    ),
];

/// (bill resource id, service, description, monthly USD).
const BILLED: &[(&str, &str, &str, f64)] = &[
    (
        "vol-0d3m0build",
        "Amazon Elastic Compute Cloud",
        "$0.08 per GB-month of General Purpose (gp3) provisioned storage",
        40.0,
    ),
    (
        "i-0d3m0web",
        "Amazon Elastic Compute Cloud",
        "$0.0416 per On Demand Linux t3.medium Instance Hour",
        30.0,
    ),
    (
        "arn:aws:ec2:us-east-1:123456789012:elastic-ip/eipalloc-0d3m0idle",
        "Amazon Virtual Private Cloud",
        "$0.005 per Idle public IPv4 address per hour",
        3.6,
    ),
    (
        "analytics-scratch-2024",
        "Amazon Simple Storage Service",
        "$0.023 per GB - first 50 TB / month of storage used",
        11.0,
    ),
    (
        "site-assets",
        "Amazon Simple Storage Service",
        "$0.023 per GB - first 50 TB / month of storage used",
        2.0,
    ),
    (
        // Deleted after the scan, or never scanned: still on the bill.
        "vol-0d3m0orphan",
        "Amazon Elastic Compute Cloud",
        "$0.08 per GB-month of General Purpose (gp3) provisioned storage",
        8.0,
    ),
];

/// The inventory as an import of a scan taken shortly before `now`.
pub fn inventory(now: DateTime<Utc>) -> (InventoryScope, Vec<InventoryResource>) {
    let resources: Vec<InventoryResource> = RESOURCES
        .iter()
        .map(
            |(id, kind, region, name, tags, properties)| InventoryResource {
                provider: "AWS".to_string(),
                cloud_account_id: Some(DEMO_CLOUD_ACCOUNT.to_string()),
                resource_id: id.to_string(),
                arn: id.starts_with("arn:").then(|| id.to_string()),
                resource_type: kind.to_string(),
                region: Some(region.to_string()),
                name: name.map(str::to_string),
                tags: tags.map(str::to_string),
                properties: properties.map(str::to_string),
            },
        )
        .collect();
    let scope = InventoryScope {
        scan_id: format!("{}scan", super::DEMO_PREFIX),
        scanned_at: now - Duration::hours(2),
        imported_at: now,
        source_path: "Demo data".to_string(),
        regions: vec![REGION.to_string()],
        resource_count: resources.len() as i64,
        scanner: Some("demo".to_string()),
    };
    (scope, resources)
}

/// Daily usage rows for the inventory's resources, from the first of the
/// current period through `now`'s day.
pub fn resource_charges(now: DateTime<Utc>) -> Vec<Charge> {
    let first = BillingPeriod::containing(now).start();
    let days = (now.date_naive() - first).num_days() + 1;
    let mut charges = Vec::new();
    for day in 0..days {
        let date = first + Duration::days(day);
        let start = day_start(date);
        for (id, service, description, monthly) in BILLED {
            let daily = monthly / 30.0;
            charges.push(Charge {
                service_name: Some(service.to_string()),
                charge_description: Some(description.to_string()),
                resource_id: Some(id.to_string()),
                region_id: Some(REGION.to_string()),
                sub_account_id: Some(DEMO_CLOUD_ACCOUNT.to_string()),
                billed_cost: Some(daily),
                effective_cost: Some(daily),
                ..Charge::new(start, start + Duration::days(1), "USD")
            });
        }
    }
    charges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analytics::insights::{insights, OWNER_TAG_KEYS};
    use crate::model::{InsightKind, ResourceCost};
    use chrono::TimeZone;

    /// The demo shows one of each finding: what the page is reviewed with.
    #[test]
    fn the_demo_inventory_shows_every_kind_of_finding() {
        let now = Utc.with_ymd_and_hms(2026, 9, 30, 12, 0, 0).unwrap();
        let (scope, resources) = inventory(now);
        let costs: Vec<ResourceCost> = BILLED
            .iter()
            .map(|(id, service, description, monthly)| ResourceCost {
                provider: "AWS".to_string(),
                cloud_account_id: Some(DEMO_CLOUD_ACCOUNT.to_string()),
                resource_id: id.to_string(),
                resource_name: None,
                service: service.to_string(),
                region: Some(REGION.to_string()),
                usage_cost: *monthly,
                idle_public_ip: description.contains("Idle public IPv4"),
            })
            .collect();

        let report = insights(&resources, &costs, Some(&scope), OWNER_TAG_KEYS);
        let kinds: Vec<InsightKind> = report.findings.iter().map(|f| f.kind).collect();
        assert_eq!(
            kinds,
            [
                InsightKind::StoppedInstance,
                InsightKind::IdlePublicIp,
                InsightKind::Unclaimed,
                InsightKind::NotInInventory,
            ]
        );
        assert_eq!(report.findings[0].cost, 40.0);
        assert_eq!(report.unclaimed_free, 1);

        let days = resource_charges(now)
            .iter()
            .map(|c| c.charge_period_start)
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert_eq!(days, 30);
    }
}
