//! The bar across the top: who the app is, then, at the far end, the ML chip, starting a
//! new job, the settings and the theme toggle (spec §9.2).
//!
//! After `genetic_algo`'s `ui/src/toolbar.rs`.

use eframe::egui::{self, Color32, RichText, TextureHandle};
use egui_material_icons::icons::{
    ICON_BRIGHTNESS_AUTO, ICON_DARK_MODE, ICON_LIGHT_MODE, ICON_SETTINGS,
};

use crate::{
    controller::{Controller, MlState, View},
    format::{ml_state_text, needs_macos_text},
    identity::APP_NAME,
    prefs::ThemeChoice,
    theme::{self, Palette},
    widgets::{self, Emphasis},
};

const LOGO_SIZE: f32 = 22.0;
/// What the ML chip's hover says it does.
const ML_CHIP_HINT: &str = "The visual check of new jobs: open its settings";

/// Draws the toolbar. `New job` opens the new-job form, `Settings` the settings, and
/// looks selected while they are shown; the ML chip opens them on the visual check's
/// card.
pub fn show(ui: &mut egui::Ui, controller: &mut Controller, logo: &TextureHandle) {
    ui.horizontal_centered(|ui| {
        ui.add(egui::Image::new((logo.id(), egui::Vec2::splat(LOGO_SIZE))));
        ui.label(RichText::new(APP_NAME).heading());
        // Right to left: the theme toggle ends the row.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            theme_toggle(ui, controller);
            let shown = controller.view() == &View::Settings;
            if widgets::icon_toggle(ui, ICON_SETTINGS, "Settings", shown).clicked() {
                controller.open_settings();
            }
            let shortcut = new_job_shortcut(ui.ctx());
            if widgets::text_button(ui, "New job", Emphasis::Primary)
                .on_hover_text(shortcut)
                .clicked()
            {
                controller.open_new_job(None);
            }
            if ml_chip(ui, &controller.ml_state()).clicked() {
                controller.open_visual_check();
            }
        });
    });
}

/// The state of the visual check as a chip that is a button, named by its text: `ML off`,
/// `ML ready`, `ML 43 %`, `ML: no model`, `ML unavailable`. Its hover says what it does,
/// after why the check cannot run when the library needs a newer macOS.
fn ml_chip(ui: &mut egui::Ui, state: &MlState) -> egui::Response {
    let palette = theme::palette(ui.ctx());
    let hint = match needs_macos_text(state) {
        Some(why) => format!("{why}\n{ML_CHIP_HINT}"),
        None => ML_CHIP_HINT.to_owned(),
    };
    widgets::chip_button(ui, &ml_state_text(state), ml_dot(palette, state)).on_hover_text(hint)
}

/// The colour of the ML chip's dot: positive when ready, the accent while it downloads,
/// warning while the model is missing, dim while the check is off or cannot run.
fn ml_dot(palette: &Palette, state: &MlState) -> Color32 {
    match state {
        MlState::Ready(_) => palette.positive,
        MlState::Downloading { .. } => palette.accent,
        MlState::Missing(_) => palette.warning,
        MlState::Off | MlState::NoRuntime(_) | MlState::NotBuilt => palette.dim,
    }
}

/// The shortcut that starts a new job, as this system writes it.
fn new_job_shortcut(context: &egui::Context) -> &'static str {
    if context.os() == egui::os::OperatingSystem::Mac {
        "Cmd+N"
    } else {
        "Ctrl+N"
    }
}

/// Cycles follow the system, light, dark; the choice is applied at once and saved.
fn theme_toggle(ui: &mut egui::Ui, controller: &mut Controller) {
    let (icon, name, next) = match controller.prefs.theme {
        ThemeChoice::System => (
            ICON_BRIGHTNESS_AUTO,
            "Theme: follow the system",
            ThemeChoice::Light,
        ),
        ThemeChoice::Light => (ICON_LIGHT_MODE, "Theme: light", ThemeChoice::Dark),
        ThemeChoice::Dark => (ICON_DARK_MODE, "Theme: dark", ThemeChoice::System),
    };
    if widgets::icon_button(ui, icon, name, Emphasis::Normal).clicked() {
        // `App` applies it at the end of this frame, as it does Settings' choice.
        controller.set_theme(next);
    }
}
