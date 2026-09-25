//! A client's protocol session: login, then spawning into the world.
//!
//! [`Session`] is a sans-IO state machine over decoded packets; [`run`] drives
//! it over a NetherNet [`Connection`]. The flow follows gophertunnel's server
//! for NetherNet, where there is no ServerToClientHandshake because DTLS already
//! encrypts the connection:
//!
//! 1. RequestNetworkSettings → NetworkSettings, then compression starts.
//! 2. Login → PlayStatus(LoginSuccess) + ResourcePacksInfo.
//! 3. ResourcePackClientResponse(downloading finished) → ResourcePackStack.
//! 4. ResourcePackClientResponse(stack finished) → JigsawStructureData,
//!    VoxelShapes, StartGame and ItemRegistry.
//! 5. RequestChunkRadius → ChunkRadiusUpdated, NetworkChunkPublisherUpdate, the
//!    chunks in view, PlayStatus(PlayerSpawn) and CreativeContent.
//! 6. SetLocalPlayerAsInitialized: the player is in the world.

use std::hash::{BuildHasher, RandomState};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mistvale_net::{ClientIdentity, Connection, Reliability};
use mistvale_protocol::batch::{self, BatchError, Compression, CompressionAlgorithm};
use mistvale_protocol::io::DecodeError;
use mistvale_protocol::login::{ConnectionRequest, LoginError};
use mistvale_protocol::nbt::Compound;
use mistvale_protocol::packet::{self, Encode as _, id};
use mistvale_protocol::packets::{
    ChunkRadiusUpdated, CreativeContent, Disconnect, DisconnectMessage, DisconnectReason,
    EXEMPTED_PACKS, GameRule, GameRuleValue, ItemRegistry, JigsawStructureData, Login,
    NetworkChunkPublisherUpdate, NetworkSettings, PackResponse, PlayStatus, PlayStatusCode,
    PlayerMovementSettings, RequestChunkRadius, RequestNetworkSettings, ResourcePackClientResponse,
    ResourcePackStack, ResourcePacksInfo, SetLocalPlayerAsInitialized, StackPack, StartGame,
    VoxelShapes,
};
use mistvale_protocol::types::Vec3;
use mistvale_protocol::{GAME_VERSION, PROTOCOL_VERSION};

use crate::world::{FlatWorld, OVERWORLD};

/// Compression the server asks clients to use.
const COMPRESSION: Compression = Compression {
    algorithm: CompressionAlgorithm::Flate,
    threshold: 256,
};

/// How long to wait for the client to hang up after we disconnect it, so the
/// Disconnect packet is delivered before the session is torn down.
const DISCONNECT_LINGER: Duration = Duration::from_secs(5);

/// Largest view distance granted, in chunks.
const MAX_VIEW_DISTANCE: i32 = 8;

/// The player's own entity IDs, as Dragonfly uses.
const PLAYER_ENTITY_ID: u64 = 1;

/// Height of a player's eyes above their feet.
const EYE_HEIGHT: f32 = 1.62;

/// Serves one client until either side closes the connection.
pub async fn run(mut connection: Connection, world: Arc<FlatWorld>) {
    let network_id = connection.network_id();
    let mut session = Session::new(connection.client_identity().cloned(), world);
    let mut compression = None;

    while let Some(message) = connection.recv().await {
        let replies = match batch::decode(&message.payload, compression.is_some()) {
            Ok(packets) => packets
                .iter()
                .map(|packet| {
                    session
                        .handle(packet)
                        .unwrap_or_else(|err| err.into_reply())
                })
                .collect(),
            Err(err) => vec![SessionError::from(err).into_reply()],
        };
        for reply in replies {
            if !reply.packets.is_empty() {
                let packets = reply.packets.iter().map(Vec::as_slice);
                let sent = match batch::encode(packets, compression) {
                    Ok(batch) => connection
                        .send(Bytes::from(batch), Reliability::Reliable)
                        .await
                        .is_ok(),
                    Err(err) => {
                        tracing::warn!(network_id, %err, "failed to encode a batch");
                        false
                    }
                };
                if !sent {
                    return;
                }
            }
            if let Some(agreed) = reply.enable_compression {
                compression = Some(agreed);
            }
            if reply.close {
                linger(&mut connection).await;
                return;
            }
        }
    }
    tracing::debug!(network_id, "client closed the connection");
}

