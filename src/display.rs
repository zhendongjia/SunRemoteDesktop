use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use ironrdp_server::{
    BitmapUpdate, DisplayUpdate, PixelFormat, RdpServerDisplay, RdpServerDisplayUpdates,
};
use tokio::sync::watch;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};

use crate::access::AccessGate;
use crate::platform::{CapturedFrame, DesktopSize};

const ACCESS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const ACCESS_STRIP_HEIGHT: u16 = 64;

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
        // Capture continues while clients disconnect or before their first
        // subscription. `send` would discard the frame when no receiver exists.
        self.sender.send_replace(Some(Arc::new(frame)));
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<CapturedFrame>>> {
        self.sender.subscribe()
    }
}

pub struct RdpDisplay {
    hub: FrameHub,
    size: DesktopSize,
    access_gate: AccessGate,
}

impl RdpDisplay {
    pub fn new(hub: FrameHub, size: DesktopSize, access_gate: AccessGate) -> Self {
        Self {
            hub,
            size,
            access_gate,
        }
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
        let mut refresh = interval_at(
            Instant::now() + ACCESS_REFRESH_INTERVAL,
            ACCESS_REFRESH_INTERVAL,
        );
        refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tracing::info!(
            width = self.size.width,
            height = self.size.height,
            "SunRDP display stream opened"
        );
        Ok(Box::new(RdpDisplayUpdates {
            receiver: self.hub.subscribe(),
            access: self.access_gate.subscribe(),
            access_gate: self.access_gate.clone(),
            size: self.size,
            sent_initial: false,
            refresh,
            access_bitmap: None,
            next_access_row: 0,
        }))
    }
}

struct RdpDisplayUpdates {
    receiver: watch::Receiver<Option<Arc<CapturedFrame>>>,
    access: watch::Receiver<crate::access::AccessSnapshot>,
    access_gate: AccessGate,
    size: DesktopSize,
    sent_initial: bool,
    refresh: Interval,
    access_bitmap: Option<BitmapUpdate>,
    next_access_row: u16,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for RdpDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        if !self.sent_initial
            || self
                .access
                .has_changed()
                .context("access state stream closed")?
        {
            self.sent_initial = true;
            self.access.borrow_and_update();
            return Ok(Some(self.current_update()?));
        }
        if self.access_bitmap.is_some() {
            return self.next_access_strip().map(Some);
        }

        let authenticated = self.access.borrow().is_authenticated();
        if authenticated {
            tokio::select! {
                changed = self.access.changed() => {
                    changed.context("access state stream closed")?;
                    self.access.borrow_and_update();
                    Ok(Some(self.current_update()?))
                }
                changed = self.receiver.changed() => {
                    changed.context("display frame stream closed")?;
                    Ok(Some(self.current_update()?))
                }
            }
        } else {
            tokio::select! {
                changed = self.access.changed() => {
                    changed.context("access state stream closed")?;
                    self.access.borrow_and_update();
                    Ok(Some(self.current_update()?))
                }
                _ = self.refresh.tick() => {
                    Ok(Some(self.current_update()?))
                }
            }
        }
    }
}

impl RdpDisplayUpdates {
    fn current_update(&mut self) -> Result<DisplayUpdate> {
        self.access_bitmap = None;
        if self.access.borrow().is_authenticated() {
            let frame = self
                .receiver
                .borrow_and_update()
                .as_ref()
                .cloned()
                .context("display frame is unavailable")?;
            to_bitmap_update(&frame).map(DisplayUpdate::Bitmap)
        } else {
            // Send bounded strips rather than a full-frame bitmap: the protocol
            // encoder caches full frames and otherwise removes identical redraws.
            // A static login screen must recover even when the first paint was
            // discarded while the client initialized or restored its surface.
            self.access_bitmap = Some(to_bitmap_update(&self.access_gate.render_frame(self.size))?);
            self.next_access_row = 0;
            self.next_access_strip()
        }
    }

