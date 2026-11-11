//! The centre for a job (spec §9.2). With one selected: a header that every state shares
//! (the job's folder, its status, and the error or warning it keeps), then what its state
//! shows: what a queued job waits for; how far a scan is, and the photos it could not
//! read; the review of a job in review (`views::review`); that a job failed or was
//! cancelled, and a way to run it again; and, once a stack of it was confirmed for a
//! write, its write and what the write made of its stacks (`views::results`). With none
//! selected: an invitation to start a first job, or to pick one.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Color32, CornerRadius, FontId, Label, Margin, RichText, Role, Stroke, TextFormat,
    WidgetInfo, text::LayoutJob,
};
use egui_material_icons::MaterialIcon;
use egui_material_icons::icons::{ICON_ERROR, ICON_WARNING};
use photostacker_core::engine::{JobId, JobView};
use photostacker_core::model::JobStatus;
use photostacker_core::{Progress, Stage};

use crate::app::FrameStats;
use crate::controller::{self, Controller};
use crate::format::{display_path, folder_name, job_status_text, percent, sentence, stage_text};
use crate::theme::{self, SEMIBOLD_FAMILY};
use crate::thumbs::ThumbCache;
use crate::views::{
    self,
    jobs::{fraction, status_color},
};
use crate::widgets::{self, Emphasis};

/// What the centre says while there is no job.
pub(crate) const INVITATION: &str = "Drop a folder of photos here, or start a new job.";
/// What the centre says while there are jobs and none is selected.
const SELECT_HINT: &str = "Select a job on the left, or start a new one.";
/// What a queued job waits for when no job is scanning or writing: the worker is busy
/// with a task of a job in review, or about to start.
const WAITING_TO_START: &str = "Waiting to start.";
const CANCELLED: &str = "This job was cancelled.";
/// The name of the scan's progress bar, and of the spinner that takes its place while a
/// stage reports only once it is done.
const SCAN_PROGRESS: &str = "Scan progress";
/// The widest a state's content runs: past it, a progress bar is a long line with little
/// in it.
const BODY_WIDTH: f32 = 560.0;
const PROGRESS_HEIGHT: f32 = 8.0;
/// The size of a banner's icon: a little above the text's, so the first line and the
/// icon share their row.
const BANNER_ICON_SIZE: f32 = 16.0;

/// What the user asked for; done once the centre is drawn, which borrows the job.
enum Action {
    NewJob,
    Cancel(JobId),
    Retry(JobId),
}

/// What the centre shows of the selected job, under its header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Centre {
    /// Its review, across the centre.
    Review,
    /// Its write and what the write made of its stacks, across the centre; for a failed
    /// or cancelled job, under what its state says.
    Results,
    /// What its state says, in a column.
    State,
}

/// What the centre shows of `job`: its review while it is in review; its write and
/// results when the controller says it has some to show ([`controller::shows_results`]);
/// else what its state says.
fn centre_of(job: &JobView) -> Centre {
    if job.status == JobStatus::ReviewPending {
        Centre::Review
    } else if controller::shows_results(job) {
        Centre::Results
    } else {
        Centre::State
    }
}

/// Draws the centre for the selected job, or, with none selected, what to do. A job in
/// review shows its review under the header, which scrolls its primary stack into view
/// when `reveal_primary` says a shortcut stepped the selection; a job with a write to
/// show, its write and its results. Returns what the review or the results laid out
/// (nothing outside them).
pub fn show(
    ui: &mut egui::Ui,
    controller: &mut Controller,
    thumbs: &mut ThumbCache,
    reveal_primary: bool,
) -> FrameStats {
    let mut stats = FrameStats::default();
    let action = match controller.selected().map(centre_of) {
        Some(Centre::Review) => {
            if let Some(job) = controller.selected() {
                header(ui, job);
            }
            ui.add_space(12.0);
            return views::review::show(ui, controller, thumbs, reveal_primary);
        }
        Some(Centre::Results) => {
            let mut action = None;
            if let Some(job) = controller.selected() {
                header(ui, job);
                ui.add_space(12.0);
                if matches!(job.status, JobStatus::Failed | JobStatus::Cancelled) {
                    action = body(ui, |ui| ended(ui, job));
                    ui.add_space(12.0);
                }
            }
            stats = views::results::show(ui, controller, thumbs);
            action
        }
        Some(Centre::State) => controller
            .selected()
            .and_then(|job| selected(ui, controller, job)),
        None => nothing_selected(ui, controller),
    };
    match action {
        Some(Action::NewJob) => controller.open_new_job(None),
        Some(Action::Cancel(job)) => controller.cancel(job),
        Some(Action::Retry(job)) => controller.retry(job),
        None => {}
    }
    stats
}

