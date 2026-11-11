//! The jobs list on the left (spec §9.2): every job, newest first, each a row with its
//! folder's name, its status, a progress bar while it runs and its totals once scanned.
//! A click on a row selects its job; the selected row shows the job's actions.

use std::path::PathBuf;

use eframe::egui::{
    self, Color32, CornerRadius, Label, Margin, RichText, Role, Sense, Stroke, UiBuilder,
    WidgetInfo,
};
use egui_material_icons::icons::{ICON_CANCEL, ICON_DELETE, ICON_FOLDER_OPEN, ICON_REFRESH};
use photostacker_core::engine::{JobId, JobView};
use photostacker_core::model::JobStatus;
use photostacker_core::{Progress, Stage};

use crate::controller::{Controller, Question, Reveal};
use crate::format::{count, display_path, job_status_text, job_totals, stage_text};
use crate::theme::{self, Palette, SEMIBOLD_FAMILY};
use crate::widgets::{self, Emphasis};

/// The height of a row's progress bar: a line, so the row stays compact.
const PROGRESS_HEIGHT: f32 = 4.0;
/// What a row's small line says while its job is being deleted.
pub const DELETING: &str = "Deleting…";

/// What the user asked for in a row; done once the list is drawn, which borrows the jobs.
enum Action {
    Select(JobId),
    Reveal(PathBuf),
    Retry(JobId),
    Cancel(JobId),
    Delete(JobId),
}

/// Draws the list, which takes the whole panel and scrolls.
pub fn show(ui: &mut egui::Ui, controller: &mut Controller) {
    let palette = theme::palette(ui.ctx());
    ui.label(RichText::new("Jobs").color(palette.secondary));
    let mut actions = Vec::new();
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            let selected = controller.selected().map(|job| job.id);
            for job in controller.jobs().iter().rev() {
                let shown = Shown {
                    name: controller.job_name(job.id),
                    progress: controller.progress(job.id),
                    selected: selected == Some(job.id),
                    deleting: controller.is_deleting(job.id),
                };
                row(ui, job, &shown, &mut actions);
            }
        });
    for action in actions {
        match action {
            Action::Select(job) => controller.select_job(job),
            Action::Reveal(folder) => controller.reveal(&folder, Reveal::OutputFolder),
            Action::Retry(job) => controller.retry(job),
            Action::Cancel(job) => controller.cancel(job),
            Action::Delete(job) => controller.ask(Question::DeleteJob(job)),
        }
    }
}

/// What a job's row shows besides the job.
struct Shown<'a> {
    /// The job's name in the list ([`Controller::job_name`]).
    name: &'a str,
    progress: Option<&'a Progress>,
    selected: bool,
    /// Its delete was sent and has not ended.
    deleting: bool,
}

/// One job's row: a single selectable widget, named `Job <name>, <status>` after the job's
/// name in the list. Its own buttons are its children, so a click on one goes to the
/// button, not to the row. While the job is being deleted, its small line says so and its
/// buttons are off.
fn row(ui: &mut egui::Ui, job: &JobView, shown: &Shown<'_>, actions: &mut Vec<Action>) {
    let Shown {
        name,
        progress,
        selected,
        deleting,
    } = *shown;
    let palette = theme::palette(ui.ctx());
    let builder = UiBuilder::new()
        .id_salt(("job", job.id))
        .sense(Sense::click());
    let response = ui
        .scope_builder(builder, |ui| {
            // Hovering one of its buttons still highlights the row.
            let pointed = ui.response().contains_pointer();
            row_frame(palette, selected, pointed).show(ui, |ui| {
                ui.set_width(ui.available_width());
                // The texts only show: a click on them is the row's.
                ui.style_mut().interaction.selectable_labels = false;
                ui.spacing_mut().item_spacing.y = 4.0;
                title(ui, job, name, palette);
                if deleting {
                    secondary(ui, DELETING.to_owned(), palette);
                } else if matches!(
                    job.status,
                    JobStatus::Queued | JobStatus::Scanning | JobStatus::Writing
                ) {
                    progress_bar(ui, job.status, progress, palette);
                } else if job.scan_done || job.counts.files > 0 {
                    secondary(ui, job_totals(&job.counts), palette);
                }
                if selected {
                    ui.add_enabled_ui(!deleting, |ui| buttons(ui, job, actions));
                }
            });
        })
        .response;
    let label = format!("Job {name}, {}", job_status_text(job.status));
    response.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, &label));
    if response.clicked() {
        actions.push(Action::Select(job.id));
    }
}

/// The row's background: the accent's outline when selected, a tint under the pointer
/// otherwise. A selected row keeps the panel's colour, so its buttons, which are
/// `surface`, stand out on it. The stroke is always there, in the fill's colour when it
/// does not show, so selecting a row never moves its content.
fn row_frame(palette: &Palette, selected: bool, pointed: bool) -> egui::Frame {
    let fill = if pointed && !selected {
        palette.surface
    } else {
        palette.panel
    };
    let stroke = if selected { palette.accent } else { fill };
    egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, stroke))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(Margin::symmetric(8, 6))
}

/// The job's name, cut short when long (its folder's full path on hover), and the status
/// chip at the far end.
fn title(ui: &mut egui::Ui, job: &JobView, name: &str, palette: &Palette) {
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            widgets::chip(
                ui,
                job_status_text(job.status),
                status_color(palette, job.status),
            );
            ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                let name = RichText::new(name)
                    .family(egui::FontFamily::Name(SEMIBOLD_FAMILY.into()))
                    .color(palette.text);
                ui.add(Label::new(name).truncate().show_tooltip_when_elided(false))
                    .on_hover_text(display_path(&job.config.input_dir));
            });
        });
    });
}

