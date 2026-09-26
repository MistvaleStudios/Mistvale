//! Showing players to each other: the player list, and adding and removing
//! player entities.

use uuid::Uuid;

use crate::io::Writer;
use crate::packet::{Encode, Packet, id};
use crate::types::{Vec3, uuid_bytes};

/// Entity metadata keys, as numbered on the wire.
pub mod metadata_key {
    pub const FLAGS: u32 = 0;
    pub const NAME: u32 = 4;
    pub const SCALE: u32 = 38;
    pub const WIDTH: u32 = 53;
    pub const HEIGHT: u32 = 54;
    /// A byte: 1 shows the name tag without the viewer aiming at the entity.
    pub const ALWAYS_SHOW_NAME_TAG: u32 = 81;
}

/// Bits of the [`metadata_key::FLAGS`] long, as gophertunnel and PocketMine
/// both number them.
pub mod entity_flag {
    pub const SNEAKING: u32 = 1;
    pub const SHOW_NAME: u32 = 14;
    pub const ALWAYS_SHOW_NAME: u32 = 15;
    pub const CAN_CLIMB: u32 = 19;
    pub const BREATHING: u32 = 35;
    pub const HAS_COLLISION: u32 = 48;
    /// Without it, the client does not pull the entity down, the local player included.
    pub const HAS_GRAVITY: u32 = 49;

    /// The flags long with each listed bit set.
    pub fn bits(flags: &[u32]) -> i64 {
        flags.iter().fold(0, |bits, flag| bits | (1 << flag))
    }
}

/// Ability bits of an [`AbilityLayer`], as gophertunnel and PocketMine number them.
pub mod ability {
    pub const BUILD: u32 = 1 << 0;
    pub const MINE: u32 = 1 << 1;
    pub const DOORS_AND_SWITCHES: u32 = 1 << 2;
    pub const OPEN_CONTAINERS: u32 = 1 << 3;
    pub const ATTACK_PLAYERS: u32 = 1 << 4;
    pub const ATTACK_MOBS: u32 = 1 << 5;
    pub const INVULNERABLE: u32 = 1 << 8;
    pub const FLYING: u32 = 1 << 9;
    pub const MAY_FLY: u32 = 1 << 10;
    pub const INSTANT_BUILD: u32 = 1 << 11;
    /// Every one of the 20 abilities.
    pub const ALL: u32 = (1 << 20) - 1;

    /// Vanilla speeds, in blocks per tick.
    pub const WALK_SPEED: f32 = 0.1;
    pub const FLY_SPEED: f32 = 0.05;
    pub const VERTICAL_FLY_SPEED: f32 = 1.0;
}

/// One layer of a player's abilities. `abilities` says which abilities the
/// layer defines, `values` which of those are granted.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AbilityLayer {
    /// 1 is the base layer.
    pub layer: u16,
    pub abilities: u32,
    pub values: u32,
    pub fly_speed: f32,
    pub vertical_fly_speed: f32,
    pub walk_speed: f32,
}

impl AbilityLayer {
    /// A base layer defining every ability, granting `values`, at vanilla speeds.
    pub fn base(values: u32) -> Self {
        Self {
            layer: 1,
            abilities: ability::ALL,
            values,
            fly_speed: ability::FLY_SPEED,
            vertical_fly_speed: ability::VERTICAL_FLY_SPEED,
            walk_speed: ability::WALK_SPEED,
        }
    }
}

/// A player's permissions and ability layers.
#[derive(Debug, Clone, PartialEq)]
pub struct AbilityData {
    pub entity_unique_id: i64,
    /// 0 visitor, 1 member, 2 operator.
    pub player_permissions: u8,
    pub command_permissions: u8,
    pub layers: Vec<AbilityLayer>,
}

impl AbilityData {
    fn write(&self, writer: &mut Writer) {
        writer.i64_le(self.entity_unique_id);
        writer.u8(self.player_permissions);
        writer.u8(self.command_permissions);
        writer.var_u32(len_u32(self.layers.len()));
        for layer in &self.layers {
            writer.u16_le(layer.layer);
            writer.u32_le(layer.abilities);
            writer.u32_le(layer.values);
            writer.f32_le(layer.fly_speed);
            writer.f32_le(layer.vertical_fly_speed);
            writer.f32_le(layer.walk_speed);
        }
    }
}

/// Tells the client what its own player may do and how fast it walks and flies.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateAbilities(pub AbilityData);

impl Packet for UpdateAbilities {
    const ID: u32 = id::UPDATE_ABILITIES;
}

