//! The spend chart shared by the Overview and Account detail pages.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::component::*;
use gpui_kit::*;

use super::data::ChartPoint;
use super::{fmt, theme};

/// Gridlines sit behind the data series, faint enough to read as context
/// rather than as part of it.
const GRIDLINE_OPACITY: f32 = 0.6;
/// The area fill is a whisper of the accent so the line keeps visual
/// priority.
const AREA_FILL_OPACITY: f32 = 0.12;
/// Vertical breathing room above and below the data range, as a fraction
/// of that range, so the line never touches the canvas edge.
const Y_PAD_FRAC: f64 = 0.15;
/// The hover guide line is fainter than the gridlines: it is transient
/// chrome, not structure, so it sits one step further back.
const GUIDE_OPACITY: f32 = 0.4;

/// The y-range the canvas maps data onto: the series min/max widened by
/// [`Y_PAD_FRAC`] on both ends, falling back to 0..=1 for an empty or flat
/// series. Overlays that land on the chart's scale (the Overview's
/// benchmark line) derive from this same range.
pub(crate) fn padded_y_range(values: impl IntoIterator<Item = f64>) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for v in values {
        min = min.min(v);
        max = max.max(v);
    }
    if !min.is_finite() || !max.is_finite() || min >= max {
        min = 0.0;
        max = 1.0;
    }
    let pad = (max - min) * Y_PAD_FRAC;
    (min - pad, max + pad)
}

/// The Overview spend chart: a smooth area chart of actual usage in the
/// accent color over a dashed olive 7-day baseline, with faint horizontal
/// gridlines and no axis labels. An empty `baseline` (the 12-month range)
/// simply draws no dashed series. `height` is rem-based so the chart
/// scales with the user's font size.
///
/// Hover interactivity lives in the caller: each frame the prepaint writes
/// the canvas bounds and the actual series' point coordinates (window
/// space) into the shared cells, which the caller uses to map the mouse
/// position to a point and to position the guide line, dot, and tooltip.
pub fn spend_area_chart(
    cx: &App,
    actual: &[ChartPoint],
    baseline: &[ChartPoint],
    height: Rems,
    points_cell: Rc<RefCell<Vec<(f32, f32)>>>,
    bounds_cell: Rc<RefCell<Option<Bounds<Pixels>>>>,
) -> impl IntoElement {
    let actual: Vec<f64> = actual.iter().map(|p| p.amount).collect();
    let baseline: Vec<f64> = baseline.iter().map(|p| p.amount).collect();

    let grid_color = theme::card_border(cx).opacity(GRIDLINE_OPACITY);
    let fill_color = theme::accent(cx).opacity(AREA_FILL_OPACITY);
    let baseline_color = theme::olive(cx);
    let line_color = theme::accent(cx);

    canvas(
        move |bounds, _window, _cx| {
            let origin_x: f32 = bounds.origin.x.into();
            let origin_y: f32 = bounds.origin.y.into();
            let w: f32 = bounds.size.width.into();
            let h: f32 = bounds.size.height.into();
            *bounds_cell.borrow_mut() = Some(bounds);

            let (min, max) = padded_y_range(actual.iter().chain(baseline.iter()).copied());

            let to_points = |data: &[f64]| -> Vec<(f32, f32)> {
                let denom = (data.len().saturating_sub(1)).max(1) as f32;
                data.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let x = origin_x + w * (i as f32 / denom);
                        let frac = ((v - min) / (max - min)) as f32;
                        let y = origin_y + h * (1.0 - frac.clamp(0.0, 1.0));
                        (x, y)
                    })
                    .collect()
            };

            let actual_points = to_points(&actual);
            *points_cell.borrow_mut() = actual_points.clone();

            (
                actual_points,
                to_points(&baseline),
                [origin_x, origin_y, origin_x + w, origin_y + h],
            )
        },
        move |_bounds, (actual, baseline, rect), window, _cx| {
            let [left, top, right, bottom] = rect;
            // Line widths derive from the rem scale, like the overlay.
            let rem = window.rem_size();

            // Faint horizontal gridlines.
            for i in 0..=3 {
                let y = top + (bottom - top) * (i as f32 / 3.0);
                let mut grid = PathBuilder::stroke(rems(0.0625).to_pixels(rem));
                grid.move_to(point(px(left), px(y)));
                grid.line_to(point(px(right), px(y)));
                if let Ok(path) = grid.build() {
                    window.paint_path(path, grid_color);
                }
            }

            // Actual spend: translucent accent fill under the line.
            if actual.len() >= 2 {
                let mut fill = PathBuilder::fill();
                fill.move_to(point(px(actual[0].0), px(bottom)));
                trace_smooth(&mut fill, &actual);
                fill.line_to(point(px(actual[actual.len() - 1].0), px(bottom)));
                fill.close();
                if let Ok(path) = fill.build() {
                    window.paint_path(path, fill_color);
                }
            }

            // 7-day baseline: dashed olive line.
            if baseline.len() >= 2 {
                let mut dashed = PathBuilder::stroke(rems(0.09375).to_pixels(rem))
                    .dash_array(&[px(4.0), px(4.0)]);
                trace_smooth(&mut dashed, &baseline);
                if let Ok(path) = dashed.build() {
                    window.paint_path(path, baseline_color);
                }
            }

            // Actual spend: solid accent line.
            if actual.len() >= 2 {
                let mut line = PathBuilder::stroke(rems(0.125).to_pixels(rem));
                trace_smooth(&mut line, &actual);
                if let Ok(path) = line.build() {
                    window.paint_path(path, line_color);
                }
            }
        },
    )
    .w_full()
    .h(height)
}

