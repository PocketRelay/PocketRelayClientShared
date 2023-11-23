pub mod servers;
pub mod update;

/// Version constant for the backend
pub const SHARED_BACKEND_VERSION: &str = env!("CARGO_PKG_VERSION");
