//! Minimal fire packet parsing and creation implementation
//! this supports the very minimal required features

use bytes::{Buf, BufMut, Bytes};
use std::io;
use tdf::{serialize_vec, TdfSerialize};
use tokio_util::codec::{Decoder, Encoder};

/// The different types of frames that can be sent
#[repr(u8)]
pub enum FrameType {
    /// Request to a server
    Request = 0x0,
    /// Response to a request
    Response = 0x1,
    /// Async notification from the server
    Notify = 0x2,
    /// Error response from the server
    Error = 0x3,
}

/// Conversion for bytes to frame type
impl From<u8> for FrameType {
    fn from(value: u8) -> Self {
        match value {
            0x1 => FrameType::Response,
            0x2 => FrameType::Notify,
            0x3 => FrameType::Error,
            _ => FrameType::Request,
        }
    }
}

/// The header for a fire frame
pub struct FrameHeader {
    /// The length of the frame contents
    pub length: usize,
    /// The component that should handle this frame
    pub component: u16,
    /// The command that should handle this frame
    pub command: u16,
    /// Error code if this is an error frame
    pub error: u16,
    /// The type of frame
    pub ty: FrameType,
    /// Frame options bitset
    pub options: u8,
    /// Sequence number for tracking request and response mappings
    pub seq: u16,
}

/// Packet framing structure
pub struct Frame {
    /// Header for the frame
    pub header: FrameHeader,
    /// The encoded byte contents of the packet
    pub contents: Bytes,
}

impl Frame {
    /// Creates a new response frame responding to the provided `header`
    /// with the bytes contents of `value`
    ///
    /// ## Arguments
    /// * `header`   - The header of the frame to respond to
    /// * `contents` - The bytes of the frame
    pub fn response_raw(header: &FrameHeader, contents: Bytes) -> Frame {
        Frame {
            header: FrameHeader {
                length: contents.len(),
                component: header.component,
                command: header.command,
                error: 0,
                ty: FrameType::Response,
                options: 0,
                seq: header.seq,
            },
            contents,
        }
    }

    /// Creates a new response frame responding to the provided `header`
    /// with the encoded contents of the `value`
    ///
    /// ## Arguments
    /// * `header` - The header of the frame to respond to
    /// * `value`  - The value to encode as the frame bytes
    #[inline]
    pub fn response<V>(header: &FrameHeader, value: V) -> Frame
    where
        V: TdfSerialize,
    {
        Self::response_raw(header, Bytes::from(serialize_vec(&value)))
    }

    /// Creates a new response frame responding to the provided `header`
    /// with empty contents
    ///
    /// ## Arguments
    /// * `header` - The header of the frame to respond to
    pub fn response_empty(header: &FrameHeader) -> Frame {
        Self::response_raw(header, Bytes::new())
    }
}

/// Codec for encoding and decoding fire frames
#[derive(Default)]
pub struct FireCodec {
    /// Incomplete frame thats currently being read
    current_frame: Option<FrameHeader>,
}

impl FireCodec {
    const MIN_HEADER_SIZE: usize = 12;
}

impl Decoder for FireCodec {
    // The codec doesn't have any errors of its own so IO error is used
    type Error = io::Error;
    // The decoder provides fire frames
    type Item = Frame;

    fn decode(&mut self, src: &mut bytes::BytesMut) -> Result<Option<Self::Item>, Self::Error> {
        let current_frame = if let Some(current_frame) = self.current_frame.as_mut() {
            // Use the existing frame
            current_frame
        } else {
            // Ensure there is at least enough bytes for the header
            if src.len() < Self::MIN_HEADER_SIZE {
                return Ok(None);
            }

            // Read the length of the fire frame
            let length: usize = src.get_u16() as usize;
            let component: u16 = src.get_u16();
            let command: u16 = src.get_u16();
            let error: u16 = src.get_u16();
            let ty: FrameType = FrameType::from(src.get_u8() >> 4);
            let options: u8 = src.get_u8() >> 4;
            let seq: u16 = src.get_u16();

            let header = FrameHeader {
                length,
                component,
                command,
                error,
                ty,
                options,
                seq,
            };

            self.current_frame.insert(header)
        };

        // Ensure there are enough bytes for the entire frame
        if src.len() < current_frame.length {
            return Ok(None);
        }

        // Take the frame we are currently working on
        let header = self.current_frame.take().expect("Missing current frame");
        // Take all the frame bytes
        let buffer = src.split_to(header.length);

        Ok(Some(Frame {
            header,
            contents: buffer.freeze(),
        }))
    }
}

impl Encoder<Frame> for FireCodec {
    type Error = io::Error;

    fn encode(&mut self, item: Frame, dst: &mut bytes::BytesMut) -> Result<(), Self::Error> {
        let header = item.header;
        dst.put_u16(header.length as u16);
        dst.put_u16(header.component);
        dst.put_u16(header.command);
        dst.put_u16(header.error);
        dst.put_u8((header.ty as u8) << 4);
        dst.put_u8(header.options << 4);
        dst.put_u16(header.seq);

        dst.extend_from_slice(&item.contents);
        Ok(())
    }
}
