//! The settings menu (`prefix C`, `grofe settings`): every option, key and
//! layout in one fuzzy list, previewed live and saved to `settings.toml`.

pub mod apply;
pub mod keys;
pub mod layouts;
pub mod overlay;
mod rows;
mod sections;

pub use sections::{Section, section_of};
