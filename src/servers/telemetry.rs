//! Server for handling decoding game client telemetry messages and
//! forwarding them to the connect Pocket Relay server

use super::{spawn_server_task, TELEMETRY_PORT};
use crate::api::{publish_telemetry_event, TelemetryEvent};
use log::error;
use std::{net::Ipv4Addr, sync::Arc};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
};
use url::Url;

/// Starts the telemetry server
///
/// ## Arguments
/// * http_client - The HTTP client used to forward messages
/// * base_url    - The server base URL to connect clients to
pub async fn start_telemetry_server(
    http_client: reqwest::Client,
    base_url: Arc<Url>,
) -> std::io::Result<()> {
    // Bind the local socket for accepting connections
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, TELEMETRY_PORT)).await?;

    // Accept connections
    loop {
        let (client_stream, _) = listener.accept().await?;

        spawn_server_task(handle(client_stream, http_client.clone(), base_url.clone()));
    }
}

/// Handler for processing telemetry client connections
async fn handle(mut client_stream: TcpStream, http_client: reqwest::Client, base_url: Arc<Url>) {
    while let Ok(event) = read_telemetry_event(&mut client_stream).await {
        if let Err(err) = publish_telemetry_event(&http_client, &base_url, event).await {
            error!("Failed to publish telemetry event: {}", err);
        }
    }
}

/// Xor cipher key used for decoding the TML3 telemetry line message
const TLM3_KEY: &[u8] = b"The truth is back in style.";

/// Reads and decodes a telemetry event from the provided
/// stream
///
/// ## Arguments
/// * stream - The stream to decode from
async fn read_telemetry_event(stream: &mut TcpStream) -> std::io::Result<TelemetryEvent> {
    let length: usize = {
        // Buffer for reading the header + padding + length bytes
        let mut header: [u8; 12] = [0u8; 12];
        stream.read_exact(&mut header).await?;

        // Copy the length portion of the header
        let mut length_bytes = [0u8; 2];
        length_bytes.copy_from_slice(&header[10..]);

        let length: u16 = u16::from_be_bytes(length_bytes);

        // Remove header size from length
        (length - 12) as usize
    };

    // Allocate a buffer to fit the entire length
    let mut buffer = vec![0u8; length];
    stream.read_exact(&mut buffer).await?;

    // Split the buffer into pairs of values
    let values: Vec<(String, String)> = buffer
        // Split the message into new lines
        .split(|value| b'\n'.eq(value))
        // Filter only on the key=value pair lines
        .filter_map(|slice| {
            let mut parts = slice.splitn(2, |value| b'='.eq(value));
            let first = parts.next()?;
            let second = parts.next()?;
            Some((first, second))
        })
        // Handle decoding the values
        .map(|(key, value)| {
            let key = String::from_utf8_lossy(key).to_string();
            let value = if key.eq("TLM3") {
                tlm3(value)
            } else {
                format!("{:?}", value)
            };

            (key, value)
        })
        .collect();

    Ok(TelemetryEvent { values })
}

/// Decodes a TLM3 line using the [`TLM3_KEY`]
///
/// ## Arguments
/// * input - The line to decode
fn tlm3(input: &[u8]) -> String {
    input
        .splitn(2, |value| b'-'.eq(value))
        .nth(1)
        .map(|line| {
            let value = xor_cipher(line, TLM3_KEY);
            // Safety: Characters from the xor_cipher are within the valid utf-8 range
            unsafe { String::from_utf8_unchecked(value) }
        })
        // Stringify the input if parsing fails
        .unwrap_or_else(|| format!("{:?}", input))
}

/// Applies an xor cipher over the input using the provided key
///
/// ## Arguments
/// * input - The input value to xor
/// * key   - The key to xor with
fn xor_cipher(input: &[u8], key: &[u8]) -> Vec<u8> {
    input
        .iter()
        // Copy the data bytes
        .copied()
        // Iterate along-side the key
        .zip(key.iter().cycle().copied())
        // Process the next value using the key
        .map(|(data, key)| ((data ^ key) % 0x80))
        // Collect the processed bytes
        .collect()
}