impl Encode for UpdateAbilities {
    fn encode_payload(&self, writer: &mut Writer) {
        self.0.write(writer);
    }
}

/// An entity attribute such as `minecraft:movement`, with its range and default.
#[derive(Debug, Clone, PartialEq)]
pub struct Attribute {
    pub name: String,
    pub min: f32,
    pub max: f32,
    pub value: f32,
    pub default_min: f32,
    pub default_max: f32,
    pub default: f32,
}

impl Attribute {
    /// An attribute from `min` to `max` currently at its default `value`.
    pub fn at_default(name: impl Into<String>, min: f32, max: f32, value: f32) -> Self {
        Self {
            name: name.into(),
            min,
            max,
            value,
            default_min: min,
            default_max: max,
            default: value,
        }
    }
}

/// Sets attributes of an entity; for the player's own entity, `minecraft:movement`
/// is the speed its client walks at.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateAttributes {
    pub entity_runtime_id: u64,
    pub attributes: Vec<Attribute>,
    pub tick: u64,
}

impl Packet for UpdateAttributes {
    const ID: u32 = id::UPDATE_ATTRIBUTES;
}

impl Encode for UpdateAttributes {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        writer.var_u32(len_u32(self.attributes.len()));
        for attribute in &self.attributes {
            writer.f32_le(attribute.min);
            writer.f32_le(attribute.max);
            writer.f32_le(attribute.value);
            writer.f32_le(attribute.default_min);
            writer.f32_le(attribute.default_max);
            writer.f32_le(attribute.default);
            writer.string(&attribute.name);
            // No modifiers.
            writer.var_u32(0);
        }
        writer.var_u64(self.tick);
    }
}

/// Updates an entity's metadata, including the player's own entity.
#[derive(Debug, Clone, PartialEq)]
pub struct SetActorData {
    pub entity_runtime_id: u64,
    pub metadata: EntityMetadata,
    /// The server tick the data belongs to.
    pub tick: u64,
}

impl Packet for SetActorData {
    const ID: u32 = id::SET_ACTOR_DATA;
}

impl Encode for SetActorData {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_u64(self.entity_runtime_id);
        self.metadata.write(writer);
        // No integer or float entity properties.
        writer.var_u32(0);
        writer.var_u32(0);
        writer.var_u64(self.tick);
    }
}

/// A value of entity metadata.
#[derive(Debug, Clone, PartialEq)]
pub enum MetadataValue {
    Byte(u8),
    Float(f32),
    String(String),
    Long(i64),
}

impl MetadataValue {
    fn type_id(&self) -> u8 {
        match self {
            Self::Byte(_) => 0,
            Self::Float(_) => 3,
            Self::String(_) => 4,
            Self::Long(_) => 7,
        }
    }
}

/// An entity's synced data (name, size, flags…), as `(key, value)` pairs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EntityMetadata(pub Vec<(u32, MetadataValue)>);

impl EntityMetadata {
    /// Each entry is the key, the value's type (as a varuint32 variant and
    /// again as a byte), then the value. Entries go in key order.
    pub fn write(&self, writer: &mut Writer) {
        let mut entries: Vec<_> = self.0.iter().collect();
        entries.sort_by_key(|(key, _)| *key);
        writer.var_u32(len_u32(entries.len()));
        for (key, value) in entries {
            writer.var_u32(*key);
            writer.var_u32(value.type_id().into());
            writer.u8(value.type_id());
            match value {
                MetadataValue::Byte(byte) => writer.u8(*byte),
                MetadataValue::Float(float) => writer.f32_le(*float),
                MetadataValue::String(text) => writer.string(text),
                MetadataValue::Long(long) => writer.var_i64(*long),
            }
        }
    }
}

/// Shows another player's entity to the client. Send a [`PlayerList`] entry
/// for the same UUID first so the client has their skin.
#[derive(Debug, Clone, PartialEq)]
pub struct AddPlayer {
    pub uuid: Uuid,
    pub username: String,
    pub entity_runtime_id: u64,
    /// Where the player's feet are.
    pub position: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub game_mode: i32,
    pub metadata: EntityMetadata,
    /// Unique ID for the ability data; Mistvale uses the runtime ID.
    pub entity_unique_id: i64,
}

impl Packet for AddPlayer {
    const ID: u32 = id::ADD_PLAYER;
}

