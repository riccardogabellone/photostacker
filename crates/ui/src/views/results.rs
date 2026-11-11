//! The writing and the results of a job (spec §9.2, §5.6, §5.7): the centre, under the
//! job's header, of a job that is being written or is done, and of one queued (for its
//! write), failed or cancelled once a stack of it was confirmed for a write
//! (`controller::shows_results`).
//!
//! First what the write is doing: while it runs, a bar of the photos written, the photo
//! just done and Cancel; once it has ended, how many stacks it wrote, skipped and failed,
//! the output folder, and the way back in for the stacks a dropped drive or a full disk
//! left unwritten (`Retry <k> stacks`), or, for a cancelled job, the write of the
//! confirmed stacks it did not reach.
//!
//! Then the stacks that were confirmed for a write, in position order, one row each.
//! Every row is [`ROW_HEIGHT`] high, so `ScrollArea::show_rows` lays out only the rows on
//! screen. A row shows the top layer's thumbnail, the stack's number, its first photo,
//! its photo count and its status (`Waiting` until the write reaches it), then the file
//! written and a button that reveals it, or why the stack was skipped or failed, and the
//! write's warnings. The stacks the write left alone, proposed or rejected, are only
//! counted, in a line right under the list. A click on a row selects its stack, on its
//! thumbnail its photo, and the inspector shows them.
//!
//! What the view counts comes from the controller, worked out once per read of the
//! stacks: a frame never walks every stack.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align, Color32, Id, Label, Layout, Rect, RichText, Role, ScrollArea, Sense, TextStyle,
    TextWrapMode, UiBuilder, Vec2, WidgetInfo, text::LayoutJob,
};
use egui_material_icons::icons::{ICON_ERROR, ICON_WARNING};
use photostacker_core::engine::{GroupId, GroupView, JobId, JobView};
use photostacker_core::model::{GroupStatus, JobStatus};
use photostacker_core::{Progress, Stage};

use crate::app::FrameStats;
use crate::controller::{Controller, ResultCounts, Reveal, Review};
use crate::format::{count, display_path, folder_name, group_status_text, sentence, stage_text};
use crate::theme::{self, Palette, SEMIBOLD_FAMILY};
use crate::thumbs::ThumbCache;
use crate::views::job::{body, named_button, progress_bar};
use crate::views::photo::{self, Photo, Shown};
use crate::views::review::{self, Look};
use crate::widgets::{self, Emphasis};

/// The height of every stack's row: its thumbnail inside its padding.
pub const ROW_HEIGHT: f32 = 88.0;
/// Inside a row, on every side.
const ROW_PADDING: f32 = 8.0;
/// The top layer's place: 72 px high, a 3:2 photo's width; other shapes are letterboxed.
const THUMB_SIZE: Vec2 = Vec2::new(108.0, 72.0);
/// Between the thumbnail and the texts.
const THUMB_GAP: f32 = 12.0;
/// Each of a row's lines of facts: a button's height.
const LINE_HEIGHT: f32 = 28.0;
/// Between the facts of a line.
const FACT_GAP: f32 = 6.0;
/// Between the list's last row and the line that counts the stacks left alone.
const NOTE_GAP: f32 = 8.0;
/// The narrowest a row's first photo's name is shown at all.
const NAME_AT_ALL: f32 = 24.0;
const LOADING: &str = "Loading the stacks…";
const WAITING_TO_WRITE: &str = "Waiting to write.";
const NOTHING_WRITTEN: &str = "No stack was written.";
/// A confirmed stack's status while its job's write has yet to reach it.
const WAITING: &str = "Waiting";
/// The name of the write's progress bar, and of the spinner before its first photo.
const WRITE_PROGRESS: &str = "Write progress";
/// The name of the header's `Open output folder`, apart from the selected job's row's.
const OPEN_OUTPUT: &str = "Open the output folder";
/// The name of the header's `Cancel`, apart from the selected job's row's.
const CANCEL_JOB: &str = "Cancel job";
/// What `Retry <k> stacks` does, on hover.
const RETRY_HOVER: &str =
    "Write again the stacks that failed, or were skipped for a photo that could not be read.";

