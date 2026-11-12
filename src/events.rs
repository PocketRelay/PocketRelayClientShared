//! Event types for any events that servers may need to emit

/// Events that tunnel servers can emit
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelEvent {
    /// Creating the tunnel server
    Creating,

    /// Performing a handshake to try and connect
    /// (UDP tunnel only, the HTTP tunnel does not have a handshake to track it goes from creating -> connected)
    Handshaking,

    /// Currently connected
    Connected,

    /// Attempting to reconnect
    RetryConnect,

    /// Error occurred while connecting and unable to connect, reached limit
    /// of connection retries
    Error,
}
