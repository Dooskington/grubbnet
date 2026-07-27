use crate::{
    buffer::{NetworkBuffer, MAX_BUFFER_SIZE},
    error::{Error, Result},
    packet::{
        parse_packet_header, serialize_packet, HeaderParse, Packet, PacketBody,
        MAX_PACKET_BODY_SIZE, PACKET_HEADER_SIZE,
    },
    read_into_buffer, send_bytes, PacketRecipient, ReadOutcome,
};
use mio::{
    net::{TcpListener, TcpStream},
    Events, Interest, Poll, Token,
};
use std::{
    collections::{HashMap, VecDeque},
    net::SocketAddr,
};

const LOCAL_TOKEN: Token = Token(0);
const EVENTS_CAPACITY: usize = 4096;

pub enum ServerEvent {
    ConnectionRejected(SocketAddr),
    ClientConnected(Token, SocketAddr),
    ClientDisconnected(Token),
    ReceivedPacket(Token, usize),
    SentPacket(Token, usize),

    #[doc(hidden)]
    __Nonexhaustive,
}

pub struct Connection {
    token: Token,
    socket: TcpStream,
    is_disconnected: bool,
    buffer: NetworkBuffer,
    outgoing_packets: VecDeque<Box<dyn PacketBody>>,
    outgoing_buffer: Vec<u8>,
}

impl Connection {
    pub fn new(token: Token, socket: TcpStream) -> Self {
        Connection {
            token,
            socket,
            is_disconnected: false,
            buffer: NetworkBuffer::new(),
            outgoing_packets: VecDeque::new(),
            outgoing_buffer: Vec::new(),
        }
    }
}

pub struct Server {
    tcp_listener: TcpListener,
    events: Events,
    poll: Poll,
    connections: HashMap<Token, Connection>,
    connection_limit: usize,
    token_counter: usize,
    incoming_packets: VecDeque<(Token, Packet)>,
}

impl Server {
    /// Begin hosting a TCP server.
    pub fn host(ip: &str, port: u16, connection_limit: usize) -> Result<Server> {
        let address = format!("{}:{}", ip, port).parse().unwrap();
        let mut tcp_listener = TcpListener::bind(address)?;

        // Register to read events
        let poll = Poll::new().unwrap();
        poll.registry()
            .register(&mut tcp_listener, LOCAL_TOKEN, Interest::READABLE)?;

        Ok(Server {
            tcp_listener,
            events: Events::with_capacity(EVENTS_CAPACITY),
            poll,
            connections: HashMap::new(),
            connection_limit,
            token_counter: 0,
            incoming_packets: VecDeque::new(),
        })
    }

    /// Get the current number of connections.
    pub fn num_connections(&self) -> usize {
        self.connections.len()
    }

    /// Get the maximum number of connections allowed.
    pub fn connection_limit(&self) -> usize {
        self.connection_limit
    }

