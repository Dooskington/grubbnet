//! Integration tests for the server read path.
//!
//! These host a real `Server` on `127.0.0.1:0` and drive it with a plain
//! `std::net::TcpStream`, exercising the socket -> buffer -> packet pipeline end to end.
//!
//! The headline test here (`sixty_four_kilobyte_burst_does_not_corrupt_the_server`) is the
//! regression test for the remote heap corruption fixed in 0.2.3: an unauthenticated peer that
//! connected and wrote 64 KB in one burst drove the connection's buffer offset to 65536 on a
//! 16384-byte array, which `NetworkBuffer::drain` then handed to an unchecked `ptr::copy`.

use grubbnet::{Server, ServerEvent, Token};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const MAX_BUFFER_SIZE: usize = 1024 * 16;
const PACKET_HEADER_SIZE: usize = 3;
const MAX_PACKET_BODY_SIZE: usize = 8192;

/// How long a test will wait for the server to reach the state it expects.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Encodes a packet exactly as it appears on the wire: a big-endian u16 body size, a u8 packet
/// id, then the body.
fn packet_bytes(id: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(PACKET_HEADER_SIZE + body.len());
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.push(id);
    out.extend_from_slice(body);
    out
}

/// A running summary of everything the server has reported. `ServerEvent` is neither `Clone`
/// nor `Debug`, so it gets tallied as it arrives.
#[derive(Default)]
struct Tally {
    connected: Vec<Token>,
    disconnected: Vec<Token>,
    rejected: usize,
    received: usize,
    sent: usize,
    unrecognized: usize,
    packets: Vec<(Token, u8, Vec<u8>)>,
}

impl Tally {
    fn absorb(&mut self, server: &mut Server) {
        for event in server.tick() {
            match event {
                ServerEvent::ClientConnected(token, _addr) => self.connected.push(token),
                ServerEvent::ClientDisconnected(token) => self.disconnected.push(token),
                ServerEvent::ConnectionRejected(_addr) => self.rejected += 1,
                ServerEvent::ReceivedPacket(_token, _bytes) => self.received += 1,
                ServerEvent::SentPacket(_token, _bytes) => self.sent += 1,
                _ => self.unrecognized += 1,
            }
        }

        for (token, packet) in server.drain_incoming_packets() {
            self.packets.push((token, packet.header.id, packet.body));
        }
    }
}

/// Ticks `server` until `stop` is satisfied or [`TIMEOUT`] elapses.
///
/// Returns whether `stop` was satisfied. Tests that expect a steady state (e.g. "the connection
/// is *not* dropped") deliberately ignore the return value and assert on the tally instead.
fn pump(server: &mut Server, tally: &mut Tally, stop: impl Fn(&Tally) -> bool) -> bool {
    let deadline = Instant::now() + TIMEOUT;

    while Instant::now() < deadline {
        tally.absorb(server);

        if stop(tally) {
            return true;
        }
    }

    false
}

/// Ticks the server for a fixed stretch of wall clock time, without an early exit.
fn pump_for(server: &mut Server, tally: &mut Tally, duration: Duration) {
    let deadline = Instant::now() + duration;

    while Instant::now() < deadline {
        tally.absorb(server);
    }
}

fn host() -> (Server, SocketAddr) {
    let server = Server::host("127.0.0.1", 0, 32).expect("failed to host test server");
    let addr = server
        .local_addr()
        .expect("failed to read the server address");

    (server, addr)
}

/// Spawns a peer that writes `payload` in one burst, then holds the connection open until the
/// server closes it (or the read times out), so the server is never the one seeing EOF first.
///
/// The write happens on its own thread because a blocking `write_all` of more than a socket
/// buffer's worth of bytes will not complete until the server starts reading, and the server
/// only reads while the test thread is ticking it.
fn spawn_peer(addr: SocketAddr, payload: Vec<u8>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("peer failed to connect");
        stream
            .set_nodelay(true)
            .expect("peer failed to set nodelay");
        stream
            .set_read_timeout(Some(TIMEOUT))
            .expect("peer failed to set a read timeout");

        // The server may drop us mid-write, which is the point of some of these tests.
        let _ = stream.write_all(&payload);
        let _ = stream.flush();

        // Block until the server closes the connection, so it stays open in the meantime.
        let mut sink = [0u8; 1024];
        let _ = stream.read(&mut sink);
    })
}

