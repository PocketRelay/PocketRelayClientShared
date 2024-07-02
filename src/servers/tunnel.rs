//! Tunneling server
//!
//! Provides a local tunnel that connects clients by tunneling through the Pocket Relay
//! server. This allows clients with more strict NATs to host games without common issues
//! faced when trying to connect
//!
//! Details can be found on the GitHub issue: https://github.com/PocketRelay/Server/issues/64

use self::codec::{TunnelCodec, TunnelMessage};
use crate::{
    api::create_server_tunnel,
    ctx::ClientContext,
    servers::{spawn_server_task, GAME_HOST_PORT, RANDOM_PORT, TUNNEL_HOST_PORT},
};
use bytes::Bytes;
use futures::{Sink, Stream};
use log::{debug, error};
use reqwest::Upgraded;
use std::{
    future::Future,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};
use tokio::{io::ReadBuf, net::UdpSocket, sync::mpsc, try_join};
use tokio_util::codec::Framed;

/// The fixed size of socket pool to use
const SOCKET_POOL_SIZE: usize = 4;
/// Max tunnel creation attempts that can be an error before cancelling
const MAX_ERROR_ATTEMPTS: u8 = 5;

// Local address the client uses to send packets
static LOCAL_SEND_TARGET: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, GAME_HOST_PORT));

/// Starts the tunnel socket pool and creates the tunnel
/// connection to the server
///
/// ## Arguments
/// * `ctx` - The client context
pub async fn start_tunnel_server(ctx: Arc<ClientContext>) -> std::io::Result<()> {
    let association = match Option::as_ref(&ctx.association) {
        Some(value) => value,
        // Don't try and tunnel without a token
        None => return Ok(()),
    };

    // Last encountered error
    let mut last_error: Option<std::io::Error> = None;
    // Number of attempts that errored
    let mut attempt_errors: u8 = 0;

    // Looping to attempt reconnecting if lost
    while attempt_errors < MAX_ERROR_ATTEMPTS {
        // Create the tunnel (Future will end if tunnel stopped)
        let reconnect_time = if let Err(err) = create_tunnel(ctx.clone(), association).await {
            error!("Failed to create tunnel: {}", err);

            // Set last error
            last_error = Some(err);

            // Increase error attempts
            attempt_errors += 1;

            // Error should be delayed by the number of errors already hit
            Duration::from_millis(1000 * attempt_errors as u64)
        } else {
            // Reset error attempts
            attempt_errors = 0;

            // Non errored reconnect can be quick
            Duration::from_millis(1000)
        };

        debug!(
            "Next tunnel create attempt in: {}s",
            reconnect_time.as_secs()
        );

        // Wait before attempting to re-create the tunnel
        tokio::time::sleep(reconnect_time).await;
    }

    Err(last_error.unwrap_or(std::io::Error::new(
        ErrorKind::Other,
        "Reached error connect limit",
    )))
}

/// Creates a new tunnel
///
/// ## Arguments
/// * `ctx`         - The client context
/// * `association` - The client association token
async fn create_tunnel(ctx: Arc<ClientContext>, association: &str) -> std::io::Result<()> {
    // Create the tunnel with the server
    let io = create_server_tunnel(&ctx.http_client, &ctx.base_url, association)
        .await
        // Wrap the tunnel with the [`TunnelCodec`] framing
        .map(|io| Framed::new(io, TunnelCodec::default()))
        // Wrap the error into an [`std::io::Error`]
        .map_err(|err| std::io::Error::new(ErrorKind::Other, err))?;
    debug!("Created server tunnel");

    // Allocate the socket pool for the tunnel
    let (tx, rx) = mpsc::unbounded_channel();
    let pool = Socket::allocate_pool(tx).await?;
    debug!("Allocated tunnel pool");

    // Start the tunnel
    Tunnel {
        io,
        rx,
        pool,
        write_state: Default::default(),
    }
    .await;

    Ok(())
}

