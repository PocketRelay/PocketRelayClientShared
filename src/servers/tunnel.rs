use std::{
    net::{Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
};

use bytes::{Buf, BufMut, Bytes};
use futures::{SinkExt, StreamExt};
use log::debug;
use reqwest::Upgraded;
use tokio::{net::UdpSocket, select, sync::mpsc, try_join};
use tokio_util::codec::{Decoder, Encoder, Framed};
use url::Url;

use crate::api::create_server_tunnel;
///
pub const TUNNEL_HOST_PORT: u16 = 42132;

///
pub async fn start_tunnel(http_client: reqwest::Client, base_url: Arc<Url>) -> std::io::Result<()> {
    // Connect the tunnel
    debug!("Allocated tunnel");
    let tunnel = create_server_tunnel(http_client, &base_url).await.unwrap();
    let tunnel = Framed::new(tunnel, TunnelCodec { partial: None });
    let (tx, rx) = mpsc::unbounded_channel();
    let pool = allocate_pool(tx).await.unwrap();
    debug!("Allocated tunnel pool");
    let tunnel = Tunnel {
        io: tunnel,
        rx,
        pool,
    };
    tunnel.handle().await;
    Ok(())
}

///
pub struct Tunnel {
    io: Framed<Upgraded, TunnelCodec>,
    // Reciever for messages that need to be sent
    rx: mpsc::UnboundedReceiver<TunnelMessage>,
    // Pool of sockets for sending messages and recieving respones
    pool: [TunnelSocketHandle; 4],
}

impl Tunnel {
    ///
    pub async fn handle(mut self) {
        loop {
            select! {
                // Recv messages thru tunnel
                message = self.io.next() => {
                    if let Some(Ok(message)) = message {
                        debug!("Recv message through tunnel");
                        if let Some(handle) = self.pool.get(message.index as usize) {
                            handle.0.send(message).unwrap();
                        }
                    } else {

                        debug!("Tunnnel droooped");
                        break;
                    }
                }

                // Sending messages through tunnel
                message = self.rx.recv() => {
                    if let Some(message) = message {
                    debug!("Sending message through tunnel");
                    self.io.send(message).await.unwrap();
                    } else {
                        debug!("Tunnnel rx droooped");
                        break;
                    }
                }
            }
        }
    }
}

///
#[derive(Clone)]
pub struct TunnelSocketHandle(mpsc::UnboundedSender<TunnelMessage>);

///
pub struct TunnelSocket {
    // Index of the socket
    index: u8,
    // Underlying socket
    socket: UdpSocket,
    // Recv messages to write to the socket
    rx: mpsc::UnboundedReceiver<TunnelMessage>,
    // Send messages to write to the tunnel
    tx: mpsc::UnboundedSender<TunnelMessage>,
}

impl TunnelSocket {
    ///
    pub fn start(
        index: u8,
        socket: UdpSocket,
        tun_tx: mpsc::UnboundedSender<TunnelMessage>,
    ) -> TunnelSocketHandle {
        let (tx, rx) = mpsc::unbounded_channel();

        tokio::spawn(async move {
            let socket = TunnelSocket {
                index,
                socket,
                rx,
                tx: tun_tx,
            };
            socket.handle().await;
        });

        TunnelSocketHandle(tx)
    }
    ///
    pub async fn handle(mut self) {
        let mut recv_buffer: [u8; 65536] = [0u8; 65536];

        loop {
            select! {
                result = self.socket.recv(&mut recv_buffer) => {

                    if let Ok(count) = result {
                        if self.index != 0 {
                        debug!("Pool message recv");
                        let buf =Bytes::copy_from_slice(&recv_buffer[..count]);
                    self.tx
                    .send(TunnelMessage {
                        index: self.index,
                        message: buf,
                    })
                    .unwrap();
                    } else{
                        debug!("Pool message send to host");
                        self.socket.send_to(
                            &recv_buffer[..count],
                            SocketAddr::V4(SocketAddrV4::new(
                                Ipv4Addr::LOCALHOST,
                                3659, /* Todo update this in protocol later to use the local addr */
                            )),
                        )
                        .await
                        .unwrap();
                    }
                    } else {
                        debug!("Tunnnel socket error");
                        break;
                    }
                }

                message = self.rx.recv() => {
                    if let Some(message) = message {
                        debug!("Pool message send");
                        self.socket.send_to(
                            &message.message,
                            SocketAddr::V4(SocketAddrV4::new(
                                Ipv4Addr::LOCALHOST,
                                3659, /* Todo update this in protocol later to use the local addr */
                            )),
                        )
                        .await
                        .unwrap();
                    } else {
                        debug!("Tunnnel socket rx error");
                        break;
                    }
                }
            };
        }
    }
}

/// Allocates a pool of sockets for the tunnel to use
pub async fn allocate_pool(
    tun_tx: mpsc::UnboundedSender<TunnelMessage>,
) -> std::io::Result<[TunnelSocketHandle; 4]> {
    let (a, b, c, d) = try_join!(
        // Host tunnel index *must* use a fixed port since its used on the server side
        UdpSocket::bind((Ipv4Addr::LOCALHOST, TUNNEL_HOST_PORT)),
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 42133)),
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 42134)),
        UdpSocket::bind((Ipv4Addr::LOCALHOST, 42135))
    )?;
    let sockets: [TunnelSocketHandle; 4] = [
        TunnelSocket::start(0, a, tun_tx.clone()),
        TunnelSocket::start(1, b, tun_tx.clone()),
        TunnelSocket::start(2, c, tun_tx.clone()),
        TunnelSocket::start(3, d, tun_tx),
    ];
    Ok(sockets)
}

/// Partially decoded tunnnel message
pub struct TunnelMessagePartial {
    ///
    pub index: u8,
    ///
    pub length: u32,
}

/// Message sent through the tunnel
pub struct TunnelMessage {
    /// Socket index to use
    pub index: u8,
    /// The message contents
    pub message: Bytes,
}

///
pub struct TunnelCodec {
    ///
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
