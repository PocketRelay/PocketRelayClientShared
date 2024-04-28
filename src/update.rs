//! Code for assisting with the updating process

use bytes::Bytes;
use hyper::header;
use serde::Deserialize;

/// Structure for the required portions of github releases
#[derive(Deserialize)]
pub struct GitHubRelease {
    /// The URL for viewing the release in the browser
    pub html_url: String,
    /// The release tag / version
    pub tag_name: String,
    /// The name of the release (Usually the same as tag_name)
    pub name: String,
    /// The date & time the release was published
    pub published_at: String,
    /// The release assets
    pub assets: Vec<GitHubReleaseAsset>,
}

/// Represents an asset from github releases that can be downloaded
#[derive(Deserialize)]
pub struct GitHubReleaseAsset {
    /// The name of the file
    pub name: String,
    /// URL for downloading the file
    pub browser_download_url: String,
}

/// Attempts to obtain the latest release from github
///
/// ## Arguments
/// * `http_client` - The HTTP client to make the request with
/// * `repository`  - The repository to get the latest release for (e.g "PocketRelay/Client")
pub async fn get_latest_release(
    http_client: &reqwest::Client,
    repository: &str,
) -> Result<GitHubRelease, reqwest::Error> {
    let url = format!(
        "https://api.github.com/repos/{}/releases/latest",
        repository
    );

    http_client
        .get(url)
        .header(header::ACCEPT, "application/json")
        .send()
        .await?
        .json()
        .await
}

/// Downloads the provided github release asset returning the
/// downloaded bytes
///
/// ## Arguments
/// * `http_client` - The HTTP client to make the request with
/// * `asset`       - The asset to download
pub async fn download_latest_release(
    http_client: &reqwest::Client,
    asset: &GitHubReleaseAsset,
) -> Result<Bytes, reqwest::Error> {
    http_client
        .get(&asset.browser_download_url)
        .send()
        .await?
        .bytes()
        .await
}
