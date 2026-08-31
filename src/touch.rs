use std::sync::Arc;

use ironrdp_server::{
    MouseButton, MouseEvent, RdpServerInputHandler, RdpeiHandler, RdpeiServer, RdpeiServerFactory,
    ServerEvent, ServerEventSender, TouchContact, TouchContactFlags, TouchEventPdu,
};
use tokio::sync::mpsc;

use crate::access::AccessGate;
use crate::display::FrameHub;
use crate::input::HostInputHandler;
use crate::platform::InputInjector;

/// Creates one MS-RDPEI endpoint per RDP connection. SunRDP currently maps the
/// primary direct-touch contact to the shared console mouse path; this makes
/// taps and single-finger drags work consistently on the access screen and the
/// mirrored Windows console without pretending to support multi-touch gestures.
pub struct DirectTouchFactory {
    injector: Arc<dyn InputInjector>,
    hub: FrameHub,
    allow_control: bool,
    access_gate: AccessGate,
}

impl DirectTouchFactory {
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
        }
    }
}

impl ServerEventSender for DirectTouchFactory {
    fn set_sender(&mut self, _sender: mpsc::UnboundedSender<ServerEvent>) {
        // RDPEI client input is handled synchronously by its DVC processor and
        // this factory has no proactive server messages to enqueue.
    }
}

impl RdpeiServerFactory for DirectTouchFactory {
    fn build_server(&self) -> RdpeiServer {
        RdpeiServer::new(Box::new(DirectTouchHandler {
            input: HostInputHandler::new(
                Arc::clone(&self.injector),
                self.hub.clone(),
                self.allow_control,
                self.access_gate.clone(),
            ),
            active_contact: None,
        }))
    }
}

struct DirectTouchHandler {
    input: HostInputHandler,
    active_contact: Option<u8>,
}

impl DirectTouchHandler {
    fn contact(&mut self, contact: TouchContact) {
        let flags = contact.contact_flags;
        let position = touch_position(&contact);

        if flags.contains(TouchContactFlags::DOWN) && self.active_contact.is_none() {
            self.active_contact = Some(contact.contact_id);
            self.input.mouse(MouseEvent::Button {
                x: position.0,
                y: position.1,
                button: MouseButton::Left,
                pressed: true,
            });
            return;
        }
        if self.active_contact != Some(contact.contact_id) {
            return;
        }
        if flags.contains(TouchContactFlags::UP) || flags.contains(TouchContactFlags::CANCELED) {
            self.input.mouse(MouseEvent::Button {
                x: position.0,
                y: position.1,
                button: MouseButton::Left,
                pressed: false,
            });
            self.active_contact = None;
        } else if flags.contains(TouchContactFlags::UPDATE) {
            self.input.mouse(MouseEvent::Move {
                x: position.0,
                y: position.1,
            });
        }
    }
}

impl RdpeiHandler for DirectTouchHandler {
    fn touch(&mut self, pdu: TouchEventPdu) {
        for frame in pdu.frames {
            for contact in frame.contacts {
                self.contact(contact);
            }
        }
    }
}

fn touch_position(contact: &TouchContact) -> (u16, u16) {
    (
        u16::try_from(contact.x.clamp(0, i32::from(u16::MAX))).unwrap_or(0),
        u16::try_from(contact.y.clamp(0, i32::from(u16::MAX))).unwrap_or(0),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use ironrdp_server::{KeyboardEvent, TouchContactFields, TouchFrame};

    use super::*;
    use crate::platform::DesktopSize;

    #[derive(Default)]
    struct RecordingInjector {
        buttons: Mutex<Vec<bool>>,
    }

    impl InputInjector for RecordingInjector {
        fn keyboard(&self, _event: &ironrdp_server::KeyboardEvent) {}

        fn mouse(&self, event: &MouseEvent, _desktop: DesktopSize) {
            if let MouseEvent::Button { pressed, .. } = event {
                self.buttons.lock().unwrap().push(*pressed);
            }
        }

        fn set_display_size(&self, _size: DesktopSize) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn contact(id: u8, x: i32, y: i32, flags: TouchContactFlags) -> TouchContact {
        TouchContact {
            contact_id: id,
            fields_present: ironrdp_server::TouchContactDataFlags::empty(),
            x,
            y,
            contact_flags: flags,
            fields: TouchContactFields::default(),
        }
    }

    #[test]
    fn direct_touch_tap_focuses_the_password_field_on_the_access_screen() {
        let size = DesktopSize {
            width: 1224,
            height: 2556,
        };
        let gate = AccessGate::new("unused.toml".into());
        gate.set_display_state(size, size, true);
        let before = gate.render_frame(size).rgba;
        let mut handler = DirectTouchHandler {
            input: HostInputHandler::new(
                Arc::new(RecordingInjector::default()),
                FrameHub::new(size),
                true,
                gate.clone(),
            ),
            active_contact: None,
        };

        handler.touch(TouchEventPdu::new(
            0,
            vec![TouchFrame::new(
                0,
                vec![contact(
                    7,
                    612,
                    1309,
                    TouchContactFlags::DOWN
                        | TouchContactFlags::INRANGE
                        | TouchContactFlags::INCONTACT,
                )],
            )],
        ));

        assert_ne!(gate.render_frame(size).rgba, before);
    }

    #[test]
    fn direct_touch_forwards_only_one_primary_contact_to_the_desktop() {
        let size = DesktopSize {
            width: 1280,
            height: 720,
        };
        let gate = AccessGate::new("unused.toml".into());
        gate.set_display_state(size, size, true);
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        gate.handle_keyboard(&KeyboardEvent::Pressed {
            code: 28,
            extended: false,
        });
        let injector = Arc::new(RecordingInjector::default());
        let mut handler = DirectTouchHandler {
            input: HostInputHandler::new(injector.clone(), FrameHub::new(size), true, gate),
            active_contact: None,
        };
        let down =
            TouchContactFlags::DOWN | TouchContactFlags::INRANGE | TouchContactFlags::INCONTACT;
        let up = TouchContactFlags::UP | TouchContactFlags::INRANGE;

        handler.touch(TouchEventPdu::new(
            0,
            vec![TouchFrame::new(
                0,
                vec![contact(1, 100, 100, down), contact(2, 200, 200, down)],
            )],
        ));
        handler.touch(TouchEventPdu::new(
            0,
            vec![TouchFrame::new(0, vec![contact(1, 100, 100, up)])],
        ));

        assert_eq!(*injector.buttons.lock().unwrap(), [true, false]);
    }
}
