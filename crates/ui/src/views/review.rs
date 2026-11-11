//! The review of a job in the centre (spec §9.2, §9.3, §5.4.1): its stacks, one row each,
//! then a row that starts a new stack, above the Unstacked tray (`views::tray`) and a bar
//! that keeps the sliders, the bulk actions and Write in sight.
//!
//! The list is virtual: every row is exactly [`ROW_HEIGHT`] high, so
//! `ScrollArea::show_rows` lays out only the rows on screen, and a job of thousands of
//! stacks costs a frame only those. Nothing a frame does walks every stack or every
//! file: a file is found by its id, which is its index in the review's files, and the
//! counts come from the controller, which works them out once per read.
//!
//! A row is one selectable widget. On its first line, the stack's number, its photo
//! count, its status chip, `needs 2 photos` for a stack of one, `manual`, `look again`,
//! the ML badge and its warnings, then Confirm and Reject at the far end; under it the
//! members' thumbnails, top layer first, as many as fit, then `+<k>`. A row never wraps
//! nor grows: what does not fit its line is left out, the least needed first (the photo
//! count, `manual`, the ML badge, `look again`, the warnings, `needs 2 photos`), never
//! the number nor the status, which its name keeps for screen readers too.
//!
//! A photo is dragged (`views::photo`) onto a stack's row to move it there, onto the
//! tray to take it out of its stack, or onto the last row to start a new stack; near the
//! list's top or bottom edge the drag scrolls the list. What the move makes of the
//! stacks comes with the next read of the review: nothing is guessed here.

use std::ops::RangeInclusive;
use std::path::Path;

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Id, Label, Layout, Margin, Rect, RichText, Role,
    Sense, Slider, Stroke, StrokeKind, TextFormat, UiBuilder, Vec2, WidgetInfo,
    style::ScrollAnimation, text::LayoutJob,
};
use egui_material_icons::icons::{ICON_CHECK, ICON_CLOSE, ICON_ERROR, ICON_WARNING};
use photostacker_core::engine::{FileView, GroupId, GroupView, MoveTarget};
use photostacker_core::model::{GroupOrigin, GroupStatus, MlStatus};

use crate::app::FrameStats;
use crate::controller::form::{COSINE_RANGE, HAMMING_RANGE, WINDOW_RANGE};
use crate::controller::{Controller, Review, WriteCounts};
use crate::format::{count, folder_name, group_status_text};
use crate::theme::{self, Palette, SEMIBOLD_FAMILY};
use crate::thumbs::ThumbCache;
use crate::views::fields::WINDOW_HOVER;
use crate::views::photo::{self, DraggedPhoto, Drop, Photo, Shown};
use crate::views::{job, tray};
use crate::widgets::{self, Emphasis};

/// The height of every stack's row: its line over its thumbnails, inside its margins.
pub const ROW_HEIGHT: f32 = 136.0;
/// The corners of a row.
const ROW_CORNER: u8 = 6;
/// Inside a row, left and right, then above and below.
const ROW_MARGIN: Vec2 = Vec2::new(8.0, 4.0);
/// The row's first line: a button's height.
const LINE_HEIGHT: f32 = 28.0;
const THUMB_HEIGHT: f32 = 96.0;
/// The width of a thumbnail not loaded yet, or missing: a 3:2 photo's.
const PLACEHOLDER_WIDTH: f32 = 144.0;
/// The narrowest and widest a thumbnail is drawn; past them, a photo is letterboxed in
/// its frame, never stretched.
const THUMB_WIDTH: RangeInclusive<f32> = 48.0..=288.0;
const THUMB_GAP: f32 = 6.0;
/// Between the facts of a row's line.
const FACT_GAP: f32 = 6.0;
/// The longest rail of the bar's sliders, which they keep while the bar is wide enough.
const SLIDER_WIDTH: f32 = 160.0;
const LOADING: &str = "Loading the review…";
/// What reads a review again once a read of it failed.
pub(crate) const TRY_AGAIN: &str = "Try again";
/// What the review of a folder with no photo says.
const NO_PHOTO: &str = "No photo was found in this folder.";
/// What it adds when the folder's subfolders were not looked at.
const NOT_RECURSIVE: &str =
    "Its subfolders were not looked at: turn on Scan subfolders in a new job.";
/// What `Write <n> stacks` does, on hover: the write ends the review (spec §5.7).
const WRITE_HOVER: &str = "Write the confirmed stacks: the review of this job ends with the write.";
/// Why `Write 0 stacks` is off, on hover.
const NOTHING_TO_WRITE: &str = "Confirm a stack of two or more photos first.";
/// What `look again` means, on hover.
const LOOK_AGAIN: &str = "A decision you made on these photos no longer applies as it was.";
/// What the row after the stacks says.
const NEW_STACK: &str = "Drop a photo here to start a new stack";
/// How near the list's top or bottom edge a dragged photo scrolls the list.
const SCROLL_EDGE: f32 = 24.0;
/// How fast it scrolls, in points a second.
const SCROLL_SPEED: f32 = 720.0;
/// The longest frame a scroll step counts, so a slow frame does not jump the list.
const SCROLL_STEP_TIME: f32 = 0.05;

/// What the user asked for; done once the review is drawn, which borrows the controller.
enum Action {
    Confirm(GroupId),
    Reject(GroupId),
    Select {
        group: GroupId,
        extend: bool,
    },
    SelectPhoto(usize),
    ConfirmAll,
    RejectAll,
    Write,
    Thresholds {
        hamming: u32,
        time_window_seconds: u32,
        cosine: Option<f32>,
    },
    Move {
        file: usize,
        to: MoveTarget,
    },
}

