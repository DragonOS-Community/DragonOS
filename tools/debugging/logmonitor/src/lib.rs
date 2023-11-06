#[macro_use]
extern crate clap;

/// Application.
pub mod app;

/// Terminal events handler.
pub mod event;

/// Widget renderer.
pub mod ui;

/// Terminal user interface.
pub mod tui;

pub mod backend;
pub mod command;
/// Event handler.
pub mod handler;
pub mod logging;

