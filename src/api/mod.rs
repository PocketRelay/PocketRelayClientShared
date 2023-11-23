use crate::servers::HTTP_PORT;
use hyper::{
    header::{self, HeaderName, HeaderValue},
    HeaderMap,
};
use reqwest::Upgraded;
use thiserror::Error;
use url::Url;

/// Endpoint used for requesting the server details
pub const DETAILS_ENDPOINT: &str = "api/server";
/// Endpoint used to publish telemetry events
pub const TELEMETRY_ENDPOINT: &str = "api/server/telemetry";
/// Endpoint for upgrading the server connection
pub const UPGRADE_ENDPOINT: &str = "api/server/upgrade";

/// Server identifier for validation
pub const SERVER_IDENT: &str = "POCKET_RELAY_SERVER";

/// Client user agent created from the name and version
pub const USER_AGENT: &str = concat!("PocketRelayClient/v", env!("CARGO_PKG_VERSION"));

// Headers used by the client
mod headers {
    /// Legacy header used to derive the server scheme (Exists only for backwards compatability)
    pub const LEGACY_SCHEME: &str = "x-pocket-relay-scheme";
    /// Legacy header used to derive the server host (Exists only for backwards compatability)
    pub const LEGACY_HOST: &str = "x-pocket-relay-host";
    /// Legacy header used to derive the server port (Exists only for backwards compatability)
    pub const LEGACY_PORT: &str = "x-pocket-relay-port";
    /// Legacy header telling the server to use local http routing
    /// (Existing only for backwards compat, this is the default behavior for newer versions)
    pub const LEGACY_LOCAL_HTTP: &str = "x-pocket-relay-local-http";
}

/// Errors that could occur when creating a server stream
#[derive(Debug, Error)]
pub enum ServerStreamError {
    /// Initial HTTP request failure
    #[error("Request failed: {0}")]
    RequestFailed(reqwest::Error),
    /// Server responded with an error message
    #[error("Server error response: {0}")]
    ServerError(reqwest::Error),
    /// Upgrading the connection failed
    #[error("Upgrade failed: {0}")]
    UpgradeFailure(reqwest::Error),
}

/// Creates a BlazeSDK upgraded stream using HTTP upgrades
/// with the Pocket Relay server
///
/// ## Arguments
/// * http_client - The HTTP client to connect with
/// * base_url    - The server base URL (Connection URL)
pub async fn create_server_stream(
    http_client: reqwest::Client,
    base_url: &Url,
) -> Result<Upgraded, ServerStreamError> {
    // Create the upgrade endpoint URL
    let endpoint_url: Url = base_url
        .join(UPGRADE_ENDPOINT)
        .expect("Failed to create upgrade endpoint");

    // Headers to provide when upgrading
    let headers: HeaderMap<HeaderValue> = [
        (header::USER_AGENT, HeaderValue::from_static(USER_AGENT)),
        (header::CONNECTION, HeaderValue::from_static("Upgrade")),
        (header::UPGRADE, HeaderValue::from_static("blaze")),
        // Headers for legacy compatability
        (
            HeaderName::from_static(headers::LEGACY_SCHEME),
            HeaderValue::from_static("http"),
        ),
        (
            HeaderName::from_static(headers::LEGACY_HOST),
            HeaderValue::from_static("127.0.0.1"),
        ),
        (
            HeaderName::from_static(headers::LEGACY_PORT),
            HeaderValue::from(HTTP_PORT),
        ),
        (
            HeaderName::from_static(headers::LEGACY_LOCAL_HTTP),
            HeaderValue::from_static("true"),
        ),
    ]
    .into_iter()
    .collect();

    // Send the HTTP request and get its response
    let response = http_client
        .get(endpoint_url)
        .headers(headers)
        .send()
        .await
        .map_err(ServerStreamError::RequestFailed)?;

    // Handle server error responses
    let response = response
        .error_for_status()
        .map_err(ServerStreamError::ServerError)?;

    // Upgrade the connection
    response
        .upgrade()
        .await
        .map_err(ServerStreamError::UpgradeFailure)
}
