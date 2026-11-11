//! The Unstacked tray of a review (spec §9.2): the photos in no stack, in a panel along
//! the bottom of the centre, above the review's bar, titled `Unstacked (<n>)`. It shows
//! whenever the review is loaded, also empty, since it is where a photo is dragged to
//! take it out of its stack.
//!
//! Its photos are thumbnails of one height, each in a place of one width, so a row holds
//! as many as the panel's width gives and every row is as high: `ScrollArea::show_rows`
//! lays out only the rows on screen, and a tray of thousands costs a frame only those.
//! A click selects a photo; a drag takes it to a stack, or to the row that starts a new
//! one (`views::photo`). The tray's top edge is dragged to resize it; the height is the
//! preference `panels.tray`, which the app saves once it settles. Until the user resizes
//! it (while the preference has no height), the tray is [`DEFAULT_HEIGHT`] high but takes
//! at most a third of the height it shares with the stacks, so a short window keeps more
//! of them in view.

use eframe::egui::{
    self, CornerRadius, CursorIcon, Id, Margin, Rect, RichText, Role, ScrollArea, Sense, Stroke,
    Vec2, WidgetInfo,
};
use photostacker_core::engine::MoveTarget;

use crate::controller::{Controller, Review};
use crate::theme::{self, SEMIBOLD_FAMILY};
use crate::thumbs::ThumbCache;
use crate::views::photo::{self, Drop, Photo, Shown};
use crate::views::review;

/// The height of a photo in the tray.
const THUMB_HEIGHT: f32 = 72.0;
/// The width of each photo's place: a 3:2 photo's at [`THUMB_HEIGHT`]; a photo of another
/// shape is letterboxed in it.
const CELL_WIDTH: f32 = 108.0;
/// Between two photos of a row.
const GAP: f32 = 6.0;
/// The lowest the tray is drawn, and dragged: its title and one whole row of photos. The
/// highest is half the height it shares with the stacks.
const MIN_HEIGHT: f32 = 128.0;
/// The tray's height until the user resizes it.
const DEFAULT_HEIGHT: f32 = 150.0;
/// Between the stacks and the tray's card: its top edge, which a drag moves, is in it.
const TOP_GAP: i8 = 8;
const PADDING: i8 = 8;
const CORNER: u8 = 6;
/// What an empty tray says.
const EMPTY: &str = "No unstacked photos. Drag a photo here to take it out of its stack.";

/// What the user asked for; done once the tray is drawn, which borrows the review.
enum Action {
    Select(usize),
    /// Into the tray.
    Move(usize),
    Resize(f32),
}

/// Draws the tray of the selected job's review, once the review is loaded, as a panel
/// along the bottom of `ui`: call it before the stacks, which take the rest. Returns how
/// many rows of photos it laid out.
pub fn show(ui: &mut egui::Ui, controller: &mut Controller, thumbs: &mut ThumbCache) -> usize {
    let Some((review, input)) = review::shown(controller) else {
        return 0;
    };
    let palette = theme::palette(ui.ctx());
    // What the tray shares with the stacks: the centre under the header, above the bar.
    let room = ui.available_rect_before_wrap();
    let highest = (room.height() / 2.0).max(MIN_HEIGHT);
    let height = match controller.prefs.panels.tray {
        Some(chosen) => chosen.clamp(MIN_HEIGHT, highest),
        None => DEFAULT_HEIGHT.min(room.height() / 3.0).max(MIN_HEIGHT),
    };
    // Where the panel goes, so a photo held over it is known before it is drawn.
    let card = Rect::from_min_max(
        egui::pos2(room.left(), room.bottom() - height + f32::from(TOP_GAP)),
        room.max,
    );
    // A photo already in the tray stays where it is.
    let target = photo::dragged(ui.ctx(), review.job)
        .filter(|dragged| review.unstacked.binary_search(&dragged.file).is_err())
        .and_then(|dragged| Some((dragged, photo::drop_target(ui, card)?)));
    let fill = match target {
        Some(_) => photo::target_fill(palette, palette.panel),
        None => palette.panel,
    };
    let frame = egui::Frame::new()
        .fill(fill)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(CORNER))
        .inner_margin(Margin::same(PADDING))
        .outer_margin(Margin {
            top: TOP_GAP,
            ..Margin::ZERO
        });
    let mut actions = Vec::new();
    let panel = egui::Panel::bottom("tray")
        .frame(frame)
        .resizable(false)
        .show_separator_line(false)
        .exact_size(height)
        .show(ui, |ui| {
            title(ui, review.unstacked.len());
            photos(ui, review, input, thumbs, &mut actions)
        });
    let outer = panel.response.rect;
    if let Some((dragged, drop)) = target {
        let drawn = Rect::from_min_max(
            egui::pos2(outer.left(), outer.top() + f32::from(TOP_GAP)),
            outer.max,
        );
        ui.painter().rect_stroke(
            drawn,
            CornerRadius::same(CORNER),
            photo::target_stroke(palette),
            egui::StrokeKind::Inside,
        );
        if drop == Drop::Dropped {
            actions.push(Action::Move(dragged.file));
        }
    }
    if let Some(height) = top_edge(ui, outer, highest) {
        actions.push(Action::Resize(height));
    }
    for action in actions {
        match action {
            Action::Select(file) => controller.select_photo(file),
            Action::Move(file) => controller.move_photo(file, MoveTarget::Tray),
            Action::Resize(height) => {
                if controller.prefs.panels.tray != Some(height) {
                    controller.prefs.panels.tray = Some(height);
                    // The panel takes it at the next frame.
                    ui.ctx().request_repaint();
                }
            }
        }
    }
    panel.inner
}

