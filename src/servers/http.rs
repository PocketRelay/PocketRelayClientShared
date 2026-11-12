//! HTTP server for safely forwarding HTTP requests that the client
//! makes along to the Pocket Relay server, since the game client
//! is only capable of communicating over SSLv3

use super::HTTP_PORT;
use crate::{api::proxy_http_request, ctx::ClientContext};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{
    body::Incoming, http::uri::PathAndQuery, server::conn::http1, service::service_fn, Request,
    Response, StatusCode,
};
use hyper_util::rt::TokioIo;
use log::error;
use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};
use tokio::net::TcpListener;

/// Starts the HTTP proxy server
///
/// ## Arguments
/// * `ctx` - The client context
pub async fn start_http_server(ctx: Arc<ClientContext>) -> std::io::Result<()> {
    // Create the socket address the server will bind too
    let addr = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, HTTP_PORT));
    let listener = TcpListener::bind(addr).await?;

    loop {
        let (stream, _) = listener.accept().await?;

        let io = TokioIo::new(stream);
        let ctx = ctx.clone();

        tokio::task::spawn(async move {
            if let Err(err) = http1::Builder::new()
                .serve_connection(io, service_fn(move |request| handle(request, ctx.clone())))
                .await
            {
                eprintln!("Error serving connection: {:?}", err);
            }
        });
    }
}

/// Handles an HTTP request from the HTTP acting as a proxy
/// for the Pocket Relay server
///
/// ## Arguments
/// * `request` - The HTTP request
/// * `ctx`     - The client context
async fn handle(
    request: Request<Incoming>,
    ctx: Arc<ClientContext>,
) -> Result<Response<Full<Bytes>>, Infallible> {
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
    let url = match ctx.base_url.join(path_and_query) {
        Ok(value) => value,
        Err(err) => {
            error!("Failed to create HTTP proxy URL: {}", err);

            let mut response = Response::default();
            *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
            return Ok(response);
        }
    };

    // Proxy the request to the server
    let response = match proxy_http_request(&ctx.http_client, url).await {
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