#[cfg(test)]
mod test {
    use crate::servers::telemetry::tlm3;

    use super::{xor_cipher, TLM3_KEY};

    /// Ensures that data put through the cipher matches the expected
    /// result that was passed in as input
    #[test]
    fn test_xor_cipher() {
        // Data that should be decodable
        let test_data =
            "123456789:;<=>?@ABCDEFGHIJKLMNOPQRSTUVWXYZ[\\]^_`abcdefghijklmnopqrstuvwxyz{|}~";

        let enc_data = xor_cipher(test_data.as_bytes(), TLM3_KEY);
        let dec_data = xor_cipher(&enc_data, TLM3_KEY);

        assert_eq!(&dec_data, test_data.as_bytes());
    }

    /// Tests that the cipher is capable of decoding some known data
    /// sent by the game
    #[test]
    fn test_known_data() {
        let enc_data = &[
            100, 88, 85, 144, 68, 64, 49, 50, 71, 141, 82, 67, 144, 82, 81, 83, 91, 146, 91, 65,
            98, 60, 59, 45, 67, 54, 107, 135, 59, 74, 111, 56, 60, 50, 91, 30, 76, 135, 148, 29,
            43, 47, 55, 77, 84, 133, 128, 71, 78, 189, 55, 56, 73, 30, 100, 88, 85, 144, 70, 54,
            51, 91, 69, 27, 89, 67, 144, 82, 81, 83, 89, 147, 70, 33, 110, 63, 58, 86, 46, 41, 111,
            142, 71, 33, 99, 59, 60, 90, 22, 141, 82, 27, 78, 141, 80, 80, 87, 93, 22, 90, 95, 22,
            75, 68, 95, 138, 22, 90, 53, 85, 84, 145, 82, 134, 134, 128, 137, 29, 90, 85, 83, 135,
            146, 144, 86, 80, 138, 25, 68, 25, 128, 54, 47, 51, 94, 144, 104,
        ];
        let expected = "000002DF/-;00000022/BOOT/SESS/OLNG/vlng=INT&tlng=INT,000002DF/-;00000023/ONLN/BLAZ/DCON/berr=-2146631680&fsta=11&tsta=3&sess=pcwdjtOCVpD\0";
        let dec_data = xor_cipher(enc_data, TLM3_KEY);

        assert_eq!(&dec_data, expected.as_bytes());
    }

    /// Tests that an entire TLM3 example can be correctly decoded
    #[test]
    fn test_tlm3_line() {
        let enc_data = &mut [
            64, 56, 97, 45, 100, 88, 85, 144, 68, 64, 49, 50, 71, 141, 82, 67, 144, 82, 81, 83, 91,
            146, 91, 65, 98, 60, 59, 45, 67, 54, 107, 135, 59, 74, 111, 56, 60, 50, 91, 30, 76,
            135, 148, 29, 43, 47, 55, 77, 84, 133, 128, 71, 78, 189, 55, 56, 73, 30, 100, 88, 85,
            144, 70, 54, 51, 91, 69, 27, 89, 67, 144, 82, 81, 83, 89, 147, 70, 33, 110, 63, 58, 86,
            46, 41, 111, 142, 71, 33, 99, 59, 60, 90, 22, 141, 82, 27, 78, 141, 80, 80, 87, 93, 22,
            90, 95, 22, 75, 68, 95, 138, 22, 90, 53, 85, 84, 145, 82, 134, 134, 128, 137, 29, 90,
            85, 83, 135, 146, 144, 86, 80, 138, 25, 68, 25, 128, 54, 47, 51, 94, 144, 104,
        ];
        let expected = "000002DF/-;00000022/BOOT/SESS/OLNG/vlng=INT&tlng=INT,000002DF/-;00000023/ONLN/BLAZ/DCON/berr=-2146631680&fsta=11&tsta=3&sess=pcwdjtOCVpD\0";
        let dec_data = tlm3(enc_data);
        assert_eq!(dec_data.as_bytes(), expected.as_bytes());
    }
}
