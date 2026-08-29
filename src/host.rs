use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use anyhow::{Context, Result};
use ironrdp_pdu::rdp::capability_sets::{BitmapCodecs, Codec, CodecProperty, NsCodec};
use ironrdp_server::{ConnectionHandler, PostConnectionAction, RdpServer};
use rcgen::generate_simple_self_signed;
use tokio::sync::watch;
use tokio_rustls::TlsAcceptor;

use crate::access::AccessGate;
use crate::auth::LocalAccountValidator;
use crate::config;
use crate::display::{FrameHub, RdpDisplay};
use crate::input::HostInputHandler;
use crate::platform::{DesktopCapture, DesktopSize, InputInjector};
use crate::touch::DirectTouchFactory;

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
    let access_gate = AccessGate::new(config_path.to_path_buf());
    let display =
        RdpDisplay::with_dynamic_resize(hub.clone(), access_gate.clone(), Arc::clone(&injector));
    let touch = DirectTouchFactory::new(
        Arc::clone(&injector),
        hub.clone(),
        settings.allow_control,
        access_gate.clone(),
    );
    let input = HostInputHandler::new(injector, hub, settings.allow_control, access_gate.clone());
    let tls_acceptor =
        build_tls_acceptor(&config::certificate_path(), &config::private_key_path())?;
    let validator = Arc::new(LocalAccountValidator::new(
        config_path.to_path_buf(),
        access_gate.clone(),
    ));
    let connections = ConnectionLimiter::new(settings.max_clients, access_gate);

    let addr: std::net::SocketAddr = format!("{}:{}", settings.bind_address, settings.port)
        .parse()
        .with_context(|| {
            format!(
                "invalid bind address {}:{}",
                settings.bind_address, settings.port
            )
        })?;

    tracing::info!("SunRDP graphics policy uses the Windows App compatible NSCodec path");
    let mut server = RdpServer::builder()
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
        .with_connection_handler(Some(Box::new(connections)))
        .build();

    tracing::info!(%addr, width = size.width, height = size.height, "SunRDP server listening");
    if let Some(mut shutdown) = shutdown {
        tokio::select! {
            result = server.run() => result.context("SunRDP server stopped"),
            _ = wait_for_shutdown(&mut shutdown) => {
                tracing::info!("SunRDP server received the service stop request");
                Ok(())
            }
        }
    } else {
        server.run().await.context("SunRDP server stopped")
    }
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

struct ConnectionLimiter {
    active: AtomicU32,
    maximum: u32,
    access_gate: AccessGate,
}

impl ConnectionLimiter {
    fn new(maximum: u32, access_gate: AccessGate) -> Self {
        Self {
            active: AtomicU32::new(0),
            maximum,
            access_gate,
        }
    }
}

impl ConnectionHandler for ConnectionLimiter {
    fn on_accept(&mut self, peer: std::net::SocketAddr) -> bool {
        let current = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        if current > self.maximum {
            self.active.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(%peer, current, maximum = self.maximum, "RDP client rejected: connection limit reached");
            false
        } else {
            self.access_gate.reset();
            tracing::info!(%peer, current, "RDP client accepted");
            true
        }
    }

    fn on_disconnected(
        &mut self,
        peer: std::net::SocketAddr,
        _duration: std::time::Duration,
        error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        let remaining = self.active.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
        self.access_gate.reset();
        if let Some(error) = error {
            tracing::warn!(%peer, remaining, ?error, "RDP client disconnected after an error");
        } else {
            tracing::info!(%peer, remaining, "RDP client disconnected");
        }
        PostConnectionAction::Continue
    }
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
}