impl Encode for AddPlayer {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.uuid(uuid_bytes(&self.uuid));
        writer.string(&self.username);
        writer.var_u64(self.entity_runtime_id);
        // Platform chat ID.
        writer.string("");
        self.position.write(writer);
        // Velocity.
        Vec3::default().write(writer);
        writer.f32_le(self.pitch);
        writer.f32_le(self.yaw);
        writer.f32_le(self.head_yaw);
        write_empty_item(writer);
        writer.var_i32(self.game_mode);
        self.metadata.write(writer);
        // No integer or float entity properties.
        writer.var_u32(0);
        writer.var_u32(0);
        // Ability data: a base layer defining every ability, as Dragonfly sends.
        AbilityData {
            entity_unique_id: self.entity_unique_id,
            player_permissions: 1,
            command_permissions: 0,
            layers: vec![AbilityLayer::base(0)],
        }
        .write(writer);
        // No entity links, no device ID, unknown build platform.
        writer.var_u32(0);
        writer.string("");
        writer.i32_le(-1);
    }
}

/// An empty item slot: network ID 0 with a zero count, metadata, stack
/// network ID, block runtime ID and user data.
fn write_empty_item(writer: &mut Writer) {
    writer.u16_le(0);
    writer.u16_le(0);
    writer.var_u32(0);
    writer.bool(false);
    writer.var_u32(0);
    writer.var_u32(0);
}

/// Removes an entity from the client's world.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoveActor {
    pub entity_unique_id: i64,
}

impl Packet for RemoveActor {
    const ID: u32 = id::REMOVE_ACTOR;
}

impl Encode for RemoveActor {
    fn encode_payload(&self, writer: &mut Writer) {
        writer.var_i64(self.entity_unique_id);
    }
}

/// Resource patch naming the geometry a classic skin uses.
const RESOURCE_PATCH: &str = r#"{"geometry":{"default":"geometry.humanoid.custom"}}"#;

/// The classic (wide-armed) 64×64 player model, defining the geometry the
/// resource patch names. Clients reject a skin whose geometry data is not JSON;
/// vanilla clients send a full definition like this one for classic skins.
const HUMANOID_GEOMETRY: &str = r#"{"format_version":"1.12.0","minecraft:geometry":[{"description":{"identifier":"geometry.humanoid.custom","texture_width":64,"texture_height":64,"visible_bounds_width":1,"visible_bounds_height":2,"visible_bounds_offset":[0,1,0]},"bones":[
{"name":"root","pivot":[0,0,0]},
{"name":"waist","parent":"root","pivot":[0,12,0]},
{"name":"body","parent":"waist","pivot":[0,24,0],"cubes":[{"origin":[-4,12,-2],"size":[8,12,4],"uv":[16,16]}]},
{"name":"jacket","parent":"body","pivot":[0,24,0],"cubes":[{"origin":[-4,12,-2],"size":[8,12,4],"uv":[16,32],"inflate":0.25}]},
{"name":"head","parent":"body","pivot":[0,24,0],"cubes":[{"origin":[-4,24,-4],"size":[8,8,8],"uv":[0,0]}]},
{"name":"hat","parent":"head","pivot":[0,24,0],"cubes":[{"origin":[-4,24,-4],"size":[8,8,8],"uv":[32,0],"inflate":0.5}]},
{"name":"rightArm","parent":"body","pivot":[-5,22,0],"cubes":[{"origin":[-8,12,-2],"size":[4,12,4],"uv":[40,16]}]},
{"name":"rightSleeve","parent":"rightArm","pivot":[-5,22,0],"cubes":[{"origin":[-8,12,-2],"size":[4,12,4],"uv":[40,32],"inflate":0.25}]},
{"name":"leftArm","parent":"body","pivot":[5,22,0],"cubes":[{"origin":[4,12,-2],"size":[4,12,4],"uv":[32,48]}]},
{"name":"leftSleeve","parent":"leftArm","pivot":[5,22,0],"cubes":[{"origin":[4,12,-2],"size":[4,12,4],"uv":[48,48],"inflate":0.25}]},
{"name":"rightLeg","parent":"root","pivot":[-1.9,12,0],"cubes":[{"origin":[-3.9,0,-2],"size":[4,12,4],"uv":[0,16]}]},
{"name":"rightPants","parent":"rightLeg","pivot":[-1.9,12,0],"cubes":[{"origin":[-3.9,0,-2],"size":[4,12,4],"uv":[0,32],"inflate":0.25}]},
{"name":"leftLeg","parent":"root","pivot":[1.9,12,0],"cubes":[{"origin":[-0.1,0,-2],"size":[4,12,4],"uv":[16,48]}]},
{"name":"leftPants","parent":"leftLeg","pivot":[1.9,12,0],"cubes":[{"origin":[-0.1,0,-2],"size":[4,12,4],"uv":[0,48],"inflate":0.25}]}
]}]}"#;

