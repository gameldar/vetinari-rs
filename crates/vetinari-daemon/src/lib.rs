//! `vetinarid`: the daemon that claims dev workflow steps from the
//! autognomus engine, runs them on this machine, and serves their progress
//! to the TUI over a Unix socket.
//!
//! It is the only crate that depends on all the others: `compose` is its
//! composition root.

pub mod compose;
pub mod hub;
pub mod runtime;
pub mod scripted;
pub mod socket;
