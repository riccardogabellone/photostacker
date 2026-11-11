//! The app's preferences (spec §7.3, §9.2): one key of the engine's `prefs` table per
//! field, each read alone, so a value that is missing or cannot be read is that field's
//! default and never voids the others.

use std::path::PathBuf;

use eframe::egui::ThemePreference;
use photostacker_core::model::{OutputFormat, RawJpegPreference, RawTone};
use photostacker_core::models::MODELS;
use photostacker_core::{Engine, JobConfig};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The preference keys.
pub(crate) const THEME: &str = "theme";
pub(crate) const WINDOW: &str = "window";
pub(crate) const PANELS: &str = "panels";
pub(crate) const FORM_DEFAULTS: &str = "form_defaults";
pub(crate) const ML_ENABLED: &str = "ml_enabled";
pub(crate) const ML_MODEL: &str = "ml_model";
pub(crate) const LAST_INPUT_DIR: &str = "last_input_dir";

/// The theme the user chose.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Follow the system's light or dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl From<ThemeChoice> for ThemePreference {
    fn from(choice: ThemeChoice) -> Self {
        match choice {
            ThemeChoice::System => ThemePreference::System,
            ThemeChoice::Light => ThemePreference::Light,
            ThemeChoice::Dark => ThemePreference::Dark,
        }
    }
}

/// The widths of the side panels and the height of the tray, in points: the sizes the user
/// chose, which the window draws when it is large enough. A value saved without one of
/// them takes that one's default.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Panels {
    pub jobs: f32,
    pub inspector: f32,
    /// `None` until the user resizes the tray, which then has a default height that gives
    /// way on a short window (`views::tray`); its height once resized, at any value.
    pub tray: Option<f32>,
}

impl Default for Panels {
    fn default() -> Self {
        Self {
            jobs: 260.0,
            inspector: 320.0,
            tray: None,
        }
    }
}

/// What the new-job form starts with: the fields of [`JobConfig`] that are not a folder
/// or the model, which the form then holds as its own (`JobForm::fields`). A value saved
/// without one of them takes that one's default.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FormDefaults {
    pub output_format: OutputFormat,
    pub recursive: bool,
    pub auto_confirm: bool,
    pub hamming_threshold: u32,
    pub time_window_seconds: u32,
    pub stage3_ambiguity_floor: u32,
    pub cosine_threshold: Option<f32>,
    pub raw_tone: RawTone,
    pub raw_jpeg_preference: RawJpegPreference,
}

impl Default for FormDefaults {
    /// The values of [`JobConfig::new`].
    fn default() -> Self {
        let config = JobConfig::new(PathBuf::new());
        Self {
            output_format: config.output_format,
            recursive: config.recursive,
            auto_confirm: config.auto_confirm,
            hamming_threshold: config.hamming_threshold,
            time_window_seconds: config.time_window_seconds,
            stage3_ambiguity_floor: config.stage3_ambiguity_floor,
            cosine_threshold: config.cosine_threshold,
            raw_tone: config.raw_tone,
            raw_jpeg_preference: config.raw_jpeg_preference,
        }
    }
}

/// Everything the app remembers between runs.
#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    pub theme: ThemeChoice,
    /// The window's inner size in points, the last it had while neither maximised,
    /// minimised nor in full screen; `None` until the window is first resized.
    pub window: Option<[f32; 2]>,
    pub panels: Panels,
    pub defaults: FormDefaults,
    /// Whether new jobs run Stage 3.
    pub ml_enabled: bool,
    /// The registry id of the model Stage 3 uses.
    pub ml_model: String,
    /// The last input folder picked, where the picker opens next.
    pub last_input_dir: Option<String>,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            theme: ThemeChoice::default(),
            window: None,
            panels: Panels::default(),
            defaults: FormDefaults::default(),
            ml_enabled: false,
            // The Stage-3 model a new job uses until the user picks another: the
            // registry's first.
            ml_model: MODELS[0].id.into(),
            last_input_dir: None,
        }
    }
}

impl Prefs {
    /// Reads every preference from `engine`'s database. Called once, at start, before the
    /// engine goes to the backend thread.
    pub fn load(engine: &Engine) -> Prefs {
        let defaults = Prefs::default();
        Prefs {
            theme: load(engine, THEME, defaults.theme),
            window: load(engine, WINDOW, defaults.window),
            panels: load(engine, PANELS, defaults.panels),
            defaults: load(engine, FORM_DEFAULTS, defaults.defaults),
            ml_enabled: load(engine, ML_ENABLED, defaults.ml_enabled),
            ml_model: load(engine, ML_MODEL, defaults.ml_model),
            last_input_dir: load(engine, LAST_INPUT_DIR, defaults.last_input_dir),
        }
    }
}

/// The preference `key`, or `default` when it was never set or cannot be read (such as a
/// value of another type, saved by another build), which is logged.
fn load<T: DeserializeOwned>(engine: &Engine, key: &str, default: T) -> T {
    match engine.pref(key) {
        Ok(Some(value)) => value,
        Ok(None) => default,
        Err(error) => {
            tracing::warn!(key, %error, "a preference cannot be read; its default is used");
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use photostacker_core::{AppDirs, EngineOptions};

    use super::*;

    #[test]
    fn a_theme_is_read_by_name_and_a_partial_value_keeps_the_missing_defaults() {
        let data = tempfile::tempdir().unwrap();
        let dirs = AppDirs::under(data.path().to_path_buf());
        let engine = Engine::start(EngineOptions {
            db: dirs.db(),
            thumbs_dir: dirs.thumbs(),
            models_dir: dirs.models(),
            embedder: None,
            installer: None,
            wake: Arc::new(|| {}),
        })
        .unwrap();
        // As an older build, which knew fewer fields, would have saved them.
        engine.set_pref(THEME, "dark").unwrap();
        engine
            .set_pref(PANELS, &BTreeMap::from([("jobs", 300.0)]))
            .unwrap();
        engine
            .set_pref(FORM_DEFAULTS, &BTreeMap::from([("recursive", true)]))
            .unwrap();

        let prefs = Prefs::load(&engine);
        assert_eq!(prefs.theme, ThemeChoice::Dark);
        assert_eq!(
            prefs.panels,
            Panels {
                jobs: 300.0,
                ..Panels::default()
            }
        );
        assert_eq!(
            prefs.defaults,
            FormDefaults {
                recursive: true,
                ..FormDefaults::default()
            }
        );
    }
}