/// What the user asked for; done once the view is drawn, which borrows the controller.
enum Action {
    Select(GroupId),
    SelectPhoto(usize),
    Reveal(PathBuf),
    OpenOutput(PathBuf),
    Cancel(JobId),
    Retry(JobId),
    Write,
    ReadAgain,
}

/// Draws the writing or the results of the selected job: until its review is loaded, a
/// quiet line. Returns how many rows of stacks it laid out.
pub fn show(ui: &mut egui::Ui, controller: &mut Controller, thumbs: &mut ThumbCache) -> FrameStats {
    let mut actions = Vec::new();
    let mut stack_rows = 0;
    if let Some(job) = controller.selected() {
        match review::shown(controller) {
            Some((review, input)) => {
                let progress = controller.progress(job.id);
                let to_write = controller.write_counts().to_write;
                body(ui, |ui| {
                    header(ui, job, progress, review.results(), to_write, &mut actions);
                });
                ui.add_space(12.0);
                stack_rows = stacks(ui, job, review, input, thumbs, &mut actions);
            }
            None => {
                if review::not_loaded(ui, controller, LOADING) {
                    actions.push(Action::ReadAgain);
                }
            }
        }
    }
    for action in actions {
        match action {
            Action::Select(group) => controller.select_group(group, false),
            Action::SelectPhoto(file) => controller.select_photo(file),
            Action::Reveal(path) => controller.reveal(&path, Reveal::File),
            Action::OpenOutput(folder) => controller.reveal(&folder, Reveal::OutputFolder),
            Action::Cancel(job) => controller.cancel(job),
            Action::Retry(job) => controller.retry(job),
            Action::Write => controller.write(),
            Action::ReadAgain => controller.read_review_again(),
        }
    }
    FrameStats {
        stack_rows,
        tray_rows: 0,
    }
}

/// Whether `job`'s write is on its way or running: its confirmed stacks wait for it.
fn writing(job: &JobView) -> bool {
    matches!(job.status, JobStatus::Queued | JobStatus::Writing)
}

/// What the write is doing: while it runs, how far it is and Cancel; queued, that it
/// waits; once it has ended, what it made of the stacks and what can be done next.
fn header(
    ui: &mut egui::Ui,
    job: &JobView,
    progress: Option<&Progress>,
    results: ResultCounts,
    to_write: usize,
    actions: &mut Vec<Action>,
) {
    match job.status {
        JobStatus::Writing => {
            write_progress(ui, progress);
            ui.add_space(8.0);
            if named_button(ui, "Cancel", CANCEL_JOB) {
                actions.push(Action::Cancel(job.id));
            }
        }
        JobStatus::Queued => {
            ui.label(WAITING_TO_WRITE);
            ui.add_space(8.0);
            if named_button(ui, "Cancel", CANCEL_JOB) {
                actions.push(Action::Cancel(job.id));
            }
        }
        JobStatus::Completed | JobStatus::Failed | JobStatus::Cancelled => {
            ui.add(Label::new(tallies(results)).wrap());
            ui.add_space(8.0);
            ui.horizontal_wrapped(|ui| {
                // A button's text never wraps: a button that does not fit goes whole to
                // the next line.
                ui.style_mut().wrap_mode = Some(TextWrapMode::Extend);
                ended_buttons(ui, job, results, to_write, actions);
            });
        }
        JobStatus::ReviewPending | JobStatus::Scanning => {}
    }
}

