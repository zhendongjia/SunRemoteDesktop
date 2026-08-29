use std::sync::Arc;

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

use crate::access::{AccessAction, AccessGate, DesktopPresentation};
use crate::display::{FrameHub, fitted_desktop_viewport};
use crate::platform::{DesktopSize, InputInjector};

pub struct HostInputHandler {
    injector: Arc<dyn InputInjector>,
    hub: FrameHub,
    allow_control: bool,
    access_gate: AccessGate,
    pointer_position: Option<(u16, u16)>,
}

impl HostInputHandler {
    pub fn new(
        injector: Arc<dyn InputInjector>,
        hub: FrameHub,
        allow_control: bool,
        access_gate: AccessGate,
    ) -> Self {
        Self {
            injector,
            hub,
            allow_control,
            access_gate,
            pointer_position: None,
        }
    }
}

impl RdpServerInputHandler for HostInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        if !self.access_gate.is_desktop_ready() {
            let action = self.access_gate.handle_keyboard(&event);
            self.execute_action(action);
        } else if self.allow_control {
            self.injector.keyboard(&event);
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let snapshot = self.access_gate.snapshot();
        let host_size = self.hub.size();
        let client_size = snapshot.client_size().unwrap_or(host_size);
        if let Some(position) = track_pointer_position(&event, client_size, self.pointer_position) {
            self.pointer_position = Some(position);
            self.hub.publish_pointer_position(position.0, position.1);
        }

        if !snapshot.is_desktop_ready() {
            // Android touchpad and mouse modes may send relative motion. The
            // access UI keeps its own hit-test position, so feed it the same
            // absolute client-canvas position that we publish to the visible
            // RDP pointer before handling a subsequent button press.
            let normalized_move = normalized_pointer_event(&event, self.pointer_position);
            let action = self
                .access_gate
                .handle_mouse(normalized_move.as_ref().unwrap_or(&event));
            self.execute_action(action);
        } else if self.allow_control {
            let normalized = normalized_pointer_event(&event, self.pointer_position);
            let event = normalized.unwrap_or(event);
            let event = map_mouse_event(event, client_size, host_size, snapshot.presentation());
            self.injector.mouse(&event, host_size);
        }
    }
}

fn normalized_pointer_event(
    event: &MouseEvent,
    pointer_position: Option<(u16, u16)>,
) -> Option<MouseEvent> {
    let (x, y) = pointer_position?;
    match event {
        MouseEvent::RelMove { .. } => Some(MouseEvent::Move { x, y }),
        MouseEvent::ButtonRel {
            button, pressed, ..
        } => Some(MouseEvent::Button {
            x,
            y,
            button: *button,
            pressed: *pressed,
        }),
        _ => None,
    }
}

fn track_pointer_position(
    event: &MouseEvent,
    client: DesktopSize,
    current: Option<(u16, u16)>,
) -> Option<(u16, u16)> {
    let max_x = client.width.saturating_sub(1);
    let max_y = client.height.saturating_sub(1);
    match event {
        MouseEvent::Move { x, y } | MouseEvent::Button { x, y, .. } => {
            Some(((*x).min(max_x), (*y).min(max_y)))
        }
        MouseEvent::RelMove { x, y } | MouseEvent::ButtonRel { x, y, .. } => {
            let (current_x, current_y) = current.unwrap_or((max_x / 2, max_y / 2));
            Some((
                (i64::from(current_x) + i64::from(*x)).clamp(0, i64::from(max_x)) as u16,
                (i64::from(current_y) + i64::from(*y)).clamp(0, i64::from(max_y)) as u16,
            ))
        }
        _ => None,
    }
}

impl HostInputHandler {
    fn execute_action(&self, action: Option<AccessAction>) {
        let Some(AccessAction::ChangeDisplaySize(size)) = action else {
            return;
        };
        if let Err(error) = self.injector.set_display_size(size) {
            self.access_gate.resolution_change_failed(&error);
        }
    }
}

fn map_mouse_event(
    event: MouseEvent,
    client: DesktopSize,
    host: DesktopSize,
    presentation: Option<DesktopPresentation>,
) -> MouseEvent {
    if presentation != Some(DesktopPresentation::Scale) {
        return event;
    }
    match event {
        MouseEvent::Move { x, y } => {
            let viewport = fitted_desktop_viewport(host, client);
            MouseEvent::Move {
                x: map_viewport_axis(x, viewport.x, viewport.width, host.width),
                y: map_viewport_axis(y, viewport.y, viewport.height, host.height),
            }
        }
        MouseEvent::Button {
            x,
            y,
            button,
            pressed,
        } => {
            let viewport = fitted_desktop_viewport(host, client);
            MouseEvent::Button {
                x: map_viewport_axis(x, viewport.x, viewport.width, host.width),
                y: map_viewport_axis(y, viewport.y, viewport.height, host.height),
                button,
                pressed,
            }
        }
        other => other,
    }
}

