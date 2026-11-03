//! The menu bar icon (macOS) or notification-area icon (Windows).
//!
//! Its title is the month's spend, and the bridge carries an exclamation
//! mark while an alert is open — enough to notice a runaway bill without
//! opening anything. A left click opens the panel ([`super::panel`]); a right click
//! the menu, which is where Quit lives, since closing the window no longer
//! quits.
//!
//! The icon can be taken down and put back while the app runs (Settings →
//! Background → Keep running in the menu bar). Icon and menu clicks arrive
//! on callbacks the platform runs, which tray-icon lets a process set only
//! once; so the callbacks, and the GPUI task they feed over a channel, are
//! set up on first install and outlive the icon, and act only while one is
//! installed.

use gpui_kit::component::Root;
use gpui_kit::*;
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use super::{panel, Status};

const MENU_OPEN: &str = "open";
const MENU_REFRESH: &str = "refresh";
const MENU_QUIT: &str = "quit";

/// What the platform callbacks report.
enum Input {
    /// A left click on the icon.
    IconClicked,
    Menu(String),
}

/// The icon, kept alive for as long as the app runs, and the panel when it
/// is open.
pub(super) struct MenuBar {
    icon: TrayIcon,
    /// Whether the icon shows the alert mark now.
    alerting: bool,
    pub(super) panel: Option<WindowHandle<Root>>,
    _status_observer: Subscription,
}

impl Global for MenuBar {}

/// Marks that the platform callbacks and the task reading them are set up.
struct EventPump;

impl Global for EventPump {}

/// Put the icon in the menu bar, unless it is there already. Returns
/// whether there is one afterwards.
pub(super) fn install(status: Entity<Status>, cx: &mut App) -> bool {
    if cx.has_global::<MenuBar>() {
        return true;
    }
    let menu = Menu::new();
    let items = [
        // Built once, in the language of the moment; a language switch
        // reaches the menu the next time the app starts.
        MenuItem::with_id(MENU_OPEN, tr!("panel.open"), true, None),
        MenuItem::with_id(MENU_REFRESH, tr!("common.refresh"), true, None),
    ];
    let quit = MenuItem::with_id(MENU_QUIT, tr!("tray.quit"), true, None);
    if let Err(e) = menu.append_items(&[
        &items[0],
        &items[1],
        &PredefinedMenuItem::separator(),
        &quit,
    ]) {
        tracing::warn!("Could not build the menu bar menu: {}", e);
    }

    let icon = match TrayIconBuilder::new()
        .with_icon(bridge_icon(false))
        .with_icon_as_template(true)
        .with_tooltip("CloudBridge")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(icon) => icon,
        Err(e) => {
            tracing::error!("Could not add the menu bar icon: {}", e);
            return false;
        }
    };

    ensure_event_pump(cx);

    let observer = cx.observe(&status, |status, cx| show_status(&status, cx));

    cx.set_global(MenuBar {
        icon,
        alerting: false,
        panel: None,
        _status_observer: observer,
    });
    // The observer fires on the next change; the title should not wait.
    show_status(&status, cx);
    true
}

/// Take the icon out of the menu bar, and close its panel if it is open.
/// A no-op when there is none.
pub(super) fn uninstall(cx: &mut App) {
    if !cx.has_global::<MenuBar>() {
        return;
    }
    let bar = cx.remove_global::<MenuBar>();
    if let Some(open) = bar.panel {
        let _ = open.update(cx, |_, window, _| window.remove_window());
    }
    // Dropping the icon removes it from the menu bar.
    drop(bar.icon);
}

/// The month's spend as the icon's title, and the tooltip.
fn show_status(status: &Entity<Status>, cx: &mut App) {
    let Some(summary) = &status.read(cx).summary else {
        return;
    };
    let (title, tooltip, alerting) = (summary.title(), summary.tooltip(), summary.alerting());
    tracing::debug!("Menu bar title: {title}");
    if !cx.has_global::<MenuBar>() {
        return;
    }
    let bar = cx.global_mut::<MenuBar>();
    if bar.alerting != alerting {
        // A plain set_icon drops the template flag on macOS.
        if let Err(e) = bar
            .icon
            .set_icon_with_as_template(Some(bridge_icon(alerting)), cfg!(target_os = "macos"))
        {
            tracing::warn!("Could not change the menu bar icon: {}", e);
        }
        bar.alerting = alerting;
    }
    bar.icon.set_title(Some(format!(" {title}")));
    if let Err(e) = bar.icon.set_tooltip(Some(tooltip)) {
        tracing::warn!("Could not set the menu bar tooltip: {}", e);
    }
}

