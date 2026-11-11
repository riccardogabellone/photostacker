//! The inspector, the window's right panel (spec §9.2): the selection in detail, and the
//! keyboard's and screen readers' way to do what dragging does. It has three states:
//!
//! - **Nothing selected** in a job: the job's facts (its folders, format, RAW tone and
//!   RAW+JPEG preference, thresholds, model) and its counts.
//! - **A stack** (the primary one): its number, status and chips, its photo count and
//!   range of exposures, what it will be written as (canvas, depth, file name), its
//!   distance and visual check; once written, its file with `Reveal`, or why it was not.
//!   Then Confirm, Reject, Split off the checked photos and Merge, and its members in
//!   layer order, each with a checkbox (for the split), its layer name, its resolution
//!   and depth, its warnings and a button that moves it to the tray.
//! - **A photo:** a larger preview, its name and folder, its exposure, its warnings, the
//!   RAW+JPEG pair it is the active file of, and the moves: to the tray, to a new stack,
//!   or to a stack of its folder.
//!
//! The larger preview and the prediction come from [`Inspect`], off the UI thread: until
//! they arrive the inspector shows the 256 px thumbnail and "Measuring…". Everything
//! scrolls vertically in one scroll area, and nothing in it widens the panel: long names
//! are cut in the middle, with the whole name on hover. A stack's members are rows of one
//! height, of which only those on screen are laid out.

use std::path::{Path, PathBuf};

use eframe::egui::{
    self, Align, Align2, Button, Checkbox, Color32, FontId, Id, Label, Layout, Rect, RichText,
    Role, ScrollArea, Sense, TextStyle, TextWrapMode, UiBuilder, Vec2, WidgetInfo, WidgetText,
};
use egui_material_icons::icons::{ICON_ERROR, ICON_REMOVE, ICON_WARNING};
use photostacker_core::engine::{FileView, GroupId, GroupView, JobView, MoveTarget};
use photostacker_core::model::{
    GroupOrigin, GroupStatus, JobStatus, MlStatus, OutputFormat, RawJpegPreference, RawTone,
};
use photostacker_core::models;
use photostacker_core::write::{StackPrediction, layer_exposure, layer_name};

use crate::controller::{Controller, Reveal, Review};
use crate::format::{
    aperture, bracket_text, camera, capitalised, count, depth, display_path, ev, ev_range,
    exposure_time, focal_length, folder_name, group_status_text, resolution, sentence, utc_offset,
};
use crate::inspect::{Inspect, Inspected};
use crate::theme::{self, SEMIBOLD_FAMILY};
use crate::thumbs::ThumbCache;
use crate::views::job::banner;
use crate::views::photo::{self, Shown};
use crate::views::review::{ml_badge, status_color};
use crate::widgets::{self, Emphasis};

/// The screen reader's name of the whole panel.
pub const PANE: &str = "Inspector";
/// What the inspector says with no job selected.
const NO_JOB: &str = "Select a job to see its details here.";
/// What a line says until the prediction arrives.
const MEASURING: &str = "Measuring…";
/// What a stack the writer would skip says.
const TOO_FEW: &str = "Needs 2 photos of one resolution to be written.";
/// What the predicted name's hover says.
const NAME_NOTE: &str = "(a name already taken gets a number added)";
/// What the preview says while the engine is busy and no larger preview is asked for.
pub const PREVIEW_WAITS: &str = "The larger preview waits while a job is running.";
/// The most stacks `Move to stack…` offers.
const MOVE_MENU: usize = 12;
/// The widest `Move to stack…`'s menu runs: its entries truncate past it.
const MENU_WIDTH: f32 = 280.0;
/// The widest a fact's caption column gets, as a share of the panel's width.
const CAPTION_SHARE: f32 = 0.45;
/// Between a fact's caption and its value.
const CAPTION_GAP: f32 = 8.0;
/// Between the inspector's sections.
const SECTION_GAP: f32 = 12.0;
/// A member's first line: its checkbox, its layer name and its button.
const MEMBER_LINE: f32 = 28.0;
/// A member's second line: its resolution, depth and warnings.
const MEMBER_DETAIL: f32 = 18.0;
/// Where a member's second line starts: under its name, past the checkbox.
const MEMBER_INDENT: f32 = 26.0;
/// Between a file the scan could not read and its reason.
const UNREADABLE_GAP: f32 = 2.0;

/// What the user asked for; done once the inspector is drawn, which borrows the
/// controller.
enum Action {
    Confirm(GroupId),
    Reject(GroupId),
    Toggle(usize),
    Split,
    Merge,
    Move { file: usize, to: MoveTarget },
    Swap(usize),
    Reveal(PathBuf),
    SelectPhoto(usize),
}

/// Draws the inspector into the right panel `ui`: the selected job's facts, its primary
/// stack, or its selected photo.
pub fn show(
    ui: &mut egui::Ui,
    controller: &mut Controller,
    thumbs: &mut ThumbCache,
    inspect: &mut Inspect,
) {
    pane(ui);
    let mut actions = Vec::new();
    ScrollArea::vertical()
        .id_salt("inspector")
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.set_max_width(ui.available_width());
            draw(ui, controller, thumbs, inspect, &mut actions);
        });
    for action in actions {
        match action {
            Action::Confirm(group) => controller.confirm(group),
            Action::Reject(group) => controller.reject(group),
            Action::Toggle(file) => controller.toggle_member(file),
            Action::Split => controller.split_checked(),
            Action::Merge => controller.merge_selected(),
            Action::Move { file, to } => controller.move_photo(file, to),
            Action::Swap(file) => controller.swap_active(file),
            Action::Reveal(path) => controller.reveal(&path, Reveal::File),
            Action::SelectPhoto(file) => controller.select_photo(file),
        }
    }
}