fn map_viewport_axis(value: u16, offset: u16, source_extent: u16, target_extent: u16) -> u16 {
    let offset = u32::from(offset);
    let source_max = u32::from(source_extent.saturating_sub(1)).max(1);
    let target_max = u32::from(target_extent.saturating_sub(1));
    let local = u32::from(value)
        .clamp(offset, offset + source_max)
        .saturating_sub(offset);
    ((local * target_max) / source_max) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopInputInjector;

    impl InputInjector for NoopInputInjector {
        fn keyboard(&self, _event: &KeyboardEvent) {}

        fn mouse(&self, _event: &MouseEvent, _desktop: DesktopSize) {}

        fn set_display_size(&self, _size: DesktopSize) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn scaled_pointer_maps_client_edges_to_host_edges() {
        let mapped = map_mouse_event(
            MouseEvent::Move { x: 1919, y: 1079 },
            DesktopSize {
                width: 1920,
                height: 1080,
            },
            DesktopSize {
                width: 2560,
                height: 1600,
            },
            Some(DesktopPresentation::Scale),
        );
        assert!(matches!(mapped, MouseEvent::Move { x: 2559, y: 1599 }));
    }

    #[test]
    fn pointer_mapping_accounts_for_letterbox_bars() {
        let client = DesktopSize {
            width: 1920,
            height: 1080,
        };
        let host = DesktopSize {
            width: 2560,
            height: 1600,
        };
        let left_bar = map_mouse_event(
            MouseEvent::Move { x: 0, y: 540 },
            client,
            host,
            Some(DesktopPresentation::Scale),
        );
        let right_bar = map_mouse_event(
            MouseEvent::Move { x: 1919, y: 540 },
            client,
            host,
            Some(DesktopPresentation::Scale),
        );
        let center = map_mouse_event(
            MouseEvent::Move { x: 960, y: 540 },
            client,
            host,
            Some(DesktopPresentation::Scale),
        );
        assert!(matches!(left_bar, MouseEvent::Move { x: 0, .. }));
        assert!(matches!(right_bar, MouseEvent::Move { x: 2559, .. }));
        assert!(matches!(center, MouseEvent::Move { x: 1280, y: 800 }));
    }

    #[test]
    fn relative_mouse_mode_tracks_a_visible_client_pointer() {
        let size = DesktopSize {
            width: 1224,
            height: 2556,
        };
        assert_eq!(
            track_pointer_position(&MouseEvent::RelMove { x: 12, y: -20 }, size, None),
            Some((623, 1257))
        );
        assert_eq!(
            track_pointer_position(
                &MouseEvent::RelMove {
                    x: 10_000,
                    y: -10_000
                },
                size,
                Some((623, 1257)),
            ),
            Some((1223, 0))
        );
    }

    #[test]
    fn relative_mouse_mode_updates_access_screen_hit_testing() {
        let position = Some((623, 1257));
        assert!(matches!(
            normalized_pointer_event(&MouseEvent::RelMove { x: 12, y: -20 }, position),
            Some(MouseEvent::Move { x: 623, y: 1257 })
        ));
        assert!(normalized_pointer_event(&MouseEvent::Move { x: 1, y: 2 }, position).is_none());
    }

    #[test]
    fn relative_mouse_mode_can_focus_the_password_field() {
        let size = DesktopSize {
            width: 1224,
            height: 2556,
        };
        let gate = AccessGate::new("unused.toml".into());
        gate.set_display_state(size, size, true);
        let before = gate.render_frame(size).rgba;
        let mut input = HostInputHandler::new(
            Arc::new(NoopInputInjector),
            FrameHub::new(size),
            true,
            gate.clone(),
        );

        // The first relative event starts at the client-canvas center. This
        // reaches the password field center in the real 1224x2556 Android
        // layout, then presses it through the same HostInputHandler path used
        // by the RDP server.
        input.mouse(MouseEvent::RelMove { x: 1, y: 32 });
        input.mouse(MouseEvent::ButtonRel {
            x: 0,
            y: 0,
            button: ironrdp_server::MouseButton::Left,
            pressed: true,
        });

        assert_ne!(
            gate.render_frame(size).rgba,
            before,
            "clicking through relative mouse mode must change the focused field"
        );
    }
}
