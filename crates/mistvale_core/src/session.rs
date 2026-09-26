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
//! 6. SetLocalPlayerAsInitialized: the player is in the world. They join the
//!    [`Players`](crate::players::Players) and plugins hear `player_join`.
//!
//! From then on, chat messages (Text) are relayed to every player unless a
//! plugin cancels them, and plugins hear of blocks broken and placed. Plugins
//! that heard `player_join` hear `player_quit` when the session ends.

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use mistvale_net::{ClientIdentity, Connection, Reliability};
use mistvale_plugins::{BlockChange, Dispatcher, Event, Player, Position};
use mistvale_protocol::batch::{self, BatchError, Compression, CompressionAlgorithm};
use mistvale_protocol::io::DecodeError;
use mistvale_protocol::login::{ConnectionRequest, IdentityClaims, LoginError};
use mistvale_protocol::nbt::Compound;
use mistvale_protocol::packet::{self, Encode as _, id};
use mistvale_protocol::packets::{
    AbilityData, AbilityLayer, Attribute, ChunkRadiusUpdated, CreativeContent, Disconnect,
    DisconnectMessage, DisconnectReason, EXEMPTED_PACKS, GameRule, GameRuleValue,
    InventoryTransaction, JigsawStructureData, Login, NetworkChunkPublisherUpdate, NetworkSettings,
    PackResponse, PlayStatus, PlayStatusCode, PlayerAction, PlayerAuthInput,
    PlayerMovementSettings, RequestChunkRadius, RequestNetworkSettings, ResourcePackClientResponse,
    ResourcePackStack, ResourcePacksInfo, SetActorData, SetLocalPlayerAsInitialized, StackPack,
    StartGame, Text, TextType, UpdateAbilities, UpdateAttributes, VoxelShapes, ability, input_flag,
    player_action, use_item_action,
};
use mistvale_protocol::types::{BlockPos, ChunkPos, Vec3};
use mistvale_protocol::{GAME_VERSION, PROTOCOL_VERSION};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::auth::AuthError;
use crate::inventory;
use crate::logins::KickNotice;
use crate::players::{
    EYE_HEIGHT, Joining, Movement, OUTBOUND_QUEUE, Profile, STANDING_HEIGHT, View, body_overlaps,
    player_metadata,
};
use crate::server::{self, Server};
use crate::storage::SavedPlayer;
use crate::view::ChunkView;
use crate::world::{MAX_Y, MIN_Y, OVERWORLD, World};

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

/// Farthest a player may break a block from, eyes to block centre, in blocks.
/// Creative reach is about 7.5; the slack covers lag between input and movement.
const MAX_REACH: f32 = 12.0;

/// Longest chat message relayed, in characters.
const MAX_CHAT_LENGTH: usize = 512;

/// How long after a player spawns plugins hear of it: 15 ticks, so messages
/// they send arrive once the client's HUD is ready.
const JOIN_EVENT_DELAY: Duration = Duration::from_millis(750);

/// Longest a chat message waits for plugins to decide whether to cancel it.
/// Past that it is sent anyway: a stuck plugin must not silence chat.
const CHAT_VERDICT_TIMEOUT: Duration = Duration::from_secs(2);

/// Serves one client until either side closes the connection.
pub async fn run(mut connection: Connection, server: Arc<Server>) {
    let entity_id = server.players.allocate_entity_id();
    let mut session = Session::new(
        connection.client_identity().cloned(),
        Arc::clone(&server.world),
        entity_id,
    );
    serve(&mut connection, &server, &mut session).await;
    // However the session ended, remember where the player left.
    if let Some((uuid, player)) = session.saved_player() {
        server.save_player(uuid, &player);
    }
}