/// Append a Catmull-Rom-smoothed polyline through `points` to the path.
fn trace_smooth(path: &mut PathBuilder, points: &[(f32, f32)]) {
    if points.len() < 2 {
        return;
    }

    let clamped = |j: isize| -> (f32, f32) {
        let j = j.clamp(0, points.len() as isize - 1);
        points[j as usize]
    };

    path.move_to(point(px(points[0].0), px(points[0].1)));
    for i in 1..points.len() {
        let p_prev = clamped(i as isize - 2);
        let p_from = points[i - 1];
        let p_to = points[i];
        let p_next = clamped(i as isize + 1);
        path.cubic_bezier_to(
            point(px(p_to.0), px(p_to.1)),
            point(
                px(p_from.0 + (p_to.0 - p_prev.0) / 6.0),
                px(p_from.1 + (p_to.1 - p_prev.1) / 6.0),
            ),
            point(
                px(p_to.0 - (p_next.0 - p_from.0) / 6.0),
                px(p_to.1 - (p_next.1 - p_from.1) / 6.0),
            ),
        );
    }
}

// ==================== Hover interactivity ====================

/// Hover state for a [`spend_area_chart`]: which point the mouse is on,
/// plus the geometry cells the canvas rewrites every frame. One per chart;
/// the owning view keeps it and clears it whenever the underlying data
/// reloads (a stale index would tag the wrong point).
///
/// The mouse listeners stay in the owning view — they need its
/// `cx.listener` type — but each is a five-liner over [`ChartHover::nearest`]
/// and [`ChartHover::set`].
pub struct ChartHover {
    /// The point under the mouse, if any.
    index: Option<usize>,
    /// The actual series' point coordinates in window space.
    points: Rc<RefCell<Vec<(f32, f32)>>>,
    /// The chart canvas bounds in window space.
    bounds: Rc<RefCell<Option<Bounds<Pixels>>>>,
}

impl ChartHover {
    pub fn new() -> Self {
        Self {
            index: None,
            points: Rc::new(RefCell::new(Vec::new())),
            bounds: Rc::new(RefCell::new(None)),
        }
    }

    /// Forget the hovered point; call when the chart's data reloads.
    pub fn clear(&mut self) {
        self.index = None;
    }

    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn set(&mut self, index: Option<usize>) {
        self.index = index;
    }

    /// The cell a [`spend_area_chart`] writes its point coordinates into.
    pub fn points_cell(&self) -> Rc<RefCell<Vec<(f32, f32)>>> {
        self.points.clone()
    }

    /// The cell a [`spend_area_chart`] writes its canvas bounds into.
    pub fn bounds_cell(&self) -> Rc<RefCell<Option<Bounds<Pixels>>>> {
        self.bounds.clone()
    }

    /// The point nearest to a window-space x, if the chart has points.
    pub fn nearest(&self, x: f32) -> Option<usize> {
        self.points
            .borrow()
            .iter()
            .enumerate()
            .min_by(|(_, (ax, _)), (_, (bx, _))| (ax - x).abs().total_cmp(&(bx - x).abs()))
            .map(|(i, _)| i)
    }
}

impl Default for ChartHover {
    fn default() -> Self {
        Self::new()
    }
}