/// Draws the review of the selected job, which is in review: until it is loaded, a quiet
/// line. With `reveal_primary`, the primary stack's row is scrolled into view, by as much
/// as it needs. Returns how many rows of the stacks and of the tray it laid out.
pub fn show(
    ui: &mut egui::Ui,
    controller: &mut Controller,
    thumbs: &mut ThumbCache,
    reveal_primary: bool,
) -> FrameStats {
    if controller.selected().is_none() {
        return FrameStats::default();
    }
    let mut actions = Vec::new();
    // The bar and the tray are panels along the bottom, the bar the lower; the stacks
    // take what they leave.
    match shown(controller) {
        Some((review, _)) => bar(ui, review, controller.write_counts(), &mut actions),
        None => {
            if not_loaded(ui, controller, LOADING) {
                controller.read_review_again();
            }
            return FrameStats::default();
        }
    }
    let tray_rows = tray::show(ui, controller, thumbs);
    let mut stack_rows = 0;
    if let Some((review, input)) = shown(controller) {
        if review.groups.is_empty() {
            let recursive = controller
                .selected()
                .is_some_and(|job| job.config.recursive);
            no_stacks(ui, review, recursive);
        }
        stack_rows = list(ui, review, input, thumbs, reveal_primary, &mut actions);
        if let Some(dragged) = photo::dragged(ui.ctx(), review.job) {
            let view = review.files.get(dragged.file);
            photo::follow_the_pointer(ui.ctx(), Shown::of(thumbs, view));
        }
    }
    for action in actions {
        match action {
            Action::Confirm(group) => controller.confirm(group),
            Action::Reject(group) => controller.reject(group),
            Action::Select { group, extend } => controller.select_group(group, extend),
            Action::SelectPhoto(file) => controller.select_photo(file),
            Action::ConfirmAll => controller.confirm_all(),
            Action::RejectAll => controller.reject_all(),
            Action::Write => controller.write_from_the_review(),
            Action::Thresholds {
                hamming,
                time_window_seconds,
                cosine,
            } => controller.set_thresholds(hamming, time_window_seconds, cosine),
            Action::Move { file, to } => controller.move_photo(file, to),
        }
    }
    FrameStats {
        stack_rows,
        tray_rows,
    }
}

/// What the centre shows until the selected job's review is loaded: `loading`, quietly, or,
/// once a read of it failed, why in a banner, and `Try again`. Returns whether `Try
/// again` was clicked.
pub(crate) fn not_loaded(ui: &mut egui::Ui, controller: &Controller, loading: &str) -> bool {
    let palette = theme::palette(ui.ctx());
    let failure = controller
        .review()
        .filter(|review| {
            controller
                .selected()
                .is_some_and(|job| job.id == review.job)
        })
        .and_then(Review::failure);
    let Some(failure) = failure else {
        ui.label(RichText::new(loading).color(palette.secondary));
        return false;
    };
    job::body(ui, |ui| {
        job::banner(ui, ICON_ERROR, "Error", palette.danger, failure);
        ui.add_space(8.0);
        widgets::text_button(ui, TRY_AGAIN, Emphasis::Normal).clicked()
    })
}

/// The selected job's review, once it is loaded, and the job's folder.
pub(crate) fn shown(controller: &Controller) -> Option<(&Review, &Path)> {
    let job = controller.selected()?;
    let review = controller
        .review()
        .filter(|review| review.job == job.id && review.loaded())?;
    Some((review, job.config.input_dir.as_path()))
}

/// A job whose scan found no stack: how many photos it holds, how many wait in the tray,
/// since the sliders below can still find stacks, and the time window the review shows
/// (the one on its way, while a change is), with a longer one to try below the top of
/// its range; or, for a folder with no photo, that it has none, and, when its subfolders
/// were not looked at, how to have them.
fn no_stacks(ui: &mut egui::Ui, review: &Review, recursive: bool) {
    let palette = theme::palette(ui.ctx());
    if review.files.is_empty() {
        ui.label(RichText::new(NO_PHOTO).color(palette.secondary));
        if !recursive {
            ui.add(Label::new(RichText::new(NOT_RECURSIVE).color(palette.secondary)).wrap());
        }
        return;
    }
    let photos = count(review.files.len(), "photo", "photos");
    ui.label(RichText::new(format!("No stack was found in {photos}.")).color(palette.secondary));
    let unstacked = review.unstacked.len();
    if unstacked > 0 {
        let text = if unstacked == 1 {
            "1 photo is unstacked.".to_owned()
        } else {
            format!("{unstacked} photos are unstacked.")
        };
        ui.label(RichText::new(text).color(palette.secondary));
    }
    let window = review.time_window_seconds;
    let compared = format!("Only photos shot within {window} s of each other are compared");
    let hint = if window < *WINDOW_RANGE.end() {
        format!("{compared}: try a longer time window.")
    } else {
        format!("{compared}.")
    };
    ui.add(Label::new(RichText::new(hint).color(palette.secondary)).wrap());
}

/// The sliders, then the bulk actions and Write, in a bar along the bottom of the
/// centre, so they stay in sight while the list scrolls.
fn bar(ui: &mut egui::Ui, review: &Review, counts: WriteCounts, actions: &mut Vec<Action>) {
    let palette = theme::palette(ui.ctx());
    let frame = egui::Frame::new()
        .fill(palette.window)
        .inner_margin(Margin::symmetric(0, 8));
    egui::Panel::bottom("review bar")
        .frame(frame)
        .show(ui, |ui| {
            sliders(ui, review, actions);
            ui.horizontal_wrapped(|ui| decisions(ui, counts, actions));
        });
}