/// The session's packet loop, until the connection closes or the session
/// ends it.
async fn serve(connection: &mut Connection, server: &Server, session: &mut Session) {
    let network_id = connection.network_id();
    let entity_id = session.entity_id;
    let mut compression = None;
    // Packets other sessions and plugins send this player, e.g. chat.
    let (outbound, mut queued) = mpsc::channel::<Bytes>(OUTBOUND_QUEUE);
    // Set once the player is in the world; leaving the loop drops it.
    let mut membership = None;
    // The plugin join event, waiting out [`JOIN_EVENT_DELAY`].
    let mut join_event: Option<(Pin<Box<tokio::time::Sleep>>, Player)> = None;
    // Set once the player's identity is verified: the one session for that
    // UUID. A newer login for the same player, or a plugin, sends a kick.
    let (kick, mut kicks) = mpsc::channel::<KickNotice>(1);
    let mut login_claim = None;
    // The player as plugins know them, once they are in the world.
    let mut plugin_player: Option<Player> = None;
    // However the session ends, plugins that heard of the join hear of the quit.
    let mut quit_event = QuitEvent {
        plugins: &server.plugins,
        player: None,
    };

    loop {
        tokio::select! {
            message = connection.recv() => {
                let Some(message) = message else {
                    tracing::debug!(network_id, "client closed the connection");
                    return;
                };
                // Packets are handled in order; a reply may queue another, such
                // as the login continuing once its token is verified.
                let mut pending = VecDeque::new();
                let packets = match batch::decode(&message.payload, compression.is_some()) {
                    Ok(packets) => packets,
                    Err(err) => {
                        pending.push_back(SessionError::from(err).into_reply());
                        Vec::new()
                    }
                };
                let mut packets = packets.iter();
                loop {
                    let reply = match pending.pop_front() {
                        Some(reply) => reply,
                        None => match packets.next() {
                            Some(packet) => session.handle(packet).unwrap_or_else(|err| err.into_reply()),
                            None => break,
                        },
                    };
                    let packets = reply.packets.iter().map(Vec::as_slice);
                    if !send(connection, packets, compression).await {
                        return;
                    }
                    if let Some(agreed) = reply.enable_compression {
                        compression = Some(agreed);
                    }
                    for event in reply.events {
                        match event {
                            SessionEvent::Authenticate(token) => {
                                let result = server.authenticator.verify(&token).await;
                                pending.push_back(session.authenticated(result));
                            }
                            SessionEvent::LoggedIn(uuid) => {
                                // Kicks this player's older session, if any.
                                login_claim = Some(server.logins.claim(uuid, kick.clone()));
                            }
                            SessionEvent::Joined { profile, movement, view } => {
                                let player = Player {
                                    name: profile.name.clone(),
                                    uuid: profile.uuid.to_string(),
                                };
                                plugin_player = Some(player.clone());
                                // Join first, so plugins greeting the player reach them too.
                                membership = Some(server.players.join(Joining {
                                    entity_id,
                                    profile,
                                    movement,
                                    view,
                                    outbound: outbound.clone(),
                                }));
                                // Plugins hear of the join a little later: the
                                // client's HUD shows messages that arrive while
                                // it is still starting up twice.
                                join_event = Some((
                                    Box::pin(tokio::time::sleep(JOIN_EVENT_DELAY)),
                                    player,
                                ));
                            }
                            SessionEvent::Moved(movement) => {
                                if let Some(membership) = &membership {
                                    membership.moved(movement);
                                }
                            }
                            SessionEvent::Viewing(view) => {
                                if let Some(membership) = &membership {
                                    membership.viewing(view);
                                }
                            }
                            SessionEvent::BrokeBlock(pos) => {
                                if let Some(broken) = server.break_block(pos)
                                    && let Some(player) = &plugin_player
                                {
                                    server.plugins.dispatch(Event::BlockBreak(
                                        block_change(server, player, pos, broken),
                                    ));
                                }
                            }
                            SessionEvent::PlacedBlock { pos, block } => {
                                if server.place_block(pos, block) {
                                    if let Some(player) = &plugin_player {
                                        server.plugins.dispatch(Event::BlockPlace(
                                            block_change(server, player, pos, block),
                                        ));
                                    }
                                } else {
                                    // Someone may have filled the spot since it
                                    // was checked; undo the client's prediction.
                                    let _ = outbound.try_send(server::block_update(pos, server.world.block(pos)));
                                }
                            }
                            SessionEvent::Swing => {
                                if let Some(membership) = &membership {
                                    membership.swing();
                                }
                            }
                            SessionEvent::Sneaking(sneaking) => {
                                if let Some(membership) = &membership {
                                    membership.sneaking(sneaking);
                                }
                            }
                            SessionEvent::Flying(flying) => {
                                if let Some(membership) = &membership {
                                    membership.flying(flying);
                                }
                            }
                            SessionEvent::Chat(message) => {
                                let cancelled = match &plugin_player {
                                    Some(player) => chat_cancelled(&server.plugins, player, &message).await,
                                    None => false,
                                };
                                if cancelled {
                                    tracing::info!(target: "chat", "[cancelled by a plugin] <{}> {message}", session.player());
                                } else {
                                    server.players.chat(session.player(), &message);
                                }
                            }
                        }
                    }
                    if reply.close {
                        drop(membership.take());
                        linger(connection).await;
                        return;
                    }
                }
            }
            Some(notice) = kicks.recv() => {
                // A newer login for this player, or a plugin: this session ends.
                let reply = Reply::disconnect(notice.reason, notice.message);
                let _ = send(connection, reply.packets.iter().map(Vec::as_slice), compression).await;
                drop(membership.take());
                drop(login_claim.take());
                linger(connection).await;
                return;
            }
            () = async { join_event.as_mut().expect("guarded").0.as_mut().await }, if join_event.is_some() => {
                let (_, player) = join_event.take().expect("guarded");
                quit_event.player = Some(player.clone());
                server.plugins.dispatch(Event::PlayerJoin(player));
            }
            Some(packet) = queued.recv() => {
                // Send everything already waiting in one batch.
                let mut packets = vec![packet];
                while let Ok(packet) = queued.try_recv() {
                    packets.push(packet);
                }
                if !send(connection, packets.iter().map(|packet| &packet[..]), compression).await {
                    return;
                }
            }
        }
    }
}

/// Tells plugins a player left when dropped, if they heard of the join.
struct QuitEvent<'a> {
    plugins: &'a Dispatcher,
    player: Option<Player>,
}

impl Drop for QuitEvent<'_> {
    fn drop(&mut self) {
        if let Some(player) = self.player.take() {
            self.plugins.dispatch(Event::PlayerQuit(player));
        }
    }
}

/// Asks plugins whether to cancel a chat message; a message they take too
/// long over is sent.
async fn chat_cancelled(plugins: &Dispatcher, player: &Player, message: &str) -> bool {
    let event = Event::PlayerChat {
        player: player.clone(),
        message: message.to_owned(),
    };
    match tokio::time::timeout(CHAT_VERDICT_TIMEOUT, plugins.dispatch_cancellable(event)).await {
        Ok(cancelled) => cancelled,
        Err(_) => {
            tracing::warn!(player = %player.name, "plugins took too long over a chat message; sending it");
            false
        }
    }
}

/// A block `player` changed at `pos`, as plugins see it.
fn block_change(server: &Server, player: &Player, pos: BlockPos, block: u32) -> BlockChange {
    BlockChange {
        player: player.clone(),
        position: Position {
            x: pos.x,
            y: pos.y,
            z: pos.z,
        },
        block: server.world.block_name(block).to_owned(),
    }
}

/// Sends packets as one reliable batch. Returns whether the connection is still usable.
async fn send<'a>(
    connection: &Connection,
    packets: impl ExactSizeIterator<Item = &'a [u8]>,
    compression: Option<Compression>,
) -> bool {
    if packets.len() == 0 {
        return true;
    }
    match batch::encode(packets, compression) {
        Ok(batch) => connection
            .send(Bytes::from(batch), Reliability::Reliable)
            .await
            .is_ok(),
        Err(err) => {
            tracing::warn!(network_id = connection.network_id(), %err, "failed to encode a batch");
            false
        }
    }
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
    /// What the rest of the server should hear about, once `packets` has been sent.
    pub events: Vec<SessionEvent>,
}