/// `Open output folder`; for a done job with stranded stacks, `Retry <k> stacks`; for a
/// cancelled one with confirmed stacks left to write, `Write the confirmed stacks`. A
/// failed or cancelled job has its own `Retry` above.
fn ended_buttons(
    ui: &mut egui::Ui,
    job: &JobView,
    results: ResultCounts,
    to_write: usize,
    actions: &mut Vec<Action>,
) {
    if named_button(ui, "Open output folder", OPEN_OUTPUT) {
        actions.push(Action::OpenOutput(job.config.output_dir.clone()));
    }
    if job.status == JobStatus::Completed && results.stranded > 0 {
        let retry = format!("Retry {}", count(results.stranded, "stack", "stacks"));
        if widgets::text_button(ui, &retry, Emphasis::Primary)
            .on_hover_text(RETRY_HOVER)
            .clicked()
        {
            actions.push(Action::Retry(job.id));
        }
    }
    if job.status == JobStatus::Cancelled
        && to_write > 0
        && widgets::text_button(ui, "Write the confirmed stacks", Emphasis::Primary).clicked()
    {
        actions.push(Action::Write);
    }
}

/// `<w> written, <s> skipped, <f> failed`.
fn tallies(results: ResultCounts) -> String {
    format!(
        "{} written, {} skipped, {} failed",
        results.written, results.skipped, results.failed
    )
}

/// The stage and `<done> of <total> photos`, then a bar and the photo just done; before
/// the write's first photo, a spinner.
fn write_progress(ui: &mut egui::Ui, progress: Option<&Progress>) {
    let palette = theme::palette(ui.ctx());
    let counted = progress.filter(|progress| progress.stage == Stage::Write);
    ui.horizontal(|ui| {
        if counted.is_none() {
            let spinner = ui.add(egui::Spinner::new().color(palette.accent));
            let enabled = ui.is_enabled();
            spinner.widget_info(|| {
                WidgetInfo::labeled(Role::ProgressIndicator, enabled, WRITE_PROGRESS)
            });
        }
        ui.label(
            RichText::new(stage_text(Stage::Write))
                .family(egui::FontFamily::Name(SEMIBOLD_FAMILY.into()))
                .color(palette.text),
        );
        if let Some(progress) = counted {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let done = format!(
                    "{} of {}",
                    progress.done,
                    count(progress.total, "photo", "photos")
                );
                ui.label(RichText::new(done).color(palette.secondary));
            });
        }
    });
    if let Some(progress) = counted {
        progress_bar(ui, progress, WRITE_PROGRESS);
    }
}

/// "<p> proposed and <r> rejected stacks were not written.", without a part that is zero;
/// nothing when both are.
fn not_written(results: ResultCounts) -> Option<String> {
    let parts: Vec<String> = [
        (results.proposed, "proposed"),
        (results.rejected, "rejected"),
    ]
    .into_iter()
    .filter(|&(stacks, _)| stacks > 0)
    .map(|(stacks, kind)| format!("{stacks} {kind}"))
    .collect();
    if parts.is_empty() {
        return None;
    }
    let (noun, verb) = if results.proposed + results.rejected == 1 {
        ("stack", "was")
    } else {
        ("stacks", "were")
    };
    Some(format!(
        "{} {noun} {verb} not written.",
        parts.join(" and ")
    ))
}

