//! Read-only security configuration checks over the scanned inventory.
//!
//! A small, deliberate set: is a security group open to the whole internet on
//! a remote-administration port? This is no compliance check and no security
//! score, and it changes nothing in the cloud.
//!
//! The rule that shapes the rest: only a group whose inbound rules were
//! actually read can be said to have no finding. Anything else is
//! `NotEvaluated`, and never counts as a pass. The input is the
//! `SecurityGroupIngress` property of `AWS::EC2::SecurityGroup`, which is what
//! the pinned corkscrew fork stores from Cloud Control's `GetResource`.

use std::net::IpAddr;

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::model::{InventoryResource, InventoryScope};

/// What a check looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityRule {
    PublicSsh,
    PublicRdp,
}

impl SecurityRule {
    pub const ALL: [SecurityRule; 2] = [SecurityRule::PublicSsh, SecurityRule::PublicRdp];

    /// The stable identity of a rule, independent of the interface language.
    pub fn id(self) -> &'static str {
        match self {
            Self::PublicSsh => "aws-sg-public-ssh",
            Self::PublicRdp => "aws-sg-public-rdp",
        }
    }

    fn port(self) -> i64 {
        match self {
            Self::PublicSsh => 22,
            Self::PublicRdp => 3389,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecuritySeverity {
    High,
}

/// Why a group could not be judged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotEvaluatedReason {
    /// The scan found the group but returned no configuration for it.
    NoConfiguration,
    /// The configuration does not include inbound rules.
    NoIngressData,
    /// The configuration or a rule in it could not be read.
    Undecodable,
}

/// What a check concluded about one security group.
#[derive(Debug, Clone, PartialEq)]
pub enum SecurityOutcome {
    Finding {
        rule: SecurityRule,
        severity: SecuritySeverity,
        /// The inbound rules that trigger it, as the scan recorded them.
        evidence: Vec<String>,
    },
    /// Every inbound rule was read, and none opens either port publicly.
    NoFinding {
        rules_read: usize,
    },
    NotEvaluated(NotEvaluatedReason),
}

/// One row of the Security card.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityCheck {
    pub resource_id: String,
    pub name: Option<String>,
    pub provider: String,
    pub cloud_account_id: Option<String>,
    pub region: Option<String>,
    pub outcome: SecurityOutcome,
}

impl SecurityCheck {
    /// Findings, then what could not be judged, then what passed.
    fn rank(&self) -> u8 {
        match self.outcome {
            SecurityOutcome::Finding { .. } => 0,
            SecurityOutcome::NotEvaluated(_) => 1,
            SecurityOutcome::NoFinding { .. } => 2,
        }
    }
}

/// What the checks were able to look at.
#[derive(Debug, Clone, PartialEq)]
pub enum SecurityCoverage {
    /// No inventory, so nothing was evaluated.
    NotScanned,
    Scanned {
        evaluated_at: DateTime<Utc>,
        /// Every part of the scan was observed. If not, a group missing here
        /// may only be one the scan did not reach.
        complete: bool,
        regions: Vec<String>,
    },
}

/// Every check of an inventory, with its coverage.
#[derive(Debug, Clone, PartialEq)]
pub struct SecurityReport {
    pub coverage: SecurityCoverage,
    /// Findings first, in a stable order.
    pub checks: Vec<SecurityCheck>,
    /// Security groups the inventory lists.
    pub groups: usize,
    pub evaluated: usize,
    pub not_evaluated: usize,
}

impl SecurityReport {
    pub fn findings(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| matches!(check.outcome, SecurityOutcome::Finding { .. }))
            .count()
    }
}

/// Whether an inventory resource is an AWS security group, under either name
/// the scanner's two paths give it.
fn is_security_group(resource: &InventoryResource) -> bool {
    resource.provider == "AWS"
        && matches!(
            resource.resource_type.as_str(),
            "ec2:security-group" | "AWS::EC2::SecurityGroup"
        )
}

/// A CIDR block covering the whole internet. Parsed rather than compared to
/// text, because `::/0` and `0:0:0:0:0:0:0:0/0` are the same range.
fn is_public(cidr: &str) -> bool {
    cidr.split_once('/').is_some_and(|(address, prefix)| {
        prefix.trim() == "0" && address.trim().parse::<IpAddr>().is_ok()
    })
}

