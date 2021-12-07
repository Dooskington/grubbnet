use crate::{
    buffer::NetworkBuffer,
    error::Result,
    packet::{deserialize_packet_header, serialize_packet, Packet, PacketBody, PACKET_HEADER_SIZE},
    send_bytes,
};
use mio::{net::TcpStream, Events, Poll, PollOpt, Ready, Token};
use std::{collections::VecDeque, io::Read};

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
    is_disconnected: bool,
}

impl Client {
    pub fn connect(ip: &str, port: u16) -> Result<Client> {
        let address = format!("{}:{}", ip, port).parse().unwrap();
        let tcp_stream = TcpStream::connect(&address)?;

        // Register for reading/writing
        let poll = Poll::new().unwrap();
        poll.register(
            &tcp_stream,
            LOCAL_TOKEN,
            Ready::readable() | Ready::writable(),
            PollOpt::edge(),
        )?;

        Ok(Client {
            tcp_stream,
            events: Events::with_capacity(EVENTS_CAPACITY),
            poll,
            buffer: NetworkBuffer::new(),
            incoming_packets: VecDeque::new(),
            outgoing_packets: VecDeque::new(),
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
                    if event.readiness().is_readable() {
                        loop {
                            // Read until there are no more incoming bytes
                            match self
                                .tcp_stream
                                .read(&mut self.buffer.data[self.buffer.offset..])
                            {
                                Ok(0) => {
                                    // "Read" 0 bytes, which means we have been disconnected
                                    net_events.push(ClientEvent::Disconnected);
                                    self.is_disconnected = true;
                                    break;
                                }
                                Ok(read_bytes) => {
                                    // Read some bytes
                                    self.buffer.offset += read_bytes;
                                }
                                Err(e) => {
                                    // Socket is not ready anymore, stop reading
                                    if e.kind() == std::io::ErrorKind::WouldBlock {
                                        break;
                                    } else {
                                        net_events.push(ClientEvent::Disconnected);

                                        eprintln!("Unexpected error when reading bytes! {}", e);
                                        self.is_disconnected = true;
                                        break;
                                    }
                                }
                            }
                        }

                        // Process incoming bytes into packets
                        while let Ok(header) = deserialize_packet_header(&mut self.buffer) {
                            // Now make sure we have enough bytes for at the rest of this packet
                            let packet_size = PACKET_HEADER_SIZE + (header.size as usize);
                            if self.buffer.offset < packet_size {
                                break;
                            }

                            // Drain the packet bytes from the front of the buffer
                            let bytes: &[u8] = &self.buffer.data[PACKET_HEADER_SIZE..packet_size];
                            let body = bytes.to_vec();
                            self.buffer.drain(packet_size);

                            let packet = Packet { header, body };

                            self.incoming_packets.push_back(packet);

                            net_events.push(ClientEvent::ReceivedPacket(packet_size));
                        }
                    }

                    // Handle writing
                    if event.readiness().is_writable() {
                        while let Some(packet) = self.outgoing_packets.pop_front() {
                            let data = match serialize_packet(packet) {
                                Ok(d) => d,
                                Err(e) => {
                                    eprintln!("Failed to serialize packet! {}", e);
                                    continue;
                                }
                            };

                            match send_bytes(&mut self.tcp_stream, &data) {
                                Ok(sent_bytes) => {
                                    net_events.push(ClientEvent::SentPacket(sent_bytes));
                                }
                                Err(e) => {
                                    net_events.push(ClientEvent::Disconnected);

                                    eprintln!("Unexpected error when sending bytes! {}", e);
                                    self.is_disconnected = true;
                                    break;
                                }
                            }
                        }
                    }
                }
                _ => unreachable!(),
            }
        }

        // We're done processing events for this tick.
        // Reregister for next tick.
        self.poll
            .reregister(
                &self.tcp_stream,
                LOCAL_TOKEN,
                Ready::readable() | Ready::writable(),
                PollOpt::edge(),
            )
            .unwrap();

        net_events
    }
}
