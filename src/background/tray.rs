//! The menu bar icon (macOS) or notification-area icon (Windows).
//!
//! Its title is the month's spend, and it carries the open-alert count when
//! there are any — enough to notice a runaway bill without opening
//! anything. A left click opens the panel ([`super::panel`]); a right click
//! the menu, which is where Quit lives, since closing the window no longer
//! quits.
//!
//! Icon and menu clicks arrive on callbacks the platform runs; they are
//! passed over a channel to a GPUI task, which is the only place that
//! touches the app.

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
    pub(super) panel: Option<WindowHandle<Root>>,
    _status_observer: Subscription,
}

impl Global for MenuBar {}

pub(super) fn install(status: Entity<Status>, cx: &mut App) {
    let menu = Menu::new();
    let items = [
        MenuItem::with_id(MENU_OPEN, "Open CloudBridge", true, None),
        MenuItem::with_id(MENU_REFRESH, "Refresh", true, None),
    ];
    let quit = MenuItem::with_id(MENU_QUIT, "Quit CloudBridge", true, None);
    if let Err(e) = menu.append_items(&[
        &items[0],
        &items[1],
        &PredefinedMenuItem::separator(),
        &quit,
    ]) {
        tracing::warn!("Could not build the menu bar menu: {}", e);
    }

    let icon = match TrayIconBuilder::new()
        .with_icon(cloud_icon())
        .with_icon_as_template(true)
        .with_tooltip("CloudBridge")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
    {
        Ok(icon) => icon,
        Err(e) => {
            // Without an icon there is no way back to a closed window, nor
            // a Quit to reach: fall back to quitting with the window.
            tracing::error!("Could not add the menu bar icon: {}", e);
            cx.set_quit_mode(QuitMode::LastWindowClosed);
            return;
        }
    };

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

    let observer = cx.observe(&status, |status, cx| {
        let status = status.read(cx);
        let Some(summary) = &status.summary else {
            return;
        };
        let (title, tooltip) = (summary.title(), summary.tooltip());
        tracing::debug!("Menu bar title: {title}");
        let bar = cx.global::<MenuBar>();
        bar.icon.set_title(Some(format!(" {title}")));
        if let Err(e) = bar.icon.set_tooltip(Some(tooltip)) {
            tracing::warn!("Could not set the menu bar tooltip: {}", e);
        }
    });

    cx.set_global(MenuBar {
        icon,
        panel: None,
        _status_observer: observer,
    });
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

/// The icon: a cloud, drawn here rather than shipped as an image so it
/// needs no asset. On macOS it is black on transparent, a template the menu
/// bar tints for light and dark; Windows draws an icon as it is, on a
/// taskbar that may be either, so there it is a grey that reads on both.
fn cloud_icon() -> Icon {
    const INK: [u8; 3] = if cfg!(target_os = "macos") {
        [0, 0, 0]
    } else {
        [0x8a, 0x8a, 0x8a]
    };
    const SIZE: u32 = 44;
    const SAMPLES: u32 = 4;
    // Lobes and base, in units of the icon's width.
    const CIRCLES: [(f32, f32, f32); 3] =
        [(0.32, 0.58, 0.17), (0.52, 0.44, 0.23), (0.72, 0.58, 0.15)];
    const BASE: (f32, f32, f32, f32) = (0.16, 0.57, 0.86, 0.75);

    let inside = |x: f32, y: f32| {
        CIRCLES
            .iter()
            .any(|(cx, cy, r)| (x - cx).powi(2) + (y - cy).powi(2) <= r * r)
            || (x >= BASE.0 && x <= BASE.2 && y >= BASE.1 && y <= BASE.3)
    };

    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    for row in 0..SIZE {
        for column in 0..SIZE {
            let mut covered = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = (column as f32 + (sx as f32 + 0.5) / SAMPLES as f32) / SIZE as f32;
                    let y = (row as f32 + (sy as f32 + 0.5) / SAMPLES as f32) / SIZE as f32;
                    covered += inside(x, y) as u32;
                }
            }
            let alpha = (covered * 255 / (SAMPLES * SAMPLES)) as u8;
            rgba.extend_from_slice(&[INK[0], INK[1], INK[2], alpha]);
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE).expect("a square RGBA buffer is a valid icon")
}
