//! A client's protocol session, from its first packet through login.
//!
//! [`Handshake`] is a sans-IO state machine over decoded packets; [`run`] drives
//! it over a NetherNet [`Connection`]. The flow follows gophertunnel's server
//! for NetherNet, where there is no ServerToClientHandshake because DTLS already
//! encrypts the connection:
//!
//! 1. RequestNetworkSettings → NetworkSettings, then compression starts.
//! 2. Login → PlayStatus(LoginSuccess) + ResourcePacksInfo.
//! 3. ResourcePackClientResponse(downloading finished) → ResourcePackStack.
//! 4. ResourcePackClientResponse(stack finished) → the game starts, which is not
//!    implemented yet, so the client is disconnected with a message instead.

use std::time::Duration;

use bytes::Bytes;
use mistvale_net::{ClientIdentity, Connection, Reliability};
use mistvale_protocol::batch::{self, BatchError, Compression, CompressionAlgorithm};
use mistvale_protocol::io::DecodeError;
use mistvale_protocol::login::{ConnectionRequest, LoginError};
use mistvale_protocol::packet::{self, Encode as _, id};
use mistvale_protocol::packets::{
    Disconnect, DisconnectMessage, DisconnectReason, EXEMPTED_PACKS, Login, NetworkSettings,
    PackResponse, PlayStatus, PlayStatusCode, RequestNetworkSettings, ResourcePackClientResponse,
    ResourcePackStack, ResourcePacksInfo, StackPack,
};
use mistvale_protocol::{GAME_VERSION, PROTOCOL_VERSION};

/// Compression the server asks clients to use.
const COMPRESSION: Compression = Compression {
    algorithm: CompressionAlgorithm::Flate,
    threshold: 256,
};

/// How long to wait for the client to hang up after we disconnect it, so the
/// Disconnect packet is delivered before the session is torn down.
const DISCONNECT_LINGER: Duration = Duration::from_secs(5);

const GAME_NOT_IMPLEMENTED: &str =
    "Mistvale BDS: login complete! Joining the world is not implemented yet.";

