mod alerts;
mod app;
mod cloud;
mod config;
mod crypto;
mod db;
mod ingest;
mod ledger;
mod secret_store;
mod ui;

use gpui::*;
use gpui_component::*;
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

fn main() {
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

    let app = Application::new().with_assets(gpui_component_assets::Assets);

    app.run(move |cx| {
        // Initialize GPUI Component
        gpui_component::init(cx);

        let reporting_currency = config::load_config()
            .map(|settings| settings.reporting_currency)
            .unwrap_or_default();

        // Load the themes and apply the persisted one before the first
        // window opens, so the app does not flash the default appearance on
        // startup. The callback re-fires when a theme file changes, so it
        // re-reads the persisted choice each time and stays idempotent.
        if let Err(e) = ThemeRegistry::watch_dir(themes_dir(), cx, |cx| {
            let settings = config::load_config().unwrap_or_default();
            match settings.theme.name {
                Some(name) => ui::theme::apply_theme_by_name(&name, cx),
                None => ui::theme::apply_named_theme(settings.theme.dark_mode, cx),
            }
        }) {
            tracing::error!("Failed to watch themes directory: {}", e);
        }

        cx.spawn(async move |cx| {
            // Initialize databases: application state, then the billing ledger.
            if let Err(e) = db::init_database() {
                tracing::error!("Database initialization failed: {}", e);
            }
            if let Err(e) = ledger::init_ledger(&reporting_currency) {
                tracing::error!("Ledger initialization failed: {}", e);
            }

            // Run the alerting rules once against the freshly opened ledger,
            // before first paint: the sidebar badge and the Alerts page read
            // what this writes. Best-effort — a failed evaluation never
            // blocks the window from opening.
            match smol::unblock(alerts::evaluate).await {
                Ok(fired) if fired > 0 => {
                    tracing::info!("Alert evaluation raised {} alert(s)", fired)
                }
                Ok(_) => {}
                Err(e) => tracing::error!("Alert evaluation failed: {}", e),
            }

            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds {
                        origin: Point::default(),
                        size: gpui::Size {
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
                    let view = cx.new(|cx| app::CloudBridgeApp::new(window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
