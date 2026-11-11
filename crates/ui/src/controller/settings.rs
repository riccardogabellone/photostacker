//! Settings (spec §9.2): opening and closing them over the centre, and the preferences
//! they and the window change. Each setter keeps its value at once and saves its key
//! through the backend as a [`Request::SavePref`], which a newer save of the same key
//! replaces while it waits, so a slider dragged across its rail writes the database a
//! few times, not once a frame. The window's size and the panels' are only handed here
//! once they have settled (`App`). The models of Stage 3 are `controller::models`'.

use std::path::Path;

use photostacker_core::engine::JobId;
use photostacker_core::models;
use serde::Serialize;

use super::{Controller, Reveal, View};
use crate::backend::Request;
use crate::prefs::{self, FormDefaults, Panels, ThemeChoice};

impl Controller {
    /// Shows Settings over what the centre shows; nothing when they are shown already.
    pub fn open_settings(&mut self) {
        if self.view != View::Settings {
            self.behind_settings = self.view;
            self.view = View::Settings;
        }
    }

    /// Shows again what Settings were opened over: the selected job, or the new-job form
    /// as it was left. A form that is gone meanwhile (its job was made) leaves the job.
    pub fn close_settings(&mut self) {
        if self.view != View::Settings {
            return;
        }
        self.view = match self.behind_settings {
            View::NewJob if self.form.is_none() => View::Job,
            behind => behind,
        };
    }

    /// Takes `theme` as the user's choice at once, and saves it.
    pub fn set_theme(&mut self, theme: ThemeChoice) {
        self.prefs.theme = theme;
        self.save_pref(prefs::THEME, theme);
    }

    /// Takes `defaults` as what new jobs start with, at once, and saves them. A form
    /// already open keeps what it holds.
    pub fn set_defaults(&mut self, defaults: FormDefaults) {
        self.prefs.defaults = defaults.clone();
        self.save_pref(prefs::FORM_DEFAULTS, defaults);
    }

    /// Takes `size` as the window's inner size, in points, and saves it.
    pub fn set_window(&mut self, size: [f32; 2]) {
        self.prefs.window = Some(size);
        self.save_pref(prefs::WINDOW, size);
    }

    /// Takes `panels` as the panels' sizes, and saves them.
    pub fn set_panels(&mut self, panels: Panels) {
        self.prefs.panels = panels;
        self.save_pref(prefs::PANELS, panels);
    }

    /// Turns the visual check of new jobs on or off, and saves it. It downloads nothing.
    pub fn set_ml_enabled(&mut self, on: bool) {
        self.prefs.ml_enabled = on;
        self.save_pref(prefs::ML_ENABLED, on);
    }

    /// Makes the registry's model `id` the active one, and saves it; another id is ignored.
    pub fn set_ml_model(&mut self, id: &str) {
        if let Some(spec) = models::model(id) {
            self.prefs.ml_model = spec.id.to_owned();
            self.save_pref(prefs::ML_MODEL, spec.id);
        }
    }

    /// Shows the folder of the app's log files in the file manager; a folder that is not
    /// there is a warning ("<path> does not exist.").
    pub fn open_logs(&mut self) {
        let logs = self.logs_dir.clone();
        self.reveal(&logs, Reveal::Folder);
    }

    /// The folder of the licence texts the package ships; `None` where there is none, as
    /// in a development build.
    pub fn licences_folder(&self) -> Option<&Path> {
        self.licences_dir.as_deref()
    }

    /// Shows the folder of the package's licence texts in the file manager; nothing where
    /// there is none. One gone since the start is a warning ("<path> does not exist.").
    pub fn open_licences(&mut self) {
        if let Some(licences) = self.licences_dir.clone() {
            self.reveal(&licences, Reveal::Folder);
        }
    }

    /// Saves `value` as the preference `key`, replacing a save of it still waiting.
    pub(super) fn save_pref<T: Serialize + Send + 'static>(&self, key: &'static str, value: T) {
        self.send(Request::SavePref {
            key,
            save: Box::new(move |engine| engine.set_pref(key, &value)),
        });
    }

    /// The view the centre shows, or, while Settings cover it, the one they go back to.
    pub(super) fn view_behind_settings(&self) -> View {
        match self.view {
            View::Settings => self.behind_settings,
            view => view,
        }
    }

    /// Selects job `id`, which a submission of the form made, and shows it; while
    /// Settings are open, closing them shows it.
    pub(super) fn select_submitted_job(&mut self, id: JobId) {
        if self.view == View::Settings {
            self.behind_settings = View::Job;
            self.choose_job(id);
        } else {
            self.select_job(id);
        }
    }
}