/// Something a session did that matters beyond its own client.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// The Login's multiplayer token needs verifying; the result goes to
    /// [`Session::authenticated`].
    Authenticate(String),
    /// The player's verified identity: they hold this UUID from now on.
    LoggedIn(Uuid),
    /// The player finished spawning and is in the world.
    Joined {
        profile: Profile,
        movement: Movement,
        view: View,
    },
    /// The player moved or looked around.
    Moved(Movement),
    /// The player's client now shows a different set of chunks.
    Viewing(View),
    /// The player broke the block at this position.
    BrokeBlock(BlockPos),
    /// The player placed `block` (a network ID) at `pos`, which was air.
    PlacedBlock { pos: BlockPos, block: u32 },
    /// The player swung their arm.
    Swing,
    /// The player started or stopped sneaking.
    Sneaking(bool),
    /// The player started or stopped flying.
    Flying(bool),
    /// The player said something in chat.
    Chat(String),
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
    /// Login is received; waiting for its token to be verified.
    Authenticating,
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
    /// The name the player logged in with.
    player: String,
    /// The player's persistent identity, known after login.
    uuid: Uuid,
    /// The runtime and unique ID of the player's entity.
    entity_id: u64,
    /// The chunks around the player and those the client already has.
    view: ChunkView,
    /// Whether the player is flying, as their client last said.
    flying: bool,
    /// Where the player is, as last reported.
    movement: Movement,
    world: Arc<World>,
}

impl Session {
    pub fn new(identity: Option<ClientIdentity>, world: Arc<World>, entity_id: u64) -> Self {
        let spawn = world.spawn();
        Self {
            stage: Stage::RequestNetworkSettings,
            identity,
            player: String::from("<unknown>"),
            uuid: Uuid::nil(),
            entity_id,
            movement: Movement {
                position: Vec3 {
                    x: spawn.x as f32 + 0.5,
                    y: spawn.y as f32 + EYE_HEIGHT,
                    z: spawn.z as f32 + 0.5,
                },
                pitch: 0.0,
                yaw: 0.0,
                head_yaw: 0.0,
                on_ground: true,
            },
            view: ChunkView::new(),
            flying: false,
            world,
        }
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// The player's name, once logged in.
    pub fn player(&self) -> &str {
        &self.player
    }

    /// The player's UUID and what to save for them, once they have been in
    /// the world; players who never finished spawning are not saved.
    pub fn saved_player(&self) -> Option<(Uuid, SavedPlayer)> {
        (self.stage == Stage::InGame).then(|| (self.uuid, self.movement.saved(self.flying)))
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
            (Stage::InGame, id::TEXT) => Ok(self.text(packet::decode(payload)?)),
            (Stage::InGame, id::PLAYER_AUTH_INPUT) => Ok(self.auth_input(packet::decode(payload)?)),
            (Stage::InGame, id::PLAYER_ACTION) => Ok(self.player_action(packet::decode(payload)?)),
            // Only item use is decoded; a transaction that cannot be read is
            // ignored rather than ending the session.
            (Stage::InGame, id::INVENTORY_TRANSACTION) => match packet::decode(payload) {
                Ok(transaction) => Ok(self.inventory_transaction(transaction)),
                Err(err) => {
                    tracing::debug!(player = %self.player, %err, "ignoring an unreadable inventory transaction");
                    Ok(Reply::default())
                }
            },
            (stage, id) if stage.in_world() => {
                // Movement input starts before the player is initialized.
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
            ..Reply::default()
        })
    }

    /// Reads the connection request and asks for its multiplayer token to be
    /// verified; [`Session::authenticated`] continues with the result.
    fn login(&mut self, login: Login) -> Result<Reply, SessionError> {
        let request = ConnectionRequest::parse(&login.connection_request)?;
        self.stage = Stage::Authenticating;
        Ok(Reply {
            events: vec![SessionEvent::Authenticate(request.token)],
            ..Reply::default()
        })
    }

