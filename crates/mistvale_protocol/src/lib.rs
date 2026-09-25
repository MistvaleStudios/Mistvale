//! Bedrock Edition network protocol for Mistvale BDS.
//!
//! Primitive codecs (varints, little-endian numerics, strings), NBT in Bedrock's
//! little-endian and network (varint) flavors, the compressed batch format, and
//! packet definitions for the protocol version below. This crate is
//! transport-agnostic: it consumes and produces the byte messages that
//! `mistvale_net` carries over NetherNet.
//!
//! So far it covers the batch framing ([`batch`]) and the login handshake
//! ([`packets`], [`login`]).

pub mod batch;
pub mod io;
pub mod login;
pub mod packet;
pub mod packets;

/// Network protocol version spoken by this build (Bedrock 26.51).
pub const PROTOCOL_VERSION: i32 = 2193;

/// Game version reported to clients, e.g. in the `GET /v1/join` status response.
pub const GAME_VERSION: &str = "1.26.51";
