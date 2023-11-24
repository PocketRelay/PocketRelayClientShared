//! Server connected to by BlazeSDK clients (Majority of the game traffic)

use super::{spawn_server_task, BLAZE_PORT};
use crate::api::create_server_stream;
use log::{debug, error};
use std::{net::Ipv4Addr, sync::Arc};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
};
use url::Url;

/// Starts the blaze server
///
/// ## Arguments
/// * http_client - The HTTP client passed around for connection upgrades
/// * base_url    - The server base URL to connect clients to
pub async fn start_blaze_server(
    http_client: reqwest::Client,
    base_url: Arc<Url>,
) -> std::io::Result<()> {
    // Bind the local socket for accepting connections
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, BLAZE_PORT)).await?;

    // Accept connections
    loop {
        let (client_stream, _) = listener.accept().await?;

        spawn_server_task(handle(client_stream, http_client.clone(), base_url.clone()))
    }
}

/// Handler for processing BlazeSDK client connections
///
/// ## Arguments
/// * client_stream - The client stream to read and write from
/// * http_client   - The HTTP client passed around for connection upgrades
/// * base_url      - The server base URL to connect clients to
async fn handle(mut client_stream: TcpStream, http_client: reqwest::Client, base_url: Arc<Url>) {
    debug!("Starting blaze connection");

    // Create a stream to the Pocket Relay server
    let mut server_stream = match create_server_stream(http_client, &base_url).await {
        Ok(stream) => stream,
        Err(err) => {
            error!("Failed to create server stream: {}", err);
            return;
        }
    };

    debug!("Blaze connection linked");

    // Copy the data between the streams
    let _ = copy_bidirectional(&mut client_stream, &mut server_stream).await;
}
