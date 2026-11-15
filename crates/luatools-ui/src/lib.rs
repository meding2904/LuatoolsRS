//! egui/eframe user interface: theme, widgets, and pages.
//!
//! The UI is a pure function of [`state::AppState`]: it renders, sends
//! [`luatools_core::Command`]s, and folds [`luatools_core::Event`]s. It never performs I/O, so
//! every screen is testable with a disconnected backend.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod app;
pub mod pages;
pub mod state;
pub mod theme;
pub mod widgets;

pub use app::{LuaToolsApp, confirm};
pub use pages::Page;
pub use state::{AppState, ToastItem};
pub use theme::{Theme, Tone};