/// Represents a tunnel and its pool of connections that it can
/// send data to and receive data from
struct Tunnel {
    /// Tunnel connection to the Pocket Relay server for sending [`TunnelMessage`]s
    /// through the server to reach a specific peer
    io: Framed<Upgraded, TunnelCodec>,
    /// Receiver for receiving messages from [`Socket`]s within the [`Tunnel::pool`]
    /// that need to be sent through [`Tunnel::io`]
    rx: mpsc::UnboundedReceiver<TunnelMessage>,
    /// Pool of [`Socket`]s that this tunnel can use for sending out messages
    pool: [SocketHandle; SOCKET_POOL_SIZE],
    /// Current state of writing [`TunnelMessage`]s to the [`Tunnel::io`]
    write_state: TunnelWriteState,
}

/// Holds the state for the current writing progress for a [`Tunnel`]
#[derive(Default)]
enum TunnelWriteState {
    /// Waiting for a message to come through the [`Tunnel::rx`]
    #[default]
    Recv,
    /// Waiting for the [`Tunnel::io`] to be writable, then writing the
    /// contained [`TunnelMessage`]
    Write(Option<TunnelMessage>),
    /// Poll flushing the bytes written to [`Tunnel::io`]
    Flush,
    /// The tunnel has stopped and should not continue
    Stop,
}

/// Holds the state for the current reading progress for a [`Tunnel`]
enum TunnelReadState {
    /// Continue reading
    Continue,
    /// The tunnel has stopped and should not continue
    Stop,
}

impl Tunnel {
    /// Polls accepting messages from [`Tunnel::rx`] then writing them to [`Tunnel::io`] and
    /// flushing the underlying stream. Provides the next [`TunnelWriteState`]
    /// when [`Poll::Ready`] is returned
    ///
    /// Should be repeatedly called until it no-longer returns [`Poll::Ready`]
    fn poll_write_state(&mut self, cx: &mut Context<'_>) -> Poll<TunnelWriteState> {
        Poll::Ready(match &mut self.write_state {
            TunnelWriteState::Recv => {
                // Try receive a packet from the write channel
                let result = ready!(Pin::new(&mut self.rx).poll_recv(cx));

                if let Some(message) = result {
                    TunnelWriteState::Write(Some(message))
                } else {
                    // All writers have closed, tunnel must be closed (Future end)
                    TunnelWriteState::Stop
                }
            }
            TunnelWriteState::Write(message) => {
                // Wait until the `io` is ready
                if ready!(Pin::new(&mut self.io).poll_ready(cx)).is_ok() {
                    let message = message
                        .take()
                        .expect("Unexpected write state without message");

                    // Write the packet to the buffer
                    Pin::new(&mut self.io)
                        .start_send(message)
                        // Packet encoder impl shouldn't produce errors
                        .expect("Message encoder errored");

                    TunnelWriteState::Flush
                } else {
                    // Failed to ready, tunnel must be closed
                    TunnelWriteState::Stop
                }
            }
            TunnelWriteState::Flush => {
                // Poll flushing `io`
                if ready!(Pin::new(&mut self.io).poll_flush(cx)).is_ok() {
                    TunnelWriteState::Recv
                } else {
                    // Failed to flush, tunnel must be closed
                    TunnelWriteState::Stop
                }
            }

            // Tunnel should *NOT* be polled if its already stopped
            TunnelWriteState::Stop => panic!("Tunnel polled after already stopped"),
        })
    }