/// The whole panel, named for screen readers. Registered before its content, which is
/// on top of it.
fn pane(ui: &mut egui::Ui) {
    let response = ui.interact(ui.max_rect(), Id::unique("inspector pane"), Sense::hover());
    response.widget_info(|| WidgetInfo::labeled(Role::Pane, true, PANE));
}

/// What the selection shows: the selected job's photo, else its primary stack, else the
/// job itself.
fn draw(
    ui: &mut egui::Ui,
    controller: &Controller,
    thumbs: &mut ThumbCache,
    inspect: &mut Inspect,
    actions: &mut Vec<Action>,
) {
    let Some(job) = controller.selected() else {
        let palette = theme::palette(ui.ctx());
        ui.add(Label::new(RichText::new(NO_JOB).color(palette.secondary)).wrap());
        return;
    };
    let review = controller
        .review()
        .filter(|review| review.job == job.id && review.loaded());
    let Some(review) = review else {
        job_facts(ui, controller, job, None);
        return;
    };
    let photo = review
        .selection
        .photo
        .and_then(|file| review.files.get(file));
    if let Some(file) = photo {
        let busy = controller.engine_busy();
        photo_state(ui, job, review, file, busy, thumbs, inspect, actions);
    } else if let Some(group) = review.primary() {
        stack_state(ui, job, review, group, inspect, actions);
    } else {
        job_facts(ui, controller, job, Some(review));
    }
}

// --- Facts ---

/// How a value too long for its line is cut.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cut {
    /// In the middle, keeping both ends: a path or a file name.
    Middle,
    /// At the end.
    End,
}

/// One fact: its caption, its value and how the value is cut.
struct Fact {
    caption: &'static str,
    value: String,
    cut: Cut,
}

impl Fact {
    fn new(caption: &'static str, value: impl Into<String>) -> Fact {
        Fact {
            caption,
            value: value.into(),
            cut: Cut::End,
        }
    }

    fn path(caption: &'static str, path: &Path) -> Fact {
        Fact {
            caption,
            value: display_path(path),
            cut: Cut::Middle,
        }
    }
}

/// `facts`, one per line: the captions in `secondary` in a column as wide as the widest
/// (at most [`CAPTION_SHARE`] of the panel), the values after them in `text`. A caption
/// is painted, not a widget: each value is named `<caption>: <value>` for screen readers.
fn facts(ui: &mut egui::Ui, facts: &[Fact]) {
    let palette = theme::palette(ui.ctx());
    let font = TextStyle::Body.resolve(ui.style());
    let widest = facts
        .iter()
        .map(|fact| text_width(ui, fact.caption, &font))
        .fold(0.0, f32::max);
    let column = widest.min(ui.available_width() * CAPTION_SHARE);
    for fact in facts {
        ui.horizontal(|ui| {
            let height = ui.text_style_height(&TextStyle::Body);
            let (rect, _) = ui.allocate_exact_size(Vec2::new(column, height), Sense::hover());
            ui.painter().with_clip_rect(rect).text(
                rect.left_center(),
                Align2::LEFT_CENTER,
                fact.caption,
                font.clone(),
                palette.secondary,
            );
            ui.add_space(CAPTION_GAP - ui.spacing().item_spacing.x);
            let name = format!("{}: {}", fact.caption, fact.value);
            value(ui, &fact.value, &name, fact.cut, palette.text);
        });
    }
}

/// `text` in `color`, on one line, cut as `cut` says when it is too long, with the whole
/// text on hover; named `name` for screen readers.
fn value(ui: &mut egui::Ui, text: &str, name: &str, cut: Cut, color: Color32) -> egui::Response {
    let font = TextStyle::Body.resolve(ui.style());
    let shown = match cut {
        Cut::Middle => widgets::fitted(ui, text, &font, ui.available_width()),
        Cut::End => text.to_owned(),
    };
    let cut_here = shown != text;
    // egui cuts it at the end, should the measure and the layout differ by a pixel.
    let response = ui.add(
        Label::new(RichText::new(shown).color(color))
            .truncate()
            .selectable(false)
            .show_tooltip_when_elided(cut == Cut::End),
    );
    let response = if cut_here {
        response.on_hover_text(text)
    } else {
        response
    };
    let enabled = ui.is_enabled();
    response.widget_info(|| WidgetInfo::labeled(Role::Label, enabled, name));
    response
}

/// How wide `text` is in `font`, on one line.
fn text_width(ui: &egui::Ui, text: &str, font: &FontId) -> f32 {
    ui.ctx().fonts_mut(|fonts| {
        fonts
            .layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER)
            .size()
            .x
    })
}

/// A heading: `text` in semibold, cut in the middle, named `name`.
fn heading(ui: &mut egui::Ui, text: &str, name: &str) {
    let palette = theme::palette(ui.ctx());
    let size = TextStyle::Body.resolve(ui.style()).size + 1.0;
    let font = FontId::new(size, egui::FontFamily::Name(SEMIBOLD_FAMILY.into()));
    let shown = widgets::fitted(ui, text, &font, ui.available_width());
    let cut = shown != text;
    let response = ui.add(
        Label::new(RichText::new(shown).font(font).color(palette.text))
            .truncate()
            .selectable(false)
            .show_tooltip_when_elided(false),
    );
    let response = if cut {
        response.on_hover_text(text)
    } else {
        response
    };
    response.widget_info(|| WidgetInfo::labeled(Role::Label, true, name));
}

