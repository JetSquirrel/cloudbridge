//! Design tokens for the "local ledger" redesign.
//!
//! A thin mapping layer over gpui-component's `Theme`: the warm palette
//! lives in `themes/cloudbridge.json` ("CloudBridge Light" / "CloudBridge
//! Dark"), loaded through `ThemeRegistry`, and every function here reads the
//! corresponding `cx.theme()` token so call sites stay one-line.

// Consumed by the page agents as each redesigned page lands.
#![allow(dead_code)]

use std::time::Duration;

use gpui_kit::component::animation::cubic_bezier;
use gpui_kit::component::button::{Button, ButtonCustomVariant};
use gpui_kit::component::{ActiveTheme, StyledExt, Theme, ThemeRegistry};
use gpui_kit::{
    div, rems, Animation, App, Div, FontWeight, Hsla, IntoElement, ParentElement, SharedString,
    Styled,
};

/// Name of the light theme in `themes/cloudbridge.json`.
pub const LIGHT_THEME_NAME: &str = "CloudBridge Light";
/// Name of the dark theme in `themes/cloudbridge.json`.
pub const DARK_THEME_NAME: &str = "CloudBridge Dark";

/// Apply the theme with this exact name from the registry.
///
/// A missing name (themes directory absent, file failed to parse) logs and
/// keeps the current theme rather than panicking.
pub fn apply_theme_by_name(name: &str, cx: &mut App) {
    match ThemeRegistry::global(cx).themes().get(name).cloned() {
        Some(theme) => {
            Theme::global_mut(cx).apply_config(&theme);
            apply_density(cx);
            cx.refresh_windows();
        }
        None => {
            tracing::warn!(
                "Theme {:?} not found in the registry; keeping the current theme",
                name
            );
        }
    }
}

/// Density is an application-level decision, not a theme one: this is a
/// dense desktop data tool, so every theme gets the same compact scale
/// (13px base font ≈ Longbridge-style tooling) and the same 6px card
/// radius, whatever the theme file ships (gpui-component defaults are
/// 16px / 12px, which read as web-sized in a desktop window).
fn apply_density(cx: &mut App) {
    let theme = Theme::global_mut(cx);
    theme.font_size = gpui_kit::px(13.0);
    theme.mono_font_size = gpui_kit::px(12.0);
    theme.radius = gpui_kit::px(6.0);
    theme.radius_lg = gpui_kit::px(8.0);
    // Direct Theme mutation must be projected so Base-owned scrollbars
    // and resize handles pick it up (per the GPUI Kit coding guide).
    Theme::sync_base(cx);
}

/// Apply one of the CloudBridge themes, chosen by the persisted dark-mode
/// flag. Fallback for configs written before the theme picker existed.
pub fn apply_named_theme(dark: bool, cx: &mut App) {
    apply_theme_by_name(
        if dark {
            DARK_THEME_NAME
        } else {
            LIGHT_THEME_NAME
        },
        cx,
    );
}

/// App background: warm cream.
pub fn app_bg(cx: &App) -> Hsla {
    cx.theme().background
}

/// Sidebar background: slightly deeper cream.
pub fn sidebar_bg(cx: &App) -> Hsla {
    cx.theme().sidebar
}

/// Card background.
pub fn card_bg(cx: &App) -> Hsla {
    cx.theme().tiles
}

/// Card border.
pub fn card_border(cx: &App) -> Hsla {
    cx.theme().border
}

/// Primary accent: terracotta / burnt orange.
pub fn accent(cx: &App) -> Hsla {
    cx.theme().primary
}

/// Accent on hover.
pub fn accent_hover(cx: &App) -> Hsla {
    cx.theme().primary_hover
}

/// A color with its lightness pulled down by `by`, clamped.
fn deepened(color: Hsla, by: f32) -> Hsla {
    Hsla {
        l: (color.l - by).clamp(0.0, 1.0),
        ..color
    }
}

/// Accent while pressed: one step deeper than hover, so a held click reads
/// as pressure rather than a second hover.
pub fn accent_pressed(cx: &App) -> Hsla {
    deepened(accent_hover(cx), 0.05)
}

/// Neutral surface while pressed (outline buttons, range pills, filter
/// chips): one step deeper than the sidebar hover wash.
pub fn surface_pressed(cx: &App) -> Hsla {
    deepened(sidebar_bg(cx), 0.04)
}

/// The entrance every dialog shares: a 150ms ease-out fade with a slight
/// rise, so no dialog can drift from the others.
pub fn dialog_enter_animation() -> Animation {
    Animation::new(Duration::from_millis(150)).with_easing(cubic_bezier(0.0, 0.0, 0.2, 1.0))
}

/// Text on the accent color.
pub fn on_accent(cx: &App) -> Hsla {
    cx.theme().primary_foreground
}

/// Critical/alert tint background, used to highlight alert cards and badges.
///
/// Intentionally the same token as `danger_bg`, but a different semantic
/// slot: this one is for alert severity, not error states.
pub fn alert_tint(cx: &App) -> Hsla {
    cx.theme().red_light
}

/// Primary text: warm near-black.
pub fn text_primary(cx: &App) -> Hsla {
    cx.theme().foreground
}

/// Muted text.
pub fn text_muted(cx: &App) -> Hsla {
    cx.theme().muted_foreground
}

/// Olive green: secondary series / positive.
pub fn olive(cx: &App) -> Hsla {
    cx.theme().chart_2
}

/// Grey: unallocated series.
pub fn grey(cx: &App) -> Hsla {
    cx.theme().chart_3
}

