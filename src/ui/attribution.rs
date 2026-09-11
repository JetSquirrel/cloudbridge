//! Attribution View — how every dollar travels from the source that billed
//! it to the business line that caused it.

use std::collections::HashMap;

use gpui_kit::component::{button::*, StyledExt};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;

use super::{data, fmt, theme};
use crate::ui::theme::CardOutline as _;

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

/// Attribution View
pub struct AttributionView {
    /// The loaded page data, once the background load has landed.
    data: Option<data::AttributionData>,
    /// Why the last load failed, if it did.
    error: Option<String>,
    /// Whether a load is in flight.
    loading: bool,
    /// Bumped on every load; a completion stamped with an older generation
    /// is discarded so a slow first load cannot clobber a newer result.
    load_generation: u64,
}

impl AttributionView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            data: None,
            error: None,
            loading: true,
            load_generation: 0,
        };
        view.load(cx);
        view
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

    /// Load the page's data off the UI thread; the ledger query is
    /// blocking.
    fn load(&mut self, cx: &mut Context<Self>) {
        self.loading = true;
        self.load_generation += 1;
        let generation = self.load_generation;
        cx.spawn(async move |this, cx| {
            let outcome = smol::unblock(data::load_attribution).await;
            this.update(cx, |view, cx| {
                if view.load_generation != generation {
                    return;
                }
                match outcome {
                    Ok(loaded) => {
                        view.data = Some(loaded);
                        view.error = None;
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

    fn render_header(&self, cx: &Context<Self>, total: Option<(f64, &str)>) -> impl IntoElement {
        let caption = total.map(|(amount, currency)| {
            format!("{} this billing period", fmt::amount(amount, currency))
        });
        div().w_full().h_flex().items_center().child(
            div()
                .v_flex()
                .gap_1()
                .child(theme::page_title(cx, "Attribution"))
                .when_some(caption, |el, text| el.child(theme::caption(cx, text))),
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

    fn render_loading(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .w_full()
            .p_8()
            .flex()
            .justify_center()
            .text_sm()
            .text_color(theme::text_muted(cx))
            .child("Loading attribution…")
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
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let body: AnyElement = if let Some(attribution) = &self.data {
            let content: AnyElement = if attribution.sankey.nodes.is_empty() {
                self.render_empty_state(cx).into_any_element()
            } else {
                div()
                    .v_flex()
                    .gap_6()
                    .child(self.render_path_row(cx, &attribution.path))
                    .child(self.render_sankey_card(cx, &attribution.sankey))
                    .child(self.render_unallocated_card(
                        cx,
                        &attribution.unallocated,
                        &attribution.currency,
                    ))
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
