//! Insights View — resources worth a look this period, priced from the bill.
//!
//! The bill says what was spent; it cannot say what is no longer needed.
//! A resource inventory can — a stopped instance, an address attached to
//! nothing, a resource no one owns — and the bill then says what each of
//! those costs. One button gets the inventory: CloudBridge installs its
//! resource scanner on first use (after saying what it is), scans each AWS
//! account with the key saved for it, and reads the results in; the
//! arithmetic is [`crate::analytics::insights`], shared by both targets.
//! Nothing here changes a resource: the page says what to look at, and the
//! people who own the account act on it.

use anyhow::Result;
use chrono::Utc;
use gpui_kit::component::{
    button::*, checkbox::Checkbox, skeleton::Skeleton, Disableable as _, Icon, IconName,
    Sizable as _, StyledExt,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{data, fmt, theme};
use crate::analytics::insights::{insights, resource_key, OWNER_TAG_KEYS};
use crate::cloud::BillingPeriod;
use crate::ingest;
use crate::ledger::query;
use crate::model::{InsightFinding, InsightKind, InsightsReport, InventoryScope};
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
}

/// Read the inventory and the period's resource costs, and judge them.
/// Blocking; wrap in `smol::unblock`.
pub fn load_insights() -> Result<InsightsData> {
    let period = BillingPeriod::containing(Utc::now()).label();
    let resources = query::inventory_resources()?;
    let scope = query::inventory_scope()?;
    let costs = query::resource_usage_costs(&period)?;
    let report = insights(&resources, &costs, scope.as_ref(), OWNER_TAG_KEYS);
    Ok(InsightsData {
        currency: data::reporting_currency(),
        period_label: period,
        report,
        scope,
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
                    return Err("Add an AWS account on the Accounts page to scan it.".to_string());
                }

                let mut scans = Vec::new();
                let mut failures = Vec::new();
                for target in targets {
                    Self::step(
                        &this,
                        cx,
                        format!(
                            "Scanning {} ({})… this usually takes a minute or two",
                            target.account.name,
                            target.regions.join(", ")
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
            parts.push(format!(
                "{} resources in {}",
                report.resources,
                if all {
                    "all regions".to_string()
                } else {
                    scope.regions.join(", ")
                }
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
        let findings: Vec<&InsightFinding> = d
            .report
            .findings
            .iter()
            .filter(|f| f.kind == kind)
            .collect();
        if findings.is_empty() {
            return div().into_any_element();
        }
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
                            .child(theme::header_cell(cx, "RESOURCE").flex_1().min_w_0())
                            .child(theme::header_cell(cx, "TYPE").w_40())
                            .child(theme::header_cell(cx, "REGION").w_32())
                            .child(theme::header_cell(cx, "THIS PERIOD").w_24().text_right()),
                    )
                    .children(
                        findings
                            .iter()
                            .map(|finding| render_row(finding, d.report.priced, &d.currency, cx)),
                    ),
            )
            .when_some(footnote, |el, note| el.child(theme::caption(cx, note)))
            .into_any_element()
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
                    "Insights scans your AWS accounts for stopped instances, idle addresses and \
                     resources nobody owns, then prices each from your bill. CloudBridge installs \
                     and runs the scanner for you; the scan only reads."
                } else {
                    "Insights scans your AWS accounts for stopped instances, idle addresses and \
                     resources nobody owns, then prices each from your bill. Scanning needs the \
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
                                    "Each AWS account is scanned with the access key saved for it. \
                                     The scan only reads: it lists resources and changes nothing.",
                                ))
                                .child(point(
                                    "The key needs read access to resources, such as the AWS \
                                     ReadOnlyAccess policy.",
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
        .child(
            div()
                .w_40()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .whitespace_nowrap()
                .text_ellipsis()
                .child(finding.resource_kind.clone()),
        )
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
