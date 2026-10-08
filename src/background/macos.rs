//! The two things a menu bar app needs from AppKit that GPUI does not
//! offer: whether the app has a Dock icon, and where its menu bar icon is.

use gpui_kit::{point, px, size, Bounds, Pixels};
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy, NSScreen, NSStatusItem};

/// Show the Dock icon and app menu while a window is open, and drop both
/// once it closes, so a closed CloudBridge lives in the menu bar alone.
pub fn set_dock_icon_visible(visible: bool) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::warn!("Dock icon change asked for off the main thread");
        return;
    };
    let policy = if visible {
        NSApplicationActivationPolicy::Regular
    } else {
        NSApplicationActivationPolicy::Accessory
    };
    NSApplication::sharedApplication(mtm).setActivationPolicy(policy);
}

/// Where the status item is, and the screen it is on, both in points from
/// the top-left corner of the primary display — the space GPUI places a
/// window in when it is given no display.
pub struct IconPlacement {
    pub icon: Bounds<Pixels>,
    pub screen: Bounds<Pixels>,
}

/// The status item's on-screen frame, or `None` when it has not been laid
/// out (the menu bar is hidden, or too crowded to show it).
pub fn icon_placement(item: &NSStatusItem) -> Option<IconPlacement> {
    let mtm = MainThreadMarker::new()?;
    let window = item.button(mtm)?.window()?;
    let frame = window.frame();
    let screen = window.screen()?.frame();
    // AppKit measures up from the primary screen's bottom edge; GPUI down
    // from its top edge.
    let primary_height = NSScreen::screens(mtm).firstObject()?.frame().size.height;
    let flip = |rect: objc2_foundation::NSRect| {
        Bounds::new(
            point(
                px(rect.origin.x as f32),
                px((primary_height - rect.origin.y - rect.size.height) as f32),
            ),
            size(px(rect.size.width as f32), px(rect.size.height as f32)),
        )
    };
    Some(IconPlacement {
        icon: flip(frame),
        screen: flip(screen),
    })
}