/// A column at most [`BODY_WIDTH`] wide, from the left of the centre, holding what `add`
/// draws.
pub(crate) fn body<R>(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let width = ui.available_width().min(BODY_WIDTH);
    ui.vertical(|ui| {
        ui.set_max_width(width);
        add(ui)
    })
    .inner
}

/// Nothing until the jobs are read, so an app with jobs never shows, at start, the
/// invitation of an app without one; then the invitation while there is no job, and a
/// hint to pick one while there are.
fn nothing_selected(ui: &mut egui::Ui, controller: &Controller) -> Option<Action> {
    if !controller.jobs_loaded() {
        ui.take_available_space();
        None
    } else if controller.jobs().is_empty() {
        invitation(ui).then_some(Action::NewJob)
    } else {
        let palette = theme::palette(ui.ctx());
        let height = ui.available_height();
        ui.vertical_centered(|ui| {
            // Roughly half the height of the text, so it sits in the middle.
            ui.add_space((height / 2.0 - 12.0).max(0.0));
            ui.label(RichText::new(SELECT_HINT).color(palette.secondary));
        });
        None
    }
}

/// The centre of an app without a job: how to start one. The toolbar has a `New job` too,
/// so this one is named apart for screen readers and tests. Returns whether it was
/// clicked.
fn invitation(ui: &mut egui::Ui) -> bool {
    let palette = theme::palette(ui.ctx());
    let height = ui.available_height();
    ui.vertical_centered(|ui| {
        // Roughly the height of what follows, so it sits in the middle.
        ui.add_space((height / 2.0 - 40.0).max(0.0));
        ui.label(RichText::new(INVITATION).color(palette.secondary));
        ui.add_space(8.0);
        named_button(ui, "New job", "Start a new job")
    })
    .inner
}

/// The selected job: its header across the centre, then its state's content in a column.
fn selected(ui: &mut egui::Ui, controller: &Controller, job: &JobView) -> Option<Action> {
    header(ui, job);
    ui.add_space(12.0);
    body(ui, |ui| match job.status {
        JobStatus::Queued => queued(ui, controller, job),
        JobStatus::Scanning => scanning(ui, controller, job),
        JobStatus::Failed | JobStatus::Cancelled => ended(ui, job),
        // Drawn by `show` with their review or their results.
        JobStatus::ReviewPending | JobStatus::Writing | JobStatus::Completed => None,
    })
}

/// The job's folder, cut in the middle to the width left by the status chip at the far
/// end, then the error and the warning the job keeps, each in a banner.
fn header(ui: &mut egui::Ui, job: &JobView) {
    let palette = theme::palette(ui.ctx());
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            widgets::chip(
                ui,
                job_status_text(job.status),
                status_color(palette, job.status),
            );
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                folder_line(ui, &job.config.input_dir);
            });
        });
    });
    // The engine's words, as sentences.
    if let Some(error) = &job.error {
        ui.add_space(8.0);
        banner(ui, ICON_ERROR, "Error", palette.danger, &sentence(error));
    }
    if let Some(warning) = &job.warning {
        ui.add_space(8.0);
        banner(
            ui,
            ICON_WARNING,
            "Warning",
            palette.warning,
            &sentence(warning),
        );
    }
}

