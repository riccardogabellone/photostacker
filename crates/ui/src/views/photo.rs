//! A photo of the review as the stacks and the tray draw it (spec §9.2): a thumbnail that
//! a click selects and a drag carries to another stack, to the tray or to a new stack;
//! and what the drop targets share: the payload a drag carries, the look of a target
//! while a photo is over it, and the copy of the photo that follows the pointer.
//!
//! egui tells a click from a drag: a thumbnail senses both, and a press becomes a drag
//! only once the pointer has moved past egui's click distance (or the press is held), so
//! a plain click still selects and a drag never does. The drag carries a [`DraggedPhoto`]
//! as egui's drag-and-drop payload, which egui drops when the button is released or Esc
//! is pressed. A target finds the pointer by its rectangle, not by egui's hover, which
//! goes to the dragged thumbnail alone.

use std::path::Path;

use eframe::egui::{
    self, Align2, Color32, CornerRadius, DragAndDrop, FontId, Id, LayerId, Order, Painter, Rect,
    Role, Sense, Stroke, StrokeKind, TextureId, Vec2, WidgetInfo, epaint::RectShape,
};
use egui_material_icons::icons::ICON_BROKEN_IMAGE;
use photostacker_core::engine::{FileView, JobId};

use crate::format::{display_path, folder_name};
use crate::theme::{self, Palette};
use crate::thumbs::{Thumb, ThumbCache};

/// What a dragged photo carries: egui's drag-and-drop payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DraggedPhoto {
    /// The job whose review the photo is dragged from.
    pub job: JobId,
    /// The photo, by its id in the job's analysis.
    pub file: usize,
}

/// The corners of a thumbnail.
const CORNER: u8 = 4;
/// The outline a drop target shows while a photo is over it.
const DROP_OUTLINE: f32 = 2.0;
/// How much of the accent a drop target's fill takes while a photo is over it; a
/// selected row takes 8 %.
const TARGET_TINT: f32 = 0.22;
/// The height of the copy of a dragged photo that follows the pointer.
const DRAGGED_HEIGHT: f32 = 64.0;
/// The narrowest and widest that copy is drawn.
const DRAGGED_WIDTH: std::ops::RangeInclusive<f32> = 32.0..=192.0;
/// Where the copy is drawn from the pointer: below and to its right, so it hides neither
/// the pointer nor what the pointer is on.
const DRAGGED_OFFSET: Vec2 = Vec2::new(12.0, 12.0);

/// What a thumbnail shows, once looked up.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Shown {
    Image(TextureId, Vec2),
    Loading,
    Missing,
}

impl Shown {
    /// `file`'s thumbnail as the cache has it; a thumbnail not loaded yet is asked for, so
    /// call it only for one on screen.
    pub(crate) fn of(thumbs: &mut ThumbCache, file: Option<&FileView>) -> Shown {
        match thumbs.get(file.and_then(|file| file.thumb.as_deref())) {
            Thumb::Ready(texture) => Shown::Image(texture.id(), texture.size_vec2()),
            Thumb::Loading => Shown::Loading,
            Thumb::Missing => Shown::Missing,
        }
    }
}

/// A photo where the review shows it.
pub(crate) struct Photo<'a> {
    pub job: JobId,
    /// The photo's id in the job's analysis.
    pub file: usize,
    /// Its view, once the review's files are read.
    pub view: Option<&'a FileView>,
    /// The job's folder, inside which the photo is named.
    pub input: &'a Path,
    pub selected: bool,
    /// Whether it can be dragged: in a review, to another stack or the tray.
    pub draggable: bool,
}

/// `photo`'s thumbnail in `rect`, named by its path inside the job's folder (its name on
/// hover): the photo letterboxed in `rect` when their shapes differ, a frame while it
/// loads, a broken image when it has none. The selected photo has an accent outline. A
/// drag of a draggable one carries a [`DraggedPhoto`]. Returns whether it was clicked.
pub(crate) fn thumbnail(ui: &mut egui::Ui, rect: Rect, shown: Shown, photo: &Photo) -> bool {
    let palette = theme::palette(ui.ctx());
    let id = Id::unique(("review photo", photo.job, photo.file));
    let sense = if photo.draggable {
        Sense::click_and_drag()
    } else {
        Sense::click()
    };
    let response = ui.interact(rect, id, sense);
    if photo.draggable {
        response.dnd_set_drag_payload(DraggedPhoto {
            job: photo.job,
            file: photo.file,
        });
    }
    let name = photo
        .view
        .map(|view| relative_path(&view.path, photo.input))
        .unwrap_or_default();
    let selected = photo.selected;
    response.widget_info(|| WidgetInfo::selected(Role::Image, true, selected, &name));
    let painter = ui.painter();
    paint(painter, palette, rect, shown);
    if selected {
        painter.rect_stroke(
            rect,
            CornerRadius::same(CORNER),
            Stroke::new(2.0, palette.accent),
            StrokeKind::Outside,
        );
    }
    let clicked = response.clicked();
    // egui keeps the dragged widget hovered: no name while it is carried.
    if let Some(view) = photo.view.filter(|_| !response.dragged()) {
        response.on_hover_text(folder_name(&view.path));
    }
    clicked
}

