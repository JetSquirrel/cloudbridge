//! Insights rules written in JavaScript: the findings a release does not
//! have to ship.
//!
//! The built-in findings are Rust (`analytics::insights`) and run on both
//! targets. On the desktop, each `.js` file in the data directory's
//! `insights/` folder is one more rule, read again every time the page
//! loads, so editing a file and reopening Insights is the whole cycle.
//! Rules are ES modules; TypeScript compiles to one with
//! `esbuild rule.ts --format=esm --outfile=rule.js`, and
//! `examples/insights/cloudbridge-insights.d.ts` types the input.
//!
//! A rule exports its names and a function:
//!
//! ```js
//! export const rule = { title: "Idle load balancers", explanation: "..." };
//! export function evaluate(input) {
//!   return input.billed
//!     .filter((r) => /* ... */)
//!     .map((r) => ({ id: r.id, evidence: "..." }));
//! }
//! ```
//!
//! `input` is the period's bill per resource, each with its charge lines
//! and the inventory resource it was matched to, and the inventory itself.
//! A finding names a resource by `id` (and `provider`, if not the billed
//! row's); what it leaves out is filled in from the bill or the inventory.
//!
//! Each rule runs in a QuickJS runtime of its own, with no module loader,
//! no file or network access, a memory cap and a time limit: a rule sees
//! only the JSON it is handed, and a broken one is reported on the page
//! rather than taking the others with it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use rquickjs::{CatchResultExt as _, Context, Function, Module, Object, Runtime, Value};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::model::{
    InsightFinding, InsightKind, InsightRuleInfo, InsightsReport, InventoryResource,
    InventoryScope, ResourceChargeLine, ResourceCost, ScriptError, ScriptRun,
};

/// The version of the input and findings rules are written against. A rule
/// that declares a newer `apiVersion` is not run: it may read fields this
/// build does not hand it. Bumped only for a change an existing rule could
/// trip over; adding a field is not one.
pub const API_VERSION: u32 = 1;

/// A rule gets this long to load and evaluate.
const TIME_LIMIT: Duration = Duration::from_secs(2);
/// And this much heap.
const MEMORY_LIMIT: usize = 64 * 1024 * 1024;

/// The folder rules are read from, under the app's data directory.
pub fn rules_dir() -> Result<PathBuf> {
    Ok(crate::config::get_app_data_dir()?.join("insights"))
}

/// Run every rule in [`rules_dir`] over the period. Never fails: what
/// cannot be read or run is an entry in [`ScriptRun::errors`].
pub fn run(
    period: &str,
    currency: &str,
    resources: &[InventoryResource],
    costs: &[ResourceCost],
    scope: Option<&InventoryScope>,
    report: &InsightsReport,
) -> ScriptRun {
    let dir = match rules_dir() {
        Ok(dir) => dir,
        Err(e) => {
            return ScriptRun {
                errors: vec![ScriptError {
                    file: "insights/".to_string(),
                    message: format!("{e:#}"),
                }],
                ..ScriptRun::default()
            }
        }
    };
    let mut run = ScriptRun {
        dir: Some(dir.display().to_string()),
        ..ScriptRun::default()
    };
    let sources = match read_sources(&dir) {
        Ok(sources) => sources,
        Err(e) => {
            run.errors.push(ScriptError {
                file: "insights/".to_string(),
                message: format!("{e:#}"),
            });
            return run;
        }
    };
    if sources.is_empty() {
        return run;
    }
    let lines = match crate::ledger::query::resource_charge_lines(period) {
        Ok(lines) => lines,
        Err(e) => {
            run.errors.push(ScriptError {
                file: "insights/".to_string(),
                message: format!("Could not read the bill's charge lines: {e:#}"),
            });
            return run;
        }
    };
    let input = build_input(period, currency, resources, costs, &lines, scope, report);
    let ran = run_rules(&sources, &input);
    run.rules = ran.rules;
    run.findings = ran.findings;
    run.errors.extend(ran.errors);
    run
}