/// Route icon and menu clicks to the app, once per process.
fn ensure_event_pump(cx: &mut App) {
    if cx.has_global::<EventPump>() {
        return;
    }
    cx.set_global(EventPump);

    let (sender, receiver) = async_channel::unbounded();
    let icon_sender = sender.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            let _ = icon_sender.send_blocking(Input::IconClicked);
        }
    }));
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let _ = sender.send_blocking(Input::Menu(event.id.0));
    }));

    cx.spawn(async move |cx| {
        while let Ok(input) = receiver.recv().await {
            cx.update(|cx| match input {
                // A click that raced the icon's removal.
                _ if !cx.has_global::<MenuBar>() => {}
                Input::IconClicked => toggle_panel(cx),
                Input::Menu(id) => match id.as_str() {
                    MENU_OPEN => crate::desktop::show_main_window(None, cx),
                    MENU_REFRESH => super::refresh_now(cx),
                    MENU_QUIT => cx.quit(),
                    _ => {}
                },
            });
        }
    })
    .detach();
}

fn toggle_panel(cx: &mut App) {
    if let Some(open) = cx.global_mut::<MenuBar>().panel.take() {
        // Already gone if it lost focus on the way here; nothing to close.
        let _ = open.update(cx, |_, window, _| window.remove_window());
        return;
    }
    let anchor = anchor(cx);
    tracing::debug!("Opening the menu bar panel at {anchor:?}");
    match panel::open(anchor, cx) {
        Ok(handle) => cx.global_mut::<MenuBar>().panel = Some(handle),
        Err(e) => tracing::error!("Could not open the menu bar panel: {}", e),
    }
}

/// Where the panel hangs from: under the icon on macOS. Windows puts the
/// notification area in the taskbar, wherever the user docked it, and
/// reports the icon in physical pixels of a display GPUI cannot name, so
/// there the panel takes the corner of the primary display's work area,
/// which is where the notification area sits by default.
fn anchor(cx: &App) -> panel::Anchor {
    #[cfg(target_os = "macos")]
    {
        if let Some(placement) = cx
            .global::<MenuBar>()
            .icon
            .ns_status_item()
            .and_then(|item| super::macos::icon_placement(&item))
        {
            return panel::Anchor::Below {
                icon: placement.icon,
                screen: placement.screen,
            };
        }
    }
    let work_area = cx
        .primary_display()
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1280.), px(800.))));
    panel::Anchor::Corner { work_area }
}

/// The icon's width and height in pixels.
const ICON_SIZE: u32 = 44;

/// The icon: the app icon's arch bridge and three dots as one solid shape,
/// with an exclamation mark in place of the dots when `alerting`. It is drawn
/// here rather than shipped as an image so it needs no asset, and the app
/// icon's slender artwork would not survive this size. On macOS it is black on
/// transparent, a template the menu bar tints for light and dark; Windows draws
/// an icon as it is, on a taskbar that may be either, so there it is a grey
/// that reads on both.
fn bridge_icon(alerting: bool) -> Icon {
    Icon::from_rgba(icon_rgba(alerting), ICON_SIZE, ICON_SIZE)
        .expect("a square RGBA buffer is a valid icon")
}

fn icon_rgba(alerting: bool) -> Vec<u8> {
    const INK: [u8; 3] = if cfg!(target_os = "macos") {
        [0, 0, 0]
    } else {
        [0x8a, 0x8a, 0x8a]
    };
    const SAMPLES: u32 = 4;

    let mut rgba = Vec::with_capacity((ICON_SIZE * ICON_SIZE * 4) as usize);
    for row in 0..ICON_SIZE {
        for column in 0..ICON_SIZE {
            let mut covered = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (column as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / ICON_SIZE as f32;
                    let y = (row as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / ICON_SIZE as f32;
                    covered += icon_covers(x, y, alerting) as u32;
                }
            }
            let alpha = (covered * 255 / (SAMPLES * SAMPLES)) as u8;
            rgba.extend_from_slice(&[INK[0], INK[1], INK[2], alpha]);
        }
    }
    rgba
}