/// The stacks confirmed for a write, with the line that counts the others under them;
/// with none, that none was written. Returns how many rows it laid out.
fn stacks(
    ui: &mut egui::Ui,
    job: &JobView,
    review: &Review,
    input: &Path,
    thumbs: &mut ThumbCache,
    actions: &mut Vec<Action>,
) -> usize {
    let palette = theme::palette(ui.ctx());
    let note = not_written(review.results());
    if review.listed().is_empty() {
        if !writing(job) {
            ui.label(RichText::new(NOTHING_WRITTEN).color(palette.secondary));
        }
        if let Some(note) = note {
            ui.add(Label::new(RichText::new(note).color(palette.secondary)).wrap());
        }
        return 0;
    }
    // The line goes right under the list's last row. The list is only as high as its
    // rows, and at most as high as the centre leaves above the line, so a list longer
    // than that scrolls with the line still in sight under it.
    let note = note.map(|note| {
        let font = TextStyle::Body.resolve(ui.style());
        ui.painter()
            .layout(note, font, palette.secondary, ui.available_width())
    });
    let note_height = note.as_ref().map_or(0.0, |note| {
        ui.spacing().item_spacing.y + NOTE_GAP + note.size().y
    });
    let listed = review.listed();
    let writing = writing(job);
    let mut laid_out = 0;
    ScrollArea::vertical()
        .id_salt(("results", review.job))
        .auto_shrink([false, true])
        .max_height((ui.available_height() - note_height).max(0.0))
        .show_rows(ui, ROW_HEIGHT, listed.len(), |ui, shown| {
            laid_out = shown.len();
            for at in shown {
                row(ui, review, listed[at], input, thumbs, writing, actions);
            }
        });
    if let Some(note) = note {
        ui.add_space(NOTE_GAP);
        ui.add(Label::new(note));
    }
    laid_out
}

/// What a stack's chip says, and the colour of its dot: a confirmed stack reads
/// `Waiting` while its job's write is on its way or running.
fn status_of(palette: &Palette, status: GroupStatus, writing: bool) -> (&'static str, Color32) {
    if writing && status == GroupStatus::Confirmed {
        (WAITING, palette.dim)
    } else {
        (
            group_status_text(status),
            review::status_color(palette, status),
        )
    }
}

/// Stack `index`'s row: one selectable widget named `Stack <n>: <first photo>, <count>,
/// <status>`, whose thumbnail and button are on top of it, so a click on one is theirs
/// alone.
fn row(
    ui: &mut egui::Ui,
    review: &Review,
    index: usize,
    input: &Path,
    thumbs: &mut ThumbCache,
    writing: bool,
    actions: &mut Vec<Action>,
) {
    let group = &review.groups[index];
    let palette = theme::palette(ui.ctx());
    let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), ROW_HEIGHT));
    let id = Id::unique(("results stack", review.job, group.id));
    let response = ui.interact(rect, id, Sense::click());
    let selection = &review.selection;
    let selected = selection.groups.contains(&group.id);
    let number = group.position + 1;
    let status = status_of(palette, group.status, writing);
    let first = group
        .members
        .first()
        .and_then(|&file| review.files.get(file))
        .map(|file| folder_name(&file.path))
        .unwrap_or_default();
    let photos = count(group.members.len(), "photo", "photos");
    let label = format!("Stack {number}: {first}, {photos}, {}", status.0);
    response.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, &label));
    if response.clicked() {
        actions.push(Action::Select(group.id));
    }
    let look = if selection.primary() == Some(group.id) {
        Look::Primary
    } else if selected {
        Look::Selected
    } else if response.contains_pointer() {
        Look::Pointed
    } else {
        Look::Plain
    };
    review::paint_row(ui, rect, palette, look);

    let inner = rect.shrink(ROW_PADDING);
    let thumb = Rect::from_min_size(inner.min, THUMB_SIZE);
    top_layer(ui, thumb, review, group, input, thumbs, actions);
    let left = thumb.right() + THUMB_GAP;
    let line = Rect::from_min_max(
        egui::pos2(left, inner.top()),
        egui::pos2(inner.right(), inner.top() + LINE_HEIGHT),
    );
    let facts = Facts {
        number,
        first: &first,
        photos: &photos,
        status,
    };
    first_line(ui, id, line, review, group, &facts);
    let detail = Rect::from_min_max(egui::pos2(left, line.bottom() + 4.0), inner.max);
    outcome(ui, id, detail, group, number, actions);
}

