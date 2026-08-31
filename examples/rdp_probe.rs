//! Local RDP wire probe. By default it never accepts an untrusted TLS
//! certificate, creates an OS logon session, or sends passwords. The optional
//! `--authenticate` mode reads credentials from environment variables so they
//! do not appear in command history. With `--resize`, it also exercises the
//! real Display Control and RDPEI DVCs, the complete
//! Deactivation-Reactivation Sequence used by Windows App when its window is
//! resized, relative mouse clicks, and direct-touch taps.
use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use ironrdp_connector::{
    BitmapConfig, ClientConnector, Config, Credentials, DesktopSize,
    connection_activation::{ConnectionActivationFactory, ConnectionActivationState},
};
use ironrdp_core::{Decode, Encode, ReadCursor, WriteBuf, WriteCursor};
use ironrdp_displaycontrol::client::DisplayControlClient;
use ironrdp_dvc::DrdynvcClient;
use ironrdp_graphics::image_processing::PixelFormat;
use ironrdp_pdu::{
    Action,
    fast_path::{EncryptionFlags, FastPathHeader, FastPathUpdatePdu, Fragmentation, UpdateCode},
    gcc::{ConnectionType, KeyboardType},
    input::{
        MousePdu, MouseRelPdu,
        fast_path::{FastPathInput, FastPathInputEvent, FastPathInputHeader, KeyboardFlags},
        mouse::PointerFlags,
        mouse_rel::PointerRelFlags,
    },
    pointer::Point16,
    rdp::capability_sets::{
        BitmapCodecs, Codec, CodecProperty, MajorPlatformType, NsCodec, RailSupportLevel,
        client_codecs_capabilities,
    },
    surface_commands::SurfaceCommand,
};
use ironrdp_rdpei::{
    RdpeiClient,
    pdu::{TouchContact, TouchContactFlags, TouchEventPdu, TouchFrame},
};
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput, image::DecodedImage};
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
    /// Initial RDP canvas size.
    #[arg(long, default_value = "1280x720", value_parser = parse_size)]
    desktop: DesktopSize,
    /// Resize the active RDP canvas through MS-RDPEDISP, for example 1000x700.
    #[arg(long, value_parser = parse_size)]
    resize: Option<DesktopSize>,
    /// Authenticate using SUNRDP_PROBE_USERNAME and SUNRDP_PROBE_PASSWORD.
    #[arg(long)]
    authenticate: bool,
    /// Authenticate before RDP activation with NLA/CredSSP.
    #[arg(long, requires = "authenticate")]
    nla: bool,
    /// Keep consuming authenticated desktop frames for this many seconds.
    #[arg(long, default_value_t = 0)]
    post_auth_wait_seconds: u64,
    /// Confirm the gated "Disconnect other clients" option before continuing.
    #[arg(long)]
    takeover: bool,
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

fn client_config(
    codec: &str,
    desktop_size: DesktopSize,
    credentials: Credentials,
    domain: Option<String>,
    autologon: bool,
    enable_credssp: bool,
) -> Result<Config> {
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
        desktop_size,
        monitor_layout: None,
        desktop_scale_factor: 100,
        enable_tls: true,
        enable_credssp,
        enable_standard_rdp_security: false,
        credentials,
        domain,
        client_build: 0,
        client_name: "SunRDP-Probe".into(),
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0x409,
        connection_type: ConnectionType::Lan,
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
        remote_application_mode: false,
        rail_support_level: RailSupportLevel::empty(),
        platform: MajorPlatformType::WINDOWS,
        hardware_id: None,
        request_data: None,
        autologon,
        enable_audio_playback: false,
        enable_audio_capture: false,
        performance_flags: Default::default(),
        license_cache: None,
        timezone_info: Default::default(),
        compression_type: None,
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
    })
}