/// A line of `text` in `color`, cut at the end when it is too long.
fn line(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    ui.add(
        Label::new(RichText::new(text).color(color))
            .truncate()
            .selectable(false),
    )
}

/// A line that wraps: `text` in `color`.
fn wrapped(ui: &mut egui::Ui, text: &str, color: Color32) {
    ui.add(
        Label::new(RichText::new(text).color(color))
            .wrap()
            .selectable(false),
    );
}

/// A warning: the icon in `warning`, then `text` in `text`, wrapping.
fn warning(ui: &mut egui::Ui, text: &str) {
    let palette = theme::palette(ui.ctx());
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let mark = ui.label(ICON_WARNING.rich_text().color(palette.warning));
        mark.widget_info(|| WidgetInfo::labeled(Role::Image, true, "Warning"));
        wrapped(ui, text, palette.text);
    });
}

// --- Nothing selected: the job ---

/// The job's facts: its folders, its settings and its counts. `review` is its review,
/// once loaded, which counts the tray.
fn job_facts(ui: &mut egui::Ui, controller: &Controller, job: &JobView, review: Option<&Review>) {
    let name = controller.job_name(job.id);
    heading(ui, name, &format!("Job {name}"));
    ui.add_space(4.0);
    let config = &job.config;
    let mut settings = vec![
        Fact::path("Input", &config.input_dir),
        Fact::path("Output", &config.output_dir),
        Fact::new(
            "Format",
            match config.output_format {
                OutputFormat::Psd => "PSD",
                OutputFormat::Tiff => "Layered TIFF",
            },
        ),
        Fact::new(
            "RAW tone",
            match config.raw_tone {
                RawTone::Srgb => "sRGB",
                RawTone::Linear => "Linear (for HDR merging)",
            },
        ),
        Fact::new(
            "RAW+JPEG",
            match config.raw_jpeg_preference {
                RawJpegPreference::Raw => "Stack the RAW",
                RawJpegPreference::Jpeg => "Stack the JPEG",
            },
        ),
        Fact::new("Hash distance", config.hamming_threshold.to_string()),
        Fact::new("Time window", format!("{} s", config.time_window_seconds)),
    ];
    let model = config.ml_model.as_deref();
    match model.map(|id| (id, models::model(id))) {
        Some((id, spec)) => {
            let name = spec.map_or(id, |spec| spec.name);
            settings.push(Fact::new("Visual check", name));
            let similarity = match (config.cosine_threshold, spec) {
                (Some(own), _) => format!("{own:.2}"),
                (None, Some(spec)) => format!("{:.2}, the model's", spec.default_cosine),
                (None, None) => "the model's".to_owned(),
            };
            settings.push(Fact::new("Similarity", similarity));
            settings.push(Fact::new(
                "Check floor",
                config.stage3_ambiguity_floor.to_string(),
            ));
        }
        None => settings.push(Fact::new("Visual check", "no visual check")),
    }
    facts(ui, &settings);

    let counts = &job.counts;
    if !job.scan_done && counts.files == 0 {
        return;
    }
    ui.add_space(SECTION_GAP);
    let mut numbers = vec![Fact::new("Photos", counts.files.to_string())];
    if counts.errors > 0 {
        numbers.push(Fact::new("Could not be read", counts.errors.to_string()));
    }
    numbers.push(Fact::new("Stacks", counts.groups().to_string()));
    for (caption, n) in [
        ("Proposed", counts.proposed),
        ("Confirmed", counts.confirmed),
        ("Rejected", counts.rejected),
        ("Written", counts.written),
        ("Skipped", counts.skipped),
        ("Failed", counts.failed),
    ] {
        if n > 0 {
            numbers.push(Fact::new(caption, n.to_string()));
        }
    }
    if let Some(review) = review {
        numbers.push(Fact::new("Unstacked", review.unstacked.len().to_string()));
    }
    facts(ui, &numbers);
    if let Some(review) = review {
        unreadable_files(ui, review, &config.input_dir);
    }
}

/// The files the job's scan could not read, once its analysis is saved: `Could not be
/// read (<n>)`, then each file's name (its path inside the job's folder on hover) over
/// its reason as a sentence, in rows of one height of which only those on screen are laid
/// out, since a job can have hundreds. Nothing when there are none.
fn unreadable_files(ui: &mut egui::Ui, review: &Review, input: &Path) {
    let unreadable = review.unreadable();
    if unreadable.is_empty() {
        return;
    }
    let palette = theme::palette(ui.ctx());
    ui.add_space(SECTION_GAP);
    let title = format!("Could not be read ({})", unreadable.len());
    heading(ui, &title, &title);
    let name_height = ui.text_style_height(&TextStyle::Body);
    let height = name_height + ui.text_style_height(&TextStyle::Small) + UNREADABLE_GAP;
    let pitch = height + ui.spacing().item_spacing.y;
    let width = ui.available_width();
    let rows = unreadable.len();
    let (_, area) = ui.allocate_space(Vec2::new(width, rows as f32 * pitch));
    let clip = ui.clip_rect();
    let first = ((clip.top() - area.top()) / pitch).floor().max(0.0) as usize;
    let end = (((clip.bottom() - area.top()) / pitch).ceil().max(0.0) as usize).min(rows);
    for (at, &id) in unreadable.iter().enumerate().take(end).skip(first) {
        let Some(file) = review.files.get(id) else {
            continue;
        };
        let rect = Rect::from_min_size(
            egui::pos2(area.left(), area.top() + at as f32 * pitch),
            Vec2::new(width, height),
        );
        let mut row = ui.new_child(
            UiBuilder::new()
                .id_salt(("unreadable", id))
                .max_rect(rect)
                .layout(Layout::top_down(Align::Min)),
        );
        row.set_clip_rect(rect.intersect(ui.clip_rect()));
        row.spacing_mut().item_spacing.y = UNREADABLE_GAP;
        let inside = photo::relative_path(&file.path, input);
        let name = folder_name(&file.path);
        let font = TextStyle::Body.resolve(row.style());
        let shown = widgets::fitted(&row, &name, &font, width);
        let response = row
            .add(
                Label::new(RichText::new(shown).color(palette.text))
                    .truncate()
                    .selectable(false)
                    .show_tooltip_when_elided(false),
            )
            .on_hover_text(&inside);
        let label = format!("Could not be read: {inside}");
        response.widget_info(|| WidgetInfo::labeled(Role::Label, true, &label));
        let reason = sentence(file.error.as_deref().unwrap_or_default());
        row.add(
            Label::new(RichText::new(reason).small().color(palette.secondary))
                .truncate()
                .selectable(false),
        );
    }
}

