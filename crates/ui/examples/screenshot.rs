//! Opens the real window on one screen of the app, draws it for a while (and until the
//! inspector has what it asked for), saves a PNG of it and exits: a way to look at the UI
//! without clicking through it.
//!
//! Usage: `just screenshot <out.png> <screen> [dark|light] [width] [height]`
//! (the window is 1360 × 820 by default).
//!
//! The app runs on a temporary data folder, never the user's, and its jobs on temporary
//! folders of synthetic photos. The screens are in `tests/screens/mod.rs`, which the
//! interaction tests draw too; a new screen is one entry in its `SCREENS`.

#[path = "../tests/screens/mod.rs"]
mod screens;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eframe::egui;
use photostacker_ui::{identity::APP_NAME, renderer::native_options};
use screens::{Held, OpenOnDrop, SCREENS, Screen, Screened, start};

/// Enough frames for the fonts, the logo's texture, the jobs' list and the panels' sizes
/// to settle.
const FRAMES: u32 = 30;
const DEFAULT_SIZE: [f32; 2] = [1360.0, 820.0];
const USAGE: &str = "usage: screenshot <out.png> <screen> [dark|light] [width] [height]";

/// What the command line asks for.
struct Request {
    out: PathBuf,
    screen: &'static Screen,
    theme: egui::Theme,
    size: [f32; 2],
}

fn parse(mut args: impl Iterator<Item = String>) -> Result<Request, String> {
    let out = PathBuf::from(args.next().ok_or(USAGE)?);
    let name = args.next().ok_or(USAGE)?;
    let screen = SCREENS
        .iter()
        .find(|screen| screen.name == name)
        .ok_or_else(|| {
            let known: Vec<&str> = SCREENS.iter().map(|screen| screen.name).collect();
            format!(
                "unknown screen {name:?}; the screens are: {}",
                known.join(", ")
            )
        })?;
    let theme = match args.next().as_deref() {
        None | Some("dark") => egui::Theme::Dark,
        Some("light") => egui::Theme::Light,
        Some(other) => return Err(format!("the theme is dark or light, not {other:?}")),
    };
    let [default_width, default_height] = DEFAULT_SIZE;
    let width = args
        .next()
        .map_or(Ok(default_width), |v| pixels("width", &v))?;
    let height = args
        .next()
        .map_or(Ok(default_height), |v| pixels("height", &v))?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument {extra:?}\n{USAGE}"));
    }
    Ok(Request {
        out,
        screen,
        theme,
        size: [width, height],
    })
}

/// A window side given on the command line, in logical pixels.
fn pixels(side: &str, value: &str) -> Result<f32, String> {
    value
        .parse()
        .ok()
        .filter(|pixels: &f32| pixels.is_finite() && *pixels >= 1.0)
        .ok_or_else(|| format!("the {side} is a number of pixels, not {value:?}"))
}

/// Draws the app for [`FRAMES`] frames, and until it is ready (for at most
/// [`READY_TIMEOUT`]), then asks for the screenshot, which saves the PNG and closes the
/// window.
struct Screenshotter {
    /// Lets a held scan or write go before the app, and with it the engine, which waits
    /// for the scan's workers and its own thread, is dropped: fields drop in their order.
    _open: Option<OpenOnDrop>,
    app: Box<dyn Screened>,
    out: PathBuf,
    frames: u32,
    /// When the screenshot is taken even if the app is not ready.
    deadline: Instant,
    /// Whether the screenshot was asked for.
    taken: bool,
    /// Whether the PNG was written, once the screenshot came.
    saved: Arc<Mutex<Option<Result<(), String>>>>,
}

/// How long a screen may wait for what it shows to arrive (the inspector's preview or
/// prediction) before it is taken all the same.
const READY_TIMEOUT: Duration = Duration::from_secs(30);

impl eframe::App for Screenshotter {
    fn raw_input_hook(&mut self, context: &egui::Context, raw_input: &mut egui::RawInput) {
        self.app.raw_input_hook(context, raw_input);
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        // Once the PNG is saved the window closes, and the app no longer sees the request:
        // with a job running it would cancel the close and ask whether to quit.
        if self.saved.lock().expect("never poisoned").is_some() {
            return;
        }
        self.app.ui(ui, frame);
        self.frames += 1;
        let waiting = !self.app.ready() && Instant::now() < self.deadline;
        if self.taken {
            return;
        }
        if self.frames < FRAMES || waiting {
            ui.ctx().request_repaint();
        } else {
            self.taken = true;
            let out = self.out.clone();
            let saved = Arc::clone(&self.saved);
            let context = ui.ctx().clone();
            ui.ctx().request_screenshot(move |image| {
                let result = save_png(&out, &image);
                *saved.lock().expect("the result is only written here") = Some(result);
                context.send_viewport_cmd(egui::ViewportCommand::Close);
                context.request_repaint();
            });
        }
    }
}

fn save_png(path: &Path, image: &egui::ColorImage) -> Result<(), String> {
    let rgba: Vec<u8> = image
        .pixels
        .iter()
        .flat_map(|pixel| pixel.to_srgba_unmultiplied())
        .collect();
    let [width, height] = image.size.map(|side| side as u32);
    image::save_buffer(path, &rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|error| format!("{}: {error}", path.display()))
}

fn main() -> ExitCode {
    let request = match parse(std::env::args().skip(1)) {
        Ok(request) => request,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    let (data, prepared, held, waker) = match start(request.screen) {
        Ok(started) => started,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let saved = Arc::new(Mutex::new(None));
    let screenshotter_saved = Arc::clone(&saved);
    let options = native_options(
        egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size(request.size),
    );
    let setup = request.screen.setup;
    let run = eframe::run_native(
        "photostacker_screenshot",
        options,
        Box::new(move |creation| {
            // Moved whole, so the closure captures it whole, not its two parts: should
            // the window never open, it is dropped in its own order.
            let held: Held = held;
            let (open, controller) = held;
            waker.attach(&creation.egui_ctx);
            let app = setup(&creation.egui_ctx, controller);
            // After the app applies its saved theme.
            creation.egui_ctx.set_theme(request.theme);
            Ok(Box::new(Screenshotter {
                _open: open,
                app,
                out: request.out,
                frames: 0,
                deadline: Instant::now() + READY_TIMEOUT,
                taken: false,
                saved: screenshotter_saved,
            }))
        }),
    );
    // The app, and with it the engine, is gone once the window is: the folders can go.
    drop(prepared);
    drop(data);
    if let Err(error) = run {
        eprintln!("the window could not run: {error}");
        return ExitCode::FAILURE;
    }
    match saved.lock().expect("the window has closed").take() {
        Some(Ok(())) => ExitCode::SUCCESS,
        Some(Err(message)) => {
            eprintln!("the screenshot could not be saved: {message}");
            ExitCode::FAILURE
        }
        None => {
            eprintln!("the window closed before the screenshot was taken");
            ExitCode::FAILURE
        }
    }
}
