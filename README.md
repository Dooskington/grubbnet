<a href="https://github.com/Dooskington/grubbnet/">
    <img src="https://i.imgur.com/O2XnKQE.png" alt="Grubbnet icon" title="Antorum" align="left" height="64" width="64" />
</a>

Grubbnet
========
[![crates.io](https://img.shields.io/crates/v/grubbnet.svg)](https://crates.io/crates/grubbnet)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Documentation](https://docs.rs/grubbnet/badge.svg)](https://docs.rs/grubbnet)

Grubbnet is a lightweight TCP client/server library, meant for writing networked applications and games. 
It's a combination of all the TCP boilerplate I usually find myself writing when I work on a networked project. 
Initially, it was an internal crate for a [multiplayer RPG.](https://dooskington.com/dev-log/0)

Grubbnet abstracts socket code, keeps track of connections, and delivers everything back to the developer in a
nice list of events. In addition to handling network events (such as client connects and disconnects), handling incoming packets is is as easy as grabbing an iterator over the incoming packet queue.

## Headers and Packets
Instead of dealing with raw bytes, Grubbnet operates based on packets that the developer can define. You can turn a struct into a packet by implementing the `PacketBody` trait, and then it is in the developers hands to define the serialization and deserialization that they want. At runtime, a packet header is created, the serialized packet body is tacked onto that, and the complete packet is sent across the wire.

Packet headers are 3 bytes (2 bytes for a 16 bit body size, and 1 byte for an 8 bit packet id). In the future, I'd like to allow developers to also define their own header for more flexibility. The header allows Grubbnet to recognize when it's being sent a packet, what the packet type is, and how many bytes it needs to wait for before it has all the data required to reconstruct the packet. After this happens, the packet id and (still serialized) body are handed back to the developer through the incoming packet queue, and they can do as they please with it.

## Usage
 Add this to your `Cargo.toml`:
 ```toml
 [dependencies]
 grubbnet = "0.1"
 ```

Hosting a barebones server that sends a simple packet:
```rust
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct MessagePacket { pub msg: String }

impl PacketBody for MessagePacket {
    fn box_clone(&self) -> Box<dyn PacketBody> {
        // This is used internally. Hopefully it can be removed some day.
        Box::new((*self).clone())
    }

    fn serialize(&self) -> Vec<u8> {
        // Define your own serialization here.
        // I like to use serde & bincode.
        bincode::config()
            .big_endian()
            .serialize::<Self>(&self)
            .unwrap()
    }

    fn deserialize(_data: &[u8]) -> Self {
        panic!("Attempted to deserialize a server-only packet!");
    }

    fn id(&self) -> u8 {
        0x00
    }
}

fn main() -> Result<()> {
    let mut server = Server::host("127.0.0.1", 7667, 32)?;
    loop {
        // Run the network tick and process any events it generates
        for event in server.tick().iter() {
            match event {
                ServerEvent::ClientConnected(token, addr) => {
                    // Send a message packet when a client connects
                    let pckt = MessagePacket { msg: "Hello, world!".to_owned() };
                    server.send(PacketRecipient::Single(*token), pckt);
                }
                ServerEvent::ClientDisconnected(token) => {}
                ServerEvent::ConnectionRejected(addr) => {}
                ServerEvent::ReceivedPacket(token, byte_count) => {}
                ServerEvent::SentPacket(token, byte_count) => {}
                _ => eprintln!("Unhandled ServerEvent!"),
            }
        }

        // Process incoming packets
        for (token, packet) in server.drain_incoming_packets().iter() {
            match packet.header.id {
                0x00 => { 
                    // Deserialize and handle, however you like
                }
                _ => eprintln!("Unhandled packet! id: {}", packet.header.id)
            }
        }
    }
}
```

## Example
I have written one example (split into two parts): a simple ping server and client.
 ```
cargo run --example simple_server
cargo run --example simple_client
 ```

The server will run, waiting for clients to connect and send `PingPacket`s, responding with `PongPacket`s.
The client will connect and send `PingPacket`s on an interval. When 5 pings are sent, the server will kick the client.

Both the `simple_server` and `simple_client` define the `PingPacket` and `PongPacket`. When using the crate for
real, if possible you should store packet definitions in a common crate. Also, it goes without saying that
these two packets didn't need to be separate types. However, I wanted to demonstrate the pattern of having
client-only and server-only packets.

## Optional Crate Feature - Crypto
There is one optional feature in this crate, `crypto`.
Enabling it will give you access to the `grubbnet::crypto` module, which is a tiny wrapper around some `openssl` and `bcrypt`
stuff for decrypting bytes with a private key, and hashing/verifying strings. This is useful for writing packets with sensitive data.
I need to write more about how this is used, but for now just know that if you enable this you are required to have the `openssl` development
libraries installed on your machine.

```rust
// Decrypt some bytes
let (decrypted_bytes, decrypted_len) = grubbnet::crypto::decrypt(rsa, &encrypted_bytes)
    .expect("Failed to decrypt password!");

// Convert those bytes into a UTF-8 plaintext password
let plaintext_password = str::from_utf8(&decrypted_bytes[0..decrypted_len])
    .expect("Password was invalid UTF-8!");

// Verify plaintext password against some hashed password
if grubbnet::crypto::verify(plaintext_password, hashed_password)? {
    println!("Authorized!");
}
```

# License

Grubbnet is distributed under the terms of the MIT license.
See [LICENSE](LICENSE.md) for more details.