/// Guide line, dot, and tooltip for the chart point under the mouse.
/// Positions come from the cells the canvas wrote on the previous
/// frame — a one-frame lag that is imperceptible in practice. The raw
/// `px(...)` values here are measured runtime geometry (the coding guide's
/// accepted exception); every size and offset derives from the rem scale
/// so the overlay zooms with the base font.
pub fn hover_overlay(
    cx: &App,
    hover: &ChartHover,
    points: &[ChartPoint],
    currency: &str,
    rem: Pixels,
) -> Option<Vec<AnyElement>> {
    let index = hover.index()?;
    let bounds = (*hover.bounds.borrow())?;
    let (x, y) = *hover.points.borrow().get(index)?;
    let point = points.get(index)?;

    // 10px dot, 150px tooltip at the default 16px rem.
    let dot: f32 = rems(0.625).to_pixels(rem).into();
    let tip_w: f32 = rems(9.375).to_pixels(rem).into();

    let origin_x: f32 = bounds.origin.x.into();
    let origin_y: f32 = bounds.origin.y.into();
    let width: f32 = bounds.size.width.into();
    let rel_x = x - origin_x;
    let rel_y = y - origin_y;

    // Clamped so the tooltip never leaves the chart.
    let tip_left = (rel_x - tip_w / 2.0).clamp(0.0, (width - tip_w).max(0.0));
    // Above the point unless there is no headroom; 56px / 14px at the
    // default rem.
    let headroom: f32 = rems(3.5).to_pixels(rem).into();
    let below: f32 = rems(0.875).to_pixels(rem).into();
    let buf: f32 = rems(0.25).to_pixels(rem).into();
    let tip_top = if rel_y > headroom + buf {
        rel_y - headroom
    } else {
        rel_y + below
    };

    Some(vec![
        div()
            .absolute()
            .left(px(rel_x))
            .top_0()
            .bottom_0()
            // 1px hairline guide: a physical-pixel boundary.
            .w(px(1.0))
            .bg(theme::text_muted(cx).opacity(GUIDE_OPACITY))
            .into_any_element(),
        div()
            .absolute()
            .left(px(rel_x - dot / 2.0))
            .top(px(rel_y - dot / 2.0))
            .size(px(dot))
            .rounded_full()
            .bg(theme::accent(cx))
            .border_2()
            .border_color(theme::card_bg(cx))
            .into_any_element(),
        theme::card(cx)
            .absolute()
            .left(px(tip_left))
            .top(px(tip_top))
            .w(px(tip_w))
            .px_2()
            .py_1()
            .shadow_md()
            .v_flex()
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted(cx))
                    .child(point.label.clone()),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_primary(cx))
                    .child(fmt::amount(point.amount, currency)),
            )
            .into_any_element(),
    ])
}

// ==================== Treemap heatmap ====================
//
// The Attribution page's composition view, borrowed from Wealthfolio's
// composition chart: tile area is the bucket's current-period cost, tile
// color is its month-over-month move on a saturating ramp. This is a cost
// app, so the ramp is inverted against a portfolio's: more spend heats
// toward the accent (the movers table's attention color for a positive
// delta), less spend toward the olive (the theme's positive).

/// The largest buckets get their own tile; the tail folds into one
/// "<N> more" tile. Past this count the thin tiles are unreadable.
pub const TILE_CAP: usize = 11;

/// Heat-ramp constant: a ±50% month-over-month move maps to t = 0.5,
/// halfway to the saturated end of the ramp.
const RAMP_K: f64 = 0.5;

/// Breathing room between tiles, in px — canvas-mirroring geometry, like
/// the Sankey's gaps, so it tracks the pixel-exact layout rather than the
/// font scale.
const TILE_GAP: f32 = 1.0;
const TILE_RADIUS: f32 = 3.0;
/// Inset of a tile's label from its top-left corner.
const TILE_PAD: f32 = 6.0;

/// One treemap tile: a breakdown bucket of the current period, with the
/// same bucket's previous-period total for the heat color.
#[derive(Clone, Debug)]
pub struct TreemapItem {
    pub label: String,
    /// Current-period cost; the tile's area is proportional to it.
    pub amount: f64,
    /// Previous-period cost; zero marks a bucket with no base to compare
    /// against (a new bucket).
    pub previous: f64,
}

impl TreemapItem {
    pub fn new(label: impl Into<String>, amount: f64, previous: f64) -> Self {
        Self {
            label: label.into(),
            amount,
            previous,
        }
    }

    /// Month-over-month change ratio; `None` when there is no
    /// previous-period base (a new bucket).
    pub fn change(&self) -> Option<f64> {
        (self.previous > 0.0).then(|| (self.amount - self.previous) / self.previous)
    }
}

/// The `cap` largest buckets as tiles, with the tail folded into one
/// "<N> more" tile. Both periods are summed over the tail so its heat is
/// as honest as a named bucket's.
pub fn top_tiles(mut items: Vec<TreemapItem>, cap: usize) -> Vec<TreemapItem> {
    items.sort_by(|a, b| b.amount.total_cmp(&a.amount));
    if items.len() <= cap {
        return items;
    }
    let tail = items.split_off(cap);
    let (amount, previous) = tail.iter().fold((0.0, 0.0), |(a, p), item| {
        (a + item.amount, p + item.previous)
    });
    items.push(TreemapItem::new(
        format!("{} more", tail.len()),
        amount,
        previous,
    ));
    items
}

