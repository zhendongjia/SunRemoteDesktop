use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use ironrdp_server::ServerEvent;
use tokio::sync::mpsc;

use crate::display::FrameHub;
use crate::platform::{DesktopSize, InputInjector};

type SessionId = u64;

#[derive(Clone)]
pub struct SessionCoordinator {
    inner: Arc<CoordinatorInner>,
}

struct CoordinatorInner {
    state: Mutex<CoordinatorState>,
    maximum_transports: usize,
    hub: FrameHub,
    injector: Arc<dyn InputInjector>,
}

#[derive(Default)]
struct CoordinatorState {
    next_id: SessionId,
    owner: Option<SessionId>,
    original_display_size: Option<DesktopSize>,
    sessions: BTreeMap<SessionId, SessionEntry>,
}

struct SessionEntry {
    peer: SocketAddr,
    quit: Option<mpsc::UnboundedSender<ServerEvent>>,
}

#[derive(Clone)]
pub struct SessionLease {
    id: SessionId,
    coordinator: SessionCoordinator,
}

impl SessionCoordinator {
    pub fn new(configured_maximum: u32, hub: FrameHub, injector: Arc<dyn InputInjector>) -> Self {
        // Keep the configured number of normal connections plus one bounded
        // candidate slot. The extra client stays behind the access gate until
        // it authenticates and explicitly takes ownership.
        let maximum_transports = usize::try_from(configured_maximum.max(1))
            .unwrap_or(usize::MAX - 1)
            .saturating_add(1);
        Self {
            inner: Arc::new(CoordinatorInner {
                state: Mutex::new(CoordinatorState::default()),
                maximum_transports,
                hub,
                injector,
            }),
        }
    }

    pub fn reserve(&self, peer: SocketAddr) -> Option<SessionLease> {
        let mut state = self.lock_state();
        if state.sessions.len() >= self.inner.maximum_transports {
            return None;
        }

        state.next_id = state.next_id.wrapping_add(1).max(1);
        let id = state.next_id;
        if state.owner.is_none() {
            state.owner = Some(id);
            state.original_display_size = Some(self.inner.hub.size());
        }
        state.sessions.insert(id, SessionEntry { peer, quit: None });
        Some(SessionLease {
            id,
            coordinator: self.clone(),
        })
    }

    pub fn disconnect_all(&self, reason: &str) {
        let senders = {
            let state = self.lock_state();
            state
                .sessions
                .values()
                .filter_map(|entry| entry.quit.clone())
                .collect::<Vec<_>>()
        };
        for sender in senders {
            let _ = sender.send(ServerEvent::Quit(reason.to_string()));
        }
    }

    pub fn active_count(&self) -> usize {
        self.lock_state().sessions.len()
    }