// --- A stack ---

/// The primary stack: what it is and will be, its actions and its members.
fn stack_state(
    ui: &mut egui::Ui,
    job: &JobView,
    review: &Review,
    group: &GroupView,
    inspect: &mut Inspect,
    actions: &mut Vec<Action>,
) {
    let palette = theme::palette(ui.ctx());
    let number = group.position + 1;
    ui.horizontal_wrapped(|ui| {
        heading_inline(ui, &format!("Stack {number}"));
        widgets::chip(
            ui,
            group_status_text(group.status),
            status_color(palette, group.status),
        );
        if group.members.len() < 2 {
            widgets::chip(ui, "needs 2 photos", palette.warning);
        }
        if group.origin == GroupOrigin::Manual {
            widgets::chip(ui, "manual", palette.accent);
        }
        if group.review_reset {
            widgets::chip(ui, "look again", palette.warning);
        }
        ml_badge(ui, group);
    });
    ui.add_space(4.0);
    line(
        ui,
        &count(group.members.len(), "photo", "photos"),
        palette.secondary,
    );
    if let Some((lowest, highest)) = exposures(review, group) {
        line(ui, &ev_range(lowest, highest), palette.secondary);
    }
    let prediction = inspect.prediction(job, group, &review.files);
    measured(ui, group, &prediction, actions);
    line(
        ui,
        &format!("hash distance up to {}", group.max_distance),
        palette.secondary,
    );
    if let Some(check) = visual_check(group) {
        line(ui, &check, palette.secondary);
    }
    write_outcome(ui, review, group);

    ui.add_space(SECTION_GAP);
    let editable = job.status == JobStatus::ReviewPending;
    stack_buttons(ui, review, group, editable, actions);

    ui.add_space(SECTION_GAP);
    line(ui, "Photos, top layer first", palette.secondary);
    let ready = match &prediction {
        Inspected::Ready(prediction) => Some(*prediction),
        Inspected::Waiting | Inspected::Failed(_) => None,
    };
    let waiting = matches!(prediction, Inspected::Waiting);
    members(ui, review, group, ready, waiting, editable, actions);
}

/// A heading inside a row: `text` in semibold.
fn heading_inline(ui: &mut egui::Ui, text: &str) {
    let palette = theme::palette(ui.ctx());
    let size = TextStyle::Body.resolve(ui.style()).size + 1.0;
    let font = FontId::new(size, egui::FontFamily::Name(SEMIBOLD_FAMILY.into()));
    ui.add(Label::new(RichText::new(text).font(font).color(palette.text)).selectable(false));
}

/// The lowest and the highest exposure of `group`'s members, as their layer names show
/// them; none when no member has one.
fn exposures(review: &Review, group: &GroupView) -> Option<(f32, f32)> {
    group
        .members
        .iter()
        .filter_map(|&member| review.files.get(member)?.exif.as_ref())
        .filter_map(layer_exposure)
        .fold(None, |range, ev| match range {
            None => Some((ev, ev)),
            Some((low, high)) => Some((low.min(ev), high.max(ev))),
        })
}

/// What the prediction says of the stack: its canvas and depth, and, until it is
/// written, the file it will be written as; "Measuring…" until it arrives.
fn measured(
    ui: &mut egui::Ui,
    group: &GroupView,
    prediction: &Inspected<'_, StackPrediction>,
    actions: &mut Vec<Action>,
) {
    let palette = theme::palette(ui.ctx());
    match prediction {
        Inspected::Ready(prediction) => match prediction.canvas {
            Some((width, height)) => {
                let alpha = if prediction.has_alpha {
                    ", with alpha"
                } else {
                    ""
                };
                let canvas = format!(
                    "{}, {}{alpha}",
                    resolution(width, height),
                    depth(prediction.sixteen_bit)
                );
                line(ui, &canvas, palette.text);
            }
            None => {
                line(ui, "None of its photos can be read.", palette.text);
            }
        },
        Inspected::Waiting => {
            line(ui, MEASURING, palette.secondary);
        }
        Inspected::Failed(reason) => {
            wrapped(
                ui,
                &format!("Its photos cannot be measured: {reason}"),
                palette.text,
            );
        }
    }
    ui.add_space(4.0);
    if let Some(path) = &group.output_path {
        written(ui, path, actions);
        return;
    }
    match prediction {
        Inspected::Ready(prediction) => match &prediction.file_name {
            Some(name) => {
                line(ui, "Will be written as", palette.secondary);
                let font = FontId::new(
                    TextStyle::Body.resolve(ui.style()).size,
                    egui::FontFamily::Name(SEMIBOLD_FAMILY.into()),
                );
                let shown = widgets::fitted(ui, name, &font, ui.available_width());
                ui.add(
                    Label::new(RichText::new(shown).font(font).color(palette.text))
                        .truncate()
                        .selectable(false)
                        .show_tooltip_when_elided(false),
                )
                .on_hover_text(format!("{name}\n{NAME_NOTE}"));
            }
            None => warning(ui, TOO_FEW),
        },
        Inspected::Waiting => {
            line(ui, "Will be written as", palette.secondary);
            line(ui, MEASURING, palette.secondary);
        }
        Inspected::Failed(_) => {}
    }
}