    /// Continues the login with the verified identity from the multiplayer
    /// token, or disconnects a player who could not be verified.
    pub fn authenticated(&mut self, result: Result<IdentityClaims, AuthError>) -> Reply {
        if self.stage != Stage::Authenticating {
            return Reply::default();
        }
        let claims = match result {
            Ok(claims) => claims,
            Err(err) => {
                tracing::info!(%err, "rejecting a player who could not be authenticated");
                return Reply::disconnect(
                    DisconnectReason::NOT_AUTHENTICATED,
                    format!("Mistvale BDS: could not verify your Microsoft account.\n{err}"),
                );
            }
        };

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
                return Reply::disconnect(
                    DisconnectReason::NOT_AUTHENTICATED,
                    "Mistvale BDS: your login does not match this connection.",
                );
            }
        }

        // Verified tokens always name the player; the fallbacks only apply to
        // unchecked tokens in offline mode.
        self.uuid = claims.identity.unwrap_or_else(|| {
            tracing::debug!("login has no persistent identity; using a random UUID");
            Uuid::new_v4()
        });
        self.player = claims
            .display_name
            .unwrap_or_else(|| String::from("Player"));
        // Returning players start where they left.
        if let Some(saved) = self.world.load_player(self.uuid) {
            self.movement = Movement::from_saved(&saved);
            self.flying = saved.flying;
            tracing::debug!(uuid = %self.uuid, ?saved, "restored the player's position");
        }
        tracing::info!(name = %self.player, uuid = %self.uuid, "player logged in");
        self.stage = Stage::ResourcePacks;
        Reply {
            packets: vec![
                PlayStatus {
                    status: PlayStatusCode::LoginSuccess,
                }
                .encode(),
                ResourcePacksInfo::default().encode(),
            ],
            events: vec![SessionEvent::LoggedIn(self.uuid)],
            ..Reply::default()
        }
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
                    start_game(&self.world, self.entity_id, &self.movement).encode(),
                    // Only the items players are given.
                    inventory::item_registry().encode(),
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

    /// Grants a view distance and sends the chunks in it the client lacks,
    /// nearest first. The first time, this also lets the client spawn.
    fn chunk_radius(&mut self, request: RequestChunkRadius) -> Reply {
        let radius = request.radius.clamp(1, MAX_VIEW_DISTANCE);
        let mut packets = vec![ChunkRadiusUpdated { radius }.encode()];
        packets.extend(self.stream_chunks(radius));
        if self.stage == Stage::Spawning {
            // The player's own entity data: without HasGravity the client
            // does not pull its player down, and they float.
            packets.push(
                SetActorData {
                    entity_runtime_id: self.entity_id,
                    metadata: player_metadata(&self.player, false),
                    tick: 0,
                }
                .encode(),
            );
            // Speeds: the client moves its own player, using the movement
            // attribute and the walk and fly speeds of its ability layer.
            packets.push(self.own_attributes().encode());
            packets.push(self.own_abilities().encode());
            // A hotbar of blocks to build with.
            packets.push(inventory::starting_inventory().encode());
            packets.push(
                PlayStatus {
                    status: PlayStatusCode::PlayerSpawn,
                }
                .encode(),
            );
            packets.push(CreativeContent.encode());
            self.stage = Stage::Initializing;
        }
        Reply {
            packets,
            events: self.view_event(),
            ..Reply::default()
        }
    }

    /// Vanilla defaults for the attributes that govern how the client moves
    /// its player, plus health.
    fn own_attributes(&self) -> UpdateAttributes {
        UpdateAttributes {
            entity_runtime_id: self.entity_id,
            attributes: vec![
                Attribute::at_default("minecraft:movement", 0.0, f32::MAX, ability::WALK_SPEED),
                Attribute::at_default("minecraft:underwater_movement", 0.0, f32::MAX, 0.02),
                Attribute::at_default("minecraft:lava_movement", 0.0, f32::MAX, 0.02),
                Attribute::at_default("minecraft:health", 0.0, 20.0, 20.0),
            ],
            tick: 0,
        }
    }

    /// A creative player's abilities at vanilla walk and fly speeds.
    fn own_abilities(&self) -> UpdateAbilities {
        let creative = ability::BUILD
            | ability::MINE
            | ability::DOORS_AND_SWITCHES
            | ability::OPEN_CONTAINERS
            | ability::ATTACK_PLAYERS
            | ability::ATTACK_MOBS
            | ability::INVULNERABLE
            | ability::MAY_FLY
            | ability::INSTANT_BUILD;
        // Flying is an ability value too: granting it keeps a player who
        // left in the air flying when they return.
        let values = if self.flying {
            creative | ability::FLYING
        } else {
            creative
        };
        UpdateAbilities(AbilityData {
            entity_unique_id: i64::try_from(self.entity_id)
                .expect("entity IDs stay far below i64::MAX"),
            player_permissions: 1,
            command_permissions: 0,
            layers: vec![AbilityLayer::base(values)],
        })
    }

    /// The chunks the client shows now, for the rest of the server once the
    /// player is in the world.
    fn view_event(&self) -> Vec<SessionEvent> {
        match self.view.centre() {
            Some(centre) if self.stage == Stage::InGame => vec![SessionEvent::Viewing(View {
                centre,
                radius: self.view.radius(),
            })],
            _ => Vec::new(),
        }
    }

    /// Centres the view on the player's chunk: a NetworkChunkPublisherUpdate,
    /// so the client renders around its new position, then every chunk in
    /// range it does not have yet.
    fn stream_chunks(&mut self, radius: i32) -> Vec<Vec<u8>> {
        let block = BlockPos::containing(self.movement.feet());
        let centre = ChunkPos::of_block(block);
        let chunks = self.view.update(centre, radius);

        let mut packets = Vec::with_capacity(chunks.len() + 1);
        packets.push(
            NetworkChunkPublisherUpdate {
                position: block,
                radius: radius.unsigned_abs() << 4,
            }
            .encode(),
        );
        packets.extend(
            chunks
                .iter()
                .map(|chunk| self.world.chunk(chunk.x, chunk.z).encode()),
        );
        tracing::debug!(
            player = %self.player,
            centre = ?(centre.x, centre.z),
            radius,
            sent = chunks.len(),
            "streamed chunks"
        );
        packets
    }

    fn initialized(&mut self, packet: SetLocalPlayerAsInitialized) -> Reply {
        if packet.entity_runtime_id != self.entity_id {
            tracing::warn!(
                runtime_id = packet.entity_runtime_id,
                "client initialized an unexpected entity"
            );
        }
        if self.stage == Stage::InGame {
            return Reply::default();
        }
        self.stage = Stage::InGame;
        tracing::info!(player = %self.player, entity_id = self.entity_id, "player spawned in the world");
        Reply {
            events: vec![SessionEvent::Joined {
                profile: Profile {
                    name: self.player.clone(),
                    uuid: self.uuid,
                },
                movement: self.movement,
                view: View {
                    centre: self.view.centre().unwrap_or_else(|| self.movement.chunk()),
                    radius: self.view.radius(),
                },
            }]
            .into_iter()
            // A returning player still flying: remembered for the next save.
            .chain(self.flying.then_some(SessionEvent::Flying(true)))
            .collect(),
            ..Reply::default()
        }
    }

    /// Records where the client says its player is. The client is trusted
    /// for now: movement is not validated beyond rejecting non-finite values.
    fn auth_input(&mut self, input: PlayerAuthInput) -> Reply {
        // Blocks broken this tick, even when the player stands still.
        if input.block_actions_unread {
            tracing::debug!(player = %self.player, "block actions hidden behind an item stack request");
        }
        let mut events: Vec<SessionEvent> = input
            .block_actions
            .iter()
            .filter(|action| {
                // Everyone is in creative mode, where starting to break a
                // block breaks it; survival clients finish with a prediction.
                matches!(
                    action.action,
                    player_action::START_BREAK | player_action::PREDICT_DESTROY_BLOCK
                )
            })
            .filter_map(|action| self.break_block(action.position))
            .collect();

        // What others see: the arm swinging at air or at a block, and sneaking.
        let flags = &input.input_flags;
        if flags.contains(&input_flag::MISSED_SWING) || !events.is_empty() {
            events.push(SessionEvent::Swing);
        }
        if flags.contains(&input_flag::START_SNEAKING) {
            events.push(SessionEvent::Sneaking(true));
        } else if flags.contains(&input_flag::STOP_SNEAKING) {
            events.push(SessionEvent::Sneaking(false));
        }

        // Flying: remembered for saving, and answered with the abilities that
        // accept it, as the client expects.
        let flying = if flags.contains(&input_flag::START_FLYING) {
            Some(true)
        } else if flags.contains(&input_flag::STOP_FLYING) {
            Some(false)
        } else {
            None
        };
        let mut packets = Vec::new();
        if let Some(flying) = flying {
            self.flying = flying;
            events.push(SessionEvent::Flying(flying));
            packets.push(self.own_abilities().encode());
        }

        let mut reply = self.movement_input(input);
        reply.packets.extend(packets);
        reply.events.extend(events);
        reply
    }

    /// Right-clicking a block with a hotbar block places it against the
    /// clicked face. The client predicts the placement, so a refused one is
    /// undone with the block that is really there.
    fn inventory_transaction(&mut self, transaction: InventoryTransaction) -> Reply {
        let InventoryTransaction::UseItem(use_item) = transaction else {
            return Reply::default();
        };
        if use_item.action != use_item_action::CLICK_BLOCK || use_item.held_item.is_empty() {
            return Reply::default();
        }
        let target = use_item.target();
        let block = inventory::hotbar_block(use_item.hotbar_slot)
            .filter(|block| *block == use_item.held_item.block_runtime_id);
        match block {
            Some(block) if self.may_place(target) => Reply {
                events: vec![
                    SessionEvent::PlacedBlock { pos: target, block },
                    SessionEvent::Swing,
                ],
                ..Reply::default()
            },
            _ => {
                tracing::debug!(player = %self.player, ?target, slot = use_item.hotbar_slot, "refusing a placement");
                Reply::send(vec![
                    server::block_update(target, self.world.block(target)).to_vec(),
                ])
            }
        }
    }

    /// Whether a block may go at `pos`: reachable, in a loaded chunk, into
    /// air, and not inside the player placing it.
    fn may_place(&self, pos: BlockPos) -> bool {
        if self.break_block(pos).is_none() || self.world.block(pos) != self.world.air() {
            return false;
        }
        // A quick check against the placer's own box, from their latest
        // input; `Server::place_block` checks every player's.
        !body_overlaps(self.movement.feet(), STANDING_HEIGHT, pos)
    }

    /// A PlayerAction: creative clients report instant breaks this way too.
    fn player_action(&mut self, action: PlayerAction) -> Reply {
        let events = match action.action {
            player_action::CREATIVE_DESTROY_BLOCK => self
                .break_block(action.block_position)
                .into_iter()
                .collect(),
            _ => Vec::new(),
        };
        Reply {
            events,
            ..Reply::default()
        }
    }

    /// Checks that the player may break the block at `pos`: inside the
    /// world's height, within reach, and in a chunk their client has.
    fn break_block(&self, pos: BlockPos) -> Option<SessionEvent> {
        let centre = Vec3 {
            x: pos.x as f32 + 0.5,
            y: pos.y as f32 + 0.5,
            z: pos.z as f32 + 0.5,
        };
        let eyes = self.movement.position;
        let reach_squared =
            (centre.x - eyes.x).powi(2) + (centre.y - eyes.y).powi(2) + (centre.z - eyes.z).powi(2);
        let in_view = self.view.centre().is_some_and(|centre| {
            View {
                centre,
                radius: self.view.radius(),
            }
            .contains(ChunkPos::of_block(pos))
        });
        if !(MIN_Y..=MAX_Y).contains(&pos.y) || reach_squared > MAX_REACH * MAX_REACH || !in_view {
            tracing::debug!(player = %self.player, ?pos, "refusing to break an unreachable block");
            return None;
        }
        Some(SessionEvent::BrokeBlock(pos))
    }

    fn movement_input(&mut self, input: PlayerAuthInput) -> Reply {
        let movement = Movement {
            position: input.position,
            pitch: input.pitch,
            yaw: input.yaw,
            head_yaw: input.head_yaw,
            // Standing or walking on the ground leaves no vertical movement.
            on_ground: input.delta.y == 0.0,
        };
        let finite = movement.position.is_finite()
            && [movement.pitch, movement.yaw, movement.head_yaw]
                .iter()
                .all(|angle| angle.is_finite());
        if !finite {
            tracing::debug!(player = %self.player, ?input, "ignoring non-finite movement");
            return Reply::default();
        }
        if movement == self.movement {
            return Reply::default();
        }
        self.movement = movement;

        // Crossing into another chunk moves the view along with the player.
        let mut events = vec![SessionEvent::Moved(movement)];
        let packets = if self.view.crosses_into(movement.chunk()) {
            let packets = self.stream_chunks(self.view.radius());
            events.extend(self.view_event());
            packets
        } else {
            Vec::new()
        };
        Reply {
            packets,
            events,
            ..Reply::default()
        }
    }

    /// Relays the player's chat messages; clients send no other kind of text.
    fn text(&mut self, text: Text) -> Reply {
        if text.text_type != TextType::Chat {
            tracing::debug!(player = %self.player, text_type = ?text.text_type, "ignoring text that is not chat");
            return Reply::default();
        }
        // Line breaks and other control characters would let a player fake
        // extra lines, such as a message from someone else.
        let message: String = text
            .message
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let message = message.trim();
        if message.is_empty() {
            return Reply::default();
        }
        if message.chars().count() > MAX_CHAT_LENGTH {
            let warning =
                format!("§cChat messages can be at most {MAX_CHAT_LENGTH} characters long.");
            return Reply::send(vec![Text::raw(warning).encode()]);
        }
        tracing::info!(target: "chat", "<{}> {message}", self.player);
        Reply {
            events: vec![SessionEvent::Chat(message.to_owned())],
            ..Reply::default()
        }
    }
}