fn probe_credentials(authenticate: bool) -> Result<(Credentials, Option<String>, bool)> {
    if !authenticate {
        return Ok((
            Credentials::UsernamePassword {
                username: String::new(),
                password: String::new(),
            },
            None,
            false,
        ));
    }

    let username = std::env::var("SUNRDP_PROBE_USERNAME")
        .context("SUNRDP_PROBE_USERNAME is required with --authenticate")?;
    let password = std::env::var("SUNRDP_PROBE_PASSWORD")
        .context("SUNRDP_PROBE_PASSWORD is required with --authenticate")?;
    ensure!(!username.trim().is_empty(), "probe username is empty");
    ensure!(!password.is_empty(), "probe password is empty");
    let domain = std::env::var("SUNRDP_PROBE_DOMAIN")
        .ok()
        .filter(|value| !value.trim().is_empty());
    Ok((
        Credentials::UsernamePassword { username, password },
        domain,
        true,
    ))
}

fn parse_size(value: &str) -> Result<DesktopSize, String> {
    let (width, height) = value
        .split_once(['x', 'X'])
        .ok_or_else(|| "expected WIDTHxHEIGHT".to_owned())?;
    let width = width
        .parse::<u16>()
        .map_err(|_| "width must be an integer from 200 through 8192".to_owned())?;
    let height = height
        .parse::<u16>()
        .map_err(|_| "height must be an integer from 200 through 8192".to_owned())?;
    if !(200..=8192).contains(&width) || width % 2 != 0 {
        return Err("width must be an even integer from 200 through 8192".to_owned());
    }
    if !(200..=8192).contains(&height) {
        return Err("height must be an integer from 200 through 8192".to_owned());
    }
    Ok(DesktopSize { width, height })
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

    async fn wait_for_pointer_position(
        &mut self,
        stream: &mut Stream,
        expected: Point16,
    ) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            ensure!(
                !remaining.is_zero(),
                "timed out waiting for the pointer liveness response"
            );
            let (action, packet) = tokio::time::timeout(remaining, stream.read_pdu())
                .await
                .context("timed out waiting for the pointer liveness response")??;
            self.bytes += packet.len();
            if action != Action::FastPath {
                continue;
            }
            let mut cursor = ReadCursor::new(&packet);
            FastPathHeader::decode(&mut cursor)?;
            let update = FastPathUpdatePdu::decode(&mut cursor)?;
            if update.update_code != UpdateCode::PositionPointer {
                continue;
            }
            let actual = Point16::decode(&mut ReadCursor::new(update.data))?;
            ensure!(
                actual == expected,
                "pointer liveness response differs from the requested position: expected {}x{}, received {}x{}",
                expected.x,
                expected.y,
                actual.x,
                actual.y
            );
            return Ok(());
        }
    }
}

async fn run_wire_probe(stream: &mut Stream, size: DesktopSize) -> Result<()> {
    let started = Instant::now();
    let mut reader = SurfaceReader::default();
    let codec_id = read_full_screen(&mut reader, stream, size).await?;
    println!(
        "first_screen: codec_id={codec_id}, rdp_bytes={}, elapsed_ms={}",
        reader.bytes,
        started.elapsed().as_millis()
    );

    let tab = FastPathInput::new(vec![
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 15),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 15),
    ])?;
    let input_started = Instant::now();
    stream.write_all(&ironrdp_core::encode_vec(&tab)?).await?;
    reader.next(stream).await?;
    println!(
        "tab_first_response_ms={}",
        input_started.elapsed().as_millis()
    );
    Ok(())
}

async fn read_full_screen(
    reader: &mut SurfaceReader,
    stream: &mut Stream,
    size: DesktopSize,
) -> Result<u8> {
    let mut rows = vec![false; usize::from(size.height)];
    loop {
        let (codec, left, top, right, bottom) = reader.next(stream).await?;
        ensure!(
            left == 0 && right == size.width,
            "initial screen must cover the desktop width"
        );
        ensure!(
            top < bottom && usize::from(bottom) <= rows.len(),
            "invalid surface rectangle"
        );
        rows[usize::from(top)..usize::from(bottom)].fill(true);
        if rows.iter().all(|row| *row) {
            return Ok(codec);
        }
    }
}

