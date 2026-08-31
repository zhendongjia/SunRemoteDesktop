use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use ironrdp_displaycontrol::pdu::DisplayControlMonitorLayout;
use ironrdp_pdu::pointer::Point16;
use ironrdp_server::{
    BitmapUpdate, DisplayUpdate, PixelFormat, RGBAPointer, RdpServerDisplay,
    RdpServerDisplayUpdates,
};
use tokio::sync::{mpsc, watch};
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};

use crate::access::{AccessAction, AccessGate};
use crate::platform::{CapturedFrame, DesktopSize, InputInjector};

const ACCESS_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const ACCESS_STRIP_HEIGHT: u16 = 64;
const DYNAMIC_RESIZE_DEBOUNCE: Duration = Duration::from_millis(450);

#[derive(Clone)]
pub struct FrameHub {
    sender: watch::Sender<Option<Arc<CapturedFrame>>>,
    size_sender: watch::Sender<DesktopSize>,
    supported_sizes_sender: watch::Sender<Vec<DesktopSize>>,
    available_sender: watch::Sender<bool>,
    pointer_sender: watch::Sender<Option<Point16>>,
    availability_generation: Arc<AtomicU64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DesktopViewport {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

pub fn fitted_desktop_viewport(source: DesktopSize, canvas: DesktopSize) -> DesktopViewport {
    let source_width = u64::from(source.width.max(1));
    let source_height = u64::from(source.height.max(1));
    let canvas_width = u64::from(canvas.width.max(1));
    let canvas_height = u64::from(canvas.height.max(1));

    let (width, height) = if source_width * canvas_height > canvas_width * source_height {
        let height = (source_height * canvas_width + source_width / 2) / source_width;
        (canvas_width, height.clamp(1, canvas_height))
    } else {
        let width = (source_width * canvas_height + source_height / 2) / source_height;
        (width.clamp(1, canvas_width), canvas_height)
    };
    let width = width as u16;
    let height = height as u16;
    DesktopViewport {
        x: (canvas.width - width) / 2,
        y: (canvas.height - height) / 2,
        width,
        height,
    }
}

impl FrameHub {
    pub fn new(size: DesktopSize) -> Self {
        Self::with_availability(size, true)
    }

    pub fn new_unavailable(size: DesktopSize) -> Self {
        Self::with_availability(size, false)
    }

    fn with_availability(size: DesktopSize, available: bool) -> Self {
        let initial = CapturedFrame {
            width: size.width,
            height: size.height,
            rgba: placeholder_frame(size),
        };
        let (sender, _) = watch::channel(Some(Arc::new(initial)));
        let (size_sender, _) = watch::channel(size);
        let (supported_sizes_sender, _) = watch::channel(vec![size]);
        let (available_sender, _) = watch::channel(available);
        let (pointer_sender, _) = watch::channel(None);
        Self {
            sender,
            size_sender,
            supported_sizes_sender,
            available_sender,
            pointer_sender,
            availability_generation: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn publish(&self, frame: CapturedFrame) {
        // Capture continues while clients disconnect or before their first
        // subscription. `send` would discard the frame when no receiver exists.
        let size = DesktopSize {
            width: frame.width,
            height: frame.height,
        };
        if *self.size_sender.borrow() != size {
            self.size_sender.send_replace(size);
        }
        if !self.supported_sizes_sender.borrow().contains(&size) {
            self.supported_sizes_sender.send_modify(|sizes| {
                sizes.push(size);
                normalize_display_sizes(sizes);
            });
        }
        // Cancel any delayed unavailable transition left by the previous
        // console agent before publishing the replacement agent's first frame.
        self.availability_generation.fetch_add(1, Ordering::AcqRel);
        self.sender.send_replace(Some(Arc::new(frame)));
        if !*self.available_sender.borrow() {
            self.available_sender.send_replace(true);
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<Option<Arc<CapturedFrame>>> {
        self.sender.subscribe()
    }

    pub fn size(&self) -> DesktopSize {
        *self.size_sender.borrow()
    }

    pub fn subscribe_size(&self) -> watch::Receiver<DesktopSize> {
        self.size_sender.subscribe()
    }

    pub fn supported_sizes(&self) -> Vec<DesktopSize> {
        self.supported_sizes_sender.borrow().clone()
    }

    pub fn set_display_capabilities(
        &self,
        current_size: DesktopSize,
        mut supported_sizes: Vec<DesktopSize>,
    ) {
        if *self.size_sender.borrow() != current_size {
            self.size_sender.send_replace(current_size);
        }
        if !supported_sizes.contains(&current_size) {
            supported_sizes.push(current_size);
        }
        normalize_display_sizes(&mut supported_sizes);
        self.supported_sizes_sender.send_replace(supported_sizes);
    }

    pub fn subscribe_supported_sizes(&self) -> watch::Receiver<Vec<DesktopSize>> {
        self.supported_sizes_sender.subscribe()
    }

    pub fn is_available(&self) -> bool {
        *self.available_sender.borrow()
    }

    pub fn set_available(&self, available: bool) {
        self.availability_generation.fetch_add(1, Ordering::AcqRel);
        if *self.available_sender.borrow() != available {
            self.available_sender.send_replace(available);
        }
    }

    /// Keeps the latest frame available during a short capture-agent handoff.
    /// A frame published by the replacement agent cancels this transition.
    pub fn set_unavailable_after(&self, delay: Duration) {
        let generation = self
            .availability_generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        let availability_generation = Arc::clone(&self.availability_generation);
        let available_sender = self.available_sender.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            if availability_generation.load(Ordering::Acquire) == generation
                && *available_sender.borrow()
            {
                available_sender.send_replace(false);
            }
        });
    }

    pub fn subscribe_available(&self) -> watch::Receiver<bool> {
        self.available_sender.subscribe()
    }

    pub fn publish_pointer_position(&self, x: u16, y: u16) {
        self.pointer_sender.send_if_modified(|position| {
            let next = Some(Point16 { x, y });
            if *position == next {
                false
            } else {
                *position = next;
                true
            }
        });
    }

    fn subscribe_pointer_position(&self) -> watch::Receiver<Option<Point16>> {
        self.pointer_sender.subscribe()
    }
}

fn normalize_display_sizes(sizes: &mut Vec<DesktopSize>) {
    sizes.retain(|size| size.width > 0 && size.height > 0);
    sizes.sort_unstable_by_key(|size| {
        (
            u32::from(size.width) * u32::from(size.height),
            size.width,
            size.height,
        )
    });
    sizes.dedup();
}

pub struct RdpDisplay {
    hub: FrameHub,
    client_size: watch::Sender<DesktopSize>,
    access_gate: AccessGate,
    resize_requests: Option<mpsc::UnboundedSender<DesktopSize>>,
    pending_layout: Option<(u64, DesktopSize)>,
}

impl RdpDisplay {
    pub fn new(hub: FrameHub, access_gate: AccessGate) -> Self {
        let client_size = hub.size();
        Self {
            hub,
            client_size: watch::channel(client_size).0,
            access_gate,
            resize_requests: None,
            pending_layout: None,
        }
    }

    pub fn with_dynamic_resize(
        hub: FrameHub,
        access_gate: AccessGate,
        injector: Arc<dyn InputInjector>,
    ) -> Self {
        let (resize_requests, receiver) = mpsc::unbounded_channel();
        tokio::spawn(dynamic_resize_worker(
            receiver,
            injector,
            access_gate.clone(),
        ));
        let mut display = Self::new(hub, access_gate);
        display.resize_requests = Some(resize_requests);
        display
    }

    fn forward_pending_access_action(&self) {
        let Some(AccessAction::ChangeDisplaySize(target)) = self.access_gate.take_pending_action()
        else {
            return;
        };
        if self
            .resize_requests
            .as_ref()
            .is_none_or(|requests| requests.send(target).is_err())
        {
            tracing::warn!(
                width = target.width,
                height = target.height,
                "remembered physical-display resize handler is unavailable"
            );
        }
    }
}

async fn dynamic_resize_worker(
    mut receiver: mpsc::UnboundedReceiver<DesktopSize>,
    injector: Arc<dyn InputInjector>,
    access_gate: AccessGate,
) {
    while let Some(mut target) = receiver.recv().await {
        loop {
            match tokio::time::timeout(DYNAMIC_RESIZE_DEBOUNCE, receiver.recv()).await {
                Ok(Some(newer)) => target = newer,
                Ok(None) => return,
                Err(_) => break,
            }
        }

        // A disconnect resets the access policy. Do not let a request queued
        // by the previous connection change the physical console afterwards.
        if !access_gate.should_apply_display_size(target) {
            continue;
        }
        tracing::info!(
            width = target.width,
            height = target.height,
            "applying debounced client resize to the physical display"
        );
        if let Err(error) = injector.set_display_size(target) {
            tracing::warn!(
                ?error,
                width = target.width,
                height = target.height,
                "unable to apply the dynamic physical-display resize; continuing with proportional scaling"
            );
        }
    }
}

#[async_trait::async_trait]
impl RdpServerDisplay for RdpDisplay {
    async fn size(&mut self) -> ironrdp_server::DesktopSize {
        let client_size = *self.client_size.borrow();
        ironrdp_server::DesktopSize {
            width: client_size.width,
            height: client_size.height,
        }
    }

    async fn request_initial_size(
        &mut self,
        client_size: ironrdp_server::DesktopSize,
    ) -> ironrdp_server::DesktopSize {
        let handshake_size = DesktopSize {
            width: client_size.width,
            height: client_size.height,
        };
        let generation = self.access_gate.connection_generation();
        let client_size = match self.pending_layout.take() {
            Some((pending_generation, pending_size)) if pending_generation == generation => {
                tracing::info!(
                    width = pending_size.width,
                    height = pending_size.height,
                    handshake_width = handshake_size.width,
                    handshake_height = handshake_size.height,
                    "SunRDP retained the dynamic client size during graphics reactivation"
                );
                pending_size
            }
            _ => handshake_size,
        };
        self.client_size.send_replace(client_size);
        self.access_gate.set_display_capabilities(
            client_size,
            self.hub.size(),
            self.hub.supported_sizes(),
            self.hub.is_available(),
        );
        self.forward_pending_access_action();
        tracing::info!(
            width = client_size.width,
            height = client_size.height,
            "SunRDP adopted the client desktop size"
        );
        ironrdp_server::DesktopSize {
            width: client_size.width,
            height: client_size.height,
        }
    }

    fn request_layout(&mut self, layout: DisplayControlMonitorLayout) {
        let monitors = layout.monitors();
        if monitors.len() != 1 || !monitors[0].is_primary() {
            tracing::warn!(
                monitors = monitors.len(),
                "SunRDP currently supports dynamic resizing for one primary monitor only"
            );
            return;
        }
        let monitor = &monitors[0];
        let (width, height) = monitor.dimensions();
        let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height)) else {
            tracing::warn!(width, height, "client dynamic resolution is out of range");
            return;
        };
        let requested = DesktopSize { width, height };
        let desktop_scale_factor = monitor.desktop_scale_factor();
        let device_scale_factor = monitor.device_scale_factor();
        let physical_dimensions_mm = monitor.physical_dimensions();
        if *self.client_size.borrow() == requested {
            tracing::info!(
                width,
                height,
                desktop_scale_factor = ?desktop_scale_factor,
                device_scale_factor = ?device_scale_factor,
                physical_dimensions_mm = ?physical_dimensions_mm,
                "SunRDP received client display metrics for the current resolution"
            );
            return;
        }

        tracing::info!(
            width,
            height,
            desktop_scale_factor = ?desktop_scale_factor,
            device_scale_factor = ?device_scale_factor,
            physical_dimensions_mm = ?physical_dimensions_mm,
            "SunRDP received a dynamic client-resolution request"
        );
        self.pending_layout = Some((self.access_gate.connection_generation(), requested));
        // Publish the protocol resize first. This ensures the old update stream
        // deactivates before an access-state repaint can target the new canvas.
        self.client_size.send_replace(requested);
        if let Some(AccessAction::ChangeDisplaySize(target)) =
            self.access_gate.request_client_resize(requested)
            && self
                .resize_requests
                .as_ref()
                .is_none_or(|requests| requests.send(target).is_err())
        {
            tracing::warn!(
                width,
                height,
                "dynamic physical-display resize handler is unavailable"
            );
        }
    }

    async fn updates(&mut self) -> Result<Box<dyn RdpServerDisplayUpdates>> {
        let client_size = *self.client_size.borrow();
        let mut refresh = interval_at(
            Instant::now() + ACCESS_REFRESH_INTERVAL,
            ACCESS_REFRESH_INTERVAL,
        );
        refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
        tracing::info!(
            width = client_size.width,
            height = client_size.height,
            "SunRDP display stream opened"
        );
        self.access_gate.set_display_capabilities(
            client_size,
            self.hub.size(),
            self.hub.supported_sizes(),
            self.hub.is_available(),
        );
        self.forward_pending_access_action();
        Ok(Box::new(RdpDisplayUpdates {
            receiver: self.hub.subscribe(),
            host_size: self.hub.subscribe_size(),
            supported_sizes: self.hub.subscribe_supported_sizes(),
            host_available: self.hub.subscribe_available(),
            pointer_position: self.hub.subscribe_pointer_position(),
            access: self.access_gate.subscribe(),
            access_gate: self.access_gate.clone(),
            resize_requests: self.resize_requests.clone(),
            client_size_updates: self.client_size.subscribe(),
            client_size,
            sent_initial: false,
            pointer_shape_sent: false,
            pointer_position_sent: false,
            refresh,
            access_bitmap: None,
            next_access_row: 0,
        }))
    }
}

struct RdpDisplayUpdates {
    receiver: watch::Receiver<Option<Arc<CapturedFrame>>>,
    host_size: watch::Receiver<DesktopSize>,
    supported_sizes: watch::Receiver<Vec<DesktopSize>>,
    host_available: watch::Receiver<bool>,
    pointer_position: watch::Receiver<Option<Point16>>,
    access: watch::Receiver<crate::access::AccessSnapshot>,
    access_gate: AccessGate,
    resize_requests: Option<mpsc::UnboundedSender<DesktopSize>>,
    client_size_updates: watch::Receiver<DesktopSize>,
    client_size: DesktopSize,
    sent_initial: bool,
    pointer_shape_sent: bool,
    pointer_position_sent: bool,
    refresh: Interval,
    access_bitmap: Option<BitmapUpdate>,
    next_access_row: u16,
}

#[async_trait::async_trait]
impl RdpServerDisplayUpdates for RdpDisplayUpdates {
    async fn next_update(&mut self) -> Result<Option<DisplayUpdate>> {
        if self
            .client_size_updates
            .has_changed()
            .context("client display-size stream closed")?
        {
            return Ok(Some(self.client_resize_update()));
        }
        self.sync_display_state();
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
        if !self.pointer_shape_sent {
            self.pointer_shape_sent = true;
            return Ok(Some(DisplayUpdate::RGBAPointer(default_arrow_pointer())));
        }
        if !self.pointer_position_sent {
            self.pointer_position_sent = true;
            if let Some(update) = self.pointer_position_update() {
                return Ok(Some(update));
            }
        } else if self
            .pointer_position
            .has_changed()
            .context("pointer-position stream closed")?
        {
            return Ok(self.pointer_position_update());
        }
        if self.access_bitmap.is_some() {
            return self.next_access_strip().map(Some);
        }

        let desktop_ready = self.access.borrow().is_desktop_ready();
        if desktop_ready {
            tokio::select! {
                biased;
                changed = self.client_size_updates.changed() => {
                    changed.context("client display-size stream closed")?;
                    Ok(Some(self.client_resize_update()))
                }
                changed = self.access.changed() => {
                    changed.context("access state stream closed")?;
                    self.access.borrow_and_update();
                    Ok(Some(self.current_update()?))
                }
                changed = self.pointer_position.changed() => {
                    changed.context("pointer-position stream closed")?;
                    Ok(self.pointer_position_update())
                }
                changed = self.receiver.changed() => {
                    changed.context("display frame stream closed")?;
                    Ok(Some(self.current_update()?))
                }
                changed = self.host_size.changed() => {
                    changed.context("host display-size stream closed")?;
                    self.sync_display_state();
                    Ok(Some(self.current_update()?))
                }
                changed = self.supported_sizes.changed() => {
                    changed.context("host display-mode stream closed")?;
                    self.sync_display_state();
                    Ok(Some(self.current_update()?))
                }
                changed = self.host_available.changed() => {
                    changed.context("host availability stream closed")?;
                    self.sync_display_state();
                    Ok(Some(self.current_update()?))
                }
            }
        } else {
            tokio::select! {
                biased;
                changed = self.client_size_updates.changed() => {
                    changed.context("client display-size stream closed")?;
                    Ok(Some(self.client_resize_update()))
                }
                changed = self.access.changed() => {
                    changed.context("access state stream closed")?;
                    self.access.borrow_and_update();
                    Ok(Some(self.current_update()?))
                }
                changed = self.pointer_position.changed() => {
                    changed.context("pointer-position stream closed")?;
                    Ok(self.pointer_position_update())
                }
                changed = self.host_size.changed() => {
                    changed.context("host display-size stream closed")?;
                    self.sync_display_state();
                    Ok(Some(self.current_update()?))
                }
                changed = self.supported_sizes.changed() => {
                    changed.context("host display-mode stream closed")?;
                    self.sync_display_state();
                    Ok(Some(self.current_update()?))
                }
                changed = self.host_available.changed() => {
                    changed.context("host availability stream closed")?;
                    self.sync_display_state();
                    Ok(Some(self.current_update()?))
                }
                _ = self.refresh.tick() => {
                    self.sync_display_state();
                    Ok(Some(self.current_update()?))
                }
            }
        }
    }
}

fn default_arrow_pointer() -> RGBAPointer {
    const WIDTH: u16 = 24;
    const HEIGHT: u16 = 24;
    let mut data = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 4);

    // RDP 32-bit pointer rows are bottom-up and pixels are BGRA. A compact
    // high-contrast arrow remains legible on both light and dark desktops.
    for y in (0..HEIGHT).rev() {
        for x in 0..WIDTH {
            let head_edge = 1 + y / 2;
            let in_head = (1..=15).contains(&y) && (1..=head_edge).contains(&x);
            let in_stem = (12..=22).contains(&y) && (5..=9).contains(&x);
            let white_head = (3..=13).contains(&y) && (2..head_edge).contains(&x);
            let white_stem = (13..=20).contains(&y) && (6..=7).contains(&x);
            let pixel = if white_head || white_stem {
                [255, 255, 255, 255]
            } else if in_head || in_stem {
                [0, 0, 0, 255]
            } else {
                [0, 0, 0, 0]
            };
            data.extend_from_slice(&pixel);
        }
    }

    RGBAPointer {
        cache_index: 0,
        width: WIDTH,
        height: HEIGHT,
        hot_x: 1,
        hot_y: 1,
        data,
    }
}

impl RdpDisplayUpdates {
    fn client_resize_update(&mut self) -> DisplayUpdate {
        self.client_size = *self.client_size_updates.borrow_and_update();
        self.access_bitmap = None;
        self.next_access_row = 0;
        self.pointer_shape_sent = false;
        self.pointer_position_sent = false;
        tracing::info!(
            width = self.client_size.width,
            height = self.client_size.height,
            "SunRDP reactivating the RDP graphics surface at the client size"
        );
        DisplayUpdate::Resize(ironrdp_server::DesktopSize {
            width: self.client_size.width,
            height: self.client_size.height,
        })
    }

