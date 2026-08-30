use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use ironrdp_pdu::rdp::capability_sets::{BitmapCodecs, Codec, CodecProperty, NsCodec};
use ironrdp_server::RdpServer;
use rcgen::generate_simple_self_signed;
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpSocket;
use tokio::sync::watch;
use tokio::task::{JoinSet, LocalSet};
use tokio_rustls::TlsAcceptor;

use crate::access::AccessGate;
use crate::auth::LocalAccountValidator;
use crate::config;
use crate::display::{FrameHub, RdpDisplay};
use crate::input::HostInputHandler;
use crate::platform::{DesktopCapture, DesktopSize, InputInjector};
use crate::session::SessionCoordinator;
use crate::touch::DirectTouchFactory;
use crate::trust::{ClientIdentityResolver, TrustedClientStore};

const TCP_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const TCP_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const TCP_KEEPALIVE_RETRIES: u32 = 3;

#[cfg(windows)]
pub async fn run_server(config_path: &Path) -> Result<()> {
    let settings = config::load_from(config_path)?;
    if !validate_settings(&settings)? {
        return Ok(());
    }

    let capture = crate::platform::windows::WindowsDesktopCapture::new(settings.fps)
        .context("initialize Windows desktop capture")?;
    let size = capture.size();
    let hub = FrameHub::new(size);
    let capture_hub = hub.clone();
    Box::new(capture).start(Box::new(move |frame| capture_hub.publish(frame)))?;

    let injector: Arc<dyn InputInjector> =
        Arc::new(crate::platform::windows::WindowsInputInjector::new());
    run_server_with_backend(settings, config_path, size, hub, injector, None).await
}

#[cfg(windows)]
pub async fn run_service_server(
    config_path: &Path,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let settings = config::load_from(config_path)?;
    if !validate_settings(&settings)? {
        return Ok(());
    }

    let backend = tokio::select! {
        result = crate::bridge::windows::wait_for_service_backend() => result?,
        _ = wait_for_shutdown(&mut shutdown) => {
            tracing::info!("Windows service stopped while waiting for a session agent");
            return Ok(());
        }
    };
    run_server_with_backend(
        settings,
        config_path,
        backend.size,
        backend.hub,
        backend.injector,
        Some(shutdown),
    )
    .await
}

#[cfg(windows)]
async fn run_server_with_backend(
    settings: config::AppConfig,
    config_path: &Path,
    size: DesktopSize,
    hub: FrameHub,
    injector: Arc<dyn InputInjector>,
    shutdown: Option<watch::Receiver<bool>>,
) -> Result<()> {
    let local = LocalSet::new();
    local
        .run_until(run_server_listener(
            settings,
            config_path,
            size,
            hub,
            injector,
            shutdown,
        ))
        .await
}

