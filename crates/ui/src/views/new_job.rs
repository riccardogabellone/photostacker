//! The new-job form in the centre (spec §9.2): the folders, the output, the grouping and
//! the visual check, each in a card, then Start and Cancel. The view edits the
//! controller's [`JobForm`] and asks the controller for the rest: the folder dialogs,
//! the submission and closing the form. A refusal shows under the field it is about.
//!
//! The fields other than the folders and the visual check, and the column of cards, are
//! `views::fields`', which Settings draws too, on the defaults of new jobs.
//!
//! The cards, switches and sliders follow `genetic_algo`'s `ui/src/rules_panel.rs`.

use eframe::egui::{self, Label, RichText, Role, TextEdit, TextWrapMode, WidgetInfo};
use photostacker_core::models::{self, ModelSpec};

use crate::controller::form::{Field, JobForm};
use crate::controller::{Controller, MlState};
use crate::theme;
use crate::views::fields::{
    CARD_GAP, column, error_line, grouping_fields, output_fields, similarity_field, subfolders,
};
use crate::views::job::named_button;
use crate::widgets::{self, Emphasis};

/// The form's title. Not `New job`, which names the toolbar's button.
const TITLE: &str = "Scan a folder";
/// The visible text of the Browse buttons, each named after its folder.
const BROWSE: &str = "Browse…";
/// The name of the form's `Cancel`: the selected job's row may have a `Cancel` of its own.
pub const CANCEL_NEW_JOB: &str = "Cancel new job";

/// What the user asked of the controller; done once the form is drawn, which borrows it.
enum Action {
    BrowseInput,
    BrowseOutput,
    Start,
    Cancel,
}

/// Draws the form, which takes the whole centre: the cards scroll when the window is
/// short, above a bar that keeps Start and Cancel in sight.
pub fn show(ui: &mut egui::Ui, controller: &mut Controller) {
    let state = controller.ml_state();
    let model = controller.job_model().and_then(models::model);
    let Some(form) = controller.form() else {
        return;
    };
    let mut action = None;
    let palette = theme::palette(ui.ctx());
    let bar = egui::Frame::new()
        .fill(palette.window)
        .inner_margin(egui::Margin::symmetric(0, 8));
    egui::Panel::bottom("new_job_buttons")
        .frame(bar)
        .show(ui, |ui| {
            column(ui, |ui| buttons(ui, form.submitting, &mut action));
        });
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            column(ui, |ui| {
                ui.label(RichText::new(TITLE).heading());
                ui.add_space(CARD_GAP);
                // Locked while the engine decides, so the answer is about what is shown.
                ui.add_enabled_ui(!form.submitting, |ui| {
                    folders(ui, form, &mut action);
                    ui.add_space(CARD_GAP);
                    widgets::card(ui, "Output", None, |ui| output_fields(ui, &mut form.fields));
                    ui.add_space(CARD_GAP);
                    widgets::card(ui, "Grouping", None, |ui| {
                        grouping_fields(ui, &mut form.fields, form.error.as_ref());
                    });
                    ui.add_space(CARD_GAP);
                    visual_check(ui, form, &state, model);
                });
            });
        });
    match action {
        Some(Action::BrowseInput) => controller.browse_input(),
        Some(Action::BrowseOutput) => controller.browse_output(),
        Some(Action::Start) => controller.submit_form(),
        Some(Action::Cancel) => controller.close_form(),
        None => {}
    }
}

/// The photo folder, the output folder and whether to scan the subfolders.
fn folders(ui: &mut egui::Ui, form: &mut JobForm, action: &mut Option<Action>) {
    widgets::card(ui, "Folders", None, |ui| {
        if folder_field(ui, form, Field::Input) {
            *action = Some(Action::BrowseInput);
        }
        ui.add_space(4.0);
        if folder_field(ui, form, Field::Output) {
            *action = Some(Action::BrowseOutput);
        }
        ui.add_space(4.0);
        subfolders(ui, &mut form.fields.recursive);
    });
}

