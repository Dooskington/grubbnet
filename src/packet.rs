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
#[derive(Clone)]
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

pub fn serialize_packet(body: Box<dyn PacketBody>) -> Result<Vec<u8>, Error> {
    // Serialize the packet body first so we know the size
    let mut body_data: Vec<u8> = body.serialize()?;

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

pub fn deserialize_packet_header(buffer: &mut NetworkBuffer) -> Result<PacketHeader, Error> {
    let mut reader = Cursor::new(&buffer.data[..]);

    // Read body size
    let body_size = reader.read_u16::<NetworkEndian>()? as usize;

    // If the packet is too big, kick the client so we have some basic protection from being overloaded
    if body_size >= MAX_PACKET_BODY_SIZE {
        eprintln!(
            "Packet body is {} bytes, but max body size is ({} bytes)!",
            body_size, MAX_PACKET_BODY_SIZE
        );

        return Err(Error::InvalidData);
    }

    // Read packet id
    let packet_id = reader.read_u8()?;

    let header = PacketHeader {
        size: body_size as u16,
        id: packet_id,
    };

    Ok(header)
}