    /// Polls reading messages from [`Tunnel::io`] and sending them to the correct
    /// handle within the [`Tunnel::pool`]. Provides the next [`TunnelReadState`]
    /// when [`Poll::Ready`] is returned
    ///
    /// Should be repeatedly called until it no-longer returns [`Poll::Ready`]
    fn poll_read_state(&mut self, cx: &mut Context<'_>) -> Poll<TunnelReadState> {
        // Try receive a message from the `io`
        let Some(Ok(message)) = ready!(Pin::new(&mut self.io).poll_next(cx)) else {
            // Cannot read next message stop the tunnel
            return Poll::Ready(TunnelReadState::Stop);
        };

        if message.index == 255 {
            // Write a ping response if we aren't already writing another message
            if let TunnelWriteState::Recv = self.write_state {
                // Move to a writing state
                self.write_state = TunnelWriteState::Write(Some(TunnelMessage {
                    index: 255,
                    message: Bytes::new(),
                }));

                // Poll the write state
                if let Poll::Ready(next_state) = self.poll_write_state(cx) {
                    self.write_state = next_state;

                    // Tunnel has stopped
                    if let TunnelWriteState::Stop = self.write_state {
                        return Poll::Ready(TunnelReadState::Stop);
                    }
                }
            }

            return Poll::Ready(TunnelReadState::Continue);
        }

        // Get the handle to use within the connection pool
        let handle = self.pool.get(message.index as usize);

        // Send the message to the handle if its valid
        if let Some(handle) = handle {
            _ = handle.0.send(message);
        }

        Poll::Ready(TunnelReadState::Continue)
    }
}

impl Future for Tunnel {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Poll the write half
        while let Poll::Ready(next_state) = this.poll_write_state(cx) {
            this.write_state = next_state;

            // Tunnel has stopped
            if let TunnelWriteState::Stop = this.write_state {
                return Poll::Ready(());
            }
        }

        // Poll the read half
        while let Poll::Ready(next_state) = this.poll_read_state(cx) {
            // Tunnel has stopped
            if let TunnelReadState::Stop = next_state {
                return Poll::Ready(());
            }
        }

        Poll::Pending
    }
}

/// Handle to a [`Socket`] for sending [`TunnelMessage`]s that the
/// socket should send to the [`LOCAL_SEND_TARGET`]
#[derive(Clone)]
struct SocketHandle(mpsc::UnboundedSender<TunnelMessage>);

/// Size of the socket read buffer 2^16 bytes
///
/// Can likely be reduced to 2^15 bytes or 2^13 bytes (or lower) since
/// highest observed message length was 1254 bytes but testing is required
/// before that can take place
const READ_BUFFER_LENGTH: usize = 2usize.pow(16);

/// Socket used by a [`Tunnel`] for sending and receiving messages in
/// order to simulate another player on the local network
struct Socket {
    // Index of the socket
    index: u8,
    // The underlying socket for sending and receiving
    socket: UdpSocket,
    /// Receiver for messages coming from the the [`Tunnel`] that need to be
    /// send through the socket
    rx: mpsc::UnboundedReceiver<TunnelMessage>,
    /// Sender for sending [`TunnelMessage`]s through the associated [`Tunnel`]
    /// in order for them to be sent to the correct peer on the other side
    tun_tx: mpsc::UnboundedSender<TunnelMessage>,
    /// Buffer for reading bytes from the `socket`
    read_buffer: [u8; READ_BUFFER_LENGTH],
    /// Current state of writing [`TunnelMessage`]s to the `socket`
    write_state: SocketWriteState,
}

/// Holds the state for the current writing progress for a [`Socket`]
#[derive(Default)]
enum SocketWriteState {
    /// Waiting for a message to come through the [`Socket::rx`]
    #[default]
    Recv,
    /// Waiting for the [`Socket::socket`] to write the bytes
    Write(Bytes),
    /// The tunnel has stopped and should not continue
    Stop,
}

/// Holds the state for the current reading progress for a [`Socket`]
enum SocketReadState {
    /// Continue reading
    Continue,
    /// The tunnel has stopped and should not continue
    Stop,
}

