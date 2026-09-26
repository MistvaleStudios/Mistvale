//! Who is in the world, where they are, and how to reach them.
//!
//! Players see each other: joining sends the newcomer a player list entry and
//! an entity for everyone already here, and everyone else the same for the
//! newcomer. Movement is recorded as it arrives and sent to the other players
//! once per tick by [`Players::broadcast_movement`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use mistvale_protocol::packet::Encode;
use mistvale_protocol::packets::{
    AddPlayer, EntityMetadata, MetadataValue, MoveMode, MovePlayer, PlayerList, PlayerListEntry,
    RemoveActor, Skin, Text, metadata_key,
};
use mistvale_protocol::types::Vec3;
use tokio::sync::mpsc::{self, error::TrySendError};
use uuid::Uuid;

/// Packets that may wait for one player before more are dropped.
pub const OUTBOUND_QUEUE: usize = 256;

/// Height of a player's eyes above their feet.
pub const EYE_HEIGHT: f32 = 1.62;

/// Encoded packets for a session to batch and send to its client.
pub type Outbound = mpsc::Sender<Bytes>;

/// Who a player is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// The persistent identity, stable across sessions and name changes.
    pub uuid: Uuid,
}

/// Where a player is and where they look. Angles are in degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Movement {
    /// Where the player's eyes are: their feet plus [`EYE_HEIGHT`].
    pub position: Vec3,
    pub pitch: f32,
    pub yaw: f32,
    pub head_yaw: f32,
    pub on_ground: bool,
}

/// A player entering the world.
pub struct Joining {
    /// The runtime and unique ID of the player's entity, from
    /// [`Players::allocate_entity_id`].
    pub entity_id: u64,
    pub profile: Profile,
    pub movement: Movement,
    pub outbound: Outbound,
}

struct Online {
    profile: Profile,
    movement: Movement,
    /// Whether `movement` changed since the last broadcast.
    moved: bool,
    outbound: Outbound,
}

impl Online {
    fn send(&self, packet: Bytes) {
        if let Err(TrySendError::Full(_)) = self.outbound.try_send(packet) {
            tracing::warn!(player = %self.profile.name, "dropped a packet for a player who is not keeping up");
        }
    }
}

/// The players in the world, by entity ID. Sessions join once their player has
/// spawned and leave when their [`Membership`] is dropped.
#[derive(Default)]
pub struct Players {
    last_entity_id: AtomicU64,
    online: Mutex<HashMap<u64, Online>>,
}

impl Players {
    pub fn new() -> Self {
        Self::default()
    }

    /// A new entity ID for a session's player, unique for the server's lifetime.
    pub fn allocate_entity_id(&self) -> u64 {
        self.last_entity_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Adds a player and shows them and everyone already online to each other.
    pub fn join(&self, joining: Joining) -> Membership<'_> {
        let Joining {
            entity_id,
            profile,
            movement,
            outbound,
        } = joining;
        let newcomer = Online {
            profile,
            movement,
            moved: false,
            outbound,
        };
        let mut online = self.online();

        let others: Vec<_> = online.iter().collect();
        if !others.is_empty() {
            let entries = others
                .iter()
                .map(|(id, other)| list_entry(**id, &other.profile))
                .collect();
            newcomer.send(encode(&PlayerList::Add(entries)));
            for (id, other) in &others {
                newcomer.send(encode(&add_player(**id, &other.profile, &other.movement)));
            }
        }
        let list = encode(&PlayerList::Add(vec![list_entry(
            entity_id,
            &newcomer.profile,
        )]));
        let entity = encode(&add_player(entity_id, &newcomer.profile, &movement));
        for (_, other) in &others {
            other.send(list.clone());
            other.send(entity.clone());
        }

        online.insert(entity_id, newcomer);
        Membership {
            players: self,
            entity_id,
        }
    }

    pub fn count(&self) -> usize {
        self.online().len()
    }

