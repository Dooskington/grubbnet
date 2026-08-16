extern crate byteorder;
extern crate mio;

use crate::buffer::NetworkBuffer;
use crate::Error;
use byteorder::{NetworkEndian, ReadBytesExt, WriteBytesExt};
use std::any::Any;
use std::io::Cursor;

pub const PACKET_HEADER_SIZE: usize = 3; // 2 bytes for size, 1 byte for id
pub const MAX_PACKET_BODY_SIZE: usize = 8192;
pub const MAX_PACKET_SIZE: usize = PACKET_HEADER_SIZE + MAX_PACKET_BODY_SIZE;

/// PacketHeader
/// The header included with every packet. Contains the packet body size and packet id.
#[derive(Clone, Debug)]
pub struct PacketHeader {
    pub size: u16,
    pub id: u8,
}

/// PacketBody
/// Implementors of this trait can be serialized into a packet body.
pub trait PacketBody: Any + Send + Sync {
    fn box_clone(&self) -> Box<dyn PacketBody>;

    fn serialize(&self) -> Result<Vec<u8>, Error>;
    fn deserialize(data: &[u8]) -> Result<Self, Error>
    where
        Self: Sized;
    fn id(&self) -> u8;
}

impl Clone for Box<dyn PacketBody> {
    fn clone(&self) -> Box<dyn PacketBody> {
        self.box_clone()
    }
}

/// Packet
/// A header and a variable size body.
#[derive(Clone)]
pub struct Packet {
    pub header: PacketHeader,
    pub body: Vec<u8>,
}

/// Serializes `body` into a complete packet, header included.
///
/// Returns [`Error::PacketTooLarge`] if the body reaches [`MAX_PACKET_BODY_SIZE`], mirroring
/// the check [`parse_packet_header`] applies on the way in. Without it a body over 65535 wraps
/// the 16 bit length field, and the peer reads the middle of the body as the next header and
/// desynchronises for the rest of the connection.
pub fn serialize_packet(body: Box<dyn PacketBody>) -> Result<Vec<u8>, Error> {
    // Serialize the packet body first so we know the size
    let mut body_data: Vec<u8> = body.serialize()?;

    if body_data.len() >= MAX_PACKET_BODY_SIZE {
        return Err(Error::PacketTooLarge(body_data.len(), MAX_PACKET_BODY_SIZE));
    }

    // Create payload and write header (body size and id)
    let mut data: Vec<u8> = Vec::new();
    data.write_u16::<NetworkEndian>(body_data.len() as u16)?;
    data.write_u8(body.id())?;

    // TODO (Declan, 4/26/2019)
    // Need to add some sort of magic number to the header to make sure the packet was meant for us

    // Combine the body and header
    data.append(&mut body_data);

    Ok(data)
}

/// The outcome of attempting to read a packet header off the front of a buffer.
///
/// The public [`deserialize_packet_header`] collapses this to a `Result`, which cannot express
/// the difference between "the rest of the header has not arrived yet" and "the peer sent
/// something illegal". Internally that distinction matters: the first means wait for more
/// bytes, the second means drop the connection.
#[derive(Debug)]
pub(crate) enum HeaderParse {
    /// A complete, in-range header was parsed off the front of the buffer.
    Parsed(PacketHeader),
    /// Fewer than [`PACKET_HEADER_SIZE`] bytes are buffered. Wait for more data.
    Incomplete,
    /// The header declares a body larger than [`MAX_PACKET_BODY_SIZE`]. Protocol violation.
    Invalid,
}

