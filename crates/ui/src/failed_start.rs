//! The window of an app that could not start (spec §9.2): what went wrong, in the user's
//! words, and a way out; for a database that cannot be opened, a way to its folder too.
//! `main` opens it instead of [`crate::app::App`] when the controller cannot start.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui::{self, TextureHandle};

use crate::{
    controller::{Notice, Reveal, StartError, reveal_in_the_background},
    format::{display_path, sentence},
    identity::{self, APP_NAME},
    platform::Platform,
    theme,
    widgets::{self, Emphasis},
};

/// The widest the card gets, so its text wraps into lines that read easily.
const WIDTH: f32 = 480.0;
/// The space inside the card.
const MARGIN: i8 = 16;
/// The logo on it: the toolbar's, at its own size.
const LOGO_SIZE: f32 = 32.0;

/// What the window of a start that failed with `error` says (spec §9.2).
fn failed_start_text(error: &StartError) -> String {
    match error {
        StartError::NoAppDirs => format!("{APP_NAME} cannot find a folder for its data."),
        // After a quit during a job, the hidden app lives on until its engine has stopped
        // the job, which a large decode can put seconds away.
        StartError::AlreadyRunning(_) => format!(
            "{APP_NAME} is already running. Its window has your jobs. If you have just \
             closed it, it may still be finishing: try again in a moment."
        ),
        StartError::Engine { db, error } => database_text(db, error),
        StartError::Io { db, error } => database_text(db, error),
        StartError::Thread(error) => sentence(&format!(
            "{APP_NAME} cannot start one of its threads: {error}"
        )),
    }
}

/// What the window says when the database at `db` cannot be opened, for `error`: the
/// path once (SQLite's words name it already, and then the sentence does not), and a
/// full stop at the end, unless the error's words end with one.
fn database_text(db: &Path, error: &dyn std::fmt::Display) -> String {
    let error = error.to_string();
    let shown = display_path(db);
    let names_the_path = error.contains(&shown) || error.contains(&db.display().to_string());
    let mut text = if names_the_path {
        format!("{APP_NAME} cannot open its database: {error}")
    } else {
        format!("{APP_NAME} cannot open its database at {shown}: {error}")
    };
    if !text.ends_with('.') {
        text.push('.');
    }
    text
}

/// The window of an app that could not start: why, and a way out; for a database that
/// cannot be opened, a way to its folder too.
pub struct FailedStart {
    /// What went wrong, as [`failed_start_text`] says it.
    text: String,
    /// The folder of the database that cannot be opened, which `Open the folder` shows.
    folder: Option<PathBuf>,
    platform: Arc<dyn Platform>,
    logo: TextureHandle,
    /// Why the folder could not be shown, sent by the reveal's thread and taken in.
    post: Sender<Notice>,
    posted: Receiver<Notice>,
    /// Why the folder could not be shown, the last time.
    problem: Option<String>,
    /// `Quit` has been given the keyboard, as a question's cautious answer is when it
    /// opens, so Enter and Space quit at once.
    focused: bool,
}

impl FailedStart {
    /// Installs the theme and loads the logo; `platform` shows the folder. Call once,
    /// before the first frame.
    pub fn new(context: &egui::Context, error: &StartError, platform: Arc<dyn Platform>) -> Self {
        theme::install(context);
        let (post, posted) = crossbeam_channel::unbounded();
        Self {
            text: failed_start_text(error),
            folder: error.data_folder().map(Path::to_path_buf),
            platform,
            logo: identity::logo_texture(context),
            post,
            posted,
            problem: None,
            focused: false,
        }
    }

