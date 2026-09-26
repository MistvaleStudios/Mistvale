# Mistvale BDS — Architecture

Living design document. Plan approved 2026-09-25; research is current as of that date.

| | |
|---|---|
| Target | Minecraft Bedrock Edition **26.51**, network protocol **2193** |
| Transport | **NetherNet only** (WebRTC). RakNet is not implemented. |
| Language | Rust, edition 2024, MSRV 1.93, safe Rust (`unsafe_code = "forbid"`), tokio |
| Plugins | Zero-build, hot-reloaded scripts: Luau (`mlua`), JS/TS (`deno_core`), Python (RustPython) |

Contents: [1 Goals](#1-goals-and-constraints) ·
[2 Research findings](#2-research-findings) ·
[3 NetherNet wire reference](#3-nethernet-wire-reference) ·
[4 Architecture](#4-architecture) ·
[5 Approved decisions](#5-approved-decisions-2026-09-25) ·
[6 Build plan](#6-build-plan) ·
[7 Risks](#7-risks-and-open-questions) ·
[8 Sources](#8-sources)

---

## 1. Goals and constraints

- A high-performance custom Bedrock dedicated server written in Rust.
- Speak Bedrock 26.51 / protocol 2193 (released 2026-09-15 on Windows and PlayStation,
  2026-09-16 elsewhere; server version 1.26.51.1).
- NetherNet only. BDS has defaulted to `transport=nethernet` since 1.26.50, and 26.60
  (in preview) removes RakNet entirely.
- Plugins are plain text files in `plugins/`, loaded and hot-reloaded with no build step.
- Safe Rust: our crates forbid `unsafe`. FFI stays inside `mlua` and `rusty_v8`.

## 2. Research findings

### 2.1 Confirmed from the original brief

- 26.51 is protocol 2193.
- Signaling runs over HTTP on the server's TCP port (default 19132), under `/v1/join`.
- Game traffic runs over UDP on the `ReliableDataChannel` and `UnreliableDataChannel`
  WebRTC data channels.

### 2.2 Corrections to the original brief

1. **Exact endpoints.** `GET /v1/join` is both the capability probe and the server-list
   status. `POST /v1/join/{networkId}` exchanges the SDP offer and answer
   ([§3.1](#31-signaling-http-on-the-tcp-server-port)).
2. **A mandatory `a=identity` (RFC 8827) in the SDP answer.** The brief does not mention
   it, and the client rejects answers without it ([§3.2](#32-identity-assertion-rfc-8827)).
3. **Segmentation is not a fixed "> 10,000 bytes" rule.** Every message carries a 1-byte
   countdown, and the segment payload is the negotiated `max-message-size` − 1
   ([§3.4](#34-data-channel-framing-segmentation)). The 10,000 figure comes from
   `df-mc/nethernet-spec`, which was reverse-engineered from 1.20.50. It survives only as
   a stale doc comment in go-nethernet.
4. **Framing above the transport.** There is no `0xFE` batch header and no app-level
   encryption. Compression and batching still apply ([§3.5](#35-game-packet-framing)).

DeepWiki's AI-generated page on Mojang's docs describes a uint32 fragment header. That
contradicts both Mojang's guide and go-nethernet's code, so it is disregarded.

### 2.3 Reference projects

- **WaterdogPE** (Java proxy)
  - Runs RakNet and NetherNet side by side, using a bundled libdatachannel.
  - Signaling modes: `builtin`, `nxs` (external provider), `hybrid` and `plugin`.
  - Signaling defaults to the listener port. Media uses `udp_port`: 0 means an ephemeral
    port per peer, and a fixed port means ICE UDP muxing.
  - `server_type: bedrock` probes a backend's `GET /v1/join` to pick the transport.
  - Reverse proxies must forward both routes.
  - The P-384 identity key (`identity_file`) must be shared across proxy instances.
  - *Takeaways:* keep signaling behind a trait, and make the identity key portable.
- **Pumpkin** (Rust)
  - Merged 26.51 support on 2026-09-17 (PR #3472).
  - NetherNet uses the `webrtc` crate plus axum, with a P-384 identity and hashed Bedrock
    block IDs.
  - Uses tokio for I/O and rayon for CPU-bound work. Plugins run via wasmtime and native
    libraries.
  - Explored LAN discovery (UDP 7551) in PR #2825, then dropped it on 2026-09-22 in favor
    of HTTP signaling.
- **Dragonfly** (Go)
  - Each world is owned by a single goroutine, and every mutation runs in a transaction
    (`world.Tx`). Code running elsewhere schedules closures with `World.Do`, so the hot
    path needs no locks.
  - `Loader`/`Viewer` stream chunks to players.
  - Sub-chunks are paletted storages with bits per block ∈ {0,1,2,3,4,5,6,8,16}.
  - Network block IDs are FNV-1a-32 hashes of the block's `{name, states}` NBT, with
    `UseBlockNetworkIDHashes` set in StartGame.
  - Worlds persist to LevelDB (`mcdb`).
  - NetherNet support (PR #1290) is built on go-nethernet:
    - Its UDP mux defaults to 19133, because pion fails with `EADDRINUSE` next to RakNet
      on 0.0.0.0:19132.
    - Its built-in signaling is plaintext HTTP, with TLS left to a reverse proxy.
- **go-nethernet / gophertunnel** (Go): the reference implementation of the wire details
  in §3.
- **Mojang/bedrock-protocol-docs**
  - Official packet, type and enum JSON schemas for each release (tag `v1.26.51` = 2193),
    plus the NetherNet onboarding guide.
  - The schemas are machine-readable: `x-underlying-type`, `x-ordinal-index`,
    `x-serialization-options`, and packet IDs in `$metaProperties["[cereal:packet]"]`.
  - Licensed "All rights reserved" under the Minecraft EULA. **Use as a reference only;
    never vendor it into this repository.**

## 3. NetherNet wire reference

### 3.1 Signaling (HTTP on the TCP server port)

Clients request the signaling endpoint on exactly the host and port the player entered,
with no fallback ports. They try HTTPS first, then plain HTTP.

**`GET /v1/join`** is the capability probe and the server-list status.

- Any 2xx means NetherNet is supported. Any other status makes the client abort.
- The response is `Content-Type: application/json`.
- `gameType` values: 0 Survival, 1 Creative, 2 Adventure.

```json
{
  "name": "Dedicated Server",
  "protocol": 2193,
  "version": "1.26.51",
  "level": "Bedrock level",
  "players": 0,
  "maxPlayers": 10,
  "gameType": 0
}
```

**`POST /v1/join/{networkId}`** exchanges the SDP offer and answer.

- `networkId` is an opaque client identifier, currently a decimal u64.
- The request body is the SDP offer (`application/sdp`, UTF-8) with every ICE candidate
  inline.
- A 2xx response carries the SDP answer (`application/sdp`) with every candidate inline.
- The client sends exactly one request per attempt and never retries. Any non-2xx
  response ends the attempt.

Mistvale mirrors go-nethernet's handler:

| Condition | Response |
|---|---|
| Missing or invalid `networkId`, empty body | 400 |
| Offer larger than 1 MiB | 413 |
| Offer not admitted | 503 |
| Negotiation took longer than 15 s | 502 |

Error bodies are `text/plain`.

### 3.2 Identity assertion (RFC 8827)

**Attribute format**

- Both the offer and the answer carry a session-level `a=identity:<base64(JSON)>`
  (standard, padded base64), placed before the first `m=` line.
- The decoded JSON is `{"assertion":"<string>","idp":{"domain":"<domain>","protocol":"default"}}`.
  The `assertion` value is itself JSON encoded as a string (double-encoded):
  `{"fingerprints":"<detached JWS>","token":"<JWT>"}`.
- `fingerprints` is a detached JWS (RFC 7515 Appendix F) in the compact form
  `base64url(header)..base64url(signature)`.
- The JWS covers the canonical JSON of the SDP's `a=fingerprint` lines:
  `{"fingerprint":[{"algorithm":"sha-256","digest":"AA:BB:..."}]}`.
  Canonical means sorted keys, no insignificant whitespace and minimal escaping
  (a subset of RFC 8785).

**Client offer.** The `token` is the player's GameServerToken from the Minecraft auth
service. It is RS256-signed (per go-nethernet), carries the XUID, UUID and PlayFabId, and
includes a `cpk` claim with the client's P-384 public key. 26.40+ clients send `cpk` as a
JWK object; older clients sent a base64 SPKI DER string. `idp.domain` names the issuer,
observed from a 1.26.51 client as `https://authorization.franchise.minecraft-services.net/`
(with the trailing slash). The server:

1. Validates the JWT signature against the auth service's keys.
2. Rebuilds the fingerprint JSON from the offer.
3. Verifies the JWS with `cpk`.
4. Authorizes the player.
5. **Strips `a=identity` before handing the SDP to WebRTC.** Unknown attributes cause the
   SDP to be rejected.

Over HTTP, any non-2xx response rejects the player. go-nethernet uses
`ErrorCodeIdentityNotAllowed` (37) on its other signaling transports.

**Server answer**

- The `token` is a self-signed ES384 JWT. Mistvale shapes it like BDS does (per
  go-nethernet):
  - header `{"alg":"ES384","x5u":"<base64 SPKI DER public key>"}`
  - claims `cpk` (our public key as a JWK, RFC 7517), `iat`, and `exp` = `iat` + 60 s
- Mojang's guide also recommends `iss`, which the trust prompt would display. BDS omits it.
- `idp.domain` is `"self"`, as on BDS.
- `fingerprints` is a detached JWS (`{"alg":"ES384"}` header) over the answer's
  fingerprints, signed with the same key.
- **The answer must always include `a=identity`.**

**How the client verifies our answer**

1. It checks the JWT self-signature using `cpk`, then the fingerprint JWS, then `exp`.
2. It applies a trust anchor:
   - **HTTPS:** accepted silently, because TLS authenticates the endpoint.
   - **Plain HTTP:** trust on first use (TOFU). A pinned key is accepted. An unknown key
     triggers a prompt that shows the key fingerprint, and an accepted key is pinned.
3. **Changing our key re-prompts every player.**

### 3.3 WebRTC session

**Transport settings**

- Data channels only, with no audio or video.
- UDP only (TCP candidates disabled), `max-bundle`, and no trickle ICE.
- No STUN/TURN by default, so only host candidates are exchanged. The server must
  therefore put a client-reachable address in its answer's candidates, via a configurable
  advertise address for NAT or a public IP.
- ICE roles: the client is controlling and the server is controlled.
- **Mistvale runs ICE-lite by default.** The answer carries a session-level `a=ice-lite`,
  and the server never probes candidate pairs; it only answers the client's checks.
  Every datagram therefore leaves through the socket that received the client's
  traffic. Under full ICE, the server's own checks also tried pairs Windows cannot route,
  e.g. from a VirtualBox host-only adapter to the LAN (error 10051); those failures were
  harmless but noisy. `ListenerConfig::ice_lite` (`MISTVALE_ICE_LITE=false`) switches
  back to full ICE.
- str0m's ICE-lite agent accepts only host candidates, so advertised public addresses
  are announced as host candidates in lite mode (standard for 1:1 NAT).

**SDP**

- Media line: `m=application 9 UDP/DTLS/SCTP webrtc-datachannel`.
- Attributes: `a=sctp-port:5000` and `a=max-message-size:262144`.
- The offer uses `a=setup:actpass`. Mojang's example answer uses `a=setup:active`; the
  guide allows either active or passive.

**Data channels.** The client creates both channels, and the server receives them
(`ondatachannel`):

| Label | Ordered | Reliable | maxRetransmits |
|---|---|---|---|
| `ReliableDataChannel` | yes | yes | default |
| `UnreliableDataChannel` | no | no | 0 |

**Connection sequence**

1. The client gathers all its candidates.
2. The client sends `POST /v1/join/{networkId}` with its offer.
3. The server sets the remote description, creates an answer and gathers its candidates.
4. The server responds 200 with the answer.
5. ICE connectivity checks run.
6. DTLS completes. The certificate must match `a=fingerprint`.
7. The SCTP association is established.
8. The data channels open.

Answer layout written by `mistvale_net::sdp`. It mirrors go-nethernet's encoder, which
works with vanilla clients:
- `a=identity` is session-level, while `a=fingerprint` is media-level.
- `a=ice-options:trickle` is always present.
- There is no `a=end-of-candidates`.
- Candidates use libwebrtc's extension format.

```text
v=0
o=- <session-id> 2 IN IP4 127.0.0.1
s=-
t=0 0
a=ice-lite                                (unless full ICE is configured)
a=group:BUNDLE <mid>
a=extmap-allow-mixed
a=msid-semantic: WMS
a=identity:<base64 identity JSON>
m=application <default port> UDP/DTLS/SCTP webrtc-datachannel
c=IN IP4 <default address>
a=candidate:<foundation> 1 udp <priority> <ip> <port> typ host generation 0 ufrag <ufrag> network-id 0 network-cost 0
a=ice-ufrag:<ufrag>
a=ice-pwd:<pwd>
a=ice-options:trickle
a=fingerprint:sha-256 <OUR:DTLS:CERT:DIGEST>
a=setup:active
a=mid:<mid, echoed from the offer>
a=sctp-port:5000
a=max-message-size:262144
```

### 3.4 Data-channel framing (segmentation)

- Every message on both channels is `[u8 remaining][payload]`.
- `remaining` is the number of segments still to follow. **0 means a complete message or
  the final segment.** A 3-segment message is `[0x02]…`, `[0x01]…`, `[0x00]…`. The
  receiver accumulates payloads until it sees 0.
- **Segment payload = negotiated SCTP `max-message-size` − 1**, which is 262,143 for
  256 KiB. go-nethernet reads the size from the remote SDP. Sending smaller segments is
  always valid.
- Only the reliable channel fragments. On the unreliable channel the header is always
  `0x00`, and oversize messages are dropped.
- The u8 header allows at most 256 segments. go-nethernet refuses to send more than 255.
- **Mistvale's current limits (str0m `DirectApi`):**
  - We send segments of min(client `max-message-size`, 64 KiB) − 1 = 65,535 bytes, because
    str0m's direct API cannot pass it the client's advertised size.
  - We receive SCTP messages up to the 256 KiB we advertise.
  - str0m buffers at most 128 KiB across streams, so outgoing segments wait in a
    per-session queue, and `Connection::send` applies backpressure beyond 4 MiB.
- The Mistvale receiver adds two protections:
  - It enforces a strict countdown: a restart mid-message or a skipped value is a protocol
    error, and the connection closes.
  - It caps the reassembled size as a DoS guard.

### 3.5 Game packet framing

**What NetherNet removes**

- No `0xFE` batch header: go-nethernet's `BatchHeader()` returns nil.
- No app-level encryption: DTLS already encrypts, so go-nethernet's `DisableEncryption()`
  returns true. There is no ServerToClientHandshake and no AES layer.

**What still applies.** Compression and batching are independent of the transport:

1. The first message is `RequestNetworkSettings`, uncompressed.
   `ClientNetworkVersion` is an int32, big-endian.
2. The server replies with `NetworkSettings` (packet ID 143). Its fields, in order:
   - compression threshold, u16 (0 = disabled, 1 = compress everything)
   - compression algorithm, u16
   - client throttle enabled, bool
   - client throttle threshold, u8
   - client throttle scalar, f32
3. After that, every message is `[algorithm u8][(varuint32 length, packet)…]`, and the
   tail is compressed:
   - `0x00`: zlib (raw DEFLATE)
   - `0x01`: snappy
   - `0xFF`: none
4. gophertunnel caps a batch at 812 packets.

The first client message is exactly `06 | C1 01 | 00 00 08 91`: a one-byte batch length
(6), packet ID 193 as a varuint32, then protocol 2193 as a big-endian int32.

**Login flow over NetherNet** (gophertunnel's server; implemented in
`mistvale_core::session`). There is no ServerToClientHandshake step.

| Client sends | Server replies |
|---|---|
| RequestNetworkSettings (193) | NetworkSettings (143): raw DEFLATE, threshold 256; compression starts for both sides. On a protocol mismatch, PlayStatus `LoginFailedClient`/`LoginFailedServer` instead. |
| Login (1) | PlayStatus `LoginSuccess` (2) + ResourcePacksInfo (6), with no packs |
| ClientCacheStatus (129) | nothing; the blob cache is not supported |
| ResourcePackClientResponse (8) `downloadingfinished` | ResourcePackStack (7): gophertunnel's 8 exempted vanilla packs, base game version `1.26.51` |
| ResourcePackClientResponse `resourcepackstackfinished` | StartGame is next. It is **not implemented**, so Mistvale sends Disconnect (5) with a message. |

- **Packet layouts at protocol 2193:**
  - `ResourcePackClientResponse` is a varuint32 tag (0 cancel, 1 downloading,
    2 downloadingfinished, 3 resourcepackstackfinished) followed by the tag's name as a
    string.
  - `Disconnect` is a varint32 reason, a "hide screen" bool, then message and filtered
    message.
  - Lists use varuint32 counts, except the experiments list, which uses a u32.
- **The Login connection request** holds two blobs, each prefixed with a u32 LE length:
  - auth JSON `{AuthenticationType, Certificate: "{\"chain\":[…]}", Token}`
  - the client-data JWT

  The multiplayer `Token` carries `xid` (XUID), `xname` (gamertag) and `cpk`
  (base64 SPKI DER).

**Security consequence.** Without app-level encryption, a captured Login packet could be
replayed; go-nethernet warns about this itself. Mistvale therefore requires the Login
token's `cpk` to equal the key proven by the SDP `a=identity`, as gophertunnel does.
Otherwise it sends Disconnect `NotAuthenticated`.

**Player authentication chain** (gophertunnel `service` package):

1. Minecraft services discovery.
2. The auth service environment (`ServiceURI`, `Issuer`).
3. The OpenID configuration and JWKS.
4. Verify the multiplayer token.

The exact URLs will be confirmed when auth is implemented.

### 3.5.1 Spawn sequence

Implemented in `mistvale_core::session`, following gophertunnel's server with
Dragonfly's values.

| Client sends | Server replies |
|---|---|
| ResourcePackClientResponse `resourcepackstackfinished` | JigsawStructureData (313; NBT with empty `processors`, `template_pools`, `jigsaws`, `structure_sets` lists), VoxelShapes (337, empty), StartGame (11), ItemRegistry (162, empty) |
| RequestChunkRadius (69) | ChunkRadiusUpdated (70, capped at 8), NetworkChunkPublisherUpdate (121: spawn and radius × 16 blocks), every LevelChunk (58) in the circle nearest-first, then on the first request PlayStatus `PlayerSpawn` + CreativeContent (145, empty) |
| SetLocalPlayerAsInitialized (113) | nothing; the player is in the world |
| anything else once in the world, e.g. PlayerAuthInput (144) every tick | ignored for now |

- **StartGame** has 81 fields. The encoder was checked line by line against
  gophertunnel's 2193 marshal. The quirks:
  - `BlockPos` is three zigzag varints.
  - UUIDs are two u64 LE halves, the most significant first.
  - Optionals are a presence bool followed by the value.
  - `PropertyData` is network NBT.
  - `PlayerPosition` is the eye position (feet + 1.62).
- **Values:**
  - Entity IDs are 1.
  - Creative mode, peaceful, the flat generator (2), noon.
  - The `showcoordinates` game rule is on.
  - Server-authoritative inventory and block breaking, as Dragonfly sends.
  - `use_block_network_id_hashes = true`.
- **Block network IDs** are FNV-1a-32 of little-endian NBT `{"name", "states"}`, with
  states sorted by name and no version field (Dragonfly's `network_block_hash.go`). The
  client hashes its own states, so no block palette is sent.
- **LevelChunk:** chunk x/z and dimension as varints; the sub-chunk count as a varuint32
  (at most 64); an optional sub-chunk limit, absent; a cache bool, false; empty blob
  hashes; then the payload.
- **The payload** is:
  - the sub-chunks, from the bottom up
  - one biome storage per sub-chunk of the dimension (24 for the overworld), where
    `0xFF` repeats the previous one
  - a zero border-block byte
  - no block entities
- **Sub-chunks** are `[9][layer count][y index]` followed by paletted layers. Each layer
  is:
  - a header of `bits << 1 | 1`
  - little-endian u32 words, packing `32 / bits` indices LSB-first in x→z→y order
    (bits ∈ {0, 1, 2, 3, 4, 5, 6, 8, 16})
  - a palette of zigzag varints; 0-bit layers omit the palette size
- **Deliberately not sent:**
  - BiomeDefinitionList. Mojang describes it as the list of *all available biomes*, so
    an empty one could remove plains, which the chunks use. gophertunnel's minimal
    server omits it too.
  - AvailableActorIdentifiers.
  - Inventories, attributes and entity metadata.
- The old ItemComponent packet does not exist in 2193; ItemRegistry (162) replaced it.

### 3.6 Ports and discovery

| Port | Protocol | Use |
|---|---|---|
| 19132 (`server-port`) | TCP | HTTP(S) signaling |
| 19133 (default, configurable) | UDP | WebRTC media for all peers, on one muxed socket |
| 7551 | UDP | LAN discovery. BDS binds it; not planned for Mistvale. |

Xbox Live and Realms use WebSocket signaling via `signal.franchise.minecraft-services.net`.
That is out of scope.

### 3.7 Version timeline

| Release | Protocol | Date |
|---|---|---|
| 1.26.50 | 2193 | 2026-09-15 |
| 1.26.51 (hotfix) | 2193 | 2026-09-15/16 |
| 1.26.60 preview.21 → preview.28 | 2207 → 2216 | 2026-09-02 → 2026-09-22 |

26.60 removes RakNet.

## 4. Architecture

### 4.1 Workspace layout

```text
D:/Mistvale/
├── Cargo.toml              # workspace: resolver 3, edition 2024, MSRV 1.93, shared deps + lints
├── docs/ARCHITECTURE.md    # this document
├── keys/                   # runtime: identity.pem (auto-generated, git-ignored)
├── plugins/                # hot-reloaded *.luau / *.ts / *.js / *.py
└── crates/
    ├── mistvale_protocol/  # varints, NBT, batch codec, packets @ 2193
    ├── mistvale_net/       # signaling + WebRTC + segmentation → byte messages
    ├── mistvale_plugins/   # engine trait, Luau/JS/Python hosts, hot reload
    └── mistvale_core/      # 20 TPS loop, world, entities; binary `mistvale`
```

### 4.2 Dependency graph

```text
                 mistvale_core  (lib + bin `mistvale`)
                /       |        \
   mistvale_net  mistvale_protocol  mistvale_plugins
```

- The three leaf crates don't depend on each other, so each one builds and tests in
  isolation.
- `mistvale_net` only moves bytes and knows nothing about Minecraft packets.

### 4.3 `mistvale_net`

| Module | Responsibility |
|---|---|
| `signaling` | axum routes for `GET /v1/join` (status JSON from a `watch` channel) and `POST /v1/join/{networkId}`, with the limits and status codes in §3.1. Offers go through the `OfferHandler` trait so a proxy- or NXS-style provider can plug in. Serving TLS on the same port (peeking the first byte, `0x16`) is not implemented yet. |
| `sdp` | Parses the data-channel-only offer (`a=identity` is read separately and never reaches str0m) and writes answers in the §3.3 layout. |
| `identity` | Loads or creates the persistent P-384 key (`keys/identity.pem`, PKCS#8 PEM, 0600 on Unix) and signs answers (§3.2). For clients, verifies that the token's `cpk` key signed the offer's fingerprints; the token itself is **not** verified yet (§7). |
| `mux` | One UDP socket per local address (default port 19133). Datagrams are routed to sessions by the STUN `USERNAME` ufrag, then by learned (local, remote) paths. Sends always leave through the socket bound to str0m's chosen source address. Windows `ConnectionReset` receive errors are ignored, and unroutable-pair send errors are logged only at trace level. |
| `peer` | One tokio task per session driving str0m through `DirectApi`: ICE controlled, DTLS active, SCTP client, the client's channels matched by label. Handles segmentation, backpressure, a connect timeout, a 10 s ICE-disconnect grace and graceful close. |
| `listener` | `Listener` / `ListenerConfig`. Discovers interfaces, generates one DTLS certificate at startup, answers offers and yields a `Connection` once the reliable channel opens. |
| `segment` | Pure, I/O-free `split` / `Reassembler` (§3.4), with unit tests. |

API: `Listener::bind(config, status)` → `accept()` → a `Connection` with
`recv() -> Option<Message>`, `send(Bytes, Reliability)` and `client_identity()`.
`Listener::update_status` changes what `GET /v1/join` reports.

The `mistvale` binary starts a listener with the defaults. These environment variables
override them:
- `MISTVALE_SIGNALING_ADDR`
- `MISTVALE_MEDIA_PORT`
- `MISTVALE_MEDIA_IPS`
- `MISTVALE_ADVERTISE_IPS`
- `MISTVALE_ICE_LITE` (`false` switches to full ICE)

For local testing, bind to `127.0.0.1` to avoid the Windows Firewall prompt.

Tests: 26 unit tests plus `tests/loopback.rs`, an end-to-end session against a str0m client
using its standard SDP API. That run covers signaling, identity in both directions, ICE,
DTLS, SCTP, and multi-segment messages both ways.

### 4.4 `mistvale_protocol`

- **Primitives:** varint/zigzag, little-endian numerics, varuint32-prefixed strings.
- **NBT:** written in-house, because existing crates target Java's big-endian format.
  Supports Bedrock's little-endian (disk) and network (varint) flavors, serde-based.
- **Batch codec:** as described in §3.5.
- **Packets:** hand-written for the login path first. Later, an optional `xtask codegen`
  can read a developer's local copy of Mojang's schemas (never committed).
- **Versioning:** `PROTOCOL_VERSION = 2193`, structured so 26.60 (2216+) can be added
  alongside it.
- **Implemented:**

  | Module | Contents |
  |---|---|
  | `io` | `Reader` / `Writer` primitives |
  | `batch` | framing; raw DEFLATE / Snappy / none; a 16 MiB decompression cap and 812 packets per batch |
  | `packet` | the varuint32 header with sub-client bits; `Packet` / `Encode` / `Decode` traits; IDs checked against Mojang's schemas |
  | `packets` | the handshake packets |
  | `login` | connection-request parsing and unverified identity claims, including the persistent UUID: the token's `leguuid`, otherwise vanilla's MD5 (v3) UUID of `pocket-auth-1-xuid:` + XUID; the legacy chain's `extraData.identity` |
  | `block` | `BlockState` and hashed network IDs |
  | `chunk` | `PalettedStorage`, `SubChunk` and the LevelChunk payload builder |
  | `nbt` | encode-only NBT in the network flavor |
  | `types` | `BlockPos` and `Vec3` |
  | `packets::spawn` | StartGame and the §3.5.1 packets |
  | `packets::text` | Text (ID 9): `[bool translate][varuint32 body variant][u8 type]`, then author (chat, whisper, announcement), message (1..=65536 bytes), parameters (translate, popups; ≤ 4), XUID, platform chat ID, optional filtered message. The variant must match the type. |

  | `packets::movement` | PlayerAuthInput, decoded up to the position delta, with the rest skipped; input flags kept as raw IDs, since Mojang's enum and gophertunnel's number them differently. MovePlayer, without teleports. |
  | `packets::entity` | AddPlayer, RemoveActor, PlayerList (a per-entry varuint32 variant plus an action byte), entity metadata, and a classic `Skin` |
  | `packets::block` | PlayerAction, PlayerAuthInput `BlockAction`s, UpdateBlock, and the action numbers (StartBreak 0, CreativeDestroyBlock 13, PredictDestroyBlock 26), confirmed by PocketMine. PlayerAuthInput now reads on to its block actions: it steps over an item interaction, gives up at an item stack request, and a tail it cannot parse only leaves the block actions unread. |
  | `types` (additions) | `Vec2`, and `uuid_bytes`: Bedrock's UUID order, two little-endian u64 halves |

  51 unit tests. They include a golden decode of a real client's first message, and FNV-1a
  checked against the reference vectors plus golden hash-input bytes. NBT decoding is not
  started yet.

### 4.5 `mistvale_core`

- **Game loop (first cut implemented):** `tick::TickLoop` runs `Server::tick` on a
  dedicated OS thread (`game-loop`) with a fixed 50 ms step, not a tokio task, to avoid
  scheduler jitter. The tokio runtime owns I/O.
  - Ticks follow a schedule rather than sleeping 50 ms after each one, so a slow tick is
    made up by quicker ones.
  - A tick over 50 ms logs a warning. More than 1 s behind, the loop skips the missed
    ticks instead of racing to catch up.
  - Dropping the `TickLoop` stops it after the current tick.
  - So far a tick only broadcasts movement. Sessions still write player state into
    `Players` behind a mutex rather than through channels; that changes once the
    simulation owns real state.
- **Tick phases:** drain inbound → simulate → dispatch plugin events → flush one batch per
  player. This gives natural batching and better compression.
- **World:** a single owner, as in Dragonfly, with a chunk map per dimension.
  - Sub-chunks use Bedrock's paletted storage.
  - Network block IDs are hashed, with `UseBlockNetworkIDHashes` set.
  - Chunks generate on a rayon pool, and results merge at the start of a tick.
  - Each player has a chunk streaming radius.
- **Metrics:** MSPT metrics, and bounded catch-up after tick overruns.
- **Entities:** an ECS. `bevy_ecs` standalone is the leading candidate; the choice is made
  when implementing core.
- **Sessions (implemented):** `session::Session` is a sans-IO state machine for the §3.5
  login and the §3.5.1 spawn. Its stages are RequestNetworkSettings → Login →
  ResourcePacks → Spawning → Initializing → InGame. It returns replies to send,
  compression to enable afterwards, and whether to close. `session::run` drives it over a
  NetherNet `Connection`.
  - Before the player is in the world, any protocol error ends with a Disconnect **with a
    message**, so the client shows a reason instead of timing out.
  - Once in the world, packets without a handler are ignored.
  - After a Disconnect it waits up to 5 s for the client to hang up.
  - Replies also carry `SessionEvent`s for the rest of the server: `Joined` on the first
    SetLocalPlayerAsInitialized, `Moved` when PlayerAuthInput reports a new position or
    rotation (in game only; non-finite values are ignored), and `Chat(message)`.
  - Each session gets its own entity ID from `Players::allocate_entity_id` (runtime ID =
    unique ID), used in its StartGame and by everyone who sees the player.
- **Players and chat (implemented):** `server::Server` holds the world, the
  `players::Players` registry and the plugin `Dispatcher`. Each session has a bounded
  queue (256 packets) that `run` drains alongside the connection, batching whatever is
  waiting. On `Joined` the player is registered first, then plugins get `player_join`, so
  a greeting reaches the new player too. A `Membership` guard removes the player on any
  exit.
  - Chat: only `TextType::Chat` is accepted, and only in game. The author is always the
    session's player; the packet's source name and XUID are ignored. Control characters
    become spaces, so nobody can fake a second line. Messages over 512 characters are
    refused with a warning to the sender. Everything else goes to everyone as Raw text
    `<name> message`, as Dragonfly does, so no player list is needed.
  - A full player queue drops that player's packet rather than stalling the others.
- **Visibility and movement (implemented):** `Players` keys players by entity ID and
  keeps each one's `Movement`: eye position, pitch, yaw, head yaw, and an on-ground guess
  (no vertical delta).
  - Joining puts everyone in everyone's player list at once, which also gives clients the
    skins before any entity appears.
  - **Entity tracker:** each player also has their client's `View` (centre chunk and
    radius, reported by the session whenever it recentres) and the set of entities their
    client has. Every tick, `Players::tick`:
    - sends AddPlayer (with the current position) to each viewer whose view now contains
      another player's chunk;
    - sends RemoveActor to viewers whose view no longer does;
    - then sends each player who moved since the last tick, as a MovePlayer (normal mode,
      eye position, the server tick), only to viewers who already had them.
    A player is never sent their own entity or movement. Either side moving updates
    visibility: a player flying back towards someone standing still reappears for them.
  - Leaving (the `Membership` drop) sends RemoveActor to those who saw the player, and a
    PlayerList removal to everyone.
  - AddPlayer uses the feet position (eyes − 1.62), as Dragonfly does. Its metadata,
    `players::player_metadata`, is: name; scale 1; a 0.6 × 1.8 bounding box; an
    always-shown name tag (key 81); and the flags HasGravity, HasCollision, Breathing,
    CanClimb, ShowName and AlwaysShowName. Gophertunnel and PocketMine number these flags
    the same way.
  - The same metadata goes to each player about **their own** entity, as a SetActorData
    just before PlayerSpawn. Without HasGravity the client does not pull its own player
    down: the first live test showed players drifting upward after they stopped flying.
  - Also during spawn, each player gets their own **UpdateAttributes** (`minecraft:movement`
    0.1, underwater and lava movement 0.02, health 20) and **UpdateAbilities** (a base
    layer defining all 20 abilities, granting the creative ones, at walk 0.1, fly 0.05 and
    vertical fly 1.0). The client moves its own player with these; without them walking
    was far faster than vanilla. Other players' AddPlayer carries the same base layer
    without granted abilities; its all-abilities mask used to miss bit 19 (vertical fly
    speed).
  - Skins are not forwarded yet: each player appears with a plain 64×64 classic skin,
    coloured from their UUID, on a humanoid geometry the skin defines itself.
- **Chunk streaming (implemented):** each session keeps a `view::ChunkView`: the chunk
  its player stands in, the granted radius (capped at 8), and the chunks its client has.
  - A RequestChunkRadius, or PlayerAuthInput crossing into another chunk (the feet's
    block, divided by 16 and rounded down), recentres the view. The session then sends a
    NetworkChunkPublisherUpdate at the player's block with radius × 16 blocks, followed
    by every chunk in the new circle the client lacks, nearest first.
  - Chunks that fall out of range are forgotten, because the client unloads them, so
    they are sent again on return. A one-chunk step sends only the circle's leading
    edge (about 17 chunks at radius 8).
  - Streaming happens in the session as input arrives, not in the tick loop. Chunks
    come from `FlatWorld`, so no generation cost is involved yet.
- **World (mutable, implemented):** `world::World` is an endless superflat overworld with
  vanilla's default layers (bedrock at y = -64, two dirt, grass at -61) in plains. New
  players spawn at (0, -60, 0), centred on the block.
  - Unchanged columns all share one generated column and its encoded payload.
  - The first change to a column copies it into a map of changed columns behind a
    mutex. Each changed column keeps its sub-chunk storages and a cached payload, which
    is dropped on change and rebuilt when the chunk is next sent. So streaming, and
    players arriving later, see the change.
  - A column sends sub-chunks up to its highest non-air one, filling any gaps with air.
  - `block` and `set_block` are bounded to y = -64..=319; `set_block` reports whether
    anything changed.
- **Block breaking (implemented):**
  - The session accepts breaks from PlayerAuthInput block actions (StartBreak, since
    everyone is in creative, and PredictDestroyBlock) and from a PlayerAction with
    CreativeDestroyBlock. It checks each one first: the block must be within the world's
    height, within 12 blocks of the eyes (creative reach is about 7.5), and in a chunk
    the client has.
  - `Server::break_block` sets the block to air. If that changed anything, it sends an
    UpdateBlock (network flag, layer 0) to every player whose view contains the chunk,
    the breaker included.
  - Breaking is handled as input arrives, not in the tick loop. Nothing is dropped as
    items, and there is no survival break timing or tool check yet.
  - Breaking also sends a LevelEvent 2001 (`DESTROY_BLOCK`, with the broken block's
    network ID) to the chunk's viewers: the breaking particles and sound.
- **Inventory (first cut):** `inventory` gives every player a fixed creative hotbar of
  nine vanilla blocks without block states: stone, grass, dirt, cobblestone, oak planks,
  sand, glass, white wool and bookshelf.
  - The ItemRegistry sent with StartGame lists only these items, with vanilla's item
    network IDs and entry version 2, as in PocketMine's `required_item_list.json`.
  - An InventoryContent for window 0 goes out at spawn. Each stack has a unique stack
    network ID, and each block item carries its block's hashed network ID.
  - Stacks never run out, and inventory moves (item stack requests) are not handled.
- **Block placing (implemented):** placing arrives in InventoryTransaction as a UseItem
  transaction with action ClickBlock; only that part is decoded.
  - The decode fails soft: an unreadable transaction is ignored.
  - The block goes against the clicked face, and must come from a hotbar slot whose item
    matches the held one. The target must be reachable, in a loaded chunk, air, and not
    inside the placing player's own box (a quick check in the session).
  - `Server::place_block` refuses a block that overlaps **any** online player's box.
    `Players::occupies` uses a box 0.6 wide and 1.8 tall (1.5 while sneaking); touching a
    face is fine. A block inside another player traps them, and their client fights it
    with rapid snapping (found live on 2026-09-26).
  - Otherwise it places only into air, checked under the world lock. It then sends
    viewers an UpdateBlock and a LevelSoundEvent `place` (sounds are named by string in
    2193).
  - The client places a block before hearing back, so a refused placement is undone: the
    placer gets an UpdateBlock with what is really there.
- **Swings and sneaking (implemented):** these come from input flags.
  - MissedSwing (bit 39), a break or a placement sends an Animate arm swing to everyone
    who sees the player.
  - StartSneaking and StopSneaking (bits 27 and 28) toggle the Sneaking entity flag and
    a 1.5-block height. The resulting SetActorData goes to viewers and to the player
    themselves, which Mojang's docs say the client expects.
  - AddPlayer carries the current sneaking state.

- **Persistence (implemented):** the world talks to storage only through the
  `storage::WorldStorage` trait. It lists saved chunks, loads and saves a chunk column,
  and loads and saves a player. `World::with_storage` takes any backend. `World::open(dir)`
  uses `BinStorage`, Mistvale's own format; the binary reads the directory from
  `MISTVALE_WORLD_DIR` (default `world`, which is git-ignored). `World::new` stays in
  memory, for tests.
  - **Blocks cross the trait by name**, as a palette of block states (name and states)
    plus 4096 indices per sub-chunk, the way vanilla worlds store them. So a LevelDB
    backend needs no knowledge of network IDs.
  - The world maps its network IDs (state hashes) back to names through a table of every
    block it can hold: air, the superflat layers and the hotbar blocks.
  - A block with no known name is stored as a raw placeholder,
    `mistvale:raw_network_id` with its ID as a state, so nothing is lost.
  - `BinStorage` writes one file per changed column, `chunks/c.<x>.<z>.bin`: the magic
    `MVCH`, a version byte, then zlib data. Format **version 2**, written now, stores per
    sub-chunk a palette of names and states and u16 indices; `storage.rs` documents the
    layout.
  - **Version 1** files (u32 hashes per block) still load, as raw placeholders, and are
    rewritten as version 2 the next time their chunk changes.
  - Writes go to a `.tmp` file that is renamed over the old one, so a crash never leaves
    half a chunk. There are no new dependencies: `flate2` was already in use, and SQLite
    (C) or `sled` (pre-1.0, dormant) were not needed for per-chunk blobs.
  - Opening lists the saved chunks without reading them. A saved chunk is loaded the
    first time it is read, changed or sent. A file that cannot be read is logged, and the
    chunk is generated instead.
  - Changes mark a column dirty. The tick loop saves dirty columns every 100 ticks (5 s),
    and once more when it stops (Ctrl+C). Columns are copied out under the lock and
    written after it is released, and a failed write stays dirty for the next save.
    Killing the process loses at most 5 s of changes.
  - **Players:** `players/<uuid>.json` holds the feet position, pitch, yaw, head yaw and
    whether the player was flying (`flying` defaults to false, so older files still
    load).
  - A player is saved when their session ends, however it ends; everyone online is also
    saved on every world save (5 s) and at shutdown.
  - At login the session loads the file, so StartGame, the first chunks and the entity
    others see all start there. A file that cannot be read, or a position outside the
    world's height, is ignored and the player starts at the spawn.
  - **Flying:** the session follows StartFlying and StopFlying (input bits 42 and 43) and
    answers each with UpdateAbilities, as Mojang's docs say the client expects. The
    abilities sent at spawn and in those answers include the Flying value while the
    player flies, so a player who left in the air stays in the air.
- **Storage format and vanilla worlds (decided 2026-09-26):** keep the `.bin` format for
  the MVP. The trait and the name-based palettes are in place; a LevelDB backend for
  vanilla worlds comes later. The reasoning is in §7.

  Generation beyond superflat and entities are not started yet.

### 4.6 `mistvale_plugins`

- **Threading:** each engine runs on its own OS thread, because the VMs are `!Send`, with
  one isolated VM per plugin. Luau's thread is named `luau-plugins`. A `ScriptEngine`
  trait will be extracted when the second engine arrives, so its shape comes from real
  needs.
- **Messaging only (implemented):** game → plugins for `Event`s, plugins → game for
  `Action`s. Plugins have no direct access to the world.
  - `Dispatcher::dispatch(Event)` queues an event for the plugin thread, which calls
    every handler plugin by plugin in name order. Each handler call gets the full
    execution limit, and a failing handler is logged without stopping the others.
  - Actions go through a bounded channel (1024) that core drains. When it is full,
    `server.broadcast` raises a Lua error instead of blocking.
  - Events so far: `player_join`, with a read-only `{ name, uuid }` table. The UUID
    is the persistent identity; the XUID is never exposed.
  - Luau API: `server.on(event, handler)` (unknown event names are an error) and
    `server.broadcast(message)` (Raw chat to every player; empty messages are an error).
    The `server` table is read-only after sandboxing. Handlers live in the VM's
    registry, so a reload drops the old ones with the old VM.
  - Reload debouncing uses its own deadline, so a steady stream of events cannot
    postpone reloads.
- **Hot reload (implemented):** a `notify` watcher on `plugins/` (non-recursive,
  `*.luau`) debounced by 200 ms. Once a file stops changing, the plugin is reloaded into
  a fresh VM, or unloaded if the file was deleted. A reload that fails logs the error
  and keeps the running version. State-handoff hooks are not built yet.
- **Luau (default, implemented)** in `mistvale_plugins::{host, luau}`. `PluginHost::start`
  returns once every plugin has loaded, and a broken plugin never stops startup.
  Each VM is set up in this order:
  1. A memory limit (64 MiB by default).
  2. `print` and a `log.{trace,debug,info,warn,error}` table, installed before
     sandboxing so scripts cannot replace them. Output goes to `tracing` under the
     `plugin` target through a swappable `Output` sink.
  3. `Lua::sandbox(true)`: read-only libraries and globals, with script writes kept local.
  4. An interrupt that aborts any call running past the execution limit (1 s by default).
- **JS/TS (`js` feature):** a `deno_core` `JsRuntime`. TypeScript is transpiled on load
  with `deno_ast`, so there is still no build step. Pulls a prebuilt V8 of more than
  100 MB.
- **Python (`python` feature):** RustPython 0.5. It is much slower than CPython and its
  stdlib is incomplete.

### 4.7 Cross-cutting

- **Safety:** `unsafe_code = "forbid"` across the workspace. FFI `unsafe` stays inside
  mlua, rusty_v8 and RustPython.
- **Errors:** `thiserror` in libraries, `anyhow` in the binary.
- **Logging:** `tracing`, filtered with `RUST_LOG`.
- **Profiles:** the dev profile optimizes dependencies (opt-level 2), because pure-Rust
  crypto, compression and the Luau VM are very slow unoptimized. Release builds use
  thin LTO with line-table debug info.
- **Configuration:** a `mistvale.toml` file is planned.

## 5. Approved decisions (2026-09-25)

**1. WebRTC engine: str0m 0.23 via `DirectApi`.** `webrtc` 0.21 is the fallback, behind
the same `rtc` interface.
- It is sans-IO, so one UDP socket can serve all peers and tests are deterministic.
- `DirectApi` gives exact control of the SDP and the DTLS role. str0m's SDP path
  hard-codes `passive` for an `actpass` offer, while Mojang's example answer uses `active`.
- Crypto backend: str0m's default `aws-lc-rs` (approved 2026-09-25). We started on
  `rust-crypto`, but it also compiled AWS-LC through dimpl's `rcgen` feature, so it built
  two crypto stacks. AWS-LC builds on the dev machine without CMake or NASM.
- Pumpkin uses `webrtc`, whose new architecture only went stable in July 2026.

**2. Segment size = negotiated `max-message-size` − 1.** This matches Mojang's guide and
go-nethernet, and the receiver accepts any segment size.

**3. `keys/identity.pem` (P-384) is auto-generated on first run and git-ignored.**
- `a=identity` requires it.
- A stable key avoids repeated TOFU prompts.
- HTTPS with a real certificate removes the prompt entirely.

**4. `main.rs` lives in `mistvale_core` (bin `mistvale`).** This keeps exactly four crates.

**5. JS (`js`) and Python (`python`) sit behind cargo features, off by default.**
`deno_core` pulls a prebuilt V8 of more than 100 MB and links slowly. Keeping them off
keeps everyday builds fast while the Luau bridge comes first.

## 6. Build plan

Each step starts only after explicit confirmation.

| Step | Scope | Done when | Status |
|---|---|---|---|
| 1 | Workspace, dependencies, four crate skeletons, git | `cargo check --workspace` passes cleanly | ✅ done 2026-09-25 |
| 2 | `mistvale_net` draft: signaling server, SDP, identity, segmenter/reassembler with tests, str0m peer driver | Compiles, tests pass, and `curl` against `/v1/join` works. A live 26.51 client handshake is the real test and may need iteration. | ✅ done 2026-09-25; a vanilla 1.26.51 client connected over LAN |
| 3 | `mistvale_plugins` Luau bridge: load `plugins/*.luau` into a sandbox and route `print` to the log | The `plugins/hello.luau` smoke test works | ✅ done 2026-09-25 (prints on boot; hot reload verified live) |
| 4 | Protocol handshake: batch codec, NetworkSettings → Login → resource packs; ICE-lite so sends only use paths the client proved | A live client gets past RequestNetworkSettings and receives Mistvale's disconnect message | ✅ done 2026-09-25; a vanilla 1.26.51 client over ICE-lite showed the disconnect message (commit `5aab46c`) |
| 5 | World spawning: StartGame, empty registries, hashed block IDs, flat chunks, PlayerSpawn → SetLocalPlayerAsInitialized | A live client leaves "Building terrain" and stands on grass | ✅ done 2026-09-25; a vanilla 1.26.51 client spawned on the grass at (8, -60, 8) (commit `98908a5`) |
| 6 | Plugins meet the world: `player_join` event, Text packet and chat relay, Luau `server.on` / `server.broadcast`, welcome message in `hello.luau` | A live client sees the welcome message, and chat is echoed | ✅ done 2026-09-25; the yellow welcome appeared for a vanilla 1.26.51 client (commit `59e8247`) |
| 7 | Tick loop and visibility: 20 TPS game loop, PlayerAuthInput decoding, per-player position, rotation and head yaw, PlayerList / AddPlayer / MovePlayer / RemoveActor between players | Two live clients see each other move | ✅ done 2026-09-26; two clients (PC and Android) saw each other move after the skin geometry fix |
| 8 | Chunk streaming: track each player's chunk, recentre on crossing a boundary, send the chunks newly in range (radius ≤ 8) with a NetworkChunkPublisherUpdate; always-visible name tags | Walking or flying far keeps loading terrain | ✅ done 2026-09-26; streaming worked live (commit `5064e11`) |
| 9 | Entity tracker (AddPlayer and RemoveActor as players enter and leave each other's view), and own-entity metadata with HasGravity so players stop floating | A player returning to a stationary one reappears; players fall after flying | ✅ done 2026-09-26; returning players reappear and players fall after flying (commit `25e21c9`) |
| 10 | Vanilla movement speed: the player's own UpdateAttributes (`minecraft:movement` 0.1, underwater and lava 0.02, health 20) and UpdateAbilities (creative abilities; walk 0.1, fly 0.05, vertical fly 1.0) during spawn | Walking feels like vanilla | ✅ done 2026-09-26; walking feels like vanilla (commit `4bce850`) |
| 11 | Mutable world and block breaking: `World` keeps changed columns; breaks from PlayerAuthInput block actions (StartBreak, PredictDestroyBlock) and PlayerAction (CreativeDestroyBlock), checked for height, reach and loaded chunk; UpdateBlock to every player with the chunk | Broken blocks stay broken, for everyone, and after walking away and back | ✅ done 2026-09-26; breaks persist and sync across clients (commit `e9ebff0`) |
| 12 | Block placing and feedback: a hotbar of 9 vanilla blocks (a partial ItemRegistry with vanilla item IDs, InventoryContent at spawn), ClickBlock from InventoryTransaction placed against the clicked face (reach, air, not inside the placer; refusals undone with UpdateBlock), UpdateBlock plus the `place` sound; break particles (LevelEvent 2001); arm swings (Animate); sneaking (SetActorData) | Blocks can be placed and everyone sees and hears building; swings and crouching show | ✅ done 2026-09-26; hotbar, placing, particles, sounds, swings and sneaking work (commit `c3a895b`) |
| 13 | Placement never overlaps a player: every online player's box is checked, and refusals are rolled back | A block cannot be placed where another player stands | ✅ done 2026-09-26; blocks vanish when placed inside another player (commit `8e8e1c4`) |
| 14 | World persistence: changed chunks saved as one compressed file each under `world/chunks/`, every 5 s from the tick loop and on shutdown; loaded the first time a chunk is used, generated otherwise | Builds survive a server restart | ✅ done 2026-09-26; builds survive restarts (commit `0fdbf88`) |
| 15 | Player persistence and spawn: `players/<uuid>.json` (feet position and rotation) saved on leave, every 5 s and at shutdown, restored at login; new players spawn at (0, -60, 0); broadcasts logged under the `chat` target | Rejoining puts you where you left; new players start at 0, 0 | ✅ done 2026-09-26; position and view direction restore on rejoin |
| 16 | Flying state saved and restored (UpdateAbilities answers StartFlying and StopFlying); storage behind the `WorldStorage` trait with chunk format v2 (palettes of block names and states; v1 still read); server broadcasts as System text | Leaving while flying, you rejoin in the air; old and new chunk files load | 🧪 ready for a live test (2026-09-26) |

Later steps are proposed but not yet scheduled:
- player auth (JWKS verification of the multiplayer token)
- vanilla item and biome data (ItemRegistry, BiomeDefinitionList, CreativeContent)
- real skins forwarded from each client's login data (persona pieces and tints included)
- movement validation (speed and teleport checks) and server corrections
- chunk streaming
- more plugin events (chat, quit) and actions
- the JS/TS and Python engines
- persistence

## 7. Risks and open questions

- **Live-client interop: session setup verified.** On 2026-09-25 a vanilla Bedrock
  1.26.51 client joined over LAN. Signaling, our identity assertion, ICE, DTLS (server
  active), SCTP and the client's reliable data channel all worked. The same day, over
  ICE-lite, the client completed the step 4 login handshake: compressed batches both ways,
  and Mistvale's Disconnect message was displayed. Still unverified:
  - NAT'd or public deployments using advertised addresses
- **Minimal registries work for spawning.** The spawn sends an empty ItemRegistry and no
  BiomeDefinitionList, mirroring gophertunnel's minimal server. A live 1.26.51 client
  spawned with them on 2026-09-25. Inventories and the creative menu stay empty until
  vanilla item data arrives, which would come from a BDS data dump subject to licensing
  (see below).
- **str0m's SDP candidate parser is strict.** It expects the `ufrag` extension after
  `network-id`, while libwebrtc (and so our answer) writes it before. It then silently
  drops the candidate. This only affects str0m acting as a client, as in the loopback
  test, which normalizes the lines.
- **Visibility works live after a skin fix.** The first two-client test (2026-09-26)
  disconnected both clients the moment the second one joined. The generated skin had an
  empty geometry string, which Mojang's schema says must be valid JSON. The skin now
  carries a full `geometry.humanoid.custom` definition (format 1.12.0, engine version
  `0.0.0`), the way vanilla clients send classic skins. AddPlayer and its ability layer
  were re-checked against Mojang's schema and match, and the retest succeeded. Movement is
  still trusted as the client reports it. Sneaking and arm swings are now shown, because
  the flag numbering question is settled: the bit numbers in Mojang's PlayerActionType
  descriptions match gophertunnel's input flags (StartSneaking is bit 27, MissedSwing
  bit 39). Mojang's enum just omits some entries.
- **Vanilla world compatibility.** Vanilla Bedrock worlds are LevelDB databases (Mojang's
  fork, with raw-zlib compression), keyed by chunk, dimension and record type. Sub-chunks
  are stored with little-endian NBT palettes of block names and states. Reading them
  natively is the better end state, so owners can drop in a world, but most of the cost
  is not the database:
  - A pure-Rust LevelDB (`rusty-leveldb`, with a custom compressor) or C bindings to
    Mojang's fork. Both are workable.
  - Little-endian NBT *decoding*; Mistvale only encodes network NBT today.
  - Upgrading old block states: older worlds use renamed or merged blocks (`stone` with
    `stone_type`, and so on), which need Mojang's upgrade tables.
  - Block entities, entities, biomes, and the level.dat and player records, or keeping
    them intact when writing.

  Palettes store names and states, and Mistvale's network IDs are hashes of exactly
  those, so blocks themselves map cleanly. The plan is to move the `.bin` format to name
  palettes first (it has a version byte), then add LevelDB as a second backend: first
  read-only import, then native read and write.
- **Saved chunks store block names (resolved 2026-09-26).** Version 1 stored state
  hashes, which change when Mojang renames a block or changes its states. Version 2
  stores names and states. A rename across versions will still need an upgrade table,
  as for vanilla worlds, but a saved block can no longer silently become a different
  one. Blocks outside the world's known table are kept as raw placeholders. Changed
  columns still stay in memory until shutdown; unloading is not needed at this scale
  yet.
- **A partial ItemRegistry is untested with a live client.** Only nine items are
  registered, with vanilla's IDs, where vanilla sends about 1,900. An empty registry
  worked for spawning, but held items might not render or be usable. The hotbar blocks
  are also assumed to have no block states in 26.51. If one shows as an unknown block,
  its state hash is wrong.
- **Players are not authenticated yet.** We verify that the offer's `cpk` key signed its
  DTLS fingerprints, which binds the session to that key. We do not yet verify the
  GameServerToken's RS256 signature against the Minecraft auth service JWKS, so the
  claimed identity is unproven. The discovery URL, issuer and JWKS still need to be
  confirmed. Until then the name and UUID plugins see are claims, and must not guard
  permissions. A second login with the same UUID is not kicked yet either.
- **Send segment size.** str0m's direct API caps what we send at 64 KiB per SCTP message
  (§3.4). That is valid NetherNet, but smaller than BDS's 256 KiB. It could be lifted
  upstream or by switching engines.
- **UDP reachability.** The media port must be reachable through NAT and firewalls, which
  needs an advertise-address setting.
- **Data licensing.** Block palette, item and creative data come from BDS dumps, and
  Mojang's schemas are under the EULA.
- **Optional engines.** `deno_core` is large and slow to compile; RustPython performs
  poorly.
- **26.60** will bump the protocol (2216+ in preview).

## 8. Sources

- [Mojang — NetherNet HTTP Signaling Partner Onboarding Guide](https://mojang.github.io/bedrock-protocol-docs/guides/nether-net-onboarding-guide/)
- [Mojang/bedrock-protocol-docs](https://github.com/Mojang/bedrock-protocol-docs) · [releases](https://github.com/Mojang/bedrock-protocol-docs/releases)
- [df-mc/nethernet-spec](https://github.com/df-mc/nethernet-spec) (1.20.50-era)
- [df-mc/go-nethernet](https://github.com/df-mc/go-nethernet) (`conn.go`, `listener.go`, `identity.go`, `endpoint/handler.go`)
- [Sandertv/gophertunnel](https://github.com/Sandertv/gophertunnel) (`minecraft/protocol/packet` encoder/decoder, `minecraft/service`)
- [Bedrock Edition 26.51 — Minecraft Wiki](https://minecraft.wiki/w/Bedrock_Edition_26.51)
- [WaterdogPE — NetherNet Configuration](https://docs.waterdog.dev/waterdogpe-setup/nethernet-configuration)
- [Dragonfly PR #1290 — first-class NetherNet support](https://github.com/df-mc/dragonfly/pull/1290) · [world package](https://pkg.go.dev/github.com/df-mc/dragonfly/server/world)
- [Pumpkin PR #3472 — 26.51](https://github.com/Pumpkin-MC/Pumpkin/pull/3472) · [PR #2825 — LAN discovery](https://github.com/Pumpkin-MC/Pumpkin/pull/2825)
- [playit.gg — NetherNet vs RakNet](https://playit.gg/support/minecraft-bedrock-nethernet/)
- [str0m](https://github.com/algesten/str0m) · [webrtc v0.20 announcement](https://webrtc.rs/blog/2026/07/31/announcing-webrtc-v0.20.0.html)
