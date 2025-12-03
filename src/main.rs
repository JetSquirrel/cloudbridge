mod app;
mod cloud;
mod config;
mod crypto;
mod db;
mod ui;

use gpui::*;
use gpui_component::*;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

fn main() {
    // Initialize logging
    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer())
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

