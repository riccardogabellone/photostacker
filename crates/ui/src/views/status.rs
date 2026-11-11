//! The bar along the bottom: the selected job's totals on the left, and on the right what
//! the engine is doing (nothing once it has stopped: the centre says so), then the state
//! of the visual check in the ML chip's words (spec §9.2).

use eframe::egui::{self, Label, RichText};
use photostacker_core::Progress;
use photostacker_core::Stage;
use photostacker_core::engine::JobView;
use photostacker_core::model::JobStatus;

use crate::controller::Controller;
use crate::format::{count, job_totals, ml_state_text, percent};
use crate::theme;

/// Draws the status bar, one text row high. The right side gives way to the left: a long
/// folder name is cut short rather than widening the window.
pub fn show(ui: &mut egui::Ui, controller: &Controller) {
    let palette = theme::palette(ui.ctx());
    // One text row: a plain horizontal layout would be a widget's height.
    let row = egui::vec2(
        ui.available_width(),
        ui.text_style_height(&egui::TextStyle::Body),
    );
    let layout = egui::Layout::left_to_right(egui::Align::Center);
    ui.allocate_ui_with_layout(row, layout, |ui| {
        if let Some(job) = controller.selected()
            && (job.scan_done || job.counts.files > 0)
        {
            let unstacked = controller
                .review()
                .filter(|review| review.job == job.id && review.loaded())
                .map(|review| review.unstacked.len());
            ui.label(totals(job, unstacked));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // The toolbar chip's words, as text: the bar's one fixed item, at its end.
            let ml = ml_state_text(&controller.ml_state());
            ui.label(RichText::new(ml).color(palette.secondary));
            // Once the engine has stopped, nothing runs: the centre says so, and the last
            // progress it sent is no activity.
            let Some(text) = activity(controller).filter(|_| controller.engine_stopped().is_none())
            else {
                return;
            };
            ui.label(RichText::new("·").color(palette.secondary));
            // Cut short, the full text shows on hover.
            ui.add(Label::new(RichText::new(text).color(palette.secondary)).truncate());
        });
    });
}

/// `job`'s totals, with the photos in its tray once its review has read them:
/// `412 photos · 38 stacks · 12 unstacked`.
fn totals(job: &JobView, unstacked: Option<usize>) -> String {
    let mut text = job_totals(&job.counts);
    if let Some(unstacked) = unstacked {
        text.push_str(&format!(" · {unstacked} unstacked"));
    }
    text
}

/// What the engine is doing: the newest job that is scanning, writing, or in review with
/// its visual check running.
fn activity(controller: &Controller) -> Option<String> {
    controller.jobs().iter().rev().find_map(|job| {
        job_activity(
            job,
            controller.job_name(job.id),
            controller.progress(job.id),
        )
    })
}

/// What `job`'s running task is doing, from its `progress`, if it runs one, with the job
/// named `folder` as the list names it: `Scanning trip: 41 %`, `Writing trip: 3 of 9
/// photos`, `Visual check trip (2): 12 of 40`.
fn job_activity(job: &JobView, folder: &str, progress: Option<&Progress>) -> Option<String> {
    match (job.status, progress) {
        // Only these stages tick per photo; the others report once, when they are done.
        (JobStatus::Scanning, Some(progress))
            if matches!(progress.stage, Stage::Metadata | Stage::Hash) =>
        {
            let percent = percent(progress.done, progress.total);
            Some(format!("Scanning {folder}: {percent} %"))
        }
        (JobStatus::Scanning, _) => Some(format!("Scanning {folder}")),
        (JobStatus::Writing, Some(progress)) if progress.stage == Stage::Write => {
            let total = count(progress.total, "photo", "photos");
            Some(format!("Writing {folder}: {} of {total}", progress.done))
        }
        (JobStatus::Writing, _) => Some(format!("Writing {folder}")),
        (JobStatus::ReviewPending, Some(progress)) if progress.stage == Stage::Embed => {
            Some(format!(
                "Visual check {folder}: {} of {}",
                progress.done, progress.total
            ))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use photostacker_core::JobConfig;
    use photostacker_core::engine::JobCounts;

    use super::*;

    fn job(status: JobStatus) -> JobView {
        JobView {
            id: 1,
            created_at: "2026-11-11T10:00:00.000Z".into(),
            status,
            scan_done: true,
            config: JobConfig::new(PathBuf::from("/photos/trip")),
            error: None,
            warning: None,
            counts: JobCounts::default(),
        }
    }

    fn progress(stage: Stage, done: usize, total: usize) -> Progress {
        Progress {
            stage,
            done,
            total,
            current: None,
        }
    }

    #[test]
    fn the_totals_count_the_tray_once_the_review_has_read_it() {
        let mut job = job(JobStatus::ReviewPending);
        job.counts = JobCounts {
            files: 5,
            proposed: 2,
            ..JobCounts::default()
        };
        assert_eq!(totals(&job, None), "5 photos · 2 stacks");
        assert_eq!(totals(&job, Some(1)), "5 photos · 2 stacks · 1 unstacked");
    }

    #[test]
    fn the_activity_names_the_job_and_how_far_its_task_is() {
        let activity = |status, progress: Option<Progress>| {
            job_activity(&job(status), "trip", progress.as_ref())
        };
        assert_eq!(
            activity(JobStatus::Scanning, Some(progress(Stage::Hash, 41, 100))).as_deref(),
            Some("Scanning trip: 41 %")
        );
        // A stage that reports only when it is done gives no percentage.
        assert_eq!(
            activity(JobStatus::Scanning, Some(progress(Stage::Discover, 1, 1))).as_deref(),
            Some("Scanning trip")
        );
        assert_eq!(
            activity(JobStatus::Scanning, None).as_deref(),
            Some("Scanning trip")
        );
        assert_eq!(
            activity(JobStatus::Writing, Some(progress(Stage::Write, 3, 9))).as_deref(),
            Some("Writing trip: 3 of 9 photos")
        );
        assert_eq!(
            activity(
                JobStatus::ReviewPending,
                Some(progress(Stage::Embed, 12, 40))
            )
            .as_deref(),
            Some("Visual check trip: 12 of 40")
        );
        assert_eq!(
            job_activity(
                &job(JobStatus::Scanning),
                "trip (2)",
                Some(&progress(Stage::Hash, 1, 4))
            )
            .as_deref(),
            Some("Scanning trip (2): 25 %")
        );
        assert_eq!(activity(JobStatus::ReviewPending, None), None);
        assert_eq!(activity(JobStatus::Queued, None), None);
        assert_eq!(
            activity(JobStatus::Completed, Some(progress(Stage::Embed, 1, 2))),
            None
        );
    }
}