    /// The address this server is listening on.
    ///
    /// Useful when hosting on port 0 and letting the OS pick a free port.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.tcp_listener.local_addr()?)
    }

    /// Drain any incoming packets and return them.
    pub fn drain_incoming_packets(&mut self) -> Vec<(Token, Packet)> {
        self.incoming_packets.drain(..).collect()
    }

    /// Kick a connection from the server.
    pub fn kick(&mut self, connection_token: Token) -> Result<()> {
        let conn: &mut Connection = match self.connections.get_mut(&connection_token) {
            Some(c) => c,
            None => {
                return Err(Error::ConnectionNotFound);
            }
        };

        conn.is_disconnected = true;

        Ok(())
    }

    /// Send a packet.
    /// This function will box the packet, then queue it to be sent on the next server tick.
    pub fn send(&mut self, recipient: PacketRecipient, packet: impl PacketBody) {
        let boxed: Box<dyn PacketBody> = Box::new(packet);
        self.send_boxed(recipient, boxed);
    }

    /// Send a boxed packet.
    /// Similar to `send`, but this is moreuseful when you have a boxed packet already and don't want
    /// to cast it to a concrete type before sending it.
    pub fn send_boxed(&mut self, recipient: PacketRecipient, packet_boxed: Box<dyn PacketBody>) {
        match recipient {
            PacketRecipient::All => {
                for (_, connection) in self.connections.iter_mut() {
                    connection.outgoing_packets.push_back(packet_boxed.clone());
                }
            }
            PacketRecipient::Single(t) => {
                if let Some(connection) = self.connections.get_mut(&t) {
                    connection.outgoing_packets.push_back(packet_boxed);
                }
            }
            PacketRecipient::Exclude(t) => {
                let filtered = self.connections.iter_mut().filter(|(tok, _c)| tok.0 != t.0);
                for (_token, connection) in filtered {
                    connection.outgoing_packets.push_back(packet_boxed.clone());
                }
            }
            PacketRecipient::ExcludeMany(filter) => {
                let filtered = self
                    .connections
                    .iter_mut()
                    .filter(|(tok, _c)| !filter.contains(tok));
                for (_token, connection) in filtered {
                    connection.outgoing_packets.push_back(packet_boxed.clone());
                }
            }
            PacketRecipient::Include(targets) => {
                let filtered = self
                    .connections
                    .iter_mut()
                    .filter(|(tok, _c)| targets.contains(tok));
                for (_token, connection) in filtered {
                    connection.outgoing_packets.push_back(packet_boxed.clone());
                }
            }
        }
    }

    /// Runs a network tick, which sends/receives packets based on socket readiness, as well as accepts new connections.
    pub fn tick(&mut self) -> Vec<ServerEvent> {
        let timeout_dur = std::time::Duration::from_millis(1);
        self.poll
            .poll(&mut self.events, Some(timeout_dur))
            .unwrap_or_else(|e| panic!("Failed to poll for new events! {}", e));

        let mut net_events: Vec<ServerEvent> = Vec::new();
        for event in self.events.iter() {
            match event.token() {
                // Local socket is ready to accept
                LOCAL_TOKEN => loop {
                    let (mut socket, addr) = match self.tcp_listener.accept() {
                        Ok((socket, addr)) => (socket, addr),
                        Err(e) => {
                            if e.kind() == std::io::ErrorKind::WouldBlock {
                                break;
                            }

                            println!("{}", e);
                            break;
                        }
                    };

                    if self.num_connections() >= self.connection_limit() {
                        println!("Rejecting connection from {}, server is full!", addr.ip());

                        net_events.push(ServerEvent::ConnectionRejected(addr));
                        continue;
                    }

                    // Disable Nagle's algorithm so small, latency-sensitive packets are
                    // sent immediately instead of being buffered by the OS.
                    if let Err(e) = socket.set_nodelay(true) {
                        eprintln!(
                            "Failed to set TCP_NODELAY on connection from {}! {}",
                            addr, e
                        );
                    }

                    // Increment our token counter, then create a new token for this connection
                    self.token_counter += 1;
                    let token = Token(self.token_counter);

                    // Register the new socket to receive events
                    self.poll.registry().register(
                        &mut socket,
                        token,
                        Interest::READABLE | Interest::WRITABLE,
                    ).unwrap_or_else(|e| panic!("Failed to register poll for new connection (Token {}, Address {}). {}", token.0, addr, e));

                    // Insert the new connection
                    self.connections
                        .insert(token, Connection::new(token, socket));

                    net_events.push(ServerEvent::ClientConnected(token, addr));
                },
                // Connection socket is ready to read/write
                token => {
                    // Get the connection
                    let conn: &mut Connection =
                        self.connections.get_mut(&token).unwrap_or_else(|| {
                            panic!(
                                "Attempted to handle socket event for non-existent connection {}!",
                                token.0
                            )
                        });

                    // Handle reading
                    if event.is_readable() {
                        // Read bytes into this connection's buffer until the socket runs dry
                        // or the buffer fills up. The destination slice is recomputed on every
                        // iteration inside the helper: hoisting it out of the loop is what let
                        // an unauthenticated 64 KB burst drive `offset` to 65536 on a
                        // 16384-byte array in 0.2.2.
                        //
                        // At most one bufferful is taken per readable event, so a peer sending
                        // a firehose of bytes cannot starve the other connections. The
                        // reregister at the end of this tick re-arms readiness for the rest.
                        let read_outcome = read_into_buffer(&mut conn.socket, &mut conn.buffer);

                        // Hard invariant: the read path must never leave the buffer offset past
                        // the end of the backing array. 0.2.2 violated this and then fed the
                        // resulting out-of-range length straight into an unsafe pointer copy.
                        debug_assert!(
                            conn.buffer.offset <= MAX_BUFFER_SIZE,
                            "Connection {} buffer offset ({}) escaped the backing array ({} bytes)",
                            conn.token.0,
                            conn.buffer.offset,
                            MAX_BUFFER_SIZE
                        );

                        if conn.buffer.offset > MAX_BUFFER_SIZE {
                            eprintln!(
                                "Connection {} buffer offset ({}) exceeded the maximum buffer size ({} bytes)! Dropping connection.",
                                conn.token.0, conn.buffer.offset, MAX_BUFFER_SIZE
                            );

                            conn.buffer.offset = MAX_BUFFER_SIZE;
                            conn.is_disconnected = true;
                        }

                        match read_outcome {
                            // Socket ran dry, or the buffer filled up. Either way, process
                            // whatever arrived. A full buffer is deliberately not treated as a
                            // disconnect here; that check happens after packets are drained.
                            ReadOutcome::WouldBlock | ReadOutcome::BufferFull => {}
                            ReadOutcome::Closed => {
                                // "Read" 0 bytes, which means the socket has closed
                                conn.is_disconnected = true;
                            }
                            ReadOutcome::Error(e) => {
                                eprintln!(
                                    "Unexpected error when reading bytes from connection {}! {}",
                                    conn.token.0, e
                                );

                                conn.is_disconnected = true;
                            }
                        }

                        // Process incoming bytes into packets
                        loop {
                            let header = match parse_packet_header(&conn.buffer) {
                                HeaderParse::Parsed(header) => header,
                                // The rest of the header hasn't arrived yet. Wait for it.
                                HeaderParse::Incomplete => break,
                                HeaderParse::Invalid => {
                                    // The header declares a body we will never accept, so the
                                    // stream can never resynchronize. Kick the client so we
                                    // have some basic protection from being overloaded.
                                    eprintln!(
                                        "Connection {} sent a packet header declaring a body larger than the max body size ({} bytes)! Dropping connection.",
                                        conn.token.0, MAX_PACKET_BODY_SIZE
                                    );

                                    conn.is_disconnected = true;
                                    break;
                                }
                            };

                            // Now make sure we have enough bytes for at the rest of this packet
                            let packet_size = PACKET_HEADER_SIZE + (header.size as usize);
                            if conn.buffer.len() < packet_size {
                                break;
                            }

                            // Drain the packet bytes from the front of the buffer
                            let body =
                                conn.buffer.filled()[PACKET_HEADER_SIZE..packet_size].to_vec();
                            if conn.buffer.try_drain(packet_size).is_err() {
                                // Unreachable: `packet_size <= buffer.len()` was just checked.
                                // Drop the connection rather than spin on the same bytes.
                                eprintln!(
                                    "Failed to drain {} bytes from connection {}'s buffer! Dropping connection.",
                                    packet_size, conn.token.0
                                );

                                conn.is_disconnected = true;
                                break;
                            }

                            let packet = Packet { header, body };

                            self.incoming_packets.push_back((token, packet));

                            net_events.push(ServerEvent::ReceivedPacket(conn.token, packet_size));
                        }

                        // MAX_BUFFER_SIZE is roughly twice MAX_PACKET_SIZE, so a peer speaking
                        // the protocol can never leave the buffer completely full with no
                        // complete packet in it. If it is still full here, the peer is either
                        // desynced or deliberately wedging the connection: there is no room to
                        // read more and nothing to process, so it would spin forever.
                        if !conn.is_disconnected && conn.buffer.is_full() {
                            eprintln!(
                                "Connection {}'s buffer is full but contains no complete packet! Dropping connection.",
                                conn.token.0
                            );

                            conn.is_disconnected = true;
                        }
                    }

                    // Handle writing
                    if event.is_writable() {
                        // Serialize any newly queued packets onto the end of the outgoing
                        // byte buffer, preserving send order.
                        while let Some(packet) = conn.outgoing_packets.pop_front() {
                            let data = match serialize_packet(packet) {
                                Ok(d) => d,
                                Err(e) => {
                                    eprintln!("Failed to serialize packet! {}", e);
                                    continue;
                                }
                            };

                            conn.outgoing_buffer.extend_from_slice(&data);
                            net_events.push(ServerEvent::SentPacket(token, data.len()));
                        }

                        // Flush as much of the outgoing buffer as the socket will accept.
                        // Any bytes left unsent (partial write or WouldBlock) are kept and
                        // retried on the next writable event.
                        if !conn.outgoing_buffer.is_empty() {
                            match send_bytes(&mut conn.socket, &conn.outgoing_buffer) {
                                Ok(sent_bytes) => {
                                    conn.outgoing_buffer.drain(..sent_bytes);
                                }
                                Err(e) => {
                                    eprintln!(
                                        "Unexpected error when sending bytes to connection {}! {}",
                                        conn.token.0, e
                                    );
                                    conn.is_disconnected = true;
                                }
                            }
                        }
                    }

                    // We're done processing events for this connection for this tick.
                    // Reregister for next tick, unless it's on its way out: reregistering a
                    // socket we're about to drop can fail on a broken fd, and that failure is
                    // a panic. A misbehaving peer must not be able to take the server down.
                    //
                    // Skipping it here is safe because `is_disconnected` is not touched again
                    // before the `retain` at the end of this tick removes exactly these
                    // connections, so a connection that survives the tick is always re-armed.
                    //
                    // Note that in the `BufferFull` case above we stop reading before the
                    // socket returns `WouldBlock`, and mio only guarantees another readiness
                    // event after a full drain. This reregister is what re-arms readiness for
                    // the leftover bytes: it maps to `EPOLL_CTL_MOD` on epoll and to a
                    // re-issued AFD poll on Windows IOCP, both of which report a socket that
                    // already has data pending.
                    if !conn.is_disconnected {
                        self.poll
                            .registry()
                            .reregister(
                                &mut conn.socket,
                                conn.token,
                                Interest::READABLE | Interest::WRITABLE,
                            )
                            .unwrap_or_else(|e| {
                                panic!(
                                    "Failed to reregister poll for connection (Token {}). {}",
                                    token.0, e
                                )
                            });
                    }
                }
            }
        }

        // Iterate through disconnected connections and send ClientDisconnected event
        for (tok, _) in self.connections.iter().filter(|&(_, c)| c.is_disconnected) {
            net_events.push(ServerEvent::ClientDisconnected(*tok));
        }

        // Retain any connections which aren't disconnected
        self.connections.retain(|_, v| !v.is_disconnected);

        net_events
    }
}
