use std::sync::Arc;

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};

use crate::platform::{DesktopSize, InputInjector};

pub struct HostInputHandler {
    injector: Arc<dyn InputInjector>,
    desktop: DesktopSize,
    allow_control: bool,
}

impl HostInputHandler {
    pub fn new(
        injector: Arc<dyn InputInjector>,
        desktop: DesktopSize,
        allow_control: bool,
    ) -> Self {
        Self {
            injector,
            desktop,
            allow_control,
        }
    }
}

impl RdpServerInputHandler for HostInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        if self.allow_control {
            self.injector.keyboard(&event);
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        if self.allow_control {
            self.injector.mouse(&event, self.desktop);
        }
    }
}
