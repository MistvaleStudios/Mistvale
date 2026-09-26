//! Particles, sounds and animations other players see and hear.

use crate::io::Writer;
use crate::packet::{Encode, Packet, id};
use crate::types::Vec3;

/// World events: particles and sounds tied to a position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LevelEvent {
    /// One of the `LevelEvent::` constants.
    pub event: i32,
    pub position: Vec3,
    pub data: i32,
}

impl LevelEvent {
    /// A block's breaking particles and sound; `data` is the broken block's
    /// network ID.
    pub const DESTROY_BLOCK: i32 = 2001;
}

impl Packet for LevelEvent {
    const ID: u32 = id::LEVEL_EVENT;
}

impl Encode for LevelEvent {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i32(self.event);
        self.position.write(writer);
        writer.var_i32(self.data);
    }
}

/// A sound at a position, named by its sound event (e.g. `place`).
#[derive(Debug, Clone, PartialEq)]
pub struct LevelSoundEvent {
    pub sound: String,
    pub position: Vec3,
    /// Sound-specific data; for block sounds, the block's network ID.
    pub data: i32,
}

impl LevelSoundEvent {
    /// The sound of placing a block.
    pub const PLACE: &str = "place";
}

impl Packet for LevelSoundEvent {
    const ID: u32 = id::LEVEL_SOUND_EVENT;
}

impl Encode for LevelSoundEvent {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.string(&self.sound);
        self.position.write(writer);
        writer.var_i32(self.data);
        // No entity type (":" as Dragonfly sends), not a baby, relative volume,
        // no entity, and not fired at another position.
        writer.string(":");
        writer.bool(false);
        writer.bool(false);
        writer.i64_le(-1);
        writer.bool(false);
    }
}

/// An entity animation; Mistvale sends arm swings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Animate {
    /// One of the `Animate::` constants.
    pub action: u8,
    pub entity_runtime_id: u64,
}

impl Animate {
    pub const SWING_ARM: u8 = 1;
}

impl Packet for Animate {
    const ID: u32 = id::ANIMATE;
}

impl Encode for Animate {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.u8(self.action);
        writer.var_u64(self.entity_runtime_id);
        // Data, then no swing source.
        writer.f32_le(0.0);
        writer.bool(false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effect_layouts() {
        let destroy = LevelEvent {
            event: LevelEvent::DESTROY_BLOCK,
            position: Vec3::default(),
            data: 5,
        };
        let mut expected = vec![0x19, 0xA2, 0x1F];
        expected.extend([0; 12]);
        expected.push(10);
        assert_eq!(destroy.encode(), expected);

        let swing = Animate {
            action: Animate::SWING_ARM,
            entity_runtime_id: 2,
        };
        assert_eq!(swing.encode(), [0x2C, 0x01, 0x02, 0, 0, 0, 0, 0x00]);

        let place = LevelSoundEvent {
            sound: LevelSoundEvent::PLACE.into(),
            position: Vec3::default(),
            data: 7,
        }
        .encode();
        assert_eq!(place[..3], [0x7B, 0x05, b'p']);
        assert_eq!(place[place.len() - 13..place.len() - 9], [0x01, b':', 0, 0]);
    }
}
