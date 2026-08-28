use std::ffi::c_void;
use std::mem::size_of;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, split};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::core::{BOOL, PCWSTR};

use super::{
    Handshake, InputCommand, encode_keyboard, encode_mouse, read_frame, read_handshake, read_input,
    write_frame, write_handshake,
};
use crate::config;
use crate::display::FrameHub;
use crate::platform::{DesktopCapture, DesktopSize, InputInjector};

const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\SunRemoteDesktop.SessionBridge.v1";
const PIPE_NAME_ENV: &str = "SUN_REMOTE_DESKTOP_PIPE";
const INPUT_QUEUE_LENGTH: usize = 512;
const RETRY_DELAY: Duration = Duration::from_secs(1);

pub struct ServiceBridgeBackend {
    pub size: DesktopSize,
    pub hub: FrameHub,
    pub injector: Arc<dyn InputInjector>,
}

struct BridgeInputInjector {
    sender: Mutex<Option<mpsc::Sender<Vec<u8>>>>,
}

impl BridgeInputInjector {
    fn new() -> Self {
        Self {
            sender: Mutex::new(None),
        }
    }

    fn attach(&self) -> mpsc::Receiver<Vec<u8>> {
        let (sender, receiver) = mpsc::channel(INPUT_QUEUE_LENGTH);
        *self.sender.lock().expect("bridge input mutex poisoned") = Some(sender);
        receiver
    }

    fn detach(&self) {
        *self.sender.lock().expect("bridge input mutex poisoned") = None;
    }

    fn send(&self, data: Vec<u8>) {
        let sender = self
            .sender
            .lock()
            .expect("bridge input mutex poisoned")
            .clone();
        if let Some(sender) = sender
            && let Err(error) = sender.try_send(data)
        {
            tracing::warn!(?error, "session-agent input queue is unavailable or full");
        }
    }
}

impl InputInjector for BridgeInputInjector {
    fn keyboard(&self, event: &ironrdp_server::KeyboardEvent) {
        if let Some(data) = encode_keyboard(event) {
            self.send(data);
        }
    }

    fn mouse(&self, event: &ironrdp_server::MouseEvent, _desktop: DesktopSize) {
        self.send(encode_mouse(event));
    }
}

pub async fn wait_for_service_backend() -> Result<ServiceBridgeBackend> {
    wait_for_service_backend_at(&pipe_name()).await
}

async fn wait_for_service_backend_at(pipe_name: &str) -> Result<ServiceBridgeBackend> {
    tracing::info!(pipe = pipe_name, "waiting for a Windows session agent");
    let (pipe, handshake) = accept_agent(pipe_name, None).await;
    let size = handshake.size;
    let hub = FrameHub::new(size);
    let bridge_injector = Arc::new(BridgeInputInjector::new());
    let injector: Arc<dyn InputInjector> = bridge_injector.clone();
    let background_hub = hub.clone();
    let background_pipe_name = pipe_name.to_string();
    tokio::spawn(async move {
        serve_agents(
            background_pipe_name,
            pipe,
            size,
            background_hub,
            bridge_injector,
        )
        .await;
    });
    Ok(ServiceBridgeBackend {
        size,
        hub,
        injector,
    })
}

async fn serve_agents(
    pipe_name: String,
    mut pipe: NamedPipeServer,
    size: DesktopSize,
    hub: FrameHub,
    injector: Arc<BridgeInputInjector>,
) {
    loop {
        tracing::info!(
            width = size.width,
            height = size.height,
            "Windows session agent connected"
        );
        if let Err(error) = serve_agent(&mut pipe, size, &hub, &injector).await {
            tracing::warn!(?error, "Windows session agent disconnected");
        }
        injector.detach();
        drop(pipe);
        let (next_pipe, _) = accept_agent(&pipe_name, Some(size)).await;
        pipe = next_pipe;
    }
}