async fn run_authenticated_wire_probe(
    stream: &mut Stream,
    size: DesktopSize,
    post_auth_wait: Duration,
    takeover: bool,
) -> Result<()> {
    let started = Instant::now();
    let mut reader = SurfaceReader::default();
    let first_codec = read_full_screen(&mut reader, stream, size).await?;
    println!(
        "first_screen: codec_id={first_codec}, rdp_bytes={}, elapsed_ms={}",
        reader.bytes,
        started.elapsed().as_millis()
    );

    // Authentication always reaches the host-mode picker. Enter confirms its
    // default (the physical screen's current mode). A complete follow-up
    // surface proves the server kept the same connection alive while opening
    // the captured physical desktop.
    let mut confirmation_events = Vec::new();
    if takeover {
        confirmation_events.extend([
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 57),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 57),
        ]);
    }
    confirmation_events.extend([
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 28),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 28),
    ]);
    let confirmation = FastPathInput::new(confirmation_events)?;
    let input_started = Instant::now();
    let bytes_before = reader.bytes;
    stream
        .write_all(&ironrdp_core::encode_vec(&confirmation)?)
        .await?;
    let desktop_codec = read_full_screen(&mut reader, stream, size).await?;
    println!(
        "authenticated_desktop: codec_id={desktop_codec}, rdp_bytes={}, connection_alive=true, response_ms={}",
        reader.bytes - bytes_before,
        input_started.elapsed().as_millis()
    );

    if !post_auth_wait.is_zero() {
        println!(
            "handoff_window: ready, wait_seconds={}",
            post_auth_wait.as_secs()
        );
        let deadline = Instant::now() + post_auth_wait;
        let mut packets = 0u64;
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining.min(Duration::from_millis(250)), stream.read_pdu())
                .await
            {
                Ok(Ok((_action, packet))) => {
                    reader.bytes += packet.len();
                    packets += 1;
                }
                Ok(Err(error)) => return Err(error.into()),
                Err(_) => {}
            }
        }
        let expected = Point16 {
            x: size.width / 4,
            y: size.height / 4,
        };
        let pointer = FastPathInput::new(vec![
            FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::MOVE,
                number_of_wheel_rotation_units: 0,
                x_position: 1,
                y_position: 1,
            }),
            FastPathInputEvent::MouseEvent(MousePdu {
                flags: PointerFlags::MOVE,
                number_of_wheel_rotation_units: 0,
                x_position: expected.x,
                y_position: expected.y,
            }),
        ])?;
        stream
            .write_all(&ironrdp_core::encode_vec(&pointer)?)
            .await?;
        reader.wait_for_pointer_position(stream, expected).await?;
        println!(
            "post_auth_wait: packets={packets}, pointer={}x{}, connection_alive=true",
            expected.x, expected.y
        );
    }
    Ok(())
}

async fn process_next(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    deadline: Instant,
    bytes: &mut usize,
) -> Result<Vec<ActiveStageOutput>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    ensure!(!remaining.is_zero(), "timed out waiting for an RDP frame");
    let (action, packet) = tokio::time::timeout(remaining, stream.read_pdu())
        .await
        .context("timed out waiting for an RDP frame")??;
    *bytes += packet.len();
    let mut retained = Vec::new();
    for output in stage.process(image, action, &packet)? {
        match output {
            ActiveStageOutput::ResponseFrame(frame) => stream.write_all(&frame).await?,
            ActiveStageOutput::Terminate(reason) => {
                anyhow::bail!("server terminated the probe: {reason:?}")
            }
            other => retained.push(other),
        }
    }
    Ok(retained)
}

async fn probe_pointer(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    size: DesktopSize,
    bytes: &mut usize,
    mut default_shape_seen: bool,
) -> Result<(u16, u16)> {
    let expected_x = size.width / 3;
    let expected_y = size.height / 3;
    let input = FastPathInput::new(vec![
        // Force the watch channel to change even when a previous probe left
        // the shared pointer at the expected target. The display task may
        // coalesce both moves, but its final value is still the target below.
        FastPathInputEvent::MouseEvent(MousePdu {
            flags: PointerFlags::MOVE,
            number_of_wheel_rotation_units: 0,
            x_position: 1,
            y_position: 1,
        }),
        FastPathInputEvent::MouseEvent(MousePdu {
            flags: PointerFlags::MOVE,
            number_of_wheel_rotation_units: 0,
            x_position: expected_x,
            y_position: expected_y,
        }),
    ])?;
    stream.write_all(&ironrdp_core::encode_vec(&input)?).await?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for output in process_next(stage, image, stream, deadline, bytes).await? {
            match output {
                ActiveStageOutput::PointerDefault | ActiveStageOutput::PointerBitmap(_) => {
                    default_shape_seen = true
                }
                ActiveStageOutput::PointerPosition { x, y } => {
                    ensure!(
                        default_shape_seen,
                        "server moved the RDP pointer without advertising its default shape"
                    );
                    ensure!(
                        (x, y) == (expected_x, expected_y),
                        "server pointer position differs from the client input: expected {expected_x}x{expected_y}, received {x}x{y}"
                    );
                    println!("pointer: default_shape=true, position={x}x{y}");
                    return Ok((x, y));
                }
                _ => {}
            }
        }
    }
}

