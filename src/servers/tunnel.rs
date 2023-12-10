//! Tunneling server

use bytes::Bytes;
use futures::{Future, Sink, Stream};
use log::debug;
use reqwest::Upgraded;
use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};
use tokio::{io::ReadBuf, net::UdpSocket, sync::mpsc, try_join};
use tokio_util::codec::Framed;
use url::Url;

use crate::{
    api::create_server_tunnel,
    servers::{RANDOM_PORT, TUNNEL_HOST_PORT},
};

use self::codec::{TunnelCodec, TunnelMessage};

///
pub async fn start_tunnel_server(
    http_client: reqwest::Client,
    base_url: Arc<Url>,
) -> std::io::Result<()> {
    // Connect the tunnel
    debug!("Allocated tunnel");
    let tunnel = create_server_tunnel(http_client, &base_url).await.unwrap();
    let tunnel = Framed::new(tunnel, TunnelCodec::default());
    let (tx, rx) = mpsc::unbounded_channel();
    let pool = allocate_pool(tx).await.unwrap();
    debug!("Allocated tunnel pool");

    Tunnel {
        io: tunnel,
        rx,
        pool,
        write_state: TunnelWriteState::Recv,
        stop: false,
    }
    .await;

    // TODO: Attempt reconnect on error

    Ok(())
}

/// Represents a tunnel and its pool of conenctions that it can
/// send data to
struct Tunnel {
    /// Underlying tunneled IO for sending messages through the server
    io: Framed<Upgraded, TunnelCodec>,
    /// Reciever for messages that need to be sent through `io`
    rx: mpsc::UnboundedReceiver<TunnelMessage>,
    /// Pool of sockets for sending messages and recieving respones
    pool: [TunnelSocketHandle; 4],
    /// Future state for writing to the `io`
    write_state: TunnelWriteState,
    /// Whether the future has been stopped
    stop: bool,
}

enum TunnelWriteState {
    /// Recieve the message to write
    Recv,
    /// Wait for the stream to be writable
    Write {
        // The message to write
        message: Option<TunnelMessage>,
    },
    // Poll flushing the tunnel
    Flush,
}

impl Tunnel {
    /// Polls accepting messages from `rx` then writing them to `io` and
    /// flushing the underlying stream.
    ///
    /// Should be repeatedly called until it no-longer returns `Poll::Ready`
    fn poll_write_state(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match &mut self.write_state {
            TunnelWriteState::Recv => {
                // Try receive a packet from the write channel
                let result = ready!(Pin::new(&mut self.rx).poll_recv(cx));

                if let Some(message) = result {
                    self.write_state = TunnelWriteState::Write {
                        message: Some(message),
                    };
                } else {
                    // All writers have closed, session must be closed (Future end)
                    self.stop = true;
                }
            }
            TunnelWriteState::Write { message } => {
                // Wait until the inner is ready
                if ready!(Pin::new(&mut self.io).poll_ready(cx)).is_ok() {
                    let message = message
                        .take()
                        .expect("Unexpected write state without message");

                    // Write the packet to the buffer
                    Pin::new(&mut self.io)
                        .start_send(message)
                        // Packet encoder impl shouldn't produce errors
                        .expect("Message encoder errored");

                    self.write_state = TunnelWriteState::Flush;
                } else {
                    // Failed to ready, session must be closed
                    self.stop = true;
                }
            }
            TunnelWriteState::Flush => {
                // Wait until the flush is complete
                if ready!(Pin::new(&mut self.io).poll_flush(cx)).is_ok() {
                    self.write_state = TunnelWriteState::Recv;
                } else {
                    // Failed to flush, session must be closed
                    self.stop = true
                }
            }
        }

        Poll::Ready(())
    }

    /// Polls reading messages from `io` and sending them to the correct
    /// handle within the `pool`.
    ///
    /// Should be repeatedly called until it no-longer returns `Poll::Ready`
    fn poll_read_state(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // Try receive a message from the `io`
        let result = ready!(Pin::new(&mut self.io).poll_next(cx));

        if let Some(Ok(message)) = result {
            // Get the handle to use within the connection pool
            let handle = self.pool.get(message.index as usize);

            // Send the message to the handle if its valid
            if let Some(handle) = handle {
                handle.0.send(message).unwrap();
            }
        } else {
            // Reader has closed or reading encountered an error (Either way stop reading)
            self.stop = true;
        }

        Poll::Ready(())
    }
}

impl Future for Tunnel {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        while this.poll_write_state(cx).is_ready() {}
        while this.poll_read_state(cx).is_ready() {}

