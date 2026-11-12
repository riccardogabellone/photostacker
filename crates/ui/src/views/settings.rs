//! Settings in the centre (spec §9.2): the appearance, what new jobs start with, the
//! visual check and its models (`views::models`), and About, each in a card of a column
//! that scrolls, above a bar that keeps `Done` in sight. `Done`, or Esc, shows again what
//! the centre showed before.
//!
//! Every change is the user's preference at once and is saved (the controller's
//! setters); `App` applies a new theme in the same frame. The defaults of new jobs are
//! drawn by the functions the new-job form draws its own with (`views::fields`), so
//! they offer exactly its choices and its sliders' ranges: no value saved here can fail
//! the engine's validation. The similarity they start with is the visual check's card's.

use eframe::egui::{self, Label, RichText, Role, WidgetInfo};

use crate::controller::Controller;
use crate::identity::APP_NAME;
use crate::prefs::{FormDefaults, ThemeChoice};
use crate::theme::{self, SEMIBOLD_FAMILY};
use crate::views::fields::{CARD_GAP, choice, column, grouping_fields, output_fields, subfolders};
use crate::views::models;
use crate::widgets::{self, Emphasis};

/// The title. Not `Settings`, which names the toolbar's button.
const TITLE: &str = "App settings";
/// The name of `Done` for screen readers and tests: a done job's status chip is also
/// read "Done", on the same screen.
const CLOSE: &str = "Close settings";
/// What sets the defaults of new jobs back to [`FormDefaults::default`].
const RESET: &str = "Reset to the built-in defaults";
/// What the About card says before the components that are not MIT.
const LICENCE: &str =
    "Free software under the MIT licence. These parts have licences of their own:";
/// What the About card says, in a package, before its way to the licence texts.
const LICENCES_SHIP: &str = "The licence texts of every part ship with the app.";
/// The HEVC patents' caveat (spec §3).
#[cfg(feature = "heic")]
const HEVC: &str = "HEVC, the codec of HEIC photos, may be covered by patents in some \
    countries: decoding it is the same grey area GIMP and ImageMagick ship with.";

/// What the user asked for; done once the view is drawn, which borrows the preferences.
enum Action {
    Done,
    Theme(ThemeChoice),
    Defaults(FormDefaults),
    OpenLicences,
    OpenLogs,
}

/// Draws Settings, which take the whole centre.
pub fn show(ui: &mut egui::Ui, controller: &mut Controller) {
    let palette = theme::palette(ui.ctx());
    let mut action = None;
    let bar = egui::Frame::new()
        .fill(palette.window)
        .inner_margin(egui::Margin::symmetric(0, 8));
    egui::Panel::bottom("settings_buttons")
        .frame(bar)
        .show(ui, |ui| {
            column(ui, |ui| {
                let enabled = ui.is_enabled();
                let done = widgets::text_button(ui, "Done", Emphasis::Normal);
                done.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, CLOSE));
                if done.clicked() {
                    action = Some(Action::Done);
                }
            });
        });
    egui::ScrollArea::vertical()
        .auto_shrink(false)
        .show(ui, |ui| {
            column(ui, |ui| {
                ui.label(RichText::new(TITLE).heading());
                ui.add_space(CARD_GAP);
                appearance(ui, controller.prefs.theme, &mut action);
                ui.add_space(CARD_GAP);
                new_jobs(ui, &controller.prefs.defaults, &mut action);
                ui.add_space(CARD_GAP);
                models::show(ui, controller);
                ui.add_space(CARD_GAP);
                let packaged = controller.licences_folder().is_some();
                about(ui, packaged, &mut action);
            });
        });
    match action {
        Some(Action::Done) => controller.close_settings(),
        Some(Action::Theme(choice)) => controller.set_theme(choice),
        Some(Action::Defaults(defaults)) => controller.set_defaults(defaults),
        Some(Action::OpenLicences) => controller.open_licences(),
        Some(Action::OpenLogs) => controller.open_logs(),
        None => {}
    }
}

/// The theme: the toolbar's toggle shows the same choice.
fn appearance(ui: &mut egui::Ui, current: ThemeChoice, action: &mut Option<Action>) {
    widgets::card(ui, "Appearance", None, |ui| {
        let mut chosen = current;
        choice(
            ui,
            "Theme",
            &mut chosen,
            &[
                (ThemeChoice::System, "Follow the system"),
                (ThemeChoice::Light, "Light"),
                (ThemeChoice::Dark, "Dark"),
            ],
        );
        if chosen != current {
            *action = Some(Action::Theme(chosen));
        }
    });
}

