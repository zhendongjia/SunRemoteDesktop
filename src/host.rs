use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use anyhow::{Context, Result};
use ironrdp_server::{ConnectionHandler, PostConnectionAction, RdpServer};
use rcgen::generate_simple_self_signed;
use tokio_rustls::TlsAcceptor;

use crate::auth::LocalAccountValidator;
use crate::config;
use crate::display::{FrameHub, RdpDisplay};
use crate::input::HostInputHandler;
use crate::platform::{DesktopCapture, InputInjector};

#[cfg(windows)]
pub async fn run_server(config_path: &Path) -> Result<()> {
    let settings = config::load_from(config_path)?;
    if !settings.enabled {
        tracing::info!("RDP desktop host is disabled");
        return Ok(());
    }
    if settings.allowed_users.is_empty() {
        anyhow::bail!(
            "no allowed local users configured; add at least one account in the admin UI"
        );
    }

    let capture = crate::platform::windows::WindowsDesktopCapture::new(settings.fps)
        .context("initialize Windows desktop capture")?;
    let size = capture.size();
    let hub = FrameHub::new(size);
    let capture_hub = hub.clone();
    Box::new(capture).start(Box::new(move |frame| capture_hub.publish(frame)))?;

    let injector: Arc<dyn InputInjector> =
        Arc::new(crate::platform::windows::WindowsInputInjector::new());
    let display = RdpDisplay::new(hub, size);
    let input = HostInputHandler::new(injector, size, settings.allow_control);
    let tls_acceptor =
        build_tls_acceptor(&config::certificate_path(), &config::private_key_path())?;
    let validator = Arc::new(LocalAccountValidator::new(config_path.to_path_buf()));
    let connections = ConnectionLimiter::new(settings.max_clients);

    let addr: std::net::SocketAddr = format!("{}:{}", settings.bind_address, settings.port)
        .parse()
        .with_context(|| {
            format!(
                "invalid bind address {}:{}",
                settings.bind_address, settings.port
            )
        })?;

    let mut server = RdpServer::builder()
        .with_addr(addr)
        .with_tls(tls_acceptor)
        .with_input_handler(input)
        .with_display_handler(display)
        .with_credential_validator(Some(validator))
        .with_connection_handler(Some(Box::new(connections)))
        .build();

    tracing::info!(%addr, width = size.width, height = size.height, "RDP mirror server listening");
    server.run().await.context("RDP server stopped")
}

#[cfg(not(windows))]
pub async fn run_server(_config_path: &Path) -> Result<()> {
    anyhow::bail!("当前版本只实现 Windows 桌面采集和输入注入；跨平台接口已预留")
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
    Ok(identity.make_acceptor()?)
}

struct ConnectionLimiter {
    active: AtomicU32,
    maximum: u32,
}

impl ConnectionLimiter {
    fn new(maximum: u32) -> Self {
        Self {
            active: AtomicU32::new(0),
            maximum,
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
            tracing::info!(%peer, current, "RDP client accepted");
            true
        }
    }

    fn on_disconnected(
        &mut self,
        peer: std::net::SocketAddr,
        _duration: std::time::Duration,
        _error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        let remaining = self.active.fetch_sub(1, Ordering::AcqRel).saturating_sub(1);
        tracing::info!(%peer, remaining, "RDP client disconnected");
        PostConnectionAction::Continue
    }
}