/// The top layer's thumbnail in `rect`, which a click selects; nothing is asked of the
/// cache unless it is on screen.
fn top_layer(
    ui: &mut egui::Ui,
    rect: Rect,
    review: &Review,
    group: &GroupView,
    input: &Path,
    thumbs: &mut ThumbCache,
    actions: &mut Vec<Action>,
) {
    let Some(&file) = group.members.first() else {
        return;
    };
    if !ui.is_rect_visible(rect) {
        return;
    }
    let view = review.files.get(file);
    let photo = Photo {
        job: review.job,
        file,
        view,
        input,
        selected: review.selection.photo == Some(file),
        // Nowhere to drop it: the stacks are no longer edited.
        draggable: false,
    };
    if photo::thumbnail(ui, rect, Shown::of(thumbs, view), &photo) {
        actions.push(Action::SelectPhoto(file));
    }
}

/// What a row's first line says of its stack.
struct Facts<'a> {
    number: usize,
    first: &'a str,
    photos: &'a str,
    /// The chip's text and the colour of its dot.
    status: (&'static str, Color32),
}

/// `Stack <n>`, the first photo's name (cut in the middle, whole on hover), the photo
/// count and the status chip, then the write's warnings at the far end of `line`. A line
/// too short for them leaves out the photo count, then cuts the name, then leaves it out;
/// the number and the status always stay, as the row's name keeps them all.
fn first_line(
    ui: &mut egui::Ui,
    id: Id,
    line: Rect,
    review: &Review,
    group: &GroupView,
    facts: &Facts,
) {
    let palette = theme::palette(ui.ctx());
    let mut child = ui.new_child(
        UiBuilder::new()
            .scope_id(id.with("facts"))
            .max_rect(line)
            .layout(Layout::left_to_right(Align::Center)),
    );
    child.set_clip_rect(line.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.x = FACT_GAP;
    // The texts only show: a click on them is the row's.
    child.style_mut().interaction.selectable_labels = false;

    let warnings = group.warnings.len();
    let number = RichText::new(format!("Stack {}", facts.number))
        .family(egui::FontFamily::Name(SEMIBOLD_FAMILY.into()));
    let photos = RichText::new(facts.photos).color(palette.secondary);
    let warnings_width = if warnings > 0 {
        FACT_GAP + review::text_width(&child, review::warnings_text(&child, warnings))
    } else {
        0.0
    };
    let fixed = review::text_width(&child, number.clone())
        + widgets::chip_width(&child, facts.status.0)
        + warnings_width;
    let with_count =
        line.width() - fixed - review::text_width(&child, photos.clone()) - 3.0 * FACT_GAP;
    let without_count = line.width() - fixed - 2.0 * FACT_GAP;
    // The count gives way before the name is cut: the name says which stack it is.
    let whole_name = review::text_width(&child, facts.first);
    let (name_width, show_count) = if with_count >= whole_name {
        (with_count, true)
    } else {
        (without_count, false)
    };

    child.label(number);
    if name_width >= NAME_AT_ALL {
        let font = TextStyle::Body.resolve(child.style());
        let shown = widgets::fitted(&child, facts.first, &font, name_width);
        let cut = shown != facts.first;
        let name = child.add(Label::new(RichText::new(shown).color(palette.text)));
        if cut {
            name.on_hover_text(facts.first);
        }
    }
    if show_count {
        child.label(photos);
    }
    let (status, color) = facts.status;
    widgets::chip(&mut child, status, color);
    if warnings > 0 {
        child.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let enabled = ui.is_enabled();
            let name = format!("Warnings of stack {}: {warnings}", facts.number);
            let response = ui.add(Label::new(review::warnings_text(ui, warnings)));
            response.widget_info(|| WidgetInfo::labeled(Role::Label, enabled, &name));
            // Built only while it shows.
            response.on_hover_ui(|ui| {
                ui.label(warning_lines(review, group));
            });
        });
    }
}

