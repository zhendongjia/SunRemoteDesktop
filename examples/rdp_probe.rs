//! Local, credential-free RDP wire probe. It never accepts an untrusted TLS
//! certificate, creates an OS logon session, or sends passwords.
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use ironrdp_connector::{BitmapConfig, ClientConnector, Config, Credentials, DesktopSize};
use ironrdp_core::{Decode, ReadCursor, encode_vec};
use ironrdp_pdu::{
    Action,
    fast_path::{FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode},
    gcc::KeyboardType,
    input::fast_path::{FastPathInput, FastPathInputEvent, KeyboardFlags},
    rdp::capability_sets::{
        BitmapCodecs, Codec, CodecProperty, MajorPlatformType, NsCodec, client_codecs_capabilities,
    },
    surface_commands::SurfaceCommand,
};
use ironrdp_tokio::{FramedWrite, TokioFramed, connect_begin, connect_finalize, mark_as_upgraded};
use tokio::net::TcpStream;
use tokio_rustls::{
    TlsConnector,
    rustls::{
        ClientConfig, RootCertStore,
        pki_types::{CertificateDer, ServerName, pem::PemObject},
    },
};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:3390")]
    address: SocketAddr,
    #[arg(long, default_value = "nscodec", value_parser = ["nscodec", "remotefx"])]
    codec: String,
    #[arg(long)]
    certificate: Option<PathBuf>,
}

type Stream = TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>;

struct NoExternalAuthentication;
impl ironrdp_tokio::NetworkClient for NoExternalAuthentication {
    async fn send(
        &mut self,
        _: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ironrdp_connector::ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!(
            "external authentication is disabled in this probe"
        ))
    }
}