/// Saturating ramp t = |m| / (|m| + k): rises quickly for small moves and
/// flattens as they grow, so a doubling and a tenfold move both read as
/// extreme without one blowing out the scale.
pub fn heat_t(m: f64) -> f64 {
    let m = m.abs();
    m / (m + RAMP_K)
}

/// Mix two colors in RGB space (hue lerps swing through unrelated colors
/// when one end is a desaturated neutral).
pub fn mix_color(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let a = Rgba::from(a);
    let b = Rgba::from(b);
    Hsla::from(Rgba {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    })
}

/// A tile's heat color from its MoM change: cost up ramps from the neutral
/// grey toward `up`, cost down toward `down`, ~zero stays neutral, and a
/// bucket with no previous base takes `new`. Pure over the resolved theme
/// colors so the tests can drive it without GPUI.
pub fn heat_color(change: Option<f64>, up: Hsla, down: Hsla, neutral: Hsla, new: Hsla) -> Hsla {
    match change {
        None => new,
        Some(m) if m > 0.0 => mix_color(neutral, up, heat_t(m) as f32),
        Some(m) => mix_color(neutral, down, heat_t(m) as f32),
    }
}

/// Luminance-aware label color for a tile background: `dark` on light
/// tiles, `light` on dark ones.
pub fn text_on(bg: Hsla, dark: Hsla, light: Hsla) -> Hsla {
    let rgb = Rgba::from(bg);
    let luminance = 0.2126 * rgb.r + 0.7152 * rgb.g + 0.0722 * rgb.b;
    if luminance > 0.55 {
        dark
    } else {
        light
    }
}

/// Squarified treemap layout: one `[x, y, w, h]` rect per value, in input
/// order, packed into `width`×`height` with areas proportional to the
/// values. Pure geometry — the canvas resolves it to window space and the
/// tests cover it without GPUI.
pub fn squarify(values: &[f64], width: f32, height: f32) -> Vec<[f32; 4]> {
    let mut rects = vec![[0.0; 4]; values.len()];
    let total: f64 = values.iter().sum();
    if values.is_empty() || total <= 0.0 || width <= 0.0 || height <= 0.0 {
        return rects;
    }
    let scale = width as f64 * height as f64 / total;
    // Biggest first; squarified layouts degenerate otherwise.
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&a, &b| values[b].total_cmp(&values[a]));
    let areas: Vec<f64> = order.iter().map(|&i| values[i].max(0.0) * scale).collect();

    let mut rect = Remaining {
        x: 0.0,
        y: 0.0,
        w: width as f64,
        h: height as f64,
    };
    let mut row: Vec<usize> = Vec::new();
    let (mut row_sum, mut row_min, mut row_max) = (0.0f64, f64::INFINITY, 0.0f64);

    // The worst aspect ratio a row would produce laid along `side`: with
    // the row's areas summing to `sum`, the band is `sum / side` thick and
    // each tile extends `area * side / sum` along the side.
    let worst = |sum: f64, min: f64, max: f64, side: f64| -> f64 {
        let s2 = side * side;
        ((s2 * max) / (sum * sum)).max((sum * sum) / (s2 * min))
    };

    for i in 0..areas.len() {
        let area = areas[i];
        let side = rect.w.min(rect.h);
        // Add to the current row while that improves (or starts) it; a
        // zero-length side means the rect is spent — the remaining tiles
        // collapse onto its edge.
        if !row.is_empty()
            && side > 0.0
            && worst(row_sum + area, row_min.min(area), row_max.max(area), side)
                > worst(row_sum, row_min, row_max, side)
        {
            lay_row(&row, &areas, &order, &mut rects, &mut rect);
            row.clear();
            row_sum = 0.0;
            row_min = f64::INFINITY;
            row_max = 0.0;
        }
        row.push(i);
        row_sum += area;
        row_min = row_min.min(area);
        row_max = row_max.max(area);
    }
    if !row.is_empty() {
        lay_row(&row, &areas, &order, &mut rects, &mut rect);
    }
    rects
}