    fn unregister(&self, id: SessionId) {
        let restore = {
            let mut state = self.lock_state();
            let Some(entry) = state.sessions.remove(&id) else {
                return;
            };
            let was_owner = state.owner == Some(id);
            tracing::info!(session_id = id, peer = %entry.peer, was_owner, "SunRDP session released");
            if was_owner {
                if let Some(next_owner) = state.sessions.keys().next().copied() {
                    state.owner = Some(next_owner);
                    tracing::info!(
                        session_id = next_owner,
                        "remaining SunRDP session inherited display ownership"
                    );
                    None
                } else {
                    state.owner = None;
                    state.original_display_size.take()
                }
            } else {
                None
            }
        };

        if let Some(original) = restore
            && self.inner.hub.size() != original
        {
            if let Err(error) = self.inner.injector.set_display_size(original) {
                tracing::warn!(
                    ?error,
                    width = original.width,
                    height = original.height,
                    "unable to restore the physical display after the owning client disconnected"
                );
            } else {
                tracing::info!(
                    width = original.width,
                    height = original.height,
                    "restoring the physical display after the owning client disconnected"
                );
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, CoordinatorState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl SessionLease {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn attach(&self, sender: mpsc::UnboundedSender<ServerEvent>) {
        if let Some(entry) = self.coordinator.lock_state().sessions.get_mut(&self.id) {
            entry.quit = Some(sender);
        }
    }

    pub fn has_other_owner(&self) -> bool {
        self.coordinator
            .lock_state()
            .owner
            .is_some_and(|owner| owner != self.id)
    }

    pub fn is_owner(&self) -> bool {
        self.coordinator.lock_state().owner == Some(self.id)
    }

    pub fn take_over(&self) -> usize {
        let senders = {
            let mut state = self.coordinator.lock_state();
            if !state.sessions.contains_key(&self.id) {
                return 0;
            }
            if state.owner != Some(self.id) {
                if state.original_display_size.is_none() {
                    state.original_display_size = Some(self.coordinator.inner.hub.size());
                }
                state.owner = Some(self.id);
            }
            state
                .sessions
                .iter()
                .filter(|(id, _)| **id != self.id)
                .filter_map(|(_, entry)| entry.quit.clone())
                .collect::<Vec<_>>()
        };
        let count = senders.len();
        for sender in senders {
            let _ = sender.send(ServerEvent::Quit(
                "another authenticated client took over the physical console".to_string(),
            ));
        }
        tracing::info!(
            session_id = self.id,
            disconnected_clients = count,
            "authenticated SunRDP client took ownership"
        );
        count
    }

    pub fn close(&self) {
        self.coordinator.unregister(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::CapturedFrame;

    #[derive(Default)]
    struct RecordingInjector {
        display_sizes: Mutex<Vec<DesktopSize>>,
    }

    impl InputInjector for RecordingInjector {
        fn keyboard(&self, _event: &ironrdp_server::KeyboardEvent) {}

        fn mouse(&self, _event: &ironrdp_server::MouseEvent, _desktop: DesktopSize) {}

        fn set_display_size(&self, size: DesktopSize) -> anyhow::Result<()> {
            self.display_sizes.lock().unwrap().push(size);
            Ok(())
        }
    }

    fn size(width: u16, height: u16) -> DesktopSize {
        DesktopSize { width, height }
    }

    #[test]
    fn authenticated_candidate_can_take_over_and_kick_the_owner() {
        let hub = FrameHub::new(size(1366, 768));
        let injector = Arc::new(RecordingInjector::default());
        let coordinator = SessionCoordinator::new(1, hub, injector);
        let owner = coordinator
            .reserve("127.0.0.1:1001".parse().unwrap())
            .unwrap();
        let candidate = coordinator
            .reserve("127.0.0.1:1002".parse().unwrap())
            .unwrap();
        let (owner_quit, mut owner_events) = mpsc::unbounded_channel();
        owner.attach(owner_quit);

        assert!(!owner.has_other_owner());
        assert!(candidate.has_other_owner());
        assert_eq!(candidate.take_over(), 1);
        assert!(candidate.is_owner());
        assert!(matches!(
            owner_events.try_recv(),
            Ok(ServerEvent::Quit(reason)) if reason.contains("took over")
        ));
    }

    #[test]
    fn takeover_transfers_display_restore_ownership() {
        let original = size(1366, 768);
        let hub = FrameHub::new(original);
        let injector = Arc::new(RecordingInjector::default());
        let coordinator = SessionCoordinator::new(1, hub.clone(), injector.clone());
        let owner = coordinator
            .reserve("127.0.0.1:1001".parse().unwrap())
            .unwrap();
        let candidate = coordinator
            .reserve("127.0.0.1:1002".parse().unwrap())
            .unwrap();
        candidate.take_over();
        hub.publish(CapturedFrame {
            width: 1280,
            height: 720,
            rgba: vec![0; 1280 * 720 * 4],
        });

        owner.close();
        assert!(injector.display_sizes.lock().unwrap().is_empty());
        candidate.close();
        assert_eq!(*injector.display_sizes.lock().unwrap(), vec![original]);
    }

    #[test]
    fn remaining_candidate_inherits_ownership_without_restoring_early() {
        let original = size(1366, 768);
        let hub = FrameHub::new(original);
        let injector = Arc::new(RecordingInjector::default());
        let coordinator = SessionCoordinator::new(1, hub.clone(), injector.clone());
        let owner = coordinator
            .reserve("127.0.0.1:1001".parse().unwrap())
            .unwrap();
        let candidate = coordinator
            .reserve("127.0.0.1:1002".parse().unwrap())
            .unwrap();
        hub.publish(CapturedFrame {
            width: 1280,
            height: 720,
            rgba: vec![0; 1280 * 720 * 4],
        });

        owner.close();
        assert!(candidate.is_owner());
        assert!(injector.display_sizes.lock().unwrap().is_empty());
        candidate.close();
        assert_eq!(*injector.display_sizes.lock().unwrap(), vec![original]);
    }

    #[test]
    fn one_candidate_slot_is_reserved_beyond_the_configured_limit() {
        let hub = FrameHub::new(size(1366, 768));
        let coordinator = SessionCoordinator::new(1, hub, Arc::new(RecordingInjector::default()));
        assert!(
            coordinator
                .reserve("127.0.0.1:1001".parse().unwrap())
                .is_some()
        );
        assert!(
            coordinator
                .reserve("127.0.0.1:1002".parse().unwrap())
                .is_some()
        );
        assert!(
            coordinator
                .reserve("127.0.0.1:1003".parse().unwrap())
                .is_none()
        );
    }
}
