//! What the new-job form and Settings share (spec §9.2): the centre's column of cards,
//! and the job's fields other than the folders and the model. The form draws them on its
//! own fields, Settings on the defaults of new jobs, so both always offer the same
//! choices, the same words and the sliders' ranges, which `JobConfig::validate` accepts at
//! every corner (`controller::form`'s tests).
//!
//! The cards, switches and sliders follow `genetic_algo`'s `ui/src/rules_panel.rs`.

use eframe::egui::{self, Label, RichText, Slider};
use photostacker_core::model::{OutputFormat, RawJpegPreference, RawTone};

use crate::controller::form::{COSINE_RANGE, FLOOR_RANGE, Field, HAMMING_RANGE, WINDOW_RANGE};
use crate::prefs::FormDefaults;
use crate::theme;
use crate::widgets;

/// The widest the column runs: past it, its fields would be long lines with little in
/// them.
const MAX_WIDTH: f32 = 560.0;
/// Between two cards.
pub(crate) const CARD_GAP: f32 = 8.0;
/// What a slider's row holds besides its rail: its value and its longest label.
const SLIDER_ROW_REST: f32 = 190.0;
const SLIDER_WIDTH: std::ops::RangeInclusive<f32> = 60.0..=240.0;
/// What the time window means, on hover: the form's slider, Settings' and the review's.
pub(crate) const WINDOW_HOVER: &str = "The longest wait between two shots of one stack.";

/// A column at most [`MAX_WIDTH`] wide, in the middle of the centre.
pub(crate) fn column(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width().min(MAX_WIDTH);
    let margin = ((ui.available_width() - width) / 2.0).floor();
    ui.horizontal_top(|ui| {
        ui.add_space(margin);
        ui.vertical(|ui| {
            ui.set_width(width);
            add(ui);
        });
    });
}

/// The `Scan subfolders` switch.
pub(crate) fn subfolders(ui: &mut egui::Ui, recursive: &mut bool) {
    widgets::switch(ui, recursive, "Scan subfolders")
        .on_hover_text("Also look for stacks in every folder inside the photo folder.");
}

/// What is written and how: the format, whether to wait for the review, and the RAWs.
pub(crate) fn output_fields(ui: &mut egui::Ui, fields: &mut FormDefaults) {
    choice(
        ui,
        "Format",
        &mut fields.output_format,
        &[
            (OutputFormat::Psd, "PSD"),
            (OutputFormat::Tiff, "Layered TIFF"),
        ],
    );
    widgets::switch(ui, &mut fields.auto_confirm, "Write without review")
        .on_hover_text("Write every stack found at once, without stopping for a review.");
    choice(
        ui,
        "RAW tone",
        &mut fields.raw_tone,
        &[
            (RawTone::Srgb, "sRGB"),
            (RawTone::Linear, "Linear (for HDR merging)"),
        ],
    );
    choice(
        ui,
        "RAW+JPEG pairs",
        &mut fields.raw_jpeg_preference,
        &[
            (RawJpegPreference::Raw, "Stack the RAW"),
            (RawJpegPreference::Jpeg, "Stack the JPEG"),
        ],
    );
}

/// A caption, then one radio button per option, on as many lines as the width needs.
pub(crate) fn choice<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    caption: &str,
    value: &mut T,
    options: &[(T, &str)],
) {
    let palette = theme::palette(ui.ctx());
    ui.label(RichText::new(caption).color(palette.secondary));
    ui.horizontal_wrapped(|ui| {
        // A widget's own fill hardly shows on a card: the circles get theirs.
        theme::show_choices(ui);
        for (option, text) in options {
            ui.radio_value(value, *option, *text);
        }
    });
}

/// How alike photos must be to make a stack: the sliders, each over the refusal about it
/// when `error`, the form's last, is.
pub(crate) fn grouping_fields(
    ui: &mut egui::Ui,
    fields: &mut FormDefaults,
    error: Option<&(Field, String)>,
) {
    set_slider_width(ui);
    ui.add(Slider::new(&mut fields.hamming_threshold, HAMMING_RANGE).text("Hash distance"))
        .on_hover_text("How different two photos may look and still be one stack.");
    error_line(ui, error, Field::Hamming);
    ui.add(Slider::new(&mut fields.time_window_seconds, WINDOW_RANGE).text("Time window (s)"))
        .on_hover_text(WINDOW_HOVER);
    error_line(ui, error, Field::Window);
    ui.add(Slider::new(&mut fields.stage3_ambiguity_floor, FLOOR_RANGE).text("Visual check floor"))
        .on_hover_text("Stacks whose photos differ by more than this get the visual check.");
    error_line(ui, error, Field::Floor);
}

/// Whether the job sets a similarity of its own (`Custom similarity`), and its slider while
/// it does, from `model_cosine`, the model's own threshold, when it is turned on. Off, the
/// model's threshold applies.
pub(crate) fn similarity_field(ui: &mut egui::Ui, cosine: &mut Option<f32>, model_cosine: f32) {
    let mut custom = cosine.is_some();
    if widgets::switch(ui, &mut custom, "Custom similarity")
        .on_hover_text("Off, the model's own threshold applies.")
        .changed()
    {
        let start = model_cosine.clamp(*COSINE_RANGE.start(), *COSINE_RANGE.end());
        *cosine = custom.then_some(start);
    }
    if let Some(cosine) = cosine {
        set_slider_width(ui);
        ui.add(
            Slider::new(cosine, COSINE_RANGE)
                .text("Similarity")
                .step_by(0.01)
                .fixed_decimals(2),
        )
        .on_hover_text("How alike the model must find the photos of a stack.");
    }
}

/// Sliders as wide as the card leaves them, so a row never runs past it.
pub(crate) fn set_slider_width(ui: &mut egui::Ui) {
    let width =
        (ui.available_width() - SLIDER_ROW_REST).clamp(*SLIDER_WIDTH.start(), *SLIDER_WIDTH.end());
    ui.spacing_mut().slider_width = width;
}

/// The refusal under `field`, if `error`, the last one, is about it, in the danger colour.
pub(crate) fn error_line(ui: &mut egui::Ui, error: Option<&(Field, String)>, field: Field) {
    if let Some((_, text)) = error.filter(|(at, _)| *at == field) {
        let palette = theme::palette(ui.ctx());
        ui.add(Label::new(RichText::new(text).color(palette.danger)).wrap());
    }
}
