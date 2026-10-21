//! Domain and application layer for Vetinari.
//!
//! This crate holds the pure core: domain types, the ports (traits) that
//! describe what the outside world must provide, and the application services
//! that orchestrate them. It performs no I/O of its own.

pub mod application;
pub mod domain;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
