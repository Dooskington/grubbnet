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
///
/// Returns the number of bytes actually written. This may be fewer than
/// `buffer.len()` if the socket's send buffer filled up (a `WouldBlock`
/// condition on a nonblocking socket), in which case the caller is responsible
/// for retrying the unsent remainder on the next writable event. Only returns
/// an `Error` on a genuine I/O failure.
pub fn send_bytes(socket: &mut TcpStream, buffer: &[u8]) -> Result<usize> {
    write_bytes(socket, buffer)
}

/// Writes `buffer` to `socket`, advancing past partially written chunks.
///
/// Generic over `Write` so the partial-write and `WouldBlock` handling can be
/// unit tested without a real socket.
fn write_bytes<W: Write>(socket: &mut W, buffer: &[u8]) -> Result<usize> {
    if buffer.is_empty() {
        return Err(Error::InvalidData);
    }

    let mut total_sent: usize = 0;
    while total_sent < buffer.len() {
        match socket.write(&buffer[total_sent..]) {
            Ok(0) => {
                // The socket isn't able to accept any more bytes right now.
                break;
            }
            Ok(sent_bytes) => {
                total_sent += sent_bytes;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // The send buffer is full. Stop here and let the caller retry
                // the remaining bytes once the socket is writable again.
                break;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Interrupted before any bytes were written, so just retry.
                continue;
            }
            Err(e) => {
                return Err(e.into());
            }
        }
    }

    Ok(total_sent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::io::{self, ErrorKind, Write};

    /// A `Write` implementation that returns a scripted sequence of results,
    /// recording everything it accepts so tests can assert on byte ordering.
    struct MockWriter {
        results: VecDeque<io::Result<usize>>,
        written: Vec<u8>,
    }

    impl MockWriter {
        fn new(results: Vec<io::Result<usize>>) -> Self {
            MockWriter {
                results: results.into_iter().collect(),
                written: Vec::new(),
            }
        }
    }

    impl Write for MockWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match self.results.pop_front() {
                Some(Ok(n)) => {
                    let n = n.min(buf.len());
                    self.written.extend_from_slice(&buf[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                // Default once the script is exhausted: accept everything.
                None => {
                    self.written.extend_from_slice(buf);
                    Ok(buf.len())
                }
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn empty_buffer_is_invalid() {
        let mut w = MockWriter::new(vec![]);
        assert!(matches!(write_bytes(&mut w, &[]), Err(Error::InvalidData)));
    }

    #[test]
    fn full_write_in_one_call() {
        let mut w = MockWriter::new(vec![Ok(5)]);
        let sent = write_bytes(&mut w, b"hello").unwrap();
        assert_eq!(sent, 5);
        assert_eq!(w.written, b"hello");
    }

    #[test]
    fn partial_writes_advance_the_slice() {
        // Two partial writes should not re-send already sent bytes.
        let mut w = MockWriter::new(vec![Ok(2), Ok(3)]);
        let sent = write_bytes(&mut w, b"hello").unwrap();
        assert_eq!(sent, 5);
        assert_eq!(w.written, b"hello");
    }

    #[test]
    fn would_block_stops_without_error_and_preserves_remainder() {
        // Send 2 bytes, then the socket would block.
        let mut w = MockWriter::new(vec![Ok(2), Err(io::Error::from(ErrorKind::WouldBlock))]);
        let sent = write_bytes(&mut w, b"hello").unwrap();
        assert_eq!(sent, 2);
        assert_eq!(w.written, b"he");

        // The caller retries the remainder on the next writable event.
        let mut w2 = MockWriter::new(vec![Ok(3)]);
        let sent2 = write_bytes(&mut w2, b"llo").unwrap();
        assert_eq!(sent2, 3);
        assert_eq!(w2.written, b"llo");
    }

    #[test]
    fn would_block_immediately_sends_nothing() {
        let mut w = MockWriter::new(vec![Err(io::Error::from(ErrorKind::WouldBlock))]);
        let sent = write_bytes(&mut w, b"hello").unwrap();
        assert_eq!(sent, 0);
        assert!(w.written.is_empty());
    }

    #[test]
    fn interrupted_is_retried() {
        let mut w = MockWriter::new(vec![Err(io::Error::from(ErrorKind::Interrupted)), Ok(5)]);
        let sent = write_bytes(&mut w, b"hello").unwrap();
        assert_eq!(sent, 5);
        assert_eq!(w.written, b"hello");
    }

    #[test]
    fn real_error_is_returned() {
        let mut w = MockWriter::new(vec![Err(io::Error::from(ErrorKind::ConnectionReset))]);
        match write_bytes(&mut w, b"hello") {
            Err(Error::Io(e)) => assert_eq!(e.kind(), ErrorKind::ConnectionReset),
            other => panic!("expected Io error, got {:?}", other),
        }
    }
}
