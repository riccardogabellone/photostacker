//! Small widgets shared by the views, drawn from the active palette. Each interactive one
//! names itself for screen readers and tests.
//!
//! Copied from `genetic_algo`'s `ui/src/widgets.rs` (without its sparkline) and adapted;
//! `chip` and `text_button` are new here.

use eframe::egui::{
    self, Button, Color32, CornerRadius, FontId, Margin, Rect, Response, RichText, Role, Stroke,
    Vec2, WidgetInfo,
};
use egui_material_icons::MaterialIcon;

use crate::format::truncate_middle;
use crate::theme;

/// The height of buttons, so icon and text buttons line up in a row.
pub(crate) const BUTTON_HEIGHT: f32 = 28.0;

/// How a button reads: the one main action of a view is filled with the accent, an action
/// that destroys something with the danger colour, and a decision in force that accepts
/// something (a confirmed stack's Confirm) with the positive colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emphasis {
    Primary,
    Danger,
    Positive,
    Normal,
}

/// The fill of a button of `emphasis`, under its text in `on_accent`; none for a normal
/// or disabled one.
fn emphasis_fill(palette: &theme::Palette, emphasis: Emphasis, enabled: bool) -> Option<Color32> {
    if !enabled {
        return None;
    }
    match emphasis {
        Emphasis::Primary => Some(palette.accent),
        Emphasis::Danger => Some(palette.danger),
        Emphasis::Positive => Some(palette.positive),
        Emphasis::Normal => None,
    }
}

/// A button showing only an icon. `label` is its name: shown as a tooltip and given to
/// screen readers (and tests), which would otherwise read the icon's private codepoint.
pub fn icon_button(
    ui: &mut egui::Ui,
    icon: MaterialIcon,
    label: &str,
    emphasis: Emphasis,
) -> Response {
    let palette = theme::palette(ui.ctx());
    let enabled = ui.is_enabled();
    let text = icon.rich_text().size(18.0);
    let button = match emphasis_fill(palette, emphasis, enabled) {
        Some(fill) => Button::new(text.color(palette.on_accent)).fill(fill),
        None => Button::new(text),
    };
    let response = ui
        .add(button.min_size(Vec2::new(32.0, BUTTON_HEIGHT)))
        .on_hover_text(label);
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, label));
    response
}

/// [`icon_button`] for something the button shows while `selected` (the toolbar's
/// `Settings` while Settings are shown): then with a look of its own, its `surface` fill
/// tinted 12 % towards the accent and a 1 px accent outline, which marks it on the
/// toolbar's `panel` (a selected stack's row is tinted 8 % on `panel`, a selected job's row
/// only outlined). Screen readers hear its state.
pub fn icon_toggle(ui: &mut egui::Ui, icon: MaterialIcon, label: &str, selected: bool) -> Response {
    let palette = theme::palette(ui.ctx());
    let enabled = ui.is_enabled();
    let mut button = Button::new(icon.rich_text().size(18.0));
    if selected {
        button = button
            .fill(theme::mix(palette.surface, palette.accent, SELECTED_TINT))
            .stroke(Stroke::new(1.0, palette.accent));
    }
    let response = ui
        .add(button.min_size(Vec2::new(32.0, BUTTON_HEIGHT)))
        .on_hover_text(label);
    response.widget_info(|| WidgetInfo::selected(Role::Button, enabled, selected, label));
    response
}

/// How far a selected toggle's fill goes from `surface` towards the accent.
const SELECTED_TINT: f32 = 0.12;

/// A button with a text label, which is also its name for screen readers and tests.
pub fn text_button(ui: &mut egui::Ui, label: &str, emphasis: Emphasis) -> Response {
    text_button_enabled(ui, true, label, emphasis)
}

/// [`text_button`], disabled unless `enabled`. Unlike a button in a disabled scope, it is
/// one widget, so a row that wraps can move it whole to its next line.
pub fn text_button_enabled(
    ui: &mut egui::Ui,
    enabled: bool,
    label: &str,
    emphasis: Emphasis,
) -> Response {
    let palette = theme::palette(ui.ctx());
    let button = match emphasis_fill(palette, emphasis, enabled && ui.is_enabled()) {
        Some(fill) => Button::new(RichText::new(label).color(palette.on_accent)).fill(fill),
        None => Button::new(label),
    };
    ui.add_enabled(enabled, button.min_size(Vec2::new(0.0, BUTTON_HEIGHT)))
}

