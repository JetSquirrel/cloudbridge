//! Attribution View — how every dollar travels from the source that billed
//! it to the business line that caused it, with drill-downs by service,
//! region, and service category over the same period.

use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;
use gpui_kit::component::{button::*, skeleton::Skeleton, Sizable as _, StyledExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{chart, data, fmt, theme};
use crate::cloud::BillingPeriod;
use crate::ledger::query::{self, BreakdownDim};
use crate::ui::theme::CardOutline as _;
use crate::{db, ingest};

/// Sankey drawing constants.
const SANKEY_HEIGHT: f32 = 420.0;
const NODE_WIDTH: f32 = 10.0;
const NODE_RADIUS: f32 = 3.0;
const NODE_GAP: f32 = 6.0;
const LINK_OPACITY: f32 = 0.35;
/// Width of the label columns flanking the diagram. Stays in px rather
/// than a rem helper: it is canvas-mirroring geometry and must track the
/// sankey's pixel-exact heights and gaps, not the font scale.
const LABEL_WIDTH: f32 = 110.0;

/// Number of Sankey columns in the data (0 source … N-1 business line).
fn column_count(data: &data::SankeyData) -> usize {
    data.nodes
        .iter()
        .map(|n| n.column)
        .max()
        .map_or(0, |m| m + 1)
}

/// A color per business-line node: the largest line takes the accent, the
/// rest cycle the olive tones, and Unallocated is always grey.
fn line_colors(cx: &App, data: &data::SankeyData) -> HashMap<String, Hsla> {
    let columns = column_count(data);
    let mut lines: Vec<&data::SankeyNode> = data
        .nodes
        .iter()
        .filter(|n| n.column + 1 == columns)
        .collect();
    lines.sort_by(|a, b| {
        b.value
            .partial_cmp(&a.value)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let palette = [
        theme::accent(cx),
        theme::olive(cx),
        theme::olive_light(cx),
        theme::warning_text(cx),
    ];
    let mut map = HashMap::new();
    let mut next = 0;
    for node in lines {
        let color = if node.label == "Unallocated" {
            theme::grey(cx)
        } else {
            let color = palette[next % palette.len()];
            next += 1;
            color
        };
        map.insert(node.id.clone(), color);
    }
    map
}

/// For every node, the business line the largest share of its downstream
/// flow reaches.
fn dominant_lines(data: &data::SankeyData) -> HashMap<&str, &str> {
    let columns = column_count(data);
    let mut dominant: HashMap<&str, &str> = HashMap::new();
    if columns == 0 {
        return dominant;
    }
    for node in data.nodes.iter().filter(|n| n.column == columns - 1) {
        dominant.insert(node.id.as_str(), node.id.as_str());
    }
    // Walk the columns right to left so each target's line is settled
    // before the nodes feeding it are scored.
    for column in (0..columns - 1).rev() {
        for node in data.nodes.iter().filter(|n| n.column == column) {
            let mut totals: HashMap<&str, f64> = HashMap::new();
            for link in data.links.iter().filter(|l| l.from == node.id) {
                if let Some(line) = dominant.get(link.to.as_str()) {
                    *totals.entry(line).or_insert(0.0) += link.value;
                }
            }
            if let Some((line, _)) = totals
                .into_iter()
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            {
                dominant.insert(node.id.as_str(), line);
            }
        }
    }
    dominant
}

/// Pixel heights for every node plus the raw value-to-pixel scale, sharing
/// one vertical scale across columns: every column totals the same spend,
/// so the scale is set by the column with the most nodes.
fn node_heights(data: &data::SankeyData) -> (HashMap<String, f32>, f32) {
    let columns = column_count(data);
    let max_nodes = (0..columns)
        .map(|col| data.nodes.iter().filter(|n| n.column == col).count())
        .max()
        .unwrap_or(1)
        .max(1);
    let column_total: f64 = data
        .nodes
        .iter()
        .filter(|n| n.column == 0)
        .map(|n| n.value)
        .sum();
    let scale = if column_total > 0.0 {
        ((SANKEY_HEIGHT - NODE_GAP * (max_nodes as f32 - 1.0)) / column_total as f32).max(0.0)
    } else {
        0.0
    };
    let heights = data
        .nodes
        .iter()
        .map(|n| (n.id.clone(), ((n.value as f32) * scale).max(2.0)))
        .collect();
    (heights, scale)
}

/// Owned per-node snapshot the canvas closures can capture.
struct NodeSnap {
    id: String,
    column: usize,
}

/// Owned per-link snapshot the canvas closures can capture.
struct LinkSnap {
    from: String,
    to: String,
    value: f64,
}

/// Prepaint state for the Sankey canvas: every bar and ribbon, resolved to
/// pixels.
struct SankeyLayout {
    bars: Vec<(Bounds<Pixels>, Hsla)>,
    ribbons: Vec<(Path<Pixels>, Hsla)>,
}

/// Which dimension the page breaks the period down by. `Tag` is the
/// classic view — the Sankey and the Unallocated card; the other three are
/// flat breakdowns over the ledger's stored dimensions.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DrillDim {
    Tag,
    Service,
    Region,
    ServiceCategory,
}

impl DrillDim {
    const ALL: [DrillDim; 4] = [
        DrillDim::Tag,
        DrillDim::Service,
        DrillDim::Region,
        DrillDim::ServiceCategory,
    ];

    fn label(self) -> &'static str {
        match self {
            DrillDim::Tag => "Tag",
            DrillDim::Service => "Service",
            DrillDim::Region => "Region",
            DrillDim::ServiceCategory => "Category",
        }
    }

    fn id(self) -> &'static str {
        match self {
            DrillDim::Tag => "dim-tag",
            DrillDim::Service => "dim-service",
            DrillDim::Region => "dim-region",
            DrillDim::ServiceCategory => "dim-category",
        }
    }

    /// The breakdown card's title in this dimension. `Tag` never renders
    /// the card; its title only keeps the match total.
    fn title(self) -> &'static str {
        match self {
            DrillDim::Tag => "By business line",
            DrillDim::Service => "By service",
            DrillDim::Region => "By region",
            DrillDim::ServiceCategory => "By service category",
        }
    }

    /// The bucket column's table header.
    fn bucket_header(self) -> &'static str {
        match self {
            DrillDim::Tag => "BUSINESS LINE",
            DrillDim::Service => "SERVICE",
            DrillDim::Region => "REGION",
            DrillDim::ServiceCategory => "SERVICE CATEGORY",
        }
    }

    /// Footnote under a breakdown that has an 'Other' bucket, naming what
    /// landed there.
    fn other_caption(self) -> &'static str {
        match self {
            DrillDim::Tag => "Charges with no business-line tag read as 'Other'.",
            DrillDim::Service => "Charges with no service read as 'Other'.",
            DrillDim::Region => "Charges with no region read as 'Other'.",
            DrillDim::ServiceCategory => "Charges with no service category read as 'Other'.",
        }
    }

    /// This dimension's rows of the loaded drill-down.
    fn rows(self, drilldown: &DrilldownData) -> &[(String, f64)] {
        match self {
            DrillDim::Tag => &[],
            DrillDim::Service => &drilldown.by_service,
            DrillDim::Region => &drilldown.by_region,
            DrillDim::ServiceCategory => &drilldown.by_category,
        }
    }

    /// The previous period's rows, the treemap's heat base.
    fn prev_rows(self, drilldown: &DrilldownData) -> &[(String, f64)] {
        match self {
            DrillDim::Tag => &[],
            DrillDim::Service => &drilldown.prev_by_service,
            DrillDim::Region => &drilldown.prev_by_region,
            DrillDim::ServiceCategory => &drilldown.prev_by_category,
        }
    }
}

