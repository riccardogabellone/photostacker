//! The window: a toolbar on top, the jobs on the left, the inspector on the right, a
//! status bar at the bottom and the job, the new-job form or Settings in the centre (spec
//! §9.2), with the open question as a dialog over them and the notices as toasts. The
//! window also takes what is dropped on it and the shortcuts, the whole window's and the
//! review's (§9.3); asked to close while a job runs, it asks first
//! ([`App::on_close_request`]). When the app cannot start, `main` opens
//! [`crate::failed_start::FailedStart`] instead.
//!
//! The side panels' widths are the preferences `panels.jobs` and `panels.inspector`:
//! each frame a panel is given its preference, or less when the window is too narrow for
//! it, and a width that comes back other than the one given is the user's resize, which
//! becomes the preference. The window's inner size and the panels' sizes are saved once
//! they have changed and stayed the same for a second (`sizes::Settle`), and when the window
//! closes.
//!
//! The `App::show(ui)` split, which lets tests draw frames without a window, is
//! `genetic_algo`'s.

use std::path::PathBuf;
use std::time::Duration;

use eframe::egui::{
    self, Color32, FontId, Key, Modifiers, RichText, Stroke, TextFormat, TextureHandle,
    ThemePreference, text::LayoutJob,
};
use egui_material_icons::icons::{ICON_ERROR, ICON_INFO, ICON_WARNING};
use egui_notify::{Anchor, Toast, Toasts};
use photostacker_core::engine::JobId;
use photostacker_core::model::JobStatus;
use photostacker_core::models;

use crate::{
    controller::{self, Controller, Notice, NoticeLevel, Question, View},
    format::{self, download_question},
    identity,
    inspect::Inspect,
    prefs::{Panels, ThemeChoice},
    sizes::{DrawnPanels, Settle, giving, ordinary_size, resized_by_the_user},
    theme,
    thumbs::ThumbCache,
    views,
    widgets::{self, Emphasis},
};

const TOOLBAR_HEIGHT: f32 = 48.0;
/// The space above and below the status bar's one row of text.
const STATUS_PADDING: i8 = 4;
const JOBS_MIN_WIDTH: f32 = 200.0;
const JOBS_MAX_WIDTH: f32 = 400.0;
const INSPECTOR_MIN_WIDTH: f32 = 260.0;
/// The largest share of the window's width the inspector may take: on a narrow window it
/// gives way before the stacks do.
const INSPECTOR_MAX_SHARE: f32 = 0.4;
/// The narrowest the centre gets while the window is at least 900 px wide: the side
/// panels give way first, the jobs list down to its least width, the inspector to its.
const CENTRE_MIN_WIDTH: f32 = 400.0;
/// The widest a question's text runs before it wraps.
const QUESTION_TEXT_WIDTH: f32 = 360.0;
/// The widest a toast's text runs before it wraps: a long path must not run off the window.
const TOAST_TEXT_WIDTH: f32 = 360.0;
/// The gap between the toasts and the window's right edge, and below the toolbar.
const TOAST_MARGIN: f32 = 12.0;
/// What the centre says while files are dragged over the window.
const DROP_HINT: &str = "Drop to start a new job";
/// The gap between the centre's edge and the outline it shows while files hover.
const DROP_OUTLINE_INSET: f32 = 8.0;
/// What the dialog asks before the job named `name` is deleted.
fn delete_question(name: &str) -> String {
    format!("Delete {name}? Your photos and the stacks already written are not touched.")
}

/// `Write <n> stacks`, the review's action, for `n` stacks.
fn write_label(stacks: usize) -> String {
    format!("Write {}", format::count(stacks, "stack", "stacks"))
}

/// What the dialog asks before `to_write` stacks are written while `proposed` are still
/// proposed: the write ends the review of the job (spec §5.7).
fn write_question(to_write: usize, proposed: usize) -> String {
    let left = if proposed == 1 {
        "1 stack is".to_owned()
    } else {
        format!("{proposed} stacks are")
    };
    format!(
        "{} now? {left} still proposed and will not be written. The review of this job ends \
         with the write.",
        write_label(to_write)
    )
}

/// What the centre says, above what it shows, once the engine's worker has stopped.
pub const ENGINE_STOPPED: &str =
    "Photostacker's engine has stopped. Close and reopen the app; your jobs are kept.";

/// What the dialog asks when the window is closed while a job is queued, scanning or
/// writing.
const QUIT_WHILE_WORKING: &str = "A job is still running. If you quit now it stops, and \
                                  continues the next time you open Photostacker.";

/// How much of the review the last frame laid out, as the views report it: the tests
/// check that a large job costs a frame only its visible rows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStats {
    /// The rows of the stack list laid out.
    pub stack_rows: usize,
    /// The rows of the tray laid out.
    pub tray_rows: usize,
}

