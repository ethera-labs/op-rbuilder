//! Sidecar callback integration for XT inclusion confirmation.

mod client;
mod config;
#[cfg(test)]
mod overrides;
mod types;

pub use client::SidecarClient;
pub use config::SidecarConfig;
pub use types::{ConfirmIncludedRequest, SidecarError};
