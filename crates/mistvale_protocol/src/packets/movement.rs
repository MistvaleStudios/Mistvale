//! Player movement: the client's input each tick, and positions sent to others.

use crate::io::{DecodeError, Reader, Writer};
use crate::packet::{Decode, Encode, Packet, id};
use crate::types::{Vec2, Vec3};

/// Most input flags a PlayerAuthInput may list; the protocol defines about 65.
const MAX_INPUT_FLAGS: u32 = 128;

/// What the client did this tick, sent every client tick (20 per second) once
/// it is in the world, even while standing still.
///
/// Only the leading fields are decoded. After `delta` come item interactions,
/// item stack requests, block actions and vehicle data, which are skipped.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerAuthInput {
    pub pitch: f32,
    pub yaw: f32,
    /// Where the player's eyes are: their feet plus 1.62 blocks.
    pub position: Vec3,
    pub move_vector: Vec2,
    pub head_yaw: f32,
    /// IDs of the input flags set this tick (sneaking, jumping, collisions…).
    /// Kept raw: Mojang's schema and gophertunnel number them differently.
    pub input_flags: Vec<i32>,
    pub input_mode: u32,
    pub play_mode: u32,
    pub interaction_model: i32,
    pub interact_rotation: Vec2,
    /// The client's tick counter.
    pub tick: u64,
    /// How far the player moved this tick.
    pub delta: Vec3,
}

impl Packet for PlayerAuthInput {
    const ID: u32 = id::PLAYER_AUTH_INPUT;
}

impl Decode for PlayerAuthInput {
    fn decode_payload(reader: &mut Reader<'_>) -> Result<Self, DecodeError> {
        let pitch = reader.f32_le()?;
        let yaw = reader.f32_le()?;
        let position = Vec3::read(reader)?;
        let move_vector = Vec2::read(reader)?;
        let head_yaw = reader.f32_le()?;
        let count = reader.var_u32()?;
        if count > MAX_INPUT_FLAGS {
            return Err(DecodeError::InvalidValue {
                field: "input flag count",
                value: count.into(),
            });
        }
        let input_flags = (0..count)
            .map(|_| reader.var_i32())
            .collect::<Result<_, _>>()?;
        let input = Self {
            pitch,
            yaw,
            position,
            move_vector,
            head_yaw,
            input_flags,
            input_mode: reader.var_u32()?,
            play_mode: reader.var_u32()?,
            interaction_model: reader.var_i32()?,
            interact_rotation: Vec2::read(reader)?,
            tick: reader.var_u64()?,
            delta: Vec3::read(reader)?,
        };
        // Skip the optional trailing fields.
        reader.take(reader.remaining())?;
        Ok(input)
    }
}

impl Encode for PlayerAuthInput {
    /// Writes the decoded fields followed by empty optional fields and zero
    /// vectors, as a client that did nothing else this tick would.
    fn encode_payload(&self, writer: &mut Writer) {
        writer.f32_le(self.pitch);
        writer.f32_le(self.yaw);
        self.position.write(writer);
        self.move_vector.write(writer);
        writer.f32_le(self.head_yaw);
        writer.var_u32(u32::try_from(self.input_flags.len()).expect("a few flags"));
        for flag in &self.input_flags {
            writer.var_i32(*flag);
        }
        writer.var_u32(self.input_mode);
        writer.var_u32(self.play_mode);
        writer.var_i32(self.interaction_model);
        self.interact_rotation.write(writer);
        writer.var_u64(self.tick);
        self.delta.write(writer);
        // No item interaction, item stack request, block actions, vehicle
        // rotation or predicted vehicle.
        for _ in 0..5 {
            writer.bool(false);
        }
        // Analogue move vector, camera orientation and raw move vector.
        Vec2::default().write(writer);
        Vec3::default().write(writer);
        Vec2::default().write(writer);
    }
}

/// How a [`MovePlayer`] moves the player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveMode {
    /// Interpolated movement.
    Normal = 0,
    Reset = 1,
    Rotation = 3,
}

/// Moves a player, as seen by another client. Teleports are not supported yet.
#[derive(Debug, Clone, PartialEq)]
pub struct MovePlayer {
    pub entity_runtime_id: u64,
    /// Where the player's eyes are: their feet plus 1.62 blocks.
    pub position: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub mode: MoveMode,
    pub on_ground: bool,
    /// The entity the player rides, or 0.
    pub ridden_entity_runtime_id: u64,
    /// The server tick the movement belongs to.
    pub tick: u64,
}

impl Packet for MovePlayer {
    const ID: u32 = id::MOVE_PLAYER;
}

impl Encode for MovePlayer {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        self.position.write(writer);
        writer.f32_le(self.pitch);
        writer.f32_le(self.yaw);
        writer.f32_le(self.head_yaw);
        writer.u8(self.mode as u8);
        writer.bool(self.on_ground);
        writer.var_u64(self.ridden_entity_runtime_id);
        // No teleport data.
        writer.bool(false);
        writer.var_u64(self.tick);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{decode, read_header};

    fn input() -> PlayerAuthInput {
        PlayerAuthInput {
            pitch: 10.0,
            yaw: -90.0,
            position: Vec3 {
                x: 8.5,
                y: -58.38,
                z: 8.5,
            },
            move_vector: Vec2 { x: 0.0, y: 1.0 },
            head_yaw: -90.0,
            input_flags: vec![9, 49],
            input_mode: 1,
            play_mode: 0,
            interaction_model: 0,
            interact_rotation: Vec2 { x: 10.0, y: -90.0 },
            tick: 1234,
            delta: Vec3 {
                x: 0.0,
                y: 0.0,
                z: 0.2,
            },
        }
    }

    #[test]
    fn decodes_the_leading_fields_and_skips_the_rest() {
        let bytes = input().encode();
        // Header, then pitch and yaw as little-endian floats.
        assert_eq!(bytes[..2], [0x90, 0x01]);
        assert_eq!(bytes[2..6], 10.0f32.to_le_bytes());
        assert_eq!(bytes[6..10], (-90.0f32).to_le_bytes());

        let (header, payload) = read_header(&bytes).unwrap();
        assert_eq!(header.id, id::PLAYER_AUTH_INPUT);
        assert_eq!(decode::<PlayerAuthInput>(payload).unwrap(), input());
    }

    #[test]
    fn rejects_absurd_flag_counts() {
        let mut bytes = vec![0x90, 0x01];
        bytes.extend([0; 4 * 8]);
        bytes.extend([0xFF, 0x0F]);
        let (_, payload) = read_header(&bytes).unwrap();
        assert!(matches!(
            decode::<PlayerAuthInput>(payload),
            Err(DecodeError::InvalidValue {
                field: "input flag count",
                ..
            })
        ));
    }

    #[test]
    fn move_player_layout() {
        let packet = MovePlayer {
            entity_runtime_id: 2,
            position: Vec3 {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            pitch: 0.0,
            yaw: 90.0,
            head_yaw: 45.0,
            mode: MoveMode::Normal,
            on_ground: true,
            ridden_entity_runtime_id: 0,
            tick: 300,
        };
        let mut expected = vec![0x13, 0x02];
        for value in [1.0f32, 2.0, 3.0, 0.0, 90.0, 45.0] {
            expected.extend(value.to_le_bytes());
        }
        expected.extend([0x00, 0x01, 0x00, 0x00, 0xAC, 0x02]);
        assert_eq!(packet.encode(), expected);
    }
}
