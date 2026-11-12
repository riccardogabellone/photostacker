// Release builds on Windows open no console window next to the app.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;

use eframe::egui;
use photostacker_core::AppDirs;
use photostacker_ui::{
    app::App,
    controller::{Controller, RuntimeCheck, StartError, StartOptions},
    failed_start::FailedStart,
    identity::{APP_ID, APP_NAME, window_icon},
    logging,
    platform::{self, Native},
    renderer::native_options,
    wake::Waker,
};

const DEFAULT_SIZE: [f32; 2] = [1360.0, 820.0];
const MIN_SIZE: [f32; 2] = [900.0, 560.0];

fn main() -> eframe::Result {
    // First, so a panic from here on reaches the log, except one the core catches.
    logging::install_panic_hook();
    // Once, for the log and the controller alike.
    let dirs = AppDirs::resolve();
    // Kept until `main` returns: dropping it writes out what waits for the log file.
    let _log_file = logging::init(dirs.as_ref().map(AppDirs::logs).as_deref());
    logging::log_the_start(dirs.as_ref());
    // The controller starts before the window, so the window opens at its saved size; the
    // waker gets the window's context once there is one.
    let waker = Waker::default();
    let started = dirs.ok_or(StartError::NoAppDirs).and_then(|dirs| {
        Controller::start(StartOptions {
            dirs,
            wake: waker.wake(),
            platform: Arc::new(Native),
            embedder: None,
            installer: None,
            // Looked for once, before the first frame: the app reads no file on the UI
            // thread after (the rule and its exceptions: crates/ui/DESIGN.md, State and
            // the engine).
            runtime: RuntimeCheck::look(),
            licences: platform::licences_folder(),
        })
    });
    if let Err(error) = &started {
        tracing::error!("{APP_NAME} cannot start: {error}");
    }
    let saved_size = started
        .as_ref()
        .ok()
        .and_then(|controller| controller.prefs.window);
    let options = native_options(
        egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_app_id(APP_ID)
            .with_icon(window_icon())
            .with_inner_size(window_size(saved_size))
            .with_min_inner_size(MIN_SIZE),
    );
    let run = eframe::run_native(
        APP_NAME,
        options,
        Box::new(move |creation| {
            let app: Box<dyn eframe::App> = match started {
                Ok(controller) => {
                    waker.attach(&creation.egui_ctx);
                    Box::new(App::new(&creation.egui_ctx, controller))
                }
                Err(error) => Box::new(FailedStart::new(
                    &creation.egui_ctx,
                    &error,
                    Arc::new(Native),
                )),
            };
            Ok(app)
        }),
    );
    match &run {
        Ok(()) => tracing::info!("{APP_NAME} has quit"),
        Err(error) => tracing::error!("the window cannot run: {error}"),
    }
    run
}

/// The size the window opens at: the saved one, never under the minimum, or the default.
fn window_size(saved: Option<[f32; 2]>) -> [f32; 2] {
    match saved {
        Some([width, height]) if width.is_finite() && height.is_finite() => {
            [width.max(MIN_SIZE[0]), height.max(MIN_SIZE[1])]
        }
        _ => DEFAULT_SIZE,
    }
}
