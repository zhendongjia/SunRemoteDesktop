use std::mem::size_of;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Graphics::Gdi::{
    CDS_FULLSCREEN, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT,
    DM_PELSWIDTH, ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
};
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
        let monitor = primary_monitor()?;
        let image = monitor
            .capture_image()
            .context("capture the Windows primary monitor")?;
        let size = frame_size(image.width(), image.height())?;
        Ok(Self {
            size,
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
        let mut current_size = self.size;
        thread::Builder::new()
            .name("sunrdp-desktop-capture".to_string())
            .spawn(move || {
                let frame_period = Duration::from_millis((1000 / fps.max(1)) as u64);
                loop {
                    let monitor = match primary_monitor() {
                        Ok(monitor) => monitor,
                        Err(error) => {
                            tracing::warn!(
                                ?error,
                                "unable to acquire the Windows primary monitor; retrying"
                            );
                            thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    };
                    loop {
                        let started = std::time::Instant::now();
                        match monitor.capture_image() {
                            Ok(image) => {
                                let size = match frame_size(image.width(), image.height()) {
                                    Ok(size) => size,
                                    Err(error) => {
                                        tracing::warn!(
                                            ?error,
                                            "captured desktop exceeds SunRDP size limits"
                                        );
                                        break;
                                    }
                                };
                                if size != current_size {
                                    tracing::info!(
                                        previous_width = current_size.width,
                                        previous_height = current_size.height,
                                        actual_width = size.width,
                                        actual_height = size.height,
                                        "captured desktop size changed"
                                    );
                                    current_size = size;
                                }
                                publish(CapturedFrame {
                                    width: size.width,
                                    height: size.height,
                                    rgba: image.into_raw(),
                                });
                            }
                            Err(error) => {
                                tracing::warn!(
                                    ?error,
                                    "desktop capture handle became invalid; reacquiring the monitor"
                                );
                                break;
                            }
                        }
                        let elapsed = started.elapsed();
                        if elapsed < frame_period {
                            thread::sleep(frame_period - elapsed);
                        }
                    }

                    thread::sleep(Duration::from_secs(1));
                }
            })
            .context("start desktop capture thread")?;
        Ok(())
    }
}

fn primary_monitor() -> Result<xcap::Monitor> {
    xcap::Monitor::all()?
        .into_iter()
        .find(|monitor| monitor.is_primary().unwrap_or(false))
        .context("find the primary monitor")
}

fn frame_size(width: u32, height: u32) -> Result<DesktopSize> {
    let width = u16::try_from(width).context("captured monitor is wider than RDP allows")?;
    let height = u16::try_from(height).context("captured monitor is taller than RDP allows")?;
    anyhow::ensure!(
        width > 0 && height > 0,
        "primary monitor has no usable size"
    );
    Ok(DesktopSize { width, height })
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
                let (absolute_x, absolute_y) = absolute_mouse_position(*x, *y, desktop);
                (
                    absolute_x,
                    absolute_y,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                )
            }
            ironrdp_server::MouseEvent::RelMove { x, y } => (*x, *y, 0, MOUSEEVENTF_MOVE),
            ironrdp_server::MouseEvent::Button {
                x,
                y,
                button,
                pressed,
            } => {
                let Some(button_flag) = mouse_button_flag(*button, *pressed) else {
                    return;
                };
                let (absolute_x, absolute_y) = absolute_mouse_position(*x, *y, desktop);
                (
                    absolute_x,
                    absolute_y,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | button_flag,
                )
            }
            ironrdp_server::MouseEvent::ButtonRel {
                x,
                y,
                button,
                pressed,
            } => {
                let Some(button_flag) = mouse_button_flag(*button, *pressed) else {
                    return;
                };
                (*x, *y, 0, MOUSEEVENTF_MOVE | button_flag)
            }
            ironrdp_server::MouseEvent::VerticalScroll { value } => {
                (0, 0, wheel_data(i32::from(*value)), MOUSEEVENTF_WHEEL)
            }
            ironrdp_server::MouseEvent::HorizontalScroll { value } => {
                (0, 0, wheel_data(i32::from(*value)), MOUSEEVENTF_HWHEEL)
            }
            ironrdp_server::MouseEvent::Scroll { x, y } => {
                if *y != 0 {
                    (0, 0, wheel_data(*y), MOUSEEVENTF_WHEEL)
                } else {
                    (0, 0, wheel_data(*x), MOUSEEVENTF_HWHEEL)
                }
            }
            _ => return,
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

    fn set_display_size(&self, size: DesktopSize) -> Result<()> {
        set_primary_display_size(size)
    }
}