fn client_config(codec: &str) -> Result<Config> {
    let codecs = if codec == "nscodec" {
        BitmapCodecs(vec![Codec {
            id: 1,
            property: CodecProperty::NsCodec(NsCodec {
                is_dynamic_fidelity_allowed: false,
                is_subsampling_allowed: false,
                color_loss_level: 1,
            }),
        }])
    } else {
        client_codecs_capabilities(&["remotefx:on", "qoi:off", "qoiz:off"])
            .map_err(anyhow::Error::msg)?
    };
    Ok(Config {
        desktop_size: DesktopSize {
            width: 1280,
            height: 720,
        },
        desktop_scale_factor: 100,
        enable_tls: true,
        enable_credssp: false,
        credentials: Credentials::UsernamePassword {
            username: String::new(),
            password: String::new(),
        },
        domain: None,
        client_build: 0,
        client_name: "SunRDP-Probe".into(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0x409,
        ime_file_name: String::new(),
        bitmap: Some(BitmapConfig {
            lossy_compression: true,
            color_depth: 32,
            codecs,
        }),
        dig_product_id: String::new(),
        client_dir: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: MajorPlatformType::WINDOWS,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        performance_flags: Default::default(),
        license_cache: None,
        timezone_info: Default::default(),
        compression_type: None,
        enable_server_pointer: false,
        pointer_software_rendering: false,
        multitransport_flags: None,
    })
}

#[derive(Default)]
struct SurfaceReader {
    fragments: Vec<u8>,
    bytes: usize,
}
impl SurfaceReader {
    async fn next(&mut self, stream: &mut Stream) -> Result<(u8, u16, u16, u16, u16)> {
        loop {
            let (action, packet) = tokio::time::timeout(Duration::from_secs(15), stream.read_pdu())
                .await
                .context("timed out waiting for a display update")??;
            self.bytes += packet.len();
            if action != Action::FastPath {
                continue;
            }
            let mut cursor = ReadCursor::new(&packet);
            FastPathHeader::decode(&mut cursor)?;
            let update = FastPathUpdatePdu::decode(&mut cursor)?;
            ensure!(
                update.compression_flags.is_none(),
                "unexpected bulk compression"
            );
            if update.update_code != UpdateCode::SurfaceCommands {
                continue;
            }
            if matches!(
                update.fragmentation,
                Fragmentation::Single | Fragmentation::First
            ) {
                self.fragments.clear();
            }
            self.fragments.extend_from_slice(update.data);
            ensure!(
                self.fragments.len() <= 16 * 1024 * 1024,
                "display update exceeds the probe limit"
            );
            if !matches!(
                update.fragmentation,
                Fragmentation::Single | Fragmentation::Last
            ) {
                continue;
            }
            let mut surface = ReadCursor::new(&self.fragments);
            match SurfaceCommand::decode(&mut surface)? {
                SurfaceCommand::SetSurfaceBits(bits) | SurfaceCommand::StreamSurfaceBits(bits) => {
                    let rect = bits.destination;
                    return Ok((
                        bits.extended_bitmap_data.codec_id,
                        rect.left,
                        rect.top,
                        rect.right,
                        rect.bottom,
                    ));
                }
                SurfaceCommand::FrameMarker(_) => {}
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    ensure!(
        args.address.ip().is_loopback(),
        "this probe only connects to the local SunRDP service"
    );
    let cert_path = args
        .certificate
        .unwrap_or_else(sun_remote_desktop::config::certificate_path);
    let mut roots = RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(&cert_path)? {
        roots.add(cert?)?;
    }
    let tls = TlsConnector::from(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));
    let started = Instant::now();
    let tcp = TcpStream::connect(args.address).await?;
    tcp.set_nodelay(true)?;
    let mut connector = ClientConnector::new(client_config(&args.codec)?, tcp.local_addr()?);
    let mut plain = TokioFramed::new(tcp);
    let upgrade = tokio::time::timeout(
        Duration::from_secs(10),
        connect_begin(&mut plain, &mut connector),
    )
    .await??;
    let stream = tls
        .connect(
            ServerName::try_from("localhost")?,
            plain.into_inner_no_leftover(),
        )
        .await?;
    let mut stream = TokioFramed::new(stream);
    let upgraded = mark_as_upgraded(upgrade, &mut connector);
    let connected = tokio::time::timeout(
        Duration::from_secs(10),
        connect_finalize(
            upgraded,
            connector,
            &mut stream,
            &mut NoExternalAuthentication,
            "localhost".into(),
            vec![],
            None,
        ),
    )
    .await??;
    println!(
        "connected: requested_codec={}, desktop={}x{}, handshake_ms={}",
        args.codec,
        connected.desktop_size.width,
        connected.desktop_size.height,
        started.elapsed().as_millis()
    );
    let mut reader = SurfaceReader::default();
    let mut rows = vec![false; usize::from(connected.desktop_size.height)];
    let first_started = Instant::now();
    let codec_id = loop {
        let (codec, left, top, right, bottom) = reader.next(&mut stream).await?;
        ensure!(
            left == 0 && right == connected.desktop_size.width,
            "initial screen must cover the desktop width"
        );
        ensure!(
            top < bottom && usize::from(bottom) <= rows.len(),
            "invalid surface rectangle"
        );
        rows[usize::from(top)..usize::from(bottom)].fill(true);
        if rows.iter().all(|row| *row) {
            break codec;
        }
    };
    println!(
        "first_screen: codec_id={codec_id}, rdp_bytes={}, elapsed_ms={}",
        reader.bytes,
        first_started.elapsed().as_millis()
    );
    // Empty credentials kept the connection at the SunRDP access screen. Tab
    // only changes its focus; no local account is authenticated by this probe.
    let tab = FastPathInput::new(vec![
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 15),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 15),
    ])?;
    let input_started = Instant::now();
    stream.write_all(&encode_vec(&tab)?).await?;
    reader.next(&mut stream).await?;
    println!(
        "tab_first_response_ms={}",
        input_started.elapsed().as_millis()
    );
    Ok(())
}
