use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver as StdReceiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncWriteExt, split};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HLOCAL, LocalFree, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DuplicateTokenEx, GetTokenInformation, IsWellKnownSid, PSECURITY_DESCRIPTOR,
    SECURITY_ATTRIBUTES, SecurityImpersonation, SetTokenInformation, TOKEN_ALL_ACCESS,
    TOKEN_DUPLICATE, TOKEN_QUERY, TOKEN_USER, TokenPrimary, TokenSessionId, TokenUser,
    WinLocalSystemSid,
};
use windows::Win32::System::Pipes::{GetNamedPipeClientProcessId, GetNamedPipeClientSessionId};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, DESKTOP_READOBJECTS, GetUserObjectInformationW, OpenInputDesktop, UOI_NAME,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CreateProcessAsUserW, GetCurrentProcess, GetExitCodeProcess, OpenProcess,
    OpenProcessToken, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
    TerminateProcess, WaitForSingleObject,
};
use windows::core::{BOOL, PCWSTR, PWSTR};

use super::{
    Handshake, InputCommand, encode_display_size, encode_keyboard, encode_mouse, read_frame,
    read_handshake, read_input, write_frame, write_handshake,
};
use crate::config;
use crate::display::FrameHub;
use crate::platform::{DesktopCapture, DesktopSize, InputInjector};

const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\SunRemoteDesktop.SessionBridge.v1";
const PIPE_NAME_ENV: &str = "SUN_REMOTE_DESKTOP_PIPE";
const INPUT_QUEUE_LENGTH: usize = 512;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const WAITING_DESKTOP_SIZE: DesktopSize = DesktopSize {
    width: 1280,
    height: 720,
};
const NO_ACTIVE_CONSOLE_SESSION: u32 = u32::MAX;
const CONSOLE_AGENT_POLL: Duration = Duration::from_millis(250);
const CONSOLE_AGENT_HANDOFF_GRACE: Duration = Duration::from_secs(3);
const EXIT_SWITCH_DEFAULT: u32 = 240;
const EXIT_SWITCH_WINLOGON: u32 = 241;
const EXIT_SWITCH_SCREENSAVER: u32 = 242;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsoleDesktop {
    Default,
    Winlogon,
    ScreenSaver,
}

impl ConsoleDesktop {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "winlogon" => Ok(Self::Winlogon),
            "screensaver" | "screen-saver" => Ok(Self::ScreenSaver),
            _ => anyhow::bail!("unsupported Windows console desktop: {value}"),
        }
    }

    fn argument(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Winlogon => "winlogon",
            Self::ScreenSaver => "screensaver",
        }
    }

    fn object_name(self) -> &'static str {
        match self {
            Self::Default => r"winsta0\default",
            Self::Winlogon => r"winsta0\winlogon",
            Self::ScreenSaver => r"winsta0\screensaver",
        }
    }

    fn switch_exit_code(self) -> u32 {
        match self {
            Self::Default => EXIT_SWITCH_DEFAULT,
            Self::Winlogon => EXIT_SWITCH_WINLOGON,
            Self::ScreenSaver => EXIT_SWITCH_SCREENSAVER,
        }
    }

    fn from_switch_exit_code(code: u32) -> Option<Self> {
        match code {
            EXIT_SWITCH_DEFAULT => Some(Self::Default),
            EXIT_SWITCH_WINLOGON => Some(Self::Winlogon),
            EXIT_SWITCH_SCREENSAVER => Some(Self::ScreenSaver),
            _ => None,
        }
    }
}

struct OwnedWinHandle(HANDLE);

impl Drop for OwnedWinHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

struct ConsoleAgentProcess {
    handle: OwnedWinHandle,
    process_id: u32,
    session_id: u32,
    desktop: ConsoleDesktop,
}