/// The regression test for the 0.2.2 remote heap corruption.
///
/// One anonymous `connect()` plus one ~64 KB `write()`. The first three bytes are a valid
/// header declaring a zero-length body (`packet_size == PACKET_HEADER_SIZE == 3`), which is the
/// combination that reached the vulnerable `drain`: the safe, bounds-checked slice on the
/// preceding line is `&data[3..3]`, an in-bounds empty slice, so it never fires.
///
/// In 0.2.2 the destination slice was computed once outside the read loop while `offset` kept
/// accumulating inside it, so four 16384-byte reads left `offset == 65536`. `drain(3)` then ran
/// `ptr::copy(data + 3, data, 65533)` over a 16384-byte array living in a `HashMap`-owned
/// `Connection` - a ~48 KB out-of-bounds read and write across the heap, next to the
/// `VecDeque<Box<dyn PacketBody>>` vtable pointers.
#[test]
fn sixty_four_kilobyte_burst_does_not_corrupt_the_server() {
    let (mut server, addr) = host();

    // 64 KB: a valid empty-body header, then filler that cannot be resynchronized into a
    // packet, so the connection is expected to be dropped rather than parsed forever.
    let mut payload = vec![0xFFu8; 65536];
    payload[0] = 0x00;
    payload[1] = 0x00;
    payload[2] = 0x00;

    let peer = spawn_peer(addr, payload);

    let mut tally = Tally::default();
    assert!(
        pump(&mut server, &mut tally, |t| !t.disconnected.is_empty()),
        "the server never dropped the misbehaving connection"
    );

    // The process survived the burst.
    assert_eq!(tally.connected.len(), 1, "expected exactly one connection");
    assert_eq!(
        tally.disconnected, tally.connected,
        "the connection that was dropped is not the one that connected"
    );
    assert_eq!(tally.unrecognized, 0, "an unrecognized event was reported");

    // The valid empty packet at the front of the burst was delivered intact before the
    // connection was dropped.
    assert_eq!(tally.packets.len(), 1, "expected the one valid packet");
    assert_eq!(tally.packets[0].1, 0x00);
    assert!(tally.packets[0].2.is_empty());

    // And the connection was actually reaped, not just reported.
    assert_eq!(
        server.num_connections(),
        0,
        "the dropped connection was not removed"
    );

    peer.join().expect("peer thread panicked");
}

/// The same shape of attack, but with a payload that is entirely zeroes.
///
/// Every three bytes decode as a legal empty packet, so this is the variant that keeps the
/// connection alive while driving the read loop as hard as possible. It is the harshest test of
/// the read/drain cycle: thousands of `try_drain` calls against a completely full buffer.
#[test]
fn a_full_buffer_of_empty_packets_is_drained_without_corruption() {
    let (mut server, addr) = host();

    let payload = vec![0x00u8; 65536];
    let expected = payload.len() / PACKET_HEADER_SIZE;

    let peer = spawn_peer(addr, payload);

    let mut tally = Tally::default();
    assert!(
        pump(&mut server, &mut tally, |t| t.packets.len() >= expected),
        "only {} of {} empty packets arrived",
        tally.packets.len(),
        expected
    );

    assert_eq!(tally.connected.len(), 1);
    assert!(
        tally
            .packets
            .iter()
            .all(|(_, id, body)| *id == 0 && body.is_empty()),
        "an empty packet decoded to something else"
    );

    // These are all well-formed packets, so the peer is not misbehaving and must not be kicked.
    assert!(
        tally.disconnected.is_empty(),
        "a peer sending well-formed packets was dropped"
    );

    drop(server);
    peer.join().expect("peer thread panicked");
}