/// Reads a packet header off the front of `buffer` without consuming it.
///
/// Never inspects a byte that has not actually been received: prior to 0.2.3 this read three
/// bytes unconditionally, so a one or two byte header was completed with stale bytes left over
/// from earlier traffic. Never logs either, so a peer dribbling partial headers cannot be used
/// to flood the host's logs.
pub(crate) fn parse_packet_header(buffer: &NetworkBuffer) -> HeaderParse {
    // Only bytes that have actually arrived may be parsed.
    if buffer.len() < PACKET_HEADER_SIZE {
        return HeaderParse::Incomplete;
    }

    let mut reader = Cursor::new(&buffer.data[..PACKET_HEADER_SIZE]);

    // Read body size
    let body_size = match reader.read_u16::<NetworkEndian>() {
        Ok(size) => size as usize,
        Err(_) => return HeaderParse::Incomplete,
    };

    // If the packet is too big, the caller should kick the client so we have some basic
    // protection from being overloaded
    if body_size >= MAX_PACKET_BODY_SIZE {
        return HeaderParse::Invalid;
    }

    // Read packet id
    let packet_id = match reader.read_u8() {
        Ok(id) => id,
        Err(_) => return HeaderParse::Incomplete,
    };

    HeaderParse::Parsed(PacketHeader {
        size: body_size as u16,
        id: packet_id,
    })
}

