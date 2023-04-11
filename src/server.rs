use crate::{
    buffer::NetworkBuffer,
    error::{Error, Result},
    packet::{deserialize_packet_header, serialize_packet, Packet, PacketBody, PACKET_HEADER_SIZE},
    send_bytes, PacketRecipient,
};
use mio::{
    net::{TcpListener, TcpStream},
    Events, Interest, Poll, Token,
};
use std::{
    collections::{HashMap, VecDeque},
    io::Read,
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
}

impl Connection {
    pub fn new(token: Token, socket: TcpStream) -> Self {
        Connection {
            token,
            socket,
            is_disconnected: false,
            buffer: NetworkBuffer::new(),
            outgoing_packets: VecDeque::new(),
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
                LOCAL_TOKEN => match self.tcp_listener.accept() {
                    Ok((mut socket, addr)) => {
                        if self.num_connections() >= self.connection_limit() {
                            println!("Rejecting connection from {}, server is full!", addr.ip());

                            net_events.push(ServerEvent::ConnectionRejected(addr));
                            continue;
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
                    }
                    Err(e) => println!("{}", e),
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
                        // Loop and read bytes into this connections buffer, until there are no more incoming bytes
                        let buffer = &mut conn.buffer.data[conn.buffer.offset..];
                        loop {
                            match conn.socket.read(buffer) {
                                Ok(0) => {
                                    // "Read" 0 bytes, which means the socket has closed
                                    conn.is_disconnected = true;
                                    break;
                                }
                                Ok(read_bytes) => {
                                    // Read some bytes
                                    conn.buffer.offset += read_bytes;
                                }
                                Err(e) => {
                                    // Socket is not ready anymore, stop reading
                                    if e.kind() == std::io::ErrorKind::WouldBlock {
                                        break;
                                    } else {
                                        eprintln!("Unexpected error when reading bytes from connection {}! {}", conn.token.0, e);
                                        conn.is_disconnected = true;
                                        break;
                                    }
                                }
                            }
                        }

                        // Process incoming bytes into packets
                        while let Ok(header) = deserialize_packet_header(&mut conn.buffer) {
                            // Now make sure we have enough bytes for at the rest of this packet
                            let packet_size = PACKET_HEADER_SIZE + (header.size as usize);
                            if conn.buffer.offset < packet_size {
                                break;
                            }

                            // Drain the packet bytes from the front of the buffer
                            let bytes: &[u8] = &conn.buffer.data[PACKET_HEADER_SIZE..packet_size];
                            let body = bytes.to_vec();
                            conn.buffer.drain(packet_size);

                            let packet = Packet { header, body };

                            self.incoming_packets.push_back((token, packet));

                            net_events.push(ServerEvent::ReceivedPacket(conn.token, packet_size));
                        }
                    }

                    // Handle writing
                    if event.is_writable() {
                        while let Some(packet) = conn.outgoing_packets.pop_front() {
                            let data = match serialize_packet(packet) {
                                Ok(d) => d,
                                Err(e) => {
                                    eprintln!("Failed to serialize packet! {}", e);
                                    continue;
                                }
                            };

                            match send_bytes(&mut conn.socket, &data) {
                                Ok(sent_bytes) => {
                                    net_events.push(ServerEvent::SentPacket(token, sent_bytes));
                                }
                                Err(e) => {
                                    eprintln!(
                                        "Unexpected error when sending bytes to connection {}! {}",
                                        conn.token.0, e
                                    );
                                    conn.is_disconnected = true;
                                    break;
                                }
                            }
                        }
                    }

                    // We're done processing events for this connection for this tick.
                    // Reregister for next tick.
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

        // Iterate through disconnected connections and send ClientDisconnected event
        for (tok, _) in self.connections.iter().filter(|&(_, c)| c.is_disconnected) {
            net_events.push(ServerEvent::ClientDisconnected(*tok));
        }

        // Retain any connections which aren't disconnected
        self.connections.retain(|_, v| !v.is_disconnected);

        net_events
    }
}
