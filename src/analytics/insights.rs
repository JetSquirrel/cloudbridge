//! Insights: resources worth a look, priced from the bill.
//!
//! The inputs are what the two backends read — the resource inventory an
//! import copied in from a corkscrew scan, the scan's scope, and each
//! resource's usage cost in a period — and the output is the same on both
//! targets. A bill row is matched to an inventory resource by its full id
//! or ARN first, and failing that by the resource's own id: an ARN on
//! either side is reduced to its last segment (`…:instance/i-0abc` →
//! `i-0abc`, `…:function:api` → `api`), which is how a bare Cloud Control
//! identifier and an ARN of the same resource meet. A key two resources
//! share (every Amplify branch called `main`) is settled by region, and
//! left unmatched rather than guessed when that does not settle it.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::model::{
    InsightFinding, InsightKind, InsightsReport, InventoryResource, InventoryScope, ResourceCost,
};

/// Tag keys that name who owns a resource, compared without regard to
/// case. `business_line` is the key the Attribution page groups by.
pub const OWNER_TAG_KEYS: &[&str] = &[
    "owner",
    "team",
    "project",
    "cost-center",
    "costcenter",
    "cost_center",
    "business_line",
];

/// The sources whose resources carry tags an owner can be read from. A
/// Cloudflare Worker or bucket has no such tags, so calling it unclaimed
/// would list every one of them; its resources are matched against the
/// bill, and judged by nothing else.
const TAGGED_SOURCES: &[&str] = &["AWS"];

/// The sources the findings are written for: stopped instances, idle
/// addresses and owner tags are AWS's notions. Another source's scan is
/// still matched against the bill, and is listed by
/// [`inventory_by_source`] so it does not vanish from the page.
pub const JUDGED_SOURCES: &[&str] = &["AWS"];

/// One source's resources, counted by type.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInventory {
    pub source: String,
    pub total: usize,
    /// Most numerous first.
    pub types: Vec<TypeCount>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeCount {
    pub resource_type: String,
    pub count: usize,
    /// Up to [`SAMPLE_NAMES`] of them, by name, to say which.
    pub names: Vec<String>,
    /// Every one of them, as indices into the resources the inventory was
    /// built from, in name order — what a drill-down lists.
    pub members: Vec<usize>,
}

/// One kind of finding's resources of one type: the summary row a list of
/// findings opens from.
#[derive(Debug, Clone, PartialEq)]
pub struct FindingGroup {
    pub resource_type: String,
    pub count: usize,
    pub cost: f64,
    /// Indices into the report's findings, costliest first.
    pub members: Vec<usize>,
}

/// The findings of `kind`, grouped by resource type: costliest group
/// first, then the largest. A page lists these, and a group's findings
/// only when it is opened — 400 unclaimed stacks are one row until then.
pub fn findings_by_type(findings: &[InsightFinding], kind: InsightKind) -> Vec<FindingGroup> {
    let mut by_type: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (i, finding) in findings.iter().enumerate() {
        if finding.kind == kind {
            by_type
                .entry(finding.resource_kind.as_str())
                .or_default()
                .push(i);
        }
    }
    let mut groups: Vec<FindingGroup> = by_type
        .into_iter()
        .map(|(resource_type, mut members)| {
            members.sort_by(|&a, &b| findings[b].cost.total_cmp(&findings[a].cost));
            FindingGroup {
                resource_type: resource_type.to_string(),
                count: members.len(),
                cost: members.iter().map(|&i| findings[i].cost).sum(),
                members,
            }
        })
        .collect();
    groups.sort_by(|a, b| {
        b.cost
            .total_cmp(&a.cost)
            .then(b.count.cmp(&a.count))
            .then_with(|| a.resource_type.cmp(&b.resource_type))
    });
    groups
}

/// How many names a type is illustrated with.
pub const SAMPLE_NAMES: usize = 3;