    /// Sends every player who moved since the last call to everyone else.
    pub fn broadcast_movement(&self, tick: u64) {
        let mut online = self.online();
        let moved: Vec<(u64, Movement)> = online
            .iter_mut()
            .filter(|(_, player)| player.moved)
            .map(|(id, player)| {
                player.moved = false;
                (*id, player.movement)
            })
            .collect();
        for (mover, movement) in moved {
            let packet = encode(&MovePlayer {
                entity_runtime_id: mover,
                position: movement.position,
                pitch: movement.pitch,
                yaw: movement.yaw,
                head_yaw: movement.head_yaw,
                mode: MoveMode::Normal,
                on_ground: movement.on_ground,
                ridden_entity_runtime_id: 0,
                tick,
            });
            for (_, other) in online.iter().filter(|(id, _)| **id != mover) {
                other.send(packet.clone());
            }
        }
    }

    /// Queues an encoded packet for every player. A player whose queue is full
    /// misses it rather than holding everyone else up.
    pub fn broadcast(&self, packet: &Bytes) {
        for online in self.online().values() {
            online.send(packet.clone());
        }
    }

    /// Shows `message` in every player's chat.
    pub fn broadcast_message(&self, message: &str) {
        if message.is_empty() || message.len() > Text::MAX_MESSAGE_LEN {
            tracing::warn!(
                len = message.len(),
                "not broadcasting a message that is empty or too long"
            );
            return;
        }
        self.broadcast(&encode(&Text::raw(message)));
    }

    /// Relays a player's chat message to everyone, including its author.
    pub fn chat(&self, from: &str, message: &str) {
        self.broadcast_message(&format!("<{from}> {message}"));
    }

    fn online(&self) -> MutexGuard<'_, HashMap<u64, Online>> {
        // The map stays consistent even if a holder panicked.
        self.online.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A player's place in [`Players`]; dropping it removes the player and their
/// entity from everyone's view.
#[must_use = "the player leaves when this is dropped"]
pub struct Membership<'a> {
    players: &'a Players,
    entity_id: u64,
}

impl Membership<'_> {
    /// Records where the player is now; others see it on the next tick.
    pub fn moved(&self, movement: Movement) {
        if let Some(player) = self.players.online().get_mut(&self.entity_id) {
            player.movement = movement;
            player.moved = true;
        }
    }
}

impl Drop for Membership<'_> {
    fn drop(&mut self) {
        let mut online = self.players.online();
        let Some(left) = online.remove(&self.entity_id) else {
            return;
        };
        let entity = encode(&RemoveActor {
            entity_unique_id: unique_id(self.entity_id),
        });
        let list = encode(&PlayerList::Remove(vec![left.profile.uuid]));
        for other in online.values() {
            other.send(entity.clone());
            other.send(list.clone());
        }
    }
}

fn encode(packet: &impl Encode) -> Bytes {
    Bytes::from(packet.encode())
}

fn unique_id(entity_id: u64) -> i64 {
    i64::try_from(entity_id).expect("entity IDs stay far below i64::MAX")
}

fn list_entry(entity_id: u64, profile: &Profile) -> PlayerListEntry {
    // Real skins are not forwarded yet: each player gets a plain colour
    // derived from their UUID.
    let [red, green, blue, ..] = profile.uuid.into_bytes();
    PlayerListEntry {
        uuid: profile.uuid,
        entity_unique_id: unique_id(entity_id),
        username: profile.name.clone(),
        skin: Skin::solid(
            format!("mistvale.{}", profile.uuid),
            [red, green, blue, 255],
        ),
    }
}

fn add_player(entity_id: u64, profile: &Profile, movement: &Movement) -> AddPlayer {
    AddPlayer {
        uuid: profile.uuid,
        username: profile.name.clone(),
        entity_runtime_id: entity_id,
        position: Vec3 {
            y: movement.position.y - EYE_HEIGHT,
            ..movement.position
        },
        pitch: movement.pitch,
        yaw: movement.yaw,
        head_yaw: movement.head_yaw,
        game_mode: 1,
        metadata: EntityMetadata(vec![
            (metadata_key::FLAGS, MetadataValue::Long(0)),
            (
                metadata_key::NAME,
                MetadataValue::String(profile.name.clone()),
            ),
            (metadata_key::SCALE, MetadataValue::Float(1.0)),
            (metadata_key::WIDTH, MetadataValue::Float(0.6)),
            (metadata_key::HEIGHT, MetadataValue::Float(1.8)),
            (metadata_key::ALWAYS_SHOW_NAME_TAG, MetadataValue::Byte(1)),
        ]),
        entity_unique_id: unique_id(entity_id),
    }
}

