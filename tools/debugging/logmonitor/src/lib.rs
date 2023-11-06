#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

extern crate clap;

extern crate lazy_static;

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
pub mod constant;
/// Event handler.
pub mod handler;
pub mod logging;