/// The rect still to fill, advanced as each row is laid out.
struct Remaining {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Lay one row out as a band across the remaining rect's short side and
/// advance the remaining rect past it.
fn lay_row(
    row: &[usize],
    areas: &[f64],
    order: &[usize],
    rects: &mut [[f32; 4]],
    rect: &mut Remaining,
) {
    let sum: f64 = row.iter().map(|&i| areas[i]).sum();
    if rect.w <= rect.h {
        // Short side is the width: a horizontal band across the top.
        let band = if rect.w > 0.0 { sum / rect.w } else { 0.0 };
        let mut cursor = rect.x;
        for &i in row {
            let item_w = if band > 0.0 { areas[i] / band } else { 0.0 };
            rects[order[i]] = [cursor as f32, rect.y as f32, item_w as f32, band as f32];
            cursor += item_w;
        }
        rect.y += band;
        rect.h = (rect.h - band).max(0.0);
    } else {
        // Short side is the height: a vertical band down the left.
        let band = if rect.h > 0.0 { sum / rect.h } else { 0.0 };
        let mut cursor = rect.y;
        for &i in row {
            let item_h = if band > 0.0 { areas[i] / band } else { 0.0 };
            rects[order[i]] = [rect.x as f32, cursor as f32, band as f32, item_h as f32];
            cursor += item_h;
        }
        rect.x += band;
        rect.w = (rect.w - band).max(0.0);
    }
}

/// Prepaint state for the treemap canvas: every tile quad and label,
/// resolved to window space.
struct TreemapLayout {
    tiles: Vec<(Bounds<Pixels>, Hsla)>,
    labels: Vec<(ShapedLine, Point<Pixels>, Pixels)>,
}

/// The Attribution treemap heatmap: one tile per bucket, area ∝ current
/// cost, color from the MoM heat ramp. Labels are painted inside tiles big
/// enough to carry them (name, plus the amount when a second line fits);
/// every tile's exact numbers live in the hover tooltip.
///
/// Hover interactivity mirrors [`spend_area_chart`]: the prepaint writes
/// the tile bounds (window space) into the shared cell each frame, which
/// the caller uses to hit-test the mouse and to position the tooltip.
pub fn treemap_heatmap(
    cx: &App,
    items: &[TreemapItem],
    currency: &str,
    height: Rems,
    hover: &TreemapHover,
) -> impl IntoElement {
    // Cost up takes the accent — the movers table's attention color for a
    // positive delta; cost down takes the olive, the theme's positive.
    // New buckets take the warning yellow so they never read as a neutral
    // zero.
    let up = theme::accent(cx);
    let down = theme::olive(cx);
    let neutral = theme::grey(cx);
    let new = theme::warning_text(cx);
    let dark = theme::text_primary(cx);
    let light = theme::on_accent(cx);

    // The closures are 'static, so the tiles cross over as owned values.
    let tiles: Vec<TreemapItem> = items.to_vec();
    let currency = currency.to_string();
    let cells = hover.tiles_cell();

    canvas(
        move |bounds, window, _cx| {
            let w: f32 = bounds.size.width.into();
            let h: f32 = bounds.size.height.into();
            let values: Vec<f64> = tiles.iter().map(|t| t.amount).collect();
            let rects = squarify(&values, w, h);

            // ~text_xs at the current density.
            let font_size = rems(0.6875).to_pixels(window.rem_size());
            let line_height = font_size * 1.4;

            let mut tile_bounds = Vec::with_capacity(rects.len());
            let mut layout = TreemapLayout {
                tiles: Vec::with_capacity(rects.len()),
                labels: Vec::new(),
            };
            for (item, [x, y, tw, th]) in tiles.iter().zip(rects.iter()) {
                let rect = Bounds {
                    origin: point(
                        bounds.origin.x + px(*x + TILE_GAP),
                        bounds.origin.y + px(*y + TILE_GAP),
                    ),
                    size: size(
                        px((tw - 2.0 * TILE_GAP).max(0.0)),
                        px((th - 2.0 * TILE_GAP).max(0.0)),
                    ),
                };
                tile_bounds.push(rect);
                let color = heat_color(item.change(), up, down, neutral, new);
                layout.tiles.push((rect, color));

                // Labels: shaped here, painted below; a tile too small for
                // its text carries none rather than spilling over its
                // neighbors (the Sankey's thin-node rule).
                let inner_w: f32 = (f32::from(rect.size.width) - 2.0 * TILE_PAD).max(0.0);
                let inner_h: f32 = (f32::from(rect.size.height) - 2.0 * TILE_PAD).max(0.0);
                let text_color = text_on(color, dark, light);
                let name = SharedString::from(item.label.clone());
                let amount = SharedString::from(fmt::amount(item.amount, &currency));
                let name_line = shape_tile_label(&name, font_size, text_color, window);
                if inner_h >= f32::from(line_height) && f32::from(name_line.width()) <= inner_w {
                    let origin = point(rect.origin.x + px(TILE_PAD), rect.origin.y + px(TILE_PAD));
                    layout.labels.push((name_line, origin, line_height));
                    let amount_line = shape_tile_label(&amount, font_size, text_color, window);
                    if inner_h >= 2.0 * f32::from(line_height)
                        && f32::from(amount_line.width()) <= inner_w
                    {
                        let origin = point(
                            rect.origin.x + px(TILE_PAD),
                            rect.origin.y + px(TILE_PAD) + line_height,
                        );
                        layout.labels.push((amount_line, origin, line_height));
                    }
                }
            }
            *cells.borrow_mut() = tile_bounds;
            layout
        },
        move |_bounds, layout, window, cx| {
            for (bounds, color) in &layout.tiles {
                window
                    .paint_quad(fill(*bounds, *color).corner_radii(Corners::all(px(TILE_RADIUS))));
            }
            for (line, origin, line_height) in &layout.labels {
                let _ = line.paint(*origin, *line_height, TextAlign::Left, None, window, cx);
            }
        },
    )
    .w_full()
    .h(height)
}

/// Shape one line of tile text with the window's current text style.
fn shape_tile_label(
    text: &SharedString,
    font_size: Pixels,
    color: Hsla,
    window: &mut Window,
) -> ShapedLine {
    let run = TextRun {
        len: text.len(),
        font: window.text_style().font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    };
    window
        .text_system()
        .shape_line(text.clone(), font_size, &[run], None)
}

/// Hover state for a [`treemap_heatmap`]: which tile the mouse is on, plus
/// the tile-bounds cell the canvas rewrites every frame. One per treemap;
/// the owning view keeps it and clears it whenever the underlying data
/// reloads or the dimension switches (stale bounds would tag the wrong
/// tile).
pub struct TreemapHover {
    /// The tile under the mouse, if any.
    index: Option<usize>,
    /// The tile bounds in window space, in the tiles' data order.
    tiles: Rc<RefCell<Vec<Bounds<Pixels>>>>,
}

impl TreemapHover {
    pub fn new() -> Self {
        Self {
            index: None,
            tiles: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Forget the hovered tile; call when the treemap's data changes.
    pub fn clear(&mut self) {
        self.index = None;
    }

    pub fn index(&self) -> Option<usize> {
        self.index
    }

    pub fn set(&mut self, index: Option<usize>) {
        self.index = index;
    }

    /// The cell a [`treemap_heatmap`] writes its tile bounds into.
    pub fn tiles_cell(&self) -> Rc<RefCell<Vec<Bounds<Pixels>>>> {
        self.tiles.clone()
    }

    /// The tile containing a window-space point, if any.
    pub fn tile_at(&self, x: f32, y: f32) -> Option<usize> {
        let position = point(px(x), px(y));
        self.tiles
            .borrow()
            .iter()
            .position(|b| b.contains(&position))
    }
}

impl Default for TreemapHover {
    fn default() -> Self {
        Self::new()
    }
}

/// Hovered-tile outline plus tooltip with the tile's exact amount and MoM
/// move. Positions come from the cell the canvas wrote on the previous
/// frame — the same one-frame-lag idiom as [`hover_overlay`].
pub fn treemap_hover_overlay(
    cx: &App,
    hover: &TreemapHover,
    items: &[TreemapItem],
    currency: &str,
    rem: Pixels,
) -> Option<Vec<AnyElement>> {
    let index = hover.index()?;
    let tiles = hover.tiles.borrow();
    let tile = *tiles.get(index)?;
    let item = items.get(index)?;

    // The canvas extent, derived from the union of the tiles, to clamp the
    // tooltip inside the chart.
    let canvas_left = tiles
        .iter()
        .map(|b| f32::from(b.origin.x))
        .fold(f32::INFINITY, f32::min);
    let canvas_top = tiles
        .iter()
        .map(|b| f32::from(b.origin.y))
        .fold(f32::INFINITY, f32::min);
    let canvas_right = tiles
        .iter()
        .map(|b| f32::from(b.origin.x) + f32::from(b.size.width))
        .fold(f32::NEG_INFINITY, f32::max);
    if !canvas_left.is_finite() || !canvas_right.is_finite() {
        return None;
    }

    let tip_w: f32 = rems(11.0).to_pixels(rem).into();
    let tile_left = f32::from(tile.origin.x) - canvas_left;
    let tile_top = f32::from(tile.origin.y) - canvas_top;
    let tile_w: f32 = tile.size.width.into();
    let tile_h: f32 = tile.size.height.into();
    let canvas_w = canvas_right - canvas_left;

    // Above the tile unless there is no headroom; 48px / 12px / 4px at
    // the default rem.
    let headroom: f32 = rems(3.0).to_pixels(rem).into();
    let below: f32 = rems(0.75).to_pixels(rem).into();
    let buf: f32 = rems(0.25).to_pixels(rem).into();
    let tip_left = (tile_left + tile_w / 2.0 - tip_w / 2.0).clamp(0.0, (canvas_w - tip_w).max(0.0));
    let tip_top = if tile_top > headroom + buf {
        tile_top - headroom
    } else {
        tile_top + tile_h + below
    };

    let mom = match item.change() {
        Some(m) => {
            let color = if m > 0.0 {
                theme::accent(cx)
            } else if m < 0.0 {
                theme::olive(cx)
            } else {
                theme::text_muted(cx)
            };
            div()
                .text_xs()
                .text_color(color)
                .child(format!("{} vs last month", fmt::change_pct(m * 100.0)))
        }
        None => div()
            .text_xs()
            .text_color(theme::warning_text(cx))
            .child("New this period".to_string()),
    };

    Some(vec![
        // Hovered-tile outline: the heat color stays put, a hairline ring
        // marks the selection.
        div()
            .absolute()
            .left(px(tile_left))
            .top(px(tile_top))
            .w(px(tile_w))
            .h(px(tile_h))
            .rounded(px(TILE_RADIUS))
            .border_2()
            .border_color(theme::text_primary(cx))
            .into_any_element(),
        theme::card(cx)
            .absolute()
            .left(px(tip_left))
            .top(px(tip_top))
            .w(px(tip_w))
            .px_2()
            .py_1()
            .shadow_md()
            .v_flex()
            .child(
                div()
                    .text_xs()
                    .text_color(theme::text_muted(cx))
                    .child(item.label.clone()),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_primary(cx))
                    .child(fmt::amount(item.amount, currency)),
            )
            .child(mom)
            .into_any_element(),
    ])
}

#[cfg(test)]
mod tests {
    // Explicit imports only: a `use super::*` glob re-pulls `gpui_kit::*`
    // into the test expansion and tips the crate over the default macro
    // recursion limit.
    use super::{heat_color, heat_t, mix_color, squarify, text_on, top_tiles, TreemapItem};
    use gpui_kit::{Hsla, Rgba};

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.01
    }

