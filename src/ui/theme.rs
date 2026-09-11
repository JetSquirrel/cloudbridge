//! Design tokens for the "local ledger" redesign.
//!
//! A thin mapping layer over gpui-component's `Theme`: the warm palette
//! lives in `themes/cloudbridge.json` ("CloudBridge Light" / "CloudBridge
//! Dark"), loaded through `ThemeRegistry`, and every function here reads the
//! corresponding `cx.theme()` token so call sites stay one-line.

// Consumed by the page agents as each redesigned page lands.
#![allow(dead_code)]

use gpui_kit::component::button::{Button, ButtonCustomVariant};
use gpui_kit::component::{ActiveTheme, Theme, ThemeRegistry};
use gpui_kit::{div, App, Div, FontWeight, Hsla, ParentElement, SharedString, Styled};

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

/// Text on the accent color.
pub fn on_accent(cx: &App) -> Hsla {
    cx.theme().primary_foreground
}

/// Critical/alert tint background (highlighted alert cards and badges).
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

/// Error banner tint.
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
        .active(sidebar_bg(cx))
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
