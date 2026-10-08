//! Insights View — resources worth a look this period, priced from the bill.
//!
//! The bill says what was spent; it cannot say what is no longer needed.
//! A resource inventory can — a stopped instance, an address attached to
//! nothing, a resource no one owns — and the bill then says what each of
//! those costs. One button gets the inventory: CloudBridge installs its
//! resource scanner on first use (after saying what it is), scans each AWS
//! and Cloudflare account with the credential saved for it, and reads the
//! results in; the
//! arithmetic is [`crate::analytics::insights`], shared by both targets.
//! Nothing here changes a resource: the page says what to look at, and the
//! people who own the account act on it.

use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use gpui_kit::component::{
    button::*, checkbox::Checkbox, skeleton::Skeleton, Disableable as _, Icon, IconName,
    Sizable as _, StyledExt,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{data, fmt, theme};
use crate::analytics::insights::{
    findings_by_type, insights, inventory_by_source, resource_key, FindingGroup, SourceInventory,
    JUDGED_SOURCES, OWNER_TAG_KEYS,
};
use crate::cloud::BillingPeriod;
use crate::ingest;
use crate::ledger::query;
use crate::model::{
    InsightFinding, InsightKind, InsightsReport, InventoryResource, InventoryScope, ResourceUsage,
};
use crate::ui::theme::CardOutline as _;

actions!(insights, [CloseScanDialog]);

/// Key context for the install dialog, so Escape closes it.
const SCAN_DIALOG_CONTEXT: &str = "InsightsScanDialog";

/// The kinds in the order the page shows them: the ones a person can act
/// on first, the one that may just be a scan's blind spot last.
const KINDS: [InsightKind; 4] = [
    InsightKind::StoppedInstance,
    InsightKind::IdlePublicIp,
    InsightKind::Unclaimed,
    InsightKind::NotInInventory,
];

/// One load of the page: the period's report and what it was judged from.
pub struct InsightsData {
    pub currency: String,
    pub period_label: String,
    pub report: InsightsReport,
    pub scope: Option<InventoryScope>,
    /// What the scan found, by source and type.
    pub inventory: Vec<SourceInventory>,
    /// The resources `inventory` indexes into.
    pub resources: Vec<InventoryResource>,
    /// Each kind's findings by resource type, what a card lists until a
    /// type is opened.
    pub finding_groups: Vec<(InsightKind, Vec<FindingGroup>)>,
    /// This period's usage by resource, keyed as the inventory meets the
    /// bill: `(provider, resource_key)`.
    pub usage: HashMap<(String, String), Vec<ResourceUsage>>,
}

/// Read the inventory and the period's resource costs, and judge them.
/// Blocking; wrap in `smol::unblock`.
pub fn load_insights() -> Result<InsightsData> {
    let period = BillingPeriod::containing(Utc::now()).label();
    let resources = query::inventory_resources()?;
    let scope = query::inventory_scope()?;
    let costs = query::resource_usage_costs(&period)?;
    let report = insights(&resources, &costs, scope.as_ref(), OWNER_TAG_KEYS);
    let mut usage: HashMap<(String, String), Vec<ResourceUsage>> = HashMap::new();
    for row in query::resource_usage_quantities(&period)? {
        usage
            .entry((
                row.provider.clone(),
                resource_key(&row.resource_id).to_string(),
            ))
            .or_default()
            .push(row);
    }
    let finding_groups = KINDS
        .iter()
        .map(|kind| (*kind, findings_by_type(&report.findings, *kind)))
        .collect();
    Ok(InsightsData {
        currency: data::reporting_currency(),
        period_label: period,
        report,
        scope,
        inventory: inventory_by_source(&resources),
        resources,
        finding_groups,
        usage,
    })
}

/// The scan dialog's region choice.
#[derive(Debug, Clone, PartialEq)]
struct RegionChoice {
    /// Every default region, or only `chosen`.
    all: bool,
    chosen: Vec<String>,
}

impl RegionChoice {
    /// What the saved setting says.
    fn saved() -> Self {
        let saved = crate::config::load_config()
            .ok()
            .and_then(|config| config.scan_regions)
            .filter(|regions| !regions.is_empty());
        match saved {
            Some(chosen) => Self { all: false, chosen },
            None => Self {
                all: true,
                chosen: Vec::new(),
            },
        }
    }

    fn can_scan(&self) -> bool {
        self.all || !self.chosen.is_empty()
    }

    /// Persist the choice for later scans.
    fn save(&self) -> Result<()> {
        let mut config = crate::config::load_config()?;
        config.scan_regions = (!self.all).then(|| self.chosen.clone());
        crate::config::save_config(&config)
    }
}

/// Where a scan is.
#[derive(Debug, Clone, PartialEq)]
enum ScanState {
    Idle,
    /// The scan dialog is open: which regions, and — before the first
    /// scan — what will be installed.
    Configuring(RegionChoice),
    /// Working; the step it is on, in the user's terms.
    Running(String),
}

pub struct InsightsView {
    data: Option<InsightsData>,
    /// Why the last load failed, if it did.
    error: Option<String>,
    loading: bool,
    /// Bumped on every load; an older completion is discarded.
    load_generation: u64,
    scan: ScanState,
    /// What the last scan did, or why it failed.
    scan_outcome: Option<Result<String, String>>,
    /// Focus anchor the install dialog tracks, so Escape reaches it.
    dialog_focus: FocusHandle,
    /// The groups opened, by [`group_key`], each with how many of its rows
    /// are shown. A list renders only what is open, so a scan of
    /// thousands costs a page of rows, not thousands.
    expanded: HashMap<String, usize>,
}

/// Rows an opened group shows at first, and adds per "Show more".
const PAGE_ROWS: usize = 50;

/// The key a group's open state is kept under: what it lists, and which
/// type.
fn group_key(scope: &str, resource_type: &str) -> String {
    format!("{scope}|{resource_type}")
}

impl InsightsView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        static BIND_KEYS: std::sync::Once = std::sync::Once::new();
        BIND_KEYS.call_once(|| {
            cx.bind_keys([KeyBinding::new(
                "escape",
                CloseScanDialog,
                Some(SCAN_DIALOG_CONTEXT),
            )]);
        });
        Self {
            data: None,
            error: None,
            loading: false,
            load_generation: 0,
            scan: ScanState::Idle,
            scan_outcome: None,
            expanded: HashMap::new(),
            dialog_focus: cx.focus_handle(),
        }
    }

    /// Start the first load if none has run; the shell calls this on the
    /// page's first visit.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.data.is_none() && !self.loading {
            self.load(cx);
        }
    }

    /// Reload on navigation; a no-op while a load is in flight.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.load(cx);
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.load_generation += 1;
        let generation = self.load_generation;
        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(load_insights).await;
            this.update(cx, |view, cx| {
                if view.load_generation != generation {
                    return;
                }
                match outcome {
                    Ok(loaded) => {
                        view.data = Some(loaded);
                        view.error = None;
                    }
                    Err(e) => view.error = Some(format!("Could not load insights: {e}")),
                }
                view.loading = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The scan button: the first time, ask — what will be installed, and
    /// which regions; after that, scan straight away with the saved regions.
    fn request_scan(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scan != ScanState::Idle {
            return;
        }
        if ingest::scanner_installed() {
            self.scan_outcome = None;
            self.run_scan(cx);
        } else {
            self.open_settings(window, cx);
        }
    }

    /// Open the scan dialog on the saved region choice.
    fn open_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.scan != ScanState::Idle {
            return;
        }
        self.scan = ScanState::Configuring(RegionChoice::saved());
        self.dialog_focus.focus(window, cx);
        cx.notify();
    }

    fn cancel_scan(&mut self, cx: &mut Context<Self>) {
        if matches!(self.scan, ScanState::Configuring(_)) {
            self.scan = ScanState::Idle;
            cx.notify();
        }
    }

    fn update_choice(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut RegionChoice)) {
        if let ScanState::Configuring(choice) = &mut self.scan {
            change(choice);
            cx.notify();
        }
    }

    /// Save the region choice and scan with it.
    fn confirm_scan(&mut self, cx: &mut Context<Self>) {
        let ScanState::Configuring(choice) = &self.scan else {
            return;
        };
        if !choice.can_scan() {
            return;
        }
        if let Err(e) = choice.save() {
            self.scan_outcome = Some(Err(format!("Couldn't save the regions: {e}")));
        } else {
            self.scan_outcome = None;
        }
        self.scan = ScanState::Idle;
        self.run_scan(cx);
    }

    fn step(this: &WeakEntity<Self>, cx: &mut AsyncApp, step: String) {
        cx.update(|cx| {
            this.update(cx, |view, cx| {
                view.scan = ScanState::Running(step);
                cx.notify();
            })
            .ok();
        });
    }

    /// Install if needed, scan every AWS account, and read the results
    /// in. Each step is its own blocking call, so the page can say which
    /// one it is on.
    fn run_scan(&mut self, cx: &mut Context<Self>) {
        self.scan = ScanState::Running("Getting ready…".to_string());
        cx.notify();

        cx.spawn(async move |this, cx| {
            let outcome: Result<String, String> = async {
                if !ingest::scanner_installed() {
                    Self::step(&this, cx, "Installing the scanner…".to_string());
                    smol::unblock(ingest::install_scanner)
                        .await
                        .map_err(|e| format!("Couldn't install the scanner: {e}"))?;
                }

                let targets = smol::unblock(ingest::scan_targets)
                    .await
                    .map_err(|e| e.to_string())?;
                if targets.is_empty() {
                    return Err(
                        "Add an AWS or Cloudflare account on the Accounts page to scan it."
                            .to_string(),
                    );
                }

                let mut scans = Vec::new();
                let mut failures = Vec::new();
                for target in targets {
                    // A provider scanned whole has no regions to name.
                    let place = if target.regions.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", target.regions.join(", "))
                    };
                    Self::step(
                        &this,
                        cx,
                        format!(
                            "Scanning {}{place}… this usually takes a minute or two",
                            target.account.name,
                        ),
                    );
                    let name = target.account.name.clone();
                    match smol::unblock(move || ingest::scan_target(&target)).await {
                        Ok(path) => scans.push(path),
                        Err(e) => failures.push(format!("{name}: {e}")),
                    }
                }
                if scans.is_empty() {
                    return Err(failures.join("\n"));
                }

                Self::step(&this, cx, "Reading the results…".to_string());
                let scope = smol::unblock(move || ingest::import_scans(&scans))
                    .await
                    .map_err(|e| format!("Couldn't read the scan: {e}"))?;
                tracing::info!("Insights scan: {} resources", scope.resource_count);
                // A complete scan says so in the page itself; only what
                // it could not reach needs a message.
                Ok(if failures.is_empty() {
                    String::new()
                } else {
                    format!("Not scanned: {}", failures.join("; "))
                })
            }
            .await;

            cx.update(|cx| {
                this.update(cx, |view, cx| {
                    view.scan = ScanState::Idle;
                    let scanned = outcome.is_ok();
                    view.scan_outcome = Some(outcome);
                    if scanned {
                        view.load(cx);
                    }
                    cx.notify();
                })
                .ok();
            });
        })
        .detach();
    }

    fn scan_button(&self, id: &'static str, label: &'static str, cx: &Context<Self>) -> Button {
        Button::new(id)
            .label(label)
            .small()
            .custom(theme::outline_variant(cx))
            .card_outline(cx)
            .disabled(self.scan != ScanState::Idle)
            .on_click(cx.listener(|this, _, window, cx| this.request_scan(window, cx)))
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let caption = match &self.data {
            Some(d) => format!(
                "Resources worth a look in {}, priced from the bill",
                d.period_label
            ),
            None => "Resources worth a look this period, priced from the bill".to_string(),
        };
        div()
            .w_full()
            .h_flex()
            .items_start()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, "Insights"))
                    .child(theme::caption(cx, caption)),
            )
            // Once there is an inventory the header carries the rescan;
            // before that, the empty state's button is the way in. The
            // browser scans nothing: the demo ships its inventory.
            .when_some(
                self.data
                    .as_ref()
                    .and_then(|d| d.scope.as_ref())
                    .filter(|_| ingest::scanner_supported()),
                |el, scope| {
                    el.child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_3()
                            .child(theme::caption(
                                cx,
                                format!("Scanned {}", fmt::relative_time(scope.scanned_at)),
                            ))
                            .child(
                                Button::new("insights-regions")
                                    .label("Regions")
                                    .small()
                                    .ghost()
                                    .disabled(self.scan != ScanState::Idle)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_settings(window, cx)
                                    })),
                            )
                            .child(self.scan_button("insights-rescan", "Scan again", cx)),
                    )
                },
            )
    }

    /// What the findings were judged from, in one line.
    fn render_scope(&self, d: &InsightsData, cx: &Context<Self>) -> impl IntoElement {
        let report = &d.report;
        let mut parts = Vec::new();
        if let Some(scope) = &d.scope {
            let all = crate::model::AWS_DEFAULT_REGIONS
                .iter()
                .all(|r| scope.regions.iter().any(|s| s == r));
            // Regions are AWS's; a source scanned whole has none to name.
            let regions: Vec<&str> = scope
                .regions
                .iter()
                .map(String::as_str)
                .filter(|r| *r != "global")
                .collect();
            let by_source: Vec<String> = d
                .inventory
                .iter()
                .map(|source| {
                    if source.source == "AWS" {
                        format!(
                            "{} AWS in {}",
                            source.total,
                            if all {
                                "all regions".to_string()
                            } else {
                                regions.join(", ")
                            }
                        )
                    } else {
                        format!("{} {}", source.total, source.source)
                    }
                })
                .collect();
            parts.push(format!(
                "{} resources: {}",
                report.resources,
                by_source.join(", ")
            ));
            if report.described < report.resources {
                // Stopped instances can only be told from configuration.
                parts.push(format!(
                    "configuration read for {} of them",
                    report.described
                ));
            }
        }
        if report.priced && report.billed_cost > 0.0 {
            parts.push(format!(
                "{:.1}% of resource-level usage matched",
                100.0 * report.matched_cost / report.billed_cost
            ));
        }
        theme::caption(cx, parts.join(" · "))
    }

    fn render_stats(&self, d: &InsightsData, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .h_flex()
            .items_stretch()
            .gap_4()
            .children(KINDS.iter().map(|kind| {
                let of_kind = d.report.findings.iter().filter(|f| f.kind == *kind);
                let (count, cost) = of_kind.fold((0, 0.0), |(n, c), f| (n + 1, c + f.cost));
                let plural = if count == 1 { "" } else { "s" };
                // Without a resource-level bill there is no price to lead
                // with; the count is the figure.
                let (value, sub) = if d.report.priced {
                    (
                        fmt::amount(cost, &d.currency),
                        format!("{count} resource{plural}"),
                    )
                } else {
                    (count.to_string(), format!("resource{plural}, not priced"))
                };
                theme::stat_card(
                    cx,
                    stat_label(*kind),
                    value,
                    div().text_color(theme::text_muted(cx)).child(sub),
                )
            }))
    }

    fn render_kind(&self, d: &InsightsData, kind: InsightKind, cx: &Context<Self>) -> AnyElement {
        let Some((_, groups)) = d
            .finding_groups
            .iter()
            .find(|(of, groups)| *of == kind && !groups.is_empty())
        else {
            return div().into_any_element();
        };
        let scope = format!("finding:{kind:?}");
        let footnote = match kind {
            InsightKind::Unclaimed if d.report.unclaimed_free > 0 => Some(format!(
                "{} more unclaimed resource{} cost nothing this period",
                d.report.unclaimed_free,
                if d.report.unclaimed_free == 1 {
                    ""
                } else {
                    "s"
                }
            )),
            _ => None,
        };

        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::section_title(cx, section_title(kind)))
                    .child(theme::caption(cx, explanation(kind))),
            )
            .child(
                div()
                    .v_flex()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_4()
                            .pb_2()
                            .child(theme::header_cell(cx, "TYPE").flex_1().min_w_0())
                            .child(theme::header_cell(cx, "RESOURCES").w_24().text_right())
                            .child(theme::header_cell(cx, "THIS PERIOD").w_24().text_right()),
                    )
                    .children(groups.iter().map(|group| {
                        let key = group_key(&scope, &group.resource_type);
                        let shown = self.expanded.get(&key).copied();
                        let cost = if d.report.priced {
                            fmt::amount(group.cost, &d.currency)
                        } else {
                            "—".to_string()
                        };
                        div()
                            .v_flex()
                            .child(self.render_group_row(
                                &key,
                                group.resource_type.clone(),
                                group.count,
                                div().w_24().text_right().child(cost),
                                shown.is_some(),
                                cx,
                            ))
                            .when_some(shown, |el, shown| {
                                el.child(
                                    div()
                                        .pl_6()
                                        .v_flex()
                                        .children(group.members.iter().take(shown).map(|&i| {
                                            render_row(
                                                &d.report.findings[i],
                                                d.report.priced,
                                                &d.currency,
                                                cx,
                                            )
                                        }))
                                        .child(self.render_more(&key, group.count, shown, cx)),
                                )
                            })
                    })),
            )
            .when_some(footnote, |el, note| el.child(theme::caption(cx, note)))
            .into_any_element()
    }

    /// A type's summary row: click to open its resources, again to close.
    fn render_group_row(
        &self,
        key: &str,
        label: String,
        count: usize,
        trailing: impl IntoElement,
        open: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let toggle_key = key.to_string();
        div()
            .id(SharedString::from(format!("group-{key}")))
            .h_flex()
            .items_center()
            .gap_4()
            .py_2()
            .border_t_1()
            .border_color(theme::card_border(cx))
            .cursor_pointer()
            .hover(|style| style.bg(theme::sidebar_bg(cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Icon::new(if open {
                            IconName::ChevronDown
                        } else {
                            IconName::ChevronRight
                        })
                        .small()
                        .text_color(theme::text_muted(cx)),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(label),
                    ),
            )
            .child(div().w_24().text_right().child(count.to_string()))
            .child(trailing)
            .on_click(cx.listener(move |this, _, _, cx| {
                if this.expanded.remove(&toggle_key).is_none() {
                    this.expanded.insert(toggle_key.clone(), PAGE_ROWS);
                }
                cx.notify();
            }))
    }

    /// Under an open group: how many of its rows show, and a button for
    /// the next page of them while there are more.
    fn render_more(&self, key: &str, total: usize, shown: usize, cx: &Context<Self>) -> Div {
        let more_key = key.to_string();
        div()
            .h_flex()
            .items_center()
            .gap_3()
            .py_2()
            .when(total > shown, |el| {
                el.child(theme::caption(cx, format!("Showing {shown} of {total}")))
                    .child(
                        Button::new(SharedString::from(format!("more-{key}")))
                            .label(format!("Show {} more", (total - shown).min(PAGE_ROWS)))
                            .ghost()
                            .small()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(shown) = this.expanded.get_mut(&more_key) {
                                    *shown += PAGE_ROWS;
                                }
                                cx.notify();
                            })),
                    )
            })
    }

    /// A source the findings are not written for, listed by type, so its
    /// scan shows on the page rather than only in a resource count.
    fn render_inventory(
        &self,
        d: &InsightsData,
        source: &SourceInventory,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let scope = format!("inventory:{}", source.source);
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    // `section_title` takes a fixed string; this one
                    // names the source, so it is set the same way by hand.
                    .child(
                        div()
                            .text_base()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_primary(cx))
                            .child(inventory_title(&source.source)),
                    )
                    .child(theme::caption(
                        cx,
                        format!(
                            "{} resources from the last scan, by type; open a type to list \
                             them. This period's cost is the bill split by each resource's \
                             share of the usage — an estimate, adding up to the bill. No \
                             findings are written for {} yet.",
                            source.total, source.source
                        ),
                    )),
            )
            .child(
                div()
                    .v_flex()
                    .child(
                        div()
                            .h_flex()
                            .items_center()
                            .gap_4()
                            .pb_2()
                            .child(theme::header_cell(cx, "TYPE").flex_1().min_w_0())
                            .child(theme::header_cell(cx, "RESOURCES").w_24().text_right())
                            .child(theme::header_cell(cx, "THIS PERIOD").w_24().text_right())
                            .child(theme::header_cell(cx, "INCLUDING").w_96()),
                    )
                    .children(self.costed_types(d, source).into_iter().map(
                        |(kind, cost, members)| {
                            let key = group_key(&scope, &kind.resource_type);
                            let shown = self.expanded.get(&key).copied();
                            let more = kind.count.saturating_sub(kind.names.len());
                            let mut names = kind.names.join(", ");
                            if more > 0 {
                                names.push_str(&format!(" and {more} more"));
                            }
                            div()
                                .v_flex()
                                .child(
                                    self.render_group_row(
                                        &key,
                                        type_label(&source.source, &kind.resource_type),
                                        kind.count,
                                        div()
                                            .h_flex()
                                            .gap_4()
                                            .child(
                                                div()
                                                    .w_24()
                                                    .text_right()
                                                    .child(fmt::amount(cost, &d.currency)),
                                            )
                                            .child(
                                                div()
                                                    .w_96()
                                                    .whitespace_nowrap()
                                                    .text_ellipsis()
                                                    .text_sm()
                                                    .text_color(theme::text_muted(cx))
                                                    .child(names),
                                            ),
                                        shown.is_some(),
                                        cx,
                                    ),
                                )
                                .when_some(shown, |el, shown| {
                                    el.child(
                                        div()
                                            .pl_6()
                                            .v_flex()
                                            .children(members.iter().take(shown).map(|&i| {
                                                let resource = &d.resources[i];
                                                let key = (
                                                    resource.provider.clone(),
                                                    resource_key(&resource.resource_id).to_string(),
                                                );
                                                render_resource_row(
                                                    resource,
                                                    d.usage.get(&key),
                                                    d.report.resource_cost.get(i).copied(),
                                                    &d.currency,
                                                    cx,
                                                )
                                            }))
                                            .child(self.render_more(&key, kind.count, shown, cx)),
                                    )
                                })
                        },
                    )),
            )
    }

    /// A source's types with what the bill charged each this period, and
    /// their resources in the order to list them: costliest first, so the
    /// bucket or namespace behind a spike heads its type, and the type
    /// heads the card. Unpriced, they keep the scan's order.
    fn costed_types<'a>(
        &self,
        d: &InsightsData,
        source: &'a SourceInventory,
    ) -> Vec<(&'a crate::analytics::insights::TypeCount, f64, Vec<usize>)> {
        let cost_of = |i: usize| d.report.resource_cost.get(i).copied().unwrap_or(0.0);
        let mut types: Vec<_> = source
            .types
            .iter()
            .map(|kind| {
                let mut members = kind.members.clone();
                members.sort_by(|&a, &b| cost_of(b).total_cmp(&cost_of(a)));
                let cost = members.iter().map(|&i| cost_of(i)).sum::<f64>();
                (kind, cost, members)
            })
            .collect();
        types.sort_by(|a, b| b.1.total_cmp(&a.1));
        types
    }

    /// Said once, above the findings, when the bill names no resources:
    /// what is listed is real, but nothing on it can be priced.
    fn render_unpriced(&self, cx: &Context<Self>) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .h_flex()
            .items_center()
            .justify_between()
            .gap_4()
            .child(
                div()
                    .min_w_0()
                    .v_flex()
                    .gap_1()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme::text_primary(cx))
                            .child("Prices need a resource-level bill"),
                    )
                    .child(theme::caption(
                        cx,
                        "This period's bill names services, not resources, so nothing below can \
                         be priced yet. Point the AWS account at its Data Export on the Accounts \
                         page and refresh.",
                    )),
            )
            .child(
                Button::new("insights-open-accounts-pricing")
                    .label("Open Accounts")
                    .small()
                    .custom(theme::outline_variant(cx))
                    .card_outline(cx)
                    .on_click(|_, _, cx| {
                        crate::app::navigate_to(crate::app::CurrentView::Accounts, cx)
                    }),
            )
    }

    fn render_empty(&self, d: Option<&InsightsData>, cx: &Context<Self>) -> impl IntoElement {
        let has_inventory = d.is_some_and(|d| d.scope.is_some());
        let (title, body) = if has_inventory {
            (
                "Nothing to review this period",
                "No stopped instance, idle address, unclaimed or missing resource has a \
                 charge this period.",
            )
        } else {
            (
                "Find resources to cut",
                if ingest::scanner_supported() {
                    "Insights scans your AWS and Cloudflare accounts for stopped instances, idle \
                     addresses and resources nobody owns, then prices each from your bill. CloudBridge installs \
                     and runs the scanner for you; the scan only reads."
                } else {
                    "Insights scans your AWS and Cloudflare accounts for stopped instances, idle \
                     addresses and resources nobody owns, then prices each from your bill. Scanning needs the \
                     desktop app on macOS (Apple Silicon) or Windows."
                },
            )
        };
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_primary(cx))
                    .child(title),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_muted(cx))
                    .child(body),
            )
            .child(
                div()
                    .pt_1()
                    .h_flex()
                    .gap_2()
                    .when(!has_inventory && ingest::scanner_supported(), |el| {
                        el.child(self.scan_button("insights-scan", "Set up and scan", cx))
                    })
                    .child(
                        Button::new("insights-open-accounts")
                            .label("Open Accounts")
                            .small()
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .on_click(|_, _, cx| {
                                crate::app::navigate_to(crate::app::CurrentView::Accounts, cx)
                            }),
                    ),
            )
    }

    /// The scan in progress: the step it is on, and that it is working.
    fn render_progress(&self, step: &str, cx: &Context<Self>) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .h_flex()
            .items_center()
            .gap_3()
            .child(
                Icon::new(IconName::LoaderCircle)
                    .size_4()
                    .text_color(theme::text_muted(cx))
                    .with_animation(
                        "insights-scan-spinner",
                        Animation::new(std::time::Duration::from_secs(1)).repeat(),
                        |icon, delta| icon.transform(Transformation::rotate(percentage(delta))),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_primary(cx))
                    .child(step.to_string()),
            )
    }

    /// The scan dialog: before the first scan, what will be installed and
    /// what the scan does; every time it opens, which regions to cover.
    fn render_scan_dialog(&self, cx: &Context<Self>) -> AnyElement {
        let ScanState::Configuring(choice) = &self.scan else {
            return div().size_0().into_any_element();
        };
        let installing = !ingest::scanner_installed();
        let point = |text: &'static str| {
            div()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child(text)
        };
        let mode = |id: &'static str, label: &'static str, all: bool| {
            let active = choice.all == all;
            let button = Button::new(id)
                .label(label)
                .small()
                .rounded_full()
                .custom(theme::range_pill(cx, active))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.update_choice(cx, |choice| choice.all = all);
                }));
            if active {
                button.card_outline(cx)
            } else {
                button
            }
        };

        let regions = div()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Regions to scan"),
            )
            .child(
                div()
                    .h_flex()
                    .gap_1()
                    .child(mode("insights-regions-all", "All regions", true))
                    .child(mode("insights-regions-chosen", "Choose regions", false)),
            )
            .child(theme::caption(
                cx,
                if choice.all {
                    "Every region AWS enables by default. Scanning all of them takes a few minutes."
                } else {
                    "Only the regions ticked below."
                },
            ))
            .when(!choice.all, |el| {
                el.child(div().flex().flex_wrap().gap_x_4().gap_y_2().children(
                    crate::model::AWS_DEFAULT_REGIONS.iter().map(|region| {
                        let region = region.to_string();
                        let ticked = choice.chosen.contains(&region);
                        div().w_32().child(
                            Checkbox::new(SharedString::from(format!("insights-region-{region}")))
                                .label(region.clone())
                                .checked(ticked)
                                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                                    let region = region.clone();
                                    let checked = *checked;
                                    this.update_choice(cx, |choice| {
                                        choice.chosen.retain(|r| *r != region);
                                        if checked {
                                            choice.chosen.push(region);
                                            choice.chosen.sort();
                                        }
                                    });
                                })),
                        )
                    }),
                ))
            });

        let confirm_label = if installing {
            "Install and scan"
        } else {
            "Scan"
        };
        let can_scan = choice.can_scan();

        div()
            .id("insights-scan-scrim")
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(theme::scrim(cx))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.cancel_scan(cx)),
            )
            .child(
                div()
                    .id("insights-scan-dialog")
                    .occlude()
                    .key_context(SCAN_DIALOG_CONTEXT)
                    .track_focus(&self.dialog_focus)
                    .on_action(cx.listener(|this, _: &CloseScanDialog, _, cx| {
                        this.cancel_scan(cx);
                        cx.stop_propagation();
                    }))
                    .w_128()
                    .p_6()
                    .rounded_xl()
                    .bg(theme::card_bg(cx))
                    .border_1()
                    .border_color(theme::card_border(cx))
                    .text_color(theme::text_primary(cx))
                    .shadow_lg()
                    .v_flex()
                    .gap_4()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::BOLD)
                            .child(if installing {
                                "Install the resource scanner?"
                            } else {
                                "Scan settings"
                            }),
                    )
                    .when(installing, |el| {
                        el.child(
                            div()
                                .v_flex()
                                .gap_2()
                                .child(point(
                                    "CloudBridge downloads corkscrew, a free open-source scanner \
                                     (MIT licence, about 30 MB), and keeps it in its own data folder.",
                                ))
                                .child(point(
                                    "Each AWS or Cloudflare account is scanned with the credential \
                                     saved for it. The scan only reads: it lists resources and \
                                     changes nothing.",
                                ))
                                .child(point(
                                    "An AWS key needs read access to resources, such as the \
                                     ReadOnlyAccess policy. A Cloudflare token needs Read on \
                                     Workers, R2, KV, Queues, D1 and zones, beside Billing.",
                                )),
                        )
                    })
                    .child(regions)
                    .child(
                        div()
                            .h_flex()
                            .gap_2()
                            .justify_end()
                            .child(
                                Button::new("insights-scan-cancel")
                                    .label("Cancel")
                                    .ghost()
                                    .on_click(cx.listener(|this, _, _, cx| this.cancel_scan(cx))),
                            )
                            .child(
                                Button::new("insights-scan-confirm")
                                    .label(confirm_label)
                                    .primary()
                                    .disabled(!can_scan)
                                    .on_click(cx.listener(|this, _, _, cx| this.confirm_scan(cx))),
                            ),
                    )
                    .with_animation(
                        "insights-scan-dialog-enter",
                        theme::dialog_enter_animation(),
                        |this, delta| this.opacity(delta).mt(px(10.0 * (1.0 - delta))),
                    ),
            )
            .into_any_element()
    }

    fn render_loading(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_6()
            .child(div().w_full().h_flex().gap_4().children((0..4).map(|_| {
                theme::card(cx)
                    .flex_1()
                    .p_5()
                    .v_flex()
                    .gap_2()
                    .child(Skeleton::new().w_24().h_3())
                    .child(Skeleton::new().w_32().h_6())
            })))
            .child(
                theme::card(cx)
                    .w_full()
                    .p_5()
                    .v_flex()
                    .gap_3()
                    .child(Skeleton::new().w_40().h_4())
                    .children((0..5).map(|_| Skeleton::new().w_full().h_4())),
            )
    }

    fn render_message(&self, cx: &Context<Self>, message: &Result<String, String>) -> Div {
        let (bg, fg, text) = match message {
            Ok(text) => (theme::sidebar_bg(cx), theme::text_primary(cx), text),
            Err(text) => (theme::danger_bg(cx), theme::danger(cx), text),
        };
        div()
            .w_full()
            .p_3()
            .rounded_md()
            .bg(bg)
            .text_sm()
            .text_color(fg)
            .child(text.clone())
    }
}