/// The app's window.
pub struct App {
    /// What the last frame laid out.
    stats: FrameStats,
    logo: TextureHandle,
    controller: Controller,
    /// The textures of the selected job's review thumbnails.
    thumbs: ThumbCache,
    /// The job whose thumbnails `thumbs` holds: the one selected at the last frame.
    thumbs_job: Option<JobId>,
    /// The inspector's previews and stack predictions.
    inspect: Inspect,
    toasts: Toasts,
    /// The open question once its cautious answer has been given the keyboard.
    focused_question: Option<Question>,
    /// A text field had the keyboard when the last frame ended. egui takes it away when
    /// Esc is pressed, before the window sees the key, so this says whether that Esc
    /// left a field or was meant for the window.
    typing: bool,
    /// A control had the keyboard when the last frame ended, a text field or another
    /// (as `typing`, for the same reason): its keys are its own, a slider's arrows, a
    /// button's Enter, so the review's shortcuts leave them to it.
    focused: bool,
    /// A shortcut stepped the selection this frame: the review scrolls its primary stack
    /// into view.
    reveal_primary: bool,
    /// The theme last applied to egui: a change of the preference, from the toolbar's
    /// toggle or from Settings, is applied at the end of the frame that made it.
    applied_theme: ThemeChoice,
    /// The widths the last frame drew the jobs list and the inspector at.
    drawn: DrawnPanels,
    /// The window's inner size, saved once it settles: from the first size seen.
    window_size: Option<Settle<[f32; 2]>>,
    /// The panels' sizes, saved once they settle.
    panel_sizes: Settle<Panels>,
    /// The user answered `Quit` to [`Question::Quit`]: the next request to close the
    /// window closes it.
    quit_confirmed: bool,
}

impl App {
    /// Installs the theme, applies the saved choice of theme and loads the logo. Call
    /// once, before the first frame.
    pub fn new(context: &egui::Context, controller: Controller) -> Self {
        theme::install(context);
        context.set_theme(ThemePreference::from(controller.prefs.theme));
        let panel_sizes = Settle::new(controller.prefs.panels);
        let controller_theme = controller.prefs.theme;
        Self {
            stats: FrameStats::default(),
            logo: identity::logo_texture(context),
            controller,
            thumbs: ThumbCache::new(context),
            thumbs_job: None,
            inspect: Inspect::new(context),
            toasts: Toasts::default()
                .with_anchor(Anchor::TopRight)
                .with_margin(egui::vec2(TOAST_MARGIN, TOOLBAR_HEIGHT + TOAST_MARGIN)),
            focused_question: None,
            typing: false,
            focused: false,
            reveal_primary: false,
            applied_theme: controller_theme,
            drawn: DrawnPanels::default(),
            window_size: None,
            panel_sizes,
            quit_confirmed: false,
        }
    }

    /// What a request to close the window does (spec §9.2): while a job is queued,
    /// scanning or writing, the close is cancelled and [`Question::Quit`] asked, in place
    /// of a question already open; while it is open, a request asks nothing more and
    /// still waits for its answer, even once the job has ended. After its `Quit`, and with
    /// no job running, the window closes. A model's download does not ask: its part is
    /// kept, and the next download resumes it.
    pub fn on_close_request(&mut self) -> CloseDecision {
        if self.quit_confirmed {
            return CloseDecision::Close;
        }
        if self.controller.question() == Some(&Question::Quit) {
            return CloseDecision::Ask;
        }
        if !self.controller.is_working() {
            return CloseDecision::Close;
        }
        self.controller.ask(Question::Quit);
        CloseDecision::Ask
    }

    /// Gives this frame's request to close the window, if any, to
    /// [`App::on_close_request`], and cancels the close when it asks first; whether the
    /// window closes. Asking again in the same frame changes nothing.
    fn take_close_request(&mut self, context: &egui::Context) -> bool {
        if !context.input(|input| input.viewport().close_requested()) {
            return false;
        }
        // So a job that has just started or ended counts.
        self.controller.poll();
        match self.on_close_request() {
            CloseDecision::Close => true,
            CloseDecision::Ask => {
                context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                false
            }
        }
    }

    pub fn controller(&self) -> &Controller {
        &self.controller
    }

    pub fn controller_mut(&mut self) -> &mut Controller {
        &mut self.controller
    }

    /// What the last frame laid out.
    pub fn stats(&self) -> &FrameStats {
        &self.stats
    }

    /// Whether the inspector waits for a preview or a prediction it asked for.
    pub fn inspecting(&self) -> bool {
        self.inspect.busy()
    }

    /// The inspector's previews and predictions. For tests.
    pub fn inspect(&self) -> &Inspect {
        &self.inspect
    }

    /// Draws one frame. Only a toast counting down, or thumbnails left to upload, asks for
    /// the next: the engine's events, the backend's replies and each thumbnail loaded wake
    /// the window, so an app with nothing running and no toast stays idle.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.stats = FrameStats::default();
        self.reveal_primary = false;
        self.controller.poll();
        let closing = self.take_close_request(ui.ctx());
        self.follow_the_selected_job();
        self.thumbs.poll(ui.ctx());
        self.inspect.poll(ui.ctx());
        self.take_drops(ui.ctx());
        self.take_shortcuts(ui.ctx());
        self.toast_notices(ui.ctx());
        // A drop while a question is open is ignored, so nothing invites one then.
        let hovering = self.controller.question().is_none()
            && ui.ctx().input(|input| !input.raw.hovered_files.is_empty());
        // Before any panel takes its share.
        let window_width = ui.available_width();

