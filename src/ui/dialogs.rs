//! Opening a page's dialog through `Root`.
//!
//! `Root` keeps a dialog as a builder it calls on every frame, with no
//! handle on the page that opened it. These run that builder on the page
//! instead, so a dialog reads the page's live state (an in-flight save, the
//! row it is about) and wires its buttons with `cx.listener`, as the page's
//! own render does.
//!
//! Escape, the backdrop and the close button dismiss a dialog and call its
//! `on_close`; a page closing one itself (after a save) calls
//! `window.close_dialog`, which does not. Either way `Root` returns focus
//! to where it was before the dialog opened.

use gpui_kit::component::dialog::{AlertDialog, Dialog};
use gpui_kit::component::WindowExt as _;
use gpui_kit::{Context, Window};

/// Open a dialog built by `build` on the page every frame.
pub(super) fn open<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    build: impl Fn(&mut V, Dialog, &mut Window, &mut Context<V>) -> Dialog + 'static,
) {
    let view = cx.entity().downgrade();
    window.open_dialog(cx, move |dialog, window, cx| match view.upgrade() {
        Some(view) => view.update(cx, |this, cx| build(this, dialog, window, cx)),
        None => dialog,
    });
}

/// Open a confirmation built by `build` on the page every frame. An alert
/// dialog has no backdrop dismissal and no close button: the choice has to
/// be made, or escaped.
pub(super) fn open_alert<V: 'static>(
    window: &mut Window,
    cx: &mut Context<V>,
    build: impl Fn(&mut V, AlertDialog, &mut Window, &mut Context<V>) -> AlertDialog + 'static,
) {
    let view = cx.entity().downgrade();
    window.open_alert_dialog(cx, move |alert, window, cx| match view.upgrade() {
        Some(view) => view.update(cx, |this, cx| build(this, alert, window, cx)),
        None => alert,
    });
}