async fn run_server_listener(
    settings: config::AppConfig,
    config_path: &Path,
    size: DesktopSize,
    hub: FrameHub,
    injector: Arc<dyn InputInjector>,
    shutdown: Option<watch::Receiver<bool>>,
) -> Result<()> {
    let tls_acceptor =
        build_tls_acceptor(&config::certificate_path(), &config::private_key_path())?;
    let coordinator =
        SessionCoordinator::new(settings.max_clients, hub.clone(), Arc::clone(&injector));
    let identities = ClientIdentityResolver::discover();
    let trusted_clients = TrustedClientStore::load(config::trusted_clients_path());

    let addr: std::net::SocketAddr = format!("{}:{}", settings.bind_address, settings.port)
        .parse()
        .with_context(|| {
            format!(
                "invalid bind address {}:{}",
                settings.bind_address, settings.port
            )
        })?;

    let socket = match addr {
        std::net::SocketAddr::V4(_) => TcpSocket::new_v4().context("create IPv4 socket")?,
        std::net::SocketAddr::V6(_) => TcpSocket::new_v6().context("create IPv6 socket")?,
    };
    socket.bind(addr).context("bind SunRDP listener")?;
    let listener = socket.listen(128).context("start SunRDP listener")?;
    let mut connections = JoinSet::new();
    let mut shutdown = shutdown;

    tracing::info!("SunRDP graphics policy uses the Windows App compatible NSCodec path");
    tracing::info!(%addr, width = size.width, height = size.height, "SunRDP server listening");
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted.context("accept RDP client")?;
                if let Err(error) = configure_transport(&stream) {
                    tracing::warn!(%peer, ?error, "unable to apply RDP TCP keepalive policy");
                }
                let Some(session) = coordinator.reserve(peer) else {
                    tracing::warn!(
                        %peer,
                        current = coordinator.active_count(),
                        maximum = settings.max_clients.saturating_add(1),
                        "RDP client rejected: owner and takeover candidate slots are full"
                    );
                    drop(stream);
                    continue;
                };
                let candidate = session.has_other_owner();
                let identity = identities.resolve(peer.ip());
                let access_gate = AccessGate::new_for_session(
                    config_path.to_path_buf(),
                    session.clone(),
                    identity.clone(),
                    trusted_clients.clone(),
                );
                let mut server = build_connection_server(
                    addr,
                    &settings,
                    config_path,
                    hub.clone(),
                    Arc::clone(&injector),
                    access_gate,
                    tls_acceptor.clone(),
                );
                session.attach(server.event_sender().clone());
                tracing::info!(
                    %peer,
                    session_id = session.id(),
                    client = %identity.label(),
                    takeover_candidate = candidate,
                    current = coordinator.active_count(),
                    "RDP client accepted"
                );
                connections.spawn_local(async move {
                    let started = tokio::time::Instant::now();
                    let result = server.run_connection(stream).await;
                    let duration = started.elapsed();
                    session.close();
                    if let Err(error) = result {
                        tracing::warn!(%peer, ?duration, ?error, "RDP client disconnected after an error");
                    } else {
                        tracing::info!(%peer, ?duration, "RDP client disconnected");
                    }
                });
            }
            Some(joined) = connections.join_next() => {
                if let Err(error) = joined {
                    tracing::error!(?error, "SunRDP connection task failed");
                }
            }
            _ = async {
                match shutdown.as_mut() {
                    Some(receiver) => wait_for_shutdown(receiver).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                tracing::info!("SunRDP server received the service stop request");
                break;
            }
        }
    }

    coordinator.disconnect_all("SunRDP server is shutting down");
    let graceful = async {
        while let Some(joined) = connections.join_next().await {
            if let Err(error) = joined {
                tracing::error!(?error, "SunRDP connection task failed during shutdown");
            }
        }
    };
    if tokio::time::timeout(Duration::from_secs(3), graceful)
        .await
        .is_err()
    {
        connections.abort_all();
    }
    Ok(())
}

fn configure_transport(stream: &tokio::net::TcpStream) -> Result<()> {
    stream.set_nodelay(true).context("enable TCP_NODELAY")?;
    let keepalive = TcpKeepalive::new()
        .with_time(TCP_KEEPALIVE_IDLE)
        .with_interval(TCP_KEEPALIVE_INTERVAL)
        .with_retries(TCP_KEEPALIVE_RETRIES);
    SockRef::from(stream)
        .set_tcp_keepalive(&keepalive)
        .context("enable TCP keepalive")
}

fn build_connection_server(
    addr: std::net::SocketAddr,
    settings: &config::AppConfig,
    config_path: &Path,
    hub: FrameHub,
    injector: Arc<dyn InputInjector>,
    access_gate: AccessGate,
    tls_acceptor: TlsAcceptor,
) -> RdpServer {
    let display =
        RdpDisplay::with_dynamic_resize(hub.clone(), access_gate.clone(), Arc::clone(&injector));
    let touch = DirectTouchFactory::new(
        Arc::clone(&injector),
        hub.clone(),
        settings.allow_control,
        access_gate.clone(),
    );
    let input = HostInputHandler::new(injector, hub, settings.allow_control, access_gate.clone());
    let validator = Arc::new(LocalAccountValidator::new(
        config_path.to_path_buf(),
        access_gate,
    ));
    RdpServer::builder()
        .with_addr(addr)
        .with_tls(tls_acceptor)
        .with_input_handler(input)
        .with_display_handler(display)
        .with_rdpei_factory(Some(Box::new(touch)))
        .with_honor_client_desktop_size(Some(ironrdp_server::DesktopSize {
            width: 8192,
            height: 8192,
        }))
        .with_bitmap_codecs(display_codecs())
        .with_credential_validator(Some(validator))
        .build()
}