#[derive(Debug, Clone, Copy)]
struct AgentIdentity {
    process_id: u32,
    session_id: u32,
}

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

    fn send(&self, data: Vec<u8>) -> Result<()> {
        let sender = self
            .sender
            .lock()
            .expect("bridge input mutex poisoned")
            .clone()
            .context("Windows session agent is not connected")?;
        sender
            .try_send(data)
            .context("session-agent input queue is unavailable or full")
    }
}

impl InputInjector for BridgeInputInjector {
    fn keyboard(&self, event: &ironrdp_server::KeyboardEvent) {
        if let Some(data) = encode_keyboard(event)
            && let Err(error) = self.send(data)
        {
            tracing::warn!(
                ?error,
                "unable to forward keyboard input to the session agent"
            );
        }
    }

    fn mouse(&self, event: &ironrdp_server::MouseEvent, _desktop: DesktopSize) {
        let data = encode_mouse(event);
        if !data.is_empty()
            && let Err(error) = self.send(data)
        {
            tracing::warn!(?error, "unable to forward mouse input to the session agent");
        }
    }

    fn set_display_size(&self, size: DesktopSize) -> Result<()> {
        self.send(encode_display_size(size))
    }
}

pub async fn wait_for_service_backend() -> Result<ServiceBridgeBackend> {
    wait_for_service_backend_at(&pipe_name()).await
}

async fn wait_for_service_backend_at(pipe_name: &str) -> Result<ServiceBridgeBackend> {
    wait_for_service_backend_with_policy(pipe_name, true).await
}