/// Warning badge background (yellow tint).
pub fn warning_bg(cx: &App) -> Hsla {
    cx.theme().yellow_light
}

/// Warning badge text.
pub fn warning_text(cx: &App) -> Hsla {
    cx.theme().yellow
}

/// Error text (failures, destructive confirmations).
pub fn danger(cx: &App) -> Hsla {
    cx.theme().red
}

/// Error banner tint, used behind failure/error banners.
///
/// Intentionally the same token as `alert_tint`, but a different semantic
/// slot: this one is for error states, not alert severity highlighting.
pub fn danger_bg(cx: &App) -> Hsla {
    cx.theme().red_light
}

/// Success banner text.
pub fn success(cx: &App) -> Hsla {
    cx.theme().green
}

/// Success banner tint.
pub fn success_bg(cx: &App) -> Hsla {
    cx.theme().green_light
}

/// Modal overlay scrim: dark, half-opaque.
pub fn scrim(cx: &App) -> Hsla {
    cx.theme().overlay
}

/// Lighter olive: extra sankey-cycle color so the olive-toned lines stay
/// distinguishable (chart.4 in the CloudBridge themes).
pub fn olive_light(cx: &App) -> Hsla {
    cx.theme().chart_4
}

/// Base card: card background, 1px border, theme radius (6px — see
/// `apply_density`). Padding and layout are left to the caller.
pub fn card(cx: &App) -> Div {
    div()
        .bg(card_bg(cx))
        .border_1()
        .border_color(card_border(cx))
        .rounded(cx.theme().radius)
}

/// The warm-palette outline button: card surface, ink label.
///
/// Owned here rather than per page, so the Overview and Alerts buttons
/// cannot drift apart.
pub fn outline_variant(cx: &App) -> ButtonCustomVariant {
    ButtonCustomVariant::new(cx)
        .color(card_bg(cx))
        .foreground(text_primary(cx))
        .hover(sidebar_bg(cx))
        .active(surface_pressed(cx))
}

/// The card border on a button that carries a custom variant.
///
/// `ButtonCustomVariant` derives its border from its background colour, so
/// a visible outline has to be set on the instance; the component refines
/// the per-instance style over the variant's, so this wins.
pub trait CardOutline: Styled + Sized {
    fn card_outline(self, cx: &App) -> Self {
        self.border_1().border_color(card_border(cx))
    }
}

impl CardOutline for Button {}

/// Fully rounded badge/pill with the given colors.
pub fn pill(text: impl Into<SharedString>, bg: Hsla, fg: Hsla) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .bg(bg)
        .text_xs()
        .text_color(fg)
        .child(text.into())
}

/// Small colored dot (sync status, series legends).
pub fn dot(color: Hsla) -> Div {
    div().size_2().rounded_full().bg(color)
}

/// Page heading used at the top of every content view.
pub fn page_title(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .text_2xl()
        .font_weight(FontWeight::BOLD)
        .text_color(text_primary(cx))
        .child(text.into())
}

/// Muted caption under a page heading or chart.
pub fn caption(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .text_sm()
        .text_color(text_muted(cx))
        .child(text.into())
}

// Solid accent Buttons use `.primary()`, which reads the theme's primary
// tokens. A custom variant cannot stand in for it: gpui-kit paints a custom
// variant's resting background at a fifth of its color, so a solid accent
// came out as a pale wash with white text on it.

/// Segmented-control pill: the active range reads as a raised chip, the
/// others as plain text on the track.
pub fn range_pill(cx: &App, active: bool) -> ButtonCustomVariant {
    let pill = ButtonCustomVariant::new(cx)
        .foreground(text_muted(cx))
        .hover(sidebar_bg(cx))
        .active(surface_pressed(cx));
    if active {
        pill.color(card_bg(cx)).foreground(text_primary(cx))
    } else {
        pill
    }
}

/// KPI card: muted label, bold 3xl value, small sub-line.
pub fn stat_card(cx: &App, label: &'static str, value: String, sub: impl IntoElement) -> Div {
    card(cx)
        .flex_1()
        // min_w_0: four equal cards must shrink below their content
        // width instead of overflowing the row on narrow windows.
        .min_w_0()
        .p_5()
        .v_flex()
        .gap_1()
        .child(div().text_xs().text_color(text_muted(cx)).child(label))
        .child(
            div()
                // 13px base: text_3xl ≈ 24px — the KPI tier, clearly above
                // the text_2xl (≈20px) page title.
                .text_3xl()
                .font_weight(FontWeight::BOLD)
                .text_color(text_primary(cx))
                .child(value),
        )
        .child(div().text_sm().child(sub))
}

/// Bold section heading inside a page.
pub fn section_title(cx: &App, text: &'static str) -> Div {
    div()
        .text_base()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(text_primary(cx))
        .child(text)
}

/// Muted table header cell. Between xs (≈9.75px at the 13px base — too
/// small to read as a column label) and sm: 0.85rem ≈ 11px.
pub fn header_cell(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .text_size(rems(0.85))
        .text_color(text_muted(cx))
        .child(text.into())
}

/// Circular numeric badge (18px), e.g. the sidebar open-alerts count.
pub fn count_badge(_cx: &App, count: usize, bg: Hsla, fg: Hsla) -> Div {
    div()
        .size(rems(1.125))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .bg(bg)
        .text_color(fg)
        .child(count.to_string())
}

/// Outline version of `pill`: no fill, 1px card border, ink label.
pub fn pill_outline(cx: &App, text: impl Into<SharedString>) -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_full()
        .border_1()
        .border_color(card_border(cx))
        .text_xs()
        .text_color(text_primary(cx))
        .child(text.into())
}