/// `folder`'s path in one line, cut in the middle when it is too long, so both its root
/// and its own name stay in sight; the whole path shows on hover, and is the line's name
/// for screen readers and tests.
fn folder_line(ui: &mut egui::Ui, folder: &Path) {
    let palette = theme::palette(ui.ctx());
    let path = display_path(folder);
    let size = egui::TextStyle::Body.resolve(ui.style()).size;
    let font = FontId::new(size, egui::FontFamily::Name(SEMIBOLD_FAMILY.into()));
    let shown = widgets::fitted(ui, &path, &font, ui.available_width());
    let enabled = ui.is_enabled();
    // Cut short by egui too, should the measure and the layout ever differ by a pixel.
    let label = Label::new(RichText::new(shown).font(font).color(palette.text))
        .truncate()
        .selectable(false)
        .show_tooltip_when_elided(false);
    let response = ui.add(label).on_hover_text(path.as_str());
    response.widget_info(|| WidgetInfo::labeled(Role::Label, enabled, &path));
}

/// A message the job keeps: an icon in `color`, named `kind`, then `text` in the text
/// colour, which wraps, on a frame tinted with `color`. The status colours are marks,
/// never small text (see `theme`'s contrast tests).
pub(crate) fn banner(
    ui: &mut egui::Ui,
    icon: MaterialIcon,
    kind: &str,
    color: Color32,
    text: &str,
) {
    let palette = theme::palette(ui.ctx());
    egui::Frame::new()
        .fill(theme::mix(palette.window, color, 0.12))
        .stroke(Stroke::new(1.0, theme::mix(palette.window, color, 0.45)))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                let enabled = ui.is_enabled();
                let mark = ui.label(icon.rich_text().size(BANNER_ICON_SIZE).color(color));
                mark.widget_info(|| WidgetInfo::labeled(Role::Image, enabled, kind));
                ui.add(Label::new(RichText::new(text).color(palette.text)).wrap());
            });
        });
}

/// What a queued job waits for: the job that is scanning or writing, by its name in the
/// list, or nothing in particular; and Cancel.
fn queued(ui: &mut egui::Ui, controller: &Controller, job: &JobView) -> Option<Action> {
    let running = controller.jobs().iter().find(|other| {
        other.id != job.id && matches!(other.status, JobStatus::Scanning | JobStatus::Writing)
    });
    let text = running.map_or_else(
        || WAITING_TO_START.to_owned(),
        |running| format!("Waiting for {}.", controller.job_name(running.id)),
    );
    ui.add(Label::new(text).wrap());
    ui.add_space(8.0);
    named_button(ui, "Cancel", "Cancel job").then_some(Action::Cancel(job.id))
}

/// How far the scan is, Cancel, and the photos it could not read.
fn scanning(ui: &mut egui::Ui, controller: &Controller, job: &JobView) -> Option<Action> {
    scan_progress(ui, controller.progress(job.id));
    ui.add_space(8.0);
    let cancel = named_button(ui, "Cancel", "Cancel job");
    scan_errors(
        ui,
        &job.config.input_dir,
        controller.scan_errors(job.id),
        controller.scan_errors_dropped(job.id),
    );
    cancel.then_some(Action::Cancel(job.id))
}

/// The stage, then, for a stage that reports each photo, a bar with its count and the
/// photo just done; a stage that reports only once it is done turns a spinner instead,
/// which goes, with the frames it asks for, when the scan does.
fn scan_progress(ui: &mut egui::Ui, progress: Option<&Progress>) {
    let palette = theme::palette(ui.ctx());
    let counted = progress.filter(|progress| counts_photos(progress.stage));
    // Until its first report, the scan lists the folder's photos.
    let stage = stage_text(progress.map_or(Stage::Discover, |progress| progress.stage));
    ui.horizontal(|ui| {
        if counted.is_none() {
            let spinner = ui.add(egui::Spinner::new().color(palette.accent));
            let enabled = ui.is_enabled();
            spinner.widget_info(|| {
                WidgetInfo::labeled(Role::ProgressIndicator, enabled, SCAN_PROGRESS)
            });
        }
        ui.label(
            RichText::new(stage)
                .family(egui::FontFamily::Name(SEMIBOLD_FAMILY.into()))
                .color(palette.text),
        );
        if let Some(progress) = counted {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let count = format!("{} of {}", progress.done, progress.total);
                ui.label(RichText::new(count).color(palette.secondary));
            });
        }
    });
    let Some(progress) = counted else {
        return;
    };
    progress_bar(ui, progress, SCAN_PROGRESS);
}

