//! Server connected to by BlazeSDK clients (Majority of the game traffic)

use super::{spawn_server_task, BLAZE_PORT};
use crate::{api::create_server_stream, ctx::ClientContext};
use log::{debug, error};
use std::{net::Ipv4Addr, sync::Arc};
use tokio::{
    io::copy_bidirectional,
    net::{TcpListener, TcpStream},
};

/// Starts the blaze server
///
/// ## Arguments
/// * `ctx` - The client context
pub async fn start_blaze_server(ctx: Arc<ClientContext>) -> std::io::Result<()> {
    // Bind the local socket for accepting connections
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, BLAZE_PORT)).await?;

    // Accept connections
    loop {
        let (client_stream, _) = listener.accept().await?;

        spawn_server_task(handle(client_stream, ctx.clone()));
    }
}

/// Handler for processing BlazeSDK client connections
///
/// ## Arguments
/// * `client_stream` - The client stream to read and write from
/// * `ctx`           - The client context
async fn handle(mut client_stream: TcpStream, ctx: Arc<ClientContext>) {
    debug!("Starting blaze connection");

    // Create a stream to the Pocket Relay server
    let mut server_stream = match create_server_stream(
        &ctx.http_client,
        &ctx.base_url,
        Option::as_ref(&ctx.association),
    )
    .await
    {
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
