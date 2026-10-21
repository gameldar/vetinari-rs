//! Ratatui front-end for Vetinari.
//!
//! The front-end is split so that almost all of it can be tested without a
//! terminal: [`app::App`] is the state and the decisions, [`event`] turns
//! terminal keys into domain chords, and `ui` draws a frame from the state.
//! Only the event loop in the binary touches the real terminal.

pub mod app;
pub mod controller;
pub mod event;
pub mod overlay;
pub mod startup;
pub mod text_input;
pub mod ui;