async fn wait_for_service_backend_with_policy(
    pipe_name: &str,
    require_active_console: bool,
) -> Result<ServiceBridgeBackend> {
    tracing::info!(
        pipe = pipe_name,
        "starting the Windows session-agent bridge"
    );
    let size = WAITING_DESKTOP_SIZE;
    let hub = FrameHub::new_unavailable(size);
    let bridge_injector = Arc::new(BridgeInputInjector::new());
    let injector: Arc<dyn InputInjector> = bridge_injector.clone();
    let background_hub = hub.clone();
    let background_pipe_name = pipe_name.to_string();
    tokio::spawn(async move {
        serve_agents(
            background_pipe_name,
            background_hub,
            bridge_injector,
            require_active_console,
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
    hub: FrameHub,
    injector: Arc<BridgeInputInjector>,
    require_active_console: bool,
) {
    loop {
        let (mut pipe, handshake, identity) =
            accept_agent(&pipe_name, require_active_console).await;
        tracing::info!(
            process_id = identity.process_id,
            session_id = identity.session_id,
            protocol_version = handshake.version,
            width = handshake.size.width,
            height = handshake.size.height,
            "Windows session agent connected"
        );
        if let Err(error) = serve_agent(
            &mut pipe,
            &hub,
            &injector,
            identity.session_id,
            require_active_console,
        )
        .await
        {
            tracing::warn!(?error, "Windows session agent disconnected");
        }
        injector.detach();
        // A Windows PIN/password sign-in switches the physical input desktop
        // from winlogon to default. Keep the authenticated RDP presentation and
        // latest frame while the service-managed replacement agent starts.
        // The first replacement frame cancels this delayed transition.
        hub.set_unavailable_after(CONSOLE_AGENT_HANDOFF_GRACE);
    }
}

async fn accept_agent(
    pipe_name: &str,
    require_active_console: bool,
) -> (NamedPipeServer, Handshake, AgentIdentity) {
    loop {
        let result = async {
            let mut pipe = create_pipe_server(pipe_name, require_active_console)?;
            pipe.connect().await.context("accept session-agent pipe")?;
            let identity = pipe_client_identity(&pipe)?;
            if require_active_console {
                ensure_active_console_session(identity.session_id)?;
                ensure_local_system_process(identity.process_id)?;
            }
            let handshake = read_handshake(&mut pipe).await?;
            Ok::<_, anyhow::Error>((pipe, handshake, identity))
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
    hub: &FrameHub,
    injector: &BridgeInputInjector,
    agent_session_id: u32,
    require_active_console: bool,
) -> Result<()> {
    let (mut reader, mut writer) = split(pipe);
    let mut input_receiver = injector.attach();

    let receive_frames = async {
        loop {
            let frame = read_frame(&mut reader).await?;
            if require_active_console {
                ensure_active_console_session(agent_session_id)
                    .context("physical console session changed")?;
            }
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
    let agent_session_id = current_process_session_id()?;
    ensure_active_console_session(agent_session_id)
        .context("SunRDP only captures the physical Windows console session")?;
    let settings = config::load_from(config_path)?;
    let capture = loop {
        match crate::platform::windows::WindowsDesktopCapture::new(settings.fps) {
            Ok(capture) => break capture,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    "interactive desktop is unavailable; session agent will retry"
                );
                tokio::time::sleep(RETRY_DELAY).await;
            }
        }
    };
    let size = capture.size();
    let hub = FrameHub::new(size);
    let capture_hub = hub.clone();
    Box::new(capture).start(Box::new(move |frame| capture_hub.publish(frame)))?;
    let injector: Arc<dyn InputInjector> =
        Arc::new(crate::platform::windows::WindowsInputInjector::new());
    let pipe_name = pipe_name();

    tracing::info!(
        session_id = agent_session_id,
        pipe = pipe_name,
        width = size.width,
        height = size.height,
        "Windows session agent started"
    );
    loop {
        match ClientOptions::new().open(&pipe_name) {
            Ok(mut pipe) => {
                let current_size = hub.size();
                if let Err(error) = write_handshake(&mut pipe, current_size).await {
                    tracing::warn!(?error, "unable to initialize the service bridge");
                } else {
                    tracing::info!("session agent connected to the system service");
                    if let Err(error) = exchange_with_service(pipe, &hub, injector.as_ref()).await {
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

pub async fn run_console_agent(config_path: &Path, desktop: ConsoleDesktop) -> Result<()> {
    anyhow::ensure!(
        current_process_is_local_system()?,
        "the service-managed console agent must run as LocalSystem"
    );
    let actual_desktop = active_input_desktop()?;
    if actual_desktop != desktop {
        std::process::exit(actual_desktop.switch_exit_code() as i32);
    }

    std::thread::Builder::new()
        .name("sunrdp-input-desktop-watch".to_string())
        .spawn(move || {
            loop {
                std::thread::sleep(CONSOLE_AGENT_POLL);
                match active_input_desktop() {
                    Ok(actual) if actual != desktop => {
                        std::process::exit(actual.switch_exit_code() as i32);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::debug!(?error, "unable to query the active input desktop");
                    }
                }
            }
        })
        .context("start the Windows input-desktop watcher")?;

    tracing::info!(
        desktop = desktop.argument(),
        "service-managed physical console agent selected the input desktop"
    );
    run_agent(config_path).await
}

pub fn supervise_console_agent(config_path: PathBuf, shutdown: StdReceiver<()>) {
    let executable = match std::env::current_exe().context("locate the SunRDP service binary") {
        Ok(path) => path,
        Err(error) => {
            tracing::error!(
                ?error,
                "unable to start the physical console agent supervisor"
            );
            return;
        }
    };
    let mut desktop = ConsoleDesktop::Default;

    loop {
        match shutdown.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => return,
            Err(TryRecvError::Empty) => {}
        }

        let session_id = match active_console_session_id() {
            Ok(session_id) => session_id,
            Err(error) => {
                tracing::debug!(?error, "physical console session is not ready");
                if shutdown.recv_timeout(RETRY_DELAY).is_ok() {
                    return;
                }
                continue;
            }
        };
        let child = match spawn_console_agent(&executable, &config_path, session_id, desktop) {
            Ok(child) => child,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    session_id,
                    desktop = desktop.argument(),
                    "unable to start the service-managed console agent"
                );
                if shutdown.recv_timeout(RETRY_DELAY).is_ok() {
                    return;
                }
                continue;
            }
        };
        tracing::info!(
            process_id = child.process_id,
            session_id = child.session_id,
            desktop = child.desktop.argument(),
            "service-managed physical console agent started"
        );

        loop {
            match shutdown.try_recv() {
                Ok(()) | Err(TryRecvError::Disconnected) => {
                    terminate_console_agent(&child);
                    return;
                }
                Err(TryRecvError::Empty) => {}
            }

            if active_console_session_id().ok() != Some(child.session_id) {
                tracing::info!(
                    process_id = child.process_id,
                    session_id = child.session_id,
                    "physical console session changed; replacing its agent"
                );
                terminate_console_agent(&child);
                break;
            }

            match unsafe {
                WaitForSingleObject(child.handle.0, CONSOLE_AGENT_POLL.as_millis() as u32)
            } {
                WAIT_TIMEOUT => {}
                WAIT_OBJECT_0 => {
                    let mut exit_code = 0;
                    if let Err(error) =
                        unsafe { GetExitCodeProcess(child.handle.0, &mut exit_code) }
                    {
                        tracing::warn!(?error, "unable to read console-agent exit status");
                    }
                    if let Some(next_desktop) = ConsoleDesktop::from_switch_exit_code(exit_code) {
                        tracing::info!(
                            previous_desktop = desktop.argument(),
                            next_desktop = next_desktop.argument(),
                            "physical input desktop changed"
                        );
                        desktop = next_desktop;
                    } else {
                        tracing::warn!(
                            process_id = child.process_id,
                            exit_code,
                            "physical console agent exited; restarting"
                        );
                        if shutdown.recv_timeout(RETRY_DELAY).is_ok() {
                            return;
                        }
                    }
                    break;
                }
                WAIT_FAILED => {
                    tracing::warn!(
                        process_id = child.process_id,
                        "wait for console agent failed"
                    );
                    terminate_console_agent(&child);
                    break;
                }
                status => {
                    tracing::warn!(
                        process_id = child.process_id,
                        status = status.0,
                        "unexpected console-agent wait status"
                    );
                    terminate_console_agent(&child);
                    break;
                }
            }
        }
    }
}

fn spawn_console_agent(
    executable: &Path,
    config_path: &Path,
    session_id: u32,
    desktop: ConsoleDesktop,
) -> Result<ConsoleAgentProcess> {
    let token = system_primary_token_for_session(session_id)?;
    let executable_text = executable
        .to_str()
        .context("the service executable path is not valid Unicode")?;
    let config_text = config_path
        .to_str()
        .context("the service configuration path is not valid Unicode")?;
    let command = format!(
        "\"{executable_text}\" console-agent --desktop {} --config \"{config_text}\"",
        desktop.argument()
    );
    let mut command = wide(&command);
    let executable_wide = wide(executable_text);
    let mut desktop_wide = wide(desktop.object_name());
    let current_directory = executable
        .parent()
        .and_then(Path::to_str)
        .context("the service installation directory is not valid Unicode")?;
    let current_directory = wide(current_directory);
    let startup = STARTUPINFOW {
        cb: size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop_wide.as_mut_ptr()),
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessAsUserW(
            Some(token.0),
            PCWSTR(executable_wide.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            PCWSTR(current_directory.as_ptr()),
            &startup,
            &mut process,
        )
    }
    .with_context(|| {
        format!(
            "start the physical console agent in Windows session {session_id} on {}",
            desktop.object_name()
        )
    })?;
    unsafe {
        let _ = CloseHandle(process.hThread);
    }
    Ok(ConsoleAgentProcess {
        handle: OwnedWinHandle(process.hProcess),
        process_id: process.dwProcessId,
        session_id,
        desktop,
    })
}

fn system_primary_token_for_session(session_id: u32) -> Result<OwnedWinHandle> {
    let mut current_token = HANDLE::default();
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut current_token,
        )
    }
    .context("open the SunRDP service process token")?;
    let current_token = OwnedWinHandle(current_token);
    let mut primary_token = HANDLE::default();
    unsafe {
        DuplicateTokenEx(
            current_token.0,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary_token,
        )
    }
    .context("duplicate the LocalSystem service token")?;
    let primary_token = OwnedWinHandle(primary_token);
    unsafe {
        SetTokenInformation(
            primary_token.0,
            TokenSessionId,
            (&session_id as *const u32).cast(),
            size_of::<u32>() as u32,
        )
    }
    .with_context(|| format!("assign the console agent token to Windows session {session_id}"))?;
    Ok(primary_token)
}

fn terminate_console_agent(child: &ConsoleAgentProcess) {
    if let Err(error) = unsafe { TerminateProcess(child.handle.0, 1) } {
        tracing::debug!(
            ?error,
            process_id = child.process_id,
            "console agent already exited"
        );
    }
    unsafe {
        let _ = WaitForSingleObject(child.handle.0, 5_000);
    }
}

fn active_input_desktop() -> Result<ConsoleDesktop> {
    let desktop = unsafe { OpenInputDesktop(Default::default(), false, DESKTOP_READOBJECTS) }
        .context("open the Windows input desktop")?;
    let result = input_desktop_name(desktop.0).and_then(|name| ConsoleDesktop::parse(&name));
    unsafe {
        let _ = CloseDesktop(desktop);
    }
    result
}

fn input_desktop_name(desktop: *mut c_void) -> Result<String> {
    let mut required = 0;
    let _ = unsafe {
        GetUserObjectInformationW(HANDLE(desktop), UOI_NAME, None, 0, Some(&mut required))
    };
    anyhow::ensure!(
        required >= 2,
        "Windows returned an empty input desktop name"
    );
    let mut buffer = vec![0u16; required.div_ceil(2) as usize];
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desktop),
            UOI_NAME,
            Some(buffer.as_mut_ptr().cast()),
            required,
            Some(&mut required),
        )
    }
    .context("query the Windows input desktop name")?;
    let length = buffer
        .iter()
        .position(|value| *value == 0)
        .unwrap_or(buffer.len());
    Ok(String::from_utf16_lossy(&buffer[..length]))
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn current_process_session_id() -> Result<u32> {
    let mut session_id = 0;
    unsafe { ProcessIdToSessionId(std::process::id(), &mut session_id) }
        .context("query the session agent Windows session")?;
    Ok(session_id)
}

fn active_console_session_id() -> Result<u32> {
    let session_id = unsafe { WTSGetActiveConsoleSessionId() };
    anyhow::ensure!(
        session_id != NO_ACTIVE_CONSOLE_SESSION,
        "Windows has no active physical console session"
    );
    Ok(session_id)
}

fn ensure_active_console_session(agent_session_id: u32) -> Result<()> {
    let console_session_id = active_console_session_id()?;
    anyhow::ensure!(
        is_active_console_session(agent_session_id, console_session_id),
        "rejecting session agent from Windows session {agent_session_id}; active physical console session is {console_session_id}"
    );
    Ok(())
}

fn is_active_console_session(agent_session_id: u32, console_session_id: u32) -> bool {
    console_session_id != NO_ACTIVE_CONSOLE_SESSION && agent_session_id == console_session_id
}

fn pipe_client_identity(pipe: &NamedPipeServer) -> Result<AgentIdentity> {
    let handle = HANDLE(pipe.as_raw_handle());
    let mut process_id = 0;
    let mut session_id = 0;
    unsafe {
        GetNamedPipeClientProcessId(handle, &mut process_id)
            .context("query session-agent process id")?;
        GetNamedPipeClientSessionId(handle, &mut session_id)
            .context("query session-agent Windows session")?;
    }
    Ok(AgentIdentity {
        process_id,
        session_id,
    })
}

fn ensure_local_system_process(process_id: u32) -> Result<()> {
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }
        .context("open the session-agent process")?;
    let process = OwnedWinHandle(process);
    anyhow::ensure!(
        process_is_local_system(process.0)?,
        "rejecting session agent process {process_id}; the service-managed console agent must run as LocalSystem"
    );
    Ok(())
}

fn current_process_is_local_system() -> Result<bool> {
    process_is_local_system(unsafe { GetCurrentProcess() })
}

fn process_is_local_system(process: HANDLE) -> Result<bool> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }
        .context("open process token for LocalSystem check")?;
    let token = OwnedWinHandle(token);

    let mut required = 0;
    let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut required) };
    anyhow::ensure!(
        required >= size_of::<TOKEN_USER>() as u32,
        "invalid token user size"
    );
    let mut buffer = vec![0u8; required as usize];
    unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required,
            &mut required,
        )
    }
    .context("query process token user")?;
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };
    Ok(unsafe { IsWellKnownSid(token_user.User.Sid, WinLocalSystemSid) }.as_bool())
}