#[cfg(test)]
mod tests {
    use mistvale_protocol::packet::{self, id};
    use mistvale_protocol::packets::TextType;

    use super::*;

    fn joining(players: &Players, name: &str) -> (Joining, mpsc::Receiver<Bytes>) {
        let (outbound, queue) = mpsc::channel(16);
        let joining = Joining {
            entity_id: players.allocate_entity_id(),
            profile: Profile {
                name: name.into(),
                uuid: Uuid::new_v4(),
            },
            movement: Movement {
                position: Vec3 {
                    x: 8.5,
                    y: -60.0 + EYE_HEIGHT,
                    z: 8.5,
                },
                pitch: 0.0,
                yaw: 0.0,
                head_yaw: 0.0,
                on_ground: true,
            },
            outbound,
        };
        (joining, queue)
    }

    fn ids(queue: &mut mpsc::Receiver<Bytes>) -> Vec<u32> {
        std::iter::from_fn(|| queue.try_recv().ok())
            .map(|packet| packet::read_header(&packet).unwrap().0.id)
            .collect()
    }

    fn text(packet: &[u8]) -> Text {
        let (header, payload) = packet::read_header(packet).unwrap();
        assert_eq!(header.id, id::TEXT);
        packet::decode(payload).unwrap()
    }

    #[test]
    fn players_see_each_other_join_move_and_leave() {
        let players = Players::new();
        let (steve, mut steve_queue) = joining(&players, "Steve");
        let (alex, mut alex_queue) = joining(&players, "Alex");
        assert_ne!(steve.entity_id, alex.entity_id);

        let steve = players.join(steve);
        assert!(ids(&mut steve_queue).is_empty(), "nobody else was online");
        let alex = players.join(alex);
        assert_eq!(ids(&mut alex_queue), [id::PLAYER_LIST, id::ADD_PLAYER]);
        assert_eq!(ids(&mut steve_queue), [id::PLAYER_LIST, id::ADD_PLAYER]);

        // Nothing is sent until a player moves, and never to the mover.
        players.broadcast_movement(1);
        assert!(ids(&mut steve_queue).is_empty());
        steve.moved(Movement {
            position: Vec3 {
                x: 9.0,
                y: -58.38,
                z: 8.5,
            },
            pitch: 5.0,
            yaw: 90.0,
            head_yaw: 80.0,
            on_ground: true,
        });
        players.broadcast_movement(2);
        assert_eq!(ids(&mut alex_queue), [id::MOVE_PLAYER]);
        assert!(ids(&mut steve_queue).is_empty());
        players.broadcast_movement(3);
        assert!(ids(&mut alex_queue).is_empty(), "each move is sent once");

        drop(steve);
        assert_eq!(ids(&mut alex_queue), [id::REMOVE_ACTOR, id::PLAYER_LIST]);
        assert_eq!(players.count(), 1);
        drop(alex);
        assert_eq!(players.count(), 0);
    }

    #[test]
    fn chat_reaches_everyone_online() {
        let players = Players::new();
        let (steve, mut steve_queue) = joining(&players, "Steve");
        let (alex, mut alex_queue) = joining(&players, "Alex");
        let _steve = players.join(steve);
        let _alex = players.join(alex);
        ids(&mut steve_queue);
        ids(&mut alex_queue);

        players.chat("Steve", "hello");
        for queue in [&mut steve_queue, &mut alex_queue] {
            let text = text(&queue.try_recv().unwrap());
            assert_eq!(text.text_type, TextType::Raw);
            assert_eq!(text.message, "<Steve> hello");
        }
    }

    #[test]
    fn full_queues_and_empty_messages_are_skipped() {
        let players = Players::new();
        let (outbound, mut queue) = mpsc::channel(1);
        let (mut steve, _) = joining(&players, "Steve");
        steve.outbound = outbound;
        let _membership = players.join(steve);

        players.broadcast_message("");
        assert!(queue.try_recv().is_err());

        players.broadcast_message("one");
        players.broadcast_message("two");
        assert_eq!(text(&queue.try_recv().unwrap()).message, "one");
        assert!(queue.try_recv().is_err());
    }
}
