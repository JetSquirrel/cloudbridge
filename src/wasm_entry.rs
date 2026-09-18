//! The browser entry point.
//!
//! `desktop.rs` is the other one. Everything between them — the shell, the
//! pages, the view models, the alerting rules — is the same code: this file
//! only supplies what the browser cannot do for itself. Fonts, because the
//! web platform starts with an empty font database and GPUI would have
//! nothing to measure text with. A theme, because the desktop watches a
//! directory of theme files and a browser has no directory. And data, because
//! the whole point of the demo is that it opens with a bill already in it.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::component::*;
use gpui_kit::web::{WebBackendPreference, WebPlatform};
use gpui_kit::{prelude::*, *};
use wasm_bindgen::prelude::*;

thread_local! {
    static APPLICATION: RefCell<Option<ApplicationHandle>> = const { RefCell::new(None) };
}

/// Boot the demo. Called once, from `web/site/src/main.js`.
#[wasm_bindgen]
pub fn run() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    tracing_wasm::set_as_global_default();

    gpui_kit::platform::web_init();

    // Single-threaded on purpose: the web backend's data layer is in memory,
    // so there is no blocking work worth a worker thread, and staying single-
    // threaded avoids needing cross-origin isolation for `SharedArrayBuffer`.
    let platform = Rc::new(WebPlatform::new_with_backend(
        false,
        WebBackendPreference::Auto,
    ));
    let http_client = Arc::new(platform.fetch_http_client());
    let app = Application::with_platform(platform)
        .with_http_client(http_client)
        // The one argument is the prefix the icon source fetches from.
        // Relative, not empty: it resolves against the page's own URL, so the
        // same module works served from the site root and from a
        // subdirectory — which is where the demo is published, under
        // `/demo/`. `scripts/build-web.sh` puts `assets/` next to the page.
        .with_assets(gpui_kit::assets::Assets::new("."));

    let launch = move |cx: &mut App| {
        gpui_kit::init(cx);
        add_fonts(cx);
        apply_theme(cx);
        load_demo_data();

        cx.open_window(WindowOptions::default(), |window, cx| {
            let view = cx.new(|cx| crate::app::CloudBridgeApp::new(window, cx));
            cx.new(|cx| Root::new(view, window, cx))
        })
        .expect("Failed to open the window");
        cx.activate(true);
    };

    APPLICATION.with(|application| *application.borrow_mut() = Some(app.run_embedded(launch)));

    Ok(())
}

/// Register the fonts the interface is measured and drawn with.
///
/// The web platform starts with an empty font database — there are no system
/// fonts to fall back on, which is the one way a browser differs most from
/// the desktop here. Three of these are the faces the UI asks for by name.
///
/// The fourth, IBM Plex Sans, is not: GPUI resolves its `.SystemUIFont` alias
/// to that family on this platform, and every element that has not been given
/// a family explicitly — including text measured before the first frame —
/// carries the alias. Without it the text system panics rather than falling
/// back, so it is not optional even though nothing here names it.
///
/// Noto Sans SC is subsetted to the characters the interface uses: the demo's
/// source names include 火山引擎, and the bundled subset covers exactly that
/// kind of thing rather than the whole script.
fn add_fonts(cx: &mut App) {
    let ui = Cow::Borrowed(include_bytes!("../web/site/fonts/Inter-Regular.ttf").as_slice());
    let mono =
        Cow::Borrowed(include_bytes!("../web/site/fonts/JetBrainsMono-Regular.ttf").as_slice());
    let system =
        Cow::Borrowed(include_bytes!("../web/site/fonts/IBMPlexSans-Regular.ttf").as_slice());
    let cjk =
        Cow::Borrowed(include_bytes!("../web/site/fonts/NotoSansSC-Regular-subset.ttf").as_slice());

    cx.text_system()
        .add_fonts(vec![ui, mono, system, cjk])
        .expect("Failed to load the bundled fonts");
}

/// Put the CloudBridge theme in the registry and select it.
///
/// The desktop loads every file in `./themes` and watches the directory for
/// changes. Neither is possible here, so the one theme the demo ships is
/// parsed straight out of the source. Everything about how a theme is then
/// applied — the density, the persisted choice, the fallback pair — is the
/// same code path the desktop takes.
fn apply_theme(cx: &mut App) {
    if let Err(e) = ThemeRegistry::global_mut(cx)
        .load_themes_from_str(include_str!("../themes/cloudbridge.json"))
    {
        tracing::warn!("Could not load the CloudBridge theme: {}", e);
    }

    let settings = crate::config::load_config().unwrap_or_default();
    match settings.theme.name {
        Some(name) => crate::ui::theme::apply_theme_by_name(&name, cx),
        None => crate::ui::theme::apply_named_theme(settings.theme.dark_mode, cx),
    }
}

/// Fill the ledger with the demo bill and run the rules against it.
///
/// Done before the first frame rather than on a background task, because
/// there is nothing to wait for: the rows are built in memory in microseconds
/// and the window should not open onto an empty dashboard and then repaint
/// with figures.
///
/// The order matters. The ledger has to know the reporting currency before
/// anything reads an amount, the seed has to land before the rules look for a
/// balance to fall below, and the rules have to run before the sidebar draws
/// its alert badge — exactly as on the desktop.
fn load_demo_data() {
    let reporting_currency = crate::config::load_config()
        .map(|settings| settings.reporting_currency)
        .unwrap_or_default();

    if let Err(e) = crate::db::init_database() {
        tracing::error!("Application state initialization failed: {}", e);
    }
    if let Err(e) = crate::ledger::init_ledger(&reporting_currency) {
        tracing::error!("Ledger initialization failed: {}", e);
    }

    match crate::ledger::demo::seed_demo() {
        Ok(summary) => tracing::info!("Demo data loaded: {}", summary),
        Err(e) => tracing::error!("Demo data failed to load: {}", e),
    }

    match crate::alerts::evaluate() {
        Ok(fired) if fired > 0 => tracing::info!("Alert evaluation raised {} alert(s)", fired),
        Ok(_) => {}
        Err(e) => tracing::error!("Alert evaluation failed: {}", e),
    }
}