/// Gives the client time to read our last packets and hang up by itself.
async fn linger(connection: &mut Connection) {
    let _ = tokio::time::timeout(DISCONNECT_LINGER, async {
        while connection.recv().await.is_some() {}
    })
    .await;
}

/// What to do after handling a packet.
#[derive(Debug, Default)]
pub struct Reply {
    /// Encoded packets to send together, in order.
    pub packets: Vec<Vec<u8>>,
    /// Compression both sides use once `packets` has been sent.
    pub enable_compression: Option<Compression>,
    /// Close the connection once `packets` has been sent.
    pub close: bool,
}

impl Reply {
    fn send(packets: Vec<Vec<u8>>) -> Self {
        Self {
            packets,
            ..Self::default()
        }
    }

    /// Disconnects the client, showing `message` on its disconnect screen.
    fn disconnect(reason: DisconnectReason, message: impl Into<String>) -> Self {
        let disconnect = Disconnect {
            reason,
            message: Some(DisconnectMessage {
                message: message.into(),
                filtered_message: String::new(),
            }),
        };
        Self {
            packets: vec![disconnect.encode()],
            close: true,
            ..Self::default()
        }
    }
}

/// Where the session is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    RequestNetworkSettings,
    Login,
    ResourcePacks,
    /// StartGame is sent; waiting for the client's view distance.
    Spawning,
    /// Chunks and PlayerSpawn are sent; waiting for the client to finish loading.
    Initializing,
    InGame,
}

impl Stage {
    /// Whether the player has a world to be in. Packets without a handler are
    /// ignored from here on, since clients send many kinds while playing.
    fn in_world(self) -> bool {
        matches!(self, Self::Spawning | Self::Initializing | Self::InGame)
    }
}