/// Serves one client until either side closes the connection.
pub async fn run(mut connection: Connection) {
    let network_id = connection.network_id();
    let mut handshake = Handshake::new(connection.client_identity().cloned());
    let mut compression = None;

    while let Some(message) = connection.recv().await {
        let replies = match batch::decode(&message.payload, compression.is_some()) {
            Ok(packets) => packets
                .iter()
                .map(|packet| {
                    handshake
                        .handle(packet)
                        .unwrap_or_else(|err| err.into_reply())
                })
                .collect(),
            Err(err) => vec![HandshakeError::from(err).into_reply()],
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

/// Where the login handshake is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    RequestNetworkSettings,
    Login,
    ResourcePacks,
    Complete,
}

/// Why the handshake failed; each error disconnects the client.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("malformed batch: {0}")]
    Batch(#[from] BatchError),
    #[error("malformed packet: {0}")]
    Decode(#[from] DecodeError),
    #[error("malformed login: {0}")]
    Login(#[from] LoginError),
    #[error("unexpected packet {id} while waiting for {stage:?}")]
    UnexpectedPacket { id: u32, stage: Stage },
}

impl HandshakeError {
    fn into_reply(self) -> Reply {
        tracing::debug!(err = %self, "handshake failed");
        let reason = match self {
            Self::UnexpectedPacket { .. } => DisconnectReason::UNEXPECTED_PACKET,
            _ => DisconnectReason::BAD_PACKET,
        };
        Reply::disconnect(reason, format!("Mistvale BDS: {self}"))
    }
}

/// The login handshake, one decoded packet at a time.
#[derive(Debug)]
pub struct Handshake {
    stage: Stage,
    /// The identity the client proved during NetherNet signaling, if any.
    identity: Option<ClientIdentity>,
}

impl Handshake {
    pub fn new(identity: Option<ClientIdentity>) -> Self {
        Self {
            stage: Stage::RequestNetworkSettings,
            identity,
        }
    }

    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// Handles one encoded packet (header and payload).
    pub fn handle(&mut self, packet: &[u8]) -> Result<Reply, HandshakeError> {
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
            (stage, id) => Err(HandshakeError::UnexpectedPacket { id, stage }),
        }
    }

    fn request_network_settings(
        &mut self,
        request: RequestNetworkSettings,
    ) -> Result<Reply, HandshakeError> {
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

    fn login(&mut self, login: Login) -> Result<Reply, HandshakeError> {
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
    ) -> Result<Reply, HandshakeError> {
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
                self.stage = Stage::Complete;
                Ok(Reply::disconnect(
                    DisconnectReason::DISCONNECTED,
                    GAME_NOT_IMPLEMENTED,
                ))
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

    #[test]
    fn walks_the_whole_login_handshake() {
        let (identity, token) = client();
        let mut handshake = Handshake::new(Some(identity));

        let reply = handshake
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

        let reply = handshake.handle(&login_packet(token)).unwrap();
        assert_eq!(ids(&reply), [id::PLAY_STATUS, id::RESOURCE_PACKS_INFO]);
        let status: PlayStatus = decode_only(&reply.packets[0]);
        assert_eq!(status.status, PlayStatusCode::LoginSuccess);
        assert_eq!(handshake.stage(), Stage::ResourcePacks);

        // Clients send their blob cache support at some point; it is ignored.
        let cache = [0x81, 0x01, 0x00];
        assert!(handshake.handle(&cache).unwrap().packets.is_empty());

        let reply = handshake
            .handle(&pack_response(PackResponse::DownloadingFinished))
            .unwrap();
        assert_eq!(ids(&reply), [id::RESOURCE_PACK_STACK]);
        assert!(!reply.close);

        let reply = handshake
            .handle(&pack_response(PackResponse::StackFinished))
            .unwrap();
        assert_eq!(ids(&reply), [id::DISCONNECT]);
        assert!(reply.close);
        let disconnect: Disconnect = decode_only(&reply.packets[0]);
        assert_eq!(disconnect.message.unwrap().message, GAME_NOT_IMPLEMENTED);
        assert_eq!(handshake.stage(), Stage::Complete);
    }

    #[test]
    fn rejects_other_protocol_versions_before_compression() {
        for (client_protocol, expected) in [
            (PROTOCOL_VERSION - 1, PlayStatusCode::LoginFailedClient),
            (PROTOCOL_VERSION + 1, PlayStatusCode::LoginFailedServer),
        ] {
            let mut handshake = Handshake::new(None);
            let reply = handshake
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
        let mut handshake = Handshake::new(Some(identity));
        handshake
            .handle(
                &RequestNetworkSettings {
                    client_protocol: PROTOCOL_VERSION,
                }
                .encode(),
            )
            .unwrap();

        let reply = handshake.handle(&login_packet(other_token)).unwrap();
        assert!(reply.close);
        let disconnect: Disconnect = decode_only(&reply.packets[0]);
        assert_eq!(disconnect.reason, DisconnectReason::NOT_AUTHENTICATED);
        assert_eq!(handshake.stage(), Stage::Login);
    }

    #[test]
    fn out_of_order_packets_disconnect_with_a_reason() {
        let (_, token) = client();
        let mut handshake = Handshake::new(None);
        let err = handshake.handle(&login_packet(token)).unwrap_err();
        assert!(matches!(
            err,
            HandshakeError::UnexpectedPacket {
                id: id::LOGIN,
                stage: Stage::RequestNetworkSettings
            }
        ));

        let reply = err.into_reply();
        assert!(reply.close);
        let disconnect: Disconnect = decode_only(&reply.packets[0]);
        assert_eq!(disconnect.reason, DisconnectReason::UNEXPECTED_PACKET);
    }
}
