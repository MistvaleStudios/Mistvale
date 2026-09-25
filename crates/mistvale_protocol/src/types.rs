//! Small value types shared by packets.

use crate::io::Writer;

/// A block position; each coordinate is a zigzag varint on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

impl BlockPos {
    pub fn write(&self, writer: &mut Writer) {
        writer.var_i32(self.x);
        writer.var_i32(self.y);
        writer.var_i32(self.z);
    }
}

/// A position or direction in floating point, three little-endian f32s on the wire.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub fn write(&self, writer: &mut Writer) {
        writer.f32_le(self.x);
        writer.f32_le(self.y);
        writer.f32_le(self.z);
    }
}
