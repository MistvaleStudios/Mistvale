//! Who is in the world, and how to reach them.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use mistvale_protocol::packet::Encode as _;
use mistvale_protocol::packets::Text;
use tokio::sync::mpsc::{self, error::TrySendError};
use uuid::Uuid;

/// Packets that may wait for one player before more are dropped.
pub const OUTBOUND_QUEUE: usize = 256;

/// Encoded packets for a session to batch and send to its client.
pub type Outbound = mpsc::Sender<Bytes>;

/// Who a player is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// The persistent identity, stable across sessions and name changes.
    pub uuid: Uuid,
}

struct Online {
    profile: Profile,
    outbound: Outbound,
}

/// The players in the world. Sessions join once their player has spawned and
/// leave when their [`Membership`] is dropped.
#[derive(Default)]
pub struct Players {
    next_id: AtomicU64,
    online: Mutex<HashMap<u64, Online>>,
}

impl Players {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a player who receives what is broadcast through `outbound`.
    pub fn join(&self, profile: Profile, outbound: Outbound) -> Membership<'_> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.online().insert(id, Online { profile, outbound });
        Membership { players: self, id }
    }

    pub fn count(&self) -> usize {
        self.online().len()
    }

    /// Queues an encoded packet for every player. A player whose queue is full
    /// misses it rather than holding everyone else up.
    pub fn broadcast(&self, packet: &Bytes) {
        for online in self.online().values() {
            if let Err(TrySendError::Full(_)) = online.outbound.try_send(packet.clone()) {
                tracing::warn!(player = %online.profile.name, "dropped a packet for a player who is not keeping up");
            }
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
        self.broadcast(&Bytes::from(Text::raw(message).encode()));
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

/// A player's place in [`Players`]; dropping it removes the player.
#[must_use = "the player leaves when this is dropped"]
pub struct Membership<'a> {
    players: &'a Players,
    id: u64,
}

impl Drop for Membership<'_> {
    fn drop(&mut self) {
        self.players.online().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use mistvale_protocol::packet::{self, id};
    use mistvale_protocol::packets::TextType;

    use super::*;

    fn profile(name: &str) -> Profile {
        Profile {
            name: name.into(),
            uuid: Uuid::new_v4(),
        }
    }

    fn text(packet: &[u8]) -> Text {
        let (header, payload) = packet::read_header(packet).unwrap();
        assert_eq!(header.id, id::TEXT);
        packet::decode(payload).unwrap()
    }

    #[test]
    fn chat_reaches_everyone_online_until_they_leave() {
        let players = Players::new();
        let (steve_outbound, mut steve) = mpsc::channel(4);
        let (alex_outbound, mut alex) = mpsc::channel(4);
        let steve_membership = players.join(profile("Steve"), steve_outbound);
        let alex_membership = players.join(profile("Alex"), alex_outbound);
        assert_eq!(players.count(), 2);

        players.chat("Steve", "hello");
        for queue in [&mut steve, &mut alex] {
            let text = text(&queue.try_recv().unwrap());
            assert_eq!(text.text_type, TextType::Raw);
            assert_eq!(text.message, "<Steve> hello");
        }

        drop(alex_membership);
        assert_eq!(players.count(), 1);
        players.broadcast_message("bye");
        assert_eq!(text(&steve.try_recv().unwrap()).message, "bye");
        assert!(alex.try_recv().is_err());
        drop(steve_membership);
        assert_eq!(players.count(), 0);
    }

    #[test]
    fn full_queues_and_empty_messages_are_skipped() {
        let players = Players::new();
        let (outbound, mut queue) = mpsc::channel(1);
        let _membership = players.join(profile("Steve"), outbound);

        players.broadcast_message("");
        assert!(queue.try_recv().is_err());

        players.broadcast_message("one");
        players.broadcast_message("two");
        assert_eq!(text(&queue.try_recv().unwrap()).message, "one");
        assert!(queue.try_recv().is_err());
    }
}