/// How the non-tag breakdown card presents its buckets: the MoM heat
/// treemap or the classic share table.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BreakdownMode {
    Map,
    Table,
}

impl BreakdownMode {
    const ALL: [BreakdownMode; 2] = [BreakdownMode::Map, BreakdownMode::Table];

    fn label(self) -> &'static str {
        match self {
            BreakdownMode::Map => "Map",
            BreakdownMode::Table => "Table",
        }
    }

    fn id(self) -> &'static str {
        match self {
            BreakdownMode::Map => "mode-map",
            BreakdownMode::Table => "mode-table",
        }
    }
}

/// Attribution View
pub struct AttributionView {
    /// The loaded page data, once the background load has landed.
    data: Option<data::AttributionData>,
    /// The breakdowns behind the non-tag dimensions and the Top resources
    /// card; lands in the same flight as `data`.
    drilldown: Option<DrilldownData>,
    /// The dimension the header switcher has selected.
    dim: DrillDim,
    /// Map vs. table for the non-tag breakdown card.
    breakdown_mode: BreakdownMode,
    /// Treemap hover state (tile under the mouse + the canvas's per-frame
    /// tile-bounds cell); drives the hover outline and tooltip.
    treemap_hover: chart::TreemapHover,
    /// Why the last load failed, if it did.
    error: Option<String>,
    /// Whether a load is in flight.
    loading: bool,
    /// Bumped on every load; a completion stamped with an older generation
    /// is discarded so a slow first load cannot clobber a newer result.
    load_generation: u64,
}

