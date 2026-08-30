use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use fontdue::{Font, FontSettings};
use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent};
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::platform::{CapturedFrame, DesktopSize};
use crate::session::SessionLease;

const MAX_FIELD_LENGTH: usize = 256;

#[derive(Clone)]
pub struct AccessGate {
    inner: Arc<AccessGateInner>,
}

struct AccessGateInner {
    config_path: PathBuf,
    session: Option<SessionLease>,
    state: Mutex<AccessState>,
    sender: watch::Sender<AccessSnapshot>,
}

#[derive(Clone, Debug)]
pub struct AccessSnapshot {
    username: String,
    password_length: usize,
    focus: AccessField,
    status: AccessStatus,
    authenticated: bool,
    client_size: Option<DesktopSize>,
    host_size: Option<DesktopSize>,
    host_available: bool,
    resolution_selection: ResolutionSelection,
    resolution_policy: Option<ResolutionSelection>,
    presentation: Option<DesktopPresentation>,
    takeover_required: bool,
    disconnect_others: bool,
}

impl AccessSnapshot {
    pub fn is_desktop_ready(&self) -> bool {
        self.authenticated && self.presentation.is_some() && !self.takeover_required
    }

    pub fn presentation(&self) -> Option<DesktopPresentation> {
        self.presentation
    }