async fn exchange_with_service(
    pipe: tokio::net::windows::named_pipe::NamedPipeClient,
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
                InputCommand::Mouse(event) => injector.mouse(&event, hub.size()),
                InputCommand::SetDisplaySize(size) => injector.set_display_size(size)?,
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

fn create_pipe_server(pipe_name: &str, system_agent_only: bool) -> Result<NamedPipeServer> {
    // The installed service accepts only its LocalSystem console helper. Tests
    // opt into an interactive-user ACL while still rejecting remote pipe clients.
    let sddl = if system_agent_only {
        "D:P(A;;GA;;;SY)"
    } else {
        "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;IU)"
    };
    let sddl: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
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
        let service = tokio::spawn(async move {
            wait_for_service_backend_with_policy(&service_pipe, false).await
        });

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
        assert!(!backend.hub.is_available());
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
        assert!(backend.hub.is_available());
        assert_eq!(
            frames.borrow_and_update().as_ref().unwrap().rgba,
            expected_pixels
        );

        let resized = DesktopSize {
            width: 6,
            height: 3,
        };
        let resized_pixels = vec![37; 72];
        write_frame(
            &mut writer,
            &crate::platform::CapturedFrame {
                width: resized.width,
                height: resized.height,
                rgba: resized_pixels.clone(),
            },
        )
        .await
        .unwrap();
        tokio::time::timeout(Duration::from_secs(2), frames.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(backend.hub.size(), resized);
        assert_eq!(
            frames.borrow_and_update().as_ref().unwrap().rgba,
            resized_pixels
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
        write_handshake(&mut reconnected_client, resized)
            .await
            .unwrap();
        let (_reader, mut writer) = split(reconnected_client);
        let reconnected_pixels = vec![91; 72];
        write_frame(
            &mut writer,
            &crate::platform::CapturedFrame {
                width: resized.width,
                height: resized.height,
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

    #[test]
    fn active_console_policy_rejects_other_windows_sessions() {
        assert!(is_active_console_session(2, 2));
        assert!(!is_active_console_session(1, 2));
        assert!(!is_active_console_session(2, NO_ACTIVE_CONSOLE_SESSION));
    }

    #[test]
    fn console_desktop_switch_codes_round_trip() {
        for desktop in [
            ConsoleDesktop::Default,
            ConsoleDesktop::Winlogon,
            ConsoleDesktop::ScreenSaver,
        ] {
            assert_eq!(
                ConsoleDesktop::from_switch_exit_code(desktop.switch_exit_code()),
                Some(desktop)
            );
            assert_eq!(ConsoleDesktop::parse(desktop.argument()).unwrap(), desktop);
        }
        assert!(ConsoleDesktop::parse("remote").is_err());
    }
}
