mod client;
mod error;
mod server;

pub mod buffer;
pub mod packet;

#[cfg(feature = "crypto")]
pub mod crypto;

use mio::net::TcpStream;
use std::io::{Read, Write};

pub use client::{Client, ClientEvent};
pub use error::{Error, Result};
pub use mio::Token;
pub use server::{Server, ServerEvent};

use crate::buffer::{NetworkBuffer, MAX_BUFFER_SIZE};

pub enum PacketRecipient {
    All,
    Single(Token),
    Exclude(Token),
    ExcludeMany(Vec<Token>),
    Include(Vec<Token>),
}

/// Why a read loop stopped pulling bytes off a socket.
#[derive(Debug)]
pub(crate) enum ReadOutcome {
    /// The socket has no more bytes available right now. This is the normal exit.
    WouldBlock,
    /// The peer performed an orderly shutdown.
    Closed,
    /// The buffer filled up before the socket ran dry.
    ///
    /// `MAX_BUFFER_SIZE` is roughly twice `MAX_PACKET_SIZE`, so a well behaved peer can never
    /// fill the buffer without also having put at least one complete packet in it. If the
    /// buffer is still full once the caller has drained every complete packet, the peer is
    /// misbehaving and the connection should be dropped.
    BufferFull,
    /// A genuine I/O error.
    Error(std::io::Error),
}