/// What the bar's sliders show: the thresholds the review regroups with.
struct Thresholds {
    hamming: u32,
    time_window_seconds: u32,
    cosine: Option<f32>,
}

/// The bar's sliders, in the order they are laid out.
#[derive(Debug, Clone, Copy)]
enum BarSlider {
    Hash,
    Window,
    Similarity,
}

const BAR_SLIDERS: [BarSlider; 3] = [BarSlider::Hash, BarSlider::Window, BarSlider::Similarity];

/// `Hash distance`, `Time window (s)` and `Similarity`, in that order, in rows: each one
/// joins the row of the one before it when it fits there, else starts the next row. Their
/// rails are as long as the widest slider alone leaves them, from [`MIN_RAIL`] to
/// [`SLIDER_WIDTH`]. Every change regroups with what all three show: the backend keeps
/// only the newest waiting, so a drag sends no more than the engine runs.
fn sliders(ui: &mut egui::Ui, review: &Review, actions: &mut Vec<Action>) {
    let gap = ui.spacing().item_spacing.x;
    // What each slider's row holds besides its rail; after `Similarity`, its button (or
    // the shorter words saying why it is off).
    let rests = BAR_SLIDERS.map(|slider| match slider {
        BarSlider::Hash => slider_rest(ui, "Hash distance", "40"),
        BarSlider::Window => slider_rest(ui, "Time window (s)", "60"),
        BarSlider::Similarity => {
            slider_rest(ui, "Similarity", "0.00") + gap + button_width(ui, "Model default")
        }
    });
    let available = ui.available_width();
    let widest = rests.into_iter().fold(0.0, f32::max);
    let rail = (available - widest).clamp(MIN_RAIL, SLIDER_WIDTH);
    ui.spacing_mut().slider_width = rail;
    let rows = slider_rows(rests.map(|rest| rest + rail), available);
    let mut thresholds = Thresholds {
        hamming: review.hamming,
        time_window_seconds: review.time_window_seconds,
        cosine: review.cosine,
    };
    let mut changed = false;
    for row in rows {
        ui.horizontal(|ui| {
            for (n, slider) in row.into_iter().enumerate() {
                if n > 0 {
                    // Apart, so one slider's name does not read as the next one's.
                    ui.add_space(SLIDERS_GAP - gap);
                }
                changed |= match slider {
                    BarSlider::Hash => hash_slider(ui, &mut thresholds),
                    BarSlider::Window => window_slider(ui, &mut thresholds),
                    BarSlider::Similarity => similarity_slider(ui, review, &mut thresholds),
                };
            }
        });
    }
    if changed {
        let Thresholds {
            hamming,
            time_window_seconds,
            cosine,
        } = thresholds;
        actions.push(Action::Thresholds {
            hamming,
            time_window_seconds,
            cosine,
        });
    }
}

/// The shortest a slider's rail gets in a narrow bar.
const MIN_RAIL: f32 = 80.0;
/// Between two sliders that share a row.
const SLIDERS_GAP: f32 = 24.0;

/// [`BAR_SLIDERS`] in rows, given the `widths` of their rows alone: each one joins the
/// row of the one before it when it fits beside it, [`SLIDERS_GAP`] apart, in
/// `available`; else it starts the next row.
fn slider_rows(widths: [f32; 3], available: f32) -> Vec<Vec<BarSlider>> {
    let mut rows: Vec<Vec<BarSlider>> = Vec::new();
    let mut used = 0.0;
    for (slider, width) in BAR_SLIDERS.into_iter().zip(widths) {
        match rows.last_mut() {
            Some(row) if used + SLIDERS_GAP + width <= available => {
                row.push(slider);
                used += SLIDERS_GAP + width;
            }
            _ => {
                rows.push(vec![slider]);
                used = width;
            }
        }
    }
    rows
}

/// How wide a slider's row is besides its rail: its value's field, as wide as `widest`
/// value needs, and its name, with the gaps between them.
fn slider_rest(ui: &egui::Ui, name: &str, widest: &str) -> f32 {
    let spacing = ui.spacing();
    let value = button_width(ui, widest).max(spacing.interact_size.x);
    value + 2.0 * spacing.item_spacing.x + text_width(ui, name)
}

/// How wide a text button of `text` is.
fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    text_width(ui, text) + 2.0 * ui.spacing().button_padding.x
}

/// `Hash distance`; returns whether it moved.
fn hash_slider(ui: &mut egui::Ui, thresholds: &mut Thresholds) -> bool {
    ui.add(Slider::new(&mut thresholds.hamming, HAMMING_RANGE).text("Hash distance"))
        .on_hover_text("How different two photos may look and still be one stack.")
        .changed()
}

/// `Time window (s)`, over the new-job form's range; returns whether it moved.
fn window_slider(ui: &mut egui::Ui, thresholds: &mut Thresholds) -> bool {
    ui.add(Slider::new(&mut thresholds.time_window_seconds, WINDOW_RANGE).text("Time window (s)"))
        .on_hover_text(WINDOW_HOVER)
        .changed()
}

