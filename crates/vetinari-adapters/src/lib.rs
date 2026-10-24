//! Infrastructure adapters for Vetinari.
//!
//! Every module here implements a port from `vetinari_core::application::ports`
//! against something real: a process, a socket, an HTTP API, or the OS
//! keychain. Nothing in this crate is depended upon by the core.

pub mod agent;
pub mod autognomus;
pub mod clock;
pub mod config;
pub mod credentials;
pub mod devflow;
pub mod git;
pub mod oauth;
pub mod runtime;
pub mod skills;
pub mod sources;
pub mod workspace;

#[cfg(test)]
mod testing;

mod http;

pub use http::HttpClient;
pub use runtime::{Paths, Runtime};
