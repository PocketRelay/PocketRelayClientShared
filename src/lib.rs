pub use reqwest;
pub use semver::Version;
pub use url::Url;

pub mod api;
pub mod blaze;
pub mod servers;
pub mod update;

/// Version constant for the backend
pub const SHARED_BACKEND_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The minimum server version supported by this client
pub const MIN_SERVER_VERSION: Version = Version::new(0, 5, 0);