/// A thumbnail's face in `rect`: its frame on `surface`, then the photo fitted in it, or
/// a broken image when it has none.
pub(crate) fn paint(painter: &Painter, palette: &Palette, rect: Rect, shown: Shown) {
    painter.rect_filled(rect, CornerRadius::same(CORNER), palette.surface);
    match shown {
        Shown::Image(texture, size) => {
            let uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
            painter.add(
                RectShape::filled(fit(size, rect), CornerRadius::same(CORNER), Color32::WHITE)
                    .with_texture(texture, uv),
            );
        }
        Shown::Loading => {}
        Shown::Missing => {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                ICON_BROKEN_IMAGE.codepoint,
                FontId::new(24.0, ICON_BROKEN_IMAGE.font_family()),
                palette.dim,
            );
        }
    }
}

/// The largest rectangle of `size`'s shape centred in `frame`.
fn fit(size: Vec2, frame: Rect) -> Rect {
    if size.x <= 0.0 || size.y <= 0.0 {
        return frame;
    }
    let scale = (frame.width() / size.x).min(frame.height() / size.y);
    Rect::from_center_size(frame.center(), size * scale)
}

/// `path` inside the job's folder `input`, as the user knows it; the whole path when it
/// is not inside.
pub(crate) fn relative_path(path: &Path, input: &Path) -> String {
    path.strip_prefix(input)
        .map_or_else(|_| display_path(path), display_path)
}

/// The photo of `job`'s review being dragged, if one is. A drag from another job's review
/// (the selection changed during it) drops nowhere in this one.
pub(crate) fn dragged(context: &egui::Context, job: JobId) -> Option<DraggedPhoto> {
    DragAndDrop::payload::<DraggedPhoto>(context)
        .map(|photo| *photo)
        .filter(|photo| photo.job == job)
}

/// Where a dragged photo is, for one drop target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Drop {
    /// Held over the target, which shows it ([`target_fill`], [`target_stroke`]).
    Over,
    /// Released on it this frame: the drag has ended.
    Dropped,
}

/// A drop target in `rect`, for a photo being dragged that a drop there would move: where
/// the photo is, or `None` while the pointer is not over it. Call it before drawing the
/// target, which looks a target while the photo is over it.
pub(crate) fn drop_target(ui: &egui::Ui, rect: Rect) -> Option<Drop> {
    if !ui.rect_contains_pointer(rect) {
        return None;
    }
    if ui.input(|input| input.pointer.any_released()) {
        DragAndDrop::clear_payload(ui.ctx());
        Some(Drop::Dropped)
    } else {
        Some(Drop::Over)
    }
}

/// The fill of a drop target under a photo, over its own fill `base`: a tint of the
/// accent clearly stronger than a selected row's, so a target shows on a selected row
/// too.
pub(crate) fn target_fill(palette: &Palette, base: Color32) -> Color32 {
    theme::mix(base, palette.accent, TARGET_TINT)
}

/// The outline of a drop target under a photo, drawn inside it.
pub(crate) fn target_stroke(palette: &Palette) -> Stroke {
    Stroke::new(DROP_OUTLINE, palette.accent)
}

/// The copy of the dragged photo that follows the pointer, small, over everything else,
/// until it is dropped.
pub(crate) fn follow_the_pointer(context: &egui::Context, shown: Shown) {
    let Some(pointer) = context.pointer_latest_pos() else {
        return;
    };
    if context.input(|input| input.pointer.any_released()) {
        return;
    }
    let width = match shown {
        Shown::Image(_, size) if size.y > 0.0 => {
            (DRAGGED_HEIGHT * size.x / size.y).clamp(*DRAGGED_WIDTH.start(), *DRAGGED_WIDTH.end())
        }
        _ => DRAGGED_HEIGHT * 1.5,
    };
    let rect = Rect::from_min_size(pointer + DRAGGED_OFFSET, Vec2::new(width, DRAGGED_HEIGHT));
    let palette = theme::palette(context);
    let painter = context.layer_painter(LayerId::new(Order::Tooltip, Id::unique("dragged photo")));
    let shadow = context.global_style().visuals.popup_shadow;
    painter.add(shadow.as_shape(rect, CornerRadius::same(CORNER)));
    paint(&painter, palette, rect, shown);
    painter.rect_stroke(
        rect,
        CornerRadius::same(CORNER),
        Stroke::new(1.0, palette.accent),
        StrokeKind::Outside,
    );
}