    pub fn client_size(&self) -> Option<DesktopSize> {
        self.client_size
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopPresentation {
    Native,
    Scale,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AccessAction {
    ChangeDisplaySize(DesktopSize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessField {
    Username,
    Password,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ResolutionSelection {
    Scale,
    MatchDisplay,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessStatus {
    Ready,
    MissingCredentials,
    Checking,
    Rejected,
    BackendError,
    ResolutionRequired,
    WaitingForDesktop,
    TakeoverConfirmationRequired,
    Granted,
}

struct AccessState {
    username: String,
    password: String,
    focus: AccessField,
    status: AccessStatus,
    authenticated: bool,
    shift: bool,
    caps_lock: bool,
    generation: u64,
    client_size: Option<DesktopSize>,
    host_size: Option<DesktopSize>,
    host_available: bool,
    resolution_selection: ResolutionSelection,
    resolution_policy: Option<ResolutionSelection>,
    presentation: Option<DesktopPresentation>,
    takeover_required: bool,
    disconnect_others: bool,
    pointer: (u16, u16),
}

impl Default for AccessState {
    fn default() -> Self {
        Self {
            username: String::new(),
            password: String::new(),
            focus: AccessField::Username,
            status: AccessStatus::Ready,
            authenticated: false,
            shift: false,
            caps_lock: false,
            generation: 0,
            client_size: None,
            host_size: None,
            host_available: false,
            resolution_selection: ResolutionSelection::Scale,
            resolution_policy: None,
            presentation: None,
            takeover_required: false,
            disconnect_others: false,
            pointer: (0, 0),
        }
    }
}

impl AccessState {
    fn snapshot(&self) -> AccessSnapshot {
        AccessSnapshot {
            username: self.username.clone(),
            password_length: self.password.chars().count(),
            focus: self.focus,
            status: self.status,
            authenticated: self.authenticated,
            client_size: self.client_size,
            host_size: self.host_size,
            host_available: self.host_available,
            resolution_selection: self.resolution_selection,
            resolution_policy: self.resolution_policy,
            presentation: self.presentation,
            takeover_required: self.takeover_required,
            disconnect_others: self.disconnect_others,
        }
    }
}

impl AccessGate {
    pub fn new(config_path: PathBuf) -> Self {
        Self::new_with_session(config_path, None)
    }

    pub(crate) fn new_for_session(config_path: PathBuf, session: SessionLease) -> Self {
        Self::new_with_session(config_path, Some(session))
    }

    fn new_with_session(config_path: PathBuf, session: Option<SessionLease>) -> Self {
        let state = AccessState::default();
        let (sender, _) = watch::channel(state.snapshot());
        Self {
            inner: Arc::new(AccessGateInner {
                config_path,
                session,
                state: Mutex::new(state),
                sender,
            }),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<AccessSnapshot> {
        self.inner.sender.subscribe()
    }

    pub fn snapshot(&self) -> AccessSnapshot {
        self.lock_state().snapshot()
    }

    pub(crate) fn connection_generation(&self) -> u64 {
        self.lock_state().generation
    }

    pub fn is_desktop_ready(&self) -> bool {
        self.lock_state().snapshot().is_desktop_ready()
    }

    pub fn reset(&self) {
        let mut state = self.lock_state();
        let generation = state.generation.wrapping_add(1);
        let host_size = state.host_size;
        let host_available = state.host_available;
        *state = AccessState {
            generation,
            host_size,
            host_available,
            ..AccessState::default()
        };
        self.publish(&state);
    }

    pub fn set_display_sizes(&self, client_size: DesktopSize, host_size: DesktopSize) {
        self.set_display_state(client_size, host_size, true);
    }

    /// Applies a client-side dynamic-resolution request.
    ///
    /// The physical console follows the client only after the user explicitly
    /// selected the "match physical display" option for this connection. In
    /// scaling mode the RDP canvas still follows the client, but the host mode
    /// remains untouched.
    pub fn request_client_resize(&self, client_size: DesktopSize) -> Option<AccessAction> {
        let mut state = self.lock_state();
        let old_snapshot = state.snapshot();
        state.client_size = Some(client_size);

        let action = if !state.authenticated {
            None
        } else if !self.owns_display() {
            state.takeover_required = true;
            state.presentation = None;
            state.status = AccessStatus::ResolutionRequired;
            None
        } else if !state.host_available {
            state.presentation = None;
            state.status = AccessStatus::WaitingForDesktop;
            None
        } else {
            let matches_host = state.host_size == Some(client_size);
            match state.resolution_policy {
                Some(ResolutionSelection::Scale) => {
                    state.presentation = Some(if matches_host {
                        DesktopPresentation::Native
                    } else {
                        DesktopPresentation::Scale
                    });
                    state.status = AccessStatus::Granted;
                    None
                }
                Some(ResolutionSelection::MatchDisplay) => {
                    state.presentation = Some(if matches_host {
                        DesktopPresentation::Native
                    } else {
                        DesktopPresentation::Scale
                    });
                    state.status = AccessStatus::Granted;
                    (!matches_host).then_some(AccessAction::ChangeDisplaySize(client_size))
                }
                None if matches_host => {
                    // No choice is necessary when both sides already match. A
                    // later resize defaults to the non-invasive scaling policy.
                    state.resolution_policy = Some(ResolutionSelection::Scale);
                    state.presentation = Some(DesktopPresentation::Native);
                    state.status = AccessStatus::Granted;
                    None
                }
                None => {
                    state.presentation = None;
                    state.status = AccessStatus::ResolutionRequired;
                    None
                }
            }
        };

        if state.snapshot() != old_snapshot {
            self.publish(&state);
        }
        action
    }

    pub fn should_follow_client_size(&self, client_size: DesktopSize) -> bool {
        let state = self.lock_state();
        state.authenticated
            && state.host_available
            && self.owns_display()
            && state.resolution_policy == Some(ResolutionSelection::MatchDisplay)
            && state.client_size == Some(client_size)
    }

    pub fn set_display_state(
        &self,
        client_size: DesktopSize,
        host_size: DesktopSize,
        host_available: bool,
    ) {
        let mut state = self.lock_state();
        let old_snapshot = state.snapshot();
        state.client_size = Some(client_size);
        state.host_size = Some(host_size);
        state.host_available = host_available;
        state.takeover_required = state.authenticated && self.has_other_owner();

        if state.authenticated {
            if state.takeover_required {
                state.presentation = None;
                state.status = AccessStatus::ResolutionRequired;
            } else if !host_available {
                state.presentation = None;
                state.status = AccessStatus::WaitingForDesktop;
            } else if state.client_size == state.host_size {
                state.presentation = Some(DesktopPresentation::Native);
                state.status = AccessStatus::Granted;
            } else if state.resolution_policy == Some(ResolutionSelection::Scale) {
                // The first choice keeps the physical mode fixed even if it is
                // later changed manually in Windows display settings.
                state.presentation = Some(DesktopPresentation::Scale);
                state.status = AccessStatus::Granted;
            } else if state.resolution_policy == Some(ResolutionSelection::MatchDisplay)
                && state.presentation == Some(DesktopPresentation::Scale)
            {
                // A dynamically requested physical mode may only approximate
                // an arbitrary window size. Keep the exact RDP canvas and
                // letterbox the nearest physical mode when necessary.
                state.status = AccessStatus::Granted;
            } else {
                state.presentation = None;
                state.status = AccessStatus::ResolutionRequired;
            }
        }

        if state.snapshot() != old_snapshot {
            self.publish(&state);
        }
    }

    pub fn show_login(&self) {
        let mut state = self.lock_state();
        if !state.authenticated && state.status != AccessStatus::Checking {
            state.status = AccessStatus::Ready;
            self.publish(&state);
        }
    }

    pub fn begin_validation(&self, username: &str) -> u64 {
        let mut state = self.lock_state();
        state.username = username.trim().to_string();
        state.password.clear();
        state.status = AccessStatus::Checking;
        state.authenticated = false;
        state.resolution_policy = None;
        state.presentation = None;
        state.takeover_required = false;
        state.disconnect_others = false;
        let generation = state.generation;
        self.publish(&state);
        generation
    }

    pub fn finish_validation(&self, generation: u64, username: &str, result: anyhow::Result<bool>) {
        let mut state = self.lock_state();
        if state.generation != generation {
            return;
        }
        state.password.clear();
        match result {
            Ok(true) => {
                state.username = username.trim().to_string();
                state.authenticated = true;
                state.takeover_required = self.has_other_owner();
                state.disconnect_others = false;
                if state.takeover_required {
                    state.status = AccessStatus::ResolutionRequired;
                    state.resolution_policy = None;
                    state.presentation = None;
                    state.resolution_selection = ResolutionSelection::Scale;
                } else if !state.host_available {
                    state.status = AccessStatus::WaitingForDesktop;
                    state.presentation = None;
                } else if state.client_size.is_some() && state.client_size == state.host_size {
                    state.status = AccessStatus::Granted;
                    state.resolution_policy = Some(ResolutionSelection::Scale);
                    state.presentation = Some(DesktopPresentation::Native);
                } else {
                    state.status = AccessStatus::ResolutionRequired;
                    state.resolution_policy = None;
                    state.presentation = None;
                    state.resolution_selection = ResolutionSelection::Scale;
                }
                tracing::info!(user = %state.username, "SunRDP access granted");
            }
            Ok(false) => {
                state.status = AccessStatus::Rejected;
                state.authenticated = false;
                state.resolution_policy = None;
                state.presentation = None;
                state.focus = AccessField::Password;
                tracing::warn!(user = %state.username, "SunRDP access rejected");
            }
            Err(error) => {
                state.status = AccessStatus::BackendError;
                state.authenticated = false;
                state.resolution_policy = None;
                state.presentation = None;
                state.focus = AccessField::Password;
                tracing::error!(?error, "SunRDP local account validation failed");
            }
        }
        self.publish(&state);
    }

    pub fn handle_keyboard(&self, event: &KeyboardEvent) -> Option<AccessAction> {
        let mut submission = None;
        let action = {
            let mut state = self.lock_state();
            if state.snapshot().is_desktop_ready() || state.status == AccessStatus::Checking {
                return None;
            }

            if state.authenticated {
                if !state.host_available {
                    return None;
                }
                let apply = handle_resolution_keyboard(&mut state, event);
                let action = apply
                    .then(|| self.apply_resolution_choice(&mut state))
                    .flatten();
                if action.is_some() || !matches!(event, KeyboardEvent::Released { .. }) {
                    self.publish(&state);
                }
                action
            } else {
                let (changed, submit) = handle_login_keyboard(&mut state, event);
                if changed {
                    state.status = AccessStatus::Ready;
                }
                if submit {
                    submission = prepare_submission(&mut state);
                }
                if changed || submit {
                    self.publish(&state);
                }
                None
            }
        };
        self.spawn_validation(submission);
        action
    }

    pub fn handle_mouse(&self, event: &MouseEvent) -> Option<AccessAction> {
        let mut submission = None;
        let action = {
            let mut state = self.lock_state();
            match event {
                MouseEvent::Move { x, y } => {
                    state.pointer = (*x, *y);
                    return None;
                }
                MouseEvent::Button {
                    x,
                    y,
                    button: MouseButton::Left,
                    pressed: true,
                } => state.pointer = (*x, *y),
                _ => return None,
            }

            if state.snapshot().is_desktop_ready() || state.status == AccessStatus::Checking {
                return None;
            }
            let size = state.client_size?;
            let layout = UiLayout::new(size, state.authenticated, state.takeover_required);
            let (x, y) = state.pointer;
            if state.authenticated {
                if !state.host_available {
                    return None;
                }
                if layout.primary.contains(x, y) {
                    state.resolution_selection = ResolutionSelection::Scale;
                    state.status = AccessStatus::ResolutionRequired;
                    self.publish(&state);
                    None
                } else if layout.secondary.contains(x, y) {
                    state.resolution_selection = ResolutionSelection::MatchDisplay;
                    state.status = AccessStatus::ResolutionRequired;
                    self.publish(&state);
                    None
                } else if layout.takeover.is_some_and(|rect| rect.contains(x, y)) {
                    state.disconnect_others = !state.disconnect_others;
                    state.status = AccessStatus::ResolutionRequired;
                    self.publish(&state);
                    None
                } else if layout.submit.contains(x, y) {
                    let action = self.apply_resolution_choice(&mut state);
                    self.publish(&state);
                    action
                } else {
                    None
                }
            } else {
                if layout.primary.contains(x, y) {
                    state.focus = AccessField::Username;
                    state.status = AccessStatus::Ready;
                    self.publish(&state);
                } else if layout.secondary.contains(x, y) {
                    state.focus = AccessField::Password;
                    state.status = AccessStatus::Ready;
                    self.publish(&state);
                } else if layout.submit.contains(x, y) {
                    submission = prepare_submission(&mut state);
                    self.publish(&state);
                }
                None
            }
        };
        self.spawn_validation(submission);
        action
    }

    pub fn resolution_change_failed(&self, error: &anyhow::Error) {
        let mut state = self.lock_state();
        tracing::warn!(
            ?error,
            "unable to change the physical display resolution; continuing with proportional scaling"
        );
        if state.authenticated && state.resolution_policy == Some(ResolutionSelection::MatchDisplay)
        {
            state.presentation = Some(DesktopPresentation::Scale);
            state.status = AccessStatus::Granted;
            self.publish(&state);
        }
    }

    pub fn render_frame(&self, size: DesktopSize) -> CapturedFrame {
        let snapshot = self.inner.sender.borrow().clone();
        render_access_frame(size, &snapshot)
    }

    fn has_other_owner(&self) -> bool {
        self.inner
            .session
            .as_ref()
            .is_some_and(SessionLease::has_other_owner)
    }

    fn owns_display(&self) -> bool {
        self.inner
            .session
            .as_ref()
            .is_none_or(SessionLease::is_owner)
    }

    fn apply_resolution_choice(&self, state: &mut AccessState) -> Option<AccessAction> {
        let has_other_owner = self.has_other_owner();
        state.takeover_required = has_other_owner;
        if has_other_owner && !state.disconnect_others {
            state.status = AccessStatus::TakeoverConfirmationRequired;
            state.presentation = None;
            return None;
        }

        if let Some(session) = self.inner.session.as_ref()
            && !session.is_owner()
        {
            session.take_over();
        }
        state.takeover_required = false;
        state.disconnect_others = false;
        apply_resolution_choice(state)
    }

    fn spawn_validation(&self, submission: Option<ValidationSubmission>) {
        let Some((generation, username, password)) = submission else {
            return;
        };
        let gate = self.clone();
        let config_path = self.inner.config_path.clone();
        let thread_username = username.clone();
        let spawn_result = std::thread::Builder::new()
            .name("sunrdp-account-validation".to_string())
            .spawn(move || {
                let result = crate::auth::verify_account(&config_path, &thread_username, &password);
                gate.finish_validation(generation, &thread_username, result);
            });
        if let Err(error) = spawn_result {
            self.finish_validation(generation, &username, Err(error.into()));
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AccessState> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish(&self, state: &AccessState) {
        self.inner.sender.send_replace(state.snapshot());
    }
}

type ValidationSubmission = (u64, String, Zeroizing<String>);

fn handle_login_keyboard(state: &mut AccessState, event: &KeyboardEvent) -> (bool, bool) {
    let mut changed = false;
    let mut submit = false;
    match event {
        KeyboardEvent::Pressed { code: 42 | 54, .. } => state.shift = true,
        KeyboardEvent::Released { code: 42 | 54, .. } => state.shift = false,
        KeyboardEvent::Pressed { code: 58, .. } => state.caps_lock = !state.caps_lock,
        KeyboardEvent::Pressed { code: 14, .. } => {
            active_field_mut(state).pop();
            changed = true;
        }
        KeyboardEvent::Pressed { code: 15, .. } => {
            state.focus = match state.focus {
                AccessField::Username => AccessField::Password,
                AccessField::Password => AccessField::Username,
            };
            changed = true;
        }
        KeyboardEvent::Pressed { code: 28, .. } => submit = true,
        KeyboardEvent::Pressed { code, extended } if !extended => {
            if let Some(character) = scan_code_character(*code, state.shift, state.caps_lock) {
                changed = push_character(state, character);
            }
        }
        KeyboardEvent::UnicodePressed(code) => {
            if let Some(character) = char::from_u32(u32::from(*code))
                && !character.is_control()
            {
                changed = push_character(state, character);
            }
        }
        _ => {}
    }
    (changed, submit)
}

fn handle_resolution_keyboard(state: &mut AccessState, event: &KeyboardEvent) -> bool {
    match event {
        KeyboardEvent::Pressed {
            code: 15 | 75 | 77 | 72 | 80,
            ..
        } => {
            state.resolution_selection = match state.resolution_selection {
                ResolutionSelection::Scale => ResolutionSelection::MatchDisplay,
                ResolutionSelection::MatchDisplay => ResolutionSelection::Scale,
            };
            state.status = AccessStatus::ResolutionRequired;
            false
        }
        KeyboardEvent::Pressed { code: 57, .. } if state.takeover_required => {
            state.disconnect_others = !state.disconnect_others;
            state.status = AccessStatus::ResolutionRequired;
            false
        }
        KeyboardEvent::Pressed { code: 28, .. } => true,
        _ => false,
    }
}

fn apply_resolution_choice(state: &mut AccessState) -> Option<AccessAction> {
    match state.resolution_selection {
        ResolutionSelection::Scale => {
            state.resolution_policy = Some(ResolutionSelection::Scale);
            state.presentation = Some(DesktopPresentation::Scale);
            state.status = AccessStatus::Granted;
            None
        }
        ResolutionSelection::MatchDisplay => {
            let target = state.client_size?;
            state.resolution_policy = Some(ResolutionSelection::MatchDisplay);
            // A physical display can expose only a finite set of modes, while
            // clients (especially phones in portrait orientation) request
            // arbitrary canvas sizes. Start proportional presentation now and
            // let an exact capture size upgrade it to Native asynchronously.
            state.presentation = Some(DesktopPresentation::Scale);
            state.status = AccessStatus::Granted;
            Some(AccessAction::ChangeDisplaySize(target))
        }
    }
}

fn prepare_submission(state: &mut AccessState) -> Option<ValidationSubmission> {
    if state.username.trim().is_empty() || state.password.is_empty() {
        state.status = AccessStatus::MissingCredentials;
        return None;
    }
    state.status = AccessStatus::Checking;
    let username = state.username.trim().to_string();
    let password = Zeroizing::new(std::mem::take(&mut state.password));
    Some((state.generation, username, password))
}

fn active_field_mut(state: &mut AccessState) -> &mut String {
    match state.focus {
        AccessField::Username => &mut state.username,
        AccessField::Password => &mut state.password,
    }
}

fn push_character(state: &mut AccessState, character: char) -> bool {
    let field = active_field_mut(state);
    if field.chars().count() >= MAX_FIELD_LENGTH {
        return false;
    }
    field.push(character);
    true
}

fn scan_code_character(code: u8, shift: bool, caps_lock: bool) -> Option<char> {
    let letter = match code {
        16 => Some('q'),
        17 => Some('w'),
        18 => Some('e'),
        19 => Some('r'),
        20 => Some('t'),
        21 => Some('y'),
        22 => Some('u'),
        23 => Some('i'),
        24 => Some('o'),
        25 => Some('p'),
        30 => Some('a'),
        31 => Some('s'),
        32 => Some('d'),
        33 => Some('f'),
        34 => Some('g'),
        35 => Some('h'),
        36 => Some('j'),
        37 => Some('k'),
        38 => Some('l'),
        44 => Some('z'),
        45 => Some('x'),
        46 => Some('c'),
        47 => Some('v'),
        48 => Some('b'),
        49 => Some('n'),
        50 => Some('m'),
        _ => None,
    };
    if let Some(letter) = letter {
        return Some(if shift ^ caps_lock {
            letter.to_ascii_uppercase()
        } else {
            letter
        });
    }

    let pair = match code {
        2 => ('1', '!'),
        3 => ('2', '@'),
        4 => ('3', '#'),
        5 => ('4', '$'),
        6 => ('5', '%'),
        7 => ('6', '^'),
        8 => ('7', '&'),
        9 => ('8', '*'),
        10 => ('9', '('),
        11 => ('0', ')'),
        12 => ('-', '_'),
        13 => ('=', '+'),
        26 => ('[', '{'),
        27 => (']', '}'),
        39 => (';', ':'),
        40 => ('\'', '"'),
        41 => ('`', '~'),
        43 => ('\\', '|'),
        51 => (',', '<'),
        52 => ('.', '>'),
        53 => ('/', '?'),
        57 => (' ', ' '),
        71 => ('7', '7'),
        72 => ('8', '8'),
        73 => ('9', '9'),
        75 => ('4', '4'),
        76 => ('5', '5'),
        77 => ('6', '6'),
        79 => ('1', '1'),
        80 => ('2', '2'),
        81 => ('3', '3'),
        82 => ('0', '0'),
        83 => ('.', '.'),
        _ => return None,
    };
    Some(if shift { pair.1 } else { pair.0 })
}

#[derive(Clone, Copy, Debug, Default)]
struct Rect {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

impl Rect {
    fn contains(self, x: u16, y: u16) -> bool {
        let x = f32::from(x);
        let y = f32::from(y);
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }
}

struct UiLayout {
    card: Rect,
    content: Rect,
    primary: Rect,
    secondary: Rect,
    takeover: Option<Rect>,
    submit: Rect,
    compact: bool,
}

impl UiLayout {
    fn new(size: DesktopSize, resolution: bool, takeover_required: bool) -> Self {
        let width = f32::from(size.width);
        let height = f32::from(size.height);
        let compact = height < 620.0 || width < 760.0;
        let margin = if compact { 16.0 } else { 28.0 };
        let desired_height: f32 = if resolution && takeover_required {
            720.0
        } else if resolution {
            610.0
        } else {
            620.0
        };
        let desired_width: f32 = if resolution { 720.0 } else { 610.0 };
        let card = Rect {
            width: desired_width.min(width - margin * 2.0).max(320.0),
            height: desired_height.min(height - margin * 2.0).max(360.0),
            x: 0.0,
            y: 0.0,
        };
        let card = Rect {
            x: (width - card.width) / 2.0,
            y: (height - card.height) / 2.0,
            ..card
        };
        let padding = if compact { 28.0 } else { 46.0 };
        let content = Rect {
            x: card.x + padding,
            y: card.y + padding,
            width: card.width - padding * 2.0,
            height: card.height - padding * 2.0,
        };
        let field_height = if compact { 50.0 } else { 58.0 };
        let (primary_y, secondary_y, submit_y, item_height) = if resolution {
            let first = content.y
                + if takeover_required {
                    if compact { 116.0 } else { 138.0 }
                } else if compact {
                    134.0
                } else {
                    164.0
                };
            let option_height = if takeover_required {
                if compact { 62.0 } else { 72.0 }
            } else if compact {
                72.0
            } else {
                86.0
            };
            (
                first,
                first + option_height + 12.0,
                content.y + content.height - field_height,
                option_height,
            )
        } else {
            let first = content.y + if compact { 132.0 } else { 166.0 };
            (
                first,
                first + field_height + 42.0,
                content.y + content.height - field_height - 34.0,
                field_height,
            )
        };
        Self {
            card,
            content,
            primary: Rect {
                x: content.x,
                y: primary_y,
                width: content.width,
                height: item_height,
            },
            secondary: Rect {
                x: content.x,
                y: secondary_y,
                width: content.width,
                height: item_height,
            },
            takeover: (resolution && takeover_required).then_some(Rect {
                x: content.x,
                y: secondary_y + item_height + 10.0,
                width: content.width,
                height: item_height,
            }),
            submit: Rect {
                x: content.x,
                y: submit_y,
                width: content.width,
                height: field_height,
            },
            compact,
        }
    }
}

fn render_access_frame(size: DesktopSize, snapshot: &AccessSnapshot) -> CapturedFrame {
    let mut canvas = Canvas::new(size);
    canvas.background();
    let waiting = snapshot.authenticated && !snapshot.host_available;
    let resolution =
        snapshot.authenticated && snapshot.host_available && !snapshot.is_desktop_ready();
    let layout = UiLayout::new(size, resolution, snapshot.takeover_required);
    canvas.rounded_rect(layout.card, 22.0, [16, 27, 46, 245]);
    canvas.rounded_outline(layout.card, 22.0, 1.0, [74, 96, 129, 150]);
    canvas.brand(layout.content.x, layout.content.y, layout.compact);
    if waiting {
        render_waiting(&mut canvas, layout, snapshot);
    } else if resolution {
        render_resolution(&mut canvas, layout, snapshot);
    } else {
        render_login(&mut canvas, layout, snapshot);
    }
    CapturedFrame {
        width: size.width,
        height: size.height,
        rgba: canvas.rgba,
    }
}

fn render_waiting(canvas: &mut Canvas, layout: UiLayout, snapshot: &AccessSnapshot) {
    let title_size = if layout.compact { 25.0 } else { 32.0 };
    let title_y = layout.content.y + if layout.compact { 52.0 } else { 76.0 };
    canvas.text(
        layout.content.x,
        title_y,
        "Waiting for the physical console",
        title_size,
        [246, 249, 255, 255],
        layout.content.width,
    );
    let client = snapshot.client_size.unwrap_or(DesktopSize {
        width: 0,
        height: 0,
    });
    canvas.text(
        layout.content.x,
        title_y + title_size + 10.0,
        &format!("Remote client  {} × {}", client.width, client.height),
        if layout.compact { 13.0 } else { 15.0 },
        [153, 169, 194, 255],
        layout.content.width,
    );
    let panel = Rect {
        x: layout.content.x,
        y: title_y + title_size + if layout.compact { 68.0 } else { 92.0 },
        width: layout.content.width,
        height: if layout.compact { 112.0 } else { 138.0 },
    };
    canvas.rounded_rect(panel, 14.0, [10, 20, 36, 255]);
    canvas.rounded_outline(panel, 14.0, 1.0, [67, 85, 112, 220]);
    canvas.circle(panel.x + 32.0, panel.y + 35.0, 9.0, [91, 194, 255, 255]);
    canvas.text(
        panel.x + 54.0,
        panel.y + 20.0,
        "SunRDP is ready and your access is verified",
        if layout.compact { 15.0 } else { 17.0 },
        [239, 244, 252, 255],
        panel.width - 74.0,
    );
    canvas.text(
        panel.x + 54.0,
        panel.y + if layout.compact { 51.0 } else { 58.0 },
        "Physical console capture is not available yet.",
        if layout.compact { 12.5 } else { 14.0 },
        [139, 156, 182, 255],
        panel.width - 74.0,
    );
    canvas.text(
        panel.x + 54.0,
        panel.y + if layout.compact { 75.0 } else { 84.0 },
        "No desktop frames or input are being forwarded.",
        if layout.compact { 12.5 } else { 14.0 },
        [139, 156, 182, 255],
        panel.width - 74.0,
    );
    canvas.text(
        layout.content.x,
        layout.content.y + layout.content.height - 22.0,
        "This page will continue automatically when the physical console becomes available.",
        13.0,
        [126, 143, 169, 255],
        layout.content.width,
    );
}

fn render_login(canvas: &mut Canvas, layout: UiLayout, snapshot: &AccessSnapshot) {
    let title_size = if layout.compact { 27.0 } else { 34.0 };
    let title_y = layout.content.y + if layout.compact { 42.0 } else { 52.0 };
    canvas.text(
        layout.content.x,
        title_y,
        "Connect securely",
        title_size,
        [246, 249, 255, 255],
        layout.content.width,
    );
    canvas.text(
        layout.content.x,
        title_y + title_size + 6.0,
        "Sign in with an allowed local Windows account",
        if layout.compact { 13.0 } else { 15.0 },
        [153, 169, 194, 255],
        layout.content.width,
    );

    canvas.label(layout.primary.x, layout.primary.y - 23.0, "WINDOWS ACCOUNT");
    canvas.field(layout.primary, snapshot.focus == AccessField::Username);
    let username = if snapshot.username.is_empty() {
        "Computer\\username"
    } else {
        &snapshot.username
    };
    let username_color = if snapshot.username.is_empty() {
        [113, 130, 155, 255]
    } else {
        [239, 244, 252, 255]
    };
    canvas.text(
        layout.primary.x + 17.0,
        layout.primary.y + 13.0,
        username,
        if layout.compact { 17.0 } else { 19.0 },
        username_color,
        layout.primary.width - 34.0,
    );

    canvas.label(layout.secondary.x, layout.secondary.y - 23.0, "PASSWORD");
    canvas.field(layout.secondary, snapshot.focus == AccessField::Password);
    let masked = "•".repeat(snapshot.password_length.min(48));
    let password = if masked.is_empty() {
        "Enter your password"
    } else {
        &masked
    };
    let password_color = if masked.is_empty() {
        [113, 130, 155, 255]
    } else {
        [239, 244, 252, 255]
    };
    canvas.text(
        layout.secondary.x + 17.0,
        layout.secondary.y + 13.0,
        password,
        if layout.compact { 17.0 } else { 19.0 },
        password_color,
        layout.secondary.width - 34.0,
    );

    canvas.button(layout.submit, "Connect");
    let (message, color) = match snapshot.status {
        AccessStatus::MissingCredentials => {
            ("Enter both your account and password", [255, 190, 92, 255])
        }
        AccessStatus::Checking => ("Checking your local Windows account…", [91, 194, 255, 255]),
        AccessStatus::Rejected => (
            "Access denied. Check the account or allow-list.",
            [255, 114, 122, 255],
        ),
        AccessStatus::BackendError => (
            "Authentication service unavailable. Try again.",
            [255, 114, 122, 255],
        ),
        _ => (
            "Tab switches fields  •  Enter connects",
            [126, 143, 169, 255],
        ),
    };
    canvas.text(
        layout.content.x,
        layout.submit.y + layout.submit.height + 12.0,
        message,
        13.0,
        color,
        layout.content.width,
    );
}

fn render_resolution(canvas: &mut Canvas, layout: UiLayout, snapshot: &AccessSnapshot) {
    let title_size = if layout.compact { 25.0 } else { 32.0 };
    let title_y = layout.content.y + if layout.compact { 40.0 } else { 52.0 };
    canvas.text(
        layout.content.x,
        title_y,
        "Choose the display fit",
        title_size,
        [246, 249, 255, 255],
        layout.content.width,
    );
    let client = snapshot.client_size.unwrap_or(DesktopSize {
        width: 0,
        height: 0,
    });
    let host = snapshot.host_size.unwrap_or(DesktopSize {
        width: 0,
        height: 0,
    });
    let dimensions = format!(
        "Remote client  {} × {}     Shared screen  {} × {}",
        client.width, client.height, host.width, host.height
    );
    canvas.text(
        layout.content.x,
        title_y + title_size + 7.0,
        &dimensions,
        if layout.compact { 12.5 } else { 14.0 },
        [153, 169, 194, 255],
        layout.content.width,
    );

    canvas.option_card(
        layout.primary,
        snapshot.resolution_selection == ResolutionSelection::Scale,
        "Scale to this window",
        "Keep proportions and add bars when needed",
        true,
        false,
    );
    canvas.option_card(
        layout.secondary,
        snapshot.resolution_selection == ResolutionSelection::MatchDisplay,
        "Match the physical display",
        &format!(
            "Change the local screen to {} × {}",
            client.width, client.height
        ),
        false,
        false,
    );
    if let Some(takeover) = layout.takeover {
        canvas.option_card(
            takeover,
            snapshot.disconnect_others,
            "Disconnect other clients",
            "Take control of the shared physical console",
            false,
            true,
        );
    }

    canvas.button(layout.submit, "Continue");
    let (message, color) = match snapshot.status {
        AccessStatus::TakeoverConfirmationRequired => (
            "Confirm disconnecting the current client before continuing",
            [255, 190, 92, 255],
        ),
        _ if snapshot.takeover_required => (
            "Tab changes fit  •  Space confirms takeover  •  Enter continues",
            [126, 143, 169, 255],
        ),
        _ => (
            "Tab or arrow keys switch options  •  Enter continues",
            [126, 143, 169, 255],
        ),
    };
    canvas.text(
        layout.content.x,
        layout.submit.y + layout.submit.height + 12.0,
        message,
        13.0,
        color,
        layout.content.width,
    );
}

struct Canvas {
    size: DesktopSize,
    rgba: Vec<u8>,
}

impl Canvas {
    fn new(size: DesktopSize) -> Self {
        Self {
            size,
            rgba: vec![0; usize::from(size.width) * usize::from(size.height) * 4],
        }
    }

    fn background(&mut self) {
        let width = usize::from(self.size.width);
        let height = usize::from(self.size.height);
        for y in 0..height {
            let t = y as f32 / height.max(1) as f32;
            let color = [
                (7.0 + 5.0 * t) as u8,
                (14.0 + 10.0 * t) as u8,
                (29.0 + 16.0 * t) as u8,
                255,
            ];
            for x in 0..width {
                let offset = (y * width + x) * 4;
                self.rgba[offset..offset + 4].copy_from_slice(&color);
            }
        }
        let radius =
            (f32::from(self.size.width).min(f32::from(self.size.height)) * 0.38).max(120.0);
        self.circle(
            f32::from(self.size.width) * 0.13,
            f32::from(self.size.height) * 0.18,
            radius,
            [255, 162, 48, 18],
        );
        self.circle(
            f32::from(self.size.width) * 0.88,
            f32::from(self.size.height) * 0.88,
            radius * 0.8,
            [44, 135, 255, 15],
        );
    }

    fn brand(&mut self, x: f32, y: f32, compact: bool) {
        let size = if compact { 25.0 } else { 29.0 };
        self.circle(
            x + size * 0.48,
            y + size * 0.48,
            size * 0.46,
            [255, 174, 54, 255],
        );
        self.circle(
            x + size * 0.48,
            y + size * 0.48,
            size * 0.20,
            [255, 223, 134, 255],
        );
        self.text(
            x + size + 10.0,
            y - 1.0,
            "SunRemoteDesktop",
            if compact { 18.0 } else { 20.0 },
            [240, 245, 253, 255],
            360.0,
        );
        self.text(
            x + size + 10.0,
            y + if compact { 19.0 } else { 22.0 },
            "SunRDP protected access",
            11.5,
            [126, 143, 169, 255],
            360.0,
        );
    }

    fn label(&mut self, x: f32, y: f32, text: &str) {
        self.text(x, y, text, 11.5, [153, 169, 194, 255], 400.0);
    }

    fn field(&mut self, rect: Rect, focused: bool) {
        if focused {
            self.rounded_rect(
                Rect {
                    x: rect.x - 3.0,
                    y: rect.y - 3.0,
                    width: rect.width + 6.0,
                    height: rect.height + 6.0,
                },
                12.0,
                [255, 174, 54, 42],
            );
        }
        self.rounded_rect(rect, 10.0, [10, 20, 36, 255]);
        self.rounded_outline(
            rect,
            10.0,
            if focused { 2.0 } else { 1.0 },
            if focused {
                [255, 174, 54, 255]
            } else {
                [67, 85, 112, 220]
            },
        );
    }

    fn option_card(
        &mut self,
        rect: Rect,
        selected: bool,
        title: &str,
        detail: &str,
        recommended: bool,
        checkbox: bool,
    ) {
        if selected {
            self.rounded_rect(
                Rect {
                    x: rect.x - 3.0,
                    y: rect.y - 3.0,
                    width: rect.width + 6.0,
                    height: rect.height + 6.0,
                },
                14.0,
                [255, 174, 54, 35],
            );
        }
        self.rounded_rect(
            rect,
            12.0,
            if selected {
                [35, 39, 48, 255]
            } else {
                [11, 22, 39, 255]
            },
        );
        self.rounded_outline(
            rect,
            12.0,
            if selected { 2.0 } else { 1.0 },
            if selected {
                [255, 174, 54, 255]
            } else {
                [67, 85, 112, 220]
            },
        );
        let indicator = Rect {
            x: rect.x + 16.0,
            y: rect.y + rect.height / 2.0 - 8.0,
            width: 16.0,
            height: 16.0,
        };
        if checkbox {
            self.rounded_rect(
                indicator,
                4.0,
                if selected {
                    [255, 174, 54, 255]
                } else {
                    [42, 57, 80, 255]
                },
            );
            self.rounded_outline(indicator, 4.0, 1.0, [92, 108, 132, 255]);
            if selected {
                self.rounded_rect(
                    Rect {
                        x: indicator.x + 4.0,
                        y: indicator.y + 4.0,
                        width: 8.0,
                        height: 8.0,
                    },
                    2.0,
                    [255, 244, 216, 255],
                );
            }
        } else {
            self.circle(
                rect.x + 24.0,
                rect.y + rect.height / 2.0,
                8.0,
                if selected {
                    [255, 174, 54, 255]
                } else {
                    [42, 57, 80, 255]
                },
            );
            if selected {
                self.circle(
                    rect.x + 24.0,
                    rect.y + rect.height / 2.0,
                    3.0,
                    [255, 244, 216, 255],
                );
            }
        }
        let title_y = rect.y + if rect.height < 80.0 { 13.0 } else { 16.0 };
        self.text(
            rect.x + 45.0,
            title_y,
            title,
            17.0,
            [240, 245, 253, 255],
            rect.width - 65.0,
        );
        self.text(
            rect.x + 45.0,
            title_y + 25.0,
            detail,
            12.5,
            [139, 156, 182, 255],
            rect.width - 65.0,
        );
        if recommended && rect.width > 440.0 {
            let badge = Rect {
                x: rect.x + rect.width - 112.0,
                y: rect.y + 14.0,
                width: 94.0,
                height: 24.0,
            };
            self.rounded_rect(badge, 12.0, [81, 57, 20, 255]);
            self.text(
                badge.x + 11.0,
                badge.y + 5.0,
                "RECOMMENDED",
                9.5,
                [255, 201, 103, 255],
                badge.width - 20.0,
            );
        }
    }

    fn button(&mut self, rect: Rect, label: &str) {
        self.rounded_rect(rect, 11.0, [255, 169, 42, 255]);
        self.rounded_outline(rect, 11.0, 1.0, [255, 213, 126, 255]);
        self.centered_text(rect, label, 17.0, [28, 23, 15, 255]);
    }

    fn centered_text(&mut self, rect: Rect, text: &str, px: f32, color: [u8; 4]) {
        let width = measure_text(text, px);
        let x = rect.x + ((rect.width - width).max(0.0) / 2.0);
        let y = rect.y + ((rect.height - px).max(0.0) / 2.0) - 1.0;
        self.text(x, y, text, px, color, rect.width);
    }

    fn text(&mut self, x: f32, y: f32, text: &str, px: f32, color: [u8; 4], max_width: f32) {
        if let Some(font) = system_font() {
            let baseline = y + font
                .horizontal_line_metrics(px)
                .map_or(px * 0.82, |metrics| metrics.ascent);
            let mut cursor = x;
            for character in text.chars() {
                let metrics = font.metrics(character, px);
                if cursor + metrics.advance_width > x + max_width {
                    break;
                }
                let (metrics, bitmap) = font.rasterize(character, px);
                let glyph_x = cursor + metrics.xmin as f32;
                let glyph_y = baseline - metrics.height as f32 - metrics.ymin as f32;
                for row in 0..metrics.height {
                    for column in 0..metrics.width {
                        let alpha = bitmap[row * metrics.width + column];
                        if alpha != 0 {
                            self.blend_pixel(
                                (glyph_x + column as f32) as i32,
                                (glyph_y + row as f32) as i32,
                                color,
                                alpha,
                            );
                        }
                    }
                }
                cursor += metrics.advance_width;
            }
        } else {
            self.bitmap_text(
                x as usize,
                y as usize,
                text,
                (px / 8.0).round().max(1.0) as usize,
                color,
                (x + max_width) as usize,
            );
        }
    }

    fn rounded_rect(&mut self, rect: Rect, radius: f32, color: [u8; 4]) {
        self.paint_round_rect(rect, radius, color, false, 0.0);
    }

    fn rounded_outline(&mut self, rect: Rect, radius: f32, thickness: f32, color: [u8; 4]) {
        self.paint_round_rect(rect, radius, color, true, thickness);
    }

    fn paint_round_rect(
        &mut self,
        rect: Rect,
        radius: f32,
        color: [u8; 4],
        outline: bool,
        thickness: f32,
    ) {
        let min_x = rect.x.max(0.0) as i32;
        let min_y = rect.y.max(0.0) as i32;
        let max_x = (rect.x + rect.width).min(f32::from(self.size.width)).ceil() as i32;
        let max_y = (rect.y + rect.height)
            .min(f32::from(self.size.height))
            .ceil() as i32;
        for y in min_y..max_y {
            for x in min_x..max_x {
                if inside_rounded(rect, radius, x as f32 + 0.5, y as f32 + 0.5)
                    && (!outline
                        || !inside_rounded(
                            Rect {
                                x: rect.x + thickness,
                                y: rect.y + thickness,
                                width: rect.width - thickness * 2.0,
                                height: rect.height - thickness * 2.0,
                            },
                            (radius - thickness).max(0.0),
                            x as f32 + 0.5,
                            y as f32 + 0.5,
                        ))
                {
                    self.blend_pixel(x, y, color, 255);
                }
            }
        }
    }

    fn circle(&mut self, center_x: f32, center_y: f32, radius: f32, color: [u8; 4]) {
        let min_x = (center_x - radius).max(0.0) as i32;
        let min_y = (center_y - radius).max(0.0) as i32;
        let max_x = (center_x + radius).min(f32::from(self.size.width)).ceil() as i32;
        let max_y = (center_y + radius).min(f32::from(self.size.height)).ceil() as i32;
        let radius_squared = radius * radius;
        for y in min_y..max_y {
            for x in min_x..max_x {
                let dx = x as f32 + 0.5 - center_x;
                let dy = y as f32 + 0.5 - center_y;
                if dx * dx + dy * dy <= radius_squared {
                    self.blend_pixel(x, y, color, 255);
                }
            }
        }
    }

    fn blend_pixel(&mut self, x: i32, y: i32, color: [u8; 4], coverage: u8) {
        if x < 0 || y < 0 || x >= i32::from(self.size.width) || y >= i32::from(self.size.height) {
            return;
        }
        let offset = (y as usize * usize::from(self.size.width) + x as usize) * 4;
        let alpha = u16::from(color[3]) * u16::from(coverage) / 255;
        let inverse = 255 - alpha;
        for (channel, source) in color.iter().copied().enumerate().take(3) {
            self.rgba[offset + channel] = ((u16::from(source) * alpha
                + u16::from(self.rgba[offset + channel]) * inverse)
                / 255) as u8;
        }
        self.rgba[offset + 3] = 255;
    }

    fn bitmap_text(
        &mut self,
        x: usize,
        y: usize,
        text: &str,
        scale: usize,
        color: [u8; 4],
        max_x: usize,
    ) {
        let mut cursor_x = x;
        for character in text.chars() {
            if let Some(glyph) = BASIC_FONTS.get(character) {
                for (glyph_y, row) in glyph.iter().enumerate() {
                    for glyph_x in 0..8 {
                        if row & (1 << glyph_x) != 0 {
                            let rect = Rect {
                                x: (cursor_x + glyph_x * scale) as f32,
                                y: (y + glyph_y * scale) as f32,
                                width: scale as f32,
                                height: scale as f32,
                            };
                            self.rounded_rect(rect, 0.0, color);
                        }
                    }
                }
            }
            cursor_x += 9 * scale;
            if cursor_x >= max_x {
                break;
            }
        }
    }
}

fn inside_rounded(rect: Rect, radius: f32, x: f32, y: f32) -> bool {
    if rect.width <= 0.0
        || rect.height <= 0.0
        || x < rect.x
        || y < rect.y
        || x >= rect.x + rect.width
        || y >= rect.y + rect.height
    {
        return false;
    }
    let radius = radius.min(rect.width / 2.0).min(rect.height / 2.0);
    let nearest_x = x.clamp(rect.x + radius, rect.x + rect.width - radius);
    let nearest_y = y.clamp(rect.y + radius, rect.y + rect.height - radius);
    let dx = x - nearest_x;
    let dy = y - nearest_y;
    dx * dx + dy * dy <= radius * radius
}

fn system_font() -> Option<&'static Font> {
    static FONT: OnceLock<Option<Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        let windows = std::env::var_os("WINDIR").map(PathBuf::from)?;
        ["segoeui.ttf", "arial.ttf"]
            .into_iter()
            .find_map(|name| std::fs::read(windows.join("Fonts").join(name)).ok())
            .and_then(|bytes| Font::from_bytes(bytes, FontSettings::default()).ok())
    })
    .as_ref()
}

fn measure_text(text: &str, px: f32) -> f32 {
    if let Some(font) = system_font() {
        text.chars()
            .map(|character| font.metrics(character, px).advance_width)
            .sum()
    } else {
        text.chars().count() as f32 * 9.0 * (px / 8.0).round().max(1.0)
    }
}

impl PartialEq for AccessSnapshot {
    fn eq(&self, other: &Self) -> bool {
        self.username == other.username
            && self.password_length == other.password_length
            && self.focus == other.focus
            && self.status == other.status
            && self.authenticated == other.authenticated
            && self.client_size == other.client_size
            && self.host_size == other.host_size
            && self.host_available == other.host_available
            && self.resolution_selection == other.resolution_selection
            && self.resolution_policy == other.resolution_policy
            && self.presentation == other.presentation
            && self.takeover_required == other.takeover_required
            && self.disconnect_others == other.disconnect_others
    }
}

#[cfg(test)]
mod tests {
    use crate::display::FrameHub;
    use crate::platform::InputInjector;
    use crate::session::SessionCoordinator;

    use super::*;

    #[derive(Default)]
    struct RecordingInjector {
        display_sizes: Mutex<Vec<DesktopSize>>,
    }

    impl InputInjector for RecordingInjector {
        fn keyboard(&self, _event: &KeyboardEvent) {}

        fn mouse(&self, _event: &MouseEvent, _desktop: DesktopSize) {}

        fn set_display_size(&self, size: DesktopSize) -> anyhow::Result<()> {
            self.display_sizes.lock().unwrap().push(size);
            Ok(())
        }
    }

    fn size(width: u16, height: u16) -> DesktopSize {
        DesktopSize { width, height }
    }

    #[test]
    fn scan_code_mapping_handles_letters_and_symbols() {
        assert_eq!(scan_code_character(30, false, false), Some('a'));
        assert_eq!(scan_code_character(30, true, false), Some('A'));
        assert_eq!(scan_code_character(30, false, true), Some('A'));
        assert_eq!(scan_code_character(2, true, false), Some('!'));
        assert_eq!(scan_code_character(43, false, false), Some('\\'));
    }

    #[test]
    fn access_frame_matches_the_client_size() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let client = size(640, 480);
        gate.set_display_sizes(client, size(2560, 1600));
        let frame = gate.render_frame(client);
        assert_eq!((frame.width, frame.height), (client.width, client.height));
        assert_eq!(frame.rgba.len(), 640 * 480 * 4);
    }

    #[test]
    fn mismatched_displays_stay_gated_until_scaling_is_selected() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        gate.set_display_sizes(size(1920, 1080), size(2560, 1600));
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        assert!(!gate.is_desktop_ready());
        assert_eq!(
            gate.handle_keyboard(&KeyboardEvent::Pressed {
                code: 28,
                extended: false
            }),
            None
        );
        assert_eq!(
            gate.snapshot().presentation(),
            Some(DesktopPresentation::Scale)
        );
        assert!(gate.is_desktop_ready());
    }

    #[test]
    fn matching_displays_open_without_an_extra_prompt() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        gate.set_display_sizes(size(1920, 1080), size(1920, 1080));
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        assert_eq!(
            gate.snapshot().presentation(),
            Some(DesktopPresentation::Native)
        );
        assert!(gate.is_desktop_ready());
    }

    #[test]
    fn scaling_choice_never_requests_a_physical_resize() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        gate.set_display_sizes(size(1920, 1080), size(2560, 1600));
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        assert_eq!(
            gate.handle_keyboard(&KeyboardEvent::Pressed {
                code: 28,
                extended: false,
            }),
            None
        );

        assert_eq!(gate.request_client_resize(size(1600, 900)), None);
        assert_eq!(
            gate.snapshot().presentation(),
            Some(DesktopPresentation::Scale)
        );
        assert!(!gate.should_follow_client_size(size(1600, 900)));
    }

    #[test]
    fn match_display_choice_follows_later_client_resizes() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let initial_client = size(1920, 1200);
        gate.set_display_sizes(initial_client, size(2560, 1600));
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        gate.handle_keyboard(&KeyboardEvent::Pressed {
            code: 15,
            extended: false,
        });
        assert_eq!(
            gate.handle_keyboard(&KeyboardEvent::Pressed {
                code: 28,
                extended: false,
            }),
            Some(AccessAction::ChangeDisplaySize(initial_client))
        );
        assert_eq!(
            gate.snapshot().presentation(),
            Some(DesktopPresentation::Scale)
        );
        assert!(gate.is_desktop_ready());
        gate.set_display_sizes(initial_client, initial_client);

        let resized = size(1600, 1000);
        assert_eq!(
            gate.request_client_resize(resized),
            Some(AccessAction::ChangeDisplaySize(resized))
        );
        assert_eq!(
            gate.snapshot().presentation(),
            Some(DesktopPresentation::Scale)
        );
        assert!(gate.should_follow_client_size(resized));
        gate.set_display_sizes(resized, resized);
        assert_eq!(
            gate.snapshot().presentation(),
            Some(DesktopPresentation::Native)
        );
    }

    #[test]
    fn match_display_accepts_the_closest_mode_for_a_portrait_phone() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let phone = size(1224, 2556);
        gate.set_display_sizes(phone, size(1920, 1200));
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        gate.handle_keyboard(&KeyboardEvent::Pressed {
            code: 15,
            extended: false,
        });
        assert_eq!(
            gate.handle_keyboard(&KeyboardEvent::Pressed {
                code: 28,
                extended: false,
            }),
            Some(AccessAction::ChangeDisplaySize(phone))
        );

        // Windows selected this closest available physical mode in the real
        // Android regression. The exact phone canvas stays active and the
        // physical frame is proportionally fitted instead of showing an error.
        gate.set_display_sizes(phone, size(1280, 1440));
        assert_eq!(
            gate.snapshot().presentation(),
            Some(DesktopPresentation::Scale)
        );
        assert_eq!(gate.snapshot().status, AccessStatus::Granted);
        assert!(gate.is_desktop_ready());
    }

    #[test]
    fn authentication_stays_gated_while_the_console_agent_is_unavailable() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        gate.set_display_state(size(1920, 1080), size(2560, 1600), false);
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));
        assert!(!gate.is_desktop_ready());
        assert_eq!(gate.snapshot().status, AccessStatus::WaitingForDesktop);
        gate.set_display_state(size(1920, 1080), size(2560, 1600), true);
        assert_eq!(gate.snapshot().status, AccessStatus::ResolutionRequired);
    }