/// A job that is queued, scanning or writing: how far the current stage of its task is.
fn progress_bar(
    ui: &mut egui::Ui,
    status: JobStatus,
    progress: Option<&Progress>,
    palette: &Palette,
) {
    let (fraction, text) = match progress {
        // A write counts the photos it has put in its stacks.
        Some(progress) if progress.stage == Stage::Write => (
            fraction(progress.done, progress.total),
            format!(
                "Writing: {} of {}",
                progress.done,
                count(progress.total, "photo", "photos")
            ),
        ),
        Some(progress) => (
            fraction(progress.done, progress.total),
            format!(
                "{}: {} of {}",
                stage_text(progress.stage),
                progress.done,
                progress.total
            ),
        ),
        None if status == JobStatus::Queued => (0.0, "Waiting".to_owned()),
        // A scan lists its folder first, and says so once that is done.
        None if status == JobStatus::Scanning => (0.0, stage_text(Stage::Discover).to_owned()),
        None => (0.0, "Starting".to_owned()),
    };
    ui.add(
        egui::ProgressBar::new(fraction)
            .desired_height(PROGRESS_HEIGHT)
            .fill(palette.accent)
            .corner_radius(CornerRadius::same(2)),
    );
    secondary(ui, text, palette);
}

/// `done` of `total` as a fraction from 0 to 1, for a progress bar.
pub(crate) fn fraction(done: usize, total: usize) -> f32 {
    if total == 0 {
        0.0
    } else {
        done.min(total) as f32 / total as f32
    }
}

/// A small line in the secondary colour, cut short when long.
fn secondary(ui: &mut egui::Ui, text: String, palette: &Palette) {
    let text = RichText::new(text).small().color(palette.secondary);
    ui.add(Label::new(text).truncate());
}

/// The selected job's actions: open its output folder, retry, cancel, and, apart at the
/// far end, delete.
fn buttons(ui: &mut egui::Ui, job: &JobView, actions: &mut Vec<Action>) {
    ui.horizontal(|ui| {
        if widgets::icon_button(ui, ICON_FOLDER_OPEN, "Open output folder", Emphasis::Normal)
            .clicked()
        {
            actions.push(Action::Reveal(job.config.output_dir.clone()));
        }
        if can_retry(job)
            && widgets::icon_button(ui, ICON_REFRESH, "Retry", Emphasis::Normal).clicked()
        {
            actions.push(Action::Retry(job.id));
        }
        if can_cancel(job.status)
            && widgets::icon_button(ui, ICON_CANCEL, "Cancel", Emphasis::Normal).clicked()
        {
            actions.push(Action::Cancel(job.id));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::icon_button(ui, ICON_DELETE, "Delete", Emphasis::Normal).clicked() {
                actions.push(Action::Delete(job.id));
            }
        });
    });
}

/// A failed or cancelled job runs again; a done job only when some of its stacks failed or
/// were skipped (the engine refuses a retry that would write nothing).
fn can_retry(job: &JobView) -> bool {
    match job.status {
        JobStatus::Failed | JobStatus::Cancelled => true,
        JobStatus::Completed => job.counts.failed + job.counts.skipped > 0,
        JobStatus::Queued | JobStatus::Scanning | JobStatus::ReviewPending | JobStatus::Writing => {
            false
        }
    }
}

fn can_cancel(status: JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Queued | JobStatus::Scanning | JobStatus::Writing
    )
}

/// The colour of a status's chip dot: the accent while the job runs or waits for its
/// review, the status colours once it ended, and the secondary text colour for a job
/// that waits or was stopped. The centre's header uses it too.
pub(crate) fn status_color(palette: &Palette, status: JobStatus) -> Color32 {
    match status {
        JobStatus::Queued | JobStatus::Cancelled => palette.secondary,
        JobStatus::Scanning | JobStatus::ReviewPending | JobStatus::Writing => palette.accent,
        JobStatus::Completed => palette.positive,
        JobStatus::Failed => palette.danger,
    }
}

#[cfg(test)]
mod tests {
    use photostacker_core::JobConfig;
    use photostacker_core::engine::JobCounts;

    use super::*;

    fn job(status: JobStatus, counts: JobCounts) -> JobView {
        JobView {
            id: 1,
            created_at: "2026-11-11T10:00:00.000Z".into(),
            status,
            scan_done: true,
            config: JobConfig::new(PathBuf::from("/photos/trip")),
            error: None,
            warning: None,
            counts,
        }
    }

    #[test]
    fn a_done_job_retries_only_its_failed_or_skipped_stacks() {
        let written = JobCounts {
            written: 3,
            ..JobCounts::default()
        };
        assert!(!can_retry(&job(JobStatus::Completed, written)));
        for counts in [
            JobCounts {
                failed: 1,
                ..written
            },
            JobCounts {
                skipped: 1,
                ..written
            },
        ] {
            assert!(can_retry(&job(JobStatus::Completed, counts)));
        }
        for status in [JobStatus::Failed, JobStatus::Cancelled] {
            assert!(can_retry(&job(status, JobCounts::default())));
        }
        for status in [
            JobStatus::Queued,
            JobStatus::Scanning,
            JobStatus::ReviewPending,
            JobStatus::Writing,
        ] {
            assert!(!can_retry(&job(status, JobCounts::default())));
        }
    }
}
