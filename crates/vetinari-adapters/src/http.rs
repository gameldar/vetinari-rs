//! Shared HTTP client for the API-backed adapters.

use std::time::Duration;

use vetinari_core::application::ports::{PortError, PortResult};

/// A reqwest client with Vetinari's defaults applied.
#[derive(Debug, Clone)]
pub struct HttpClient(reqwest::Client);

impl HttpClient {
    /// How long any single API call may take.
    pub const TIMEOUT: Duration = Duration::from_secs(20);

    /// Build a client.
    pub fn new() -> PortResult<Self> {
        reqwest::Client::builder()
            .timeout(Self::TIMEOUT)
            .user_agent(concat!("vetinari/", env!("CARGO_PKG_VERSION")))
            .build()
            .map(Self)
            .map_err(|error| PortError::unavailable("http client", error.to_string()))
    }

    /// The underlying client.
    #[must_use]
    pub const fn inner(&self) -> &reqwest::Client {
        &self.0
    }
}