        egui::Panel::top("toolbar")
            .exact_size(TOOLBAR_HEIGHT)
            .show(ui, |ui| {
                views::toolbar::show(ui, &mut self.controller, &self.logo);
            });
        // Declared before the side panels, so it runs under them across the whole width.
        // Its height is set: a bottom panel keeps the height it had and clips what grows.
        let status_frame = egui::Frame::side_top_panel(ui.style())
            .inner_margin(egui::Margin::symmetric(8, STATUS_PADDING));
        let status_height =
            (ui.text_style_height(&egui::TextStyle::Body) + 2.0 * f32::from(STATUS_PADDING)).ceil();
        egui::Panel::bottom("status")
            .frame(status_frame)
            .exact_size(status_height)
            .show(ui, |ui| views::status::show(ui, &self.controller));
        // The side panels give way to keep the centre CENTRE_MIN_WIDTH wide: their widest
        // is what the window leaves them, each frame, so a narrowed window narrows them,
        // and a widened one gives them their preferences back.
        let jobs_max = (window_width - CENTRE_MIN_WIDTH - INSPECTOR_MIN_WIDTH)
            .clamp(JOBS_MIN_WIDTH, JOBS_MAX_WIDTH);
        let jobs_width = self
            .controller
            .prefs
            .panels
            .jobs
            .clamp(JOBS_MIN_WIDTH, jobs_max);
        let jobs = egui::Panel::left("jobs")
            .resizable(true)
            .default_size(jobs_width)
            .size_range(giving(
                jobs_width,
                self.drawn.jobs,
                JOBS_MIN_WIDTH,
                jobs_max,
            ))
            .show(ui, |ui| views::jobs::show(ui, &mut self.controller));
        let jobs_drawn = jobs.response.rect.width();
        let inspector_max = (window_width * INSPECTOR_MAX_SHARE)
            .min(window_width - CENTRE_MIN_WIDTH - jobs_drawn)
            .max(INSPECTOR_MIN_WIDTH);
        let inspector_width = self
            .controller
            .prefs
            .panels
            .inspector
            .clamp(INSPECTOR_MIN_WIDTH, inspector_max);
        let inspector_range = giving(
            inspector_width,
            self.drawn.inspector,
            INSPECTOR_MIN_WIDTH,
            inspector_max,
        );
        let inspector = egui::Panel::right("inspector")
            .resizable(true)
            .default_size(inspector_width)
            .size_range(inspector_range)
            .show(ui, |ui| {
                views::inspector::show(
                    ui,
                    &mut self.controller,
                    &mut self.thumbs,
                    &mut self.inspect,
                );
            });
        let inspector_drawn = inspector.response.rect.width();
        self.drawn = DrawnPanels {
            jobs: Some(jobs_drawn),
            inspector: Some(inspector_drawn),
        };
        let panels = &mut self.controller.prefs.panels;
        if resized_by_the_user(jobs_width, jobs_drawn) {
            panels.jobs = jobs_drawn;
        }
        if resized_by_the_user(inspector_width, inspector_drawn) {
            panels.inspector = inspector_drawn;
        }
        // The centre is the window's own colour, a step back from the panels around it.
        let centre_frame =
            egui::Frame::central_panel(ui.style()).fill(theme::palette(ui.ctx()).window);
        let reveal = self.reveal_primary;
        let stats = egui::CentralPanel::default()
            .frame(centre_frame)
            .show(ui, |ui| {
                // A line that stays, whatever the centre shows: the views keep the last
                // progress the engine sent.
                if self.controller.engine_stopped().is_some() {
                    let danger = theme::palette(ui.ctx()).danger;
                    views::job::banner(ui, ICON_ERROR, "Error", danger, ENGINE_STOPPED);
                    ui.add_space(12.0);
                }
                if hovering {
                    drop_hint(ui);
                    FrameStats::default()
                } else if self.controller.view() == &View::NewJob {
                    views::new_job::show(ui, &mut self.controller);
                    FrameStats::default()
                } else if self.controller.view() == &View::Settings {
                    views::settings::show(ui, &mut self.controller);
                    FrameStats::default()
                } else {
                    views::job::show(ui, &mut self.controller, &mut self.thumbs, reveal)
                }
            })
            .inner;
        self.stats = stats;