/// Whether the shape covers a point, in units of the icon's width.
fn icon_covers(x: f32, y: f32, alerting: bool) -> bool {
    // The bridge: a shallow arc for the deck, and an arch cut through it.
    const LEFT: f32 = 0.03;
    const RIGHT: f32 = 0.97;
    const BASE: f32 = 0.88;
    const CROWN: f32 = 0.46;
    const DECK_END: f32 = 0.60;
    const ARCH: (f32, f32, f32) = (0.5, BASE, 0.27);

    const DOTS: [(f32, f32, f32); 3] =
        [(0.26, 0.29, 0.08), (0.55, 0.17, 0.11), (0.81, 0.32, 0.085)];

    // The exclamation mark: a bar with rounded ends, and a dot.
    const BAR: (f32, f32, f32, f32) = (0.5, 0.09, 0.20, 0.06);
    const MARK_DOT: (f32, f32, f32) = (0.5, 0.355, 0.055);

    let in_disc = |(cx, cy, r): (f32, f32, f32)| (x - cx).powi(2) + (y - cy).powi(2) <= r * r;

    let half_span = (RIGHT - LEFT) / 2.0;
    let rise = DECK_END - CROWN;
    let deck_radius = (half_span * half_span + rise * rise) / (2.0 * rise);
    let deck = CROWN + deck_radius
        - (deck_radius * deck_radius - (x - 0.5).powi(2))
            .max(0.0)
            .sqrt();
    if (LEFT..=RIGHT).contains(&x) && (deck..=BASE).contains(&y) && !in_disc(ARCH) {
        return true;
    }

    if alerting {
        let (bar_x, top, bottom, half) = BAR;
        let along = y.clamp(top, bottom);
        (x - bar_x).powi(2) + (y - along).powi(2) <= half * half || in_disc(MARK_DOT)
    } else {
        DOTS.into_iter().any(in_disc)
    }
}

#[cfg(test)]
mod tests {
    use super::{icon_covers, icon_rgba, ICON_SIZE};

    #[test]
    fn the_bridge_has_an_opening_under_a_solid_deck() {
        for alerting in [false, true] {
            assert!(icon_covers(0.5, 0.55, alerting), "the deck at the crown");
            assert!(icon_covers(0.1, 0.8, alerting), "a pier");
            assert!(icon_covers(0.9, 0.8, alerting), "a pier");
            assert!(!icon_covers(0.5, 0.8, alerting), "the arch");
        }
    }

    #[test]
    fn an_alert_changes_only_what_is_above_the_bridge() {
        for row in 0..ICON_SIZE {
            for column in 0..ICON_SIZE {
                let (x, y) = (
                    (column as f32 + 0.5) / ICON_SIZE as f32,
                    (row as f32 + 0.5) / ICON_SIZE as f32,
                );
                if y >= 0.45 {
                    assert_eq!(icon_covers(x, y, false), icon_covers(x, y, true));
                }
            }
        }
        assert_ne!(icon_rgba(false), icon_rgba(true));
    }

    #[test]
    fn the_mark_stands_clear_of_the_bridge_and_its_own_dot() {
        assert!(icon_covers(0.5, 0.13, true), "the bar");
        assert!(icon_covers(0.5, 0.355, true), "the dot");
        assert!(!icon_covers(0.5, 0.275, true), "between bar and dot");
        assert!(!icon_covers(0.5, 0.435, true), "between dot and deck");
    }

    #[test]
    fn nothing_is_clipped_at_the_edge_of_the_icon() {
        let size = ICON_SIZE as usize;
        for alerting in [false, true] {
            let rgba = icon_rgba(alerting);
            assert_eq!(rgba.len(), size * size * 4);
            for row in 0..size {
                for column in 0..size {
                    if row == 0 || column == 0 || row == size - 1 || column == size - 1 {
                        assert_eq!(rgba[(row * size + column) * 4 + 3], 0);
                    }
                }
            }
        }
    }
}
