use grubbnet::{packet::PacketBody, Result, Server, ServerEvent, Token, PacketRecipient};
use std::collections::HashMap;

/// 0x00 - Ping Packet
/// Client
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct PingPacket {
    pub msg: String,
}

impl PacketBody for PingPacket {
    fn box_clone(&self) -> Box<dyn PacketBody> {
        Box::new((*self).clone())
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        panic!("Attempted to serialize a client-only packet!");
    }

    fn deserialize(data: &[u8]) -> Result<Self> {
        match bincode::config().big_endian().deserialize::<Self>(data) {
            Ok(p) => Ok(p),
            Err(_e) => Err(grubbnet::Error::InvalidData),
        }
    }

    fn id(&self) -> u8 {
        0x00
    }
}

/// 0x00 - Pong Packet
/// Server
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct PongPacket {
    pub msg: String,
}

impl PacketBody for PongPacket {
    fn box_clone(&self) -> Box<dyn PacketBody> {
        Box::new((*self).clone())
    }

    fn serialize(&self) -> Result<Vec<u8>> {
        match bincode::config().big_endian().serialize::<Self>(&self) {
            Ok(d) => Ok(d),
            Err(_e) => Err(grubbnet::Error::InvalidData),
        }
    }

    fn deserialize(_data: &[u8]) -> Result<Self> {
        panic!("Attempted to deserialize a server-only packet!");
    }

    fn id(&self) -> u8 {
        0x00
    }
}

fn main() -> Result<()> {
    // Begin hosting a TCP server
    let mut server = Server::host("127.0.0.1", 7667, 32)?;
    println!("Hosting on 127.0.0.1:7667...");

    // We are going to keep track of the # of pings we receive from each client, and kick them
    // after they have sent a certain amount.
    let mut ping_counters: HashMap<Token, u32> = HashMap::new();

    loop {
        // Sleep for a lil bit so we don't hog the CPU
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Run the network tick and process any events it generates
        for event in server.tick().iter() {
            match event {
                ServerEvent::ClientConnected(token, addr) => {
                    println!(
                        "Client {} connected from {} ({}/{})",
                        token.0,
                        addr.ip(),
                        server.num_connections(),
                        server.connection_limit(),
                    );
                }
                ServerEvent::ClientDisconnected(token) => {
                    println!("Client {} disconnected.", token.0);
                }
                ServerEvent::ConnectionRejected(addr) => {
                    println!(
                        "Rejected connection from {} (Connection limit reached)",
                        addr.ip(),
                    );
                }
                ServerEvent::ReceivedPacket(token, byte_count) => {
                    println!(
                        "Received packet from client {} ({} bytes)",
                        token.0, byte_count
                    );
                }
                ServerEvent::SentPacket(token, byte_count) => {
                    println!("Sent packet to client {} ({} bytes)", token.0, byte_count);
                }
                _ => eprintln!("Unhandled ServerEvent!"),
            }
        }

        // Process incoming packets
        for (token, packet) in server.drain_incoming_packets().iter() {
            match packet.header.id {
                0x00 => {
                    let packet = PingPacket::deserialize(&packet.body);
                    println!("Got ping from client {}: {}", token.0, packet.unwrap().msg);

                    // Increment the ping counter for this client
                    let counter = ping_counters.entry(*token).or_insert(0);
                    *counter += 1;

                    if *counter >= 5 {
                        // Kick the client when they reach 5 pings.
                        println!("Client {} sent 5 pings. Kicking them.", token.0);
                        server.kick(*token)?;

                        ping_counters.remove_entry(token);
                    } else {
                        // Otherwise just send a ping response (pong).
                        let pong = PongPacket { msg: "Pong!".to_owned() };
                        server.send(PacketRecipient::Single(*token), pong);
                    }
                }
                _ => eprintln!("Unhandled packet! id: {}", packet.header.id)
            }
        }
    }

    Ok(())
}