        self.question(ui.ctx());
        // Theme-dependent, and egui-notify takes its shadow by value.
        let shadow = ui.style().visuals.popup_shadow;
        self.toasts = std::mem::take(&mut self.toasts).with_shadow(shadow);
        self.toasts.show(ui.ctx());
        self.typing = ui.ctx().text_edit_focused();
        self.focused = ui.ctx().memory(|memory| memory.focused().is_some());
        self.apply_the_theme(ui.ctx());
        self.save_settled_sizes(ui.ctx(), closing);
    }

    /// Applies the theme preference once it changes: at once, in the frame that changed
    /// it. Only a change: the screenshot example forces a theme of its own.
    fn apply_the_theme(&mut self, context: &egui::Context) {
        let theme = self.controller.prefs.theme;
        if theme != self.applied_theme {
            self.applied_theme = theme;
            context.set_theme(ThemePreference::from(theme));
        }
    }

    /// Saves the window's inner size and the panels' sizes once each has changed and
    /// stayed the same for a second, and at once when the window closes (`closing`; a
    /// close that is cancelled keeps them waiting). The window's size is taken only while
    /// it is neither maximised, minimised nor in full screen: the last ordinary size
    /// stays; and at a zoom of 1, so egui's zoom, which the next start does not keep,
    /// never shrinks it. While a change waits, a frame is asked for when its second is
    /// over, or an idle window would never save it.
    fn save_settled_sizes(&mut self, context: &egui::Context, closing: bool) {
        let zoom = context.zoom_factor();
        let (time, size) =
            context.input(|input| (input.time, ordinary_size(input.viewport(), zoom)));
        if let Some(size) = size {
            self.window_size
                .get_or_insert_with(|| Settle::new(size))
                .see(size, time);
        }
        self.panel_sizes.see(self.controller.prefs.panels, time);
        if closing {
            self.save_waiting_sizes();
        } else {
            let settled_window = self
                .window_size
                .as_mut()
                .and_then(|window| window.due(time));
            if let Some(size) = settled_window {
                self.controller.set_window(size);
            }
            if let Some(panels) = self.panel_sizes.due(time) {
                self.controller.set_panels(panels);
            }
        }
        let next = [
            self.window_size.as_ref().and_then(Settle::due_at),
            self.panel_sizes.due_at(),
        ]
        .into_iter()
        .flatten()
        .reduce(f64::min);
        if let Some(at) = next {
            context.request_repaint_after(Duration::from_secs_f64((at - time).max(0.0)));
        }
    }

    /// Saves at once the window's size and the panels' sizes that wait for their second:
    /// the window is closing.
    fn save_waiting_sizes(&mut self) {
        if let Some(size) = self.window_size.as_mut().and_then(Settle::flush) {
            self.controller.set_window(size);
        }
        if let Some(panels) = self.panel_sizes.flush() {
            self.controller.set_panels(panels);
        }
    }

    /// Once another job is selected, or none (the selected one was deleted), the
    /// thumbnails of the job left go: only one job's are shown at a time.
    fn follow_the_selected_job(&mut self) {
        let selected = self.controller.selected().map(|job| job.id);
        if selected == self.thumbs_job {
            return;
        }
        if let Some(left) = self.thumbs_job {
            self.thumbs.forget_job(&self.controller.job_thumbs(left));
        }
        self.thumbs_job = selected;
    }

    /// Gives the controller the files and folders dropped on the window this frame,
    /// unless a question is open.
    fn take_drops(&mut self, context: &egui::Context) {
        let dropped: Vec<PathBuf> = context.input(|input| {
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        });
        if !dropped.is_empty() && self.controller.question().is_none() {
            self.controller.dropped(&dropped);
        }
    }

    /// Ctrl+N (Cmd+N on macOS) opens the new-job form; Esc closes it, unless it is being
    /// submitted (as its Cancel, which is disabled then), and closes Settings as their
    /// Done does. In a review, the keys of [`App::review_shortcuts`]; in a job's writing
    /// and results, Esc clears the selection. None acts while a question is open or a
    /// text field has the keyboard, and Esc leaves an open popup to close itself.
    fn take_shortcuts(&mut self, context: &egui::Context) {
        if self.controller.question().is_some() || self.typing {
            return;
        }
        let escape = |context: &egui::Context| {
            !egui::Popup::is_any_open(context)
                && context.input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape))
        };
        if context.input_mut(|input| input.consume_key(Modifiers::COMMAND, Key::N)) {
            self.controller.open_new_job(None);
        } else if self.controller.view() == &View::NewJob {
            if !self.controller.form().is_some_and(|form| form.submitting) && escape(context) {
                self.controller.close_form();
            }
        } else if self.controller.view() == &View::Settings {
            if escape(context) {
                self.controller.close_settings();
            }
        } else if self.reviewing() {
            self.review_shortcuts(context);
        } else if self.showing_results() && !self.focused && escape(context) {
            self.controller.clear_selection();
        }
    }

    /// Whether the centre shows the selected job's writing and results, loaded.
    fn showing_results(&self) -> bool {
        let Some(job) = self.controller.selected() else {
            return false;
        };
        self.controller.view() == &View::Job
            && controller::shows_results(job)
            && self
                .controller
                .review()
                .is_some_and(|review| review.job == job.id && review.loaded())
    }

    /// Whether the centre shows the selected job's review, loaded.
    fn reviewing(&self) -> bool {
        let Some(job) = self
            .controller
            .selected()
            .filter(|job| job.status == JobStatus::ReviewPending)
        else {
            return false;
        };
        self.controller.view() == &View::Job
            && self
                .controller
                .review()
                .is_some_and(|review| review.job == job.id && review.loaded())
    }

    /// The review's keys (spec §9.3): Enter confirms the selected stacks, Delete (and
    /// Backspace on macOS, whose keyboards name it Delete) rejects them, Up and Down step
    /// the stacks, Left and Right the photos of the primary one, and Esc clears the
    /// selection. While a control has the keyboard they are its own (a slider's arrows, a
    /// button's Enter), and while a popup or a menu is open (`Move to stack…`, opened with
    /// the mouse) they are its, Esc closing it: none acts on the review behind it.
    fn review_shortcuts(&mut self, context: &egui::Context) {
        if self.focused || egui::Popup::is_any_open(context) {
            return;
        }
        let keys = context.input_mut(review_keys);
        if keys.confirm {
            self.controller.confirm_selected();
        }
        if keys.reject {
            self.controller.reject_selected();
        }
        if keys.stacks != 0 || keys.photos != 0 {
            self.controller.step_selection(keys.stacks, keys.photos);
            self.reveal_primary = true;
        }
        if keys.clear {
            self.controller.clear_selection();
        }
    }

    /// Turns the new notices into toasts: the controller has made equal ones of one frame
    /// one, and many a few (`Controller::take_notices`).
    fn toast_notices(&mut self, context: &egui::Context) {
        for notice in self.controller.take_notices() {
            self.toasts.add(toast(context, &notice));
        }
    }

    /// The open question as a dialog over a dimmed window. The cautious answer comes
    /// first and has the keyboard when the dialog opens; Esc, or a click outside the
    /// dialog, gives it too. The action is filled with `danger` when it destroys
    /// something, with the accent when it does not (a download, quitting while a job
    /// runs: the job continues at the next start).
    fn question(&mut self, context: &egui::Context) {
        let Some(question) = self.controller.question().cloned() else {
            self.focused_question = None;
            return;
        };
        let (text, yes, no, emphasis) = match question {
            Question::DeleteJob(job) => (
                delete_question(self.controller.job_name(job)),
                "Delete job".to_owned(),
                "Keep",
                Emphasis::Danger,
            ),
            Question::DownloadModel(id) => (
                models::model(id).map_or_else(
                    || format!("Download {id}?"),
                    |spec| download_question(spec.name, spec.size, spec.urls),
                ),
                "Download".to_owned(),
                "Not now",
                Emphasis::Primary,
            ),
            Question::Quit => (
                QUIT_WHILE_WORKING.to_owned(),
                "Quit".to_owned(),
                "Keep working",
                Emphasis::Primary,
            ),
            Question::Write {
                to_write, proposed, ..
            } => (
                write_question(to_write, proposed),
                write_label(to_write),
                "Keep reviewing",
                Emphasis::Primary,
            ),
        };
        // The action's name, when the window may show a button of its text under the
        // dialog: the review's bar has a `Write <n> stacks` of its own.
        let yes_name = match &question {
            Question::Write { .. } => Some(format!("{yes} now")),
            _ => None,
        };
        let opened = self.focused_question.as_ref() != Some(&question);
        let palette = theme::palette(context);
        // On the panel's colour, so its buttons, which are `surface`, stand out.
        let frame = egui::Frame::popup(&context.global_style()).fill(palette.panel);
        let dialog = egui::Modal::new(egui::Id::unique("question"))
            .frame(frame)
            .backdrop_color(palette.window.gamma_multiply(0.6))
            .show(context, |ui| {
                ui.set_max_width(QUESTION_TEXT_WIDTH);
                ui.add(egui::Label::new(text).wrap());
                ui.add_space(12.0);
                let mut answer = None;
                // Right to left: the action that does it ends the row.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let action = widgets::text_button(ui, &yes, emphasis);
                    if let Some(name) = &yes_name {
                        action.widget_info(|| {
                            egui::WidgetInfo::labeled(egui::Role::Button, true, name)
                        });
                    }
                    if action.clicked() {
                        answer = Some(true);
                    }
                    let keep = widgets::text_button(ui, no, Emphasis::Normal);
                    if opened {
                        keep.request_focus();
                    }
                    if keep.clicked() {
                        answer = Some(false);
                    }
                });
                answer
            });
        let quit = question == Question::Quit;
        self.focused_question = Some(question);
        let answer = dialog
            .inner
            .or_else(|| dialog.should_close().then_some(false));
        if let Some(yes) = answer {
            self.controller.answer(yes);
            // The window's own answer: the close it asked about goes through now. Hidden
            // first: dropping the controller waits for the engine to stop its job at the
            // next check, which a large decode can put seconds away, and a window left on
            // screen meanwhile would not answer ("Not responding" on Windows).
            if yes && quit {
                self.quit_confirmed = true;
                context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
                context.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

/// What the review's keys ask for in one frame.
#[derive(Debug, Default, PartialEq, Eq)]
struct ReviewKeys {
    confirm: bool,
    reject: bool,
    /// Steps down the stacks (up when negative).
    stacks: i32,
    /// Steps along the primary stack's photos (back when negative).
    photos: i32,
    clear: bool,
}

/// Takes the review's keys out of `input`, so no widget sees them too. Enter and
/// Delete act once per press: a held key's repeats are taken but ignored, or holding it
/// would send the decision again for every selected stack many times a second. The
/// arrows count their repeats, so a held arrow keeps stepping.
fn review_keys(input: &mut egui::InputState) -> ReviewKeys {
    let mut steps = |key| input.count_and_consume_key(Modifiers::NONE, key) as i32;
    let stacks = steps(Key::ArrowDown) - steps(Key::ArrowUp);
    let photos = steps(Key::ArrowRight) - steps(Key::ArrowLeft);
    let mut reject = pressed_once(input, Key::Delete);
    if cfg!(target_os = "macos") {
        reject |= pressed_once(input, Key::Backspace);
    }
    ReviewKeys {
        confirm: pressed_once(input, Key::Enter),
        reject,
        stacks,
        photos,
        clear: input.consume_key(Modifiers::NONE, Key::Escape),
    }
}

/// Takes every press of `key` without modifiers out of `input`; whether one of them
/// was a new press, not a key-repeat.
fn pressed_once(input: &mut egui::InputState, key: Key) -> bool {
    let mut pressed = false;
    input.events.retain(|event| match event {
        egui::Event::Key {
            key: pressed_key,
            pressed: true,
            repeat,
            modifiers,
            ..
        } if *pressed_key == key && modifiers.matches_logically(Modifiers::NONE) => {
            pressed |= !repeat;
            false
        }
        _ => true,
    });
    pressed
}

/// A notice's toast: its level's icon in its colour, then its text, wrapped. It goes on
/// its own after 4 s (information), 6 s (a warning) or 8 s (an error), and cannot be
/// closed by hand: egui-notify's close button is a glyph the bundled fonts lack.
fn toast(context: &egui::Context, notice: &Notice) -> Toast {
    let palette = theme::palette(context);
    let (icon, color, seconds) = match notice.level {
        NoticeLevel::Info => (ICON_INFO, palette.accent, 4),
        NoticeLevel::Warning => (ICON_WARNING, palette.warning, 6),
        NoticeLevel::Error => (ICON_ERROR, palette.danger, 8),
    };
    let body = context.global_style().text_styles[&egui::TextStyle::Body].clone();
    let mut job = LayoutJob::default();
    job.append(
        icon.codepoint,
        0.0,
        TextFormat::simple(FontId::new(body.size, icon.font_family()), color),
    );
    // The placeholder colour takes the toast's text colour, from the theme shown.
    job.append(
        &notice.text,
        6.0,
        TextFormat::simple(body, Color32::PLACEHOLDER),
    );
    job.wrap.max_width = TOAST_TEXT_WIDTH;
    let caption = context.fonts_mut(|fonts| fonts.layout_job(job));
    let mut toast = Toast::basic(caption);
    toast
        .duration(Duration::from_secs(seconds))
        .closable(false)
        .show_progress_bar(false);
    toast
}

impl eframe::App for App {
    /// Also while the window is minimised or hidden, when eframe runs only this: a close
    /// from the taskbar then asks too, and the question waits for the window to be shown.
    /// A close that goes through saves the sizes still waiting here, since no `show` may
    /// follow: after `Quit` the window is hidden before it is closed.
    fn logic(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        if self.take_close_request(context) {
            self.save_waiting_sizes();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

/// What a request to close the window does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseDecision {
    /// The window closes.
    Close,
    /// The close is cancelled, and the app asks first.
    Ask,
}

/// The centre while files are dragged over the window, whatever it showed: an outline in
/// the accent and what dropping them does.
fn drop_hint(ui: &mut egui::Ui) {
    let palette = theme::palette(ui.ctx());
    let outline = ui.max_rect().shrink(DROP_OUTLINE_INSET);
    ui.painter().rect_stroke(
        outline,
        8.0,
        Stroke::new(2.0, palette.accent),
        egui::StrokeKind::Inside,
    );
    let height = ui.available_height();
    ui.vertical_centered(|ui| {
        // Roughly half the height of the text, so it sits in the middle.
        ui.add_space((height / 2.0 - 12.0).max(0.0));
        ui.label(RichText::new(DROP_HINT).heading().color(palette.text));
    });
    ui.take_available_space();
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use std::path::PathBuf;

    use eframe::egui::{Pos2, RawInput, Rect, Vec2};
    use photostacker_core::model::JobStatus;
    use photostacker_core::{AppDirs, JobConfig, Store};
    use tempfile::TempDir;

    use super::*;
    use crate::controller::{RuntimeCheck, StartOptions};
    use crate::platform::Native;
    use crate::prefs::ThemeChoice;
    use crate::thumbs::Thumb;

    fn input(width: f32, height: f32) -> RawInput {
        RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(width, height))),
            ..RawInput::default()
        }
    }

    fn frame(app: &mut App, context: &egui::Context, input: RawInput) -> egui::FullOutput {
        let mut output = context.run_ui(input, |ui| app.show(ui));
        output.textures_delta.clear(); // There is no renderer here to consume them.
        output
    }

    fn controller(dirs: &AppDirs) -> Controller {
        Controller::start(StartOptions {
            dirs: dirs.clone(),
            wake: Arc::new(|| {}),
            platform: Arc::new(Native),
            embedder: None,
            installer: None,
            runtime: RuntimeCheck {
                path: PathBuf::from("/opt/onnxruntime/lib/libonnxruntime.so"),
                problem: None,
            },
        })
        .unwrap()
    }

    /// An app on a temporary data folder, which it must not outlive.
    fn app(context: &egui::Context) -> (TempDir, App) {
        let data = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(data.path().to_path_buf());
        (data, App::new(context, controller(&dirs)))
    }

    /// Whether `output` paints `text`.
    fn paints(output: &egui::FullOutput, text: &str) -> bool {
        output.shapes.iter().any(|clipped| match &clipped.shape {
            egui::Shape::Text(shape) => shape.galley.text().contains(text),
            _ => false,
        })
    }

    /// Polls `controller` until it has read the jobs.
    fn read_jobs(controller: &mut Controller) {
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !controller.jobs_loaded() {
            assert!(
                std::time::Instant::now() < deadline,
                "the jobs were never read"
            );
            std::thread::sleep(Duration::from_millis(5));
            controller.poll();
        }
    }

    /// The centre drawn alone from `controller`, which it does not poll: whether it
    /// invites a first job.
    fn centre_invites(controller: &mut Controller) -> bool {
        let context = egui::Context::default();
        let mut thumbs = ThumbCache::new(&context);
        let mut output = context.run_ui(input(800.0, 600.0), |ui| {
            views::job::show(ui, controller, &mut thumbs, false);
        });
        output.textures_delta.clear();
        paints(&output, views::job::INVITATION)
    }

    #[test]
    fn the_centre_invites_a_first_job_only_once_it_knows_there_is_none() {
        // Before its first poll, a controller has not taken in the jobs, whatever the
        // backend has read meanwhile: this is the window's first frame.
        let empty = tempfile::tempdir().unwrap();
        let mut controller_without = controller(&AppDirs::under(empty.path().to_path_buf()));
        assert!(!centre_invites(&mut controller_without));
        read_jobs(&mut controller_without);
        assert!(centre_invites(&mut controller_without));

        let data = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(data.path().to_path_buf());
        std::fs::create_dir_all(&dirs.data).unwrap();
        {
            let store = Store::open(&dirs.db()).unwrap();
            let job = store
                .insert_job(&JobConfig::new(PathBuf::from("/photos/trip")))
                .unwrap();
            store.set_job_status(job, JobStatus::Completed).unwrap();
        }
        let mut controller_with = controller(&dirs);
        assert!(!centre_invites(&mut controller_with));
        read_jobs(&mut controller_with);
        assert_eq!(controller_with.jobs().len(), 1);
        assert!(!centre_invites(&mut controller_with));
    }

    /// A press of `key`, a key-repeat when `repeat`.
    fn key_press(key: Key, repeat: bool) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat,
            modifiers: Modifiers::NONE,
        }
    }

    /// What the review's keys ask for in one frame of `events` on `context`.
    fn review_keys_of(context: &egui::Context, events: Vec<egui::Event>) -> ReviewKeys {
        let mut keys = ReviewKeys::default();
        let raw = RawInput {
            events,
            ..input(800.0, 600.0)
        };
        let mut output = context.run_ui(raw, |ui| {
            keys = ui.ctx().input_mut(review_keys);
        });
        output.textures_delta.clear();
        keys
    }

    #[test]
    fn a_held_enter_or_delete_decides_once_and_a_held_arrow_keeps_stepping() {
        // egui tells a repeat from a press by the keys it knows are down, so the frames
        // share one context, and no key is released.
        let context = egui::Context::default();
        // A press and its repeats in one frame (a slow frame, or a fast repeat).
        let held = review_keys_of(
            &context,
            vec![
                key_press(Key::Enter, false),
                key_press(Key::Enter, true),
                key_press(Key::Enter, true),
                key_press(Key::Delete, false),
                key_press(Key::Delete, true),
                key_press(Key::ArrowUp, false),
                key_press(Key::ArrowDown, false),
                key_press(Key::ArrowDown, true),
                key_press(Key::ArrowDown, true),
            ],
        );
        let expected = ReviewKeys {
            confirm: true,
            reject: true,
            stacks: 2,
            ..ReviewKeys::default()
        };
        assert_eq!(held, expected);
        // The frames after it, while the keys are held, bring only repeats: nothing to
        // decide again, while the arrow keeps stepping.
        let repeats = review_keys_of(
            &context,
            vec![
                key_press(Key::Enter, true),
                key_press(Key::Delete, true),
                key_press(Key::ArrowUp, true),
            ],
        );
        let expected = ReviewKeys {
            stacks: -1,
            ..ReviewKeys::default()
        };
        assert_eq!(repeats, expected);
    }

    #[test]
    fn the_saved_theme_applies_at_start() {
        let data = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(data.path().to_path_buf());
        {
            let mut controller = controller(&dirs);
            controller.set_theme(ThemeChoice::Light);
            // Let the save run, as it would during a session.
            loop {
                controller.poll();
                if controller.is_idle() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        let context = egui::Context::default();
        let _app = App::new(&context, controller(&dirs));
        assert_eq!(
            context.options(|options| options.theme_preference),
            ThemePreference::Light
        );
    }

    /// A frame's input at `time`, of a window whose inner size is `size`, maximised when
    /// `maximised`.
    fn window_input(size: [f32; 2], time: f64, maximised: bool) -> RawInput {
        let mut raw = input(size[0], size[1]);
        raw.time = Some(time);
        let viewport = raw.viewports.entry(egui::ViewportId::ROOT).or_default();
        viewport.inner_rect = Some(Rect::from_min_size(Pos2::ZERO, Vec2::from(size)));
        viewport.maximized = Some(maximised);
        raw
    }

    /// How long the frame that gave `output` asked egui to wait before the next.
    fn repaint_delay(output: &egui::FullOutput) -> Duration {
        output
            .viewport_output
            .values()
            .next()
            .map_or(Duration::MAX, |viewport| viewport.repaint_delay)
    }

    #[test]
    fn a_resized_window_is_saved_once_it_settles() {
        // The rule alone is `sizes::Settle`'s, tested there with explicit times; here the
        // window's frames, with egui's input times: of several sizes within a second, only
        // the last is saved, a second after it was first seen.

        // The window: the size it opened at is not saved; a new one is, once it settles.
        let context = egui::Context::default();
        let (_data, mut app) = app(&context);
        frame(
            &mut app,
            &context,
            window_input([1360.0, 820.0], 0.0, false),
        );
        frame(
            &mut app,
            &context,
            window_input([1000.0, 700.0], 0.2, false),
        );
        frame(
            &mut app,
            &context,
            window_input([1100.0, 750.0], 0.6, false),
        );
        let waiting = frame(
            &mut app,
            &context,
            window_input([1100.0, 750.0], 1.2, false),
        );
        assert_eq!(app.controller().prefs.window, None);
        // An idle window still wakes when the second is over.
        assert!(
            repaint_delay(&waiting) <= Duration::from_millis(410),
            "{:?}",
            repaint_delay(&waiting)
        );
        frame(
            &mut app,
            &context,
            window_input([1100.0, 750.0], 1.6, false),
        );
        assert_eq!(app.controller().prefs.window, Some([1100.0, 750.0]));
        frame(
            &mut app,
            &context,
            window_input([1100.0, 750.0], 5.0, false),
        );
        assert_eq!(app.controller().prefs.window, Some([1100.0, 750.0]));
    }

    #[test]
    fn a_maximised_window_keeps_its_last_ordinary_size() {
        let context = egui::Context::default();
        let (_data, mut app) = app(&context);
        frame(
            &mut app,
            &context,
            window_input([1360.0, 820.0], 0.0, false),
        );
        frame(
            &mut app,
            &context,
            window_input([1920.0, 1080.0], 0.5, true),
        );
        frame(
            &mut app,
            &context,
            window_input([1920.0, 1080.0], 3.0, true),
        );
        assert_eq!(app.controller().prefs.window, None);

        // Resized, then maximised within the second: the ordinary size is the one saved.
        frame(
            &mut app,
            &context,
            window_input([1000.0, 700.0], 4.0, false),
        );
        frame(
            &mut app,
            &context,
            window_input([1920.0, 1080.0], 4.2, true),
        );
        frame(
            &mut app,
            &context,
            window_input([1920.0, 1080.0], 5.1, true),
        );
        assert_eq!(app.controller().prefs.window, Some([1000.0, 700.0]));
    }

    #[test]
    fn a_waiting_size_is_saved_when_the_window_closes() {
        let data = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(data.path().to_path_buf());
        {
            let context = egui::Context::default();
            let mut app = App::new(&context, controller(&dirs));
            frame(
                &mut app,
                &context,
                window_input([1360.0, 820.0], 0.0, false),
            );
            app.controller_mut().prefs.panels.tray = Some(200.0);
            let mut closing = window_input([1000.0, 700.0], 0.2, false);
            closing
                .viewports
                .entry(egui::ViewportId::ROOT)
                .or_default()
                .events
                .push(egui::ViewportEvent::Close);
            frame(&mut app, &context, closing);
            assert_eq!(app.controller().prefs.window, Some([1000.0, 700.0]));
        }
        // Saved before the engine was let go, though nothing waited for it.
        let prefs = controller(&dirs).prefs;
        assert_eq!(prefs.window, Some([1000.0, 700.0]));
        assert_eq!(prefs.panels.tray, Some(200.0));
    }

    #[test]
    fn a_stopped_engine_is_said_above_the_centre_until_the_app_closes() {
        let context = egui::Context::default();
        let (_data, mut app) = app(&context);
        let shown = frame(&mut app, &context, input(900.0, 560.0));
        assert!(!paints(&shown, ENGINE_STOPPED));

        app.controller_mut()
            .take_event(photostacker_core::EngineEvent::EngineStopped {
                message: "a bug in the loop".into(),
            });
        for _ in 0..3 {
            let shown = frame(&mut app, &context, input(900.0, 560.0));
            assert!(paints(&shown, ENGINE_STOPPED));
        }
        // Nothing runs any more, whatever the jobs' last statuses say.
        assert!(!app.controller().engine_busy());
    }

    #[test]
    fn the_smallest_window_renders() {
        let context = egui::Context::default();
        let (_data, mut app) = app(&context);
        frame(&mut app, &context, input(900.0, 560.0));
    }

    #[test]
    fn an_idle_window_stops_repainting() {
        let context = egui::Context::default();
        let (_data, mut app) = app(&context);
        let delay = |output: &egui::FullOutput| {
            output
                .viewport_output
                .values()
                .next()
                .map_or(Duration::MAX, |viewport| viewport.repaint_delay)
        };
        // Fonts loading or a panel settling may take a few frames, but a window with
        // nothing to do must then go idle instead of burning CPU forever.
        let settled = (0..300).any(|_| {
            delay(&frame(&mut app, &context, input(1360.0, 820.0))) > Duration::from_secs(1)
        });
        assert!(settled);
    }

    #[test]
    fn a_window_whose_thumbnail_loaded_goes_idle() {
        let context = egui::Context::default();
        let (data, mut app) = app(&context);
        let path = data.path().join("thumb.jpg");
        image::RgbImage::new(16, 16).save(&path).unwrap();
        assert!(matches!(app.thumbs.get(Some(&path)), Thumb::Loading));
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        while !matches!(app.thumbs.get(Some(&path)), Thumb::Ready(_)) {
            assert!(
                std::time::Instant::now() < deadline,
                "the thumbnail never loaded"
            );
            frame(&mut app, &context, input(1360.0, 820.0));
            std::thread::sleep(Duration::from_millis(1));
        }
        // The loader, with nothing left to do, must not keep the window awake.
        let settled = (0..300).any(|_| {
            let output = frame(&mut app, &context, input(1360.0, 820.0));
            output
                .viewport_output
                .values()
                .next()
                .is_none_or(|viewport| viewport.repaint_delay > Duration::from_secs(1))
        });
        assert!(settled);
    }
}