/// Engine version the geometry is written for; clients send `0.0.0` for classic skins.
const GEOMETRY_ENGINE_VERSION: &str = "0.0.0";

/// A classic skin: a 64×64 RGBA texture on the humanoid model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skin {
    pub id: String,
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes of RGBA.
    pub data: Vec<u8>,
}

impl Skin {
    /// A 64×64 skin of a single colour.
    pub fn solid(id: impl Into<String>, rgba: [u8; 4]) -> Self {
        Self {
            id: id.into(),
            width: 64,
            height: 64,
            data: rgba.repeat(64 * 64),
        }
    }

    fn write(&self, writer: &mut Writer) {
        writer.string(&self.id);
        // PlayFab ID.
        writer.string("");
        writer.string(RESOURCE_PATCH);
        writer.u32_le(self.width);
        writer.u32_le(self.height);
        writer.byte_array(&self.data);
        // No animations and no cape.
        writer.var_u32(0);
        writer.u32_le(0);
        writer.u32_le(0);
        writer.byte_array(&[]);
        writer.string(HUMANOID_GEOMETRY);
        writer.string(GEOMETRY_ENGINE_VERSION);
        // No animation data.
        writer.string("");
        // Cape ID and full ID.
        writer.string("");
        writer.string(&self.id);
        writer.u8(1); // wide arms
        writer.i32_be(0); // skin colour
        // No persona pieces or tints.
        writer.var_u32(0);
        writer.var_u32(0);
        // Not premium, persona, persona cape or primary user; does not
        // override the appearance; trusted.
        for _ in 0..5 {
            writer.bool(false);
        }
        writer.string("true");
        // Profile hash.
        writer.string("");
    }
}

/// A player shown in the client's player list, with the skin their entity uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerListEntry {
    pub uuid: Uuid,
    pub entity_unique_id: i64,
    pub username: String,
    pub skin: Skin,
}

/// Adds players to, or removes them from, the client's player list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlayerList {
    Add(Vec<PlayerListEntry>),
    Remove(Vec<Uuid>),
}

impl Packet for PlayerList {
    const ID: u32 = id::PLAYER_LIST;
}

impl Encode for PlayerList {
    /// Each entry is a variant (1 add, 0 remove), the action byte (0 add, 1
    /// remove) and the UUID; additions carry the rest of the entry.
    fn encode_payload(&self, writer: &mut Writer) {
        match self {
            Self::Add(entries) => {
                writer.var_u32(len_u32(entries.len()));
                for entry in entries {
                    writer.var_u32(1);
                    writer.u8(0);
                    writer.uuid(uuid_bytes(&entry.uuid));
                    writer.var_i64(entry.entity_unique_id);
                    writer.string(&entry.username);
                    // No XUID or platform chat ID; unknown build platform.
                    writer.string("");
                    writer.string("");
                    writer.i32_le(-1);
                    entry.skin.write(writer);
                    // Not a teacher, host or sub-client; no player colour.
                    writer.bool(false);
                    writer.bool(false);
                    writer.bool(false);
                    writer.i32_be(0);
                }
            }
            Self::Remove(uuids) => {
                writer.var_u32(len_u32(uuids.len()));
                for uuid in uuids {
                    writer.var_u32(0);
                    writer.u8(1);
                    writer.uuid(uuid_bytes(uuid));
                }
            }
        }
    }
}