async fn accept_agent(
    pipe_name: &str,
    expected_size: Option<DesktopSize>,
) -> (NamedPipeServer, Handshake) {
    loop {
        let result = async {
            let mut pipe = create_pipe_server(pipe_name)?;
            pipe.connect().await.context("accept session-agent pipe")?;
            let handshake = read_handshake(&mut pipe).await?;
            if let Some(expected_size) = expected_size {
                anyhow::ensure!(
                    handshake.size == expected_size,
                    "agent desktop changed from {}x{} to {}x{}; restart the service to renegotiate the RDP desktop",
                    expected_size.width,
                    expected_size.height,
                    handshake.size.width,
                    handshake.size.height
                );
            }
            Ok::<_, anyhow::Error>((pipe, handshake))
        }
        .await;

        match result {
            Ok(connection) => return connection,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    pipe = pipe_name,
                    "unable to accept session agent; retrying"
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    }
}

async fn serve_agent(
    pipe: &mut NamedPipeServer,
    size: DesktopSize,
    hub: &FrameHub,
    injector: &BridgeInputInjector,
) -> Result<()> {
    let (mut reader, mut writer) = split(pipe);
    let mut input_receiver = injector.attach();

    let receive_frames = async {
        loop {
            let frame = read_frame(&mut reader).await?;
            anyhow::ensure!(
                frame.width == size.width && frame.height == size.height,
                "agent sent a frame whose size differs from its handshake"
            );
            hub.publish(frame);
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    let send_input = async {
        while let Some(data) = input_receiver.recv().await {
            writer
                .write_all(&data)
                .await
                .context("write input event to session agent")?;
        }
        anyhow::bail!("session-agent input channel closed")
    };

    tokio::select! {
        result = receive_frames => result,
        result = send_input => result,
    }
}

pub async fn run_agent(config_path: &Path) -> Result<()> {
    let settings = config::load_from(config_path)?;
    let capture = crate::platform::windows::WindowsDesktopCapture::new(settings.fps)
        .context("initialize Windows desktop capture in the session agent")?;
    let size = capture.size();
    let hub = FrameHub::new(size);
    let capture_hub = hub.clone();
    Box::new(capture).start(Box::new(move |frame| capture_hub.publish(frame)))?;
    let injector: Arc<dyn InputInjector> =
        Arc::new(crate::platform::windows::WindowsInputInjector::new());
    let pipe_name = pipe_name();

    tracing::info!(
        pipe = pipe_name,
        width = size.width,
        height = size.height,
        "Windows session agent started"
    );
    loop {
        match ClientOptions::new().open(&pipe_name) {
            Ok(mut pipe) => {
                if let Err(error) = write_handshake(&mut pipe, size).await {
                    tracing::warn!(?error, "unable to initialize the service bridge");
                } else {
                    tracing::info!("session agent connected to the system service");
                    if let Err(error) =
                        exchange_with_service(pipe, size, &hub, injector.as_ref()).await
                    {
                        tracing::warn!(?error, "session-agent bridge disconnected");
                    }
                }
            }
            Err(error) => {
                tracing::debug!(
                    ?error,
                    pipe = pipe_name,
                    "system service bridge is not ready"
                );
            }
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

async fn exchange_with_service(
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
    size: DesktopSize,
    hub: &FrameHub,
    injector: &dyn InputInjector,
) -> Result<()> {
    let (mut reader, mut writer) = split(pipe);
    let mut frames = hub.subscribe();

    let send_frames = async {
        loop {
            let frame = frames
                .borrow_and_update()
                .as_ref()
                .cloned()
                .context("session-agent frame is unavailable")?;
            anyhow::ensure!(
                frame.width == size.width && frame.height == size.height,
                "captured desktop size changed; restart the session agent"
            );
            write_frame(&mut writer, &frame).await?;
            frames
                .changed()
                .await
                .context("desktop capture stream closed")?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    let receive_input = async {
        loop {
            match read_input(&mut reader).await? {
                InputCommand::Keyboard(event) => injector.keyboard(&event),
                InputCommand::Mouse(event) => injector.mouse(&event, size),
            }
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };

    tokio::select! {
        result = send_frames => result,
        result = receive_input => result,
    }
}

fn pipe_name() -> String {
    std::env::var(PIPE_NAME_ENV).unwrap_or_else(|_| DEFAULT_PIPE_NAME.to_string())
}

fn create_pipe_server(pipe_name: &str) -> Result<NamedPipeServer> {
    // LocalSystem and administrators retain full access. Only a local interactive
    // user may attach as the per-session agent; remote named-pipe clients are also
    // rejected by ServerOptions.
    let sddl: Vec<u16> = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
    }
    .context("build the session bridge security descriptor")?;

    let mut attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: BOOL(0),
    };
    let mut options = ServerOptions::new();
    options
        .first_pipe_instance(true)
        .max_instances(1)
        .reject_remote_clients(true);
    let result = unsafe {
        options.create_with_security_attributes_raw(
            pipe_name,
            &mut attributes as *mut SECURITY_ATTRIBUTES as *mut c_void,
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    result.with_context(|| format!("create protected session bridge {pipe_name}"))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;
    use ironrdp_server::KeyboardEvent;

    static PIPE_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[tokio::test]
    async fn service_bridge_moves_frames_and_input_in_opposite_directions() {
        let pipe_name = format!(
            r"\\.\pipe\SunRemoteDesktop.Test.{}.{}",
            std::process::id(),
            PIPE_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let service_pipe = pipe_name.clone();
        let service = tokio::spawn(async move { wait_for_service_backend_at(&service_pipe).await });

        let mut client = loop {
            match ClientOptions::new().open(&pipe_name) {
                Ok(client) => break client,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        let size = DesktopSize {
            width: 4,
            height: 2,
        };
        write_handshake(&mut client, size).await.unwrap();
        let backend = service.await.unwrap().unwrap();
        let mut frames = backend.hub.subscribe();
        let (mut reader, mut writer) = split(client);

        let expected_pixels: Vec<u8> = (0..32).collect();
        write_frame(
            &mut writer,
            &crate::platform::CapturedFrame {
                width: size.width,
                height: size.height,
                rgba: expected_pixels.clone(),
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), frames.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frames.borrow_and_update().as_ref().unwrap().rgba,
            expected_pixels
        );

        backend.injector.keyboard(&KeyboardEvent::Pressed {
            code: 30,
            extended: false,
        });
        let input = tokio::time::timeout(Duration::from_secs(2), read_input(&mut reader))
            .await
            .unwrap()
            .unwrap();
        match input {
            InputCommand::Keyboard(KeyboardEvent::Pressed { code, extended }) => {
                assert_eq!(code, 30);
                assert!(!extended);
            }
            _ => panic!("unexpected bridged input event"),
        }

        drop(reader);
        drop(writer);
        let mut reconnected_client = loop {
            match ClientOptions::new().open(&pipe_name) {
                Ok(client) => break client,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        write_handshake(&mut reconnected_client, size)
            .await
            .unwrap();
        let (_reader, mut writer) = split(reconnected_client);
        let reconnected_pixels = vec![91; 32];
        write_frame(
            &mut writer,
            &crate::platform::CapturedFrame {
                width: size.width,
                height: size.height,
                rgba: reconnected_pixels.clone(),
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), frames.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frames.borrow_and_update().as_ref().unwrap().rgba,
            reconnected_pixels
        );
    }
}