/// One finding under an opened type group.
fn render_row(finding: &InsightFinding, priced: bool, currency: &str, cx: &App) -> Div {
    let label = finding
        .name
        .clone()
        .unwrap_or_else(|| resource_key(&finding.resource_id).to_string());
    div()
        .w_full()
        .h_flex()
        .items_center()
        .gap_4()
        .py_2()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .text_color(theme::text_primary(cx))
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_muted(cx))
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(format!("{} · {}", finding.evidence, finding.resource_id)),
                ),
        )
        // No type column: the row sits under its type's group.
        .child(
            div()
                .w_32()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child(finding.region.clone().unwrap_or_else(|| "—".to_string())),
        )
        .child(
            div()
                .w_24()
                .text_right()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_primary(cx))
                .child(if priced {
                    fmt::amount(finding.cost, currency)
                } else {
                    "—".to_string()
                }),
        )
}

/// One scanned resource under an opened inventory type: its name, what it
/// used this period (or, with no usage on the bill, the scan's id for it),
/// where it is, and what the bill charged it.
fn render_resource_row(
    resource: &InventoryResource,
    usage: Option<&Vec<ResourceUsage>>,
    cost: Option<f64>,
    currency: &str,
    cx: &App,
) -> Div {
    let label = resource
        .name
        .clone()
        .unwrap_or_else(|| resource_key(&resource.resource_id).to_string());
    div()
        .w_full()
        .h_flex()
        .items_center()
        .gap_4()
        .py_2()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .v_flex()
                .gap_1()
                .child(
                    div()
                        .text_sm()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(label),
                )
                .child(
                    div()
                        .text_xs()
                        .text_color(theme::text_muted(cx))
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .child(match usage {
                            Some(usage) => usage_line(usage),
                            None => resource.resource_id.clone(),
                        }),
                ),
        )
        .child(
            div()
                .w_32()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child(resource.region.clone().unwrap_or_else(|| "—".to_string())),
        )
        .child(
            div()
                .w_24()
                .text_right()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(fmt::amount(cost.unwrap_or(0.0), currency)),
        )
}