/// `Unstacked (<n>)`.
fn title(ui: &mut egui::Ui, count: usize) {
    let text = RichText::new(format!("Unstacked ({count})"))
        .family(egui::FontFamily::Name(SEMIBOLD_FAMILY.into()));
    ui.add(egui::Label::new(text).selectable(false));
}

/// The tray's photos, as many to a row as fit, laying out only the rows on screen; an
/// empty tray says how to fill it. Returns how many rows it laid out.
fn photos(
    ui: &mut egui::Ui,
    review: &Review,
    input: &std::path::Path,
    thumbs: &mut ThumbCache,
    actions: &mut Vec<Action>,
) -> usize {
    let unstacked = &review.unstacked;
    if unstacked.is_empty() {
        let palette = theme::palette(ui.ctx());
        ui.add(egui::Label::new(RichText::new(EMPTY).color(palette.secondary)).wrap());
        return 0;
    }
    let per_row = ((ui.available_width() + GAP) / (CELL_WIDTH + GAP))
        .floor()
        .max(1.0) as usize;
    let rows = unstacked.len().div_ceil(per_row);
    let mut laid_out = 0;
    ScrollArea::vertical()
        .id_salt(("tray", review.job))
        .auto_shrink(false)
        .show_rows(ui, THUMB_HEIGHT, rows, |ui, shown_rows| {
            laid_out = shown_rows.len();
            for row in shown_rows {
                let (_, rect) = ui.allocate_space(Vec2::new(ui.available_width(), THUMB_HEIGHT));
                if !ui.is_rect_visible(rect) {
                    continue;
                }
                let start = row * per_row;
                let end = (start + per_row).min(unstacked.len());
                for (column, &file) in unstacked[start..end].iter().enumerate() {
                    let left = rect.left() + column as f32 * (CELL_WIDTH + GAP);
                    let cell = Rect::from_min_size(
                        egui::pos2(left, rect.top()),
                        Vec2::new(CELL_WIDTH, THUMB_HEIGHT),
                    );
                    let view = review.files.get(file);
                    let photo = Photo {
                        job: review.job,
                        file,
                        view,
                        input,
                        selected: review.selection.photo == Some(file),
                        draggable: true,
                    };
                    if photo::thumbnail(ui, cell, Shown::of(thumbs, view), &photo) {
                        actions.push(Action::Select(file));
                    }
                }
            }
        });
    laid_out
}

/// The tray's top edge, in the gap above its card, which a drag moves: while it is
/// dragged, the height it gives, between [`MIN_HEIGHT`] and `highest`. `outer` is the
/// tray's whole panel.
fn top_edge(ui: &mut egui::Ui, outer: Rect, highest: f32) -> Option<f32> {
    let radius = ui.style().interaction.resize_grab_radius_side;
    let edge = outer.top() + f32::from(TOP_GAP) / 2.0;
    let handle = Rect::from_x_y_ranges(outer.x_range(), edge - radius..=edge + radius);
    let response = ui.interact(handle, Id::unique("tray height"), Sense::drag());
    response.widget_info(|| WidgetInfo::labeled(Role::Splitter, true, "Tray height"));
    if response.hovered() || response.dragged() {
        ui.set_cursor_icon(CursorIcon::ResizeVertical);
        let widgets = &ui.visuals().widgets;
        let stroke = if response.dragged() {
            widgets.active.fg_stroke
        } else {
            widgets.hovered.fg_stroke
        };
        ui.painter().hline(outer.x_range(), edge, stroke);
    }
    let pointer = response
        .interact_pointer_pos()
        .filter(|_| response.dragged() || response.drag_stopped())?;
    // From where the edge was taken, so the tray does not jump when the drag starts.
    Some((outer.bottom() - pointer.y + f32::from(TOP_GAP) / 2.0).clamp(MIN_HEIGHT, highest))
}
