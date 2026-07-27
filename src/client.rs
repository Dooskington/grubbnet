use crate::{
    buffer::NetworkBuffer,
    error::Result,
    packet::{
        parse_packet_header, serialize_packet, HeaderParse, Packet, PacketBody,
        MAX_PACKET_BODY_SIZE, PACKET_HEADER_SIZE,
    },
    read_into_buffer, send_bytes, ReadOutcome,
};
use mio::{net::TcpStream, Events, Interest, Poll, Token};
use std::collections::VecDeque;

const LOCAL_TOKEN: Token = Token(0);
const EVENTS_CAPACITY: usize = 4096;

pub enum ClientEvent {
    Disconnected,
    ReceivedPacket(usize),
    SentPacket(usize),

    #[doc(hidden)]
    __Nonexhaustive,
}

pub struct Client {
    tcp_stream: TcpStream,
    events: Events,
    poll: Poll,
    buffer: NetworkBuffer,
    incoming_packets: VecDeque<Packet>,
    outgoing_packets: VecDeque<Box<dyn PacketBody>>,
    outgoing_buffer: Vec<u8>,
    is_disconnected: bool,
}

impl Client {
    pub fn connect(ip: &str, port: u16) -> Result<Client> {
        let address = format!("{}:{}", ip, port).parse().unwrap();
        let mut tcp_stream = TcpStream::connect(address)?;

        // Disable Nagle's algorithm so small, latency-sensitive packets are sent
        // immediately instead of being buffered by the OS.
        tcp_stream.set_nodelay(true)?;

        // Register for reading/writing
        let poll = Poll::new().unwrap();
        poll.registry().register(
            &mut tcp_stream,
            LOCAL_TOKEN,
            Interest::READABLE | Interest::WRITABLE,
        )?;

        Ok(Client {
            tcp_stream,
            events: Events::with_capacity(EVENTS_CAPACITY),
            poll,
            buffer: NetworkBuffer::new(),
            incoming_packets: VecDeque::new(),
            outgoing_packets: VecDeque::new(),
            outgoing_buffer: Vec::new(),
            is_disconnected: false,
        })
    }

    pub fn is_disconnected(&self) -> bool {
        self.is_disconnected
    }

    pub fn drain_incoming_packets(&mut self) -> Vec<Packet> {
        self.incoming_packets.drain(..).collect()
    }

    pub fn send(&mut self, packet: impl PacketBody) {
        let boxed = Box::new(packet);
        self.outgoing_packets.push_back(boxed);
    }

