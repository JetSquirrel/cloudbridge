//! The Overview page's spend chart.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::*;

use super::data::ChartPoint;
use super::theme;

/// Gridlines sit behind the data series, faint enough to read as context
/// rather than as part of it.
const GRIDLINE_OPACITY: f32 = 0.6;
/// The area fill is a whisper of the accent so the line keeps visual
/// priority.
const AREA_FILL_OPACITY: f32 = 0.12;

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

            let mut min = f64::INFINITY;
            let mut max = f64::NEG_INFINITY;
            for v in actual.iter().chain(baseline.iter()) {
                min = min.min(*v);
                max = max.max(*v);
            }
            if !min.is_finite() || !max.is_finite() || min >= max {
                min = 0.0;
                max = 1.0;
            }
            let pad = (max - min) * 0.15;
            min -= pad;
            max += pad;

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

            // Faint horizontal gridlines.
            for i in 0..=3 {
                let y = top + (bottom - top) * (i as f32 / 3.0);
                let mut grid = PathBuilder::stroke(px(1.0));
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
                let mut dashed = PathBuilder::stroke(px(1.5)).dash_array(&[px(4.0), px(4.0)]);
                trace_smooth(&mut dashed, &baseline);
                if let Ok(path) = dashed.build() {
                    window.paint_path(path, baseline_color);
                }
            }

            // Actual spend: solid accent line.
            if actual.len() >= 2 {
                let mut line = PathBuilder::stroke(px(2.0));
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