/// The inventory by source, then by type: what a scan found, whether or
/// not anything on it is a finding.
pub fn inventory_by_source(resources: &[InventoryResource]) -> Vec<SourceInventory> {
    let mut by_source: BTreeMap<&str, BTreeMap<&str, Vec<usize>>> = BTreeMap::new();
    for (i, resource) in resources.iter().enumerate() {
        by_source
            .entry(resource.provider.as_str())
            .or_default()
            .entry(resource.resource_type.as_str())
            .or_default()
            .push(i);
    }
    let name = |i: &usize| {
        resources[*i]
            .name
            .clone()
            .unwrap_or_else(|| resources[*i].resource_id.clone())
    };
    by_source
        .into_iter()
        .map(|(source, types)| {
            let mut types: Vec<TypeCount> = types
                .into_iter()
                .map(|(resource_type, mut members)| {
                    members.sort_by_key(|i| name(i));
                    TypeCount {
                        resource_type: resource_type.to_string(),
                        count: members.len(),
                        names: members.iter().take(SAMPLE_NAMES).map(&name).collect(),
                        members,
                    }
                })
                .collect();
            types.sort_by(|a, b| {
                b.count
                    .cmp(&a.count)
                    .then_with(|| a.resource_type.cmp(&b.resource_type))
            });
            SourceInventory {
                source: source.to_string(),
                total: types.iter().map(|t| t.count).sum(),
                types,
            }
        })
        .collect()
}

/// Tags a stack, app or cluster puts on what it manages: such a resource
/// has an owner even without an owner tag — whoever owns the stack.
const MANAGED_BY_TAG_KEYS: &[&str] = &[
    "aws:cloudformation:stack-name",
    "amplify:app-id",
    "alpha.eksctl.io/cluster-name",
    "eks:cluster-name",
    "elasticbeanstalk:environment-name",
    "aws:autoscaling:groupName",
];

/// The resource's own id: the last segment of an ARN's resource part, or
/// the whole of a bare id.
pub fn resource_key(id: &str) -> &str {
    let resource = if id.starts_with("arn:") {
        // arn:partition:service:region:account:resource
        id.splitn(6, ':').nth(5).unwrap_or(id)
    } else {
        id
    };
    resource
        .rsplit(['/', ':', '|'])
        .next()
        .filter(|segment| !segment.is_empty())
        .unwrap_or(resource)
}