/// A port as Cloud Control gives it: a number, or the same as text.
fn port_of(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

/// One inbound rule. `None` when it cannot be read, which is different from a
/// rule that opens nothing.
struct Ingress {
    /// Whether it covers TCP on all ports.
    all_ports: bool,
    tcp_range: Option<(i64, i64)>,
    sources: Vec<String>,
    text: String,
}

fn ingress_of(rule: &Value) -> Option<Ingress> {
    let rule = rule.as_object()?;
    let protocol = match rule.get("IpProtocol")? {
        Value::String(text) => text.trim().to_ascii_lowercase(),
        Value::Number(number) => number.to_string(),
        _ => return None,
    };
    let from = rule.get("FromPort").map(port_of);
    let to = rule.get("ToPort").map(port_of);
    // A port present but unreadable says nothing we can rely on.
    if from == Some(None) || to == Some(None) {
        return None;
    }
    let (from, to) = (from.flatten(), to.flatten());
    let (all_ports, tcp_range) = match protocol.as_str() {
        "-1" | "all" => (true, None),
        "tcp" | "6" => match (from, to) {
            (Some(-1), _) | (_, Some(-1)) => (true, None),
            (Some(from), Some(to)) => (false, Some((from, to))),
            _ => return None,
        },
        // Another protocol does not carry SSH or RDP's TCP port.
        _ => (false, None),
    };
    let mut sources = Vec::new();
    for field in ["CidrIp", "CidrIpv6"] {
        match rule.get(field) {
            Some(Value::String(cidr)) => sources.push(cidr.trim().to_string()),
            Some(Value::Null) | None => {}
            Some(_) => return None,
        }
    }
    let ports = match (from, to) {
        (Some(from), Some(to)) if from == to => format!(" port {from}"),
        (Some(from), Some(to)) => format!(" ports {from}-{to}"),
        _ => String::new(),
    };
    let text = format!("{protocol}{ports} from {}", sources.join(", "));
    Some(Ingress {
        all_ports,
        tcp_range,
        sources,
        text,
    })
}

impl Ingress {
    fn opens_publicly(&self, port: i64) -> bool {
        let covers = self.all_ports
            || self
                .tcp_range
                .is_some_and(|(from, to)| from <= port && port <= to);
        covers && self.sources.iter().any(|source| is_public(source))
    }
}

/// The outcomes for one group. A finding stands even when another of its rules
/// could not be read; without a finding, an unreadable rule means not judged.
fn outcomes_of(properties: Option<&str>) -> Vec<SecurityOutcome> {
    let not_evaluated = |reason| vec![SecurityOutcome::NotEvaluated(reason)];
    let Some(properties) = properties else {
        return not_evaluated(NotEvaluatedReason::NoConfiguration);
    };
    let Ok(Value::Object(properties)) = serde_json::from_str::<Value>(properties) else {
        return not_evaluated(NotEvaluatedReason::Undecodable);
    };
    let Some(rules) = properties.get("SecurityGroupIngress") else {
        return not_evaluated(NotEvaluatedReason::NoIngressData);
    };
    let Some(rules) = rules.as_array() else {
        return not_evaluated(NotEvaluatedReason::Undecodable);
    };

    let parsed: Vec<Option<Ingress>> = rules.iter().map(ingress_of).collect();
    let readable: Vec<&Ingress> = parsed.iter().flatten().collect();
    let findings: Vec<SecurityOutcome> = SecurityRule::ALL
        .into_iter()
        .filter_map(|rule| {
            let evidence: Vec<String> = readable
                .iter()
                .filter(|ingress| ingress.opens_publicly(rule.port()))
                .map(|ingress| ingress.text.clone())
                .collect();
            (!evidence.is_empty()).then_some(SecurityOutcome::Finding {
                rule,
                severity: SecuritySeverity::High,
                evidence,
            })
        })
        .collect();
    if !findings.is_empty() {
        return findings;
    }
    if readable.len() < rules.len() {
        return not_evaluated(NotEvaluatedReason::Undecodable);
    }
    vec![SecurityOutcome::NoFinding {
        rules_read: rules.len(),
    }]
}

/// Check every security group of an inventory.
pub fn evaluate(resources: &[InventoryResource], scope: Option<&InventoryScope>) -> SecurityReport {
    let Some(scope) = scope else {
        return SecurityReport {
            coverage: SecurityCoverage::NotScanned,
            checks: Vec::new(),
            groups: 0,
            evaluated: 0,
            not_evaluated: 0,
        };
    };

    let mut checks = Vec::new();
    let mut groups = 0;
    let mut evaluated = 0;
    let mut not_evaluated = 0;
    for resource in resources.iter().filter(|r| is_security_group(r)) {
        groups += 1;
        let outcomes = outcomes_of(resource.properties.as_deref());
        if matches!(outcomes[0], SecurityOutcome::NotEvaluated(_)) {
            not_evaluated += 1;
        } else {
            evaluated += 1;
        }
        checks.extend(outcomes.into_iter().map(|outcome| SecurityCheck {
            resource_id: resource.resource_id.clone(),
            name: resource.name.clone(),
            provider: resource.provider.clone(),
            cloud_account_id: resource.cloud_account_id.clone(),
            region: resource.region.clone(),
            outcome,
        }));
    }
    checks.sort_by(|a, b| {
        a.rank()
            .cmp(&b.rank())
            .then_with(|| a.resource_id.cmp(&b.resource_id))
    });

    SecurityReport {
        coverage: SecurityCoverage::Scanned {
            evaluated_at: scope.scanned_at,
            complete: scope.complete,
            regions: scope.regions.clone(),
        },
        checks,
        groups,
        evaluated,
        not_evaluated,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 5, 0, 0, 0).unwrap()
    }

    fn scope(complete: bool) -> InventoryScope {
        InventoryScope {
            scan_id: "scan".to_string(),
            scanned_at: at(),
            imported_at: at(),
            source_path: "scan.duckdb".to_string(),
            regions: vec!["us-east-1".to_string()],
            resource_count: 1,
            scanner: None,
            complete,
        }
    }

    fn group(id: &str, properties: Option<&str>) -> InventoryResource {
        InventoryResource {
            provider: "AWS".to_string(),
            cloud_account_id: Some("123456789012".to_string()),
            resource_id: id.to_string(),
            arn: None,
            resource_type: "ec2:security-group".to_string(),
            region: Some("us-east-1".to_string()),
            name: None,
            tags: None,
            properties: properties.map(str::to_string),
            created_at: None,
            modified_at: None,
        }
    }

    fn outcomes(ingress: &str) -> Vec<SecurityOutcome> {
        outcomes_of(Some(&format!(r#"{{"SecurityGroupIngress":{ingress}}}"#)))
    }

    fn rule_of(outcome: &SecurityOutcome) -> Option<SecurityRule> {
        match outcome {
            SecurityOutcome::Finding { rule, .. } => Some(*rule),
            _ => None,
        }
    }

    #[test]
    fn public_ssh_is_a_finding_with_its_evidence() {
        let found =
            outcomes(r#"[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,"CidrIp":"0.0.0.0/0"}]"#);
        assert_eq!(
            found,
            vec![SecurityOutcome::Finding {
                rule: SecurityRule::PublicSsh,
                severity: SecuritySeverity::High,
                evidence: vec!["tcp port 22 from 0.0.0.0/0".to_string()],
            }]
        );
    }

    #[test]
    fn public_rdp_over_ipv6_is_a_finding() {
        let found =
            outcomes(r#"[{"IpProtocol":"tcp","FromPort":3389,"ToPort":3389,"CidrIpv6":"::/0"}]"#);
        assert_eq!(rule_of(&found[0]), Some(SecurityRule::PublicRdp));
    }

    #[test]
    fn a_range_covering_the_port_and_all_traffic_count() {
        let range =
            outcomes(r#"[{"IpProtocol":"tcp","FromPort":20,"ToPort":25,"CidrIp":"0.0.0.0/0"}]"#);
        assert_eq!(rule_of(&range[0]), Some(SecurityRule::PublicSsh));

        let all = outcomes(r#"[{"IpProtocol":"-1","CidrIp":"0.0.0.0/0"}]"#);
        assert_eq!(all.len(), 2, "both rules");
        assert_eq!(rule_of(&all[1]), Some(SecurityRule::PublicRdp));

        let spelled = outcomes(
            r#"[{"IpProtocol":"6","FromPort":"22","ToPort":"22","CidrIpv6":"0:0:0:0:0:0:0:0/0"}]"#,
        );
        assert_eq!(rule_of(&spelled[0]), Some(SecurityRule::PublicSsh));
    }

    #[test]
    fn restricted_ingress_does_not_trigger() {
        for ingress in [
            r#"[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,"CidrIp":"10.0.0.0/8"}]"#,
            r#"[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,"SourceSecurityGroupId":"sg-1"}]"#,
            r#"[{"IpProtocol":"tcp","FromPort":443,"ToPort":443,"CidrIp":"0.0.0.0/0"}]"#,
            r#"[{"IpProtocol":"udp","FromPort":22,"ToPort":22,"CidrIp":"0.0.0.0/0"}]"#,
        ] {
            assert_eq!(
                outcomes(ingress),
                vec![SecurityOutcome::NoFinding { rules_read: 1 }],
                "{ingress}"
            );
        }
    }

    #[test]
    fn an_empty_rule_list_was_read_and_opens_nothing() {
        assert_eq!(
            outcomes("[]"),
            vec![SecurityOutcome::NoFinding { rules_read: 0 }]
        );
    }

    #[test]
    fn missing_or_undecodable_configuration_is_not_evaluated_never_a_pass() {
        let reason = |properties: Option<&str>| outcomes_of(properties);
        assert_eq!(
            reason(None),
            vec![SecurityOutcome::NotEvaluated(
                NotEvaluatedReason::NoConfiguration
            )]
        );
        assert_eq!(
            reason(Some(r#"{"GroupName":"web"}"#)),
            vec![SecurityOutcome::NotEvaluated(
                NotEvaluatedReason::NoIngressData
            )]
        );
        for undecodable in [
            "not json",
            "[]",
            r#"{"SecurityGroupIngress":"open"}"#,
            r#"{"SecurityGroupIngress":["x"]}"#,
            r#"{"SecurityGroupIngress":[{"FromPort":22,"ToPort":22,"CidrIp":"0.0.0.0/0"}]}"#,
            r#"{"SecurityGroupIngress":[{"IpProtocol":"tcp","CidrIp":"0.0.0.0/0"}]}"#,
            r#"{"SecurityGroupIngress":[{"IpProtocol":"tcp","FromPort":"x","ToPort":22,"CidrIp":"0.0.0.0/0"}]}"#,
        ] {
            assert_eq!(
                reason(Some(undecodable)),
                vec![SecurityOutcome::NotEvaluated(
                    NotEvaluatedReason::Undecodable
                )],
                "{undecodable}"
            );
        }
    }

    #[test]
    fn one_unreadable_rule_keeps_a_group_from_passing_but_not_from_a_finding() {
        let unreadable = outcomes(
            r#"[{"IpProtocol":"tcp","FromPort":443,"ToPort":443,"CidrIp":"10.0.0.0/8"},"x"]"#,
        );
        assert!(matches!(unreadable[0], SecurityOutcome::NotEvaluated(_)));

        let found = outcomes(
            r#"[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,"CidrIp":"0.0.0.0/0"},"x"]"#,
        );
        assert_eq!(rule_of(&found[0]), Some(SecurityRule::PublicSsh));
    }

    #[test]
    fn no_inventory_is_not_scanned_rather_than_clean() {
        let report = evaluate(&[], None);
        assert_eq!(report.coverage, SecurityCoverage::NotScanned);
        assert_eq!(report.findings(), 0);
        assert_eq!(report.evaluated, 0);
    }

    #[test]
    fn a_partial_scan_says_so_and_counts_what_it_judged() {
        let open = r#"{"SecurityGroupIngress":[{"IpProtocol":"tcp","FromPort":22,"ToPort":22,"CidrIp":"0.0.0.0/0"}]}"#;
        let closed = r#"{"SecurityGroupIngress":[]}"#;
        let resources = [
            group("sg-closed", Some(closed)),
            group("sg-unknown", None),
            group("sg-open", Some(open)),
        ];
        let report = evaluate(&resources, Some(&scope(false)));
        assert_eq!(report.groups, 3);
        assert_eq!(report.evaluated, 2);
        assert_eq!(report.not_evaluated, 1);
        assert_eq!(report.findings(), 1);
        assert!(matches!(
            report.coverage,
            SecurityCoverage::Scanned {
                complete: false,
                ..
            }
        ));
        let order: Vec<&str> = report
            .checks
            .iter()
            .map(|c| c.resource_id.as_str())
            .collect();
        assert_eq!(order, vec!["sg-open", "sg-unknown", "sg-closed"]);
    }

    #[test]
    fn a_group_open_on_both_ports_counts_once_as_a_group() {
        let both = r#"{"SecurityGroupIngress":[{"IpProtocol":"-1","CidrIp":"0.0.0.0/0"}]}"#;
        let report = evaluate(&[group("sg", Some(both))], Some(&scope(true)));
        assert_eq!(report.groups, 1);
        assert_eq!(report.evaluated, 1);
        assert_eq!(report.findings(), 2);
    }

    #[test]
    fn only_aws_security_groups_are_checked() {
        let mut other = group("i-1", Some(r#"{"SecurityGroupIngress":[]}"#));
        other.resource_type = "ec2:instance".to_string();
        let mut cloudflare = group("sg", None);
        cloudflare.provider = "Cloudflare".to_string();
        let mut named = group("sg-cfn", Some(r#"{"SecurityGroupIngress":[]}"#));
        named.resource_type = "AWS::EC2::SecurityGroup".to_string();
        let report = evaluate(&[other, cloudflare, named], Some(&scope(true)));
        assert_eq!(report.groups, 1);
        assert_eq!(report.checks[0].resource_id, "sg-cfn");
    }

    #[test]
    fn checks_are_deterministic() {
        let resources = [group("sg-b", None), group("sg-a", None)];
        let first = evaluate(&resources, Some(&scope(true)));
        let second = evaluate(&resources, Some(&scope(true)));
        assert_eq!(first, second);
        assert_eq!(first.checks[0].resource_id, "sg-a");
    }
}
