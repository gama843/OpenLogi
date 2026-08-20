//! OS input-event synthesis split out of openlogi-core so the core stays platform- and IO-free.

mod inject;
mod magnify;

pub use inject::{SYNTHETIC_EVENT_USER_DATA, ax_navigate_browser, execute, post_horizontal_scroll};
pub use magnify::post_magnification;

#[cfg(target_os = "linux")]
pub use inject::action_device_path;