/// The findings for one period. `owner_tag_keys` decides what counts as
/// claimed; [`OWNER_TAG_KEYS`] is the default.
pub fn insights(
    resources: &[InventoryResource],
    costs: &[ResourceCost],
    scope: Option<&InventoryScope>,
    owner_tag_keys: &[&str],
) -> InsightsReport {
    let index = Index::new(resources);
    let mut matched: Vec<Option<usize>> = Vec::with_capacity(costs.len());
    let mut cost_of = vec![0.0; resources.len()];
    for cost in costs {
        let hit = index.find(cost);
        if let Some(i) = hit {
            cost_of[i] += cost.usage_cost;
        }
        matched.push(hit);
    }
    let cost_by_key: HashMap<(&str, &str), f64> =
        costs.iter().fold(HashMap::new(), |mut map, c| {
            *map.entry((c.provider.as_str(), resource_key(&c.resource_id)))
                .or_insert(0.0) += c.usage_cost;
            map
        });

    // Without a resource-level bill nothing has a price, so "costs
    // nothing" cannot be told from "not priced": list them all.
    let priced = !costs.is_empty();
    let mut findings = Vec::new();
    let mut unclaimed_free = 0;
    for (i, resource) in resources.iter().enumerate() {
        let properties = resource.properties.as_deref().and_then(parse_object);

        if is_ec2_instance(&resource.resource_type)
            && properties
                .as_ref()
                .and_then(|p| p.pointer("/State/Name"))
                .and_then(Value::as_str)
                == Some("stopped")
        {
            let volumes = attached_volumes(properties.as_ref());
            let volume_cost: f64 = volumes
                .iter()
                .filter_map(|v| cost_by_key.get(&(resource.provider.as_str(), v.as_str())))
                .sum();
            findings.push(finding(
                InsightKind::StoppedInstance,
                resource,
                cost_of[i] + volume_cost,
                if volumes.is_empty() {
                    "Stopped when scanned; storage and addresses it holds still bill".to_string()
                } else {
                    format!(
                        "Stopped when scanned; its {} attached volume{} still bill",
                        volumes.len(),
                        if volumes.len() == 1 { "" } else { "s" }
                    )
                },
            ));
        }

        let tags = resource.tags.as_deref().and_then(parse_object);
        let tagged = TAGGED_SOURCES.contains(&resource.provider.as_str());
        if tagged && !is_claimed(tags.as_ref(), owner_tag_keys) {
            if cost_of[i] > 0.0 || !priced {
                findings.push(finding(
                    InsightKind::Unclaimed,
                    resource,
                    cost_of[i],
                    "No owner tag, and no stack or app that manages it".to_string(),
                ));
            } else {
                unclaimed_free += 1;
            }
        }
    }

    for (cost, hit) in costs.iter().zip(&matched) {
        if cost.idle_public_ip {
            findings.push(InsightFinding {
                kind: InsightKind::IdlePublicIp,
                provider: cost.provider.clone(),
                cloud_account_id: cost.cloud_account_id.clone(),
                resource_id: cost.resource_id.clone(),
                name: hit
                    .and_then(|i| resources[i].name.clone())
                    .or_else(|| cost.resource_name.clone()),
                resource_kind: cost.service.clone(),
                region: cost.region.clone(),
                cost: cost.usage_cost,
                evidence: "Billed as an idle public IPv4 address: allocated, attached to nothing"
                    .to_string(),
            });
        } else if hit.is_none() && cost.usage_cost > 0.0 {
            if let Some(scope) = scope.filter(|s| covers(s, cost.region.as_deref())) {
                findings.push(InsightFinding {
                    kind: InsightKind::NotInInventory,
                    provider: cost.provider.clone(),
                    cloud_account_id: cost.cloud_account_id.clone(),
                    resource_id: cost.resource_id.clone(),
                    name: cost.resource_name.clone(),
                    resource_kind: cost.service.clone(),
                    region: cost.region.clone(),
                    cost: cost.usage_cost,
                    evidence: format!(
                        "Billed this period, but not in the scan of {}",
                        scope.scanned_at.format("%Y-%m-%d")
                    ),
                });
            }
        }
    }

    findings.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then(b.cost.total_cmp(&a.cost))
            .then_with(|| a.resource_id.cmp(&b.resource_id))
    });

    let billed_cost = costs.iter().map(|c| c.usage_cost).sum();
    let matched_cost = costs
        .iter()
        .zip(&matched)
        .filter(|(_, hit)| hit.is_some())
        .map(|(c, _)| c.usage_cost)
        .sum();
    InsightsReport {
        findings,
        resources: resources.len(),
        described: resources.iter().filter(|r| r.properties.is_some()).count(),
        unclaimed_free,
        priced,
        matched_cost,
        billed_cost,
    }
}

/// The inventory looked up by resource key, the way bill rows look for it.
struct Index<'a> {
    resources: &'a [InventoryResource],
    /// Full id or ARN → resource; `None` for an id two resources share
    /// (a bare name in two regions), which only the key lookup can settle.
    exact: HashMap<(&'a str, &'a str), Option<usize>>,
    by_key: HashMap<(&'a str, &'a str), Vec<usize>>,
}

impl<'a> Index<'a> {
    fn new(resources: &'a [InventoryResource]) -> Self {
        let mut exact: HashMap<(&str, &str), Option<usize>> = HashMap::new();
        let mut by_key: HashMap<(&str, &str), Vec<usize>> = HashMap::new();
        for (i, resource) in resources.iter().enumerate() {
            let provider = resource.provider.as_str();
            let mut ids = vec![resource.resource_id.as_str()];
            if let Some(arn) = &resource.arn {
                ids.push(arn.as_str());
            }
            ids.dedup();
            for id in ids {
                exact
                    .entry((provider, id))
                    .and_modify(|slot| *slot = None)
                    .or_insert(Some(i));
            }
            let mut keys = vec![resource_key(&resource.resource_id)];
            if let Some(arn) = &resource.arn {
                keys.push(resource_key(arn));
            }
            keys.dedup();
            for key in keys {
                by_key
                    .entry((resource.provider.as_str(), key))
                    .or_default()
                    .push(i);
            }
        }
        Self {
            resources,
            exact,
            by_key,
        }
    }