    /// Runs a network tick, which sends/receives packets based on socket readiness
    pub fn tick(&mut self) -> Vec<ClientEvent> {
        if self.is_disconnected {
            return Vec::new();
        }

        let timeout_dur = std::time::Duration::from_millis(1);
        self.poll
            .poll(&mut self.events, Some(timeout_dur))
            .unwrap_or_else(|e| panic!("Failed to poll for events! {}", e));

        let mut net_events: Vec<ClientEvent> = Vec::new();
        for event in self.events.iter() {
            match event.token() {
                // Local socket is ready to read/write
                LOCAL_TOKEN => {
                    // Handle reading
                    if event.is_readable() {
                        // Read until the socket runs dry or the buffer fills up. The
                        // destination slice is recomputed on every iteration inside the helper.
                        match read_into_buffer(&mut self.tcp_stream, &mut self.buffer) {
                            // Socket ran dry, or the buffer filled up. Either way, process
                            // whatever arrived. A full buffer is deliberately not treated as a
                            // disconnect here; that check happens after packets are drained.
                            ReadOutcome::WouldBlock | ReadOutcome::BufferFull => {}
                            ReadOutcome::Closed => {
                                // "Read" 0 bytes, which means we have been disconnected
                                net_events.push(ClientEvent::Disconnected);
                                self.is_disconnected = true;
                            }
                            ReadOutcome::Error(e) => {
                                net_events.push(ClientEvent::Disconnected);

                                eprintln!("Unexpected error when reading bytes! {}", e);
                                self.is_disconnected = true;
                            }
                        }

                        // Process incoming bytes into packets
                        loop {
                            let header = match parse_packet_header(&self.buffer) {
                                HeaderParse::Parsed(header) => header,
                                // The rest of the header hasn't arrived yet. Wait for it.
                                HeaderParse::Incomplete => break,
                                HeaderParse::Invalid => {
                                    // The header declares a body we will never accept, so the
                                    // stream can never resynchronize.
                                    eprintln!(
                                        "Server sent a packet header declaring a body larger than the max body size ({} bytes)! Disconnecting.",
                                        MAX_PACKET_BODY_SIZE
                                    );

                                    net_events.push(ClientEvent::Disconnected);
                                    self.is_disconnected = true;
                                    break;
                                }
                            };

                            // Now make sure we have enough bytes for at the rest of this packet
                            let packet_size = PACKET_HEADER_SIZE + (header.size as usize);
                            if self.buffer.len() < packet_size {
                                break;
                            }

                            // Drain the packet bytes from the front of the buffer
                            let body =
                                self.buffer.filled()[PACKET_HEADER_SIZE..packet_size].to_vec();
                            if self.buffer.try_drain(packet_size).is_err() {
                                // Unreachable: `packet_size <= buffer.len()` was just checked.
                                // Disconnect rather than spin on the same bytes.
                                eprintln!(
                                    "Failed to drain {} bytes from the incoming buffer! Disconnecting.",
                                    packet_size
                                );

                                net_events.push(ClientEvent::Disconnected);
                                self.is_disconnected = true;
                                break;
                            }

                            let packet = Packet { header, body };

                            self.incoming_packets.push_back(packet);

                            net_events.push(ClientEvent::ReceivedPacket(packet_size));
                        }

                        // MAX_BUFFER_SIZE is roughly twice MAX_PACKET_SIZE, so a peer speaking
                        // the protocol can never leave the buffer completely full with no
                        // complete packet in it. If it is still full here there is no room to
                        // read more and nothing to process, so it would spin forever.
                        if !self.is_disconnected && self.buffer.is_full() {
                            eprintln!(
                                "The incoming buffer is full but contains no complete packet! Disconnecting."
                            );

                            net_events.push(ClientEvent::Disconnected);
                            self.is_disconnected = true;
                        }
                    }

                    // Handle writing
                    if event.is_writable() {
                        // Serialize any newly queued packets onto the end of the outgoing
                        // byte buffer, preserving send order.
                        while let Some(packet) = self.outgoing_packets.pop_front() {
                            let data = match serialize_packet(packet) {
                                Ok(d) => d,
                                Err(e) => {
                                    eprintln!("Failed to serialize packet! {}", e);
                                    continue;
                                }
                            };

                            self.outgoing_buffer.extend_from_slice(&data);
                            net_events.push(ClientEvent::SentPacket(data.len()));
                        }

                        // Flush as much of the outgoing buffer as the socket will accept.
                        // Any bytes left unsent (partial write or WouldBlock) are kept and
                        // retried on the next writable event.
                        if !self.outgoing_buffer.is_empty() {
                            match send_bytes(&mut self.tcp_stream, &self.outgoing_buffer) {
                                Ok(sent_bytes) => {
                                    self.outgoing_buffer.drain(..sent_bytes);
                                }
                                Err(e) => {
                                    net_events.push(ClientEvent::Disconnected);

                                    eprintln!("Unexpected error when sending bytes! {}", e);
                                    self.is_disconnected = true;
                                }
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }

        // We're done processing events for this tick.
        // Reregister for next tick, unless we've disconnected: reregistering a socket we're
        // about to drop can fail on a broken fd, and that failure is a panic.
        if !self.is_disconnected {
            self.poll
                .registry()
                .reregister(
                    &mut self.tcp_stream,
                    LOCAL_TOKEN,
                    Interest::READABLE | Interest::WRITABLE,
                )
                .unwrap();
        }

        net_events
    }
}