/// A written stack's file: its path, cut in the middle, and `Reveal`, named `Reveal <file
/// name>`.
fn written(ui: &mut egui::Ui, path: &Path, actions: &mut Vec<Action>) {
    let palette = theme::palette(ui.ctx());
    line(ui, "Written as", palette.secondary);
    let shown = display_path(path);
    value(
        ui,
        &shown,
        &format!("Written as {shown}"),
        Cut::Middle,
        palette.text,
    );
    let enabled = ui.is_enabled();
    let reveal = widgets::text_button(ui, "Reveal", Emphasis::Normal)
        .on_hover_text("Show the file in the file manager.");
    let name = format!("Reveal {}", folder_name(path));
    reveal.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, &name));
    if reveal.clicked() {
        actions.push(Action::Reveal(path.to_path_buf()));
    }
}

/// What the visual check made of the stack, when it had something to say.
fn visual_check(group: &GroupView) -> Option<String> {
    let score = group
        .ml_score
        .map(|score| format!(" ({score:.2})"))
        .unwrap_or_default();
    match group.ml_status {
        MlStatus::None => None,
        MlStatus::Confirmed => Some(format!("Visual check: alike{score}")),
        MlStatus::Split => Some(format!("Visual check: split off a larger stack{score}")),
        MlStatus::Skipped => Some("Visual check: skipped".to_owned()),
        MlStatus::Pending => Some("Visual check: pending".to_owned()),
    }
}

/// Why the stack's write failed or skipped it, as the results' row says it
/// (`format::sentence`), and the warnings of its write.
fn write_outcome(ui: &mut egui::Ui, review: &Review, group: &GroupView) {
    let palette = theme::palette(ui.ctx());
    if let Some(error) = &group.error {
        ui.add_space(8.0);
        let text = sentence(error);
        match group.status {
            GroupStatus::Skipped => banner(ui, ICON_WARNING, "Warning", palette.warning, &text),
            _ => banner(ui, ICON_ERROR, "Error", palette.danger, &text),
        }
    }
    for (file, warning_text) in &group.warnings {
        let name = review
            .files
            .get(*file)
            .map(|file| folder_name(&file.path))
            .unwrap_or_default();
        warning(ui, &format!("{name}: {warning_text}"));
    }
}

/// Confirm and Reject, the one matching the stack's decision filled; then Split off the
/// checked photos and Merge. All disabled once the job is not in review.
fn stack_buttons(
    ui: &mut egui::Ui,
    review: &Review,
    group: &GroupView,
    editable: bool,
    actions: &mut Vec<Action>,
) {
    let (confirm, reject) = match group.status {
        GroupStatus::Confirmed => (Emphasis::Positive, Emphasis::Normal),
        GroupStatus::Rejected => (Emphasis::Normal, Emphasis::Danger),
        _ => (Emphasis::Normal, Emphasis::Normal),
    };
    ui.add_enabled_ui(editable, |ui| {
        buttons_row(ui, |ui| {
            if widgets::text_button(ui, "Confirm", confirm).clicked() {
                actions.push(Action::Confirm(group.id));
            }
            if widgets::text_button(ui, "Reject", reject).clicked() {
                actions.push(Action::Reject(group.id));
            }
        });
        buttons_row(ui, |ui| {
            let checked = review.checked().len();
            let split = checked >= 2 && group.members.len() >= checked + 2;
            let response = widgets::text_button_enabled(
                ui,
                split,
                "Split off the checked photos",
                Emphasis::Normal,
            )
            .on_disabled_hover_text(
                "Check 2 photos or more, and leave 2 or more: each stack needs 2 photos.",
            );
            if response.clicked() {
                actions.push(Action::Split);
            }
            let selected = review.selection.groups.len();
            let text = if selected >= 2 {
                format!("Merge {selected} stacks")
            } else {
                "Merge stacks".to_owned()
            };
            let response = widgets::text_button_enabled(ui, selected >= 2, &text, Emphasis::Normal)
                .on_hover_text("Merge the selected stacks into this one.")
                .on_disabled_hover_text(format!(
                    "{}+click other stacks in the list to merge them into this one.",
                    if cfg!(target_os = "macos") {
                        "Cmd"
                    } else {
                        "Ctrl"
                    }
                ));
            if response.clicked() {
                actions.push(Action::Merge);
            }
        });
    });
}

/// A row of buttons that moves a button whole to its next line when it does not fit:
/// a button's text never wraps.
fn buttons_row(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal_wrapped(|ui| {
        ui.style_mut().wrap_mode = Some(TextWrapMode::Extend);
        add(ui);
    });
}

