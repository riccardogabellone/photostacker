//! The "Visual check (Stage 3)" card of Settings (spec §8.2, §9.2): the switch, the models
//! of the registry, one row each, to choose the active one and to download, import or
//! delete them, the similarity new jobs start with, and the disk the models take.
//!
//! Every control of a row carries the model's name in its name for screen readers and
//! tests (`Download DINOv2 small`), since each row has the same buttons; their text stays
//! short. Nothing here reaches the network or the file system: `Download` asks first
//! (`Question::DownloadModel`), and the controller does the rest through the engine.

use eframe::egui::{self, Align, CornerRadius, Label, RichText, Role, WidgetInfo};
use egui_material_icons::icons::ICON_WARNING;
use photostacker_core::engine::{ModelActivity, ModelView};

use crate::controller::{Controller, MlState, ModelFailure, Question};
use crate::format::{download_percent, mib, needs_macos_text};
use crate::prefs::FormDefaults;
use crate::theme;
use crate::views::fields::similarity_field;
use crate::widgets::{self, Emphasis};

/// The card's title.
const TITLE: &str = "Visual check (Stage 3)";
/// The switch, the preference `ml_enabled`.
const SWITCH: &str = "Check ambiguous stacks by their look";
/// What the check does, and what it needs.
const WHAT_IT_DOES: &str = "When the hashes leave a stack in doubt, its photos are compared by \
    their look, on this computer, by a model that is downloaded once.";
/// What a build without the `ml` feature says, alone in the card.
const NOT_BUILT: &str = "This build has no visual check.";
/// The caption over the similarity new jobs start with.
const SIMILARITY: &str = "Similarity of new jobs";
/// The width of a download's progress bar.
const BAR_WIDTH: f32 = 140.0;
/// The height of a download's progress bar: the job's.
const BAR_HEIGHT: f32 = 8.0;
/// The height of a row's state line: its buttons'.
const STATE_ROW_HEIGHT: f32 = 28.0;

