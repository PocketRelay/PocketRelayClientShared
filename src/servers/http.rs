//! HTTP server for safely forwarding HTTP requests that the client
//! makes along to the Pocket Relay server, since the game client
//! is only capable of communicating over SSLv3

use super::HTTP_PORT;
use crate::api::proxy_http_request;
use hyper::{
    http::uri::PathAndQuery,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use log::error;
use std::{
    convert::Infallible,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};
use url::Url;

/// Starts the HTTP proxy server
///
/// ## Arguments
/// * `http_client` - The HTTP client passed around for sending the requests
/// * `base_url`    - The server base URL to proxy requests to
pub async fn start_http_server(
    http_client: reqwest::Client,
    base_url: Arc<Url>,
) -> std::io::Result<()> {
    // Create the socket address the server will bind too
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, HTTP_PORT));

    // Create service that uses the `handle function`
    let make_svc = make_service_fn(move |_conn| {
        let http_client = http_client.clone();
        let base_url = base_url.clone();

        async move {
            // service_fn converts our function into a `Service`
            Ok::<_, Infallible>(service_fn(move |request| {
                handle(request, http_client.clone(), base_url.clone())
            }))
        }
    });

    let server = Server::bind(&addr).serve(make_svc);

    server
        .await
        .map_err(|err| std::io::Error::new(ErrorKind::Other, err))
}

/// Handles an HTTP request from the HTTP server proxying it along
/// to the Pocket Relay server
///
/// ## Arguments
/// * `request`     - The HTTP request
/// * `http_client` - The HTTP client to proxy the request with
/// * `base_url`    - The server base URL (Connection URL)
async fn handle(
    request: Request<Body>,
    http_client: reqwest::Client,
    base_url: Arc<Url>,
) -> Result<Response<Body>, Infallible> {
    let path_and_query = request
        .uri()
        // Extract the path and query portion of the url
        .path_and_query()
        // Convert the path to a &str
        .map(PathAndQuery::as_str)
        // Fallback to empty path if none is provided
        .unwrap_or_default();

    // Strip the leading slash if one is present
    let path_and_query = path_and_query.strip_prefix('/').unwrap_or(path_and_query);

    // Create the new url from the path
    let url = match base_url.join(path_and_query) {
        Ok(value) => value,
        Err(err) => {
            error!("Failed to create HTTP proxy URL: {}", err);

            let mut response = Response::default();
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            return Ok(response);
        }
    };

    // Proxy the request to the server
    let response = match proxy_http_request(&http_client, url).await {
        Ok(value) => value,
        Err(err) => {
            error!("Failed to proxy HTTP request: {}", err);

            let mut response = Response::default();
            *response.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            return Ok(response);
        }
    };

    Ok(response)
}