fn absolute_mouse_position(x: u16, y: u16, desktop: DesktopSize) -> (i32, i32) {
    let max_x = i32::from(desktop.width.saturating_sub(1)).max(1);
    let max_y = i32::from(desktop.height.saturating_sub(1)).max(1);
    (
        (i32::from(x).clamp(0, max_x) * 65535) / max_x,
        (i32::from(y).clamp(0, max_y) * 65535) / max_y,
    )
}

fn mouse_button_flag(
    button: ironrdp_server::MouseButton,
    pressed: bool,
) -> Option<windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS> {
    Some(match (button, pressed) {
        (ironrdp_server::MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
        (ironrdp_server::MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
        (ironrdp_server::MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
        (ironrdp_server::MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
        (ironrdp_server::MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
        (ironrdp_server::MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
        _ => return None,
    })
}

fn set_primary_display_size(size: DesktopSize) -> Result<()> {
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).context("DEVMODEW is too large")?,
        ..Default::default()
    };
    anyhow::ensure!(
        unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool(),
        "read the current primary display mode"
    );
    let mut supported = supported_primary_display_sizes()?;
    let current = DesktopSize {
        width: u16::try_from(mode.dmPelsWidth).context("current display width is too large")?,
        height: u16::try_from(mode.dmPelsHeight).context("current display height is too large")?,
    };
    if !supported.contains(&current) {
        supported.push(current);
    }
    anyhow::ensure!(
        supported.contains(&size),
        "the primary display does not advertise the requested {}x{} mode",
        size.width,
        size.height
    );
    let selected = size;
    if selected == current {
        return Ok(());
    }

    mode.dmPelsWidth = u32::from(selected.width);
    mode.dmPelsHeight = u32::from(selected.height);
    mode.dmFields |= DM_PELSWIDTH | DM_PELSHEIGHT;
    // A client-sized mode is session presentation state, not a permanent local
    // preference. Keep it temporary so a service or machine restart cannot
    // leave the physical console at a remote client's resolution.
    let result = unsafe { ChangeDisplaySettingsExW(None, Some(&mode), None, CDS_FULLSCREEN, None) };
    anyhow::ensure!(
        result == DISP_CHANGE_SUCCESSFUL,
        "Windows rejected the requested {}x{} display mode with status {}",
        selected.width,
        selected.height,
        result.0
    );
    tracing::info!(
        width = selected.width,
        height = selected.height,
        "primary display mode changed"
    );
    Ok(())
}

fn supported_primary_display_sizes() -> Result<Vec<DesktopSize>> {
    let mut sizes = Vec::new();
    for index in 0.. {
        let mut mode = DEVMODEW {
            dmSize: u16::try_from(size_of::<DEVMODEW>()).context("DEVMODEW is too large")?,
            ..Default::default()
        };
        if !unsafe { EnumDisplaySettingsW(None, ENUM_DISPLAY_SETTINGS_MODE(index), &mut mode) }
            .as_bool()
        {
            break;
        }
        let (Ok(width), Ok(height)) = (
            u16::try_from(mode.dmPelsWidth),
            u16::try_from(mode.dmPelsHeight),
        ) else {
            continue;
        };
        let size = DesktopSize { width, height };
        if width >= 200 && height >= 200 && !sizes.contains(&size) {
            sizes.push(size);
        }
    }
    anyhow::ensure!(!sizes.is_empty(), "enumerate primary-display modes");
    Ok(sizes)
}

pub(crate) fn primary_display_capabilities() -> Result<(DesktopSize, Vec<DesktopSize>)> {
    let mut mode = DEVMODEW {
        dmSize: u16::try_from(size_of::<DEVMODEW>()).context("DEVMODEW is too large")?,
        ..Default::default()
    };
    anyhow::ensure!(
        unsafe { EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut mode) }.as_bool(),
        "read the current primary display mode"
    );
    let current = DesktopSize {
        width: u16::try_from(mode.dmPelsWidth).context("current display width is too large")?,
        height: u16::try_from(mode.dmPelsHeight).context("current display height is too large")?,
    };
    let mut supported = supported_primary_display_sizes()?;
    if !supported.contains(&current) {
        supported.push(current);
    }
    supported.sort_unstable_by_key(|size| {
        (
            u32::from(size.width) * u32::from(size.height),
            size.width,
            size.height,
        )
    });
    supported.dedup();
    Ok((current, supported))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_uses_captured_pixel_dimensions() {
        assert_eq!(
            frame_size(2556, 1224).unwrap(),
            DesktopSize {
                width: 2556,
                height: 1224,
            }
        );
    }

    #[test]
    fn frame_size_rejects_empty_capture() {
        assert!(frame_size(0, 1080).is_err());
        assert!(frame_size(1920, 0).is_err());
    }
}