impl AttributionView {
    pub fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        Self {
            data: None,
            drilldown: None,
            dim: DrillDim::Tag,
            breakdown_mode: BreakdownMode::Map,
            treemap_hover: chart::TreemapHover::new(),
            error: None,
            loading: false,
            load_generation: 0,
        }
    }

    /// Start the first load if none has run. The app shell calls this on
    /// the page's first visit, so construction — and window opening —
    /// stays cheap and the hidden pages do not race the visible one for
    /// the ledger at startup.
    pub fn ensure_loaded(&mut self, cx: &mut Context<Self>) {
        if self.data.is_none() && !self.loading {
            self.load(cx);
        }
    }

    /// Reload the page's data. Called by the app shell when this page is
    /// navigated to; a no-op while a load is already in flight. Existing
    /// data stays on screen while the reload runs — no loading flash.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        if self.loading {
            return;
        }
        self.load(cx);
    }

    /// Load the page's data off the UI thread; the ledger queries are
    /// blocking. The drill-down rides in the same flight, so switching the
    /// dimension later is a re-render, not a reload.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.load_generation += 1;
        let generation = self.load_generation;
        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(|| -> Result<_> {
                let attribution = data::load_attribution()?;
                let drilldown = load_drilldown()?;
                Ok((attribution, drilldown))
            })
            .await;
            this.update(cx, |view, cx| {
                if view.load_generation != generation {
                    return;
                }
                match outcome {
                    Ok((loaded, drilldown)) => {
                        view.data = Some(loaded);
                        view.drilldown = Some(drilldown);
                        view.error = None;
                        // New tiles land in new places; a stale hover
                        // would tag the wrong bucket.
                        view.treemap_hover.clear();
                    }
                    Err(e) => {
                        view.error = Some(format!("Could not load attribution: {e}"));
                    }
                }
                view.loading = false;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn render_header(
        &self,
        cx: &mut Context<Self>,
        total: Option<(f64, &str)>,
    ) -> impl IntoElement {
        let caption = total.map(|(amount, currency)| {
            format!("{} this billing period", fmt::amount(amount, currency))
        });
        let selected = self.dim;
        div()
            .w_full()
            .h_flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .v_flex()
                    .gap_1()
                    .child(theme::page_title(cx, "Attribution"))
                    .when_some(caption, |el, text| el.child(theme::caption(cx, text))),
            )
            .child(
                div()
                    .h_flex()
                    .items_center()
                    .gap_1()
                    .p_1()
                    .rounded_full()
                    .border_1()
                    .border_color(theme::card_border(cx))
                    .children(DrillDim::ALL.iter().map(|dim| {
                        let active = *dim == selected;
                        let button = Button::new(dim.id())
                            .label(dim.label())
                            .small()
                            .rounded_full()
                            .custom(theme::range_pill(cx, active))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if this.dim != *dim {
                                    // Every dimension's data loads up
                                    // front, so a switch is instant.
                                    this.dim = *dim;
                                    this.treemap_hover.clear();
                                    cx.notify();
                                }
                            }));
                        if active {
                            button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                        } else {
                            button
                        }
                    })),
            )
    }

    fn render_path_row(&self, cx: &Context<Self>, steps: &[data::PathStep]) -> impl IntoElement {
        div()
            .w_full()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted(cx))
                    .child("PATH"),
            )
            .child(
                div().h_flex().items_center().gap_2().children(
                    steps
                        .iter()
                        .map(|step| theme::pill_outline(cx, step.label.clone())),
                ),
            )
    }

    /// Resolve every node and link of the Sankey to pixel geometry for the
    /// given canvas bounds. Colors and heights are precomputed because the
    /// paint closures run without theme or view access.
    fn layout_sankey(
        bounds: &Bounds<Pixels>,
        nodes: &[NodeSnap],
        links: &[LinkSnap],
        heights: &HashMap<String, f32>,
        scale: f32,
        colors: &HashMap<String, Hsla>,
    ) -> SankeyLayout {
        let columns = nodes.iter().map(|n| n.column).max().map_or(0, |m| m + 1);
        let height = f32::from(bounds.size.height);
        let width = f32::from(bounds.size.width);
        let x_of = |column: usize| {
            if columns > 1 {
                (width - NODE_WIDTH) * column as f32 / (columns - 1) as f32
            } else {
                0.0
            }
        };

        // Nodes stack from the bottom up so the largest flows sit low and
        // the thin ones stay legible at the top.
        struct NodeRect {
            x: f32,
            y: f32,
        }
        let mut rects: HashMap<&str, NodeRect> = HashMap::new();
        let mut bars = Vec::new();
        for column in 0..columns {
            let mut cursor = height;
            for node in nodes.iter().filter(|n| n.column == column) {
                let Some(&h) = heights.get(&node.id) else {
                    continue;
                };
                let Some(&color) = colors.get(&node.id) else {
                    continue;
                };
                cursor -= h;
                rects.insert(
                    node.id.as_str(),
                    NodeRect {
                        x: x_of(column),
                        y: cursor,
                    },
                );
                bars.push((
                    Bounds {
                        origin: point(
                            bounds.origin.x + px(x_of(column)),
                            bounds.origin.y + px(cursor),
                        ),
                        size: size(px(NODE_WIDTH), px(h)),
                    },
                    color,
                ));
                cursor -= NODE_GAP;
            }
        }

        // Ribbons are painted first so the bars cover their ends.
        let mut in_cursor: HashMap<&str, f32> = HashMap::new();
        let mut out_cursor: HashMap<&str, f32> = HashMap::new();
        let mut ribbons = Vec::new();
        for link in links {
            let (Some(from), Some(to)) =
                (rects.get(link.from.as_str()), rects.get(link.to.as_str()))
            else {
                continue;
            };
            let Some(&color) = colors.get(&link.to) else {
                continue;
            };
            let thickness = ((link.value as f32) * scale).max(1.5);
            let src_top = from.y + *out_cursor.get(link.from.as_str()).unwrap_or(&0.0);
            let dst_top = to.y + *in_cursor.get(link.to.as_str()).unwrap_or(&0.0);
            *out_cursor.entry(link.from.as_str()).or_insert(0.0) += thickness;
            *in_cursor.entry(link.to.as_str()).or_insert(0.0) += thickness;

            let src_x = from.x + NODE_WIDTH;
            let dst_x = to.x;
            let mid = (src_x + dst_x) / 2.0;
            let at = |x: f32, y: f32| point(bounds.origin.x + px(x), bounds.origin.y + px(y));

            let mut path = PathBuilder::fill();
            path.move_to(at(src_x, src_top));
            path.cubic_bezier_to(at(dst_x, dst_top), at(mid, src_top), at(mid, dst_top));
            path.line_to(at(dst_x, dst_top + thickness));
            path.cubic_bezier_to(
                at(src_x, src_top + thickness),
                at(mid, dst_top + thickness),
                at(mid, src_top + thickness),
            );
            path.close();
            if let Ok(ribbon) = path.build() {
                ribbons.push((ribbon, color.opacity(LINK_OPACITY)));
            }
        }

        SankeyLayout { bars, ribbons }
    }

    /// One flanking label column, aligned bar-for-bar with the canvas
    /// (same heights, same gap, anchored to the same bottom edge).
    fn render_label_column(
        cx: &App,
        sankey: &data::SankeyData,
        heights: &HashMap<String, f32>,
        column: usize,
        right_aligned: bool,
    ) -> Div {
        // A node thinner than a text line cannot carry its own label —
        // the text would spill over its neighbors. Such nodes are thin
        // precisely because they matter least.
        const MIN_LABEL_HEIGHT: f32 = 14.0;
        let labels = sankey
            .nodes
            .iter()
            .filter(|n| n.column == column)
            .rev()
            .map(|node| {
                let height = heights.get(&node.id).copied().unwrap_or(2.0);
                div()
                    .h(px(height))
                    .w_full()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .when(right_aligned, |el| el.justify_end())
                    .text_xs()
                    .text_color(theme::text_muted(cx))
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .when(height >= MIN_LABEL_HEIGHT, |el| {
                        el.child(node.label.clone())
                    })
                    .into_any_element()
            });
        let el = div()
            .w(px(LABEL_WIDTH))
            .h(px(SANKEY_HEIGHT))
            .v_flex()
            .justify_end()
            .gap(px(NODE_GAP))
            .flex_shrink_0()
            .children(labels);
        if right_aligned {
            el.pr_2()
        } else {
            el.pl_2()
        }
    }

    fn render_sankey_card(
        &self,
        cx: &Context<Self>,
        sankey: &data::SankeyData,
    ) -> impl IntoElement {
        let columns = column_count(sankey);
        let (heights, scale) = node_heights(sankey);
        let lines = line_colors(cx, sankey);
        let dominant = dominant_lines(sankey);
        // Theme colors are resolved here, before the canvas closures, which
        // run without theme access.
        let colors: HashMap<String, Hsla> = sankey
            .nodes
            .iter()
            .map(|node| {
                let color = dominant
                    .get(node.id.as_str())
                    .and_then(|line| lines.get(*line))
                    .copied()
                    .unwrap_or_else(|| theme::grey(cx));
                (node.id.clone(), color)
            })
            .collect();

        // Built before the canvas closures take ownership of the snapshots.
        let left_labels = Self::render_label_column(cx, sankey, &heights, 0, true);
        let right_labels = Self::render_label_column(cx, sankey, &heights, columns - 1, false);

        // The closures are 'static, so the geometry inputs cross over as
        // owned snapshots.
        let nodes: Vec<NodeSnap> = sankey
            .nodes
            .iter()
            .map(|n| NodeSnap {
                id: n.id.clone(),
                column: n.column,
            })
            .collect();
        let links: Vec<LinkSnap> = sankey
            .links
            .iter()
            .map(|l| LinkSnap {
                from: l.from.clone(),
                to: l.to.clone(),
                value: l.value,
            })
            .collect();

        // min_w_0 so the canvas compresses inside the h_flex on narrow
        // windows instead of clipping past the label columns.
        let canvas_el = canvas(
            move |bounds, _window, _cx| {
                Self::layout_sankey(&bounds, &nodes, &links, &heights, scale, &colors)
            },
            move |_bounds, layout, window, _cx| {
                for (ribbon, color) in &layout.ribbons {
                    window.paint_path(ribbon.clone(), *color);
                }
                for (bounds, color) in &layout.bars {
                    window.paint_quad(
                        fill(*bounds, *color).corner_radii(Corners::all(px(NODE_RADIUS))),
                    );
                }
            },
        )
        .flex_1()
        .min_w_0()
        .h(px(SANKEY_HEIGHT));

        theme::card(cx).w_full().p_5().child(
            div()
                .w_full()
                .h_flex()
                .child(left_labels)
                .child(canvas_el)
                .child(right_labels),
        )
    }

    fn render_unallocated_card(
        &self,
        cx: &Context<Self>,
        card: &data::UnallocatedCardData,
        currency: &str,
    ) -> impl IntoElement {
        let body: AnyElement = if card.largest.is_empty() {
            div()
                .text_sm()
                .text_color(theme::text_muted(cx))
                .child("Every charge this period reached a business line.")
                .into_any_element()
        } else {
            div()
                .v_flex()
                .children(card.largest.iter().map(|item| {
                    // Provider · service is the row; a description, when
                    // the row even has one, trails it.
                    let what = item
                        .service
                        .clone()
                        .unwrap_or_else(|| "untagged charge".to_string());
                    let mut label = format!("{} · {}", item.provider, what);
                    if let Some(description) = &item.description {
                        label.push_str(&format!(" · {description}"));
                    }
                    div()
                        .w_full()
                        .h_flex()
                        .justify_between()
                        .items_center()
                        .gap_4()
                        .py_1()
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme::text_primary(cx))
                                .child(label),
                        )
                        .child(
                            div()
                                .text_sm()
                                .text_color(theme::text_primary(cx))
                                .child(fmt::amount(item.amount, currency)),
                        )
                }))
                .into_any_element()
        };

        theme::card(cx)
            .w_full()
            .bg(theme::warning_bg(cx))
            .p_5()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_primary(cx))
                    .child(format!(
                        "Unallocated · {} ({:.1}%)",
                        fmt::amount(card.amount, currency),
                        card.pct
                    )),
            )
            .child(body)
            .child(
                div().h_flex().child(
                    Button::new("write-allocation-rule")
                        .label(card.action.clone())
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .on_click(|_, _, cx| {
                            crate::app::navigate_to(crate::app::CurrentView::Rules, cx)
                        }),
                ),
            )
    }

    /// The flat breakdown of the selected non-tag dimension: the MoM heat
    /// treemap by default, or one row per bucket, largest first, with a
    /// share bar against the dimension's total.
    fn render_breakdown_card(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        drilldown: &DrilldownData,
        dim: DrillDim,
        currency: &str,
    ) -> impl IntoElement {
        let rows = dim.rows(drilldown);
        let has_other = rows.iter().any(|(label, _)| label == "Other");
        let selected = self.breakdown_mode;
        let card = theme::card(cx).w_full().p_5().v_flex().gap_4().child(
            div()
                .h_flex()
                .items_center()
                .justify_between()
                .child(theme::section_title(cx, dim.title()))
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .gap_1()
                        .p_1()
                        .rounded_full()
                        .border_1()
                        .border_color(theme::card_border(cx))
                        .children(BreakdownMode::ALL.iter().map(|mode| {
                            let active = *mode == selected;
                            let button = Button::new(mode.id())
                                .label(mode.label())
                                .small()
                                .rounded_full()
                                .custom(theme::range_pill(cx, active))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.breakdown_mode != *mode {
                                        this.breakdown_mode = *mode;
                                        this.treemap_hover.clear();
                                        cx.notify();
                                    }
                                }));
                            if active {
                                button.card_outline(cx).font_weight(FontWeight::MEDIUM)
                            } else {
                                button
                            }
                        })),
                ),
        );

        if rows.is_empty() {
            return card.child(theme::caption(cx, "No usage in this period."));
        }

        match self.breakdown_mode {
            BreakdownMode::Map => {
                card.child(self.render_treemap_pane(window, cx, drilldown, dim, currency))
            }
            BreakdownMode::Table => card
                .child(
                    div()
                        .h_flex()
                        .items_center()
                        .pb_2()
                        .child(
                            theme::header_cell(cx, dim.bucket_header())
                                .flex_1()
                                .min_w_0(),
                        )
                        .child(theme::header_cell(cx, "AMOUNT").w_24().text_right())
                        .child(theme::header_cell(cx, "SHARE").w_32().px_2()),
                )
                .child(div().v_flex().children(rows.iter().map(|(label, amount)| {
                    breakdown_row(cx, label, *amount, total(rows), currency)
                })))
                .when(has_other, |el| {
                    el.child(theme::caption(cx, dim.other_caption()))
                }),
        }
    }

    /// The treemap heatmap pane: tile area is the bucket's current cost,
    /// tile color its MoM move. The wrapper maps the mouse position to a
    /// tile via the canvas's published bounds; the overlay draws the
    /// hovered-tile outline and the tooltip on top.
    fn render_treemap_pane(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
        drilldown: &DrilldownData,
        dim: DrillDim,
        currency: &str,
    ) -> impl IntoElement {
        let items = chart::top_tiles(
            treemap_items(dim.rows(drilldown), dim.prev_rows(drilldown)),
            chart::TILE_CAP,
        );
        div()
            .id("treemap-pane")
            .w_full()
            .relative()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                let x: f32 = event.position.x.into();
                let y: f32 = event.position.y.into();
                let hit = this.treemap_hover.tile_at(x, y);
                if hit != this.treemap_hover.index() {
                    this.treemap_hover.set(hit);
                    cx.notify();
                }
            }))
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if !*hovered && this.treemap_hover.index().is_some() {
                    this.treemap_hover.clear();
                    cx.notify();
                }
            }))
            .child(chart::treemap_heatmap(
                cx,
                &items,
                currency,
                // 320px at the default 16px rem.
                rems(20.0),
                &self.treemap_hover,
            ))
            .when_some(
                chart::treemap_hover_overlay(
                    cx,
                    &self.treemap_hover,
                    &items,
                    currency,
                    window.rem_size(),
                ),
                |el, overlay| el.children(overlay),
            )
    }

    /// The period's costliest resources across every account: display name
    /// (falling back to the resource id), its service, and what it cost.
    fn render_top_resources_card(
        &self,
        cx: &Context<Self>,
        resources: &[query::TopResource],
        currency: &str,
    ) -> impl IntoElement {
        let card = theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(theme::section_title(cx, "Top resources"));

        if resources.is_empty() {
            return card.child(theme::caption(
                cx,
                "No resource-level usage this period — charges without a \
                 resource id cannot be ranked.",
            ));
        }

        card.child(
            div()
                .h_flex()
                .items_center()
                .pb_2()
                .child(theme::header_cell(cx, "RESOURCE").flex_1().min_w_0())
                .child(theme::header_cell(cx, "SERVICE").w_40())
                .child(theme::header_cell(cx, "AMOUNT").w_24().text_right()),
        )
        .child(div().v_flex().children(resources.iter().map(|resource| {
            let name = resource
                .resource_name
                .clone()
                .unwrap_or_else(|| resource.resource_id.clone());
            div()
                .h_flex()
                .items_center()
                .py_2()
                .border_t_1()
                .border_color(theme::card_border(cx))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_sm()
                        .text_color(theme::text_primary(cx))
                        .child(name),
                )
                .child(
                    div()
                        .w_40()
                        .text_sm()
                        .text_color(theme::text_muted(cx))
                        .child(resource.service.clone()),
                )
                .child(
                    div()
                        .w_24()
                        .text_right()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(theme::text_primary(cx))
                        .child(fmt::amount(resource.amount, currency)),
                )
        })))
    }

    fn render_empty_state(&self, cx: &Context<Self>) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::BOLD)
                    .text_color(theme::text_primary(cx))
                    .child("Nothing to attribute yet"),
            )
            .child(div().text_sm().text_color(theme::text_muted(cx)).child(
                "Ingest some bills first — the source → service → business line \
                        flow appears once this period has charges.",
            ))
    }

    /// First-load placeholder shaped like the loaded page — the Sankey
    /// card at its fixed canvas height and a breakdown card below it — so
    /// the landing content does not jump the layout.
    fn render_loading(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .v_flex()
            .gap_6()
            .child(
                theme::card(cx)
                    .w_full()
                    .p_5()
                    .v_flex()
                    .gap_4()
                    .child(Skeleton::new().w_40().h_4())
                    .child(Skeleton::new().w_full().h(px(SANKEY_HEIGHT))),
            )
            .child(
                theme::card(cx)
                    .w_full()
                    .p_5()
                    .v_flex()
                    .gap_3()
                    .child(Skeleton::new().w_32().h_4())
                    .children((0..5).map(|_| Skeleton::new().w_full().h_4())),
            )
    }

    /// Compact banner shown above stale content when a background reload
    /// fails — the full-page error card is only for when there is nothing
    /// to show at all.
    fn render_error_banner(&self, cx: &Context<Self>, error: &str) -> impl IntoElement {
        div()
            .w_full()
            .p_3()
            .rounded_md()
            .bg(theme::danger_bg(cx))
            .text_sm()
            .text_color(theme::danger(cx))
            .child(error.to_string())
    }

    fn render_error(&self, cx: &Context<Self>, error: &str) -> impl IntoElement {
        theme::card(cx)
            .w_full()
            .p_5()
            .v_flex()
            .gap_4()
            .child(
                div()
                    .text_sm()
                    .text_color(theme::text_primary(cx))
                    .child(error.to_string()),
            )
            .child(
                div().h_flex().child(
                    Button::new("retry-load")
                        .label("Retry")
                        .custom(theme::outline_variant(cx))
                        .card_outline(cx)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.load(cx);
                        })),
                ),
            )
    }
}