fn len_u32(len: usize) -> u32 {
    u32::try_from(len).expect("lists are shorter than 4 billion entries")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        Uuid::parse_str("00112233-4455-6677-8899-aabbccddeeff").unwrap()
    }

    #[test]
    fn uuids_are_two_little_endian_halves() {
        assert_eq!(
            uuid_bytes(&uuid()),
            [
                0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xFF, 0xEE, 0xDD, 0xCC, 0xBB, 0xAA,
                0x99, 0x88
            ]
        );
    }

    #[test]
    fn metadata_is_sorted_and_typed_twice() {
        let metadata = EntityMetadata(vec![
            (metadata_key::SCALE, MetadataValue::Float(1.0)),
            (metadata_key::NAME, MetadataValue::String("Al".into())),
        ]);
        let mut writer = Writer::new();
        metadata.write(&mut writer);
        let mut expected = vec![0x02, 0x04, 0x04, 0x04, 0x02, b'A', b'l', 38, 0x03, 0x03];
        expected.extend(1.0f32.to_le_bytes());
        assert_eq!(writer.into_bytes(), expected);
    }

    #[test]
    fn player_list_removal_layout() {
        let bytes = PlayerList::Remove(vec![uuid()]).encode();
        assert_eq!(bytes[..4], [0x3F, 0x01, 0x00, 0x01]);
        assert_eq!(bytes[4..], uuid_bytes(&uuid()));
    }

    #[test]
    fn player_list_addition_carries_a_whole_skin() {
        let entry = PlayerListEntry {
            uuid: uuid(),
            entity_unique_id: 2,
            username: "Steve".into(),
            skin: Skin::solid("mistvale.steve", [255, 0, 0, 255]),
        };
        let bytes = PlayerList::Add(vec![entry]).encode();
        assert_eq!(bytes[..4], [0x3F, 0x01, 0x01, 0x00]);
        assert_eq!(bytes[20..23], [0x04, 0x05, b'S']);
        // The skin image dominates: 64 × 64 RGBA pixels.
        assert!(bytes.len() > 64 * 64 * 4);
    }

    #[test]
    fn skin_geometry_is_json_defining_the_patched_geometry() {
        let patch: serde_json::Value = serde_json::from_str(RESOURCE_PATCH).unwrap();
        let named = &patch["geometry"]["default"];
        let geometry: serde_json::Value = serde_json::from_str(HUMANOID_GEOMETRY).unwrap();
        let definition = &geometry["minecraft:geometry"][0];
        assert_eq!(&definition["description"]["identifier"], named);

        // Every parent is a bone defined in the same geometry.
        let bones = definition["bones"].as_array().unwrap();
        let names: Vec<_> = bones.iter().map(|bone| &bone["name"]).collect();
        for bone in bones {
            if let Some(parent) = bone.get("parent") {
                assert!(names.contains(&parent), "unknown parent {parent}");
            }
        }

        // The skin carries the definition and its engine version.
        let mut writer = Writer::new();
        Skin::solid("s", [0; 4]).write(&mut writer);
        let bytes = writer.into_bytes();
        let contains = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
        assert!(contains(HUMANOID_GEOMETRY.as_bytes()));
        assert!(contains(b"\x050.0.0"));
    }

    #[test]
    fn update_attributes_layout() {
        let packet = UpdateAttributes {
            entity_runtime_id: 1,
            attributes: vec![Attribute::at_default(
                "minecraft:movement",
                0.0,
                f32::MAX,
                0.1,
            )],
            tick: 0,
        };
        let mut expected = vec![0x1D, 0x01, 0x01];
        for value in [0.0f32, f32::MAX, 0.1, 0.0, f32::MAX, 0.1] {
            expected.extend(value.to_le_bytes());
        }
        expected.push(18);
        expected.extend(b"minecraft:movement");
        expected.extend([0x00, 0x00]);
        assert_eq!(packet.encode(), expected);
    }

    #[test]
    fn update_abilities_layout() {
        let packet = UpdateAbilities(AbilityData {
            entity_unique_id: 2,
            player_permissions: 1,
            command_permissions: 0,
            layers: vec![AbilityLayer::base(ability::MAY_FLY)],
        });
        let bytes = packet.encode();
        assert_eq!(bytes[..2], [0xBB, 0x01]);
        assert_eq!(bytes[2..10], 2i64.to_le_bytes());
        assert_eq!(bytes[10..13], [0x01, 0x00, 0x01]);
        assert_eq!(bytes[13..15], 1u16.to_le_bytes());
        assert_eq!(bytes[15..19], 0x000F_FFFFu32.to_le_bytes());
        assert_eq!(bytes[19..23], (1u32 << 10).to_le_bytes());
        // Fly, vertical fly and walk speeds.
        assert_eq!(bytes[23..27], 0.05f32.to_le_bytes());
        assert_eq!(bytes[27..31], 1.0f32.to_le_bytes());
        assert_eq!(bytes[31..], 0.1f32.to_le_bytes());
    }

    #[test]
    fn set_actor_data_layout() {
        let packet = SetActorData {
            entity_runtime_id: 1,
            metadata: EntityMetadata(vec![(
                metadata_key::FLAGS,
                MetadataValue::Long(entity_flag::bits(&[entity_flag::HAS_GRAVITY])),
            )]),
            tick: 0,
        };
        let mut expected = vec![0x27, 0x01, 0x01, 0x00, 0x07, 0x07];
        // 1 << 49, zigzagged: 1 << 50 as a varint.
        expected.extend([0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x02]);
        expected.extend([0x00, 0x00, 0x00]);
        assert_eq!(packet.encode(), expected);
    }

    #[test]
    fn remove_actor_is_a_zigzag_unique_id() {
        assert_eq!(
            RemoveActor {
                entity_unique_id: 2
            }
            .encode(),
            [0x0E, 0x04]
        );
    }
}
