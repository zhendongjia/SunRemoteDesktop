use anyhow::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DesktopSize {
    pub width: u16,
    pub height: u16,
}

#[derive(Debug, Clone)]
pub struct CapturedFrame {
    pub width: u16,
    pub height: u16,
    /// RGBA pixels, one tightly packed row after another.
    pub rgba: Vec<u8>,
}

pub trait DesktopCapture: Send {
    fn size(&self) -> DesktopSize;
    fn start(self: Box<Self>, publish: Box<dyn Fn(CapturedFrame) + Send>) -> Result<()>;
}

pub trait InputInjector: Send + Sync {
    fn keyboard(&self, event: &ironrdp_server::KeyboardEvent);
    fn mouse(&self, event: &ironrdp_server::MouseEvent, desktop: DesktopSize);
}

#[cfg(windows)]
pub mod windows;

#[cfg(not(windows))]
pub mod unsupported {
    use super::{CapturedFrame, DesktopCapture, DesktopSize, InputInjector};
    use anyhow::{Result, bail};

    pub struct UnsupportedCapture;

    impl DesktopCapture for UnsupportedCapture {
        fn size(&self) -> DesktopSize {
            DesktopSize {
                width: 1280,
                height: 720,
            }
        }

        fn start(self: Box<Self>, _publish: Box<dyn Fn(CapturedFrame) + Send>) -> Result<()> {
            bail!("this build does not yet include a desktop capture backend")
        }
    }

    pub struct UnsupportedInput;

    impl InputInjector for UnsupportedInput {
        fn keyboard(&self, _event: &ironrdp_server::KeyboardEvent) {}
        fn mouse(&self, _event: &ironrdp_server::MouseEvent, _desktop: DesktopSize) {}
    }
}