/// The framing regression test.
///
/// The frozen destination slice also silently corrupted framing whenever two reads landed in a
/// single readable event: the second read started writing at the same offset as the first and
/// overwrote it. That is the likely cause of the long-standing, unreproducible "client stuck /
/// disconnected under load" reports.
#[test]
fn several_packets_in_one_burst_arrive_intact_and_in_order() {
    let (mut server, addr) = host();

    let bodies: Vec<Vec<u8>> = (0..8u8)
        .map(|i| vec![i.wrapping_mul(17); (i as usize + 1) * 64])
        .collect();

    let mut payload = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        payload.extend_from_slice(&packet_bytes(i as u8, body));
    }

    let peer = spawn_peer(addr, payload);

    let mut tally = Tally::default();
    assert!(
        pump(&mut server, &mut tally, |t| t.packets.len() >= bodies.len()),
        "only {} of {} packets arrived",
        tally.packets.len(),
        bodies.len()
    );

    assert_eq!(tally.packets.len(), bodies.len());
    for (i, (_token, id, body)) in tally.packets.iter().enumerate() {
        assert_eq!(*id, i as u8, "packet {} arrived out of order", i);
        assert_eq!(body, &bodies[i], "packet {} body was corrupted", i);
    }

    assert_eq!(tally.received, bodies.len());

    drop(server);
    peer.join().expect("peer thread panicked");
}

/// The same framing guarantee, but for a burst several times larger than the buffer.
///
/// The server takes at most one bufferful per readable event, so this necessarily spans several
/// ticks. That also proves the buffer-full path does not stall the connection.
#[test]
fn a_burst_larger_than_the_buffer_preserves_framing_across_ticks() {
    let (mut server, addr) = host();

    // ~48 KB, i.e. three times MAX_BUFFER_SIZE, in packets that do not divide evenly into it.
    let count = 48;
    let bodies: Vec<Vec<u8>> = (0..count)
        .map(|i| vec![(i % 251) as u8; 700 + (i * 7) % 300])
        .collect();

    let mut payload = Vec::new();
    for (i, body) in bodies.iter().enumerate() {
        payload.extend_from_slice(&packet_bytes((i % 256) as u8, body));
    }
    assert!(payload.len() > 2 * MAX_BUFFER_SIZE);

    let peer = spawn_peer(addr, payload);

    let mut tally = Tally::default();
    assert!(
        pump(&mut server, &mut tally, |t| t.packets.len() >= count),
        "only {} of {} packets arrived",
        tally.packets.len(),
        count
    );

    assert_eq!(tally.packets.len(), count);
    for (i, (_token, id, body)) in tally.packets.iter().enumerate() {
        assert_eq!(*id, (i % 256) as u8, "packet {} arrived out of order", i);
        assert_eq!(body, &bodies[i], "packet {} body was corrupted", i);
    }

    assert!(
        tally.disconnected.is_empty(),
        "a peer sending well-formed packets was dropped"
    );

    drop(server);
    peer.join().expect("peer thread panicked");
}

/// A packet split across several writes must still be reassembled correctly, and a partial
/// header must never be completed with stale bytes from earlier traffic.
#[test]
fn a_packet_split_across_writes_is_reassembled() {
    let (mut server, addr) = host();

    let body: Vec<u8> = (0..500u32).map(|i| (i % 256) as u8).collect();
    let expected = body.clone();
    let wire = packet_bytes(0x5A, &body);

    let peer = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("peer failed to connect");
        stream
            .set_nodelay(true)
            .expect("peer failed to set nodelay");
        stream
            .set_read_timeout(Some(TIMEOUT))
            .expect("peer failed to set a read timeout");

        // Two bytes of the three byte header, so the third byte has genuinely not arrived.
        stream.write_all(&wire[..2]).unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // The rest of the header plus part of the body.
        stream.write_all(&wire[2..100]).unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(50));

        // The remainder.
        stream.write_all(&wire[100..]).unwrap();
        stream.flush().unwrap();

        let mut sink = [0u8; 1024];
        let _ = stream.read(&mut sink);
    });

    let mut tally = Tally::default();
    assert!(
        pump(&mut server, &mut tally, |t| !t.packets.is_empty()),
        "the split packet never arrived"
    );

    assert_eq!(tally.packets.len(), 1);
    assert_eq!(tally.packets[0].1, 0x5A);
    assert_eq!(tally.packets[0].2, expected);
    assert!(
        tally.disconnected.is_empty(),
        "a peer sending a well-formed packet was dropped"
    );

    drop(server);
    peer.join().expect("peer thread panicked");
}

