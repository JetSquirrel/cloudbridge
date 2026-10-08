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

/// The main window, while it is open, and whether the stores behind it
/// have opened — a window opened after that has to be told at once, since
/// the notice it would otherwise wait for has already gone out.
struct MainWindow {
    handle: Option<WindowHandle<Root>>,
    stores_ready: bool,
}

impl Global for MainWindow {}

/// Bring the main window up — focusing it if it is open, opening it if it
/// was closed or never opened (a login launch starts without one) — and
/// switch it to `view` when one is given.
pub fn show_main_window(view: Option<crate::app::CurrentView>, cx: &mut App) {
    #[cfg(target_os = "macos")]
    crate::background::macos::set_dock_icon_visible(true);
    cx.activate(true);

    let open = cx
        .global::<MainWindow>()
        .handle
        .and_then(|handle| {
            handle
                .update(cx, |_, window, _| window.activate_window())
                .ok()
        })
        .is_some();
    if !open {
        match open_main_window(cx) {
            Ok(handle) => {
                let main = cx.global_mut::<MainWindow>();
                main.handle = Some(handle);
                if main.stores_ready {
                    crate::app::stores_opened(cx);
                }
            }
            Err(e) => {
                tracing::error!("Could not open the window: {}", e);
                return;
            }
        }
    }
    if let Some(view) = view {
        crate::app::navigate_to(view, cx);
    }
}

fn open_main_window(cx: &mut App) -> anyhow::Result<WindowHandle<Root>> {
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
    )
}

/// Open the desktop window and run until it closes.
pub fn run() {
    let started_at = chrono::Utc::now();
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

    // Closing the window leaves the app in the menu bar where there is one;
    // the menu's Quit is what ends it. Elsewhere it quits with the window.
    let quit_mode = if crate::background::RUNS_WINDOWLESS {
        QuitMode::Explicit
    } else {
        QuitMode::LastWindowClosed
    };
    let app = gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(quit_mode);
    // The Dock icon of a running app, clicked with no window open.
    app.on_reopen(|cx| show_main_window(None, cx));

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

        cx.set_global(MainWindow {
            handle: None,
            stores_ready: false,
        });
        // Forget the window once it closes, and with it the Dock icon: a
        // closed CloudBridge lives in the menu bar.
        cx.on_window_closed(|cx, closed| {
            let main = cx.global_mut::<MainWindow>();
            if main
                .handle
                .is_some_and(|handle| handle.window_id() == closed)
            {
                main.handle = None;
                #[cfg(target_os = "macos")]
                crate::background::macos::set_dock_icon_visible(false);
            }
        })
        .detach();

        let background_launch = crate::background::launched_in_background();
        #[cfg(target_os = "macos")]
        if background_launch {
            crate::background::macos::set_dock_icon_visible(false);
        }

        cx.spawn(async move |cx| {
            // Open the window before touching the stores, so a large ledger
            // no longer delays the first frame. Until init below finishes
            // the shell holds every page load back and shows loading
            // placeholders instead (see app.rs). A login launch opens none:
            // it starts in the menu bar.
            if !background_launch {
                cx.update(|cx| show_main_window(None, cx));
            }

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
            cx.update(|cx| {
                let main = cx.global_mut::<MainWindow>();
                main.stores_ready = true;
                if main.handle.is_some() {
                    crate::app::stores_opened(cx);
                }
                // Now the ledger can be read, the schedule and the menu bar
                // can start reading it.
                crate::background::start(started_at, cx);
            });

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