/// A folder's label, its text field with a Browse button, and its error. Returns whether
/// Browse was clicked.
fn folder_field(ui: &mut egui::Ui, form: &mut JobForm, field: Field) -> bool {
    let (label, browse_name, hint) = match field {
        Field::Input => (
            "Photo folder",
            "Browse for the photo folder",
            "Paste a path, browse, or drop a folder",
        ),
        _ => (
            "Output folder",
            "Browse for the output folder",
            "The stacked folder inside the photo folder",
        ),
    };
    let mut text = match field {
        Field::Input => form.input.clone(),
        _ => form.output.clone(),
    };
    let label = ui.label(label);
    let browse_width = button_width(ui, BROWSE);
    let mut browse = false;
    ui.horizontal(|ui| {
        let width = (ui.available_width() - browse_width - ui.spacing().item_spacing.x).max(0.0);
        let edit = ui
            .add(
                TextEdit::singleline(&mut text)
                    .hint_text(hint)
                    .desired_width(width),
            )
            .labelled_by(label.id);
        if edit.changed() {
            match field {
                Field::Input => form.set_input(&text),
                _ => form.set_output(&text),
            }
        }
        let enabled = ui.is_enabled();
        let button = widgets::text_button(ui, BROWSE, Emphasis::Normal);
        button.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, browse_name));
        browse = button.clicked();
    });
    error_line(ui, form.error.as_ref(), field);
    browse
}

/// How wide a text button of `text` is.
fn button_width(ui: &egui::Ui, text: &str) -> f32 {
    let galley = egui::WidgetText::from(text).into_galley(
        ui,
        Some(TextWrapMode::Extend),
        f32::INFINITY,
        egui::TextStyle::Button,
    );
    galley.size().x + 2.0 * ui.spacing().button_padding.x
}

/// What the visual check does with the new job, in one sentence, as `state` has it (spec
/// §9.2): with `model`, the one the job names, installed or not (the job is checked once
/// it is), the sentence names it and the similarity follows; without one, why.
fn visual_check(ui: &mut egui::Ui, form: &mut JobForm, state: &MlState, model: Option<&ModelSpec>) {
    let palette = theme::palette(ui.ctx());
    widgets::card(ui, "Visual check", None, |ui| {
        let text = check_sentence(state, model);
        let color = if model.is_some() {
            palette.text
        } else {
            palette.secondary
        };
        ui.add(Label::new(RichText::new(text).color(color)).wrap());
        error_line(ui, form.error.as_ref(), Field::Model);
        if let Some(model) = model {
            similarity_field(ui, &mut form.fields.cosine_threshold, model.default_cosine);
            error_line(ui, form.error.as_ref(), Field::Cosine);
        }
    });
}

/// The card's sentence for `state`, whose job names `model` (or none).
fn check_sentence(state: &MlState, model: Option<&ModelSpec>) -> String {
    let name = model.map_or("", |model| model.name);
    match state {
        MlState::Ready(_) => format!("{name} checks the stacks the hashes leave in doubt."),
        MlState::Downloading { .. } => {
            format!("{name} checks the stacks in doubt once its download is done.")
        }
        MlState::Missing(_) => {
            format!("{name} checks the stacks in doubt once it is installed: see Settings.")
        }
        MlState::Off => "Off: turn it on in Settings.".to_owned(),
        MlState::NoRuntime(_) => "Off: ONNX Runtime is missing, so it cannot run.".to_owned(),
        MlState::NotBuilt => "This build has no visual check.".to_owned(),
    }
}

/// Start, the form's one main action, and Cancel; a spinner while the engine decides,
/// when neither can be pressed: the job would be made all the same.
fn buttons(ui: &mut egui::Ui, submitting: bool, action: &mut Option<Action>) {
    ui.horizontal(|ui| {
        ui.add_enabled_ui(!submitting, |ui| {
            if widgets::text_button(ui, "Start", Emphasis::Primary).clicked() {
                *action = Some(Action::Start);
            }
            if named_button(ui, "Cancel", CANCEL_NEW_JOB) {
                *action = Some(Action::Cancel);
            }
        });
        if submitting {
            ui.spinner();
        }
    });
}