/// Reads a packet header off the front of `buffer` without consuming it.
///
/// Returns `Error::InvalidData` if fewer than [`PACKET_HEADER_SIZE`] bytes are buffered, or if
/// the header declares a body larger than [`MAX_PACKET_BODY_SIZE`].
pub fn deserialize_packet_header(buffer: &mut NetworkBuffer) -> Result<PacketHeader, Error> {
    match parse_packet_header(buffer) {
        HeaderParse::Parsed(header) => Ok(header),
        HeaderParse::Incomplete | HeaderParse::Invalid => Err(Error::InvalidData),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Body(Vec<u8>);

    impl PacketBody for Body {
        fn box_clone(&self) -> Box<dyn PacketBody> {
            Box::new(Body(self.0.clone()))
        }

        fn serialize(&self) -> Result<Vec<u8>, Error> {
            Ok(self.0.clone())
        }

        fn deserialize(data: &[u8]) -> Result<Self, Error> {
            Ok(Body(data.to_vec()))
        }

        fn id(&self) -> u8 {
            0x2A
        }
    }

    fn serialize_body_of(size: usize) -> Result<Vec<u8>, Error> {
        serialize_packet(Box::new(Body(vec![0u8; size])))
    }

    fn buffer_with(bytes: &[u8]) -> NetworkBuffer {
        let mut buffer = NetworkBuffer::new();
        buffer.data[..bytes.len()].copy_from_slice(bytes);
        buffer.offset = bytes.len();
        buffer
    }

    #[test]
    fn parses_a_complete_header() {
        let buffer = buffer_with(&[0x00, 0x10, 0x07]);

        match parse_packet_header(&buffer) {
            HeaderParse::Parsed(header) => {
                assert_eq!(header.size, 16);
                assert_eq!(header.id, 0x07);
            }
            other => panic!("expected a parsed header, got {:?}", other),
        }
    }

    #[test]
    fn parses_an_empty_body_header() {
        let buffer = buffer_with(&[0x00, 0x00, 0x00]);

        match parse_packet_header(&buffer) {
            HeaderParse::Parsed(header) => {
                assert_eq!(header.size, 0);
                assert_eq!(header.id, 0x00);
            }
            other => panic!("expected a parsed header, got {:?}", other),
        }
    }

    #[test]
    fn an_empty_buffer_is_incomplete() {
        let buffer = NetworkBuffer::new();
        assert!(matches!(
            parse_packet_header(&buffer),
            HeaderParse::Incomplete
        ));
    }

    /// Prior to 0.2.3 a partial header was completed with whatever stale bytes happened to be
    /// left in the buffer from earlier traffic.
    #[test]
    fn a_partial_header_is_never_completed_with_stale_bytes() {
        let mut buffer = NetworkBuffer::new();

        // Stale bytes from a previous packet that would decode to an enormous body size.
        buffer.data[..3].copy_from_slice(&[0xFF, 0xFF, 0xFF]);

        // ...but only two bytes have actually arrived.
        buffer.offset = 2;

        assert!(matches!(
            parse_packet_header(&buffer),
            HeaderParse::Incomplete
        ));
    }

    #[test]
    fn an_oversized_body_is_invalid() {
        let buffer = buffer_with(&[0xFF, 0xFF, 0x00]);
        assert!(matches!(parse_packet_header(&buffer), HeaderParse::Invalid));
    }

    #[test]
    fn a_body_at_the_size_limit_is_invalid() {
        let size = (MAX_PACKET_BODY_SIZE as u16).to_be_bytes();
        let buffer = buffer_with(&[size[0], size[1], 0x00]);
        assert!(matches!(parse_packet_header(&buffer), HeaderParse::Invalid));
    }

    #[test]
    fn the_largest_legal_body_is_accepted() {
        let size = (MAX_PACKET_BODY_SIZE as u16 - 1).to_be_bytes();
        let buffer = buffer_with(&[size[0], size[1], 0x00]);

        match parse_packet_header(&buffer) {
            HeaderParse::Parsed(header) => {
                assert_eq!(header.size as usize, MAX_PACKET_BODY_SIZE - 1)
            }
            other => panic!("expected a parsed header, got {:?}", other),
        }
    }

    #[test]
    fn the_public_wrapper_reports_both_failures_as_invalid_data() {
        let mut short = buffer_with(&[0x00]);
        assert!(matches!(
            deserialize_packet_header(&mut short),
            Err(Error::InvalidData)
        ));

        let mut oversized = buffer_with(&[0xFF, 0xFF, 0x00]);
        assert!(matches!(
            deserialize_packet_header(&mut oversized),
            Err(Error::InvalidData)
        ));
    }

    #[test]
    fn a_hostile_offset_does_not_produce_a_header() {
        // `offset` is public, so it can be anything. It must still never be trusted as a length.
        let mut buffer = NetworkBuffer::new();
        buffer.offset = usize::MAX;

        // Clamped to a full buffer of zeroes, which is a legal empty-body header.
        match parse_packet_header(&buffer) {
            HeaderParse::Parsed(header) => {
                assert_eq!(header.size, 0);
                assert_eq!(header.id, 0);
            }
            other => panic!("expected a parsed header, got {:?}", other),
        }
    }

    #[test]
    fn a_serialized_packet_round_trips_through_the_header_parser() {
        let data = serialize_packet(Box::new(Body(vec![1, 2, 3, 4, 5]))).unwrap();
        assert_eq!(data.len(), PACKET_HEADER_SIZE + 5);

        let buffer = buffer_with(&data);
        match parse_packet_header(&buffer) {
            HeaderParse::Parsed(header) => {
                assert_eq!(header.size, 5);
                assert_eq!(header.id, 0x2A);
            }
            other => panic!("expected a parsed header, got {:?}", other),
        }
    }

    #[test]
    fn the_largest_legal_body_is_serialized() {
        let data = serialize_body_of(MAX_PACKET_BODY_SIZE - 1).expect("body is within the limit");

        assert_eq!(data.len(), MAX_PACKET_SIZE - 1);

        let buffer = buffer_with(&data);
        match parse_packet_header(&buffer) {
            HeaderParse::Parsed(header) => {
                assert_eq!(header.size as usize, MAX_PACKET_BODY_SIZE - 1)
            }
            other => panic!("expected a parsed header, got {:?}", other),
        }
    }

    /// The read path rejects a header at the limit, so the write path must not produce one.
    #[test]
    fn a_body_at_the_size_limit_is_refused() {
        assert!(matches!(
            serialize_body_of(MAX_PACKET_BODY_SIZE),
            Err(Error::PacketTooLarge(size, limit))
                if size == MAX_PACKET_BODY_SIZE && limit == MAX_PACKET_BODY_SIZE
        ));
    }

    /// A body of 65536 casts to a length of 0, which framed the packet as empty and left the
    /// body to be parsed as headers, desynchronising the connection permanently.
    #[test]
    fn a_body_that_would_wrap_the_length_field_is_refused() {
        let wrapping_size = u16::MAX as usize + 1;

        assert!(matches!(
            serialize_body_of(wrapping_size),
            Err(Error::PacketTooLarge(size, _)) if size == wrapping_size
        ));
    }

    #[test]
    fn an_empty_body_is_serialized() {
        let data = serialize_body_of(0).expect("an empty body is legal");

        assert_eq!(data.len(), PACKET_HEADER_SIZE);
    }
}