/// A resource's usage this period in one line: each service the bill
/// split onto it, without the allowance note in its name, and how much.
fn usage_line(usage: &[ResourceUsage]) -> String {
    usage
        .iter()
        .map(|row| {
            let service = row
                .service
                .split(" (")
                .next()
                .unwrap_or(&row.service)
                .trim();
            let amount = if row.quantity < 1.0 {
                format!("{:.2}", row.quantity)
            } else {
                fmt::quantity(row.quantity)
            };
            match row
                .unit
                .as_deref()
                .filter(|unit| !unit.is_empty() && *unit != "Count")
            {
                Some(unit) => format!("{service} {amount} {unit}"),
                None => format!("{service} {amount}"),
            }
        })
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The inventory card's title for a source.
fn inventory_title(source: &str) -> String {
    format!("{source} resources")
}

/// A resource type as a person calls it: Cloudflare's plugin names its
/// types in snake case (`worker_script`), the product names them Workers.
/// Anything not listed keeps the scanner's own name.
fn type_label(source: &str, resource_type: &str) -> String {
    let named = match (source, resource_type) {
        ("Cloudflare", "account") => "Accounts",
        ("Cloudflare", "zone") => "Zones",
        ("Cloudflare", "dns_record") => "DNS records",
        ("Cloudflare", "worker_script") => "Workers",
        ("Cloudflare", "worker_route") => "Worker routes",
        ("Cloudflare", "worker_domain") => "Worker custom domains",
        ("Cloudflare", "durable_object_namespace") => "Durable Object namespaces",
        ("Cloudflare", "durable_object") => "Durable Objects",
        ("Cloudflare", "r2_bucket") => "R2 buckets",
        ("Cloudflare", "kv_namespace") => "KV namespaces",
        ("Cloudflare", "queue") => "Queues",
        ("Cloudflare", "d1_database") => "D1 databases",
        ("Cloudflare", "secret_store") => "Secrets Stores",
        ("Cloudflare", "secret_store_secret") => "Secrets",
        _ => return resource_type.to_string(),
    };
    named.to_string()
}

fn stat_label(kind: InsightKind) -> &'static str {
    match kind {
        InsightKind::StoppedInstance => "STOPPED INSTANCES",
        InsightKind::IdlePublicIp => "IDLE ADDRESSES",
        InsightKind::Unclaimed => "UNCLAIMED",
        InsightKind::NotInInventory => "NOT IN THE SCAN",
    }
}