/// The `.js` files in `dir`, by name, as (file name, source). A missing
/// folder has none.
fn read_sources(dir: &Path) -> Result<Vec<(String, String)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut sources = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("Reading {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("js") || !path.is_file() {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let source = std::fs::read_to_string(&path).with_context(|| format!("Reading {name}"))?;
        sources.push((name, source));
    }
    sources.sort();
    Ok(sources)
}

// ==================== What a rule is handed ====================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptInput {
    pub api_version: u32,
    pub period: String,
    pub currency: String,
    pub scan: Option<ScanInput>,
    pub resources: Vec<ResourceInput>,
    pub billed: Vec<BilledInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanInput {
    pub scanned_at: String,
    pub regions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceInput {
    pub provider: String,
    pub account_id: Option<String>,
    pub id: String,
    pub arn: Option<String>,
    #[serde(rename = "type")]
    pub resource_type: String,
    pub region: Option<String>,
    pub name: Option<String>,
    /// `{}` when the scan read none.
    pub tags: Json,
    /// `null` when the scan did not read the configuration.
    pub properties: Json,
    /// This period's usage cost matched to it from the bill.
    pub cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BilledInput {
    pub provider: String,
    pub account_id: Option<String>,
    pub id: String,
    pub name: Option<String>,
    pub service: String,
    pub region: Option<String>,
    pub cost: f64,
    /// Index into `resources` of the inventory resource this is, if known.
    pub resource: Option<usize>,
    pub lines: Vec<LineInput>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LineInput {
    pub description: Option<String>,
    pub unit: Option<String>,
    pub quantity: f64,
    pub cost: f64,
}

pub fn build_input(
    period: &str,
    currency: &str,
    resources: &[InventoryResource],
    costs: &[ResourceCost],
    lines: &[ResourceChargeLine],
    scope: Option<&InventoryScope>,
    report: &InsightsReport,
) -> ScriptInput {
    let matched = crate::analytics::insights::match_costs(resources, costs);
    let mut lines_of: std::collections::HashMap<(&str, &str), Vec<LineInput>> =
        std::collections::HashMap::new();
    for line in lines {
        lines_of
            .entry((line.provider.as_str(), line.resource_id.as_str()))
            .or_default()
            .push(LineInput {
                description: line.description.clone(),
                unit: line.unit.clone(),
                quantity: line.quantity,
                cost: line.cost,
            });
    }
    let object = |json: Option<&str>, empty: Json| {
        json.and_then(|j| serde_json::from_str::<Json>(j).ok())
            .unwrap_or(empty)
    };
    ScriptInput {
        api_version: API_VERSION,
        period: period.to_string(),
        currency: currency.to_string(),
        scan: scope.map(|s| ScanInput {
            scanned_at: s.scanned_at.to_rfc3339(),
            regions: s.regions.clone(),
        }),
        resources: resources
            .iter()
            .enumerate()
            .map(|(i, r)| ResourceInput {
                provider: r.provider.clone(),
                account_id: r.cloud_account_id.clone(),
                id: r.resource_id.clone(),
                arn: r.arn.clone(),
                resource_type: r.resource_type.clone(),
                region: r.region.clone(),
                name: r.name.clone(),
                tags: object(r.tags.as_deref(), Json::Object(Default::default())),
                properties: object(r.properties.as_deref(), Json::Null),
                cost: report.resource_cost.get(i).copied().unwrap_or(0.0),
            })
            .collect(),
        billed: costs
            .iter()
            .zip(matched)
            .map(|(c, hit)| BilledInput {
                provider: c.provider.clone(),
                account_id: c.cloud_account_id.clone(),
                id: c.resource_id.clone(),
                name: c.resource_name.clone(),
                service: c.service.clone(),
                region: c.region.clone(),
                cost: c.usage_cost,
                resource: hit,
                lines: lines_of
                    .remove(&(c.provider.as_str(), c.resource_id.as_str()))
                    .unwrap_or_default(),
            })
            .collect(),
    }
}

// ==================== Running them ====================

/// The `rule` export.
#[derive(Debug, Default, Deserialize)]
struct RuleExport {
    title: Option<String>,
    label: Option<String>,
    explanation: Option<String>,
    #[serde(rename = "apiVersion")]
    api_version: Option<u32>,
}

/// One element of what `evaluate` returns.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FindingExport {
    id: String,
    provider: Option<String>,
    evidence: String,
    cost: Option<f64>,
    name: Option<String>,
    kind: Option<String>,
    region: Option<String>,
    account_id: Option<String>,
}

/// Run `sources` (file name, ES module source) over `input`.
pub fn run_rules(sources: &[(String, String)], input: &ScriptInput) -> ScriptRun {
    let mut run = ScriptRun::default();
    let input_json = match serde_json::to_string(input) {
        Ok(json) => json,
        Err(e) => {
            run.errors.push(ScriptError {
                file: "insights/".to_string(),
                message: format!("Could not serialize the input: {e}"),
            });
            return run;
        }
    };
    for (file, source) in sources {
        if run.rules.len() >= u16::MAX as usize {
            break;
        }
        match run_rule(file, source, &input_json) {
            Ok((info, findings)) => {
                let kind = InsightKind::Script(run.rules.len() as u16);
                run.findings
                    .extend(findings.into_iter().map(|f| complete(f, kind, input)));
                run.rules.push(info);
            }
            Err(e) => run.errors.push(ScriptError {
                file: file.clone(),
                message: format!("{e:#}"),
            }),
        }
    }
    run
}

fn run_rule(
    file: &str,
    source: &str,
    input_json: &str,
) -> Result<(InsightRuleInfo, Vec<FindingExport>)> {
    let id = file.strip_suffix(".js").unwrap_or(file).to_string();
    let runtime = Runtime::new()?;
    runtime.set_memory_limit(MEMORY_LIMIT);
    let deadline = Instant::now() + TIME_LIMIT;
    runtime.set_interrupt_handler(Some(Box::new(move || Instant::now() > deadline)));
    let context = Context::full(&runtime)?;

    let (rule, output) = context.with(|ctx| -> Result<(RuleExport, String)> {
        let js = |e: rquickjs::CaughtError| anyhow!("{e}");
        let (module, promise) = Module::declare(ctx.clone(), file, source)
            .catch(&ctx)
            .map_err(js)?
            .eval()
            .catch(&ctx)
            .map_err(js)?;
        promise.finish::<()>().catch(&ctx).map_err(js)?;
        let exports: Object = module.namespace().catch(&ctx).map_err(js)?;

        let rule: Value = exports.get("rule").catch(&ctx).map_err(js)?;
        let rule = if rule.is_undefined() {
            RuleExport::default()
        } else {
            let json = ctx
                .json_stringify(rule)
                .catch(&ctx)
                .map_err(js)?
                .map(|s| s.to_string())
                .transpose()?
                .unwrap_or_default();
            serde_json::from_str(&json).context("`rule` is not { title, label, explanation }")?
        };

        let evaluate: Value = exports.get("evaluate").catch(&ctx).map_err(js)?;
        let Some(evaluate) = evaluate.into_function() else {
            return Err(anyhow!("exports no `evaluate` function"));
        };
        let evaluate: Function = evaluate;
        let arg = ctx.json_parse(input_json).catch(&ctx).map_err(js)?;
        let mut result: Value = evaluate.call((arg,)).catch(&ctx).map_err(js)?;
        if let Some(promise) = result.clone().into_promise() {
            result = promise.finish::<Value>().catch(&ctx).map_err(js)?;
        }
        let json = ctx
            .json_stringify(result)
            .catch(&ctx)
            .map_err(js)?
            .map(|s| s.to_string())
            .transpose()?
            .unwrap_or_else(|| "null".to_string());
        Ok((rule, json))
    })?;

    if let Some(wanted) = rule.api_version.filter(|v| *v > API_VERSION) {
        return Err(anyhow!(
            "written for rule API {wanted}; this CloudBridge has {API_VERSION}. Update the app"
        ));
    }
    let findings: Option<Vec<FindingExport>> = serde_json::from_str(&output)
        .context("`evaluate` must return an array of { id, evidence, cost? }")?;
    let title = rule.title.unwrap_or_else(|| id.clone());
    Ok((
        InsightRuleInfo {
            label: rule.label.unwrap_or_else(|| title.to_uppercase()),
            explanation: rule.explanation.unwrap_or_default(),
            title,
            id,
        },
        findings.unwrap_or_default(),
    ))
}

/// A rule's finding with what it left out filled in from the bill row or
/// inventory resource it names.
fn complete(f: FindingExport, kind: InsightKind, input: &ScriptInput) -> InsightFinding {
    let provider_matches = |p: &str| f.provider.as_deref().is_none_or(|want| want == p);
    let billed = input
        .billed
        .iter()
        .find(|b| b.id == f.id && provider_matches(&b.provider));
    let resource = billed
        .and_then(|b| b.resource)
        .and_then(|i| input.resources.get(i))
        .or_else(|| {
            input.resources.iter().find(|r| {
                (r.id == f.id || r.arn.as_deref() == Some(&f.id)) && provider_matches(&r.provider)
            })
        });
    InsightFinding {
        kind,
        provider: f
            .provider
            .or_else(|| billed.map(|b| b.provider.clone()))
            .or_else(|| resource.map(|r| r.provider.clone()))
            .unwrap_or_default(),
        cloud_account_id: f
            .account_id
            .or_else(|| billed.and_then(|b| b.account_id.clone()))
            .or_else(|| resource.and_then(|r| r.account_id.clone())),
        name: f
            .name
            .or_else(|| resource.and_then(|r| r.name.clone()))
            .or_else(|| billed.and_then(|b| b.name.clone())),
        resource_kind: f
            .kind
            .or_else(|| resource.map(|r| r.resource_type.clone()))
            .or_else(|| billed.map(|b| b.service.clone()))
            .unwrap_or_default(),
        region: f
            .region
            .or_else(|| billed.and_then(|b| b.region.clone()))
            .or_else(|| resource.and_then(|r| r.region.clone())),
        cost: f
            .cost
            .filter(|c| c.is_finite())
            .or_else(|| billed.map(|b| b.cost))
            .or_else(|| resource.map(|r| r.cost))
            .unwrap_or(0.0),
        resource_id: f.id,
        evidence: f.evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input() -> ScriptInput {
        let line = |description: &str, unit: &str, quantity: f64, cost: f64| LineInput {
            description: Some(description.to_string()),
            unit: Some(unit.to_string()),
            quantity,
            cost,
        };
        ScriptInput {
            api_version: API_VERSION,
            period: "2026-10".to_string(),
            currency: "USD".to_string(),
            scan: None,
            resources: vec![ResourceInput {
                provider: "AWS".to_string(),
                account_id: Some("1".to_string()),
                id: "arn:aws:elasticloadbalancing:us-east-1:1:loadbalancer/app/old/abc".to_string(),
                arn: None,
                resource_type: "elasticloadbalancing:loadbalancer".to_string(),
                region: Some("us-east-1".to_string()),
                name: Some("old-api".to_string()),
                tags: Json::Object(Default::default()),
                properties: Json::Null,
                cost: 16.2,
            }],
            billed: vec![
                BilledInput {
                    provider: "AWS".to_string(),
                    account_id: Some("1".to_string()),
                    id: "arn:aws:elasticloadbalancing:us-east-1:1:loadbalancer/app/old/abc"
                        .to_string(),
                    name: None,
                    service: "Elastic Load Balancing".to_string(),
                    region: Some("us-east-1".to_string()),
                    cost: 16.2,
                    resource: Some(0),
                    lines: vec![line(
                        "$0.0225 per Application LoadBalancer-hour (or partial hour)",
                        "Hrs",
                        720.0,
                        16.2,
                    )],
                },
                BilledInput {
                    provider: "AWS".to_string(),
                    account_id: Some("1".to_string()),
                    id: "arn:aws:elasticloadbalancing:us-east-1:1:loadbalancer/app/busy/def"
                        .to_string(),
                    name: None,
                    service: "Elastic Load Balancing".to_string(),
                    region: Some("us-east-1".to_string()),
                    cost: 40.0,
                    resource: None,
                    lines: vec![
                        line(
                            "$0.0225 per Application LoadBalancer-hour (or partial hour)",
                            "Hrs",
                            720.0,
                            16.2,
                        ),
                        line(
                            "$0.008 per used Application load balancer capacity unit-hour",
                            "LCU-Hrs",
                            2975.0,
                            23.8,
                        ),
                    ],
                },
            ],
        }
    }

    const IDLE_ALB: &str = r#"
        export const rule = {
          title: "Idle load balancers",
          explanation: "Billed by the hour, used by no one.",
        };
        export function evaluate(input) {
          return input.billed
            .filter((r) => r.lines.some((l) => /LoadBalancer-hour/.test(l.description ?? "")))
            .filter((r) => !r.lines.some((l) => /capacity unit/.test(l.description ?? "") && l.quantity > 0))
            .map((r) => ({ id: r.id, evidence: `${r.lines[0].quantity} hours, no capacity units` }));
        }
    "#;

    #[test]
    fn a_rule_finds_and_the_rest_is_filled_in() {
        let run = run_rules(
            &[("idle-alb.js".to_string(), IDLE_ALB.to_string())],
            &input(),
        );
        assert_eq!(run.errors, []);
        assert_eq!(
            run.rules,
            [InsightRuleInfo {
                id: "idle-alb".to_string(),
                label: "IDLE LOAD BALANCERS".to_string(),
                title: "Idle load balancers".to_string(),
                explanation: "Billed by the hour, used by no one.".to_string(),
            }]
        );
        let [finding] = run.findings.as_slice() else {
            panic!("{:?}", run.findings)
        };
        assert_eq!(finding.kind, InsightKind::Script(0));
        assert_eq!(finding.provider, "AWS");
        assert_eq!(finding.name.as_deref(), Some("old-api"));
        assert_eq!(finding.resource_kind, "elasticloadbalancing:loadbalancer");
        assert_eq!(finding.cost, 16.2);
        assert_eq!(finding.evidence, "720 hours, no capacity units");
    }

    #[test]
    fn a_rule_for_a_newer_app_is_not_run() {
        let source =
            "export const rule = { apiVersion: 2 }; export function evaluate() { return []; }";
        let run = run_rules(&[("future.js".to_string(), source.to_string())], &input());
        assert!(run.rules.is_empty());
        assert!(
            run.errors[0].message.contains("rule API 2"),
            "{:?}",
            run.errors
        );
    }

    /// The community rules, as `insights-rules/build.mjs` compiled them,
    /// against their own fixtures in QuickJS: the build ran them in Node,
    /// and this is the engine the app has. Runs where
    /// `CLOUDBRIDGE_RULES_DIST` names the built `dist/` (CI builds it
    /// first); skipped elsewhere.
    #[test]
    fn the_community_rules_pass_their_fixtures_in_quickjs() {
        let Ok(dist) = std::env::var("CLOUDBRIDGE_RULES_DIST") else {
            eprintln!("CLOUDBRIDGE_RULES_DIST not set; community rules not run");
            return;
        };
        let dist = Path::new(&dist);
        let rules = dist.parent().expect("dist has a parent").join("rules");
        let index: Json =
            serde_json::from_str(&std::fs::read_to_string(dist.join("index.json")).unwrap())
                .unwrap();
        let entries = index["rules"].as_array().unwrap();
        assert!(!entries.is_empty());
        for entry in entries {
            let id = entry["id"].as_str().unwrap();
            let file = format!("{id}.js");
            let source = std::fs::read_to_string(dist.join(&file)).unwrap();
            let fixtures = std::fs::read_dir(rules.join(id).join("fixtures")).unwrap();
            for fixture in fixtures {
                let path = fixture.unwrap().path();
                let case: Json =
                    serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                let input: ScriptInput = serde_json::from_value(case["input"].clone())
                    .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                let run = run_rules(&[(file.clone(), source.clone())], &input);
                assert_eq!(run.errors, [], "{}", path.display());
                let mut got: Vec<&str> = run
                    .findings
                    .iter()
                    .map(|f| f.resource_id.as_str())
                    .collect();
                got.sort();
                let mut want: Vec<&str> = case["expect"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|e| e["id"].as_str().unwrap())
                    .collect();
                want.sort();
                assert_eq!(got, want, "{}", path.display());
                for e in case["expect"].as_array().unwrap() {
                    if let Some(evidence) = e["evidence"].as_str() {
                        let f = run
                            .findings
                            .iter()
                            .find(|f| f.resource_id == e["id"].as_str().unwrap())
                            .unwrap();
                        assert_eq!(f.evidence, evidence, "{}", path.display());
                    }
                }
            }
        }
    }

    #[test]
    fn a_broken_rule_is_reported_and_the_others_still_run() {
        let sources = [
            ("a-syntax.js".to_string(), "export const = ;".to_string()),
            (
                "b-throws.js".to_string(),
                "export function evaluate() { throw new Error('nope'); }".to_string(),
            ),
            (
                "c-no-evaluate.js".to_string(),
                "export const rule = {};".to_string(),
            ),
            (
                "d-bad-shape.js".to_string(),
                "export function evaluate() { return [{ id: 1 }]; }".to_string(),
            ),
            (
                "e-loops.js".to_string(),
                "export function evaluate() { for (;;) {} }".to_string(),
            ),
            ("f-ok.js".to_string(), IDLE_ALB.to_string()),
        ];
        let run = run_rules(&sources, &input());
        let failed: Vec<&str> = run.errors.iter().map(|e| e.file.as_str()).collect();
        assert_eq!(
            failed,
            [
                "a-syntax.js",
                "b-throws.js",
                "c-no-evaluate.js",
                "d-bad-shape.js",
                "e-loops.js"
            ]
        );
        assert!(
            run.errors[1].message.contains("nope"),
            "{:?}",
            run.errors[1]
        );
        assert_eq!(run.rules.len(), 1);
        assert_eq!(run.findings.len(), 1);
        assert_eq!(run.findings[0].kind, InsightKind::Script(0));
    }

    #[test]
    fn a_rule_has_no_way_to_reach_outside() {
        let source = r#"
            export function evaluate() {
              const reach = [typeof require, typeof process, typeof fetch, typeof std, typeof os];
              return [{ id: "x", evidence: reach.join(",") }];
            }
        "#;
        let run = run_rules(&[("probe.js".to_string(), source.to_string())], &input());
        assert_eq!(run.errors, []);
        assert_eq!(
            run.findings[0].evidence,
            "undefined,undefined,undefined,undefined,undefined"
        );
        let imports = "import * as std from 'std'; export function evaluate() { return []; }";
        let run = run_rules(&[("import.js".to_string(), imports.to_string())], &input());
        assert_eq!(run.errors.len(), 1);
    }

    #[test]
    fn an_async_evaluate_is_awaited() {
        let source = "export async function evaluate(input) { return [{ id: input.billed[1].id, evidence: 'later', cost: 1.5 }]; }";
        let run = run_rules(&[("later.js".to_string(), source.to_string())], &input());
        assert_eq!(run.errors, []);
        assert_eq!(run.findings[0].cost, 1.5);
        assert_eq!(run.findings[0].resource_kind, "Elastic Load Balancing");
        assert_eq!(run.rules[0].title, "later");
    }
}