/// A small rounded label such as a stack's status: a dot in `color`, then `text` in the
/// palette's text colour, which screen readers read. The colour is never the only cue,
/// and the text keeps the contrast `theme`'s test guarantees for `text` (7:1 on every
/// surface). Pass a status role (`accent`, `danger`, `warning`, `positive`): the same
/// test holds those at 3:1, the floor for a non-text mark, on `panel`, `surface` and
/// `surface_active`.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) -> Response {
    let palette = theme::palette(ui.ctx());
    let galley = chip_galley(ui, text);
    let size = chip_size(&galley);
    let padding = CHIP_PADDING;
    let gap = CHIP_GAP;
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let enabled = ui.is_enabled();
    response.widget_info(|| WidgetInfo::labeled(Role::Label, enabled, text));

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        painter.rect_stroke(
            rect,
            rect.height() / 2.0,
            Stroke::new(1.0, palette.outline),
            egui::StrokeKind::Inside,
        );
        let dot = egui::pos2(rect.left() + padding.x + CHIP_DOT_RADIUS, rect.center().y);
        painter.circle_filled(dot, CHIP_DOT_RADIUS, color);
        let text_pos = egui::pos2(
            dot.x + CHIP_DOT_RADIUS + gap,
            rect.center().y - galley.size().y / 2.0,
        );
        painter.galley(text_pos, galley, palette.text);
    }
    response
}

/// A [`chip`] that is a button (the toolbar's ML chip): its name is its text, it sits on
/// the button's `surface` fill, `surface_hover` under the pointer, and is a button's
/// height, so it lines up with the toolbar's other buttons and is as easy to hit.
pub fn chip_button(ui: &mut egui::Ui, text: &str, color: Color32) -> Response {
    let palette = theme::palette(ui.ctx());
    let galley = chip_galley(ui, text);
    let size = Vec2::new(
        chip_size(&galley).x + 2.0 * CHIP_BUTTON_EXTRA,
        CHIP_BUTTON_HEIGHT,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::click());
    let enabled = ui.is_enabled();
    response.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, text));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        let fill = if response.hovered() {
            palette.surface_hover
        } else {
            palette.surface
        };
        let radius = rect.height() / 2.0;
        painter.rect_filled(rect, radius, fill);
        painter.rect_stroke(
            rect,
            radius,
            Stroke::new(1.0, palette.outline),
            egui::StrokeKind::Inside,
        );
        let left = rect.left() + CHIP_BUTTON_EXTRA + CHIP_PADDING.x;
        let dot = egui::pos2(left + CHIP_DOT_RADIUS, rect.center().y);
        painter.circle_filled(dot, CHIP_DOT_RADIUS, color);
        let text_pos = egui::pos2(
            dot.x + CHIP_DOT_RADIUS + CHIP_GAP,
            rect.center().y - galley.size().y / 2.0,
        );
        painter.galley(text_pos, galley, palette.text);
    }
    response.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// What a [`chip_button`] has on each side besides a chip's padding.
const CHIP_BUTTON_EXTRA: f32 = 3.0;
/// A [`chip_button`]'s height: the toolbar's other buttons'.
const CHIP_BUTTON_HEIGHT: f32 = BUTTON_HEIGHT;

const CHIP_DOT_RADIUS: f32 = 3.5;
const CHIP_PADDING: Vec2 = Vec2::new(7.0, 1.0);
/// Between a chip's dot and its text.
const CHIP_GAP: f32 = 5.0;

/// How wide [`chip`] draws `text`: a row measures its chips before it draws them, to
/// leave out what does not fit.
pub fn chip_width(ui: &egui::Ui, text: &str) -> f32 {
    chip_size(&chip_galley(ui, text)).x
}

fn chip_galley(ui: &egui::Ui, text: &str) -> std::sync::Arc<egui::Galley> {
    egui::WidgetText::from(RichText::new(text).small()).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Small,
    )
}

fn chip_size(galley: &egui::Galley) -> Vec2 {
    Vec2::new(
        2.0 * CHIP_DOT_RADIUS + CHIP_GAP + galley.size().x,
        galley.size().y.max(2.0 * CHIP_DOT_RADIUS),
    ) + 2.0 * CHIP_PADDING
}

/// A small caption over a value, for live readouts such as a count.
pub fn readout(ui: &mut egui::Ui, caption: &str, value: &str) {
    let palette = theme::palette(ui.ctx());
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        ui.label(RichText::new(caption).small().color(palette.secondary));
        ui.label(
            RichText::new(value)
                .family(egui::FontFamily::Name(theme::SEMIBOLD_FAMILY.into()))
                .size(14.0)
                .color(palette.text),
        );
    });
}