/// A header declaring a body larger than `MAX_PACKET_BODY_SIZE` can never be satisfied, so the
/// stream can never resynchronize. Prior to 0.2.3 the bytes were left in the buffer and
/// re-parsed on every tick, logging a line each time: an unauthenticated log-flood vector.
#[test]
fn an_oversized_declared_body_drops_the_connection() {
    let (mut server, addr) = host();

    let mut payload = Vec::new();
    payload.extend_from_slice(&(MAX_PACKET_BODY_SIZE as u16).to_be_bytes());
    payload.push(0x01);
    payload.extend_from_slice(&[0xAB; 64]);

    let peer = spawn_peer(addr, payload);

    let mut tally = Tally::default();
    assert!(
        pump(&mut server, &mut tally, |t| !t.disconnected.is_empty()),
        "the connection declaring an oversized body was not dropped"
    );

    assert_eq!(tally.connected.len(), 1);
    assert_eq!(tally.disconnected, tally.connected);
    assert!(tally.packets.is_empty(), "an oversized packet was accepted");
    assert_eq!(server.num_connections(), 0);

    peer.join().expect("peer thread panicked");
}

/// A peer that opens a packet header and then goes quiet must not be dropped, and must not
/// cause the server to spin: it is simply an incomplete packet.
#[test]
fn a_peer_that_stops_mid_packet_is_left_alone() {
    let (mut server, addr) = host();

    // A legal header for a 4 KB body, followed by only 16 bytes of it.
    let mut payload = Vec::new();
    payload.extend_from_slice(&4096u16.to_be_bytes());
    payload.push(0x02);
    payload.extend_from_slice(&[0x11; 16]);

    let peer = spawn_peer(addr, payload);

    let mut tally = Tally::default();
    pump(&mut server, &mut tally, |t| !t.connected.is_empty());
    pump_for(&mut server, &mut tally, Duration::from_millis(500));

    assert_eq!(tally.connected.len(), 1);
    assert!(
        tally.disconnected.is_empty(),
        "a peer with a merely incomplete packet was dropped"
    );
    assert!(
        tally.packets.is_empty(),
        "an incomplete packet was accepted"
    );
    assert_eq!(server.num_connections(), 1);

    drop(server);
    peer.join().expect("peer thread panicked");
}

/// A peer that closes the connection cleanly must still be reported as a disconnect. The
/// buffer-full handling added in 0.2.3 must not swallow a genuine `Ok(0)`.
#[test]
fn a_clean_peer_shutdown_is_still_reported() {
    let (mut server, addr) = host();

    let wire = packet_bytes(0x09, &[1, 2, 3, 4]);
    let peer = std::thread::spawn(move || {
        let mut stream = TcpStream::connect(addr).expect("peer failed to connect");
        stream.write_all(&wire).unwrap();
        stream.flush().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        drop(stream);
    });

    let mut tally = Tally::default();
    assert!(
        pump(&mut server, &mut tally, |t| !t.disconnected.is_empty()),
        "a clean peer shutdown was never reported"
    );

    assert_eq!(tally.connected.len(), 1);
    assert_eq!(tally.disconnected, tally.connected);
    assert_eq!(
        tally.packets.len(),
        1,
        "the packet sent before closing was lost"
    );
    assert_eq!(tally.packets[0].2, vec![1, 2, 3, 4]);
    assert_eq!(server.num_connections(), 0);

    peer.join().expect("peer thread panicked");
}
