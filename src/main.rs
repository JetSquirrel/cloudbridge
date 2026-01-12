mod app;
mod cloud;
mod config;
mod crypto;
mod secret_store;
mod db;
mod ui;

use gpui::*;
use gpui_component::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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

        cx.spawn(async move |cx| {
            // Initialize database
            if let Err(e) = db::init_database() {
                tracing::error!("Database initialization failed: {}", e);
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
                        title: Some("CloudBridge - Multi-Cloud Cost Management".into()),
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