/// `Similarity`: off without a model; with one, it shows the model's own similarity
/// until it is moved, and `Model default` brings that back. Returns whether it changed.
fn similarity_slider(ui: &mut egui::Ui, review: &Review, thresholds: &mut Thresholds) -> bool {
    let palette = theme::palette(ui.ctx());
    let range = COSINE_RANGE;
    let Some(default) = review.default_cosine else {
        let mut off = *range.start();
        ui.add_enabled(
            false,
            Slider::new(&mut off, range)
                .text("Similarity")
                .show_value(false),
        );
        ui.label(RichText::new("no model").color(palette.secondary));
        return false;
    };
    let mut shown = thresholds
        .cosine
        .unwrap_or(default)
        .clamp(*range.start(), *range.end());
    let mut changed = false;
    if ui
        .add(
            Slider::new(&mut shown, range)
                .text("Similarity")
                .step_by(0.01)
                .fixed_decimals(2),
        )
        .on_hover_text("How alike the model must find the photos of a stack.")
        .changed()
    {
        thresholds.cosine = Some(shown);
        changed = true;
    }
    let reset = ui
        .add_enabled_ui(review.cosine.is_some(), |ui| {
            widgets::text_button(ui, "Model default", Emphasis::Normal)
                .on_hover_text("Use the model's own similarity.")
        })
        .inner;
    if reset.clicked() {
        thresholds.cosine = None;
        changed = true;
    }
    changed
}

/// `Confirm all` and `Reject all`, which decide the Proposed stacks; `Write <n> stacks`,
/// the review's one main action, and how many stacks it leaves undecided.
fn decisions(ui: &mut egui::Ui, counts: WriteCounts, actions: &mut Vec<Action>) {
    let palette = theme::palette(ui.ctx());
    ui.add_enabled_ui(counts.proposed > 0, |ui| {
        if widgets::text_button(ui, "Confirm all", Emphasis::Normal).clicked() {
            actions.push(Action::ConfirmAll);
        }
        if widgets::text_button(ui, "Reject all", Emphasis::Normal).clicked() {
            actions.push(Action::RejectAll);
        }
    });
    let write = format!("Write {}", count(counts.to_write, "stack", "stacks"));
    ui.add_enabled_ui(counts.to_write > 0, |ui| {
        if widgets::text_button(ui, &write, Emphasis::Primary)
            .on_hover_text(WRITE_HOVER)
            .on_disabled_hover_text(NOTHING_TO_WRITE)
            .clicked()
        {
            actions.push(Action::Write);
        }
    });
    if counts.proposed > 0 {
        // Whole, on the next line when this one is full.
        let waiting = format!("{} still proposed", counts.proposed);
        ui.add(Label::new(RichText::new(waiting).color(palette.secondary)).extend());
    }
}

/// The stacks, one row each, then the row that starts a new stack, laying out only the
/// rows on screen; with `reveal_primary`, the primary stack's row is scrolled into view.
/// Returns how many rows it laid out.
fn list(
    ui: &mut egui::Ui,
    review: &Review,
    input: &Path,
    thumbs: &mut ThumbCache,
    reveal_primary: bool,
    actions: &mut Vec<Action>,
) -> usize {
    // Looked for only when a step asks: a frame never walks every stack.
    let primary_at = reveal_primary
        .then(|| review.selection.primary())
        .flatten()
        .and_then(|primary| review.groups.iter().position(|group| group.id == primary));
    let dragged = photo::dragged(ui.ctx(), review.job);
    let rows = review.groups.len() + 1;
    let mut laid_out = 0;
    egui::ScrollArea::vertical()
        .id_salt(("stacks", review.job))
        .auto_shrink(false)
        .show_rows(ui, ROW_HEIGHT, rows, |ui, shown_rows| {
            if let Some(at) = primary_at {
                reveal(ui, shown_rows.start, at);
            }
            if dragged.is_some() {
                scroll_at_the_edges(ui, shown_rows.start, rows);
            }
            laid_out = shown_rows.len();
            for index in shown_rows {
                if index < review.groups.len() {
                    row(ui, review, index, input, thumbs, dragged, actions);
                } else {
                    new_stack_row(ui, dragged, actions);
                }
            }
        });
    laid_out
}

/// The row after the stacks: a photo dropped on it starts a stack of its own. It is there
/// also while nothing is dragged, so the way to start a stack shows.
fn new_stack_row(ui: &mut egui::Ui, dragged: Option<DraggedPhoto>, actions: &mut Vec<Action>) {
    let palette = theme::palette(ui.ctx());
    let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), ROW_HEIGHT));
    let target = dragged.and_then(|dragged| Some((dragged, photo::drop_target(ui, rect)?)));
    let painter = ui.painter();
    if target.is_some() {
        let fill = photo::target_fill(palette, palette.window);
        painter.rect_filled(rect, CornerRadius::same(ROW_CORNER), fill);
    }
    let stroke = match target {
        Some(_) => photo::target_stroke(palette),
        None => Stroke::new(1.0, palette.outline),
    };
    painter.rect_stroke(
        rect,
        CornerRadius::same(ROW_CORNER),
        stroke,
        StrokeKind::Inside,
    );
    let text = RichText::new(NEW_STACK).color(palette.secondary);
    ui.put(rect, Label::new(text).selectable(false));
    if let Some((dragged, Drop::Dropped)) = target {
        actions.push(Action::Move {
            file: dragged.file,
            to: MoveTarget::NewGroup,
        });
    }
}

