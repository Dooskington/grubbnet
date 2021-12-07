use grubbnet::{packet::PacketBody, Client, ClientEvent, Result};

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
        match bincode::config().big_endian().serialize::<Self>(&self) {
            Ok(d) => Ok(d),
            Err(_e) => Err(grubbnet::Error::InvalidData),
        }
    }

    fn deserialize(_data: &[u8]) -> Result<Self> {
        panic!("Attempted to deserialize a client-only packet!");
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
        panic!("Attempted to serialize a server-only packet!");
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

fn main() -> Result<()> {
    // Create a client and connect to localhost
    let mut client = Client::connect("127.0.0.1", 7667)?;

    let mut counter = 0;
    loop {
        // Sleep for a lil bit so we don't hog the CPU
        std::thread::sleep(std::time::Duration::from_millis(100));

        // Send a ping packet every 20 ticks
        counter += 1;
        if (counter % 20) == 0 {
            let ping = PingPacket {
                msg: format!("Ping! Tick {}", counter),
            };
            client.send(ping);
        }

        // Run the network tick and process any events it generates
        for event in client.tick().iter() {
            match event {
                ClientEvent::Disconnected => {
                    println!("Disconnected from server!");
                    break;
                }
                ClientEvent::ReceivedPacket(byte_count) => {
                    println!("Received packet from server ({} bytes)", byte_count);
                }
                ClientEvent::SentPacket(byte_count) => {
                    println!("Sent packet to server ({} bytes)", byte_count);
                }
                _ => eprintln!("Unhandled ClientEvent!"),
            }
        }

        if client.is_disconnected() {
            break;
        }

        // Process incoming packets
        for packet in client.drain_incoming_packets().iter() {
            match packet.header.id {
                0x00 => {
                    let packet = PongPacket::deserialize(&packet.body);
                    println!("Got pong: {}", packet.unwrap().msg);
                }
                _ => eprintln!("Unhandled packet! id: {}", packet.header.id),
            }
        }
    }

    Ok(())
}