/// The form's fields other than the folders and the visual check, as the defaults of new
/// jobs, and the way back to the built-in ones (disabled while they are those).
fn new_jobs(ui: &mut egui::Ui, defaults: &FormDefaults, action: &mut Option<Action>) {
    widgets::card(ui, "New jobs start with", None, |ui| {
        let mut edited = defaults.clone();
        subfolders(ui, &mut edited.recursive);
        ui.add_space(4.0);
        output_fields(ui, &mut edited);
        ui.add_space(4.0);
        grouping_fields(ui, &mut edited, None);
        ui.add_space(4.0);
        let built_in = FormDefaults::default();
        let changed = *defaults != built_in;
        if widgets::text_button_enabled(ui, changed, RESET, Emphasis::Normal).clicked() {
            edited = built_in;
        }
        if edited != *defaults {
            *action = Some(Action::Defaults(edited));
        }
    });
}

/// A part of the app that is not MIT: its licence, and where its source is.
struct Component {
    name: &'static str,
    licence: &'static str,
    /// The addresses of its source, one a line.
    sources: Vec<&'static str>,
}

/// The components this build holds that are not MIT, or not ours: rawler always,
/// libheif and libde265 with `heic`, ONNX Runtime and the DINOv2 models with `ml`, and
/// the typeface. Each source is its upstream repository (the DINOv2 models' from the
/// model registry), plain text the user can select and copy: the app opens no browser.
fn components() -> Vec<Component> {
    let mut components = vec![Component {
        name: "rawler",
        licence: "LGPL-2.1",
        sources: vec!["https://github.com/dnglab/dnglab"],
    }];
    #[cfg(feature = "heic")]
    components.push(Component {
        name: "libheif and libde265",
        licence: "LGPL-3.0",
        sources: vec![
            "https://github.com/strukturag/libheif",
            "https://github.com/strukturag/libde265",
        ],
    });
    #[cfg(feature = "ml")]
    {
        components.push(Component {
            name: "ONNX Runtime",
            licence: "MIT",
            sources: vec!["https://github.com/microsoft/onnxruntime"],
        });
        // Each model's repository: its download address up to the pinned revision.
        let repositories = photostacker_core::models::MODELS
            .iter()
            .filter_map(|model| model.urls.first())
            .filter_map(|url| url.split("/resolve/").next())
            .collect();
        components.push(Component {
            name: "DINOv2 models",
            licence: "Apache-2.0",
            sources: repositories,
        });
    }
    components.push(Component {
        name: "Inter",
        licence: "OFL-1.1",
        sources: vec!["https://github.com/rsms/inter"],
    });
    components
}

/// The version, the licence, the components that are not MIT, the way to the licence
/// texts in a package (`packaged`: the app has the folder of its licence texts), the
/// HEVC caveat in a build that decodes HEIC, and the logs folder.
fn about(ui: &mut egui::Ui, packaged: bool, action: &mut Option<Action>) {
    let palette = theme::palette(ui.ctx());
    let semibold = egui::FontFamily::Name(SEMIBOLD_FAMILY.into());
    widgets::card(ui, "About", None, |ui| {
        let version = format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
        ui.label(RichText::new(version).family(semibold.clone()));
        ui.add(Label::new(LICENCE).wrap());
        for component in components() {
            ui.add_space(6.0);
            // A component's lines together, apart from the next one's.
            ui.vertical(|ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(component.name).family(semibold.clone()));
                    ui.label(RichText::new(component.licence).color(palette.secondary));
                });
                for source in component.sources {
                    let source = RichText::new(source).color(palette.secondary);
                    ui.add(Label::new(source).wrap().selectable(true));
                }
            });
        }
        ui.add_space(6.0);
        // A development build has no folder of licence texts, so it says nothing of one.
        if packaged {
            ui.add(Label::new(LICENCES_SHIP).wrap());
            ui.add_space(4.0);
            if widgets::text_button(ui, "Open the licences folder", Emphasis::Normal).clicked() {
                *action = Some(Action::OpenLicences);
            }
            ui.add_space(6.0);
        }
        #[cfg(feature = "heic")]
        ui.add(Label::new(HEVC).wrap());
        ui.add_space(4.0);
        if widgets::text_button(ui, "Open the logs folder", Emphasis::Normal).clicked() {
            *action = Some(Action::OpenLogs);
        }
    });
}