/// While a photo is dragged within [`SCROLL_EDGE`] of the list's top or bottom edge,
/// scrolls the list that way, so the photo can reach a stack out of view; it asks for the
/// next frame only while there is more to scroll. `ui` is the part of the list laid out,
/// whose first row is `first`, of `rows`.
fn scroll_at_the_edges(ui: &egui::Ui, first: usize, rows: usize) {
    let Some(pointer) = ui.ctx().pointer_latest_pos() else {
        return;
    };
    let view = ui.clip_rect();
    if !view.contains(pointer) {
        return;
    }
    let spacing = ui.spacing().item_spacing.y;
    let pitch = ROW_HEIGHT + spacing;
    let top = ui.max_rect().top() - first as f32 * pitch;
    let bottom = top + rows as f32 * pitch - spacing;
    // Half a point of slack, so rounding never keeps the list asking for frames.
    let above = top < view.top() - 0.5;
    let below = bottom > view.bottom() + 0.5;
    // egui's delta moves the content: down (positive) scrolls the list up.
    let direction = if pointer.y < view.top() + SCROLL_EDGE && above {
        1.0
    } else if pointer.y > view.bottom() - SCROLL_EDGE && below {
        -1.0
    } else {
        return;
    };
    let step = SCROLL_SPEED * ui.input(|input| input.stable_dt).min(SCROLL_STEP_TIME);
    ui.scroll_with_delta_animation(Vec2::new(0.0, direction * step), ScrollAnimation::none());
    ui.ctx().request_repaint();
}

/// Scrolls the list by as much as row `at` needs to be in view, at once. `ui` is the
/// part of the list laid out, whose first row is `first`.
fn reveal(ui: &egui::Ui, first: usize, at: usize) {
    let pitch = ROW_HEIGHT + ui.spacing().item_spacing.y;
    let top = ui.max_rect().top() + (at as f32 - first as f32) * pitch;
    let rect = Rect::from_min_size(
        egui::pos2(ui.max_rect().left(), top),
        Vec2::new(ui.max_rect().width(), ROW_HEIGHT),
    );
    ui.scroll_to_rect_animation(rect, None, ScrollAnimation::none());
}

/// Stack `index`'s row: one selectable widget named `Stack <n>: <first photo>, <count>,
/// <status>`, whose buttons and thumbnails are on top of it, so a click on one is
/// theirs alone. The whole row is where a photo of another stack, or of the tray, is
/// dropped to move it into this stack.
fn row(
    ui: &mut egui::Ui,
    review: &Review,
    index: usize,
    input: &Path,
    thumbs: &mut ThumbCache,
    dragged: Option<DraggedPhoto>,
    actions: &mut Vec<Action>,
) {
    let group = &review.groups[index];
    let palette = theme::palette(ui.ctx());
    let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), ROW_HEIGHT));
    let id = row_id(review, group);
    let response = ui.interact(rect, id, Sense::click());
    let selection = &review.selection;
    let selected = selection.groups.contains(&group.id);
    let primary = selection.primary() == Some(group.id);
    let number = group.position + 1;
    let label = row_label(review, group);
    response.widget_info(|| WidgetInfo::selected(Role::Button, true, selected, &label));
    if response.clicked() {
        actions.push(Action::Select {
            group: group.id,
            extend: extends_selection(ui),
        });
    }
    // A photo of this stack stays where it is: the row is no target for it.
    let target = dragged
        .filter(|dragged| !group.members.contains(&dragged.file))
        .and_then(|dragged| Some((dragged, photo::drop_target(ui, rect)?)));
    let look = if target.is_some() {
        Look::Target
    } else if primary {
        Look::Primary
    } else if selected {
        Look::Selected
    } else if response.contains_pointer() {
        Look::Pointed
    } else {
        Look::Plain
    };
    paint_row(ui, rect, palette, look);

    let inner = rect.shrink2(ROW_MARGIN);
    let line = Rect::from_min_size(inner.min, Vec2::new(inner.width(), LINE_HEIGHT));
    let buttons = decision_buttons(ui, id, line, group, number, actions);
    let facts = Rect::from_min_max(
        line.min,
        egui::pos2(buttons.left() - ui.spacing().item_spacing.x, line.max.y),
    );
    row_facts(ui, id, facts, review, index, number);
    let strip = Rect::from_min_max(
        egui::pos2(inner.left(), inner.bottom() - THUMB_HEIGHT),
        inner.max,
    );
    thumbnails(ui, strip, review, group, input, thumbs, actions);
    if let Some((dragged, Drop::Dropped)) = target {
        actions.push(Action::Move {
            file: dragged.file,
            to: MoveTarget::Group(group.id),
        });
    }
}

/// The id of `group`'s row, from which its widgets' ids derive: the same wherever the
/// list is scrolled, and while the stack keeps its id across regroupings.
fn row_id(review: &Review, group: &GroupView) -> Id {
    Id::unique(("review stack", review.job, group.id))
}

/// `Stack <n>: <first photo>, <count>, <status>`: the row's name for screen readers and
/// tests.
fn row_label(review: &Review, group: &GroupView) -> String {
    let first = group
        .members
        .first()
        .and_then(|&file| review.files.get(file))
        .map(|file| folder_name(&file.path))
        .unwrap_or_default();
    format!(
        "Stack {}: {first}, {}, {}",
        group.position + 1,
        count(group.members.len(), "photo", "photos"),
        group_status_text(group.status)
    )
}

/// Whether a click adds to the selection (or takes out of it): Ctrl, Cmd on macOS. The
/// click's own modifiers count too, since a press and its keys can come in one frame.
fn extends_selection(ui: &egui::Ui) -> bool {
    ui.input(|input| {
        input.modifiers.command
            || input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::PointerButton {
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.command
                )
            })
    })
}