fn section_title(kind: InsightKind) -> &'static str {
    match kind {
        InsightKind::StoppedInstance => "Stopped instances",
        InsightKind::IdlePublicIp => "Idle public addresses",
        InsightKind::Unclaimed => "Unclaimed resources",
        InsightKind::NotInInventory => "Billed, but not in the scan",
    }
}

fn explanation(kind: InsightKind) -> &'static str {
    match kind {
        InsightKind::StoppedInstance => {
            "Stopped when scanned. Compute stops billing; attached volumes and addresses do not. \
             Snapshot and delete what will not start again."
        }
        InsightKind::IdlePublicIp => {
            "Public IPv4 addresses the bill charges as idle: allocated, attached to nothing. \
             Release the ones nobody needs."
        }
        InsightKind::Unclaimed => {
            "No owner tag (owner, team, project, cost-center, business_line) and no stack or app \
             that manages them. Find an owner, or remove them."
        }
        InsightKind::NotInInventory => {
            "Charged this period in a region the scan covered, but not found by it: deleted \
             since, or a type the scan does not list."
        }
    }
}

impl Render for InsightsView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = if let Some(d) = &self.data {
            let empty = d.report.findings.is_empty() && d.report.unclaimed_free == 0;
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx))
                .when_some(
                    self.scan_outcome
                        .as_ref()
                        .filter(|outcome| !matches!(outcome, Ok(text) if text.is_empty())),
                    |el, outcome| el.child(self.render_message(cx, outcome)),
                )
                .when_some(
                    match &self.scan {
                        ScanState::Running(step) => Some(step.clone()),
                        _ => None,
                    },
                    |el, step| el.child(self.render_progress(&step, cx)),
                )
                .when_some(self.error.as_ref(), |el, error| {
                    el.child(self.render_message(cx, &Err(error.clone())))
                })
                .when(d.scope.is_some() || d.report.billed_cost > 0.0, |el| {
                    el.child(self.render_scope(d, cx))
                })
                .map(|el| {
                    if empty {
                        el.child(self.render_empty(Some(d), cx))
                    } else {
                        el.when(!d.report.priced && d.scope.is_some(), |el| {
                            el.child(self.render_unpriced(cx))
                        })
                        .child(self.render_stats(d, cx))
                        .children(KINDS.iter().map(|kind| self.render_kind(d, *kind, cx)))
                    }
                })
                .children(
                    d.inventory
                        .iter()
                        .filter(|source| !JUDGED_SOURCES.contains(&source.source.as_str()))
                        .map(|source| self.render_inventory(d, source, cx)),
                )
                .into_any_element()
        } else if let Some(error) = &self.error {
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx))
                .child(self.render_message(cx, &Err(error.clone())))
                .child(
                    div().h_flex().child(
                        Button::new("insights-retry")
                            .label("Retry")
                            .custom(theme::outline_variant(cx))
                            .card_outline(cx)
                            .on_click(cx.listener(|this, _, _, cx| this.load(cx))),
                    ),
                )
                .into_any_element()
        } else if self.loading {
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx))
                .child(self.render_loading(cx))
                .into_any_element()
        } else {
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx))
                .child(self.render_empty(None, cx))
                .into_any_element()
        };

        div()
            .size_full()
            .relative()
            .child(
                div()
                    .id("insights")
                    .size_full()
                    .v_flex()
                    .gap_6()
                    .p_8()
                    .bg(theme::app_bg(cx))
                    .overflow_y_scroll()
                    .child(body),
            )
            .child(self.render_scan_dialog(cx))
    }
}