impl Socket {
    /// Allocates a pool of [`Socket`]s for a [`Tunnel`] to use
    ///
    /// ## Arguments
    /// * `tun_tx` - The tunnel sender for sending [`TunnelMessage`]s through the tunnel
    async fn allocate_pool(
        tun_tx: mpsc::UnboundedSender<TunnelMessage>,
    ) -> std::io::Result<[SocketHandle; SOCKET_POOL_SIZE]> {
        let sockets = try_join!(
            // Host socket index *must* use a fixed port since its used on the server side
            Socket::start(0, TUNNEL_HOST_PORT, tun_tx.clone()),
            // Other sockets can used OS auto assigned port
            Socket::start(1, RANDOM_PORT, tun_tx.clone()),
            Socket::start(2, RANDOM_PORT, tun_tx.clone()),
            Socket::start(3, RANDOM_PORT, tun_tx),
        )?;
        Ok(sockets.into())
    }

    /// Starts a new tunnel socket returning a [`SocketHandle`] that can be used
    /// to send [`TunnelMessage`]s to the socket
    ///
    /// ## Arguments
    /// * `index`  - The index of the socket
    /// * `port`   - The port to bind the socket on
    /// * `tun_tx` - The tunnel sender for sending [`TunnelMessage`]s through the tunnel
    async fn start(
        index: u8,
        port: u16,
        tun_tx: mpsc::UnboundedSender<TunnelMessage>,
    ) -> std::io::Result<SocketHandle> {
        // Bind the socket
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, port)).await?;
        // Set the socket send target
        socket.connect(LOCAL_SEND_TARGET).await?;

        // Create the message channel
        let (tx, rx) = mpsc::unbounded_channel();

        // Spawn the socket task
        spawn_server_task(Socket {
            index,
            socket,
            rx,
            tun_tx,
            read_buffer: [0; READ_BUFFER_LENGTH],
            write_state: Default::default(),
        });

        Ok(SocketHandle(tx))
    }

    /// Polls accepting messages from [`Socket::rx`] then writing them to the [`Socket::socket`].
    /// Provides the next [`SocketWriteState`] when [`Poll::Ready`] is returned
    ///
    /// Should be repeatedly called until it no-longer returns [`Poll::Ready`]
    fn poll_write_state(&mut self, cx: &mut Context<'_>) -> Poll<SocketWriteState> {
        Poll::Ready(match &mut self.write_state {
            SocketWriteState::Recv => {
                // Try receive a packet from the write channel
                let result = ready!(Pin::new(&mut self.rx).poll_recv(cx));

                if let Some(message) = result {
                    SocketWriteState::Write(message.message)
                } else {
                    // All writers have closed, tunnel must be closed (Future end)
                    SocketWriteState::Stop
                }
            }
            SocketWriteState::Write(message) => {
                // Try send the message to the local target
                let Ok(count) = ready!(self.socket.poll_send(cx, message)) else {
                    return Poll::Ready(SocketWriteState::Stop);
                };

                // Didn't write the entire message
                if count != message.len() {
                    // Continue with a writing state at the remaining message
                    let message = message.slice(count..);
                    SocketWriteState::Write(message)
                } else {
                    SocketWriteState::Recv
                }
            }

            // Tunnel socket should *NOT* be polled if its already stopped
            SocketWriteState::Stop => panic!("Tunnel socket polled after already stopped"),
        })
    }

    /// Polls reading messages from `socket` and sending them to the [`Tunnel`]
    /// in order for them to be sent out to the peer. Provides the next
    /// [`SocketReadState`] when [`Poll::Ready`] is returned
    ///
    /// Should be repeatedly called until it no-longer returns [`Poll::Ready`]
    fn poll_read_state(&mut self, cx: &mut Context<'_>) -> Poll<SocketReadState> {
        let mut read_buf = ReadBuf::new(&mut self.read_buffer);

        // Try receive a message from the socket
        if ready!(self.socket.poll_recv(cx, &mut read_buf)).is_err() {
            return Poll::Ready(SocketReadState::Stop);
        }

        // Get the received message
        let bytes = read_buf.filled();
        let message = Bytes::copy_from_slice(bytes);
        let message = TunnelMessage {
            index: self.index,
            message,
        };

        // Send the message through the tunnel
        Poll::Ready(if self.tun_tx.send(message).is_ok() {
            SocketReadState::Continue
        } else {
            SocketReadState::Stop
        })
    }
}