        if this.stop {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

/// Handle to a socket for sending it messages that it should send
#[derive(Clone)]
pub struct TunnelSocketHandle(mpsc::UnboundedSender<TunnelMessage>);

/// Represents an individual socket within a tunnels pool
/// of sockets
pub struct TunnelSocket {
    // Index of the socket
    index: u8,
    // Underlying socket
    socket: UdpSocket,
    // Recv messages to write to the socket
    rx: mpsc::UnboundedReceiver<TunnelMessage>,
    // Send messages to write to the tunnel
    tx: mpsc::UnboundedSender<TunnelMessage>,
    /// Buffer for reading from the socket
    read_buffer: [u8; 65536],
    /// Future state for writing to the `socket`
    write_state: TunnelSocketWriteState,
    /// Whether the future has been stopped
    stop: bool,
}

enum TunnelSocketWriteState {
    /// Recieve the message to write
    Recv,
    /// Wait for the bytes to be written
    Write {
        // The message bytes to write
        message: Bytes,
    },
}

impl Future for TunnelSocket {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        while this.poll_write_state(cx).is_ready() {}
        while this.poll_read_state(cx).is_ready() {}

        if this.stop {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

// Local address the client uses to send packets
static LOCAL_SEND_TARGET: SocketAddr = SocketAddr::V4(SocketAddrV4::new(
    Ipv4Addr::LOCALHOST,
    3659, /* Todo update this in protocol later to use the local addr */
));

impl TunnelSocket {
    ///
    pub fn start(
        index: u8,
        socket: UdpSocket,
        tun_tx: mpsc::UnboundedSender<TunnelMessage>,
    ) -> TunnelSocketHandle {
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(TunnelSocket {
            index,
            socket,
            rx,
            tx: tun_tx,
            read_buffer: [0; 65536],
            write_state: TunnelSocketWriteState::Recv,
            stop: false,
        });

        TunnelSocketHandle(tx)
    }

    fn poll_write_state(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        match &mut self.write_state {
            TunnelSocketWriteState::Recv => {
                // Try receive a packet from the write channel
                let result = ready!(Pin::new(&mut self.rx).poll_recv(cx));

                if let Some(message) = result {
                    self.write_state = TunnelSocketWriteState::Write {
                        message: message.message,
                    };
                } else {
                    // All writers have closed, session must be closed (Future end)
                    self.stop = true;
                }
            }
            TunnelSocketWriteState::Write { message } => {
                // Try send the message to the local target
                let result = ready!(self.socket.poll_send(cx, message));

                if let Ok(count) = result {
                    // Didnt write entire messsage
                    if count != message.len() {
                        // Continue with a writing state at the remaining message
                        let message = message.slice(count..);
                        self.write_state = TunnelSocketWriteState::Write { message };
                    } else {
                        self.write_state = TunnelSocketWriteState::Recv;
                    }
                } else {
                    self.stop = true;
                }
            }
        }

        Poll::Ready(())
    }

    fn poll_read_state(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        let mut read_buf = ReadBuf::new(&mut self.read_buffer);

        // Try receive a message from the socket
        let result = ready!(self.socket.poll_recv(cx, &mut read_buf));

        if result.is_ok() {
            // Get the recieved message
            let bytes = read_buf.filled();
            let message = Bytes::copy_from_slice(bytes);

            // Send the message through the tunnel
            if self
                .tx
                .send(TunnelMessage {
                    index: self.index,
                    message,
                })
                .is_err()
            {
                // Tunnel has closed
                self.stop = true;
            }
        } else {
            // Reader has closed or reading encountered an error (Either way stop reading)
            self.stop = true;
        }

        Poll::Ready(())
    }
}

/// Allocates a pool of sockets for the tunnel to use
pub async fn allocate_pool(
    tun_tx: mpsc::UnboundedSender<TunnelMessage>,
) -> std::io::Result<[TunnelSocketHandle; 4]> {
    let (a, b, c, d) = try_join!(
        // Host tunnel index *must* use a fixed port since its used on the server side
        UdpSocket::bind((Ipv4Addr::LOCALHOST, TUNNEL_HOST_PORT)),
        UdpSocket::bind((Ipv4Addr::LOCALHOST, RANDOM_PORT)),
        UdpSocket::bind((Ipv4Addr::LOCALHOST, RANDOM_PORT)),
        UdpSocket::bind((Ipv4Addr::LOCALHOST, RANDOM_PORT))
    )?;

    // Set the send addresses for the sockets
    try_join!(
        a.connect(LOCAL_SEND_TARGET),
        b.connect(LOCAL_SEND_TARGET),
        c.connect(LOCAL_SEND_TARGET),
        d.connect(LOCAL_SEND_TARGET),
    )?;

    let sockets: [TunnelSocketHandle; 4] = [
        TunnelSocket::start(0, a, tun_tx.clone()),
        TunnelSocket::start(1, b, tun_tx.clone()),
        TunnelSocket::start(2, c, tun_tx.clone()),
        TunnelSocket::start(3, d, tun_tx),
    ];
    Ok(sockets)
}

/// Encoding an decoding logic for tunnel packet messages
mod codec {
    use bytes::{Buf, BufMut, Bytes};
    use tokio_util::codec::{Decoder, Encoder};

    /// Partially decoded tunnnel message
    pub struct TunnelMessagePartial {
        /// Socket index to use
        pub index: u8,
        /// The length of the tunnel message bytes
        pub length: u32,
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
        /// Stores a partially decoded frame if one is present
        partial: Option<TunnelMessagePartial>,
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
                    let length = src.get_u32();

                    self.partial.insert(TunnelMessagePartial { index, length })
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
            dst.put_u32(item.message.len() as u32);
            dst.extend_from_slice(&item.message);
            Ok(())
        }
    }
}