/// The stack's members in layer order, one row each, laying out only the rows on
/// screen.
fn members(
    ui: &mut egui::Ui,
    review: &Review,
    group: &GroupView,
    prediction: Option<&StackPrediction>,
    waiting: bool,
    editable: bool,
    actions: &mut Vec<Action>,
) {
    let rows = group.members.len();
    let height = MEMBER_LINE + MEMBER_DETAIL;
    let pitch = height + ui.spacing().item_spacing.y;
    let width = ui.available_width();
    let (_, area) = ui.allocate_space(Vec2::new(width, rows as f32 * pitch));
    let clip = ui.clip_rect();
    let first = ((clip.top() - area.top()) / pitch).floor().max(0.0) as usize;
    let end = (((clip.bottom() - area.top()) / pitch).ceil().max(0.0) as usize).min(rows);
    for at in first..end {
        let rect = Rect::from_min_size(
            egui::pos2(area.left(), area.top() + at as f32 * pitch),
            Vec2::new(width, height),
        );
        let member = Member {
            at,
            file: group.members[at],
            prediction,
            waiting,
        };
        member_row(ui, rect, review, group, &member, editable, actions);
    }
}

/// A member of the stack shown, as its row draws it.
struct Member<'a> {
    /// Its place in the stack: 0 is the top layer.
    at: usize,
    file: usize,
    prediction: Option<&'a StackPrediction>,
    /// The prediction is on its way.
    waiting: bool,
}

/// A member's row in `rect`: its checkbox, its layer name, which selects its photo (named
/// `Select <file name>`), and `Move <name> to unstacked`, then under its name its
/// resolution and depth, or why it is left out, and its warnings.
fn member_row(
    ui: &mut egui::Ui,
    rect: Rect,
    review: &Review,
    group: &GroupView,
    member: &Member<'_>,
    editable: bool,
    actions: &mut Vec<Action>,
) {
    let palette = theme::palette(ui.ctx());
    let view = review.files.get(member.file);
    let name = view.map(|view| folder_name(&view.path)).unwrap_or_default();
    let stem = view
        .and_then(|view| view.path.file_stem())
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let layer = layer_name(&stem, view.and_then(|view| view.exif.as_ref()));
    // The prediction's entry for this member: it lists them in the stack's order.
    let probe = member
        .prediction
        .and_then(|prediction| prediction.members.get(member.at))
        .filter(|(id, _)| *id == member.file)
        .map(|(_, probe)| probe);
    let left_out = probe.is_some_and(|probe| probe.is_ok())
        && member
            .prediction
            .is_some_and(|prediction| !prediction.kept.contains(&member.file));
    // Not `dim`, which small text cannot be read in: its line says why it is left out.
    let color = if left_out {
        palette.secondary
    } else {
        palette.text
    };

    let top = Rect::from_min_size(rect.min, Vec2::new(rect.width(), MEMBER_LINE));
    let mut first = ui.new_child(
        UiBuilder::new()
            .id_salt(("member", member.file))
            .max_rect(top)
            .layout(Layout::left_to_right(Align::Center)),
    );
    first.set_clip_rect(top.intersect(ui.clip_rect()));
    let mut checked = review.checked().contains(&member.file);
    let check = first
        .scope(|ui| {
            theme::show_choices(ui);
            ui.add_enabled(editable, Checkbox::without_text(&mut checked))
        })
        .inner;
    let label = format!("Check {name}");
    check.widget_info(|| WidgetInfo::selected(Role::CheckBox, editable, checked, &label));
    if check.changed() {
        actions.push(Action::Toggle(member.file));
    }
    first.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let remove = ui
            .add_enabled_ui(editable, |ui| {
                let label = format!("Move {name} to unstacked");
                widgets::icon_button(ui, ICON_REMOVE, &label, Emphasis::Normal)
            })
            .inner;
        if remove.clicked() {
            actions.push(Action::Move {
                file: member.file,
                to: MoveTarget::Tray,
            });
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            // The layer name selects its photo, as a click on its thumbnail does: a stack's
            // photos behind its row's `+<k>` are reached so. A text, not a framed button.
            let font = TextStyle::Body.resolve(ui.style());
            let shown = widgets::fitted(ui, &layer, &font, ui.available_width());
            let select = ui
                .add(
                    Button::new(RichText::new(shown).color(color))
                        .frame(false)
                        .truncate(),
                )
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(&name);
            let label = format!("Select {name}");
            select.widget_info(|| {
                let mut info = WidgetInfo::labeled(Role::Button, true, &label);
                info.current_text_value = Some(layer.clone());
                info
            });
            if select.clicked() {
                actions.push(Action::SelectPhoto(member.file));
            }
        });
    });

    let detail = Rect::from_min_max(
        egui::pos2(rect.left() + MEMBER_INDENT, top.bottom()),
        rect.max,
    );
    let mut second = ui.new_child(
        UiBuilder::new()
            .id_salt(("member detail", member.file))
            .max_rect(detail)
            .layout(Layout::left_to_right(Align::Center)),
    );
    second.set_clip_rect(detail.intersect(ui.clip_rect()));
    let warnings = member_warnings(group, member.file, view);
    let text = match probe {
        Some(Ok(probe)) => {
            let mut text = format!(
                "{}, {}",
                resolution(probe.width, probe.height),
                depth(probe.sixteen_bit)
            );
            if left_out {
                text.push_str(" · another resolution");
            }
            text
        }
        Some(Err(reason)) => format!("Cannot be read: {reason}"),
        None if member.waiting => MEASURING.to_owned(),
        None => String::new(),
    };
    // The warnings first, from the far end, then the text in what they leave: laid out
    // after them, from the left, the text would start past the line's end.
    second.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if !warnings.is_empty() {
            let mark = WidgetText::from(
                RichText::new(format!("{} {}", ICON_WARNING.codepoint, warnings.len()))
                    .small()
                    .color(palette.warning),
            );
            let count = ui.add(Label::new(mark).selectable(false));
            let listed = warnings.join("\n");
            let name = format!("Warnings of {name}: {}", warnings.len());
            count.widget_info(|| WidgetInfo::labeled(Role::Label, true, &name));
            count.on_hover_text(listed);
        }
        ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
            // Never dim: for a photo left out it says why the file name counts fewer
            // layers.
            ui.add(
                Label::new(RichText::new(text).small().color(palette.secondary))
                    .truncate()
                    .selectable(false),
            );
        });
    });
}