    fn next_access_strip(&mut self) -> Result<DisplayUpdate> {
        let bitmap = self
            .access_bitmap
            .as_ref()
            .context("access bitmap is unavailable")?;
        let strip_height = (bitmap.height.get() / 2).clamp(1, ACCESS_STRIP_HEIGHT);
        let remaining = bitmap.height.get() - self.next_access_row;
        let height =
            NonZeroU16::new(remaining.min(strip_height)).context("access strip is empty")?;
        let strip = bitmap
            .sub(0, self.next_access_row, bitmap.width, height)
            .context("access strip is out of bounds")?;
        self.next_access_row += height.get();
        if self.next_access_row == bitmap.height.get() {
            self.access_bitmap = None;
        }
        Ok(DisplayUpdate::Bitmap(strip))
    }
}

fn to_bitmap_update(frame: &CapturedFrame) -> Result<BitmapUpdate> {
    let width = NonZeroU16::new(frame.width).context("frame width is zero")?;
    let height = NonZeroU16::new(frame.height).context("frame height is zero")?;
    let stride = NonZeroUsize::new(usize::from(frame.width) * 4).context("frame stride is zero")?;
    let expected = stride.get() * usize::from(frame.height);
    anyhow::ensure!(frame.rgba.len() == expected, "invalid RGBA frame size");

    Ok(BitmapUpdate {
        x: 0,
        y: 0,
        width,
        height,
        format: PixelFormat::RgbA32,
        data: Bytes::copy_from_slice(&frame.rgba),
        stride,
    })
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use ironrdp_pdu::rdp::capability_sets::{CmdFlags, EntropyBits};
    use ironrdp_server::bench::encoder::{UpdateEncoder, UpdateEncoderCodecs};

    fn test_size() -> DesktopSize {
        DesktopSize {
            width: 640,
            height: 480,
        }
    }

    async fn read_screen(updates: &mut dyn RdpServerDisplayUpdates, size: DesktopSize) -> Vec<u8> {
        let stride = usize::from(size.width) * 4;
        let mut pixels = vec![0; stride * usize::from(size.height)];
        let mut rows = 0;
        while rows < size.height {
            let update = tokio::time::timeout(Duration::from_secs(2), updates.next_update())
                .await
                .expect("screen must refresh without keyboard input or desktop capture")
                .unwrap()
                .unwrap();
            let DisplayUpdate::Bitmap(bitmap) = update else {
                panic!("expected a bitmap")
            };
            assert_eq!(bitmap.x, 0);
            assert_eq!(bitmap.y, rows);
            assert_eq!(bitmap.width.get(), size.width);
            for row in 0..usize::from(bitmap.height.get()) {
                let src = row * bitmap.stride.get();
                let dst = (usize::from(bitmap.y) + row) * stride;
                pixels[dst..dst + stride].copy_from_slice(&bitmap.data[src..src + stride]);
            }
            rows += bitmap.height.get();
        }
        pixels
    }

    #[tokio::test(start_paused = true)]
    async fn access_screen_repaints_without_input_or_capture() {
        let size = test_size();
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let expected = gate.render_frame(size).rgba;
        let mut display = RdpDisplay::new(FrameHub::new(size), size, gate);
        let mut updates = display.updates().await.unwrap();
        assert_eq!(read_screen(updates.as_mut(), size).await, expected);
        assert_eq!(read_screen(updates.as_mut(), size).await, expected);
    }

    #[test]
    fn frame_hub_keeps_latest_frame_before_a_client_subscribes() {
        let size = test_size();
        let hub = FrameHub::new(size);
        let pixels = vec![91; usize::from(size.width) * usize::from(size.height) * 4];
        hub.publish(CapturedFrame {
            width: size.width,
            height: size.height,
            rgba: pixels.clone(),
        });
        assert!(
            hub.subscribe().borrow().as_ref().unwrap().rgba == pixels,
            "the latest frame must survive without subscribers"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn desktop_stays_gated_and_reconnect_returns_to_access_screen() {
        let size = test_size();
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let hub = FrameHub::new(size);
        let mut display = RdpDisplay::new(hub.clone(), size, gate.clone());
        let mut updates = display.updates().await.unwrap();
        let pixels = vec![91; usize::from(size.width) * usize::from(size.height) * 4];
        hub.publish(CapturedFrame {
            width: size.width,
            height: size.height,
            rgba: pixels.clone(),
        });
        assert_eq!(
            read_screen(updates.as_mut(), size).await,
            gate.render_frame(size).rgba
        );
        assert_eq!(
            read_screen(updates.as_mut(), size).await,
            gate.render_frame(size).rgba
        );
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        assert_eq!(read_screen(updates.as_mut(), size).await, pixels);
        drop(updates);
        gate.reset();
        let mut updates = display.updates().await.unwrap();
        assert_eq!(
            read_screen(updates.as_mut(), size).await,
            gate.render_frame(size).rgba
        );
    }

    fn test_encoder(size: DesktopSize, codecs: UpdateEncoderCodecs) -> UpdateEncoder {
        UpdateEncoder::new(
            ironrdp_server::DesktopSize {
                width: size.width,
                height: size.height,
            },
            CmdFlags::SET_SURFACE_BITS,
            codecs,
            8 * 1024 * 1024,
        )
        .unwrap()
    }

    async fn encoded_bytes(encoder: &mut UpdateEncoder, update: DisplayUpdate) -> usize {
        let mut bytes = 0;
        let mut fragments = encoder.update(update);
        while let Some(fragment) = fragments.next().await {
            bytes += fragment.unwrap().data.len();
        }
        bytes
    }

    async fn encode_screen(
        updates: &mut dyn RdpServerDisplayUpdates,
        encoder: &mut UpdateEncoder,
        size: DesktopSize,
    ) -> usize {
        let mut rows = 0;
        let mut bytes = 0;
        while rows < size.height {
            let update = tokio::time::timeout(Duration::from_secs(2), updates.next_update())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let DisplayUpdate::Bitmap(ref bitmap) = update else {
                panic!("expected a bitmap")
            };
            assert_eq!(bitmap.y, rows);
            rows += bitmap.height.get();
            let encoded = encoded_bytes(encoder, update).await;
            assert!(
                encoded > 0,
                "an unchanged access-screen strip must not be removed by the encoder's diff cache"
            );
            bytes += encoded;
        }
        bytes
    }

    #[tokio::test(start_paused = true)]
    async fn access_repaints_survive_the_protocol_encoder_diff_cache() {
        let size = test_size();
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let mut display = RdpDisplay::new(FrameHub::new(size), size, gate);
        let mut updates = display.updates().await.unwrap();
        let mut codecs = UpdateEncoderCodecs::new();
        codecs.set_nscodec(Some((1, 1)));
        let mut encoder = test_encoder(size, codecs);
        let initial = encode_screen(updates.as_mut(), &mut encoder, size).await;
        let repeated = encode_screen(updates.as_mut(), &mut encoder, size).await;
        assert_eq!(initial, repeated);
    }

    #[tokio::test(start_paused = true)]
    async fn supported_codecs_compress_the_large_access_screen() {
        let size = DesktopSize {
            width: 2556,
            height: 1224,
        };
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let bitmap = to_bitmap_update(&gate.render_frame(size)).unwrap();
        let mut raw_encoder = test_encoder(size, UpdateEncoderCodecs::new());
        let raw_bytes = encoded_bytes(&mut raw_encoder, DisplayUpdate::Bitmap(bitmap)).await;
        let mut codecs = UpdateEncoderCodecs::new();
        codecs.set_nscodec(Some((1, 1)));
        let mut encoder = test_encoder(size, codecs);
        let mut display = RdpDisplay::new(FrameHub::new(size), size, gate);
        let mut updates = display.updates().await.unwrap();
        let compressed_bytes = encode_screen(updates.as_mut(), &mut encoder, size).await;
        eprintln!(
            "access screen 2556x1224: raw={raw_bytes} bytes, NSCodec={compressed_bytes} bytes"
        );
        assert!(
            compressed_bytes < raw_bytes / 10,
            "the login screen must not require megabytes per redraw"
        );
        let mut codecs = UpdateEncoderCodecs::new();
        codecs.set_remotefx(Some((EntropyBits::Rlgr3, 3)));
        let mut encoder = test_encoder(size, codecs);
        let mut updates = display.updates().await.unwrap();
        let rfx_bytes = encode_screen(updates.as_mut(), &mut encoder, size).await;
        eprintln!("access screen 2556x1224: RemoteFX={rfx_bytes} bytes");
        assert!(
            rfx_bytes < raw_bytes / 10,
            "RemoteFX clients must retain compression"
        );
        assert!(encode_screen(updates.as_mut(), &mut encoder, size).await > 0);
    }
}