/// Reads from `reader` into `buffer` until the socket runs dry or the buffer fills up.
///
/// Generic over `Read` so the loop's bounds handling can be unit tested without a real socket.
///
/// The destination slice is recomputed on every iteration. Hoisting it out of the loop freezes
/// its length at `MAX_BUFFER_SIZE - offset_at_entry` while `offset` keeps accumulating inside
/// the loop, which let a single unauthenticated 64 KB burst drive `offset` to 65536 on a
/// 16384-byte array. Every length derived from that offset afterwards - notably the copy length
/// in `NetworkBuffer::drain` - was then out of bounds. Do not hoist it.
pub(crate) fn read_into_buffer<R: Read>(reader: &mut R, buffer: &mut NetworkBuffer) -> ReadOutcome {
    // `offset` is a public field, so it may hold anything on entry. Re-establish the type
    // invariant up front so that it holds unconditionally on exit too.
    buffer.normalize();

    loop {
        // Never hand `read` a zero length slice. `Read::read` is documented to return `Ok(0)`
        // for an empty destination, which is indistinguishable from an orderly peer shutdown,
        // so a full buffer would otherwise be reported as a graceful disconnect.
        if buffer.is_full() {
            return ReadOutcome::BufferFull;
        }

        let destination = buffer.writable();
        let capacity = destination.len();

        match reader.read(destination) {
            Ok(0) => return ReadOutcome::Closed,
            Ok(read_bytes) => {
                // Hard invariant: the offset must never leave the backing array. `advance`
                // clamps at runtime; this catches a lying `Read` implementation in dev builds.
                debug_assert!(
                    read_bytes <= capacity,
                    "Read reported {} bytes into a {} byte slice",
                    read_bytes,
                    capacity
                );

                buffer.advance(read_bytes);

                debug_assert!(
                    buffer.offset <= MAX_BUFFER_SIZE,
                    "Buffer offset ({}) escaped the backing array ({} bytes)",
                    buffer.offset,
                    MAX_BUFFER_SIZE
                );
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                // Interrupted before any bytes were read, so just retry.
                continue;
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Socket is not ready anymore, stop reading.
                return ReadOutcome::WouldBlock;
            }
            Err(e) => return ReadOutcome::Error(e),
        }
    }
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
    use std::io::{self, ErrorKind, Read, Write};

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

    /// A `Read` implementation that returns a scripted sequence of results, mirroring
    /// `MockWriter`. `Ok(n)` fills the destination with `n` bytes of `fill`, saturated at the
    /// destination's length - a real `Read` can never report more bytes than the slice holds,
    /// and pretending otherwise would only test the clamp already covered in `buffer.rs`.
    struct MockReader {
        results: VecDeque<io::Result<usize>>,
        fill: u8,
    }

    impl MockReader {
        fn new(results: Vec<io::Result<usize>>) -> Self {
            MockReader {
                results: results.into_iter().collect(),
                fill: 0xAB,
            }
        }
    }

    impl Read for MockReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.results.pop_front() {
                Some(Ok(n)) => {
                    let n = n.min(buf.len());
                    for byte in buf[..n].iter_mut() {
                        *byte = self.fill;
                    }
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                // Default once the script is exhausted: the socket has run dry.
                None => Err(io::Error::from(ErrorKind::WouldBlock)),
            }
        }
    }

    /// The core regression test for the 0.2.2 heap corruption.
    ///
    /// The vulnerable loop computed `&mut data[offset..]` once, outside the loop, so four
    /// `MAX_BUFFER_SIZE` reads drove `offset` to 65536 on a 16384-byte array. Recomputing the
    /// destination each iteration means the second read is handed a zero-length slice, the
    /// buffer-full guard fires first, and `offset` tops out at capacity.
    #[test]
    fn repeated_full_size_reads_cannot_push_the_offset_past_capacity() {
        let mut reader = MockReader::new(vec![
            Ok(MAX_BUFFER_SIZE),
            Ok(MAX_BUFFER_SIZE),
            Ok(MAX_BUFFER_SIZE),
            Ok(MAX_BUFFER_SIZE),
            Err(io::Error::from(ErrorKind::WouldBlock)),
        ]);
        let mut buffer = NetworkBuffer::new();

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::BufferFull));
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
        assert!(buffer.offset <= MAX_BUFFER_SIZE);
    }

    #[test]
    fn many_partial_reads_accumulate_without_exceeding_capacity() {
        // 40 reads of 1 KB into a 16 KB buffer: the first 16 fill it, the rest cannot.
        let chunk = 1024;
        let results: Vec<io::Result<usize>> = (0..40).map(|_| Ok(chunk)).collect();
        let mut reader = MockReader::new(results);
        let mut buffer = NetworkBuffer::new();

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::BufferFull));
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
    }

    #[test]
    fn reads_append_rather_than_overwrite() {
        // Two reads landing in a single readable event must not overwrite each other. The
        // frozen slice silently corrupted framing exactly here.
        let mut reader = MockReader::new(vec![Ok(4), Ok(4)]);
        let mut buffer = NetworkBuffer::new();

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::WouldBlock));
        assert_eq!(buffer.offset, 8);
        assert_eq!(buffer.filled(), &[0xAB; 8]);
    }

    #[test]
    fn appends_after_a_previous_partial_packet() {
        let mut buffer = NetworkBuffer::new();
        buffer.data[..3].copy_from_slice(&[1, 2, 3]);
        buffer.offset = 3;

        let mut reader = MockReader::new(vec![Ok(2)]);
        read_into_buffer(&mut reader, &mut buffer);

        assert_eq!(buffer.offset, 5);
        assert_eq!(buffer.filled(), &[1, 2, 3, 0xAB, 0xAB]);
    }

    #[test]
    fn a_full_buffer_is_reported_as_full_not_as_a_disconnect() {
        // `Read::read` returns `Ok(0)` for a zero-length destination, which the old code
        // treated as an orderly peer shutdown. A full buffer must never look like a disconnect.
        let mut reader = MockReader::new(vec![Ok(0)]);
        let mut buffer = NetworkBuffer::new();
        buffer.offset = MAX_BUFFER_SIZE;

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::BufferFull));
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
    }

    #[test]
    fn a_hostile_offset_is_clamped_before_reading() {
        let mut reader = MockReader::new(vec![Ok(16)]);
        let mut buffer = NetworkBuffer::new();
        buffer.offset = usize::MAX;

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::BufferFull));
        assert_eq!(buffer.offset, MAX_BUFFER_SIZE);
    }

    #[test]
    fn a_closed_socket_is_reported_as_closed() {
        let mut reader = MockReader::new(vec![Ok(4), Ok(0)]);
        let mut buffer = NetworkBuffer::new();

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::Closed));
        assert_eq!(buffer.offset, 4);
    }

    #[test]
    fn would_block_on_the_first_read_buffers_nothing() {
        let mut reader = MockReader::new(vec![Err(io::Error::from(ErrorKind::WouldBlock))]);
        let mut buffer = NetworkBuffer::new();

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::WouldBlock));
        assert_eq!(buffer.offset, 0);
    }

    #[test]
    fn interrupted_reads_are_retried() {
        let mut reader = MockReader::new(vec![
            Err(io::Error::from(ErrorKind::Interrupted)),
            Ok(6),
            Err(io::Error::from(ErrorKind::WouldBlock)),
        ]);
        let mut buffer = NetworkBuffer::new();

        let outcome = read_into_buffer(&mut reader, &mut buffer);

        assert!(matches!(outcome, ReadOutcome::WouldBlock));
        assert_eq!(buffer.offset, 6);
    }

    #[test]
    fn a_real_read_error_is_surfaced() {
        let mut reader = MockReader::new(vec![
            Ok(2),
            Err(io::Error::from(ErrorKind::ConnectionReset)),
        ]);
        let mut buffer = NetworkBuffer::new();

        match read_into_buffer(&mut reader, &mut buffer) {
            ReadOutcome::Error(e) => assert_eq!(e.kind(), ErrorKind::ConnectionReset),
            other => panic!("expected an Error outcome, got {:?}", other),
        }

        assert_eq!(buffer.offset, 2);
    }
}
