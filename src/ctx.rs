//! Shared context state that the app should store and pass to the
//! various servers when they are started

use url::Url;

/// Shared context
pub struct ClientContext {
    /// HTTP client for the client to make requests with
    pub http_client: reqwest::Client,
    /// Base URL of the connected server
    pub base_url: Url,
    /// Optional association token
    pub association: Option<String>,
}
