//! Starting the desktop application.
//!
//! The body of what used to be `main.rs`. It is a module rather than the
//! binary itself so that the two targets can share everything above the data
//! layer — the browser never compiles this, and never opens a window this
//! way.

use gpui_kit::component::*;
use gpui_kit::*;
use std::path::PathBuf;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

/// Where the theme JSON files live.
///
/// `./themes` covers running from the repository root. A packaged .app
/// launches with an arbitrary working directory, so fall back to the
/// bundle's `Contents/Resources/themes` (populated by
/// scripts/package-macos.sh) and then to a `themes` dir next to the
/// executable. If none exists, the relative default is returned and
/// `watch_dir` creates it; the named theme is then simply not found and the
/// built-in default stays active.
fn themes_dir() -> PathBuf {
    let local = PathBuf::from("./themes");
    if local.is_dir() {
        return local;
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for candidate in [dir.join("../Resources/themes"), dir.join("themes")] {
                if candidate.is_dir() {
                    return candidate;
                }
            }
        }
    }

    local
}

/// Open the desktop window and run until it closes.
pub fn run() {
    // Initialize logging with appropriate level for release/debug
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            // Debug build: show debug logs
            EnvFilter::new("cloudbridge=debug,gpui=warn")
        } else {
            // Release build: only show warnings and errors
            EnvFilter::new("cloudbridge=warn,gpui=error")
        }
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
        .with(filter)
        .init();

    tracing::info!("Starting CloudBridge...");

    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        // Initialize GPUI Component
        gpui_kit::init(cx);

        let reporting_currency = crate::config::load_config()
            .map(|settings| settings.reporting_currency)
            .unwrap_or_default();

        // Load the themes and apply the persisted one before the first
        // window opens, so the app does not flash the default appearance on
        // startup. The callback re-fires when a theme file changes, so it
        // re-reads the persisted choice each time and stays idempotent.
        if let Err(e) = ThemeRegistry::watch_dir(themes_dir(), cx, |cx| {
            let settings = crate::config::load_config().unwrap_or_default();
            match settings.theme.name {
                Some(name) => crate::ui::theme::apply_theme_by_name(&name, cx),
                None => crate::ui::theme::apply_named_theme(settings.theme.dark_mode, cx),
            }
        }) {
            tracing::error!("Failed to watch themes directory: {}", e);
        }

        cx.spawn(async move |cx| {
            // Open the window before touching the stores, so a large ledger
            // no longer delays the first frame. Until init below finishes
            // the shell holds every page load back and shows loading
            // placeholders instead (see app.rs).
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: gpui_kit::Size {
                            width: px(1280.0),
                            height: px(800.0),
                        },
                    })),
                    titlebar: Some(TitlebarOptions {
                        title: Some("CloudBridge — local bill analysis".into()),
                        ..Default::default()
                    }),
                    ..Default::default()
                },
                |window, cx| {
                    let view = cx.new(|cx| crate::app::CloudBridgeApp::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            // Both stores open and migrate on disk and the first alert
            // evaluation scans the ledger — all blocking, so they run on a
            // worker thread now that the window is up. Best-effort: a
            // failure here never keeps the window from being usable.
            let evaluated = smol::unblock(move || {
                // Application state first, then the billing ledger.
                if let Err(e) = crate::db::init_database() {
                    tracing::error!("Database initialization failed: {}", e);
                }
                if let Err(e) = crate::ledger::init_ledger(&reporting_currency) {
                    tracing::error!("Ledger initialization failed: {}", e);
                }

                // Run the alerting rules once against the freshly opened
                // ledger; the sidebar badge and the Alerts page read what
                // this writes.
                crate::alerts::evaluate()
            })
            .await;
            match evaluated {
                Ok(fired) if fired > 0 => {
                    tracing::info!("Alert evaluation raised {} alert(s)", fired)
                }
                Ok(_) => {}
                Err(e) => tracing::error!("Alert evaluation failed: {}", e),
            }

            // The pages deferred their first loads (see app.rs) and the
            // sidebar badge reads what evaluation wrote; tell the shell the
            // stores are open so it loads the current page and status bar.
            cx.update(crate::app::stores_opened);

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