/// What the user asked for; done once the card is drawn, which borrows the controller.
enum Action {
    Enable(bool),
    Use(&'static str),
    AskToDownload(&'static str),
    Download(&'static str),
    Cancel(&'static str),
    Import(&'static str),
    Delete(&'static str),
    Defaults(FormDefaults),
}

/// Draws the card, scrolled to the top of the view when the ML chip opened Settings.
pub fn show(ui: &mut egui::Ui, controller: &mut Controller) {
    let state = controller.ml_state();
    let reveal = controller.take_reveal_visual_check();
    let mut action = None;
    let card = ui.scope(|ui| content(ui, controller, &state, &mut action));
    if reveal {
        ui.scroll_to_rect(card.response.rect, Some(Align::TOP));
    }
    match action {
        Some(Action::Enable(on)) => controller.set_ml_enabled(on),
        Some(Action::Use(id)) => controller.set_ml_model(id),
        Some(Action::AskToDownload(id)) => controller.ask(Question::DownloadModel(id)),
        Some(Action::Download(id)) => controller.download_model(id),
        Some(Action::Cancel(id)) => controller.cancel_download(id),
        Some(Action::Import(id)) => controller.import_model(id),
        Some(Action::Delete(id)) => controller.remove_model(id),
        Some(Action::Defaults(defaults)) => controller.set_defaults(defaults),
        None => {}
    }
}

/// The card itself, for `state`; what the user asked for goes to `action`.
fn content(
    ui: &mut egui::Ui,
    controller: &Controller,
    state: &MlState,
    action: &mut Option<Action>,
) {
    let palette = theme::palette(ui.ctx());
    widgets::card(ui, TITLE, None, |ui| {
        if *state == MlState::NotBuilt {
            ui.label(RichText::new(NOT_BUILT).color(palette.secondary));
            return;
        }
        let mut on = controller.prefs.ml_enabled;
        if widgets::switch(ui, &mut on, SWITCH).changed() {
            *action = Some(Action::Enable(on));
        }
        ui.add(Label::new(RichText::new(WHAT_IT_DOES).color(palette.secondary)).wrap());
        if let MlState::NoRuntime(check) = state {
            ui.add_space(4.0);
            let text = needs_macos_text(state).unwrap_or_else(|| {
                format!(
                    "ONNX Runtime was not found at {}, so the visual check cannot run.",
                    check.path.display()
                )
            });
            warning(ui, &text);
        }
        let active = controller.active_model().id;
        for view in controller.models() {
            ui.add_space(8.0);
            let failure = controller.model_failure(view.spec.id);
            row(ui, view, view.spec.id == active, failure, action);
        }
        ui.add_space(8.0);
        ui.label(RichText::new(SIMILARITY).color(palette.secondary));
        let mut defaults = controller.prefs.defaults.clone();
        let model_cosine = controller.active_model().default_cosine;
        similarity_field(ui, &mut defaults.cosine_threshold, model_cosine);
        if defaults != controller.prefs.defaults {
            *action = Some(Action::Defaults(defaults));
        }
        ui.add_space(8.0);
        let used: u64 = controller.models().iter().map(|view| view.disk_bytes).sum();
        let text = format!("Models use {} on disk.", mib(used));
        ui.label(RichText::new(text).color(palette.secondary));
    });
}

/// A warning: its icon in `warning`, the text in `text`, wrapping.
fn warning(ui: &mut egui::Ui, text: &str) {
    let palette = theme::palette(ui.ctx());
    ui.horizontal_top(|ui| {
        let enabled = ui.is_enabled();
        let mark = ui.label(ICON_WARNING.rich_text().color(palette.warning));
        mark.widget_info(|| WidgetInfo::labeled(Role::Image, enabled, "Warning"));
        ui.add(Label::new(RichText::new(text).color(palette.text)).wrap());
    });
}

/// One model: the radio button that makes it the active one, with its name, its size and
/// licence; under them, indented to the name, its state and what can be done with it;
/// under those, its last failure, in `danger`, with `Try again` after a download's.
fn row(
    ui: &mut egui::Ui,
    view: &ModelView,
    active: bool,
    failure: Option<&ModelFailure>,
    action: &mut Option<Action>,
) {
    let palette = theme::palette(ui.ctx());
    let spec = view.spec;
    let name = spec.name;
    ui.horizontal(|ui| {
        theme::show_choices(ui);
        let enabled = ui.is_enabled();
        let radio = ui.radio(active, name);
        let use_it = format!("Use {name}");
        radio.widget_info(|| WidgetInfo::selected(Role::RadioButton, enabled, active, &use_it));
        if radio.clicked() && !active {
            *action = Some(Action::Use(spec.id));
        }
        let facts = format!("{} · {}", mib(spec.size), spec.licence);
        ui.label(RichText::new(facts).color(palette.secondary));
    });
    // Under the name, past the radio button's circle.
    let indent = ui.spacing().icon_width + ui.spacing().icon_spacing;
    ui.scope(|ui| {
        // Rows of the buttons' height, so a text beside one is centred on it: set before
        // the wrapping row starts, whose first line takes its height from it.
        ui.spacing_mut().interact_size.y = STATE_ROW_HEIGHT;
        ui.horizontal_wrapped(|ui| {
            ui.add_space(indent);
            state(ui, view, action);
        });
    });
    // Shown once the read after the failure has the model idle, so a row never says
    // why a download failed beside the bar of that very download.
    if let Some(failure) = failure.filter(|_| view.activity == ModelActivity::Idle) {
        ui.horizontal_top(|ui| {
            ui.add_space(indent);
            ui.vertical(|ui| {
                let text = RichText::new(&failure.text).color(palette.danger);
                ui.add(Label::new(text).wrap());
                if failure.download
                    && named_button(ui, "Try again", &format!("Try again with {name}"))
                {
                    *action = Some(Action::Download(spec.id));
                }
            });
        });
    }
}

/// What the engine is doing with the model, or whether it is installed, and the buttons
/// that go with it.
fn state(ui: &mut egui::Ui, view: &ModelView, action: &mut Option<Action>) {
    let palette = theme::palette(ui.ctx());
    let spec = view.spec;
    let name = spec.name;
    let secondary = |text: &str| RichText::new(text).color(palette.secondary);
    match view.activity {
        ModelActivity::Downloading { done, total } => {
            let percent = download_percent(done, total);
            progress_bar(ui, percent, &format!("Download of {name}"));
            ui.label(format!("{percent} % of {}", mib(total)));
            if named_button(ui, "Cancel", &format!("Cancel the download of {name}")) {
                *action = Some(Action::Cancel(spec.id));
            }
        }
        ModelActivity::Cancelling => {
            ui.label(secondary("Cancelling…"));
        }
        ModelActivity::Importing => {
            ui.label(secondary("Importing…"));
        }
        ModelActivity::Idle if view.installed => {
            ui.label("Installed");
            if named_button(ui, "Delete", &format!("Delete {name}")) {
                *action = Some(Action::Delete(spec.id));
            }
        }
        ModelActivity::Idle => {
            ui.label(secondary("Not installed"));
            if named_button(ui, "Download", &format!("Download {name}")) {
                *action = Some(Action::AskToDownload(spec.id));
            }
            if named_button(ui, "Import a file…", &format!("Import a file for {name}")) {
                *action = Some(Action::Import(spec.id));
            }
        }
    }
}

/// A download's bar, 8 px high on a `surface_active` track, named `name` with its
/// percentage as its value.
fn progress_bar(ui: &mut egui::Ui, percent: u8, name: &str) {
    let palette = theme::palette(ui.ctx());
    let bar = ui
        .scope(|ui| {
            // The bar's track, which egui draws in this colour, would not show on a card.
            ui.visuals_mut().extreme_bg_color = palette.surface_active;
            ui.add(
                egui::ProgressBar::new(f32::from(percent) / 100.0)
                    .desired_width(BAR_WIDTH)
                    .desired_height(BAR_HEIGHT)
                    .fill(palette.accent)
                    .corner_radius(CornerRadius::same(4)),
            )
        })
        .inner;
    let enabled = ui.is_enabled();
    bar.widget_info(|| {
        let mut info = WidgetInfo::labeled(Role::ProgressIndicator, enabled, name);
        info.value = Some(f64::from(percent));
        info
    });
}

/// A normal button that shows `text` and is named `name`; whether it was clicked.
fn named_button(ui: &mut egui::Ui, text: &str, name: &str) -> bool {
    let enabled = ui.is_enabled();
    let button = widgets::text_button(ui, text, Emphasis::Normal);
    button.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, name));
    button.clicked()
}
