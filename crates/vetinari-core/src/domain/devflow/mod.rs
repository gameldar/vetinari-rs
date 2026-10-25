//! Dev workflows: multi-step templates such as `FeatureFromTicket`, run on
//! the autognomus workflow engine.
//!
//! Everything here is pure. Identifiers are parsed once at the boundary, a
//! template compiles to a graph the engine will register, and a step's result
//! is parsed from bytes the adapter read. No clock is read and no I/O is done.

pub mod compile;
#[cfg(test)]
mod engine_rules;
pub mod held;
pub mod ids;
pub mod outcome;
pub mod result;
pub mod run_view;
pub mod step;
pub mod task;
pub mod template;