    /// Edge-touching tiles are not an overlap; the epsilon absorbs the
    /// f32 rounding where one tile's edge meets the next's.
    fn overlaps(a: &[f32; 4], b: &[f32; 4]) -> bool {
        const EPS: f32 = 0.01;
        a[0] + EPS < b[0] + b[2]
            && b[0] + EPS < a[0] + a[2]
            && a[1] + EPS < b[1] + b[3]
            && b[1] + EPS < a[1] + a[3]
    }

    #[test]
    fn squarify_handles_degenerate_input() {
        assert!(squarify(&[], 100.0, 100.0).is_empty());
        assert_eq!(squarify(&[1.0], 0.0, 100.0), vec![[0.0; 4]]);
        assert_eq!(squarify(&[0.0, 0.0], 100.0, 100.0), vec![[0.0; 4]; 2]);
    }

    #[test]
    fn squarify_single_tile_fills_the_rect() {
        let rects = squarify(&[42.0], 200.0, 100.0);
        assert_eq!(rects, vec![[0.0, 0.0, 200.0, 100.0]]);
    }

    #[test]
    fn squarify_preserves_total_area_and_order() {
        let values = [600.0, 300.0, 100.0, 50.0, 25.0];
        let rects = squarify(&values, 400.0, 200.0);
        assert_eq!(rects.len(), values.len());
        let painted: f32 = rects.iter().map(|r| r[2] * r[3]).sum();
        assert!(approx(painted, 400.0 * 200.0), "painted area {painted}");
        // Areas stay proportional to the values, in input order.
        let total: f64 = values.iter().sum();
        for (rect, value) in rects.iter().zip(values.iter()) {
            let want = (value / total) as f32 * 400.0 * 200.0;
            assert!(
                approx(rect[2] * rect[3], want),
                "tile area {} vs {want}",
                rect[2] * rect[3]
            );
        }
    }