/// The warnings of `file`, a member of `group` whose view is `view`: its analysis' and
/// its write's.
fn member_warnings(group: &GroupView, file: usize, view: Option<&FileView>) -> Vec<String> {
    let analysis = view.into_iter().flat_map(|view| view.warnings.iter());
    let written = group
        .warnings
        .iter()
        .filter(|(member, _)| *member == file)
        .map(|(_, warning)| warning);
    analysis.chain(written).map(ToString::to_string).collect()
}

// --- A photo ---

/// The selected photo: its preview, its name and folder, its exposure, its warnings, its
/// pair and the moves.
#[allow(clippy::too_many_arguments)]
fn photo_state(
    ui: &mut egui::Ui,
    job: &JobView,
    review: &Review,
    file: &FileView,
    busy: bool,
    thumbs: &mut ThumbCache,
    inspect: &mut Inspect,
    actions: &mut Vec<Action>,
) {
    let palette = theme::palette(ui.ctx());
    let input = job.config.input_dir.as_path();
    let inside = photo::relative_path(&file.path, input);
    preview(ui, job, file, &inside, busy, thumbs, inspect);
    ui.add_space(8.0);
    heading(ui, &folder_name(&file.path), &format!("Photo {inside}"));
    if let Some(folder) = file.path.parent() {
        let shown = display_path(folder);
        value(
            ui,
            &shown,
            &format!("Folder {shown}"),
            Cut::Middle,
            palette.secondary,
        );
    }
    ui.add_space(8.0);
    match &file.exif {
        Some(exif) => facts(ui, &exposure_facts(exif)),
        None => {
            line(ui, "No camera data.", palette.secondary);
        }
    }
    if let Some(error) = &file.error {
        ui.add_space(8.0);
        banner(ui, ICON_ERROR, "Error", palette.danger, &sentence(error));
    }
    if !file.warnings.is_empty() {
        ui.add_space(8.0);
        for warning_text in &file.warnings {
            warning(ui, &capitalised(&warning_text.to_string()));
        }
    }
    let editable = job.status == JobStatus::ReviewPending;
    pair(ui, review, file, editable, actions);
    ui.add_space(SECTION_GAP);
    moves(ui, review, file.idx, editable, actions);
}

/// The photo's larger preview, as wide as the panel and as high as its shape gives (at
/// most as high as it is wide: a portrait photo is letterboxed); the thumbnail, scaled,
/// until it arrives, and with why under it when it cannot be had. While the engine is
/// `busy` no larger preview is asked for (one already kept still shows): it is a full
/// decode outside the engine's memory budget, which would hold a second full-resolution
/// image beside a write's.
fn preview(
    ui: &mut egui::Ui,
    job: &JobView,
    file: &FileView,
    inside: &str,
    busy: bool,
    thumbs: &mut ThumbCache,
    inspect: &mut Inspect,
) {
    let palette = theme::palette(ui.ctx());
    let inspected = if busy {
        inspect.kept_preview(job.id, file)
    } else {
        inspect.preview(job.id, file)
    };
    let (shown, note) = match inspected {
        Inspected::Ready(texture) => (Shown::Image(texture.id(), texture.size_vec2()), None),
        Inspected::Waiting => {
            let note = busy.then(|| PREVIEW_WAITS.to_owned());
            (Shown::of(thumbs, Some(file)), note)
        }
        Inspected::Failed(reason) => (
            Shown::of(thumbs, Some(file)),
            Some(format!("No larger preview: {reason}")),
        ),
    };
    let width = ui.available_width();
    let shape = match shown {
        Shown::Image(_, size) if size.x > 0.0 && size.y > 0.0 => size.y / size.x,
        // A 3:2 photo's, while nothing shows.
        _ => 2.0 / 3.0,
    };
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(width, (width * shape).min(width)), Sense::hover());
    photo::paint(ui.painter(), palette, rect, shown);
    let name = format!("Preview of {inside}");
    response.widget_info(|| WidgetInfo::labeled(Role::Image, true, &name));
    if let Some(note) = note {
        ui.add(
            Label::new(RichText::new(note).small().color(palette.secondary))
                .wrap()
                .selectable(false),
        );
    }
}