/// StartGame for the flat world: a creative-mode player standing at the spawn.
fn start_game(world: &World, entity_id: u64, movement: &Movement) -> StartGame {
    let spawn = world.spawn();
    StartGame {
        entity_unique_id: i64::try_from(entity_id).expect("entity IDs stay far below i64::MAX"),
        entity_runtime_id: entity_id,
        player_game_mode: 1,
        player_position: movement.position,
        pitch: movement.pitch,
        yaw: movement.yaw,
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
        multiplayer_correlation_id: Uuid::new_v4().to_string(),
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

#[cfg(test)]
mod tests {
    use mistvale_net::ServerIdentity;
    use mistvale_net::identity::verify_client;
    use mistvale_net::sdp::SdpFingerprint;
    use mistvale_protocol::packet::Decode;
    use mistvale_protocol::packets::{BlockAction, UseItem};

    use super::*;

    /// The entity ID tests give the player.
    const PLAYER_ENTITY_ID: u64 = 1;

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
        Session::new(identity, Arc::new(World::new()), PLAYER_ENTITY_ID)
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

    /// Sends a Login and answers its authentication request as offline
    /// verification would, with the token's own claims. Returns the reply
    /// that continues the login.
    fn log_in(session: &mut Session, token: String) -> Reply {
        let reply = session.handle(&login_packet(token.clone())).unwrap();
        assert!(
            reply.packets.is_empty(),
            "nothing is sent before verification"
        );
        assert_eq!(reply.events, [SessionEvent::Authenticate(token.clone())]);
        assert_eq!(session.stage(), Stage::Authenticating);
        let claims = mistvale_protocol::login::Jwt::parse(&token).unwrap().claims;
        session.authenticated(Ok(IdentityClaims::from_token_claims(&claims)))
    }

    #[test]
    fn unverified_players_are_disconnected() {
        let (identity, token) = client();
        let mut session = session(Some(identity));
        session
            .handle(
                &RequestNetworkSettings {
                    client_protocol: PROTOCOL_VERSION,
                }
                .encode(),
            )
            .unwrap();
        session.handle(&login_packet(token)).unwrap();
        let reply = session.authenticated(Err(AuthError::BadSignature));
        assert!(reply.close);
        let disconnect: Disconnect = decode_only(&reply.packets[0]);
        assert_eq!(disconnect.reason, DisconnectReason::NOT_AUTHENTICATED);
        let message = disconnect.message.unwrap().message;
        assert!(message.contains("signature"), "{message}");
        assert!(reply.events.is_empty(), "no identity is claimed");
    }

    #[test]
    fn a_verified_login_claims_its_uuid() {
        let (identity, token) = client();
        let mut session = session(Some(identity));
        session
            .handle(
                &RequestNetworkSettings {
                    client_protocol: PROTOCOL_VERSION,
                }
                .encode(),
            )
            .unwrap();
        let reply = log_in(&mut session, token);
        let [SessionEvent::LoggedIn(uuid)] = reply.events[..] else {
            panic!("expected a login, got {:?}", reply.events);
        };
        assert_eq!(uuid, session.uuid);
        assert!(!uuid.is_nil());
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
        log_in(&mut session, token);
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

        let reply = log_in(&mut session, token);
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
        // The player's own entity data (gravity), attributes and abilities
        // (speeds) come just before PlayerSpawn.
        assert_eq!(
            sent[sent.len() - 6..],
            [
                id::SET_ACTOR_DATA,
                id::UPDATE_ATTRIBUTES,
                id::UPDATE_ABILITIES,
                id::INVENTORY_CONTENT,
                id::PLAY_STATUS,
                id::CREATIVE_CONTENT
            ]
        );
        let attributes = &reply.packets[sent.len() - 5];
        let movement = b"minecraft:movement";
        let at = attributes
            .windows(movement.len())
            .position(|window| window == movement)
            .expect("the movement attribute is sent");
        // The six floats before the name end with the default: vanilla's 0.1.
        assert_eq!(attributes[at - 5..at - 1], 0.1f32.to_le_bytes());
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

        let reply = log_in(&mut session, other_token);
        assert!(reply.close);
        let disconnect: Disconnect = decode_only(&reply.packets[0]);
        assert_eq!(disconnect.reason, DisconnectReason::NOT_AUTHENTICATED);
        assert_eq!(session.stage(), Stage::Authenticating);
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
    fn joins_once_initialized_then_relays_chat() {
        let mut session = spawning_session();
        let request = RequestChunkRadius {
            radius: 2,
            max_radius: 32,
        };
        session.handle(&request.encode()).unwrap();

        // Chat before spawning finishes is ignored.
        let chat = |message: &str| Text {
            text_type: TextType::Chat,
            source_name: "Spoofed".into(),
            ..Text::raw(message)
        };
        assert_eq!(
            session.handle(&chat("too early").encode()).unwrap().events,
            []
        );

        let initialized = SetLocalPlayerAsInitialized {
            entity_runtime_id: PLAYER_ENTITY_ID,
        };
        let reply = session.handle(&initialized.encode()).unwrap();
        let [
            SessionEvent::Joined {
                profile,
                movement,
                view,
            },
        ] = &reply.events[..]
        else {
            panic!("expected a join, got {:?}", reply.events);
        };
        assert_eq!(profile.name, session.player());
        assert!(!profile.uuid.is_nil());
        // Their client shows the chunks around the spawn.
        assert_eq!(
            *view,
            View {
                centre: ChunkPos::new(0, 0),
                radius: 2
            }
        );
        // The player joins where StartGame put them: eyes above the spawn block.
        assert_eq!(
            movement.position,
            Vec3 {
                x: 0.5,
                y: -60.0 + EYE_HEIGHT,
                z: 0.5
            }
        );
        // Initializing again does not join twice.
        assert!(
            session
                .handle(&initialized.encode())
                .unwrap()
                .events
                .is_empty()
        );

        // The author comes from the session, never the packet; control
        // characters cannot start a fake line.
        let reply = session
            .handle(&chat("  hi\n<Admin> op me ").encode())
            .unwrap();
        assert_eq!(
            reply.events,
            [SessionEvent::Chat("hi <Admin> op me".into())]
        );
        assert!(reply.packets.is_empty());
    }

    fn auth_input(position: Vec3, yaw: f32) -> Vec<u8> {
        breaking_input(position, yaw, Vec::new())
    }

    fn breaking_input(position: Vec3, yaw: f32, block_actions: Vec<BlockAction>) -> Vec<u8> {
        PlayerAuthInput {
            pitch: 0.0,
            yaw,
            position,
            move_vector: Default::default(),
            head_yaw: yaw,
            input_flags: Vec::new(),
            input_mode: 1,
            play_mode: 0,
            interaction_model: 0,
            interact_rotation: Default::default(),
            tick: 1,
            delta: Vec3::default(),
            block_actions,
            block_actions_unread: false,
        }
        .encode()
    }

    /// A session whose player is in the world, standing at the spawn.
    fn in_game_session() -> Session {
        let mut session = spawning_session();
        let request = RequestChunkRadius {
            radius: 4,
            max_radius: 4,
        };
        session.handle(&request.encode()).unwrap();
        let initialized = SetLocalPlayerAsInitialized {
            entity_runtime_id: PLAYER_ENTITY_ID,
        };
        session.handle(&initialized.encode()).unwrap();
        session
    }

    fn place(slot: i32, block_position: BlockPos, face: u8) -> Vec<u8> {
        let held_item = inventory::starting_inventory().content[slot.clamp(0, 35) as usize];
        InventoryTransaction::UseItem(UseItem {
            action: use_item_action::CLICK_BLOCK,
            trigger: 1,
            block_position,
            face,
            hotbar_slot: slot,
            held_item,
            player_position: Vec3::default(),
            clicked_position: Vec3::default(),
            block_runtime_id: 0,
            client_prediction: 1,
        })
        .encode()
    }

    #[test]
    fn places_hotbar_blocks_against_the_clicked_face() {
        let mut session = in_game_session();
        // Click the top (face 1) of the grass two blocks east of the player.
        let grass = BlockPos { x: 2, y: -61, z: 0 };
        let reply = session.handle(&place(0, grass, 1)).unwrap();
        let stone = inventory::hotbar_block(0).unwrap();
        assert_eq!(
            reply.events,
            [
                SessionEvent::PlacedBlock {
                    pos: BlockPos { x: 2, y: -60, z: 0 },
                    block: stone
                },
                SessionEvent::Swing
            ]
        );
        assert!(reply.packets.is_empty());
    }

    #[test]
    fn refused_placements_are_undone_on_the_client() {
        let mut session = in_game_session();
        // The player stands on (0, -61, 0): placing on top of it would be
        // inside them. The client already shows the block, so it gets air back.
        for refused in [
            place(0, BlockPos { x: 0, y: -61, z: 0 }, 1),
            // Clicking the side of grass targets grass: not air.
            place(0, BlockPos { x: 2, y: -61, z: 0 }, 5),
            // Out of reach, and an empty slot.
            place(
                0,
                BlockPos {
                    x: 32,
                    y: -61,
                    z: 0,
                },
                1,
            ),
            place(20, BlockPos { x: 2, y: -61, z: 0 }, 1),
        ] {
            let reply = session.handle(&refused).unwrap();
            assert!(reply.events.is_empty(), "{:?}", reply.events);
        }
        let reply = session
            .handle(&place(0, BlockPos { x: 0, y: -61, z: 0 }, 1))
            .unwrap();
        assert_eq!(ids(&reply), [id::UPDATE_BLOCK]);
    }

    #[test]
    fn new_players_start_centred_on_the_spawn_block() {
        let mut session = session(None);
        session
            .handle(
                &RequestNetworkSettings {
                    client_protocol: PROTOCOL_VERSION,
                }
                .encode(),
            )
            .unwrap();
        let (_, token) = client();
        log_in(&mut session, token);
        session
            .handle(&pack_response(PackResponse::DownloadingFinished))
            .unwrap();
        let reply = session
            .handle(&pack_response(PackResponse::StackFinished))
            .unwrap();
        // StartGame's player position: eyes above the middle of block (0, -60, 0).
        let start_game = &reply.packets[2];
        let mut position = Vec::new();
        for value in [0.5f32, -60.0 + EYE_HEIGHT, 0.5] {
            position.extend(value.to_le_bytes());
        }
        assert!(
            start_game
                .windows(position.len())
                .any(|window| window == position),
            "StartGame places the player at (0.5, {}, 0.5)",
            -60.0 + EYE_HEIGHT
        );
    }

    #[test]
    fn flying_is_acknowledged_and_remembered() {
        let mut session = in_game_session();
        let here = Vec3 {
            x: 0.5,
            y: -58.0 + EYE_HEIGHT,
            z: 0.5,
        };
        let with_flags = |flags: Vec<i32>| {
            let mut input =
                PlayerAuthInput::decode_payload(&mut mistvale_protocol::io::Reader::new(
                    &breaking_input(here, 0.0, Vec::new())[2..],
                ))
                .unwrap();
            input.input_flags = flags;
            input.encode()
        };
        let reply = session
            .handle(&with_flags(vec![input_flag::START_FLYING]))
            .unwrap();
        assert!(reply.events.contains(&SessionEvent::Flying(true)));
        // The client is answered with abilities that include flying.
        assert_eq!(ids(&reply), [id::UPDATE_ABILITIES]);
        assert!(session.saved_player().unwrap().1.flying);

        let reply = session
            .handle(&with_flags(vec![input_flag::STOP_FLYING]))
            .unwrap();
        assert!(reply.events.contains(&SessionEvent::Flying(false)));
        assert!(!session.saved_player().unwrap().1.flying);
    }

    #[test]
    fn swings_and_sneaking_come_from_input_flags() {
        let mut session = in_game_session();
        let here = Vec3 {
            x: 0.5,
            y: -60.0 + EYE_HEIGHT,
            z: 0.5,
        };
        let with_flags = |flags: Vec<i32>| {
            let mut input =
                PlayerAuthInput::decode_payload(&mut mistvale_protocol::io::Reader::new(
                    &breaking_input(here, 0.0, Vec::new())[2..],
                ))
                .unwrap();
            input.input_flags = flags;
            input.encode()
        };
        let reply = session
            .handle(&with_flags(vec![input_flag::MISSED_SWING]))
            .unwrap();
        assert_eq!(reply.events, [SessionEvent::Swing]);
        let reply = session
            .handle(&with_flags(vec![input_flag::START_SNEAKING]))
            .unwrap();
        assert_eq!(reply.events, [SessionEvent::Sneaking(true)]);
        let reply = session
            .handle(&with_flags(vec![input_flag::STOP_SNEAKING]))
            .unwrap();
        assert_eq!(reply.events, [SessionEvent::Sneaking(false)]);
    }

    #[test]
    fn breaks_blocks_in_reach_from_either_packet() {
        let mut session = in_game_session();
        let spawn_eyes = Vec3 {
            x: 0.5,
            y: -60.0 + EYE_HEIGHT,
            z: 0.5,
        };
        let grass = BlockPos { x: 1, y: -61, z: 0 };

        // Creative clients start breaking in PlayerAuthInput, while standing still.
        let start = BlockAction {
            action: player_action::START_BREAK,
            position: grass,
            face: 1,
        };
        let reply = session
            .handle(&breaking_input(spawn_eyes, 0.0, vec![start]))
            .unwrap();
        assert!(reply.events.contains(&SessionEvent::BrokeBlock(grass)));
        // Aborting a break breaks nothing.
        let abort = BlockAction {
            action: player_action::ABORT_BREAK,
            ..start
        };
        let reply = session
            .handle(&breaking_input(spawn_eyes, 0.0, vec![abort]))
            .unwrap();
        assert!(reply.events.is_empty());

        // And report it in a PlayerAction.
        let destroy = PlayerAction {
            entity_runtime_id: PLAYER_ENTITY_ID,
            action: player_action::CREATIVE_DESTROY_BLOCK,
            block_position: grass,
            result_position: BlockPos::default(),
            face: 1,
        };
        let reply = session.handle(&destroy.encode()).unwrap();
        assert_eq!(reply.events, [SessionEvent::BrokeBlock(grass)]);

        // Out of reach, below the world or in an unloaded chunk: refused.
        for pos in [
            BlockPos {
                x: 22,
                y: -61,
                z: 0,
            },
            BlockPos { x: 0, y: -65, z: 0 },
            BlockPos {
                x: 0,
                y: -61,
                z: 900,
            },
        ] {
            let reply = session
                .handle(
                    &PlayerAction {
                        block_position: pos,
                        ..destroy
                    }
                    .encode(),
                )
                .unwrap();
            assert!(reply.events.is_empty(), "{pos:?}");
        }
    }

    /// The chunk coordinates of the LevelChunks in a reply.
    fn chunks_in(reply: &Reply) -> Vec<(i32, i32)> {
        reply
            .packets
            .iter()
            .filter_map(|packet| {
                let (header, mut payload) = packet::read_header(packet).unwrap();
                (header.id == id::LEVEL_CHUNK)
                    .then(|| (payload.var_i32().unwrap(), payload.var_i32().unwrap()))
            })
            .collect()
    }

    #[test]
    fn streams_chunks_when_crossing_into_another_chunk() {
        let mut session = spawning_session();
        let request = RequestChunkRadius {
            radius: 8,
            max_radius: 8,
        };
        let reply = session.handle(&request.encode()).unwrap();
        let first = chunks_in(&reply);
        assert_eq!(
            first.len(),
            197,
            "the circle of radius 8 around chunk (0, 0)"
        );
        session
            .handle(
                &SetLocalPlayerAsInitialized {
                    entity_runtime_id: PLAYER_ENTITY_ID,
                }
                .encode(),
            )
            .unwrap();

        // Walking within the spawn chunk sends nothing new.
        let eyes = -60.0 + EYE_HEIGHT;
        let within = Vec3 {
            x: 15.5,
            y: eyes,
            z: 8.5,
        };
        let reply = session.handle(&auth_input(within, 0.0)).unwrap();
        assert!(reply.packets.is_empty());

        // Stepping east into chunk (1, 0) recentres the view and sends the new edge.
        let across = Vec3 {
            x: 16.2,
            y: eyes,
            z: 8.5,
        };
        let reply = session.handle(&auth_input(across, 0.0)).unwrap();
        assert_eq!(
            packet::read_header(&reply.packets[0]).unwrap().0.id,
            id::NETWORK_CHUNK_PUBLISHER_UPDATE
        );
        let update: NetworkChunkPublisherUpdate = decode_only(&reply.packets[0]);
        assert_eq!(
            update.position,
            BlockPos {
                x: 16,
                y: -60,
                z: 8
            }
        );
        assert_eq!(update.radius, 8 << 4);
        let streamed = chunks_in(&reply);
        assert!(streamed.contains(&(9, 0)), "{streamed:?}");
        assert!(streamed.iter().all(|chunk| !first.contains(chunk)));
        // The rest of the server hears about the new view, for entity tracking.
        assert!(reply.events.contains(&SessionEvent::Viewing(View {
            centre: ChunkPos::new(1, 0),
            radius: 8
        })));

        // Coming back sends the west edge again, which the client unloaded.
        let reply = session.handle(&auth_input(within, 0.0)).unwrap();
        assert!(chunks_in(&reply).contains(&(-8, 0)));
    }

    #[test]
    fn reports_movement_only_in_game_and_when_it_changes() {
        let mut session = spawning_session();
        let here = Vec3 {
            x: 10.0,
            y: -58.38,
            z: 3.0,
        };
        // Input arrives before the player is initialized; it is ignored.
        assert!(
            session
                .handle(&auth_input(here, 0.0))
                .unwrap()
                .events
                .is_empty()
        );

        session
            .handle(
                &RequestChunkRadius {
                    radius: 1,
                    max_radius: 1,
                }
                .encode(),
            )
            .unwrap();
        session
            .handle(
                &SetLocalPlayerAsInitialized {
                    entity_runtime_id: PLAYER_ENTITY_ID,
                }
                .encode(),
            )
            .unwrap();

        let reply = session.handle(&auth_input(here, 90.0)).unwrap();
        let [SessionEvent::Moved(movement)] = &reply.events[..] else {
            panic!("expected a move, got {:?}", reply.events);
        };
        assert_eq!((movement.position, movement.yaw), (here, 90.0));
        assert!(movement.on_ground);

        // Standing still sends the same input every tick.
        assert!(
            session
                .handle(&auth_input(here, 90.0))
                .unwrap()
                .events
                .is_empty()
        );
        // Positions that are not numbers are ignored.
        let nowhere = Vec3 {
            x: f32::NAN,
            ..here
        };
        assert!(
            session
                .handle(&auth_input(nowhere, 90.0))
                .unwrap()
                .events
                .is_empty()
        );
    }

    #[test]
    fn rejects_overlong_chat_with_a_warning() {
        let mut session = spawning_session();
        session
            .handle(
                &RequestChunkRadius {
                    radius: 1,
                    max_radius: 1,
                }
                .encode(),
            )
            .unwrap();
        session
            .handle(
                &SetLocalPlayerAsInitialized {
                    entity_runtime_id: PLAYER_ENTITY_ID,
                }
                .encode(),
            )
            .unwrap();

        let long = Text {
            text_type: TextType::Chat,
            ..Text::raw("a".repeat(MAX_CHAT_LENGTH + 1))
        };
        let reply = session.handle(&long.encode()).unwrap();
        assert!(reply.events.is_empty());
        let warning: Text = decode_only(&reply.packets[0]);
        assert!(
            warning.message.contains("at most 512"),
            "{}",
            warning.message
        );

        // Other text types from a client are ignored.
        let tip = Text {
            text_type: TextType::Tip,
            ..Text::raw("hi")
        };
        let reply = session.handle(&tip.encode()).unwrap();
        assert!(reply.events.is_empty() && reply.packets.is_empty());
    }
}