/// An on/off switch with its label on the left. The label is part of the switch: clicking
/// it toggles too, and screen readers read it.
pub fn switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> Response {
    let palette = theme::palette(ui.ctx());
    let height = ui.spacing().interact_size.y * 0.75;
    let track = Vec2::new(height * 1.8, height);
    let gap = ui.spacing().item_spacing.x;
    let galley = egui::WidgetText::from(label).into_galley(
        ui,
        Some(egui::TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Body,
    );
    let size = Vec2::new(
        galley.size().x + gap + track.x,
        galley.size().y.max(ui.spacing().interact_size.y),
    );
    let (rect, mut response) = ui.allocate_exact_size(size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    let enabled = ui.is_enabled();
    let state = *on;
    response.widget_info(|| WidgetInfo::selected(Role::CheckBox, enabled, state, label));

    if ui.is_rect_visible(rect) {
        let progress = ui.ctx().animate_bool_responsive(response.id, *on);
        let text_color = if enabled { palette.text } else { palette.dim };
        let text_pos = egui::pos2(rect.left(), rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(text_pos, galley, text_color);

        let track_rect = Rect::from_min_size(
            egui::pos2(rect.right() - track.x, rect.center().y - track.y / 2.0),
            track,
        );
        let off_fill = if response.hovered() {
            palette.surface_hover
        } else {
            palette.surface_active
        };
        let fill = theme::mix(off_fill, palette.accent, progress);
        let radius = track.y / 2.0;
        ui.painter().rect_filled(track_rect, radius, fill);
        let knob_x = egui::lerp(
            (track_rect.left() + radius)..=(track_rect.right() - radius),
            progress,
        );
        let knob = if enabled { palette.text } else { palette.dim };
        let knob = theme::mix(knob, palette.on_accent, progress);
        ui.painter().circle_filled(
            egui::pos2(knob_x, track_rect.center().y),
            radius * 0.7,
            knob,
        );
    }
    response
}

/// A group of related settings: a titled frame. With a switch in the header, the switch
/// turns the group on and off, and its settings show only while it is on. Returns
/// whether the switch changed.
pub fn card(
    ui: &mut egui::Ui,
    title: &str,
    switch: Option<(&mut bool, &str)>,
    body: impl FnOnce(&mut egui::Ui),
) -> bool {
    let palette = theme::palette(ui.ctx());
    card_frame(palette)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let mut changed = false;
            let mut open = true;
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(title)
                        .family(egui::FontFamily::Name(theme::SEMIBOLD_FAMILY.into()))
                        .color(palette.text),
                );
                if let Some((on, label)) = switch {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        changed = self::switch(ui, on, label).changed();
                    });
                    open = *on;
                }
            });
            if open {
                body(ui);
            }
            changed
        })
        .inner
}

/// A headline number: caption, value and a detail line.
pub fn stat_tile(ui: &mut egui::Ui, caption: &str, value: &str, detail: &str) {
    let palette = theme::palette(ui.ctx());
    card_frame(palette).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.spacing_mut().item_spacing.y = 2.0;
        ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Truncate);
        ui.label(RichText::new(caption).small().color(palette.secondary));
        ui.label(
            RichText::new(value)
                .family(egui::FontFamily::Name(theme::SEMIBOLD_FAMILY.into()))
                .size(20.0)
                .color(palette.text),
        );
        ui.label(RichText::new(detail).small().color(palette.secondary));
    });
}

/// `text` cut in the middle ([`truncate_middle`]) to the most characters that fit `width`
/// in `font`, so both its ends stay in sight: a path keeps its root and its own name.
pub fn fitted(ui: &egui::Ui, text: &str, font: &FontId, width: f32) -> String {
    let measure = |text: &str| {
        ui.ctx().fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(text.to_owned(), font.clone(), Color32::PLACEHOLDER)
                .size()
                .x
        })
    };
    let full = measure(text);
    if full <= width {
        return text.to_owned();
    }
    // A guess in proportion to the width, then a character less until it fits.
    let mut budget = (text.chars().count() as f32 * width / full) as usize;
    loop {
        let shown = truncate_middle(text, budget);
        if budget <= 1 || measure(&shown) <= width {
            return shown;
        }
        budget -= 1;
    }
}

/// The outlined frame around cards and tiles.
fn card_frame(palette: &theme::Palette) -> egui::Frame {
    egui::Frame::new()
        .fill(palette.panel)
        .stroke(Stroke::new(1.0, palette.outline))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
}
