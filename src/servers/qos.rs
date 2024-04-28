//! Quality of Service server implementation, this is a pretty crude implementation
//! but it at least allows clients to sometimes obtain the correct address, usually
//! the server can fix it

use super::{spawn_server_task, QOS_PORT};
use log::debug;
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{net::UdpSocket, sync::RwLock};

/// Starts the Quality Of Service server which handles providing the public
/// address values to the clients that connect.
pub async fn start_qos_server() -> std::io::Result<()> {
    // Bind the local socket for accepting messages
    let socket: UdpSocket = UdpSocket::bind((Ipv4Addr::LOCALHOST, QOS_PORT)).await?;
    let socket: Arc<UdpSocket> = Arc::new(socket);

    // Buffer for reading incoming messages
    let mut buffer: [u8; 4096] = [0u8; 4096];

    // Accept messages
    loop {
        let (count, addr) = socket.recv_from(&mut buffer).await?;
        // Create an array from the data that was received
        let buffer: Box<[u8]> = Box::from(&buffer[..count]);

        spawn_server_task(handle(socket.clone(), addr, buffer));
    }
}

/// Handles a Quality of Service connection
///
/// ## Arguments
/// * `socket`      - The UDP socket used for sending the responses
/// * `socket_addr` - The socket address of the connection (Target for the response)
/// * `buffer`      - Buffer of bytes received from the socket
async fn handle(socket: Arc<UdpSocket>, socket_addr: SocketAddr, buffer: Box<[u8]>) {
    // Extract the IPv4 address from the socket address (Fallback to 0.0.0.0)
    let socket_ip = match socket_addr {
        SocketAddr::V4(addr) => *addr.ip(),
        _ => Ipv4Addr::UNSPECIFIED,
    };

    let address = public_address().await.unwrap_or(socket_ip);

    debug!("QoS: From: {} Resolved: {}", socket_addr, address);

    // Create buffer to store the output response
    let mut output = Vec::with_capacity(
        buffer.len() + 4 /* addr */ + 2 /* port */ + 4, /* padding */
    );

    output.extend_from_slice(&buffer);
    output.extend_from_slice(&address.octets());
    output.extend_from_slice(&socket_addr.port().to_be_bytes());
    output.extend_from_slice(&[0, 0, 0, 0]);

    // Send output response
    let _ = socket.send_to(&output, socket_addr).await;
}

/// Caching structure for the public address value
enum PublicAddrCache {
    /// The value hasn't yet been computed
    Unset,
    /// The value has been computed
    Set {
        /// The public address value
        value: Ipv4Addr,
        /// The system time the cache expires at
        expires: SystemTime,
    },
}

/// Cache value for storing the public address
static PUBLIC_ADDR_CACHE: RwLock<PublicAddrCache> = RwLock::const_new(PublicAddrCache::Unset);

/// Cache public address for 30 minutes
const ADDR_CACHE_TIME: Duration = Duration::from_secs(60 * 30);

/// Retrieves the public address of the server either using the cached
/// value if its not expired or fetching the new value from the one of
/// two possible APIs
async fn public_address() -> Option<Ipv4Addr> {
    {
        let cached = &*PUBLIC_ADDR_CACHE.read().await;
        if let PublicAddrCache::Set { value, expires } = cached {
            let time = SystemTime::now();
            if time.lt(expires) {
                return Some(*value);
            }
        }
    }

    // Hold the write lock to prevent others from attempting to update as well
    let cached = &mut *PUBLIC_ADDR_CACHE.write().await;

    // API addresses for IP lookup
    let addresses = ["https://api.ipify.org/", "https://ipv4.icanhazip.com/"];
    let mut value: Option<Ipv4Addr> = None;

    // Try all addresses using the first valid value
    for address in addresses {
        let Ok(response) = reqwest::get(address).await else {
            continue;
        };

        let Ok(response) = response.text().await else {
            continue;
        };

        let ip = response.trim().replace('\n', "");

        if let Ok(parsed) = ip.parse() {
            value = Some(parsed);
            break;
        }
    }

    // If we couldn't connect to any IP services its likely
    // we don't have internet lets try using our local address
    if value.is_none() {
        if let Ok(IpAddr::V4(addr)) = local_ip_address::local_ip() {
            value = Some(addr);
        }
    }

    let value = value?;

    // Update cached value with the new address

    *cached = PublicAddrCache::Set {
        value,
        expires: SystemTime::now() + ADDR_CACHE_TIME,
    };

    Some(value)
}