impl Future for Socket {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        // Poll the write half
        while let Poll::Ready(next_state) = this.poll_write_state(cx) {
            this.write_state = next_state;

            // Tunnel has stopped
            if let SocketWriteState::Stop = this.write_state {
                return Poll::Ready(());
            }
        }

        // Poll the read half
        while let Poll::Ready(next_state) = this.poll_read_state(cx) {
            // Tunnel has stopped
            if let SocketReadState::Stop = next_state {
                return Poll::Ready(());
            }
        }

        Poll::Pending
    }
}

mod codec {
    //! This modules contains the codec and message structures for [TunnelMessage]s
    //!
    //! # Tunnel Messages
    //!
    //! Tunnel message frames are as follows:
    //!
    //! ```text
    //!  0                   1                   2                      
    //!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3
    //! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    //! |     Index     |            Length             |
    //! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    //! |                                               :
    //! :                    Payload                    :
    //! :                                               |
    //! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
    //! ```
    //!
    //! Tunnel message frames contain the following fields:
    //!
    //! Index: 8-bits. Determines the destination of the message within the current pool.
    //!
    //! Length: 16-bits. Determines the size in bytes of the payload that follows
    //!
    //! Payload: Variable length. The message bytes payload of `Length`
    //!
    //!
    //! ## Keep alive
    //!
    //! The server will send keep-alive messages, these are in the same
    //! format as the packet above. However, the index will always be 255
    //! and the payload will be empty.

    use bytes::{Buf, BufMut, Bytes};
    use tokio_util::codec::{Decoder, Encoder};

    /// Header portion of a [TunnelMessage] that contains the
    /// index of the message and the length of the expected payload
    struct TunnelMessageHeader {
        /// Socket index to use
        index: u8,
        /// The length of the tunnel message bytes
        length: u16,
    }

    /// Message sent through the tunnel
    pub struct TunnelMessage {
        /// Socket index to use
        pub index: u8,
        /// The message contents
        pub message: Bytes,
    }

    /// Codec for encoding and decoding tunnel messages
    #[derive(Default)]
    pub struct TunnelCodec {
        /// Stores the current message header while its waiting
        /// for the full payload to become available
        partial: Option<TunnelMessageHeader>,
    }

    impl Decoder for TunnelCodec {
        type Item = TunnelMessage;
        type Error = std::io::Error;

        fn decode(&mut self, src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
            let partial = match self.partial.as_mut() {
                Some(value) => value,
                None => {
                    // Not enough room for a partial frame
                    if src.len() < 5 {
                        return Ok(None);
                    }
                    let index = src.get_u8();
                    let length = src.get_u16();

                    self.partial.insert(TunnelMessageHeader { index, length })
                }
            };
            // Not enough data for the partial frame
            if src.len() < partial.length as usize {
                return Ok(None);
            }

            let partial = self.partial.take().expect("Partial frame missing");
            let bytes = src.split_to(partial.length as usize);

            Ok(Some(TunnelMessage {
                index: partial.index,
                message: bytes.freeze(),
            }))
        }
    }

    impl Encoder<TunnelMessage> for TunnelCodec {
        type Error = std::io::Error;

        fn encode(
            &mut self,
            item: TunnelMessage,
            dst: &mut bytes::BytesMut,
        ) -> Result<(), Self::Error> {
            dst.put_u8(item.index);
            dst.put_u16(item.message.len() as u16);
            dst.extend_from_slice(&item.message);
            Ok(())
        }
    }
}
