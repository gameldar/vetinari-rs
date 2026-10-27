//! OAuth flows.

pub mod device_code;
mod loopback;
pub mod oidc;

pub use device_code::HttpDeviceCodeFlow;
pub use oidc::OidcDeviceCodeFlow;
