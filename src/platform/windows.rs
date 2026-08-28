use std::mem::size_of;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT, LogonUserW};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP,
    KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEINPUT,
    SendInput,
};

use crate::platform::{CapturedFrame, DesktopCapture, DesktopSize, InputInjector};

pub struct WindowsDesktopCapture {
    size: DesktopSize,
    fps: u32,
}

impl WindowsDesktopCapture {
    pub fn new(fps: u32) -> Result<Self> {
        let monitor = xcap::Monitor::all()?
            .into_iter()
            .find(|monitor| monitor.is_primary().unwrap_or(false))
            .context("find the primary monitor")?;
        let width =
            u16::try_from(monitor.width()?).context("primary monitor is wider than RDP allows")?;
        let height = u16::try_from(monitor.height()?)
            .context("primary monitor is taller than RDP allows")?;
        anyhow::ensure!(
            width > 0 && height > 0,
            "primary monitor has no usable size"
        );
        Ok(Self {
            size: DesktopSize { width, height },
            fps: fps.clamp(1, 120),
        })
    }
}

impl DesktopCapture for WindowsDesktopCapture {
    fn size(&self) -> DesktopSize {
        self.size
    }

    fn start(self: Box<Self>, publish: Box<dyn Fn(CapturedFrame) + Send>) -> Result<()> {
        let fps = self.fps;
        thread::Builder::new()
            .name("rdp-desktop-capture".to_string())
            .spawn(move || {
                let monitor = match xcap::Monitor::all() {
                    Ok(monitors) => match monitors
                        .into_iter()
                        .find(|monitor| monitor.is_primary().unwrap_or(false))
                    {
                        Some(monitor) => monitor,
                        None => {
                            tracing::error!("unable to find the Windows primary monitor");
                            return;
                        }
                    },
                    Err(error) => {
                        tracing::error!(?error, "unable to enumerate Windows monitors");
                        return;
                    }
                };
                let frame_period = Duration::from_millis((1000 / fps.max(1)) as u64);
                loop {
                    let started = std::time::Instant::now();
                    match monitor.capture_image() {
                        Ok(image) => {
                            let width = image.width();
                            let height = image.height();
                            if let (Ok(width), Ok(height)) =
                                (u16::try_from(width), u16::try_from(height))
                            {
                                publish(CapturedFrame {
                                    width,
                                    height,
                                    rgba: image.into_raw(),
                                });
                            }
                        }
                        Err(error) => tracing::warn!(?error, "desktop capture failed; retrying"),
                    }
                    let elapsed = started.elapsed();
                    if elapsed < frame_period {
                        thread::sleep(frame_period - elapsed);
                    }
                }
            })
            .context("start desktop capture thread")?;
        Ok(())
    }
}

#[derive(Default)]
pub struct WindowsInputInjector;

impl WindowsInputInjector {
    pub fn new() -> Self {
        Self
    }
}

impl InputInjector for WindowsInputInjector {
    fn keyboard(&self, event: &ironrdp_server::KeyboardEvent) {
        let (scan, flags) = match event {
            ironrdp_server::KeyboardEvent::Pressed { code, extended } => {
                let mut flags = KEYEVENTF_SCANCODE;
                if *extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                (*code as u16, flags)
            }
            ironrdp_server::KeyboardEvent::Released { code, extended } => {
                let mut flags = KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP;
                if *extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                (*code as u16, flags)
            }
            ironrdp_server::KeyboardEvent::UnicodePressed(code) => (*code, KEYEVENTF_UNICODE),
            ironrdp_server::KeyboardEvent::UnicodeReleased(code) => {
                (*code, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP)
            }
            ironrdp_server::KeyboardEvent::Synchronize(_) => return,
        };

        let input = INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: Default::default(),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            let _ = SendInput(&[input], size_of::<INPUT>() as i32);
        }
    }

    fn mouse(&self, event: &ironrdp_server::MouseEvent, desktop: DesktopSize) {
        let (dx, dy, mouse_data, flags) = match event {
            ironrdp_server::MouseEvent::Move { x, y } => {
                let max_x = i32::from(desktop.width.saturating_sub(1)).max(1);
                let max_y = i32::from(desktop.height.saturating_sub(1)).max(1);
                let absolute_x = (i32::from(*x).clamp(0, max_x) * 65535) / max_x;
                let absolute_y = (i32::from(*y).clamp(0, max_y) * 65535) / max_y;
                (
                    absolute_x,
                    absolute_y,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                )
            }
            ironrdp_server::MouseEvent::RelMove { x, y } => (*x, *y, 0, MOUSEEVENTF_MOVE),
            ironrdp_server::MouseEvent::LeftPressed => (0, 0, 0, MOUSEEVENTF_LEFTDOWN),
            ironrdp_server::MouseEvent::LeftReleased => (0, 0, 0, MOUSEEVENTF_LEFTUP),
            ironrdp_server::MouseEvent::RightPressed => (0, 0, 0, MOUSEEVENTF_RIGHTDOWN),
            ironrdp_server::MouseEvent::RightReleased => (0, 0, 0, MOUSEEVENTF_RIGHTUP),
            ironrdp_server::MouseEvent::MiddlePressed => (0, 0, 0, MOUSEEVENTF_MIDDLEDOWN),
            ironrdp_server::MouseEvent::MiddleReleased => (0, 0, 0, MOUSEEVENTF_MIDDLEUP),
            ironrdp_server::MouseEvent::VerticalScroll { value } => {
                (0, 0, wheel_data(i32::from(*value)), MOUSEEVENTF_WHEEL)
            }
            ironrdp_server::MouseEvent::Scroll { x, y } => {
                if *y != 0 {
                    (0, 0, wheel_data(*y), MOUSEEVENTF_WHEEL)
                } else {
                    (0, 0, wheel_data(*x), MOUSEEVENTF_HWHEEL)
                }
            }
            ironrdp_server::MouseEvent::Button4Pressed
            | ironrdp_server::MouseEvent::Button4Released
            | ironrdp_server::MouseEvent::Button5Pressed
            | ironrdp_server::MouseEvent::Button5Released => return,
        };

        let input = INPUT {
            r#type: windows::Win32::UI::Input::KeyboardAndMouse::INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: mouse_data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe {
            let _ = SendInput(&[input], size_of::<INPUT>() as i32);
        }
    }
}

fn wheel_data(value: i32) -> u32 {
    value.saturating_mul(120) as u32
}

pub fn validate_local_account(domain: &str, username: &str, password: &str) -> Result<bool> {
    if username.contains('\0') || password.contains('\0') || domain.contains('\0') {
        return Ok(false);
    }

    let domain = if domain.trim().is_empty() {
        "."
    } else {
        domain.trim()
    };
    let domain = wide(domain);
    let username = wide(username);
    let password = wide(password);
    let mut token = HANDLE::default();
    let result = unsafe {
        LogonUserW(
            windows::core::PCWSTR(username.as_ptr()),
            windows::core::PCWSTR(domain.as_ptr()),
            windows::core::PCWSTR(password.as_ptr()),
            LOGON32_LOGON_NETWORK,
            LOGON32_PROVIDER_DEFAULT,
            &mut token,
        )
    };
    if result.is_ok() {
        unsafe {
            let _ = CloseHandle(token);
        }
        Ok(true)
    } else {
        Ok(false)
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