fn login_password_field_center(size: DesktopSize) -> (u16, u16) {
    login_field_centers(size).1
}

fn login_field_centers(size: DesktopSize) -> ((u16, u16), (u16, u16)) {
    let width = f32::from(size.width);
    let height = f32::from(size.height);
    let compact = height < 620.0 || width < 760.0;
    let margin = if compact { 16.0 } else { 28.0 };
    let card_width = 610.0_f32.min(width - margin * 2.0).max(320.0);
    let card_height = 620.0_f32.min(height - margin * 2.0).max(360.0);
    let padding = if compact { 28.0 } else { 46.0 };
    let content_x = (width - card_width) / 2.0 + padding;
    let content_y = (height - card_height) / 2.0 + padding;
    let content_width = card_width - padding * 2.0;
    let field_height = if compact { 50.0 } else { 58.0 };
    let primary_y = content_y + if compact { 132.0 } else { 166.0 };
    let secondary_y = primary_y + field_height + 42.0;
    let center_x = (content_x + content_width / 2.0) as u16;
    (
        (center_x, (primary_y + field_height / 2.0) as u16),
        (center_x, (secondary_y + field_height / 2.0) as u16),
    )
}

/// Encodes relative fast-path events without relying on IronRDP 0.13's
/// `FastPathInputEvent::MouseEventRel` encoder, which currently reserves the
/// payload bytes but does not write the `MouseRelPdu` itself. SunRDP's server
/// decoder is unaffected; this compatibility encoder keeps the wire probe from
/// reporting a false input failure.
fn encode_relative_input(events: &[MouseRelPdu]) -> Result<Vec<u8>> {
    ensure!(
        (1..=255).contains(&events.len()),
        "relative input requires between 1 and 255 events"
    );
    let event_size = 1 + MouseRelPdu {
        flags: PointerRelFlags::empty(),
        x_delta: 0,
        y_delta: 0,
    }
    .size();
    let data_length = event_size * events.len();
    let header = FastPathInputHeader {
        flags: EncryptionFlags::empty(),
        data_length,
        num_events: u8::try_from(events.len())?,
    };
    let mut frame = vec![0; header.size() + data_length];
    let mut cursor = WriteCursor::new(&mut frame);
    header.encode(&mut cursor)?;
    for event in events {
        // TS_FP_INPUT_EVENT.eventCode = FASTPATH_INPUT_EVENT_MOUSE_REL (5).
        cursor.write_u8(5 << 5);
        event.encode(&mut cursor)?;
    }
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_input_compatibility_encoder_writes_decodable_payloads() {
        let events = [
            MouseRelPdu {
                flags: PointerRelFlags::MOVE,
                x_delta: -123,
                y_delta: 456,
            },
            MouseRelPdu {
                flags: PointerRelFlags::BUTTON1 | PointerRelFlags::DOWN,
                x_delta: 0,
                y_delta: 0,
            },
        ];
        let encoded = encode_relative_input(&events).unwrap();
        let decoded = FastPathInput::decode(&mut ReadCursor::new(&encoded)).unwrap();
        let expected = events.map(FastPathInputEvent::MouseEventRel);
        assert_eq!(decoded.input_events(), expected.as_slice());
    }
}