impl Render for AttributionView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = if let Some(attribution) = &self.data {
            let content: AnyElement = if attribution.sankey.nodes.is_empty() {
                self.render_empty_state(cx).into_any_element()
            } else {
                let main: AnyElement = match self.dim {
                    DrillDim::Tag => div()
                        .v_flex()
                        .gap_6()
                        .child(self.render_path_row(cx, &attribution.path))
                        .child(self.render_sankey_card(cx, &attribution.sankey))
                        .child(self.render_unallocated_card(
                            cx,
                            &attribution.unallocated,
                            &attribution.currency,
                        ))
                        .into_any_element(),
                    dim => self
                        .drilldown
                        .as_ref()
                        .map(|drilldown| {
                            self.render_breakdown_card(
                                window,
                                cx,
                                drilldown,
                                dim,
                                &attribution.currency,
                            )
                            .into_any_element()
                        })
                        // The drill-down lands in the same flight as the
                        // Sankey; its absence means the load is settling.
                        .unwrap_or_else(|| self.render_loading(cx).into_any_element()),
                };
                div()
                    .v_flex()
                    .gap_6()
                    .child(main)
                    .when_some(self.drilldown.as_ref(), |el, drilldown| {
                        el.child(self.render_top_resources_card(
                            cx,
                            &drilldown.top_resources,
                            &attribution.currency,
                        ))
                    })
                    .into_any_element()
            };
            let total: f64 = attribution
                .sankey
                .nodes
                .iter()
                .filter(|n| n.column == 0)
                .map(|n| n.value)
                .sum();
            let caption_total = if attribution.sankey.nodes.is_empty() {
                None
            } else {
                Some((total, attribution.currency.as_str()))
            };
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx, caption_total))
                // A failed background reload keeps the last good data on
                // screen; the error rides above it as a banner.
                .when_some(self.error.clone(), |el, error| {
                    el.child(self.render_error_banner(cx, &error))
                })
                .child(content)
                .into_any_element()
        } else if let Some(error) = &self.error {
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx, None))
                .child(self.render_error(cx, error))
                .into_any_element()
        } else if self.loading {
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx, None))
                .child(self.render_loading(cx))
                .into_any_element()
        } else {
            div()
                .v_flex()
                .gap_6()
                .child(self.render_header(cx, None))
                .child(self.render_empty_state(cx))
                .into_any_element()
        };

        div()
            .id("attribution")
            .size_full()
            .v_flex()
            .gap_6()
            .p_8()
            .bg(theme::app_bg(cx))
            .overflow_y_scroll()
            .child(body)
    }
}