    fn pointer_position_update(&mut self) -> Option<DisplayUpdate> {
        self.pointer_position.borrow_and_update().map(|position| {
            DisplayUpdate::PointerPosition(Point16 {
                x: position.x.min(self.client_size.width.saturating_sub(1)),
                y: position.y.min(self.client_size.height.saturating_sub(1)),
            })
        })
    }

    fn sync_display_state(&mut self) {
        let host_size = *self.host_size.borrow_and_update();
        let supported_sizes = self.supported_sizes.borrow_and_update().clone();
        let available = *self.host_available.borrow_and_update();
        self.access_gate.set_display_capabilities(
            self.client_size,
            host_size,
            supported_sizes,
            available,
        );
        if let Some(AccessAction::ChangeDisplaySize(target)) =
            self.access_gate.take_pending_action()
            && self
                .resize_requests
                .as_ref()
                .is_none_or(|requests| requests.send(target).is_err())
        {
            tracing::warn!(
                width = target.width,
                height = target.height,
                "remembered physical-display resize handler is unavailable"
            );
        }
    }

    fn current_update(&mut self) -> Result<DisplayUpdate> {
        self.access_bitmap = None;
        let snapshot = self.access_gate.snapshot();
        if snapshot.is_desktop_ready() {
            let frame = self
                .receiver
                .borrow_and_update()
                .as_ref()
                .cloned()
                .context("display frame is unavailable")?;
            let output = match snapshot.presentation() {
                Some(crate::access::DesktopPresentation::Native) => {
                    anyhow::ensure!(
                        frame.width == self.client_size.width
                            && frame.height == self.client_size.height,
                        "native desktop frame does not match the negotiated client size"
                    );
                    frame
                }
                Some(crate::access::DesktopPresentation::Scale) => {
                    Arc::new(scale_frame(&frame, self.client_size)?)
                }
                None => anyhow::bail!("desktop presentation is unavailable"),
            };
            to_bitmap_update(&output).map(DisplayUpdate::Bitmap)
        } else {
            // Send bounded strips rather than a full-frame bitmap: the protocol
            // encoder caches full frames and otherwise removes identical redraws.
            // A static login screen must recover even when the first paint was
            // discarded while the client initialized or restored its surface.
            self.access_bitmap = Some(to_bitmap_update(
                &self.access_gate.render_frame(self.client_size),
            )?);
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

fn scale_frame(frame: &CapturedFrame, target: DesktopSize) -> Result<CapturedFrame> {
    if frame.width == target.width && frame.height == target.height {
        return Ok(frame.clone());
    }
    let viewport = fitted_desktop_viewport(
        DesktopSize {
            width: frame.width,
            height: frame.height,
        },
        target,
    );
    let source = Image::from_vec_u8(
        u32::from(frame.width),
        u32::from(frame.height),
        frame.rgba.clone(),
        PixelType::U8x4,
    )
    .context("create source image for desktop scaling")?;
    let mut destination = Image::new(
        u32::from(viewport.width),
        u32::from(viewport.height),
        PixelType::U8x4,
    );
    let options = ResizeOptions::new()
        .resize_alg(desktop_resize_algorithm(
            DesktopSize {
                width: frame.width,
                height: frame.height,
            },
            DesktopSize {
                width: viewport.width,
                height: viewport.height,
            },
        ))
        .use_alpha(false);
    Resizer::new()
        .resize(&source, &mut destination, &options)
        .context("scale the shared desktop to the client size")?;
    let scaled = destination.into_vec();
    let target_stride = usize::from(target.width) * 4;
    let viewport_stride = usize::from(viewport.width) * 4;
    let mut rgba = [5, 10, 19, 255].repeat(usize::from(target.width) * usize::from(target.height));
    for row in 0..usize::from(viewport.height) {
        let source_start = row * viewport_stride;
        let target_start =
            (usize::from(viewport.y) + row) * target_stride + usize::from(viewport.x) * 4;
        rgba[target_start..target_start + viewport_stride]
            .copy_from_slice(&scaled[source_start..source_start + viewport_stride]);
    }
    Ok(CapturedFrame {
        width: target.width,
        height: target.height,
        rgba,
    })
}

fn desktop_resize_algorithm(source: DesktopSize, target: DesktopSize) -> ResizeAlg {
    if target.width > source.width || target.height > source.height {
        // Bilinear interpolation visibly softens ClearType and other one-pixel
        // desktop details. Catmull-Rom retains edge contrast during upscaling
        // without the stronger ringing that Lanczos can add around text.
        ResizeAlg::Convolution(FilterType::CatmullRom)
    } else {
        // Downscaling benefits from the wider low-pass filter to avoid aliasing
        // in dense UI details and thin glyph strokes.
        ResizeAlg::Convolution(FilterType::Lanczos3)
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
    use ironrdp_server::KeyboardEvent;
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
                continue;
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

    #[tokio::test]
    async fn display_stream_advertises_and_moves_a_visible_pointer() {
        let size = test_size();
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let hub = FrameHub::new(size);
        let mut display = RdpDisplay::new(hub.clone(), gate);
        let mut updates = display.updates().await.unwrap();

        assert!(matches!(
            updates.next_update().await.unwrap().unwrap(),
            DisplayUpdate::Bitmap(_)
        ));
        assert!(matches!(
            updates.next_update().await.unwrap().unwrap(),
            DisplayUpdate::RGBAPointer(_)
        ));

        hub.publish_pointer_position(123, 45);
        assert!(matches!(
            updates.next_update().await.unwrap().unwrap(),
            DisplayUpdate::PointerPosition(Point16 { x: 123, y: 45 })
        ));
    }

    #[tokio::test]
    async fn pointer_updates_encode_to_nonempty_rdp_frames() {
        let size = test_size();
        let mut encoder = test_encoder(size, UpdateEncoderCodecs::new());
        assert!(
            encoded_bytes(
                &mut encoder,
                DisplayUpdate::RGBAPointer(default_arrow_pointer()),
            )
            .await
                > 0
        );
        assert!(
            encoded_bytes(
                &mut encoder,
                DisplayUpdate::PointerPosition(Point16 { x: 123, y: 45 }),
            )
            .await
                > 0
        );
    }

    #[tokio::test(start_paused = true)]
    async fn access_screen_repaints_without_input_or_capture() {
        let size = test_size();
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let expected = gate.render_frame(size).rgba;
        let mut display = RdpDisplay::new(FrameHub::new(size), gate);
        let mut updates = display.updates().await.unwrap();
        assert_eq!(read_screen(updates.as_mut(), size).await, expected);
        assert_eq!(read_screen(updates.as_mut(), size).await, expected);
    }

    #[tokio::test]
    async fn display_control_request_reactivates_at_the_client_size() {
        let initial = DesktopSize {
            width: 1024,
            height: 768,
        };
        let resized = DesktopSize {
            width: 800,
            height: 600,
        };
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let mut display = RdpDisplay::new(FrameHub::new(initial), gate.clone());
        assert_eq!(
            display
                .request_initial_size(ironrdp_server::DesktopSize {
                    width: initial.width,
                    height: initial.height,
                })
                .await,
            ironrdp_server::DesktopSize {
                width: initial.width,
                height: initial.height,
            }
        );
        let mut updates = display.updates().await.unwrap();
        display.request_layout(
            DisplayControlMonitorLayout::new_single_primary_monitor(
                u32::from(resized.width),
                u32::from(resized.height),
                None,
                None,
            )
            .unwrap(),
        );

        let update = tokio::time::timeout(Duration::from_secs(2), updates.next_update())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(matches!(
            update,
            DisplayUpdate::Resize(ironrdp_server::DesktopSize {
                width: 800,
                height: 600,
            })
        ));
        assert_eq!(gate.snapshot().client_size(), Some(resized));

        // During Deactivation/Reactivation, Windows App repeats the original
        // Bitmap capability size. It must not overwrite the newer Display
        // Control layout that caused the reactivation.
        assert_eq!(
            display
                .request_initial_size(ironrdp_server::DesktopSize {
                    width: initial.width,
                    height: initial.height,
                })
                .await,
            ironrdp_server::DesktopSize {
                width: resized.width,
                height: resized.height,
            }
        );
        assert_eq!(
            display.size().await,
            ironrdp_server::DesktopSize {
                width: resized.width,
                height: resized.height,
            }
        );
    }

    #[tokio::test]
    async fn stale_dynamic_layout_does_not_leak_into_a_new_connection() {
        let initial = test_size();
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let mut display = RdpDisplay::new(FrameHub::new(initial), gate.clone());
        display.request_layout(
            DisplayControlMonitorLayout::new_single_primary_monitor(800, 600, None, None).unwrap(),
        );
        gate.reset();

        let next_client = ironrdp_server::DesktopSize {
            width: 1280,
            height: 720,
        };
        assert_eq!(
            display.request_initial_size(next_client).await,
            next_client,
            "a layout queued by a disconnected client must not resize the next connection"
        );
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
        let resized = DesktopSize {
            width: 320,
            height: 200,
        };
        hub.publish(CapturedFrame {
            width: resized.width,
            height: resized.height,
            rgba: vec![17; usize::from(resized.width) * usize::from(resized.height) * 4],
        });
        assert_eq!(hub.size(), resized);
    }

    #[tokio::test(start_paused = true)]
    async fn replacement_frame_cancels_the_agent_handoff_timeout() {
        let size = test_size();
        let hub = FrameHub::new(size);
        hub.set_unavailable_after(Duration::from_secs(3));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(
            hub.is_available(),
            "the last frame stays live during handoff"
        );

        hub.publish(CapturedFrame {
            width: size.width,
            height: size.height,
            rgba: vec![42; usize::from(size.width) * usize::from(size.height) * 4],
        });
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(
            hub.is_available(),
            "a replacement agent frame must cancel the old timeout"
        );

        hub.set_unavailable_after(Duration::from_secs(3));
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(
            !hub.is_available(),
            "a genuinely missing agent becomes unavailable after the grace period"
        );
    }

    #[test]
    fn desktop_scaling_produces_the_negotiated_client_size() {
        let source = CapturedFrame {
            width: 4,
            height: 2,
            rgba: [91, 91, 91, 255].repeat(4 * 2),
        };
        let target = DesktopSize {
            width: 2,
            height: 1,
        };
        let scaled = scale_frame(&source, target).unwrap();
        assert_eq!((scaled.width, scaled.height), (2, 1));
        assert_eq!(scaled.rgba, [91, 91, 91, 255].repeat(2));
    }

    #[test]
    fn desktop_scaling_preserves_aspect_ratio_with_centered_bars() {
        let source = CapturedFrame {
            width: 4,
            height: 2,
            rgba: [91, 92, 93, 255].repeat(4 * 2),
        };
        let target = DesktopSize {
            width: 4,
            height: 4,
        };
        assert_eq!(
            fitted_desktop_viewport(
                DesktopSize {
                    width: source.width,
                    height: source.height,
                },
                target,
            ),
            DesktopViewport {
                x: 0,
                y: 1,
                width: 4,
                height: 2,
            }
        );
        let scaled = scale_frame(&source, target).unwrap();
        let bar = [5, 10, 19, 255].repeat(4);
        let desktop = [91, 92, 93, 255].repeat(4);
        assert_eq!(&scaled.rgba[0..16], &bar);
        assert_eq!(&scaled.rgba[16..32], &desktop);
        assert_eq!(&scaled.rgba[32..48], &desktop);
        assert_eq!(&scaled.rgba[48..64], &bar);
    }

    #[test]
    fn desktop_scaling_uses_sharp_filters_instead_of_bilinear_interpolation() {
        assert_eq!(
            desktop_resize_algorithm(
                DesktopSize {
                    width: 1920,
                    height: 1200,
                },
                DesktopSize {
                    width: 2560,
                    height: 1600,
                },
            ),
            ResizeAlg::Convolution(FilterType::CatmullRom)
        );
        assert_eq!(
            desktop_resize_algorithm(
                DesktopSize {
                    width: 2560,
                    height: 1600,
                },
                DesktopSize {
                    width: 1920,
                    height: 1200,
                },
            ),
            ResizeAlg::Convolution(FilterType::Lanczos3)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn desktop_stays_gated_and_reconnect_returns_to_access_screen() {
        let size = test_size();
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let hub = FrameHub::new(size);
        let mut display = RdpDisplay::new(hub.clone(), gate.clone());
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
        gate.handle_keyboard(&KeyboardEvent::Pressed {
            code: 28,
            extended: false,
        });
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
                continue;
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
        let mut display = RdpDisplay::new(FrameHub::new(size), gate);
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
        let mut display = RdpDisplay::new(FrameHub::new(size), gate);
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