async fn probe_relative_click(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    size: DesktopSize,
    bytes: &mut usize,
    current: (u16, u16),
) -> Result<()> {
    let target = login_password_field_center(size);
    let delta_x = i16::try_from(i32::from(target.0) - i32::from(current.0))?;
    let delta_y = i16::try_from(i32::from(target.1) - i32::from(current.1))?;
    let movement = encode_relative_input(&[MouseRelPdu {
        flags: PointerRelFlags::MOVE,
        x_delta: delta_x,
        y_delta: delta_y,
    }])?;
    stream.write_all(&movement).await?;

    let move_deadline = Instant::now() + Duration::from_secs(5);
    'movement: loop {
        for output in process_next(stage, image, stream, move_deadline, bytes).await? {
            if let ActiveStageOutput::PointerPosition { x, y } = output {
                ensure!(
                    (x, y) == target,
                    "relative pointer moved to {x}x{y}, expected {}x{}",
                    target.0,
                    target.1
                );
                break 'movement;
            }
        }
    }

    let before_click = image.data().to_vec();
    let click = encode_relative_input(&[
        MouseRelPdu {
            flags: PointerRelFlags::BUTTON1 | PointerRelFlags::DOWN,
            x_delta: 0,
            y_delta: 0,
        },
        MouseRelPdu {
            flags: PointerRelFlags::BUTTON1,
            x_delta: 0,
            y_delta: 0,
        },
    ])?;
    stream.write_all(&click).await?;

    let click_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let outputs = process_next(stage, image, stream, click_deadline, bytes).await?;
        if outputs
            .iter()
            .any(|output| matches!(output, ActiveStageOutput::GraphicsUpdate(_)))
            && image.data() != before_click
        {
            println!(
                "relative_click: target={}x{}, access_screen_changed=true",
                target.0, target.1
            );
            return Ok(());
        }
    }
}

async fn probe_direct_touch(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    size: DesktopSize,
    bytes: &mut usize,
) -> Result<()> {
    let target = login_field_centers(size).0;
    let down = TouchContact::new(
        1,
        i32::from(target.0),
        i32::from(target.1),
        TouchContactFlags::DOWN | TouchContactFlags::INRANGE | TouchContactFlags::INCONTACT,
    );
    let up = TouchContact::new(
        1,
        i32::from(target.0),
        i32::from(target.1),
        TouchContactFlags::UP | TouchContactFlags::INRANGE,
    );
    let event = TouchEventPdu::new(
        0,
        vec![TouchFrame::new(0, vec![down]), TouchFrame::new(0, vec![up])],
    );
    let frame = stage
        .encode_rdpei_touch(event)
        .context("RDPEI channel is unavailable")??;
    let before_touch = image.data().to_vec();
    let input_started = Instant::now();
    stream.write_all(&frame).await?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let outputs = process_next(stage, image, stream, deadline, bytes).await?;
        if outputs
            .iter()
            .any(|output| matches!(output, ActiveStageOutput::GraphicsUpdate(_)))
            && image.data() != before_touch
        {
            println!(
                "direct_touch: target={}x{}, access_screen_changed=true, response_ms={}",
                target.0,
                target.1,
                input_started.elapsed().as_millis()
            );
            return Ok(());
        }
    }
}

fn mark_coverage(
    coverage: &mut [bool],
    size: DesktopSize,
    rect: ironrdp_pdu::geometry::InclusiveRectangle,
) -> Result<()> {
    ensure!(
        rect.left <= rect.right
            && rect.top <= rect.bottom
            && rect.right < size.width
            && rect.bottom < size.height,
        "graphics update rectangle is outside the RDP canvas: {rect:?}"
    );
    let width = usize::from(size.width);
    for y in rect.top..=rect.bottom {
        let row = usize::from(y) * width;
        coverage[row + usize::from(rect.left)..=row + usize::from(rect.right)].fill(true);
    }
    Ok(())
}

async fn wait_for_full_screen(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    size: DesktopSize,
    bytes: &mut usize,
) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut coverage = vec![false; usize::from(size.width) * usize::from(size.height)];
    let mut default_pointer_seen = false;
    while coverage.iter().any(|pixel| !pixel) {
        for output in process_next(stage, image, stream, deadline, bytes).await? {
            match output {
                ActiveStageOutput::GraphicsUpdate(rect) => {
                    mark_coverage(&mut coverage, size, rect)?;
                }
                ActiveStageOutput::DeactivateAll => {
                    anyhow::bail!("server unexpectedly deactivated before the requested resize")
                }
                ActiveStageOutput::PointerDefault | ActiveStageOutput::PointerBitmap(_) => {
                    default_pointer_seen = true
                }
                _ => {}
            }
        }
    }
    Ok(default_pointer_seen)
}

