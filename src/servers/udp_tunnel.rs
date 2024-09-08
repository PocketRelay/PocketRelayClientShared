//! UDP Tunneling server
//!
//! Provides a local tunnel that connects clients by tunneling through the Pocket Relay
//! server. This allows clients with more strict NATs to host games without common issues
//! faced when trying to connect. This is the faster UDP implementation

use crate::{
    ctx::ClientContext,
    servers::{spawn_server_task, GAME_HOST_PORT, RANDOM_PORT, TUNNEL_HOST_PORT},
};
use log::{debug, error};
use pocket_relay_udp_tunnel::{
    deserialize_message, serialize_message, MessageError, TunnelMessage,
};
use std::{
    future::Future,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};
use thiserror::Error;
use tokio::{
    io::ReadBuf,
    net::UdpSocket,
    sync::mpsc,
    time::{sleep, timeout},
    try_join,
};

/// The fixed size of socket pool to use
const SOCKET_POOL_SIZE: usize = 4;
/// Max tunnel creation attempts that can be an error before cancelling
const MAX_ERROR_ATTEMPTS: u8 = 5;

// Local address the client uses to send packets
static LOCAL_SEND_TARGET: SocketAddr =
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, GAME_HOST_PORT));

/// Errors that can occur while creating a UDP tunnel
#[derive(Debug, Error)]
pub enum UdpTunnelError {
    /// The base URL of the server is not compatible with the UDP tunnel
    /// variant (It missing the host portion?)
    #[error("host url incompatible with UDP tunnel")]
    HostIncompatible,

    /// Server version does not support the UDP tunnel or the server has
    /// explicitly disabled the tunnel
    #[error("server incompatible with UDP tunnel")]
    ServerIncompatible,

    /// Failed to bind the tunnel socket
    #[error(transparent)]
    Bind(std::io::Error),

    /// Failed to "connect" to the target server, happens when the host
    /// is unreachable or DNS resolution fails
    #[error(transparent)]
    Connect(std::io::Error),

    /// Reached timeout while attempting to complete handshake
    #[error("timeout reached while handshaking")]
    HandshakeTimeout,