/// Each warning of `group`'s write, one per line: `<file name>: <warning>`.
fn warning_lines(review: &Review, group: &GroupView) -> String {
    group
        .warnings
        .iter()
        .map(|(file, warning)| {
            let name = review
                .files
                .get(*file)
                .map(|view| folder_name(&view.path))
                .unwrap_or_default();
            format!("{name}: {warning}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// What the write made of the stack, in `rect` under its first line: the file written,
/// cut in the middle (its whole path on hover), and `Reveal`; or why it was skipped or
/// failed, wrapping within `rect` and cut short with the whole reason on hover. Nothing
/// for a stack still waiting.
fn outcome(
    ui: &mut egui::Ui,
    id: Id,
    rect: Rect,
    group: &GroupView,
    number: usize,
    actions: &mut Vec<Action>,
) {
    let palette = theme::palette(ui.ctx());
    if let Some(path) = &group.output_path {
        let line = Rect::from_min_size(rect.min, Vec2::new(rect.width(), LINE_HEIGHT));
        let mut child = ui.new_child(
            UiBuilder::new()
                .scope_id(id.with("written"))
                .max_rect(line)
                .layout(Layout::right_to_left(Align::Center)),
        );
        child.set_clip_rect(line.intersect(ui.clip_rect()));
        let enabled = child.is_enabled();
        let reveal = widgets::text_button(&mut child, "Reveal", Emphasis::Normal)
            .on_hover_text("Show the file in the file manager.");
        let name = format!("Reveal stack {number}");
        reveal.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, &name));
        if reveal.clicked() {
            actions.push(Action::Reveal(path.clone()));
        }
        child.with_layout(Layout::left_to_right(Align::Center), |ui| {
            ui.style_mut().interaction.selectable_labels = false;
            let font = TextStyle::Body.resolve(ui.style());
            let file = folder_name(path);
            let shown = widgets::fitted(ui, &file, &font, ui.available_width());
            ui.add(Label::new(RichText::new(shown).color(palette.text)))
                .on_hover_text(display_path(path));
        });
        return;
    }
    let Some(error) = &group.error else {
        return;
    };
    let (icon, color, kind) = match group.status {
        GroupStatus::Skipped => (ICON_WARNING, palette.warning, "Warning"),
        _ => (ICON_ERROR, palette.danger, "Error"),
    };
    let mut child = ui.new_child(
        UiBuilder::new()
            .scope_id(id.with("reason"))
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Min)),
    );
    child.set_clip_rect(rect.intersect(ui.clip_rect()));
    child.spacing_mut().item_spacing.x = 4.0;
    child.style_mut().interaction.selectable_labels = false;
    let enabled = child.is_enabled();
    let mark = child.label(icon.rich_text().color(color));
    mark.widget_info(|| WidgetInfo::labeled(Role::Image, enabled, kind));
    let reason = sentence(error);
    let font = TextStyle::Body.resolve(child.style());
    let rows = (rect.height() / child.text_style_height(&TextStyle::Body))
        .floor()
        .max(1.0) as usize;
    let mut text = LayoutJob::simple(reason.clone(), font, palette.text, child.available_width());
    text.wrap.max_rows = rows;
    text.wrap.overflow_character = Some('…');
    let galley = child.ctx().fonts_mut(|fonts| fonts.layout_job(text));
    let elided = galley.elided;
    let response = child.add(Label::new(galley));
    if elided {
        response.on_hover_text(reason);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stacks_a_write_left_alone_are_counted_in_one_sentence() {
        let results = |proposed, rejected| ResultCounts {
            proposed,
            rejected,
            ..ResultCounts::default()
        };
        assert_eq!(not_written(results(0, 0)), None);
        assert_eq!(
            not_written(results(1, 0)).as_deref(),
            Some("1 proposed stack was not written.")
        );
        assert_eq!(
            not_written(results(0, 3)).as_deref(),
            Some("3 rejected stacks were not written.")
        );
        assert_eq!(
            not_written(results(2, 1)).as_deref(),
            Some("2 proposed and 1 rejected stacks were not written.")
        );
        let written = ResultCounts {
            written: 2,
            skipped: 1,
            ..ResultCounts::default()
        };
        assert_eq!(tallies(written), "2 written, 1 skipped, 0 failed");
    }
}
