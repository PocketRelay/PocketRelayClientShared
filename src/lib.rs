#![warn(missing_docs, unused_variables, unused_crate_dependencies)]

//! Shared core for Pocket Relay client
//!
//! This library handles creating and running the local servers required
//! for connecting to Pocket Relay servers.
//!
//! It provides shared backend for the different variants to make it easier
//! to keep feature parody across versions
//!
//! [`PocketRelay`]: https://pocket-relay.pages.dev/

// Re-exports for dependencies
pub use reqwest;
pub use semver::Version;
pub use url::Url;

pub mod api;
pub mod fire;
pub mod servers;
pub mod update;

/// Version constant for the backend
pub const SHARED_BACKEND_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The minimum server version supported by this client
pub const MIN_SERVER_VERSION: Version = Version::new(0, 5, 0);