// ==================== Drill-down data ====================
//
// The Sankey's loader lives in `data.rs`; the new dimensions are small
// enough that their loader lives here, next to the view that renders them.
// Both ledger reads are keyed on one account's period, so the cross-account
// numbers the page shows are merged per account, the way the Accounts
// page's MTD column is built.

/// How many rows the Top resources card shows.
const TOP_RESOURCE_COUNT: usize = 10;

/// The numbers behind the Service / Region / Category dimensions and the
/// Top resources card — current period across every account, plus the
/// previous period's breakdowns as the treemap's heat base.
struct DrilldownData {
    by_service: Vec<(String, f64)>,
    by_region: Vec<(String, f64)>,
    by_category: Vec<(String, f64)>,
    prev_by_service: Vec<(String, f64)>,
    prev_by_region: Vec<(String, f64)>,
    prev_by_category: Vec<(String, f64)>,
    top_resources: Vec<query::TopResource>,
}

/// Load the drill-down data. Blocking; the view wraps it in the same
/// `smol::unblock` as the Sankey load.
fn load_drilldown() -> Result<DrilldownData> {
    let period = BillingPeriod::containing(Utc::now());
    let previous = period.previous();
    let mut by_service = Vec::new();
    let mut by_region = Vec::new();
    let mut by_category = Vec::new();
    let mut prev_by_service = Vec::new();
    let mut prev_by_region = Vec::new();
    let mut prev_by_category = Vec::new();
    let mut top_resources = Vec::new();
    for account in db::get_all_accounts()? {
        let key = ingest::period_key(&account, &period);
        merge_buckets(
            &mut by_service,
            query::breakdown_by(&key, BreakdownDim::Service)?,
        );
        merge_buckets(
            &mut by_region,
            query::breakdown_by(&key, BreakdownDim::Region)?,
        );
        merge_buckets(
            &mut by_category,
            query::breakdown_by(&key, BreakdownDim::ServiceCategory)?,
        );
        let prev_key = ingest::period_key(&account, &previous);
        merge_buckets(
            &mut prev_by_service,
            query::breakdown_by(&prev_key, BreakdownDim::Service)?,
        );
        merge_buckets(
            &mut prev_by_region,
            query::breakdown_by(&prev_key, BreakdownDim::Region)?,
        );
        merge_buckets(
            &mut prev_by_category,
            query::breakdown_by(&prev_key, BreakdownDim::ServiceCategory)?,
        );
        top_resources.extend(query::top_resources(&key, TOP_RESOURCE_COUNT)?);
    }
    // Largest first in every dimension, as each per-account breakdown was.
    for rows in [&mut by_service, &mut by_region, &mut by_category] {
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
    }
    top_resources.sort_by(|a, b| b.amount.total_cmp(&a.amount));
    top_resources.truncate(TOP_RESOURCE_COUNT);

    Ok(DrilldownData {
        by_service,
        by_region,
        by_category,
        prev_by_service,
        prev_by_region,
        prev_by_category,
        top_resources,
    })
}