    /// The one inventory resource `cost` is for, if it can be told.
    fn find(&self, cost: &ResourceCost) -> Option<usize> {
        if let Some(&Some(i)) = self
            .exact
            .get(&(cost.provider.as_str(), cost.resource_id.as_str()))
        {
            return Some(i);
        }
        let candidates = self
            .by_key
            .get(&(cost.provider.as_str(), resource_key(&cost.resource_id)))?;
        if let [only] = candidates.as_slice() {
            return Some(*only);
        }
        let mut same_region = candidates
            .iter()
            .filter(|&&i| self.resources[i].region.as_deref() == cost.region.as_deref());
        match (same_region.next(), same_region.next()) {
            (Some(&i), None) => Some(i),
            _ => None,
        }
    }
}

/// Whether the scan looked where a bill row says the resource is. A row
/// with no region, or a global one, could be anywhere, so is not counted
/// as missing.
fn covers(scope: &InventoryScope, region: Option<&str>) -> bool {
    match region {
        Some(region) if region != "global" => scope.regions.iter().any(|r| r == region),
        _ => false,
    }
}

fn is_ec2_instance(resource_type: &str) -> bool {
    resource_type.eq_ignore_ascii_case("ec2:instance")
        || resource_type.eq_ignore_ascii_case("AWS::EC2::Instance")
}

