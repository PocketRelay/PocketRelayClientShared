use self::codec::TunnelMessage;
use crate::{
    ctx::ClientContext,
    servers::{spawn_server_task, GAME_HOST_PORT, RANDOM_PORT, TUNNEL_HOST_PORT},
};
use codec::{MessageHeader, MessageReader, MessageWriter};
use log::{debug, error};
use std::{
    future::Future,
    io::ErrorKind,
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
    time::Duration,
};
use tokio::{io::ReadBuf, net::UdpSocket, sync::mpsc, time::sleep, try_join};

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
pub async fn start_tunnel_server_v2(ctx: Arc<ClientContext>) -> std::io::Result<()> {
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

async fn handshake_tunnel(socket: &UdpSocket, association: &str) -> std::io::Result<u32> {
    let mut writer = MessageWriter::default();
    let header = MessageHeader {
        tunnel_id: u32::MAX,
        version: 0,
    };

    let message = TunnelMessage::Initiate {
        association_token: association.to_string(),
    };
    header.write(&mut writer);
    message.write(&mut writer);

    socket.send(&writer.buffer).await?;

    let mut buffer = [0u8; u16::MAX as usize];

    let count = socket.recv(&mut buffer).await?;
    let buffer = &buffer[..count];
    let mut reader = MessageReader::new(buffer);

    let header = MessageHeader::read(&mut reader)
        .map_err(|_| std::io::Error::new(ErrorKind::Other, "Malformed packet"))?;
    let message = TunnelMessage::read(&mut reader)
        .map_err(|_| std::io::Error::new(ErrorKind::Other, "Malformed packet"))?;

    match message {
        TunnelMessage::Initiated { tunnel_id } => Ok(tunnel_id),
        _ => Err(std::io::Error::new(ErrorKind::Other, "Unexpected packet")),
    }
}

/// Creates a new tunnel
///
/// ## Arguments
/// * `ctx`         - The client context
/// * `association` - The client association token
async fn create_tunnel(ctx: Arc<ClientContext>, association: &str) -> std::io::Result<()> {
    let host = match ctx.base_url.host() {
        Some(value) => value,
        // Cannot form a tunnel without a host
        None => return Ok(()),
    };

    let tunnel_port = match ctx.tunnel_port {
        Some(value) => value,
        // Cannot form a tunnel without a port
        None => return Ok(()),
    };

    // Bind a local udp socket
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).await?;

    // Map connection to remote tunnel server
    socket.connect((host.to_string(), tunnel_port)).await?;

    debug!("Initiating tunnel: {}:{}", host, tunnel_port);

    let tunnel_id: u32;

    {
        let mut retry_count = 0;

        loop {
            match handshake_tunnel(&socket, association).await {
                Ok(value) => {
                    tunnel_id = value;
                    break;
                }
                Err(err) => {
                    error!("failed to handshake for token: {}", err);

                    retry_count += 1;
                    sleep(Duration::from_secs(5 * retry_count)).await;

                    if retry_count > 5 {
                        return Err(err);
                    }
                }
            }
        }
    }

    debug!("Created server tunnel");

    // Allocate the socket pool for the tunnel
    let (tx, rx) = mpsc::unbounded_channel();
    let pool = Socket::allocate_pool(tx).await?;
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

    Ok(())
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

                    let header = MessageHeader {
                        tunnel_id: self.tunnel_id,
                        version: 0,
                    };

                    let mut buffer = MessageWriter::default();
                    header.write(&mut buffer);
                    message.write(&mut buffer);

                    // Write the packet to the buffer
                    ready!(Pin::new(&mut self.socket).poll_send(cx, &buffer.buffer))
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
        let mut reader = MessageReader::new(buffer);

        let message = match TunnelMessage::read(&mut reader) {
            Ok(value) => value,
            Err(_) => return Poll::Ready(TunnelReadState::Stop),
        };

        match message {
            TunnelMessage::Forward { index, message } => {
                // Get the handle to use within the connection pool
                let handle = self.pool.get(index as usize);

                // Send the message to the handle if its valid
                if let Some(handle) = handle {
                    _ = handle.0.send(message);
                }
            }

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

mod codec {
    use thiserror::Error;

    #[derive(Default)]
    pub struct MessageWriter {
        pub buffer: Vec<u8>,
    }

    impl MessageWriter {
        #[inline]
        pub fn write_u8(&mut self, value: u8) {
            self.buffer.push(value)
        }

        #[inline]
        pub fn write_bytes(&mut self, value: &[u8]) {
            self.buffer.extend_from_slice(value)
        }

        pub fn write_u32(&mut self, value: u32) {
            self.write_bytes(&value.to_be_bytes())
        }

        pub fn write_u16(&mut self, value: u16) {
            self.write_bytes(&value.to_be_bytes())
        }
    }

    pub struct MessageReader<'a> {
        buffer: &'a [u8],
        cursor: usize,
    }

    impl<'a> MessageReader<'a> {
        pub fn new(buffer: &'a [u8]) -> MessageReader<'a> {
            MessageReader { buffer, cursor: 0 }
        }

        #[inline]
        pub fn capacity(&self) -> usize {
            self.buffer.len()
        }

        pub fn len(&self) -> usize {
            self.capacity() - self.cursor
        }

        pub fn read_u8(&mut self) -> Result<u8, MessageError> {
            if self.len() < 1 {
                return Err(MessageError::Incomplete);
            }

            let value = self.buffer[self.cursor];
            self.cursor += 1;

            Ok(value)
        }

        pub fn read_u32(&mut self) -> Result<u32, MessageError> {
            let value = self.read_bytes(4)?;
            let value = u32::from_be_bytes([value[0], value[1], value[2], value[3]]);
            Ok(value)
        }

        pub fn read_u16(&mut self) -> Result<u16, MessageError> {
            let value = self.read_bytes(2)?;
            let value = u16::from_be_bytes([value[0], value[1]]);
            Ok(value)
        }

        pub fn read_bytes(&mut self, length: usize) -> Result<&'a [u8], MessageError> {
            if self.len() < length {
                return Err(MessageError::Incomplete);
            }
            let value = &self.buffer[self.cursor..self.cursor + length];
            self.cursor += length;
            Ok(value)
        }
    }

    #[derive(Debug)]
    pub struct MessageHeader {
        /// Protocol version (For future sake)
        pub version: u8,
        /// ID of the tunnel this message is from, [u32::MAX] when the
        /// tunnel is not yet initiated
        pub tunnel_id: u32,
    }

    #[derive(Debug, Error)]
    pub enum MessageError {
        #[error("unknown message type")]
        UnknownMessageType,

        #[error("message was incomplete")]
        Incomplete,
    }

    impl MessageHeader {
        pub fn read(buf: &mut MessageReader<'_>) -> Result<MessageHeader, MessageError> {
            let version = buf.read_u8()?;
            let tunnel_id = buf.read_u32()?;

            Ok(Self { version, tunnel_id })
        }

        pub fn write(&self, buf: &mut MessageWriter) {
            buf.write_u8(self.version);
            buf.write_u32(self.tunnel_id);
        }
    }

    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    #[repr(u8)]
    pub enum MessageType {
        /// Client is requesting to initiate a connection
        Initiate = 0x0,

        /// Server has accepted a connection
        Initiated = 0x1,

        /// Forward a message on behalf of the player to
        /// another player
        Forward = 0x2,

        /// Message to keep the stream alive
        /// (When the connect is inactive)
        KeepAlive = 0x3,
    }

    impl TryFrom<u8> for MessageType {
        type Error = MessageError;
        fn try_from(value: u8) -> Result<Self, MessageError> {
            Ok(match value {
                0x0 => Self::Initiate,
                0x1 => Self::Initiated,
                0x2 => Self::Forward,
                0x3 => Self::KeepAlive,
                _ => return Err(MessageError::UnknownMessageType),
            })
        }
    }

    pub enum TunnelMessage {
        /// Client is requesting to initiate a connection
        Initiate {
            /// Association token to authenticate with
            association_token: String,
        },

        /// Server created and associated the tunnel
        Initiated {
            /// Unique ID for the tunnel to include in future messages
            /// to identify itself
            tunnel_id: u32,
        },

        /// Client wants to forward a message
        Forward {
            /// Local socket pool index the message was sent to.
            /// Used to map to the target within the game
            index: u8,

            /// Message contents to forward
            message: Vec<u8>,
        },

        /// Keep alive
        KeepAlive,
    }

    impl TunnelMessage {
        pub fn read(buf: &mut MessageReader<'_>) -> Result<TunnelMessage, MessageError> {
            let ty = buf.read_u8()?;
            let ty = MessageType::try_from(ty)?;

            match ty {
                MessageType::Initiate => {
                    // Get length of the association token
                    let length = buf.read_u16()? as usize;
                    let token_bytes = buf.read_bytes(length)?;
                    let token = String::from_utf8_lossy(token_bytes);
                    Ok(TunnelMessage::Initiate {
                        association_token: token.to_string(),
                    })
                }
                MessageType::Initiated => {
                    let tunnel_id = buf.read_u32()?;

                    Ok(TunnelMessage::Initiated { tunnel_id })
                }
                MessageType::Forward => {
                    let index = buf.read_u8()?;

                    // Get length of the association token
                    let length = buf.read_u16()? as usize;

                    let message = buf.read_bytes(length)?;

                    Ok(TunnelMessage::Forward {
                        index,
                        message: message.to_vec(),
                    })
                }
                MessageType::KeepAlive => Ok(TunnelMessage::KeepAlive),
            }
        }

        pub fn write(&self, buf: &mut MessageWriter) {
            match self {
                TunnelMessage::Initiate { association_token } => {
                    debug_assert!(association_token.len() < u16::MAX as usize);
                    buf.write_u8(MessageType::Initiate as u8);

                    buf.write_u16(association_token.len() as u16);
                    buf.write_bytes(association_token.as_bytes());
                }
                TunnelMessage::Initiated { tunnel_id } => {
                    buf.write_u8(MessageType::Initiated as u8);
                    buf.write_u32(*tunnel_id);
                }
                TunnelMessage::Forward { index, message } => {
                    buf.write_u8(MessageType::Forward as u8);
                    debug_assert!(message.len() < u16::MAX as usize);

                    buf.write_u8(*index);
                    buf.write_u16(message.len() as u16);
                    buf.write_bytes(message);
                }
                TunnelMessage::KeepAlive => {
                    buf.write_u8(MessageType::KeepAlive as u8);
                }
            }
        }
    }
}
