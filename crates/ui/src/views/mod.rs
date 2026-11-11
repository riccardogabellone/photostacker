//! The views: each draws one region of the window from what the controller holds, and
//! reports what the user did by calling it. No view calls the engine or touches a file.

pub mod fields;
pub mod inspector;
pub mod job;
pub mod jobs;
pub mod models;
pub mod new_job;
pub mod photo;
pub mod results;
pub mod review;
pub mod settings;
pub mod status;
pub mod toolbar;
pub mod tray;