fn display_control_ready(stage: &mut ActiveStage) -> bool {
    stage.display_control_ready() == Some(true)
}

async fn wait_for_display_control(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    bytes: &mut usize,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !display_control_ready(stage) {
        for output in process_next(stage, image, stream, deadline, bytes).await? {
            if matches!(output, ActiveStageOutput::DeactivateAll) {
                anyhow::bail!("server unexpectedly deactivated while opening Display Control")
            }
        }
    }
    Ok(())
}

async fn wait_for_rdpei(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    bytes: &mut usize,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while stage.rdpei_ready() != Some(true) {
        for output in process_next(stage, image, stream, deadline, bytes).await? {
            if matches!(output, ActiveStageOutput::DeactivateAll) {
                anyhow::bail!("server unexpectedly deactivated while opening RDPEI")
            }
        }
    }
    Ok(())
}

async fn wait_for_deactivate(
    stage: &mut ActiveStage,
    image: &mut DecodedImage,
    stream: &mut Stream,
    bytes: &mut usize,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        for output in process_next(stage, image, stream, deadline, bytes).await? {
            if matches!(output, ActiveStageOutput::DeactivateAll) {
                return Ok(());
            }
        }
    }
}

async fn reactivate(
    factory: &ConnectionActivationFactory,
    stage: &mut ActiveStage,
    stream: &mut Stream,
) -> Result<DesktopSize> {
    let sequence = async {
        let mut activation = factory.create();
        let mut output = WriteBuf::new();
        loop {
            let written =
                ironrdp_async::single_sequence_step_read(stream, &mut activation, &mut output)
                    .await?;
            if written.size().is_some() {
                stream.write_all(output.filled()).await?;
            }
            if let ConnectionActivationState::Finalized {
                desktop_size,
                share_id,
                enable_server_pointer,
                pointer_software_rendering,
                ..
            } = activation.connection_activation_state()
            {
                stage.set_fastpath_processor(
                    ironrdp_session::fast_path::ProcessorBuilder {
                        io_channel_id: activation.io_channel_id(),
                        user_channel_id: activation.user_channel_id(),
                        share_id,
                        enable_server_pointer,
                        pointer_software_rendering,
                    }
                    .build(),
                );
                stage.set_share_id(share_id);
                stage.set_enable_server_pointer(enable_server_pointer);
                return Ok::<_, anyhow::Error>(desktop_size);
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(15), sequence)
        .await
        .context("timed out during RDP Deactivation-Reactivation")?
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
    let (credentials, domain, autologon) = probe_credentials(args.authenticate)?;
    let mut connector = ClientConnector::new(
        client_config(
            &args.codec,
            args.desktop,
            credentials,
            domain,
            autologon,
            args.nla,
        )?,
        tcp.local_addr()?,
    );
    if args.resize.is_some() {
        connector.attach_static_channel(
            DrdynvcClient::new()
                .with_dynamic_channel(DisplayControlClient::new(|_| Ok(Vec::new())))
                .with_dynamic_channel(RdpeiClient::default()),
        );
    }
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
    let initial_size = connected.desktop_size;
    println!(
        "connected: requested_codec={}, desktop={}x{}, handshake_ms={}",
        args.codec,
        initial_size.width,
        initial_size.height,
        started.elapsed().as_millis()
    );
    if args.authenticate {
        ensure!(
            args.resize.is_none(),
            "--authenticate and --resize are separate probe modes"
        );
        return run_authenticated_wire_probe(
            &mut stream,
            initial_size,
            Duration::from_secs(args.post_auth_wait_seconds),
            args.takeover,
        )
        .await;
    }
    ensure!(
        args.post_auth_wait_seconds == 0,
        "--post-auth-wait-seconds requires --authenticate"
    );
    ensure!(!args.takeover, "--takeover requires --authenticate");
    if args.resize.is_none() {
        return run_wire_probe(&mut stream, initial_size).await;
    }
    let activation_factory = connected.activation_factory;
    let mut stage = ActiveStageBuilder {
        static_channels: connected.static_channels,
        user_channel_id: connected.user_channel_id,
        io_channel_id: connected.io_channel_id,
        message_channel_id: connected.message_channel_id,
        share_id: connected.share_id,
        compression_type: connected.compression_type,
        enable_server_pointer: connected.enable_server_pointer,
        pointer_software_rendering: connected.pointer_software_rendering,
    }
    .build();
    let mut image = DecodedImage::new(PixelFormat::RgbA32, initial_size.width, initial_size.height);
    let mut bytes = 0;
    let first_started = Instant::now();
    let mut default_pointer_seen = wait_for_full_screen(
        &mut stage,
        &mut image,
        &mut stream,
        initial_size,
        &mut bytes,
    )
    .await?;
    println!(
        "first_screen: size={}x{}, rdp_bytes={}, elapsed_ms={}",
        image.width(),
        image.height(),
        bytes,
        first_started.elapsed().as_millis()
    );

    if let Some(target) = args.resize {
        ensure!(
            target != initial_size,
            "resize target must differ from the initial size"
        );
        wait_for_display_control(&mut stage, &mut image, &mut stream, &mut bytes).await?;
        println!("display_control: ready");
        let resize_started = Instant::now();
        let resize_frame = stage
            .encode_resize(
                u32::from(target.width),
                u32::from(target.height),
                Some(100),
                None,
            )
            .context("Display Control channel was not ready")??;
        stream.write_all(&resize_frame).await?;
        wait_for_deactivate(&mut stage, &mut image, &mut stream, &mut bytes).await?;
        let activated_size = reactivate(&activation_factory, &mut stage, &mut stream).await?;
        ensure!(
            activated_size == target,
            "dynamic size was lost during graphics reactivation: requested {}x{}, finalized {}x{}",
            target.width,
            target.height,
            activated_size.width,
            activated_size.height
        );
        image = DecodedImage::new(PixelFormat::RgbA32, target.width, target.height);
        default_pointer_seen =
            wait_for_full_screen(&mut stage, &mut image, &mut stream, target, &mut bytes).await?;
        println!(
            "resize_screen: requested={}x{}, finalized={}x{}, image={}x{}, elapsed_ms={}",
            target.width,
            target.height,
            activated_size.width,
            activated_size.height,
            image.width(),
            image.height(),
            resize_started.elapsed().as_millis()
        );
    }

    let pointer_size = DesktopSize {
        width: image.width(),
        height: image.height(),
    };
    let pointer_position = probe_pointer(
        &mut stage,
        &mut image,
        &mut stream,
        pointer_size,
        &mut bytes,
        default_pointer_seen,
    )
    .await?;
    probe_relative_click(
        &mut stage,
        &mut image,
        &mut stream,
        pointer_size,
        &mut bytes,
        pointer_position,
    )
    .await?;
    wait_for_rdpei(&mut stage, &mut image, &mut stream, &mut bytes).await?;
    println!("rdpei: ready");
    probe_direct_touch(
        &mut stage,
        &mut image,
        &mut stream,
        pointer_size,
        &mut bytes,
    )
    .await?;

    // Empty credentials kept the connection at the SunRDP access screen. Tab
    // only changes its focus; no local account is authenticated by this probe.
    let tab = FastPathInput::new(vec![
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), 15),
        FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, 15),
    ])?;
    let input_started = Instant::now();
    let input = ironrdp_core::encode_vec(&tab)?;
    stream.write_all(&input).await?;
    let input_size = DesktopSize {
        width: image.width(),
        height: image.height(),
    };
    let mut input_coverage =
        vec![false; usize::from(input_size.width) * usize::from(input_size.height)];
    let deadline = Instant::now() + Duration::from_secs(10);
    'response: loop {
        for output in
            process_next(&mut stage, &mut image, &mut stream, deadline, &mut bytes).await?
        {
            if let ActiveStageOutput::GraphicsUpdate(rect) = output {
                mark_coverage(&mut input_coverage, input_size, rect)?;
                break 'response;
            }
        }
    }
    println!(
        "tab_first_response_ms={}",
        input_started.elapsed().as_millis()
    );
    Ok(())
}