/// Add one account's breakdown into the cross-account one, bucket by
/// bucket — the same idiom the Sankey's totals use in `data.rs`.
fn merge_buckets(into: &mut Vec<(String, f64)>, rows: Vec<(String, f64)>) {
    for (label, amount) in rows {
        match into.iter_mut().find(|(existing, _)| *existing == label) {
            Some((_, total)) => *total += amount,
            None => into.push((label, amount)),
        }
    }
}

/// A breakdown's grand total, the share bars' denominator.
fn total(rows: &[(String, f64)]) -> f64 {
    rows.iter().map(|(_, amount)| amount).sum()
}

/// Pair the current breakdown with the previous period's so every treemap
/// tile carries its own month-over-month base; a bucket with no previous
/// period gets zero, which the heat map reads as "new".
fn treemap_items(current: &[(String, f64)], previous: &[(String, f64)]) -> Vec<chart::TreemapItem> {
    let previous: HashMap<&str, f64> = previous
        .iter()
        .map(|(label, amount)| (label.as_str(), *amount))
        .collect();
    current
        .iter()
        .map(|(label, amount)| {
            chart::TreemapItem::new(
                label.clone(),
                *amount,
                previous.get(label.as_str()).copied().unwrap_or(0.0),
            )
        })
        .collect()
}