/// `progress` as a bar 8 px high, named `name` for screen readers with its percentage as
/// its value, then the name of the photo just done, small, under it.
pub(crate) fn progress_bar(ui: &mut egui::Ui, progress: &Progress, name: &str) {
    let palette = theme::palette(ui.ctx());
    let bar = ui
        .scope(|ui| {
            // The bar's track, which egui draws in this colour, would not show on the
            // window's.
            ui.visuals_mut().extreme_bg_color = palette.surface_active;
            ui.add(
                egui::ProgressBar::new(fraction(progress.done, progress.total))
                    .desired_height(PROGRESS_HEIGHT)
                    .fill(palette.accent)
                    .corner_radius(CornerRadius::same(4)),
            )
        })
        .inner;
    let enabled = ui.is_enabled();
    let value = percent(progress.done, progress.total) as f64;
    bar.widget_info(|| {
        let mut info = WidgetInfo::labeled(Role::ProgressIndicator, enabled, name);
        info.value = Some(value);
        info
    });
    if let Some(current) = &progress.current {
        let name = RichText::new(folder_name(current))
            .small()
            .color(palette.secondary);
        ui.add(Label::new(name).truncate());
    }
}

/// Whether `stage` reports each photo it finishes, so that it has a count to show; the
/// others report once, when they are done.
fn counts_photos(stage: Stage) -> bool {
    match stage {
        Stage::Metadata | Stage::Hash | Stage::Write | Stage::Embed => true,
        Stage::Discover | Stage::Group => false,
    }
}

/// The photos the scan could not read, in a card of their own that scrolls, oldest
/// first, and follows the newest at the bottom until the user scrolls up; past the 500
/// the controller keeps, only how many more. Nothing while there are none.
fn scan_errors(ui: &mut egui::Ui, input: &Path, errors: &[(PathBuf, String)], dropped: usize) {
    let count = errors.len() + dropped;
    if count == 0 {
        return;
    }
    let palette = theme::palette(ui.ctx());
    ui.add_space(16.0);
    let title = format!("Files that could not be read ({count})");
    widgets::card(ui, &title, None, |ui| {
        egui::ScrollArea::vertical()
            .id_salt("scan errors")
            .auto_shrink([false, true])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                // Each photo takes two lines or more: a gap tells them apart.
                ui.spacing_mut().item_spacing.y = 8.0;
                for (path, reason) in errors {
                    scan_error(ui, input, path, reason);
                }
                if dropped > 0 {
                    ui.label(RichText::new(format!("and {dropped} more")).color(palette.secondary));
                }
            });
    });
}

/// One photo the scan could not read: its name, and why under it, wrapping; its path
/// inside the job's folder on hover.
fn scan_error(ui: &mut egui::Ui, input: &Path, path: &Path, reason: &str) {
    let palette = theme::palette(ui.ctx());
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut line = LayoutJob::default();
    line.append(
        &format!("{}\n", folder_name(path)),
        0.0,
        TextFormat::simple(font.clone(), palette.text),
    );
    line.append(
        &sentence(reason),
        0.0,
        TextFormat::simple(font, palette.secondary),
    );
    let inside = path
        .strip_prefix(input)
        .map_or_else(|_| display_path(path), display_path);
    ui.add(Label::new(line).wrap()).on_hover_text(inside);
}

/// That the job failed (its error is in the banner above) or was cancelled, and Retry.
/// When it has a write to show, its results follow (`views::results`).
fn ended(ui: &mut egui::Ui, job: &JobView) -> Option<Action> {
    if job.status == JobStatus::Cancelled {
        ui.label(CANCELLED);
        ui.add_space(8.0);
    }
    let retry = named_button(ui, "Retry", "Retry job");
    retry.then_some(Action::Retry(job.id))
}

/// A button that shows `text` and is named `name` for screen readers and tests: the
/// selected row of the jobs list already has a `Cancel`, a `Retry` and an `Open output
/// folder`, and the toolbar a `New job`. Returns whether it was clicked.
pub(crate) fn named_button(ui: &mut egui::Ui, text: &str, name: &str) -> bool {
    let enabled = ui.is_enabled();
    let button = widgets::text_button(ui, text, Emphasis::Normal);
    button.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, name));
    button.clicked()
}