    #[test]
    fn squarify_never_overlaps_and_stays_in_bounds() {
        let values = [
            500.0, 250.0, 120.0, 80.0, 30.0, 12.0, 6.0, 2.0, 1.0, 0.5, 0.25,
        ];
        let rects = squarify(&values, 333.0, 217.0);
        for (i, a) in rects.iter().enumerate() {
            assert!(a[0] >= -0.01 && a[1] >= -0.01, "tile {i} out of bounds");
            assert!(
                a[0] + a[2] <= 333.01 && a[1] + a[3] <= 217.01,
                "tile {i} out of bounds"
            );
            for b in rects.iter().skip(i + 1) {
                assert!(!overlaps(a, b), "tiles {i} overlap: {a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn squarify_equal_values_make_squarish_tiles() {
        // The point of the squarified layout: no slivers for equal weights.
        let values = [1.0; 6];
        let rects = squarify(&values, 300.0, 300.0);
        for rect in &rects {
            let aspect = (rect[2] / rect[3]).max(rect[3] / rect[2]);
            assert!(aspect < 2.0, "sliver tile {rect:?} (aspect {aspect})");
        }
    }

    #[test]
    fn heat_ramp_saturates() {
        assert_eq!(heat_t(0.0), 0.0);
        // k = 0.5: a ±50% move lands halfway up the ramp.
        assert!(approx(heat_t(0.5) as f32, 0.5));
        assert!(approx(heat_t(-0.5) as f32, 0.5));
        // Monotone, bounded below 1, and flattening at the extremes.
        assert!(heat_t(1.0) > heat_t(0.5));
        assert!(heat_t(10.0) < 1.0);
        assert!(heat_t(10.0) - heat_t(5.0) < heat_t(1.0) - heat_t(0.5));
    }

    fn rgb(h: f32, s: f32, l: f32) -> Hsla {
        Hsla { h, s, l, a: 1.0 }
    }

    #[test]
    fn heat_color_picks_the_right_end() {
        let up = rgb(0.05, 0.8, 0.5); // warm/attention
        let down = rgb(0.3, 0.6, 0.4); // green/positive
        let neutral = rgb(0.0, 0.0, 0.6);
        let new = rgb(0.12, 0.9, 0.5);
        // New buckets take the "new" color untouched.
        assert_eq!(heat_color(None, up, down, neutral, new), new);
        // Zero change stays neutral.
        assert_eq!(heat_color(Some(0.0), up, down, neutral, new), neutral);
        // A cost increase leans toward `up`, a decrease toward `down`, and
        // a bigger move leans harder.
        let small_up = Rgba::from(heat_color(Some(0.1), up, down, neutral, new));
        let big_up = Rgba::from(heat_color(Some(2.0), up, down, neutral, new));
        let up_rgb = Rgba::from(up);
        assert!((big_up.r - up_rgb.r).abs() < (small_up.r - up_rgb.r).abs());
        let down_tile = Rgba::from(heat_color(Some(-2.0), up, down, neutral, new));
        let down_rgb = Rgba::from(down);
        assert!((down_tile.g - down_rgb.g).abs() < 0.3);
    }

    #[test]
    fn mix_color_endpoints_and_midpoint() {
        let black = rgb(0.0, 0.0, 0.0);
        let white = rgb(0.0, 0.0, 1.0);
        assert_eq!(mix_color(black, white, 0.0), black);
        assert_eq!(mix_color(black, white, 1.0), white);
        let mid = Rgba::from(mix_color(black, white, 0.5));
        assert!(approx(mid.r, 0.5) && approx(mid.g, 0.5) && approx(mid.b, 0.5));
    }

    #[test]
    fn text_on_tracks_luminance() {
        let dark = rgb(0.0, 0.0, 0.1);
        let light = rgb(0.0, 0.0, 0.95);
        assert_eq!(text_on(rgb(0.0, 0.0, 0.9), dark, light), dark);
        assert_eq!(text_on(rgb(0.0, 0.0, 0.15), dark, light), light);
    }

    #[test]
    fn top_tiles_folds_the_tail() {
        let items: Vec<TreemapItem> = (0..20)
            .map(|i| TreemapItem::new(format!("s{i}"), 100.0 - i as f64, 50.0))
            .collect();
        let tiles = top_tiles(items, 11);
        assert_eq!(tiles.len(), 12);
        assert_eq!(tiles[11].label, "9 more");
        // The tail sums both periods, so its heat is honest.
        let want: f64 = (11..20).map(|i| 100.0 - i as f64).sum();
        assert!(approx(tiles[11].amount as f32, want as f32));
        assert!(approx(tiles[11].previous as f32, 9.0 * 50.0));
        // Largest first, and small inputs pass through untouched.
        assert_eq!(tiles[0].label, "s0");
        let few = top_tiles(vec![TreemapItem::new("a", 1.0, 1.0)], 11);
        assert_eq!(few.len(), 1);
    }

    #[test]
    fn change_ratio_marks_new_buckets() {
        assert_eq!(TreemapItem::new("a", 10.0, 0.0).change(), None);
        let m = TreemapItem::new("a", 15.0, 10.0).change().unwrap();
        assert!((m - 0.5).abs() < 1e-9);
        let m = TreemapItem::new("a", 5.0, 10.0).change().unwrap();
        assert!((m + 0.5).abs() < 1e-9);
    }
}