fn display_codecs() -> BitmapCodecs {
    // Recent Windows App builds may advertise the standalone RemoteFX codec,
    // ACK its surface updates, and still leave the legacy surface black. Keep
    // the production offer to NSCodec until codec selection can be based on a
    // tested client profile. Clients without NSCodec safely fall back to the
    // standard bitmap path.
    let mut codecs = BitmapCodecs::default();
    codecs.0.push(Codec {
        id: 0,
        property: CodecProperty::NsCodec(NsCodec {
            is_dynamic_fidelity_allowed: false,
            is_subsampling_allowed: false,
            color_loss_level: 1,
        }),
    });
    codecs
}

#[cfg(not(windows))]
pub async fn run_server(_config_path: &Path) -> Result<()> {
    anyhow::bail!("当前版本只实现 Windows 桌面采集和输入注入；跨平台接口已预留")
}

#[cfg(not(windows))]
pub async fn run_service_server(
    _config_path: &Path,
    _shutdown: watch::Receiver<bool>,
) -> Result<()> {
    anyhow::bail!("Windows 服务和会话桥只在 Windows 构建中可用")
}

fn validate_settings(settings: &config::AppConfig) -> Result<bool> {
    if !settings.enabled {
        tracing::info!("SunRDP desktop host is disabled");
        return Ok(false);
    }
    if settings.allowed_users.is_empty() {
        anyhow::bail!(
            "no allowed local users configured; add at least one account in the admin UI"
        );
    }
    Ok(true)
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            break;
        }
    }
}

fn build_tls_acceptor(cert_path: &Path, key_path: &Path) -> Result<TlsAcceptor> {
    if !cert_path.exists() || !key_path.exists() {
        if let Some(parent) = cert_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create TLS directory {}", parent.display()))?;
        }
        let subject = std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "localhost".to_string());
        let certified = generate_simple_self_signed(vec![subject.clone(), "localhost".to_string()])
            .context("generate self-signed TLS certificate")?;
        std::fs::write(cert_path, certified.cert.pem())
            .with_context(|| format!("write {}", cert_path.display()))?;
        std::fs::write(key_path, certified.signing_key.serialize_pem())
            .with_context(|| format!("write {}", key_path.display()))?;
        tracing::info!(
            "generated a self-signed TLS certificate; RDP clients may ask you to trust it"
        );
    }

    let identity = ironrdp_server::TlsIdentityCtx::init_from_paths(cert_path, key_path)?;
    identity.make_acceptor()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_codec_offer_is_limited_to_nscodec() {
        let codecs = display_codecs();
        assert_eq!(codecs.0.len(), 1);
        assert!(matches!(codecs.0[0].property, CodecProperty::NsCodec(_)));
    }

    #[tokio::test]
    async fn accepted_connections_enable_bounded_tcp_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let connected = tokio::net::TcpStream::connect(listener.local_addr().unwrap());
        let accepted = listener.accept();
        let (client, accepted) = tokio::join!(connected, accepted);
        let _client = client.unwrap();
        let (server, _) = accepted.unwrap();

        configure_transport(&server).unwrap();
        let socket = SockRef::from(&server);
        assert!(socket.keepalive().unwrap());
        assert_eq!(
            socket.tcp_keepalive_retries().unwrap(),
            TCP_KEEPALIVE_RETRIES
        );
    }
}