    #[test]
    fn authenticated_candidate_must_confirm_before_kicking_the_owner() {
        let desktop = size(1366, 768);
        let coordinator = SessionCoordinator::new(
            1,
            FrameHub::new(desktop),
            Arc::new(RecordingInjector::default()),
        );
        let owner = coordinator
            .reserve("127.0.0.1:1001".parse().unwrap())
            .unwrap();
        let candidate = coordinator
            .reserve("127.0.0.1:1002".parse().unwrap())
            .unwrap();
        let (owner_quit, mut owner_events) = tokio::sync::mpsc::unbounded_channel();
        owner.attach(owner_quit);
        let gate = AccessGate::new_for_session(PathBuf::from("unused.toml"), candidate.clone());
        gate.set_display_sizes(desktop, desktop);
        let generation = gate.begin_validation("test-user");
        gate.finish_validation(generation, "test-user", Ok(true));

        assert!(gate.snapshot().takeover_required);
        assert!(!gate.is_desktop_ready());
        assert_eq!(
            gate.handle_keyboard(&KeyboardEvent::Pressed {
                code: 28,
                extended: false,
            }),
            None
        );
        assert_eq!(
            gate.snapshot().status,
            AccessStatus::TakeoverConfirmationRequired
        );
        assert!(owner_events.try_recv().is_err());

        gate.handle_keyboard(&KeyboardEvent::Pressed {
            code: 57,
            extended: false,
        });
        assert!(gate.snapshot().disconnect_others);
        assert_eq!(
            gate.handle_keyboard(&KeyboardEvent::Pressed {
                code: 28,
                extended: false,
            }),
            None
        );
        assert!(candidate.is_owner());
        assert!(gate.is_desktop_ready());
        assert!(matches!(
            owner_events.try_recv(),
            Ok(ironrdp_server::ServerEvent::Quit(reason)) if reason.contains("took over")
        ));
    }

    #[test]
    fn compact_takeover_layout_keeps_confirmation_above_continue() {
        let layout = UiLayout::new(size(640, 480), true, true);
        let takeover = layout.takeover.expect("takeover option");
        assert!(takeover.y + takeover.height < layout.submit.y);
    }
}