/// Why the session failed; each error disconnects the client.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("malformed batch: {0}")]
    Batch(#[from] BatchError),
    #[error("malformed packet: {0}")]
    Decode(#[from] DecodeError),
    #[error("malformed login: {0}")]
    Login(#[from] LoginError),
    #[error("unexpected packet {id} while waiting for {stage:?}")]
    UnexpectedPacket { id: u32, stage: Stage },
}

impl SessionError {
    fn into_reply(self) -> Reply {
        tracing::debug!(err = %self, "session failed");
        let reason = match self {
            Self::UnexpectedPacket { .. } => DisconnectReason::UNEXPECTED_PACKET,
            _ => DisconnectReason::BAD_PACKET,
        };
        Reply::disconnect(reason, format!("Mistvale BDS: {self}"))
    }
}

/// One client's session, one decoded packet at a time.
#[derive(Debug)]
pub struct Session {
    stage: Stage,
    /// The identity the client proved during NetherNet signaling, if any.
    identity: Option<ClientIdentity>,
    /// The name the player logged in with, for logs.
    player: String,
    world: Arc<FlatWorld>,
}

impl Session {
    pub fn new(identity: Option<ClientIdentity>, world: Arc<FlatWorld>) -> Self {
        Self {
            stage: Stage::RequestNetworkSettings,
            identity,
            player: String::from("<unknown>"),
            world,
        }
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Handles one encoded packet (header and payload).
    pub fn handle(&mut self, packet: &[u8]) -> Result<Reply, SessionError> {
        let (header, payload) = packet::read_header(packet)?;
        match (self.stage, header.id) {
            // The blob cache is not supported; the client copes without it.
            (_, id::CLIENT_CACHE_STATUS) => Ok(Reply::default()),
            (Stage::RequestNetworkSettings, id::REQUEST_NETWORK_SETTINGS) => {
                self.request_network_settings(packet::decode(payload)?)
            }
            (Stage::Login, id::LOGIN) => self.login(packet::decode(payload)?),
            (Stage::ResourcePacks, id::RESOURCE_PACK_CLIENT_RESPONSE) => {
                self.pack_response(packet::decode(payload)?)
            }
            (stage, id::REQUEST_CHUNK_RADIUS) if stage.in_world() => {
                Ok(self.chunk_radius(packet::decode(payload)?))
            }
            (Stage::Initializing | Stage::InGame, id::SET_LOCAL_PLAYER_AS_INITIALIZED) => {
                Ok(self.initialized(packet::decode(payload)?))
            }
            (stage, id) if stage.in_world() => {
                if id != id::PLAYER_AUTH_INPUT {
                    tracing::trace!(id, "ignoring packet without a handler");
                }
                Ok(Reply::default())
            }
            (stage, id) => Err(SessionError::UnexpectedPacket { id, stage }),
        }
    }

    fn request_network_settings(
        &mut self,
        request: RequestNetworkSettings,
    ) -> Result<Reply, SessionError> {
        if request.client_protocol != PROTOCOL_VERSION {
            tracing::info!(
                client_protocol = request.client_protocol,
                server_protocol = PROTOCOL_VERSION,
                "rejecting a client on another protocol version"
            );
            let status = if request.client_protocol < PROTOCOL_VERSION {
                PlayStatusCode::LoginFailedClient
            } else {
                PlayStatusCode::LoginFailedServer
            };
            return Ok(Reply {
                packets: vec![PlayStatus { status }.encode()],
                close: true,
                ..Reply::default()
            });
        }

        self.stage = Stage::Login;
        let settings = NetworkSettings {
            compression_threshold: COMPRESSION.threshold,
            compression_algorithm: COMPRESSION.algorithm.id(),
            client_throttle: false,
            client_throttle_threshold: 0,
            client_throttle_scalar: 0.0,
        };
        Ok(Reply {
            packets: vec![settings.encode()],
            enable_compression: Some(COMPRESSION),
            close: false,
        })
    }

    fn login(&mut self, login: Login) -> Result<Reply, SessionError> {
        let request = ConnectionRequest::parse(&login.connection_request)?;
        let claims = request.identity()?;

        // NetherNet has no game-level encryption, so a captured Login could be
        // replayed on another connection. The Login must carry the key this
        // connection proved during signaling.
        if let Some(identity) = &self.identity {
            let matches = claims
                .public_key
                .as_ref()
                .and_then(|key| mistvale_net::identity::parse_public_key(key).ok())
                .is_some_and(|key| key == identity.public_key);
            if !matches {
                tracing::warn!(
                    xuid = ?claims.xuid,
                    "Login key does not match the key proven during signaling"
                );
                return Ok(Reply::disconnect(
                    DisconnectReason::NOT_AUTHENTICATED,
                    "Mistvale BDS: your login does not match this connection.",
                ));
            }
        }

        tracing::info!(
            name = ?claims.display_name,
            xuid = ?claims.xuid,
            "player logged in (identity not verified yet)"
        );
        if let Some(name) = claims.display_name {
            self.player = name;
        }
        self.stage = Stage::ResourcePacks;
        Ok(Reply::send(vec![
            PlayStatus {
                status: PlayStatusCode::LoginSuccess,
            }
            .encode(),
            ResourcePacksInfo::default().encode(),
        ]))
    }

    fn pack_response(
        &mut self,
        response: ResourcePackClientResponse,
    ) -> Result<Reply, SessionError> {
        match response.response {
            PackResponse::DownloadingFinished => {
                let stack = ResourcePackStack {
                    texture_pack_required: false,
                    packs: EXEMPTED_PACKS
                        .iter()
                        .map(|(uuid, version)| StackPack {
                            id: (*uuid).to_owned(),
                            version: (*version).to_owned(),
                            sub_pack_name: String::new(),
                        })
                        .collect(),
                    base_game_version: GAME_VERSION.to_owned(),
                    experiments: Vec::new(),
                    experiments_previously_toggled: false,
                    include_editor_packs: false,
                };
                Ok(Reply::send(vec![stack.encode()]))
            }
            PackResponse::StackFinished => {
                self.stage = Stage::Spawning;
                Ok(Reply::send(vec![
                    JigsawStructureData::empty().encode(),
                    VoxelShapes.encode(),
                    start_game(&self.world).encode(),
                    // No items yet; the client spawns with an empty inventory.
                    ItemRegistry::default().encode(),
                ]))
            }
            PackResponse::Downloading(_) => Ok(Reply::disconnect(
                DisconnectReason::RESOURCE_PACK_PROBLEM,
                "Mistvale BDS: this server has no resource packs to download.",
            )),
            PackResponse::Cancel => Ok(Reply {
                close: true,
                ..Reply::default()
            }),
        }
    }

    /// Grants a view distance and sends every chunk in it, nearest first. The
    /// first time, this also lets the client spawn.
    fn chunk_radius(&mut self, request: RequestChunkRadius) -> Reply {
        let radius = request.radius.clamp(1, MAX_VIEW_DISTANCE);
        let spawn = self.world.spawn();
        let (centre_x, centre_z) = (spawn.x >> 4, spawn.z >> 4);

        let mut offsets: Vec<(i32, i32)> = (-radius..=radius)
            .flat_map(|dx| (-radius..=radius).map(move |dz| (dx, dz)))
            .filter(|(dx, dz)| dx * dx + dz * dz <= radius * radius)
            .collect();
        offsets.sort_by_key(|(dx, dz)| dx * dx + dz * dz);

        let mut packets = vec![
            ChunkRadiusUpdated { radius }.encode(),
            NetworkChunkPublisherUpdate {
                position: spawn,
                radius: radius.unsigned_abs() << 4,
            }
            .encode(),
        ];
        packets.extend(
            offsets
                .iter()
                .map(|(dx, dz)| self.world.chunk(centre_x + dx, centre_z + dz).encode()),
        );
        if self.stage == Stage::Spawning {
            packets.push(
                PlayStatus {
                    status: PlayStatusCode::PlayerSpawn,
                }
                .encode(),
            );
            packets.push(CreativeContent.encode());
            self.stage = Stage::Initializing;
        }
        tracing::debug!(player = %self.player, radius, chunks = offsets.len(), "sent chunks");
        Reply::send(packets)
    }

    fn initialized(&mut self, packet: SetLocalPlayerAsInitialized) -> Reply {
        if packet.entity_runtime_id != PLAYER_ENTITY_ID {
            tracing::warn!(
                runtime_id = packet.entity_runtime_id,
                "client initialized an unexpected entity"
            );
        }
        if self.stage != Stage::InGame {
            self.stage = Stage::InGame;
            tracing::info!(player = %self.player, "player spawned in the world");
        }
        Reply::default()
    }
}

/// StartGame for the flat world: a creative-mode player standing at the spawn.
fn start_game(world: &FlatWorld) -> StartGame {
    let spawn = world.spawn();
    StartGame {
        entity_unique_id: PLAYER_ENTITY_ID as i64,
        entity_runtime_id: PLAYER_ENTITY_ID,
        player_game_mode: 1,
        player_position: Vec3 {
            x: spawn.x as f32 + 0.5,
            y: spawn.y as f32 + EYE_HEIGHT,
            z: spawn.z as f32 + 0.5,
        },
        pitch: 0.0,
        yaw: 0.0,
        world_seed: 0,
        spawn_biome_type: 0,
        user_defined_biome_name: "plains".into(),
        dimension: OVERWORLD,
        generator: 2,
        world_game_mode: 1,
        hardcore: false,
        difficulty: 0,
        world_spawn: spawn,
        achievements_disabled: true,
        editor_world_type: 0,
        created_in_editor: false,
        exported_from_editor: false,
        day_cycle_lock_time: 0,
        education_edition_offer: 0,
        education_features_enabled: false,
        education_product_id: String::new(),
        rain_level: 0.0,
        lightning_level: 0.0,
        confirmed_platform_locked_content: false,
        multiplayer_game: true,
        lan_broadcast_enabled: true,
        xbl_broadcast_mode: 0,
        platform_broadcast_mode: 0,
        commands_enabled: true,
        texture_pack_required: false,
        game_rules: vec![GameRule {
            name: "showcoordinates".into(),
            editable: false,
            value: GameRuleValue::Bool(true),
        }],
        experiments: Vec::new(),
        experiments_previously_toggled: false,
        bonus_chest_enabled: false,
        start_with_map_enabled: false,
        player_permissions: 1,
        server_chunk_tick_radius: 4,
        has_locked_behaviour_pack: false,
        has_locked_texture_pack: false,
        from_locked_world_template: false,
        msa_gamertags_only: false,
        from_world_template: false,
        world_template_settings_locked: false,
        only_spawn_v1_villagers: false,
        persona_disabled: false,
        custom_skins_disabled: false,
        emote_chat_muted: false,
        base_game_version: GAME_VERSION.into(),
        limited_world_width: 0,
        limited_world_depth: 0,
        new_nether: false,
        force_experimental_gameplay: None,
        chat_restriction_level: 0,
        disable_player_interactions: false,
        server_editor_connection_policy: 0,
        allow_anonymous_block_drops_in_editor_worlds: false,
        level_id: String::new(),
        world_name: "Mistvale".into(),
        template_content_identity: String::new(),
        trial: false,
        player_movement_settings: PlayerMovementSettings {
            rewind_history_size: 0,
            server_authoritative_block_breaking: true,
        },
        // Noon.
        time: 6000,
        enchantment_seed: 0,
        blocks: Vec::new(),
        multiplayer_correlation_id: random_uuid(),
        server_authoritative_inventory: true,
        game_version: GAME_VERSION.into(),
        property_data: Compound::new(),
        server_block_state_checksum: 0,
        world_template_id: [0; 16],
        client_side_generation: false,
        use_block_network_id_hashes: true,
        server_authoritative_sound: false,
        server_id: String::new(),
        scenario_id: String::new(),
        world_id: String::new(),
        owner_id: String::new(),
    }
}

/// A random UUID string, from std's randomly seeded hasher.
fn random_uuid() -> String {
    let state = RandomState::new();
    let (high, low) = (state.hash_one(0u8), state.hash_one(1u8));
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        high >> 32,
        (high >> 16) & 0xFFFF,
        high & 0x0FFF,
        ((low >> 48) & 0x3FFF) | 0x8000,
        low & 0xFFFF_FFFF_FFFF
    )
}

#[cfg(test)]
mod tests {
    use mistvale_net::ServerIdentity;
    use mistvale_net::identity::verify_client;
    use mistvale_net::sdp::SdpFingerprint;
    use mistvale_protocol::packet::Decode;

    use super::*;

    /// A client identity and a Login token carrying the same key, as a vanilla
    /// client produces them. Built from a server-style assertion, whose token
    /// has the `cpk` claim a Login token needs.
    fn client() -> (ClientIdentity, String) {
        let key = ServerIdentity::generate("test").unwrap();
        let fingerprints = [SdpFingerprint {
            algorithm: "sha-256".into(),
            digest: "AA".into(),
        }];
        let identity =
            verify_client(&key.assertion(&fingerprints).unwrap(), &fingerprints).unwrap();
        let token = identity.token.clone();
        (identity, token)
    }

    fn session(identity: Option<ClientIdentity>) -> Session {
        Session::new(identity, Arc::new(FlatWorld::new()))
    }

    fn login_packet(token: String) -> Vec<u8> {
        let request = ConnectionRequest {
            authentication_type: 0,
            chain: Vec::new(),
            token,
            client_data: String::new(),
        };
        Login {
            client_protocol: PROTOCOL_VERSION,
            connection_request: request.encode(),
        }
        .encode()
    }

    fn pack_response(response: PackResponse) -> Vec<u8> {
        ResourcePackClientResponse { response }.encode()
    }

    fn ids(reply: &Reply) -> Vec<u32> {
        reply
            .packets
            .iter()
            .map(|packet| packet::read_header(packet).unwrap().0.id)
            .collect()
    }

    fn decode_only<P: Decode>(packet: &[u8]) -> P {
        let (header, payload) = packet::read_header(packet).unwrap();
        assert_eq!(header.id, P::ID);
        packet::decode(payload).unwrap()
    }

    /// Runs a session up to the point where StartGame has been sent.
    fn spawning_session() -> Session {
        let (identity, token) = client();
        let mut session = session(Some(identity));
        let request = RequestNetworkSettings {
            client_protocol: PROTOCOL_VERSION,
        };
        session.handle(&request.encode()).unwrap();
        session.handle(&login_packet(token)).unwrap();
        session
            .handle(&pack_response(PackResponse::DownloadingFinished))
            .unwrap();
        session
            .handle(&pack_response(PackResponse::StackFinished))
            .unwrap();
        assert_eq!(session.stage(), Stage::Spawning);
        session
    }

    #[test]
    fn walks_the_login_handshake() {
        let (identity, token) = client();
        let mut session = session(Some(identity));

        let reply = session
            .handle(
                &RequestNetworkSettings {
                    client_protocol: PROTOCOL_VERSION,
                }
                .encode(),
            )
            .unwrap();
        assert_eq!(ids(&reply), [id::NETWORK_SETTINGS]);
        assert_eq!(reply.enable_compression, Some(COMPRESSION));
        let settings: NetworkSettings = decode_only(&reply.packets[0]);
        assert_eq!(
            settings.compression_algorithm,
            CompressionAlgorithm::Flate.id()
        );

        let reply = session.handle(&login_packet(token)).unwrap();
        assert_eq!(ids(&reply), [id::PLAY_STATUS, id::RESOURCE_PACKS_INFO]);
        let status: PlayStatus = decode_only(&reply.packets[0]);
        assert_eq!(status.status, PlayStatusCode::LoginSuccess);
        assert_eq!(session.stage(), Stage::ResourcePacks);

        // Clients send their blob cache support at some point; it is ignored.
        let cache = [0x81, 0x01, 0x00];
        assert!(session.handle(&cache).unwrap().packets.is_empty());

        let reply = session
            .handle(&pack_response(PackResponse::DownloadingFinished))
            .unwrap();
        assert_eq!(ids(&reply), [id::RESOURCE_PACK_STACK]);

        let reply = session
            .handle(&pack_response(PackResponse::StackFinished))
            .unwrap();
        assert_eq!(
            ids(&reply),
            [
                id::JIGSAW_STRUCTURE_DATA,
                id::VOXEL_SHAPES,
                id::START_GAME,
                id::ITEM_REGISTRY
            ]
        );
        assert!(!reply.close);
        assert_eq!(session.stage(), Stage::Spawning);
    }

    #[test]
    fn spawns_after_sending_the_chunks_in_view() {
        let mut session = spawning_session();

        let request = RequestChunkRadius {
            radius: 4,
            max_radius: 32,
        };
        let reply = session.handle(&request.encode()).unwrap();
        let sent = ids(&reply);
        assert_eq!(
            sent[..2],
            [id::CHUNK_RADIUS_UPDATED, id::NETWORK_CHUNK_PUBLISHER_UPDATE]
        );
        // The chunks within a circle of radius 4: 49 of them.
        let chunks = sent.iter().filter(|id| **id == id::LEVEL_CHUNK).count();
        assert_eq!(chunks, 49);
        assert_eq!(
            sent[sent.len() - 2..],
            [id::PLAY_STATUS, id::CREATIVE_CONTENT]
        );
        let spawn: PlayStatus = decode_only(&reply.packets[sent.len() - 2]);
        assert_eq!(spawn.status, PlayStatusCode::PlayerSpawn);
        assert_eq!(session.stage(), Stage::Initializing);

        // Movement input arrives every tick and is ignored for now.
        let auth_input = [0x90, 0x01, 0x00];
        assert!(session.handle(&auth_input).unwrap().packets.is_empty());

        let initialized = SetLocalPlayerAsInitialized {
            entity_runtime_id: PLAYER_ENTITY_ID,
        };
        assert!(
            session
                .handle(&initialized.encode())
                .unwrap()
                .packets
                .is_empty()
        );
        assert_eq!(session.stage(), Stage::InGame);

        // Changing the render distance later resends chunks without respawning.
        let reply = session.handle(&request.encode()).unwrap();
        assert!(!ids(&reply).contains(&id::PLAY_STATUS));
    }

    #[test]
    fn view_distance_is_capped() {
        let mut session = spawning_session();
        let request = RequestChunkRadius {
            radius: 64,
            max_radius: 64,
        };
        let reply = session.handle(&request.encode()).unwrap();
        let (_, mut payload) = packet::read_header(&reply.packets[0]).unwrap();
        assert_eq!(payload.var_i32().unwrap(), MAX_VIEW_DISTANCE);
    }

    #[test]
    fn rejects_other_protocol_versions_before_compression() {
        for (client_protocol, expected) in [
            (PROTOCOL_VERSION - 1, PlayStatusCode::LoginFailedClient),
            (PROTOCOL_VERSION + 1, PlayStatusCode::LoginFailedServer),
        ] {
            let mut session = session(None);
            let reply = session
                .handle(&RequestNetworkSettings { client_protocol }.encode())
                .unwrap();
            assert!(reply.close);
            assert_eq!(reply.enable_compression, None);
            let status: PlayStatus = decode_only(&reply.packets[0]);
            assert_eq!(status.status, expected);
        }
    }

    #[test]
    fn login_must_carry_the_key_proven_during_signaling() {
        let (identity, _) = client();
        let (_, other_token) = client();
        let mut session = session(Some(identity));
        session
            .handle(
                &RequestNetworkSettings {
                    client_protocol: PROTOCOL_VERSION,
                }
                .encode(),
            )
            .unwrap();

        let reply = session.handle(&login_packet(other_token)).unwrap();
        assert!(reply.close);
        let disconnect: Disconnect = decode_only(&reply.packets[0]);
        assert_eq!(disconnect.reason, DisconnectReason::NOT_AUTHENTICATED);
        assert_eq!(session.stage(), Stage::Login);
    }

    #[test]
    fn out_of_order_packets_disconnect_with_a_reason() {
        let (_, token) = client();
        let mut session = session(None);
        let err = session.handle(&login_packet(token)).unwrap_err();
        assert!(matches!(
            err,
            SessionError::UnexpectedPacket {
                id: id::LOGIN,
                stage: Stage::RequestNetworkSettings
            }
        ));

        let reply = err.into_reply();
        assert!(reply.close);
        let disconnect: Disconnect = decode_only(&reply.packets[0]);
        assert_eq!(disconnect.reason, DisconnectReason::UNEXPECTED_PACKET);
    }

    #[test]
    fn random_uuids_are_well_formed() {
        let uuid = random_uuid();
        assert_eq!(uuid.len(), 36);
        assert_eq!(uuid.as_bytes()[14], b'4');
        assert!(matches!(uuid.as_bytes()[19], b'8' | b'9' | b'a' | b'b'));
    }
}
