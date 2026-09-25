//! Packet headers and the traits packets implement.

use crate::io::{DecodeError, Reader, Writer};

/// Packet IDs for protocol 2193, as listed in Mojang's protocol schemas.
pub mod id {
    pub const LOGIN: u32 = 1;
    pub const PLAY_STATUS: u32 = 2;
    pub const DISCONNECT: u32 = 5;
    pub const RESOURCE_PACKS_INFO: u32 = 6;
    pub const RESOURCE_PACK_STACK: u32 = 7;
    pub const RESOURCE_PACK_CLIENT_RESPONSE: u32 = 8;
    pub const START_GAME: u32 = 11;
    pub const CLIENT_CACHE_STATUS: u32 = 129;
    pub const NETWORK_SETTINGS: u32 = 143;
    pub const REQUEST_NETWORK_SETTINGS: u32 = 193;
}

/// A packet's varuint32 header: the packet ID in the low 10 bits, then 2 bits
/// each for the sending and receiving split-screen sub-client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub id: u32,
    pub sender_subclient: u8,
    pub target_subclient: u8,
}

impl Header {
    const ID_BITS: u32 = 0x3FF;
    const SUBCLIENT_BITS: u32 = 0x3;

    pub fn new(id: u32) -> Self {
        Self {
            id,
            sender_subclient: 0,
            target_subclient: 0,
        }
    }

    pub fn read(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let header = reader.var_u32()?;
        Ok(Self {
            id: header & Self::ID_BITS,
            sender_subclient: ((header >> 10) & Self::SUBCLIENT_BITS) as u8,
            target_subclient: ((header >> 12) & Self::SUBCLIENT_BITS) as u8,
        })
    }

    pub fn write(&self, writer: &mut Writer) {
        writer.var_u32(
            (self.id & Self::ID_BITS)
                | ((u32::from(self.sender_subclient) & Self::SUBCLIENT_BITS) << 10)
                | ((u32::from(self.target_subclient) & Self::SUBCLIENT_BITS) << 12),
        );
    }
}

/// A packet type and its ID.
pub trait Packet {
    const ID: u32;
}

/// A packet the server can send.
pub trait Encode: Packet {
    fn encode_payload(&self, writer: &mut Writer);

    /// The header and payload, ready to be framed into a batch.
    fn encode(&self) -> Vec<u8> {
        let mut writer = Writer::new();
        Header::new(Self::ID).write(&mut writer);
        self.encode_payload(&mut writer);
        writer.into_bytes()
    }
}

/// A packet the server can receive.
pub trait Decode: Packet + Sized {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError>;
}

/// Splits an encoded packet into its header and a reader over the payload.
pub fn read_header(packet: &[u8]) -> Result<(Header, Reader<'_>), DecodeError> {
    let mut reader = Reader::new(packet);
    let header = Header::read(&mut reader)?;
    Ok((header, reader))
}

/// Decodes a whole payload, rejecting any bytes left over.
pub fn decode<P: Decode>(mut payload: Reader<'_>) -> Result<P, DecodeError> {
    let packet = P::decode_payload(&mut payload)?;
    payload.finish()?;
    Ok(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_packs_subclients_above_the_id() {
        let header = Header {
            id: id::REQUEST_NETWORK_SETTINGS,
            sender_subclient: 1,
            target_subclient: 2,
        };
        let mut writer = Writer::new();
        header.write(&mut writer);
        let bytes = writer.into_bytes();
        assert_eq!(Header::read(&mut Reader::new(&bytes)), Ok(header));

        let (plain, payload) = read_header(&[0xC1, 0x01, 0xAB]).unwrap();
        assert_eq!(plain, Header::new(193));
        assert_eq!(payload.remaining(), 1);
    }
}
