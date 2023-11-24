use crate::{servers::HTTP_PORT, MIN_SERVER_VERSION};
use hyper::{
    header::{self, HeaderName, HeaderValue},
    Body, HeaderMap, Response,
};
use log::error;
use reqwest::{Client, Identity, Upgraded};
use semver::Version;
use serde::{Deserialize, Serialize};
use std::{path::Path, str::FromStr, sync::Arc};
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

/// Creates a new HTTP client to use, will use the client identity
/// if one is provided
///
/// ## Arguments
/// * identity - Optional identity for the client to use
pub fn create_http_client(identity: Option<Identity>) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder().user_agent(USER_AGENT);

    if let Some(identity) = identity {
        builder = builder.identity(identity);
    }

    builder.build()
}

#[derive(Debug, Error)]
pub enum ClientIdentityError {
    #[error("Failed to read identity: {0}")]
    Read(#[from] std::io::Error),
    #[error("Failed to create identity: {0}")]
    Create(#[from] reqwest::Error),
}

/// Attempts to read a client identity from the provided file path,
/// the file must be a .p12 / .pfx (PKCS12) format containing a
/// certificate and private key with a blank password
///
/// ## Arguments
/// * path - The path to read the identity from
pub fn read_client_identity(path: &Path) -> Result<Identity, ClientIdentityError> {
    // Read the identity file bytes
    let bytes = std::fs::read(path).map_err(ClientIdentityError::Read)?;

    // Parse the identity from the file bytes
    Identity::from_pkcs12_der(&bytes, "").map_err(ClientIdentityError::Create)
}

/// Details provided by the server. These are the only fields
/// that we need the rest are ignored by this client.
#[derive(Deserialize)]
struct ServerDetails {
    /// The Pocket Relay version of the server
    version: Version,
    /// Server identifier checked to ensure its a proper server
    #[serde(default)]
    ident: Option<String>,
}

/// Data from completing a lookup contains the resolved address
/// from the connection to the server as well as the server
/// version obtained from the server
#[derive(Debug, Clone)]
pub struct LookupData {
    /// Server url
    pub url: Arc<Url>,
    /// The server version
    pub version: Version,
}

/// Errors that can occur while looking up a server
#[derive(Debug, Error)]
pub enum LookupError {
    /// The server url was invalid
    #[error("Invalid Connection URL: {0}")]
    InvalidHostTarget(#[from] url::ParseError),
    /// The server connection failed
    #[error("Failed to connect to server: {0}")]
    ConnectionFailed(reqwest::Error),
    /// The server gave an invalid response likely not a PR server
    #[error("Server replied with error response: {0}")]
    ErrorResponse(reqwest::Error),
    /// The server gave an invalid response likely not a PR server
    #[error("Invalid server response: {0}")]
    InvalidResponse(reqwest::Error),
    /// Server wasn't a valid pocket relay server
    #[error("Server identifier was incorrect (Not a PocketRelay server?)")]
    NotPocketRelay,
    /// Server version is too old
    #[error("Server version is too outdated ({0}) this client requires servers of version {1} or greater")]
    ServerOutdated(Version, Version),
}

/// Attempts to lookup a server at the provided url to see if
/// its a Pocket Relay server
pub async fn lookup_server(
    http_client: reqwest::Client,
    host: String,
) -> Result<LookupData, LookupError> {
    let mut url = String::new();

    // Whether a scheme was inferred
    let mut inserted_scheme = false;

    // Fill in missing scheme portion
    if !host.starts_with("http://") && !host.starts_with("https://") {
        url.push_str("http://");

        inserted_scheme = true;
    }

    url.push_str(&host);

    // Ensure theres a trailing slash (URL path will be interpeted incorrectly without)
    if !url.ends_with('/') {
        url.push('/');
    }

    let mut url = Url::from_str(&url)?;

    // Update scheme to be https if the 443 port was specified and the scheme was inferred as http://
    if url.port().is_some_and(|port| port == 443) && inserted_scheme {
        let _ = url.set_scheme("https");
    }

    let info_url = url
        .join(DETAILS_ENDPOINT)
        .expect("Failed to create server details URL");

    let response = http_client
        .get(info_url)
        .header(header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(LookupError::ConnectionFailed)?;

    #[cfg(debug_assertions)]
    {
        use log::debug;

        debug!("Response Status: {}", response.status());
        debug!("HTTP Version: {:?}", response.version());
        debug!("Content Length: {:?}", response.content_length());
        debug!("HTTP Headers: {:?}", response.headers());
    }

    let response = response
        .error_for_status()
        .map_err(LookupError::ErrorResponse)?;

    let details = response
        .json::<ServerDetails>()
        .await
        .map_err(LookupError::InvalidResponse)?;

    // Handle invalid server ident
    if details.ident.is_none() || details.ident.is_some_and(|value| value != SERVER_IDENT) {
        return Err(LookupError::NotPocketRelay);
    }

    // Ensure the server is a supported version
    if details.version < MIN_SERVER_VERSION {
        return Err(LookupError::ServerOutdated(
            details.version,
            MIN_SERVER_VERSION,
        ));
    }

    Ok(LookupData {
        url: Arc::new(url),
        version: details.version,
    })
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

/// Key value pair message for telemetry events
#[derive(Serialize)]
pub struct TelemetryEvent {
    /// The telemetry values
    pub values: Vec<(String, String)>,
}

/// Publishes a new telemetry event to the Pocket Relay server
///
/// ## Arguments
/// * http_client - The HTTP client to connect with
/// * base_url    - The server base URL (Connection URL)
/// * event       - The event to publish
pub async fn publish_telemetry_event(
    http_client: &reqwest::Client,
    base_url: &Url,
    event: TelemetryEvent,
) -> Result<(), reqwest::Error> {
    // Create the upgrade endpoint URL
    let endpoint_url: Url = base_url
        .join(UPGRADE_ENDPOINT)
        .expect("Failed to create upgrade endpoint");

    // Send the HTTP request and get its response
    let response = http_client.post(endpoint_url).json(&event).send().await?;

    // Handle server error responses
    let _ = response.error_for_status()?;

    Ok(())
}

/// Errors that could occur when proxying a request
#[derive(Debug, Error)]
pub enum ProxyError {
    /// Initial HTTP request failure
    #[error("Request failed: {0}")]
    RequestFailed(reqwest::Error),
    /// Failed to read the response body bytes
    #[error("Request failed: {0}")]
    BodyFailed(reqwest::Error),
}

/// Proxies an HTTP request to the Pocket Relay server returning a
/// hyper response that can be served
///
/// ## Arguments
/// * http_client - The HTTP client to connect with
/// * url         - The server URL to request
pub async fn proxy_http_request(
    http_client: &reqwest::Client,
    url: Url,
) -> Result<Response<Body>, ProxyError> {
    // Send the HTTP request and get its response
    let response = http_client
        .get(url)
        .send()
        .await
        .map_err(ProxyError::RequestFailed)?;

    // Extract response status and headers before its consumed to load the body
    let status = response.status();
    let headers = response.headers().clone();

    // Read the response body bytes
    let body: bytes::Bytes = response.bytes().await.map_err(ProxyError::BodyFailed)?;

    // Create new response from the proxy response
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;

    Ok(response)
}
