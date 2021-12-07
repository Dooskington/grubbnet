mod client;
mod error;
mod server;

pub mod buffer;
pub mod packet;

#[cfg(feature = "crypto")]
pub mod crypto;

use mio::net::TcpStream;
use std::io::Write;

pub use client::{Client, ClientEvent};
pub use error::{Error, Result};
pub use mio::Token;
pub use server::{Server, ServerEvent};

pub enum PacketRecipient {
    All,
    Single(Token),
    Exclude(Token),
    ExcludeMany(Vec<Token>),
    Include(Vec<Token>),
}

/// Send some bytes to a socket.
/// Returns the number of bytes sent, or an `Error`.
pub fn send_bytes(socket: &mut TcpStream, buffer: &[u8]) -> Result<usize> {
    let mut len = buffer.len();
    if len == 0 {
        return Err(Error::InvalidData);
    }

    // Keep sending until we've sent the entire buffer
    while len > 0 {
        match socket.write(buffer) {
            Ok(sent_bytes) => {
                len -= sent_bytes;
            }
            Err(_) => {
                return Err(Error::FailedToSendBytes);
            }
        }
    }

    Ok(buffer.len())
}
