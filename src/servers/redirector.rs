//! Pocket Relay version of gosredirector.ea.com, informs the game clients
//! where the blaze server is located, in this case it always reports the
//! servers as localhost

use super::{spawn_server_task, BLAZE_PORT, REDIRECTOR_PORT};
use crate::fire::{FireCodec, Frame};
use blaze_ssl_async::{BlazeAccept, BlazeListener};
use futures::{SinkExt, TryStreamExt};
use log::{debug, error};
use std::{io, net::Ipv4Addr, time::Duration};
use tdf::TdfSerialize;
use thiserror::Error;
use tokio::time::{error::Elapsed, timeout};
use tokio_util::codec::Framed;

/// Starts the redirector server
pub async fn start_redirector_server() -> std::io::Result<()> {
    // Bind the local ssl socket for accepting connections
    let listener = BlazeListener::bind((Ipv4Addr::LOCALHOST, REDIRECTOR_PORT)).await?;

    // Accept connections
    loop {
        let client_accept = listener.accept().await?;
        spawn_server_task(async move {
            if let Err(err) = handle(client_accept).await {
                error!("Error while redirecting: {}", err);
            }
        });
    }
}

/// Errors that could occur during the redirection process
#[derive(Debug, Error)]
pub enum RedirectError {
    /// Error while accepting the ssl connection
    #[error(transparent)]
    BlazeSsl(#[from] blaze_ssl_async::BlazeError),
    /// Connect timed out
    #[error("Timed out")]
    Timeout(Elapsed),
    /// Error while reading packets
    #[error("Read error: {0}")]
    ReadError(io::Error),
    /// Error while writing packets
    #[error("Write error: {0}")]
    WriteError(io::Error),
}

/// Allowed time for a redirect to occur before considering
/// the connection as timed out
const REDIRECT_TIMEOUT: Duration = Duration::from_secs(60);
/// Redirector component to expect
const COMPONENT_REDIRECTOR: u16 = 0x5;
/// getServerInstance command to expect
const COMMAND_GET_SERVER_INSTANCE: u16 = 0x1;

/// Handler for processing redirector connections
///
/// ## Arguments
/// * `client_accept` - The connecting SSL client to accept
async fn handle(client_accept: BlazeAccept) -> Result<(), RedirectError> {
    let (stream, _) = client_accept.finish_accept().await?;
    let mut framed = Framed::new(stream, FireCodec::default());

    while let Some(packet) = timeout(REDIRECT_TIMEOUT, framed.try_next())
        .await
        // Handle timeout errors
        .map_err(RedirectError::Timeout)?
        // Handle reading errors
        .map_err(RedirectError::ReadError)?
    {
        let header = &packet.header;

        // Respond to unexpected packets with empty responses
        if header.component != COMPONENT_REDIRECTOR || header.command != COMMAND_GET_SERVER_INSTANCE
        {
            debug!(
                "Redirector got unexpected request {} {}",
                header.component, header.command
            );
            framed
                .send(Frame::response_empty(header))
                .await
                .map_err(RedirectError::WriteError)?;
            continue;
        }

        debug!("Redirector responding");

        framed
            .send(Frame::response(header, LocalInstanceResponse))
            .await
            .map_err(RedirectError::WriteError)?;
        break;
    }

    Ok(())
}

/// Response for redirecting to a local instance
struct LocalInstanceResponse;

impl TdfSerialize for LocalInstanceResponse {
    fn serialize<S: tdf::prelude::TdfSerializer>(&self, w: &mut S) {
        w.tag_union_start(b"ADDR", 0x0); /* Server address type */

        // Encode the net address portion
        w.group(b"VALU", |w| {
            w.tag_u32(b"IP", u32::from_be_bytes([127, 0, 0, 1]));
            w.tag_u16(b"PORT", BLAZE_PORT);
        });

        w.tag_bool(b"SECU", false);
        w.tag_bool(b"XDNS", false);
    }
}