    /// Some generic IO error occurred while reading or writing
    #[error(transparent)]
    GenericIo(#[from] std::io::Error),

    /// Received malformed packet when creating the tunnel
    #[error("malformed packet: {0}")]
    MalformedPacket(#[from] MessageError),

    /// Got an unexpected packet during the handshake process
    #[error("unexpected packet while handshaking")]
    UnexpectedPacket,

    /// Failed to allocate the local socket pool
    #[error(transparent)]
    AllocateSocketPool(std::io::Error),
}

/// Starts the tunnel socket pool and creates the tunnel
/// connection to the server
///
/// ## Arguments
/// * `ctx`         - The client context
/// * `tunnel_port` - The UDP tunnel server port to connect to
pub async fn start_udp_tunnel_server(
    ctx: Arc<ClientContext>,
    tunnel_port: u16,
) -> std::io::Result<()> {
    let host = match ctx.base_url.host() {
        Some(value) => value.to_string(),
        // Cannot form a tunnel without a host
        None => return Ok(()),
    };

    let association = match Option::as_ref(&ctx.association) {
        Some(value) => value,
        // Don't try and tunnel without a token
        None => return Ok(()),
    };

    // Last encountered error
    let mut last_error: Option<UdpTunnelError> = None;
    // Number of attempts that errored
    let mut attempt_errors: u8 = 0;

    // Looping to attempt reconnecting if lost
    while attempt_errors < MAX_ERROR_ATTEMPTS {
        // Create the tunnel (Future will end if tunnel stopped)
        let reconnect_time = if let Err(err) = create_tunnel(&host, tunnel_port, association).await
        {
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

    Err(last_error
        .map(|err| std::io::Error::new(ErrorKind::Other, err))
        .unwrap_or(std::io::Error::new(
            ErrorKind::Other,
            "Reached error connect limit",
        )))
}

/// Creates a new tunnel
///
/// ## Arguments
/// * `host`        - The host for connecting the tunnel
/// * `tunnel_port` - The port the tunnel is running on
/// * `association` - The client association token
async fn create_tunnel(
    host: &str,
    tunnel_port: u16,
    association: &str,
) -> Result<(), UdpTunnelError> {
    // Bind a local udp socket
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .await
        .map_err(UdpTunnelError::Bind)?;

    // Map connection to remote tunnel server
    socket
        .connect((host, tunnel_port))
        .await
        .map_err(UdpTunnelError::Connect)?;

    debug!("initiating tunnel: {}:{}", host, tunnel_port);

    let tunnel_id = attempt_tunnel_handshake(&socket, association).await?;

    debug!("created server tunnel: {}", tunnel_id);

    // Allocate the socket pool for the tunnel
    let (tx, rx) = mpsc::unbounded_channel();
    let pool = Socket::allocate_pool(tx)
        .await
        .map_err(UdpTunnelError::AllocateSocketPool)?;
    debug!("Allocated tunnel pool");

    // Start the tunnel
    Tunnel {
        socket,
        tunnel_id,
        rx,
        pool,
        write_state: Default::default(),
        read_buffer: [0u8; u16::MAX as usize],
    }
    .await;

    // TODO: Handle connection lost

    Ok(())
}

// Maximum number of times to try and handshake
const MAX_HANDSHAKE_ATTEMPTS: u8 = 5;

// Time to elapse without a response before the handshake is considered timed out
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Attempts to complete a tunnel handshake, will retry until
async fn attempt_tunnel_handshake(
    socket: &UdpSocket,
    association: &str,
) -> Result<u32, UdpTunnelError> {
    let mut retry_count: u8 = 0;
    let mut retry_delay: u64 = 5;
    let mut last_err: UdpTunnelError;

    loop {
        match timeout(HANDSHAKE_TIMEOUT, handshake_tunnel(socket, association)).await {
            // Successful handshake
            Ok(Ok(value)) => return Ok(value),

            // Got an error while processing the handshake
            Ok(Err(err)) => {
                error!("failed to handshake for token: {}", err);
                last_err = err
            }
            // Handshaking process timed out
            Err(_) => {
                error!("timeout while attempting tunnel handshake");
                last_err = UdpTunnelError::HandshakeTimeout
            }
        }

        retry_count += 1;

        // Wait between attempts with exponential backoff
        sleep(Duration::from_secs(retry_delay)).await;
        retry_delay *= 2;

        if retry_count > MAX_HANDSHAKE_ATTEMPTS {
            return Err(last_err);
        }
    }
}

/// Completes a tunnel handshake over the provided socket, exchanges
/// the association token for a tunnel ID to use on future connections
async fn handshake_tunnel(socket: &UdpSocket, association: &str) -> Result<u32, UdpTunnelError> {
    // Serialize and write the initiate message
    let buffer = serialize_message(
        u32::MAX,
        &TunnelMessage::Initiate {
            association_token: association.to_string(),
        },
    );

    socket.send(&buffer).await?;

    // Allocate buffer and read message
    let mut buffer = [0u8; u16::MAX as usize];
    let count = socket.recv(&mut buffer).await?;
    let buffer = &buffer[..count];

    // Deserialize a message
    let packet = deserialize_message(buffer)?;

    match packet.message {
        // Got the initiation message
        TunnelMessage::Initiated { tunnel_id } => Ok(tunnel_id),

        // Not expecting any other packets in this state
        _ => Err(UdpTunnelError::UnexpectedPacket),
    }
}

/// Represents a tunnel and its pool of connections that it can
/// send data to and receive data from
struct Tunnel {
    /// Tunnel connection to the Pocket Relay server for sending [`TunnelMessage`]s
    /// through the server to reach a specific peer
    socket: UdpSocket,
    /// Tunnel ID
    tunnel_id: u32,
    /// Receiver for receiving messages from [`Socket`]s within the [`Tunnel::pool`]
    /// that need to be sent through [`Tunnel::io`]
    rx: mpsc::UnboundedReceiver<TunnelMessage>,
    /// Pool of [`Socket`]s that this tunnel can use for sending out messages
    pool: [SocketHandle; SOCKET_POOL_SIZE],
    /// Current state of writing [`TunnelMessage`]s to the [`Tunnel::io`]
    write_state: TunnelWriteState,
    /// Buffer for reading
    read_buffer: [u8; u16::MAX as usize],
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
                if ready!(Pin::new(&mut self.socket).poll_send_ready(cx)).is_ok() {
                    let message = message
                        .take()
                        .expect("Unexpected write state without message");

                    let buffer = serialize_message(self.tunnel_id, &message);

                    // Write the packet to the buffer
                    ready!(Pin::new(&mut self.socket).poll_send(cx, &buffer))
                        // Packet encoder impl shouldn't produce errors
                        .expect("Message encoder errored");

                    TunnelWriteState::Recv
                } else {
                    // Failed to ready, tunnel must be closed
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
        if ready!(Pin::new(&mut self.socket).poll_recv_ready(cx)).is_err() {
            // Cannot read next message stop the tunnel
            return Poll::Ready(TunnelReadState::Stop);
        };

        let mut read_buffer = ReadBuf::new(&mut self.read_buffer);

        // Try receive a message from the `io`
        if ready!(Pin::new(&mut self.socket).poll_recv(cx, &mut read_buffer)).is_err() {
            // Cannot read next message stop the tunnel
            return Poll::Ready(TunnelReadState::Stop);
        };

        let buffer = read_buffer.filled();

        let packet = match deserialize_message(buffer) {
            Ok(value) => value,
            Err(err) => {
                error!("encountered invalid tunnel message: {}", err);
                return Poll::Ready(TunnelReadState::Stop);
            }
        };

        match packet.message {
            // Send forwarded messages to the correct socket handle
            TunnelMessage::Forward { index, message } => {
                // Get the handle to use within the connection pool
                let handle = self.pool.get(index as usize);

                // Send the message to the handle if its valid
                if let Some(handle) = handle {
                    _ = handle.0.send(message);
                }
            }

            // Reply to keep-alive message
            TunnelMessage::KeepAlive => {
                self.write_state = TunnelWriteState::Write(Some(TunnelMessage::KeepAlive));

                // Poll the write state
                if let Poll::Ready(next_state) = self.poll_write_state(cx) {
                    self.write_state = next_state;

                    // Tunnel has stopped
                    if let TunnelWriteState::Stop = self.write_state {
                        return Poll::Ready(TunnelReadState::Stop);
                    }
                }
            }

            _ => {
                // Ignore unexpected messages
            }
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
struct SocketHandle(mpsc::UnboundedSender<Vec<u8>>);

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
    rx: mpsc::UnboundedReceiver<Vec<u8>>,
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
    Write(Vec<u8>),
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
                    SocketWriteState::Write(message)
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
                    let remaining = message.split_off(count);
                    SocketWriteState::Write(remaining)
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
        let message = TunnelMessage::Forward {
            index: self.index,
            message: bytes.to_vec(),
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