/// One breakdown row: bucket, amount, and a share-of-dimension bar. The
/// 'Other' bucket — charges with no value for the dimension — reads muted
/// so it is not mistaken for a real one.
fn breakdown_row(cx: &App, label: &str, amount: f64, total: f64, currency: &str) -> Div {
    let share = if total > 0.0 { amount / total } else { 0.0 };
    let label_color = if label == "Other" {
        theme::text_muted(cx)
    } else {
        theme::text_primary(cx)
    };
    div()
        .h_flex()
        .items_center()
        .py_2()
        .border_t_1()
        .border_color(theme::card_border(cx))
        .child(
            div()
                .flex_1()
                // min_w_0 so a long bucket name truncates instead of
                // pushing the amount columns out of the card.
                .min_w_0()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_sm()
                .text_color(label_color)
                .child(label.to_string()),
        )
        .child(
            div()
                .w_24()
                .text_right()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text_primary(cx))
                .child(fmt::amount(amount, currency)),
        )
        .child(
            div().w_32().px_2().child(
                div()
                    .w_full()
                    .h_2()
                    .rounded_full()
                    .bg(theme::sidebar_bg(cx))
                    .child(
                        div()
                            .h_full()
                            .w(relative(share as f32))
                            .rounded_full()
                            .bg(theme::accent(cx)),
                    ),
            ),
        )
}
