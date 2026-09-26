//! `mistvale`: the Mistvale BDS server binary.
//!
//! NetherNet settings can be overridden with environment variables:
//! - `MISTVALE_SIGNALING_ADDR`: TCP address for HTTP signaling (default `0.0.0.0:19132`)
//! - `MISTVALE_MEDIA_PORT`: UDP port for WebRTC traffic (default `19133`)
//! - `MISTVALE_MEDIA_IPS`: comma-separated local addresses for WebRTC traffic
//!   (default: every IPv4 interface that is up)
//! - `MISTVALE_ADVERTISE_IPS`: comma-separated public addresses to offer clients
//! - `MISTVALE_ICE_LITE`: `false` switches from ICE-lite to full ICE (default `true`)

use std::net::IpAddr;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::Context as _;
use mistvale_core::server::{self, PLUGIN_ACTION_QUEUE, Server};
use mistvale_core::session;
use mistvale_core::tick::TickLoop;
use mistvale_core::world::FlatWorld;
use mistvale_net::{Connection, Listener, ListenerConfig, ServerStatus};
use mistvale_plugins::{PluginConfig, PluginHost};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        game = mistvale_protocol::GAME_VERSION,
        protocol = mistvale_protocol::PROTOCOL_VERSION,
        tps = mistvale_core::TICKS_PER_SECOND,
        "Mistvale BDS"
    );

    let (actions, plugin_actions) = mpsc::channel(PLUGIN_ACTION_QUEUE);
    let plugins = PluginHost::start(PluginConfig::default(), actions)
        .context("failed to start the plugin host")?;
    tracing::info!(loaded = ?plugins.loaded(), "plugins ready");
    let server = Arc::new(Server::new(FlatWorld::new(), plugins.dispatcher()));
    tokio::spawn(server::apply_plugin_actions(
        Arc::clone(&server),
        plugin_actions,
    ));
    // Stops when dropped, as `main` returns.
    let _game_loop =
        TickLoop::start(Arc::clone(&server)).context("failed to start the game loop")?;

    let status = ServerStatus {
        name: "Mistvale BDS".into(),
        protocol: mistvale_protocol::PROTOCOL_VERSION,
        version: mistvale_protocol::GAME_VERSION.into(),
        level: "Bedrock level".into(),
        players: 0,
        max_players: 20,
        game_type: 0,
    };
    let mut listener = Listener::bind(listener_config()?, status)
        .await
        .context("failed to start the NetherNet listener")?;
    tracing::info!(
        signaling = %listener.signaling_addr(),
        media = ?listener.media_addrs(),
        identity = listener.key_fingerprint(),
        "NetherNet listening"
    );

    loop {
        tokio::select! {
            connection = listener.accept() => match connection {
                Some(connection) => {
                    tokio::spawn(serve(connection, Arc::clone(&server)));
                }
                None => break,
            },
            result = tokio::signal::ctrl_c() => {
                result.context("failed to listen for Ctrl+C")?;
                tracing::info!("shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// Runs a client's protocol session until it disconnects.
async fn serve(connection: Connection, server: Arc<Server>) {
    let network_id = connection.network_id();
    tracing::info!(
        network_id,
        issuer = ?connection.client_identity().map(|identity| &identity.issuer),
        "client connected"
    );
    session::run(connection, server).await;
    tracing::info!(network_id, "client disconnected");
}

fn listener_config() -> anyhow::Result<ListenerConfig> {
    let mut config = ListenerConfig::default();
    if let Some(addr) = env_value("MISTVALE_SIGNALING_ADDR")? {
        config.signaling_addr = addr;
    }
    if let Some(port) = env_value("MISTVALE_MEDIA_PORT")? {
        config.media_port = port;
    }
    if let Some(ips) = env_addresses("MISTVALE_MEDIA_IPS")? {
        config.media_ips = ips;
    }
    if let Some(ips) = env_addresses("MISTVALE_ADVERTISE_IPS")? {
        config.advertise_ips = ips;
    }
    if let Some(ice_lite) = env_value("MISTVALE_ICE_LITE")? {
        config.ice_lite = ice_lite;
    }
    Ok(config)
}

fn env_value<T>(name: &str) -> anyhow::Result<Option<T>>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    match std::env::var(name) {
        Ok(value) => value
            .trim()
            .parse()
            .map(Some)
            .with_context(|| format!("invalid {name}: {value:?}")),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("invalid {name}")),
    }
}

fn env_addresses(name: &str) -> anyhow::Result<Option<Vec<IpAddr>>> {
    let Some(value) = env_value::<String>(name)? else {
        return Ok(None);
    };
    value
        .split(',')
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .map(|ip| {
            ip.parse()
                .with_context(|| format!("invalid address {ip:?} in {name}"))
        })
        .collect::<anyhow::Result<_>>()
        .map(Some)
}