    /// Draws one frame: in the middle of the window a card, framed as the questions are,
    /// with the logo and what went wrong, then `Open the folder` (for the database's
    /// failure) and `Quit`, at the end of the row, and why the folder could not be shown,
    /// when it could not.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        if let Some(notice) = self.posted.try_iter().last() {
            self.problem = Some(notice.text);
        }
        let palette = theme::palette(ui.ctx());
        let frame = egui::Frame::central_panel(ui.style()).fill(palette.window);
        // On the panel's colour, so its buttons, which are `surface`, stand out.
        let card = egui::Frame::popup(ui.style())
            .fill(palette.panel)
            .inner_margin(MARGIN);
        egui::CentralPanel::default().frame(frame).show(ui, |ui| {
            let height = ui.available_height();
            // Roughly half the height of the card, so it sits in the middle.
            ui.add_space((height / 2.0 - 80.0).max(0.0));
            ui.vertical_centered(|ui| {
                // A short measure, so a long path wraps into lines that read easily.
                let width = WIDTH.min(ui.available_width());
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 0.0),
                    egui::Layout::top_down(egui::Align::Min),
                    |ui| {
                        card.show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.card(ui);
                        });
                    },
                );
            });
        });
    }

    /// What the card holds.
    fn card(&mut self, ui: &mut egui::Ui) {
        ui.add(egui::Image::new((
            self.logo.id(),
            egui::Vec2::splat(LOGO_SIZE),
        )));
        ui.add_space(12.0);
        ui.add(egui::Label::new(self.text.as_str()).wrap());
        ui.add_space(16.0);
        let row = egui::vec2(ui.available_width(), widgets::BUTTON_HEIGHT);
        let layout = egui::Layout::right_to_left(egui::Align::Center);
        ui.allocate_ui_with_layout(row, layout, |ui| {
            let quit = widgets::text_button(ui, "Quit", Emphasis::Normal);
            if !self.focused {
                quit.request_focus();
                self.focused = true;
            }
            if quit.clicked() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
            if self.folder.is_some()
                && widgets::text_button(ui, "Open the folder", Emphasis::Normal).clicked()
            {
                self.open_the_folder(ui.ctx());
            }
        });
        if let Some(problem) = &self.problem {
            ui.add_space(8.0);
            ui.add(egui::Label::new(problem.as_str()).wrap());
        }
    }

    /// Shows the database's folder in the file manager, on a thread of its own, as every
    /// reveal: the desktop can take long to answer, and the window must not wait for it.
    fn open_the_folder(&mut self, context: &egui::Context) {
        let Some(folder) = self.folder.clone() else {
            return;
        };
        let post = self.post.clone();
        let context = context.clone();
        let spawned = reveal_in_the_background(
            Arc::clone(&self.platform),
            folder.clone(),
            Reveal::Folder,
            move |notice| {
                // Nothing receives once the window is gone: nobody is left to tell.
                if post.send(notice).is_ok() {
                    context.request_repaint();
                }
            },
        );
        if let Err(error) = spawned {
            self.problem = Some(format!("Cannot show {}: {error}", display_path(&folder)));
        }
    }
}

impl eframe::App for FailedStart {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use eframe::egui::{Pos2, RawInput, Rect, Vec2};
    use photostacker_core::EngineError;

    use super::*;
    use crate::platform::Native;

    #[test]
    fn a_failed_start_renders_in_the_smallest_window() {
        let context = egui::Context::default();
        let mut failed = FailedStart::new(&context, &StartError::NoAppDirs, Arc::new(Native));
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(900.0, 560.0))),
            ..RawInput::default()
        };
        let mut output = context.run_ui(input, |ui| failed.show(ui));
        output.textures_delta.clear();
    }

    #[test]
    fn the_database_failure_names_its_path_once_and_ends_with_a_full_stop() {
        let db = PathBuf::from("/data/photostacker/photostacker.db");
        let shown = display_path(&db);
        // The error names the path (SQLite's words do): the sentence leaves it out.
        let named = StartError::Engine {
            db: db.clone(),
            error: EngineError::Invalid(format!("unable to open database file: {shown}")),
        };
        assert_eq!(
            failed_start_text(&named),
            format!(
                "Photostacker cannot open its database: unable to open database file: {shown}."
            )
        );
        // It does not: the sentence names it.
        let unnamed = StartError::Io {
            db: db.clone(),
            error: io::Error::other("permission denied"),
        };
        assert_eq!(
            failed_start_text(&unnamed),
            format!("Photostacker cannot open its database at {shown}: permission denied.")
        );
        // An error that ends a sentence already gets no second full stop.
        let stopped = StartError::Engine {
            db,
            error: EngineError::Invalid("the disk is full.".into()),
        };
        assert_eq!(
            failed_start_text(&stopped),
            format!("Photostacker cannot open its database at {shown}: the disk is full.")
        );
        // A thread that cannot start is no database's failure.
        assert_eq!(
            failed_start_text(&StartError::Thread(io::Error::other(
                "Resource temporarily unavailable"
            ))),
            "Photostacker cannot start one of its threads: Resource temporarily unavailable."
        );
        // The other two are as they were.
        assert_eq!(
            failed_start_text(&StartError::NoAppDirs),
            "Photostacker cannot find a folder for its data."
        );
        assert_eq!(
            failed_start_text(&StartError::AlreadyRunning(PathBuf::from("/x.db"))),
            "Photostacker is already running. Its window has your jobs. If you have just \
             closed it, it may still be finishing: try again in a moment."
        );
    }
}