/// What the photo's EXIF says of how it was taken, the facts it has.
fn exposure_facts(exif: &photostacker_core::exif::ExifData) -> Vec<Fact> {
    let mut facts = Vec::new();
    if let Some(camera) = camera(exif.make.as_deref(), exif.model.as_deref()) {
        facts.push(Fact::new("Camera", camera));
    }
    if let Some(lens) = exif
        .lens_model
        .as_deref()
        .filter(|lens| !lens.trim().is_empty())
    {
        facts.push(Fact::new("Lens", lens.trim()));
    }
    if let Some(focal) = exif.focal_length_mm.and_then(focal_length) {
        facts.push(Fact::new("Focal length", focal));
    }
    let exposure: Vec<String> = [
        exif.exposure_time_s.and_then(exposure_time),
        exif.f_number.and_then(aperture),
        exif.iso.map(|iso| format!("ISO {iso}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !exposure.is_empty() {
        facts.push(Fact::new("Exposure", exposure.join(" · ")));
    }
    if let Some(bias) = exif.exposure_bias_ev.filter(|bias| bias.is_finite()) {
        facts.push(Fact::new("Exposure bias", format!("{} EV", ev(bias))));
    }
    if let Some(time) = exif.capture_time {
        let taken = match exif.utc_offset_minutes {
            Some(offset) => format!("{time} {}", utc_offset(offset)),
            None => time.to_string(),
        };
        facts.push(Fact::new("Taken", taken));
    }
    if let Some(bracket) = exif.bracket.as_ref().and_then(bracket_text) {
        facts.push(Fact::new("Bracket", bracket));
    }
    facts
}

/// For the active file of a RAW+JPEG pair, each passive partner: that it is not stacked,
/// and `Stack <name> instead`, which swaps the pair.
fn pair(
    ui: &mut egui::Ui,
    review: &Review,
    file: &FileView,
    editable: bool,
    actions: &mut Vec<Action>,
) {
    let palette = theme::palette(ui.ctx());
    for &partner in review.partners(file.idx) {
        let Some(view) = review.files.get(partner) else {
            continue;
        };
        let name = folder_name(&view.path);
        ui.add_space(8.0);
        wrapped(
            ui,
            &format!("Paired with {name}, which is not stacked."),
            palette.text,
        );
        let label = format!("Stack {name} instead");
        let response = ui
            .add_enabled_ui(editable, |ui| {
                ui.add(
                    Button::new(label.as_str())
                        .truncate()
                        .min_size(Vec2::new(0.0, MEMBER_LINE)),
                )
            })
            .inner
            .on_hover_text(format!("Stack {name} in this photo's place."));
        response.widget_info(|| WidgetInfo::labeled(Role::Button, editable, &label));
        if response.clicked() {
            actions.push(Action::Swap(partner));
        }
    }
}

/// `Move to unstacked`, `Start a new stack` and `Move to stack…`: what dragging the photo
/// does, from the keyboard.
fn moves(
    ui: &mut egui::Ui,
    review: &Review,
    file: usize,
    editable: bool,
    actions: &mut Vec<Action>,
) {
    let in_tray = review.unstacked.binary_search(&file).is_ok();
    // A photo of a stack is in the primary one (the selection's rules).
    let alone = review
        .primary()
        .is_some_and(|group| group.members == [file]);
    ui.add_enabled_ui(editable, |ui| {
        buttons_row(ui, |ui| {
            let unstack =
                widgets::text_button_enabled(ui, !in_tray, "Move to unstacked", Emphasis::Normal);
            if unstack.clicked() {
                actions.push(Action::Move {
                    file,
                    to: MoveTarget::Tray,
                });
            }
            let start =
                widgets::text_button_enabled(ui, !alone, "Start a new stack", Emphasis::Normal);
            if start.clicked() {
                actions.push(Action::Move {
                    file,
                    to: MoveTarget::NewGroup,
                });
            }
            let menu = widgets::text_button(ui, "Move to stack…", Emphasis::Normal);
            egui::Popup::menu(&menu).show(|ui| {
                move_menu(ui, review, file, actions);
            });
        });
    });
}

/// The entries of `Move to stack…`: the stacks of the photo's folder nearest to its own,
/// at most [`MOVE_MENU`], each `Stack <n> (<first photo>)`, in their order.
fn move_menu(ui: &mut egui::Ui, review: &Review, file: usize, actions: &mut Vec<Action>) {
    ui.set_max_width(MENU_WIDTH);
    let stacks = nearby_stacks(review, file);
    if stacks.is_empty() {
        let palette = theme::palette(ui.ctx());
        line(ui, "No other stack in this folder.", palette.secondary);
    }
    for group in stacks {
        let first = group
            .members
            .first()
            .and_then(|&member| review.files.get(member))
            .map(|view| folder_name(&view.path))
            .unwrap_or_default();
        let text = format!("Stack {} ({first})", group.position + 1);
        let response = ui.add(Button::new(text.as_str()).truncate());
        let enabled = ui.is_enabled();
        response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, &text));
        if response.clicked() {
            actions.push(Action::Move {
                file,
                to: MoveTarget::Group(group.id),
            });
        }
    }
}

/// The stacks `file` can move to from the menu: of its folder, not its own; the nearest
/// to its own stack, or for a photo of the tray the first, at most [`MOVE_MENU`], in their
/// order. It looks outwards from the photo's stack, so it usually stops after a few.
fn nearby_stacks(review: &Review, file: usize) -> Vec<&GroupView> {
    let folder = |id: usize| review.files.get(id).and_then(|view| view.path.parent());
    let Some(home) = folder(file) else {
        return Vec::new();
    };
    let groups = &review.groups;
    let fits = |group: &GroupView| {
        !group.members.contains(&file)
            && group
                .members
                .first()
                .is_some_and(|&first| folder(first) == Some(home))
    };
    let own = review
        .primary_index()
        .filter(|&at| groups[at].members.contains(&file));
    let mut near: Vec<&GroupView> = match own {
        None => groups
            .iter()
            .filter(|group| fits(group))
            .take(MOVE_MENU)
            .collect(),
        Some(at) => {
            let mut near = Vec::new();
            for distance in 1..groups.len() {
                let before = at.checked_sub(distance);
                let after = Some(at + distance).filter(|&index| index < groups.len());
                if before.is_none() && after.is_none() {
                    break;
                }
                for index in [before, after].into_iter().flatten() {
                    if fits(&groups[index]) && near.len() < MOVE_MENU {
                        near.push(&groups[index]);
                    }
                }
                if near.len() == MOVE_MENU {
                    break;
                }
            }
            near
        }
    };
    near.sort_by_key(|group| group.position);
    near
}
