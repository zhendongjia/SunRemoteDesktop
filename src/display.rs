use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;

use anyhow::{Context, Result};
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DisplayUpdate, PixelFormat, RdpServerDisplay, RdpServerDisplayUpdates,
};
use tokio::sync::watch;

use crate::platform::{CapturedFrame, DesktopSize};

#[derive(Clone)]
pub struct FrameHub {
    sender: watch::Sender<Option<Arc<CapturedFrame>>>,
}

impl FrameHub {
    pub fn new(size: DesktopSize) -> Self {
        let initial = CapturedFrame {
            width: size.width,
            height: size.height,
            rgba: placeholder_frame(size),
        };
        let (sender, _) = watch::channel(Some(Arc::new(initial)));
        Self { sender }
    }

    pub fn publish(&self, frame: CapturedFrame) {
        let _ = self.sender.send(Some(Arc::new(frame)));
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<CapturedFrame>>> {
        self.sender.subscribe()
    }
}

pub struct RdpDisplay {
    hub: FrameHub,
    size: DesktopSize,
}

impl RdpDisplay {
    pub fn new(hub: FrameHub, size: DesktopSize) -> Self {
        Self { hub, size }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for RdpDisplay {
    async fn size(&mut self) -> ironrdp_server::DesktopSize {
        ironrdp_server::DesktopSize {
            width: self.size.width,
            height: self.size.height,
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        Ok(Box::new(RdpDisplayUpdates {
            receiver: self.hub.subscribe(),
            sent_initial: false,
        }))
    }
}

struct RdpDisplayUpdates {
    receiver: watch::Receiver<Option<Arc<CapturedFrame>>>,
    sent_initial: bool,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for RdpDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        if !self.sent_initial {
            self.sent_initial = true;
            if let Some(frame) = self.receiver.borrow_and_update().as_ref().cloned() {
                return Ok(Some(to_bitmap_update(&frame)?));
            }
        }

        self.receiver
            .changed()
            .await
            .context("display frame stream closed")?;
        let frame = self
            .receiver
            .borrow_and_update()
            .as_ref()
            .cloned()
            .context("display frame is unavailable")?;
        Ok(Some(to_bitmap_update(&frame)?))
    }
}

fn to_bitmap_update(frame: &CapturedFrame) -> Result<DisplayUpdate> {
    let width = NonZeroU16::new(frame.width).context("frame width is zero")?;
    let height = NonZeroU16::new(frame.height).context("frame height is zero")?;
    let stride = NonZeroUsize::new(usize::from(frame.width) * 4).context("frame stride is zero")?;
    let expected = stride.get() * usize::from(frame.height);
    anyhow::ensure!(frame.rgba.len() == expected, "invalid RGBA frame size");

    Ok(DisplayUpdate::Bitmap(BitmapUpdate {
        x: 0,
        y: 0,
        width,
        height,
        format: PixelFormat::RgbA32,
        data: Bytes::copy_from_slice(&frame.rgba),
        stride,
    }))
}

fn placeholder_frame(size: DesktopSize) -> Vec<u8> {
    let width = usize::from(size.width);
    let height = usize::from(size.height);
    let mut rgba = vec![0u8; width * height * 4];
    for y in 0..height {
        for x in 0..width {
            let offset = (y * width + x) * 4;
            let checker = ((x / 32) + (y / 32)) % 2 == 0;
            rgba[offset] = if checker { 30 } else { 24 };
            rgba[offset + 1] = if checker { 36 } else { 29 };
            rgba[offset + 2] = if checker { 48 } else { 40 };
            rgba[offset + 3] = 255;
        }
    }
    rgba
}
