use std::sync::Arc;

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

use crate::access::AccessGate;
use crate::platform::{DesktopSize, InputInjector};

pub struct HostInputHandler {
    injector: Arc<dyn InputInjector>,
    desktop: DesktopSize,
    allow_control: bool,
    access_gate: AccessGate,
}

impl HostInputHandler {
    pub fn new(
        injector: Arc<dyn InputInjector>,
        desktop: DesktopSize,
        allow_control: bool,
        access_gate: AccessGate,
    ) -> Self {
        Self {
            injector,
            desktop,
            allow_control,
            access_gate,
        }
    }
}

impl RdpServerInputHandler for HostInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        if !self.access_gate.is_authenticated() {
            self.access_gate.handle_keyboard(&event);
        } else if self.allow_control {
            self.injector.keyboard(&event);
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        if self.access_gate.is_authenticated() && self.allow_control {
            self.injector.mouse(&event, self.desktop);
        }
    }
}