/// How a row looks, by what it is to the user, the strongest first. The results' rows
/// look the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Look {
    /// A photo is held over it, and a drop would move the photo into it.
    Target,
    /// The primary stack of the selection.
    Primary,
    /// Another selected stack.
    Selected,
    /// Under the pointer.
    Pointed,
    Plain,
}

/// The row's background: a card on the panel's colour, with the surface's under the
/// pointer. A selected row takes a faint tint of the accent and an accent outline, 2 px
/// for the primary stack and 1 px for the others, so the chips' outlines and the
/// buttons still stand out on it. A drop target takes a stronger tint and a 2 px
/// outline, so it shows on a selected row too.
pub(crate) fn paint_row(ui: &egui::Ui, rect: Rect, palette: &Palette, look: Look) {
    let selected = theme::mix(palette.panel, palette.accent, 0.08);
    let (fill, stroke) = match look {
        Look::Target => (
            photo::target_fill(palette, palette.panel),
            photo::target_stroke(palette),
        ),
        Look::Primary => (selected, Stroke::new(2.0, palette.accent)),
        Look::Selected => (selected, Stroke::new(1.0, palette.accent)),
        Look::Pointed => (palette.surface, Stroke::new(1.0, palette.outline)),
        Look::Plain => (palette.panel, Stroke::new(1.0, palette.outline)),
    };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(ROW_CORNER), fill);
    painter.rect_stroke(
        rect,
        CornerRadius::same(ROW_CORNER),
        stroke,
        StrokeKind::Inside,
    );
}

/// `Confirm stack <n>` and `Reject stack <n>` at the end of `line`; the one that matches
/// the stack's decision is filled, so the row's state shows without reading its chip.
/// Returns the rectangle they take.
fn decision_buttons(
    ui: &mut egui::Ui,
    id: Id,
    line: Rect,
    group: &GroupView,
    number: usize,
    actions: &mut Vec<Action>,
) -> Rect {
    let mut buttons = ui.new_child(
        UiBuilder::new()
            .scope_id(id.with("decide"))
            .max_rect(line)
            .layout(Layout::right_to_left(Align::Center)),
    );
    let (confirm, reject) = match group.status {
        // Green as the Confirmed chip's dot: amber marks the selection and the main
        // actions.
        GroupStatus::Confirmed => (Emphasis::Positive, Emphasis::Normal),
        GroupStatus::Rejected => (Emphasis::Normal, Emphasis::Danger),
        _ => (Emphasis::Normal, Emphasis::Normal),
    };
    // Right to left: Reject ends the line.
    let label = format!("Reject stack {number}");
    if widgets::icon_button(&mut buttons, ICON_CLOSE, &label, reject).clicked() {
        actions.push(Action::Reject(group.id));
    }
    let label = format!("Confirm stack {number}");
    if widgets::icon_button(&mut buttons, ICON_CHECK, &label, confirm).clicked() {
        actions.push(Action::Confirm(group.id));
    }
    buttons.min_rect()
}

/// What a row's line says of its stack, in this order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fact {
    Number,
    Photos,
    Status,
    /// A stack of one photo, which is not written.
    TooFew,
    Manual,
    LookAgain,
    Ml,
    Warnings,
}

/// The facts a line too short for all of them leaves out, the least needed first. The
/// number and the status stay.
const LEFT_OUT_FIRST: [Fact; 6] = [
    Fact::Photos,
    Fact::Manual,
    Fact::Ml,
    Fact::LookAgain,
    Fact::Warnings,
    Fact::TooFew,
];

/// What a stack of one photo says.
const TOO_FEW: &str = "needs 2 photos";

/// The facts of stack `index` that fit `rect`, on one line, clipped to it.
fn row_facts(ui: &mut egui::Ui, id: Id, rect: Rect, review: &Review, index: usize, number: usize) {
    let group = &review.groups[index];
    let warnings = review.warning_counts.get(index).copied().unwrap_or(0);
    let mut facts = ui.new_child(
        UiBuilder::new()
            .scope_id(id.with("facts"))
            .max_rect(rect)
            .layout(Layout::left_to_right(Align::Center)),
    );
    facts.set_clip_rect(rect.intersect(ui.clip_rect()));
    facts.spacing_mut().item_spacing.x = FACT_GAP;
    // The texts only show: a click on them is the row's.
    facts.style_mut().interaction.selectable_labels = false;

    let present = [
        Fact::Number,
        Fact::Photos,
        Fact::Status,
        Fact::TooFew,
        Fact::Manual,
        Fact::LookAgain,
        Fact::Ml,
        Fact::Warnings,
    ]
    .into_iter()
    .filter(|&fact| match fact {
        Fact::TooFew => group.members.len() < 2,
        Fact::Manual => group.origin == GroupOrigin::Manual,
        Fact::LookAgain => group.review_reset,
        Fact::Ml => ml_text(group).is_some(),
        Fact::Warnings => warnings > 0,
        Fact::Number | Fact::Photos | Fact::Status => true,
    });
    let mut shown: Vec<(Fact, f32)> = present
        .map(|fact| (fact, fact_width(&facts, fact, group, number, warnings)))
        .collect();
    let width = |shown: &[(Fact, f32)]| {
        let gaps = shown.len().saturating_sub(1) as f32 * FACT_GAP;
        shown.iter().map(|(_, width)| width).sum::<f32>() + gaps
    };
    for left_out in LEFT_OUT_FIRST {
        if width(&shown) <= rect.width() {
            break;
        }
        shown.retain(|(fact, _)| *fact != left_out);
    }
    for (fact, _) in shown {
        draw_fact(&mut facts, fact, review, group, number, warnings);
    }
}