/// The volume ids an instance's configuration lists (`Volumes`, as Cloud
/// Control's `AWS::EC2::Instance` reports them).
fn attached_volumes(properties: Option<&Value>) -> Vec<String> {
    properties
        .and_then(|p| p.get("Volumes"))
        .and_then(Value::as_array)
        .map(|volumes| {
            volumes
                .iter()
                .filter_map(|v| v.get("VolumeId").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a resource has an owner: an owner tag with a value, or a stack
/// or app that manages it.
fn is_claimed(tags: Option<&Value>, owner_tag_keys: &[&str]) -> bool {
    let Some(tags) = tags.and_then(Value::as_object) else {
        return false;
    };
    tags.iter().any(|(key, value)| {
        let has_value = value.as_str().is_some_and(|v| !v.trim().is_empty());
        has_value
            && (owner_tag_keys.iter().any(|k| k.eq_ignore_ascii_case(key))
                || MANAGED_BY_TAG_KEYS
                    .iter()
                    .any(|k| k.eq_ignore_ascii_case(key)))
    })
}

fn parse_object(json: &str) -> Option<Value> {
    serde_json::from_str::<Value>(json)
        .ok()
        .filter(Value::is_object)
}

fn finding(
    kind: InsightKind,
    resource: &InventoryResource,
    cost: f64,
    evidence: String,
) -> InsightFinding {
    InsightFinding {
        kind,
        provider: resource.provider.clone(),
        cloud_account_id: resource.cloud_account_id.clone(),
        resource_id: resource
            .arn
            .clone()
            .unwrap_or_else(|| resource.resource_id.clone()),
        name: resource.name.clone(),
        resource_kind: resource.resource_type.clone(),
        region: resource.region.clone(),
        cost,
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn resource(id: &str, kind: &str, region: &str) -> InventoryResource {
        InventoryResource {
            provider: "AWS".to_string(),
            cloud_account_id: Some("123456789012".to_string()),
            resource_id: id.to_string(),
            arn: id.starts_with("arn:").then(|| id.to_string()),
            resource_type: kind.to_string(),
            region: Some(region.to_string()),
            name: None,
            tags: None,
            properties: None,
        }
    }

    fn cost(id: &str, region: &str, amount: f64) -> ResourceCost {
        ResourceCost {
            provider: "AWS".to_string(),
            cloud_account_id: Some("123456789012".to_string()),
            resource_id: id.to_string(),
            resource_name: None,
            service: "Amazon Elastic Compute Cloud".to_string(),
            region: Some(region.to_string()),
            usage_cost: amount,
            idle_public_ip: false,
        }
    }

    fn scope(regions: &[&str]) -> InventoryScope {
        let at = Utc.with_ymd_and_hms(2026, 9, 30, 18, 0, 0).unwrap();
        InventoryScope {
            scan_id: "scan".to_string(),
            scanned_at: at,
            imported_at: at,
            source_path: "corkscrew.duckdb".to_string(),
            regions: regions.iter().map(|r| r.to_string()).collect(),
            resource_count: 0,
            scanner: None,
        }
    }

    fn kinds(report: &InsightsReport) -> Vec<(InsightKind, &str, f64)> {
        report
            .findings
            .iter()
            .map(|f| (f.kind, resource_key(&f.resource_id), f.cost))
            .collect()
    }

    #[test]
    fn a_resource_key_is_the_ids_last_segment() {
        assert_eq!(
            resource_key("arn:aws:ec2:us-east-1:1:instance/i-0abc"),
            "i-0abc"
        );
        assert_eq!(
            resource_key("arn:aws:lambda:us-east-1:1:function:api"),
            "api"
        );
        assert_eq!(resource_key("arn:aws:s3:::my-bucket"), "my-bucket");
        assert_eq!(
            resource_key("arn:aws:kms:ap-east-1:1:key/0c72-uuid"),
            "0c72-uuid"
        );
        assert_eq!(resource_key("i-0abc"), "i-0abc");
        assert_eq!(resource_key("vpc-1|subnet-2"), "subnet-2");
    }

    /// The bill names an EC2 instance bare and a Lambda function by ARN;
    /// the inventory has the instance by ARN and the function by name.
    #[test]
    fn bill_rows_meet_inventory_resources_by_their_own_id() {
        let resources = [
            resource(
                "arn:aws:ec2:us-east-1:1:instance/i-0abc",
                "ec2:instance",
                "us-east-1",
            ),
            resource("api", "AWS::Lambda::Function", "us-east-1"),
        ];
        let costs = [
            cost("i-0abc", "us-east-1", 3.0),
            cost("arn:aws:lambda:us-east-1:1:function:api", "us-east-1", 1.0),
            cost("vol-elsewhere", "eu-west-1", 2.0),
        ];
        let report = insights(&resources, &costs, None, OWNER_TAG_KEYS);
        assert_eq!(report.matched_cost, 4.0);
        assert_eq!(report.billed_cost, 6.0);
    }

    /// Two resources under one key are told apart by region, and a cost
    /// that region cannot settle is left unmatched rather than guessed.
    #[test]
    fn an_ambiguous_key_is_settled_by_region_or_not_at_all() {
        let resources = [
            resource("main", "amplify:apps/branches", "ap-east-1"),
            resource("main", "amplify:apps/branches", "us-east-1"),
        ];
        let report = insights(
            &resources,
            &[
                cost("main", "us-east-1", 1.0),
                cost("main", "eu-west-1", 5.0),
            ],
            None,
            OWNER_TAG_KEYS,
        );
        assert_eq!(report.matched_cost, 1.0);

        // A full ARN needs no key at all, however many branches are `main`.
        let branch = |app: &str| {
            resource(
                &format!("arn:aws:amplify:ap-east-1:1:apps/{app}/branches/main"),
                "amplify:apps/branches",
                "ap-east-1",
            )
        };
        let report = insights(
            &[branch("a"), branch("b")],
            &[cost(
                "arn:aws:amplify:ap-east-1:1:apps/b/branches/main",
                "ap-east-1",
                2.0,
            )],
            None,
            OWNER_TAG_KEYS,
        );
        assert_eq!(report.matched_cost, 2.0);
    }

    #[test]
    fn a_stopped_instance_carries_the_cost_of_its_volumes() {
        let mut instance = resource(
            "arn:aws:ec2:us-east-1:1:instance/i-0abc",
            "ec2:instance",
            "us-east-1",
        );
        instance.tags = Some(r#"{"owner":"platform"}"#.to_string());
        instance.properties = Some(
            r#"{"State":{"Code":80,"Name":"stopped"},"Volumes":[{"VolumeId":"vol-1","Device":"/dev/sdf"}]}"#
                .to_string(),
        );
        let mut running = resource(
            "arn:aws:ec2:us-east-1:1:instance/i-0run",
            "ec2:instance",
            "us-east-1",
        );
        running.tags = Some(r#"{"owner":"platform"}"#.to_string());
        running.properties = Some(r#"{"State":{"Name":"running"}}"#.to_string());

        let report = insights(
            &[instance, running],
            &[cost("vol-1", "us-east-1", 8.0)],
            None,
            OWNER_TAG_KEYS,
        );
        assert_eq!(
            kinds(&report),
            [(InsightKind::StoppedInstance, "i-0abc", 8.0)]
        );
        assert!(report.findings[0].evidence.contains("1 attached volume "));
        assert_eq!(report.described, 2);
    }

    /// An owner tag, or a stack that manages the resource, claims it; a
    /// free unclaimed resource is counted, not listed.
    #[test]
    fn unclaimed_resources_are_those_with_no_owner_and_a_cost() {
        let mut owned = resource("arn:aws:s3:::owned", "s3:bucket", "global");
        owned.tags = Some(r#"{"Owner":"data"}"#.to_string());
        let mut stacked = resource("arn:aws:s3:::stacked", "s3:bucket", "global");
        stacked.tags = Some(r#"{"aws:cloudformation:stack-name":"site"}"#.to_string());
        let mut blank = resource("arn:aws:s3:::blank-owner", "s3:bucket", "global");
        blank.tags = Some(r#"{"owner":" "}"#.to_string());
        let free = resource("arn:aws:s3:::free", "s3:bucket", "global");

        let report = insights(
            &[owned, stacked, blank, free],
            &[
                cost("owned", "ap-east-1", 1.0),
                cost("stacked", "ap-east-1", 1.0),
                cost("blank-owner", "ap-east-1", 2.0),
            ],
            None,
            OWNER_TAG_KEYS,
        );
        assert_eq!(
            kinds(&report),
            [(InsightKind::Unclaimed, "blank-owner", 2.0)]
        );
        assert_eq!(report.unclaimed_free, 1);
    }

    /// A bill that names no resources prices nothing: every unclaimed
    /// resource is listed, since none can be said to cost nothing.
    #[test]
    fn without_a_resource_level_bill_every_unclaimed_resource_is_listed() {
        let report = insights(
            &[
                resource("arn:aws:s3:::a", "s3:bucket", "global"),
                resource("arn:aws:s3:::b", "s3:bucket", "global"),
            ],
            &[],
            None,
            OWNER_TAG_KEYS,
        );
        assert!(!report.priced);
        assert_eq!(report.findings.len(), 2);
        assert_eq!(report.unclaimed_free, 0);
    }

    #[test]
    fn the_inventory_is_counted_by_source_and_type() {
        let cloudflare = |id: &str, kind: &str, name: &str| {
            let mut r = resource(id, kind, "global");
            r.provider = "Cloudflare".to_string();
            r.name = Some(name.to_string());
            r
        };
        let inventory = inventory_by_source(&[
            cloudflare("w1", "worker_script", "ticker"),
            cloudflare("w2", "worker_script", "api"),
            cloudflare("w3", "worker_script", "cron"),
            cloudflare("w4", "worker_script", "auth"),
            cloudflare("b1", "r2_bucket", "assets"),
            resource("arn:aws:s3:::logs", "s3:bucket", "global"),
        ]);
        assert_eq!(
            inventory
                .iter()
                .map(|s| (s.source.as_str(), s.total))
                .collect::<Vec<_>>(),
            [("AWS", 1), ("Cloudflare", 5)]
        );
        let workers = &inventory[1].types[0];
        assert_eq!(workers.resource_type, "worker_script");
        assert_eq!(workers.count, 4);
        assert_eq!(workers.names, ["api", "auth", "cron"]);
        assert_eq!(workers.members, [1, 3, 2, 0], "every worker, by name");
        // A resource with no name is named by its id.
        assert_eq!(inventory[0].types[0].names, ["arn:aws:s3:::logs"]);
    }

    #[test]
    fn findings_are_grouped_by_type_costliest_first() {
        let unclaimed = |id: &str, kind: &str, cost: f64| InsightFinding {
            kind: InsightKind::Unclaimed,
            provider: "AWS".to_string(),
            cloud_account_id: None,
            resource_id: id.to_string(),
            name: None,
            resource_kind: kind.to_string(),
            region: None,
            cost,
            evidence: String::new(),
        };
        let findings = [
            unclaimed("s1", "cloudformation:stack", 0.0),
            unclaimed("s2", "cloudformation:stack", 0.0),
            unclaimed("t1", "dynamodb:table", 3.0),
            unclaimed("t2", "dynamodb:table", 5.0),
            InsightFinding {
                kind: InsightKind::IdlePublicIp,
                ..unclaimed("ip", "ec2:elastic-ip", 9.0)
            },
        ];
        let groups = findings_by_type(&findings, InsightKind::Unclaimed);
        let summary: Vec<_> = groups
            .iter()
            .map(|g| (g.resource_type.as_str(), g.count, g.cost))
            .collect();
        assert_eq!(
            summary,
            [("dynamodb:table", 2, 8.0), ("cloudformation:stack", 2, 0.0)]
        );
        assert_eq!(groups[0].members, [3, 2], "costliest table first");
    }

    /// A Cloudflare resource has no owner tags to miss, so it is never
    /// unclaimed — but a billed zone still counts as matched.
    #[test]
    fn an_untagged_source_has_nothing_unclaimed() {
        let mut zone = resource("9a7806061c88ada191ed06f989cc3dac", "zone", "global");
        zone.provider = "Cloudflare".to_string();
        let mut worker = resource("ticker", "worker_script", "global");
        worker.provider = "Cloudflare".to_string();
        let mut zone_cost = cost("9a7806061c88ada191ed06f989cc3dac", "global", 0.75);
        zone_cost.provider = "Cloudflare".to_string();
        zone_cost.region = None;

        let report = insights(&[zone, worker], &[zone_cost], None, OWNER_TAG_KEYS);
        assert!(report.findings.is_empty(), "{:?}", kinds(&report));
        assert_eq!(report.unclaimed_free, 0);
        assert_eq!(report.matched_cost, 0.75);
    }

    #[test]
    fn an_idle_public_address_is_found_in_the_bill_alone() {
        let mut idle = cost(
            "arn:aws:ec2:us-east-1:1:elastic-ip/eipalloc-1",
            "us-east-1",
            3.6,
        );
        idle.idle_public_ip = true;
        let report = insights(&[], &[idle], None, OWNER_TAG_KEYS);
        assert_eq!(
            kinds(&report),
            [(InsightKind::IdlePublicIp, "eipalloc-1", 3.6)]
        );
    }

    /// Only a region the scan covered can be missing a resource; without a
    /// scan, nothing is.
    #[test]
    fn a_billed_resource_is_missing_only_where_the_scan_looked() {
        let costs = [
            cost("vol-gone", "us-east-1", 2.0),
            cost("vol-unscanned", "eu-west-1", 2.0),
            cost("dist-1", "global", 2.0),
            cost("vol-free", "us-east-1", 0.0),
        ];
        let report = insights(&[], &costs, Some(&scope(&["us-east-1"])), OWNER_TAG_KEYS);
        assert_eq!(
            kinds(&report),
            [(InsightKind::NotInInventory, "vol-gone", 2.0)]
        );
        assert!(report.findings[0].evidence.contains("2026-09-30"));

        assert!(insights(&[], &costs, None, OWNER_TAG_KEYS)
            .findings
            .is_empty());
    }

    #[test]
    fn findings_run_kind_by_kind_costliest_first() {
        let mut a = cost("a", "us-east-1", 1.0);
        a.idle_public_ip = true;
        let mut b = cost("b", "us-east-1", 5.0);
        b.idle_public_ip = true;
        let report = insights(
            &[],
            &[a, b, cost("c", "us-east-1", 9.0)],
            Some(&scope(&["us-east-1"])),
            OWNER_TAG_KEYS,
        );
        assert_eq!(
            kinds(&report),
            [
                (InsightKind::IdlePublicIp, "b", 5.0),
                (InsightKind::IdlePublicIp, "a", 1.0),
                (InsightKind::NotInInventory, "c", 9.0),
            ]
        );
    }
}
