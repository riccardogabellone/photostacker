//! The desktop app. `app` lays out the window (`failed_start` is the one a start that
//! failed opens instead) and `views` draw its regions from what
//! `controller` holds; `controller` reaches the engine only through `backend`'s thread,
//! `thumbs` loads the review thumbnails on a thread of its own, and `inspect` the
//! inspector's previews and stack predictions on another. The modules below
//! them hold what every view shares: the theme, the widgets, the app's identity, the
//! renderer, the formatting of paths, counts, statuses and errors, the preferences, the
//! sizes the window remembers (`sizes`), the desktop's dialogs and the app's log files
//! (`logging`).

pub mod app;
pub mod backend;
pub mod controller;
pub mod failed_start;
pub mod format;
pub mod identity;
pub mod inspect;
pub mod logging;
pub mod platform;
pub mod prefs;
pub mod renderer;
mod sizes;
pub mod theme;
pub mod thumbs;
pub mod views;
pub mod wake;
pub mod widgets;