/// How wide `fact` is drawn.
fn fact_width(ui: &egui::Ui, fact: Fact, group: &GroupView, number: usize, warnings: usize) -> f32 {
    match fact {
        Fact::Number => text_width(ui, number_text(number)),
        Fact::Photos => text_width(ui, photos_text(ui, group)),
        Fact::Status => widgets::chip_width(ui, group_status_text(group.status)),
        Fact::TooFew => widgets::chip_width(ui, TOO_FEW),
        Fact::Manual => widgets::chip_width(ui, "manual"),
        Fact::LookAgain => widgets::chip_width(ui, "look again"),
        Fact::Ml => {
            let chip = ml_text(group).map_or(0.0, |text| widgets::chip_width(ui, &text));
            let spinner = if group.ml_status == MlStatus::Pending {
                SPINNER_SIZE + FACT_GAP
            } else {
                0.0
            };
            chip + spinner
        }
        Fact::Warnings => text_width(ui, warnings_text(ui, warnings)),
    }
}

const SPINNER_SIZE: f32 = 12.0;

/// How wide `text` lays out on one line.
pub(crate) fn text_width(ui: &egui::Ui, text: impl Into<egui::WidgetText>) -> f32 {
    text.into()
        .into_galley(
            ui,
            Some(egui::TextWrapMode::Extend),
            f32::INFINITY,
            egui::TextStyle::Body,
        )
        .size()
        .x
}

fn draw_fact(
    ui: &mut egui::Ui,
    fact: Fact,
    review: &Review,
    group: &GroupView,
    number: usize,
    warnings: usize,
) {
    let palette = theme::palette(ui.ctx());
    match fact {
        Fact::Number => {
            ui.label(number_text(number));
        }
        Fact::Photos => {
            ui.label(photos_text(ui, group));
        }
        Fact::Status => {
            let status = group.status;
            widgets::chip(ui, group_status_text(status), status_color(palette, status));
        }
        Fact::TooFew => {
            widgets::chip(ui, TOO_FEW, palette.warning)
                .on_hover_text("A stack of one photo is not written: add a photo to it.");
        }
        Fact::Manual => {
            widgets::chip(ui, "manual", palette.accent)
                .on_hover_text("You made or changed this stack: moving a slider leaves it be.");
        }
        Fact::LookAgain => {
            widgets::chip(ui, "look again", palette.warning).on_hover_text(LOOK_AGAIN);
        }
        Fact::Ml => ml_badge(ui, group),
        Fact::Warnings => {
            let enabled = ui.is_enabled();
            let name = format!("Warnings of stack {number}: {warnings}");
            let response = ui.add(Label::new(warnings_text(ui, warnings)));
            response.widget_info(|| WidgetInfo::labeled(Role::Label, enabled, &name));
            // Built only while it shows.
            response.on_hover_ui(|ui| {
                ui.label(warning_lines(review, group));
            });
        }
    }
}

fn number_text(number: usize) -> RichText {
    RichText::new(format!("Stack {number}")).family(egui::FontFamily::Name(SEMIBOLD_FAMILY.into()))
}

fn photos_text(ui: &egui::Ui, group: &GroupView) -> RichText {
    let palette = theme::palette(ui.ctx());
    RichText::new(count(group.members.len(), "photo", "photos")).color(palette.secondary)
}

/// The warning icon in the warning colour, then how many, in the text colour.
pub(crate) fn warnings_text(ui: &egui::Ui, warnings: usize) -> LayoutJob {
    let palette = theme::palette(ui.ctx());
    let body = egui::TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    job.append(
        ICON_WARNING.codepoint,
        0.0,
        TextFormat::simple(
            FontId::new(body.size + 2.0, ICON_WARNING.font_family()),
            palette.warning,
        ),
    );
    job.append(
        &warnings.to_string(),
        3.0,
        TextFormat::simple(body, palette.text),
    );
    job
}

/// Each warning of `group`, one per line: `<file name>: <warning>`, its members' from
/// the analysis, then its write's.
fn warning_lines(review: &Review, group: &GroupView) -> String {
    let name = |file: usize| {
        review
            .files
            .get(file)
            .map(|view: &FileView| folder_name(&view.path))
            .unwrap_or_default()
    };
    let analysis = group.members.iter().flat_map(|&member| {
        review
            .files
            .get(member)
            .into_iter()
            .flat_map(move |file| file.warnings.iter().map(move |warning| (member, warning)))
    });
    let written = group
        .warnings
        .iter()
        .map(|(file, warning)| (*file, warning));
    analysis
        .chain(written)
        .map(|(file, warning)| format!("{}: {warning}", name(file)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The ML badge's text: none when the visual check had nothing to say.
pub(crate) fn ml_text(group: &GroupView) -> Option<String> {
    let score = group.ml_score.map(|score| format!(" {score:.2}"));
    match group.ml_status {
        MlStatus::None => None,
        MlStatus::Confirmed => Some(format!("ML{}", score.unwrap_or_default())),
        MlStatus::Split => Some(format!("ML split{}", score.unwrap_or_default())),
        MlStatus::Skipped => Some("ML skipped".to_owned()),
        MlStatus::Pending => Some("ML pending".to_owned()),
    }
}

/// What the visual check made of the stack: its score in the positive colour when it
/// kept the stack whole, in the warning colour when it split it off a larger one; dim
/// when it was skipped, with a spinner while it is pending.
pub(crate) fn ml_badge(ui: &mut egui::Ui, group: &GroupView) {
    let palette = theme::palette(ui.ctx());
    let Some(text) = ml_text(group) else {
        return;
    };
    let color = match group.ml_status {
        MlStatus::Confirmed => palette.positive,
        MlStatus::Split => palette.warning,
        MlStatus::Pending => {
            ui.add(
                egui::Spinner::new()
                    .size(SPINNER_SIZE)
                    .color(palette.accent),
            );
            palette.dim
        }
        MlStatus::Skipped | MlStatus::None => palette.dim,
    };
    widgets::chip(ui, &text, color);
}

/// The colour of a stack's status dot: dim while it waits for a decision, positive once
/// confirmed or written, danger when rejected or failed, warning when its write skipped
/// it. The results' rows use it too.
pub(crate) fn status_color(palette: &Palette, status: GroupStatus) -> Color32 {
    match status {
        GroupStatus::Proposed => palette.dim,
        GroupStatus::Confirmed | GroupStatus::Written => palette.positive,
        GroupStatus::Rejected | GroupStatus::Failed => palette.danger,
        GroupStatus::Skipped => palette.warning,
    }
}

/// The members' thumbnails along `strip`, top layer first, as many as fit, then `+<k>`
/// for the rest. A click selects the photo and its stack alone; with Ctrl (Cmd on macOS)
/// it adds the stack to the selection, or takes it out. Nothing is asked of the cache
/// unless the strip is on screen.
fn thumbnails(
    ui: &mut egui::Ui,
    strip: Rect,
    review: &Review,
    group: &GroupView,
    input: &Path,
    thumbs: &mut ThumbCache,
    actions: &mut Vec<Action>,
) {
    if !ui.is_rect_visible(strip) {
        return;
    }
    let id = row_id(review, group);
    let mut x = strip.left();
    for (at, &file) in group.members.iter().enumerate() {
        let view = review.files.get(file);
        let shown = Shown::of(thumbs, view);
        let width = match shown {
            Shown::Image(_, size) if size.y > 0.0 => {
                (THUMB_HEIGHT * size.x / size.y).clamp(*THUMB_WIDTH.start(), *THUMB_WIDTH.end())
            }
            _ => PLACEHOLDER_WIDTH,
        };
        let after = group.members.len() - at - 1;
        let room_for_more = if after > 0 {
            THUMB_GAP + more_width(ui, after)
        } else {
            0.0
        };
        if x + width + room_for_more > strip.right() {
            let hidden = &group.members[at..];
            let selected = review
                .selection
                .photo
                .is_some_and(|photo| hidden.contains(&photo));
            more(ui, id, strip, x, hidden.len(), selected);
            return;
        }
        let rect = Rect::from_min_size(egui::pos2(x, strip.top()), Vec2::new(width, THUMB_HEIGHT));
        let photo = Photo {
            job: review.job,
            file,
            view,
            input,
            selected: review.selection.photo == Some(file),
            draggable: true,
        };
        if photo::thumbnail(ui, rect, shown, &photo) {
            // With the multi-select modifier, as a click on the row's title: thumbnails
            // cover most of a row.
            actions.push(if extends_selection(ui) {
                Action::Select {
                    group: group.id,
                    extend: true,
                }
            } else {
                Action::SelectPhoto(file)
            });
        }
        x += width + THUMB_GAP;
    }
}

/// The name of the `+<k>` that stands for `more` photos past the strip: `3 more photos`,
/// `1 more photo`.
pub(crate) fn more_name(more: usize) -> String {
    if more == 1 {
        "1 more photo".to_owned()
    } else {
        format!("{more} more photos")
    }
}

/// The text of the `+<k>` that stands for the photos past the strip.
fn more_text(more: usize) -> RichText {
    RichText::new(format!("+{more}")).small()
}

/// The padding inside the `+<k>` frame.
const MORE_PADDING: Vec2 = Vec2::new(7.0, 1.0);

fn more_width(ui: &egui::Ui, more: usize) -> f32 {
    let galley = egui::WidgetText::from(more_text(more)).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Small,
    );
    galley.size().x + 2.0 * MORE_PADDING.x
}

/// `+<more>` at `x` along `strip`, in the middle of its height, in a rounded outline
/// like a chip's: the photos the strip has no room for, named `<more> more photos`. While
/// one of them is the selected photo (reached by the arrows, or from the inspector), it
/// is `selected`, with the outline a selected thumbnail has.
fn more(ui: &mut egui::Ui, id: Id, strip: Rect, x: f32, more: usize, selected: bool) {
    let palette = theme::palette(ui.ctx());
    let galley = egui::WidgetText::from(more_text(more)).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Small,
    );
    let size = galley.size() + 2.0 * MORE_PADDING;
    let rect = Rect::from_min_size(egui::pos2(x, strip.center().y - size.y / 2.0), size);
    let response = ui.interact(rect, id.with("more"), Sense::hover());
    let name = more_name(more);
    response.widget_info(|| WidgetInfo::selected(Role::Label, true, selected, &name));
    let painter = ui.painter();
    painter.rect_stroke(
        rect,
        rect.height() / 2.0,
        Stroke::new(1.0, palette.outline),
        StrokeKind::Inside,
    );
    if selected {
        painter.rect_stroke(
            rect,
            rect.height() / 2.0,
            Stroke::new(2.0, palette.accent),
            StrokeKind::Outside,
        );
    }
    painter.galley(rect.min + MORE_PADDING, galley, palette.text);
}
