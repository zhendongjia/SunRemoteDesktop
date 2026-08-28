use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use font8x8::{BASIC_FONTS, UnicodeFonts};
use ironrdp_server::KeyboardEvent;
use tokio::sync::watch;
use zeroize::Zeroizing;

use crate::platform::{CapturedFrame, DesktopSize};

const MAX_FIELD_LENGTH: usize = 256;

#[derive(Clone)]
pub struct AccessGate {
    inner: Arc<AccessGateInner>,
}

struct AccessGateInner {
    config_path: PathBuf,
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
}

impl AccessSnapshot {
    pub fn is_authenticated(&self) -> bool {
        self.authenticated
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessField {
    Username,
    Password,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessStatus {
    Ready,
    MissingCredentials,
    Checking,
    Rejected,
    BackendError,
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
        }
    }
}

impl AccessGate {
    pub fn new(config_path: PathBuf) -> Self {
        let state = AccessState::default();
        let (sender, _) = watch::channel(state.snapshot());
        Self {
            inner: Arc::new(AccessGateInner {
                config_path,
                state: Mutex::new(state),
                sender,
            }),
        }
    }

    pub fn subscribe(&self) -> watch::Receiver<AccessSnapshot> {
        self.inner.sender.subscribe()
    }

    pub fn is_authenticated(&self) -> bool {
        self.lock_state().authenticated
    }

    pub fn reset(&self) {
        let mut state = self.lock_state();
        let generation = state.generation.wrapping_add(1);
        *state = AccessState {
            generation,
            ..AccessState::default()
        };
        self.publish(&state);
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
                state.status = AccessStatus::Granted;
                state.authenticated = true;
                tracing::info!(user = %state.username, "SunRDP access granted");
            }
            Ok(false) => {
                state.status = AccessStatus::Rejected;
                state.authenticated = false;
                state.focus = AccessField::Password;
                tracing::warn!(user = %state.username, "SunRDP access rejected");
            }
            Err(error) => {
                state.status = AccessStatus::BackendError;
                state.authenticated = false;
                state.focus = AccessField::Password;
                tracing::error!(?error, "SunRDP local account validation failed");
            }
        }
        self.publish(&state);
    }

    pub fn handle_keyboard(&self, event: &KeyboardEvent) {
        let submission = {
            let mut state = self.lock_state();
            if state.authenticated || state.status == AccessStatus::Checking {
                return;
            }

            let mut changed = false;
            let mut submit = false;
            match event {
                KeyboardEvent::Pressed { code: 42 | 54, .. } => state.shift = true,
                KeyboardEvent::Released { code: 42 | 54, .. } => state.shift = false,
                KeyboardEvent::Pressed { code: 58, .. } => {
                    state.caps_lock = !state.caps_lock;
                }
                KeyboardEvent::Pressed { code: 14, .. } => {
                    active_field_mut(&mut state).pop();
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
                    if let Some(character) =
                        scan_code_character(*code, state.shift, state.caps_lock)
                    {
                        changed = push_character(&mut state, character);
                    }
                }
                KeyboardEvent::Pressed { .. } => {}
                KeyboardEvent::UnicodePressed(code) => {
                    if let Some(character) = char::from_u32(u32::from(*code))
                        && !character.is_control()
                    {
                        changed = push_character(&mut state, character);
                    }
                }
                KeyboardEvent::Released { .. }
                | KeyboardEvent::UnicodeReleased(_)
                | KeyboardEvent::Synchronize(_) => {}
            }

            if changed {
                state.status = AccessStatus::Ready;
                self.publish(&state);
            }

            if submit {
                if state.username.trim().is_empty() || state.password.is_empty() {
                    state.status = AccessStatus::MissingCredentials;
                    self.publish(&state);
                    None
                } else {
                    state.status = AccessStatus::Checking;
                    let username = state.username.trim().to_string();
                    let password = Zeroizing::new(std::mem::take(&mut state.password));
                    let generation = state.generation;
                    self.publish(&state);
                    Some((generation, username, password))
                }
            } else {
                None
            }
        };

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

    pub fn render_frame(&self, size: DesktopSize) -> CapturedFrame {
        let snapshot = self.inner.sender.borrow().clone();
        render_access_frame(size, &snapshot)
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

fn render_access_frame(size: DesktopSize, snapshot: &AccessSnapshot) -> CapturedFrame {
    let width = usize::from(size.width);
    let height = usize::from(size.height);
    let mut rgba = vec![0; width * height * 4];
    fill(&mut rgba, size, [8, 15, 28, 255]);

    let scale = if width >= 900 { 3 } else { 2 };
    let card_width = width.saturating_sub(40).min(760);
    let card_height = (360 * scale / 2).min(height.saturating_sub(40));
    let card_x = (width.saturating_sub(card_width)) / 2;
    let card_y = (height.saturating_sub(card_height)) / 2;
    draw_rect(
        &mut rgba,
        size,
        card_x,
        card_y,
        card_width,
        card_height,
        [20, 31, 50, 255],
    );
    draw_rect(
        &mut rgba,
        size,
        card_x,
        card_y,
        card_width,
        6,
        [255, 177, 33, 255],
    );

    let content_x = card_x + 36;
    let mut y = card_y + 34;
    draw_text(
        &mut rgba,
        size,
        content_x,
        y,
        "SUN REMOTE DESKTOP",
        scale,
        [245, 248, 255, 255],
    );
    y += 14 * scale;
    draw_text(
        &mut rgba,
        size,
        content_x,
        y,
        "SUNRDP LOCAL ACCOUNT ACCESS",
        scale.saturating_sub(1).max(1),
        [151, 166, 190, 255],
    );

    y += 22 * scale;
    draw_field(
        &mut rgba,
        size,
        content_x,
        y,
        card_width.saturating_sub(72),
        "USERNAME",
        &snapshot.username,
        snapshot.focus == AccessField::Username,
        scale,
    );
    y += 48 * scale;
    let masked = "*".repeat(snapshot.password_length.min(48));
    draw_field(
        &mut rgba,
        size,
        content_x,
        y,
        card_width.saturating_sub(72),
        "PASSWORD",
        &masked,
        snapshot.focus == AccessField::Password,
        scale,
    );

    y += 52 * scale;
    let (status, color) = match snapshot.status {
        AccessStatus::Ready => ("TAB: SWITCH FIELD    ENTER: CONNECT", [151, 166, 190, 255]),
        AccessStatus::MissingCredentials => {
            ("ENTER BOTH USERNAME AND PASSWORD", [255, 193, 92, 255])
        }
        AccessStatus::Checking => ("CHECKING LOCAL WINDOWS ACCOUNT...", [92, 194, 255, 255]),
        AccessStatus::Rejected => (
            "ACCESS DENIED - CHECK ACCOUNT OR PERMISSION",
            [255, 105, 105, 255],
        ),
        AccessStatus::BackendError => (
            "AUTHENTICATION SERVICE ERROR - TRY AGAIN",
            [255, 105, 105, 255],
        ),
        AccessStatus::Granted => ("ACCESS GRANTED", [104, 222, 143, 255]),
    };
    draw_text(
        &mut rgba,
        size,
        content_x,
        y,
        status,
        scale.saturating_sub(1).max(1),
        color,
    );

    CapturedFrame {
        width: size.width,
        height: size.height,
        rgba,
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_field(
    rgba: &mut [u8],
    size: DesktopSize,
    x: usize,
    y: usize,
    width: usize,
    label: &str,
    value: &str,
    focused: bool,
    scale: usize,
) {
    let border = if focused {
        [255, 177, 33, 255]
    } else {
        [70, 86, 112, 255]
    };
    draw_rect(rgba, size, x, y, width, 40 * scale, border);
    draw_rect(
        rgba,
        size,
        x + 2,
        y + 2,
        width.saturating_sub(4),
        40 * scale - 4,
        [12, 21, 36, 255],
    );
    draw_text(
        rgba,
        size,
        x + 12,
        y + 6,
        label,
        scale.saturating_sub(1).max(1),
        [151, 166, 190, 255],
    );
    draw_text(
        rgba,
        size,
        x + 12,
        y + 17 * scale,
        value,
        scale,
        [245, 248, 255, 255],
    );
}

fn fill(rgba: &mut [u8], size: DesktopSize, color: [u8; 4]) {
    draw_rect(
        rgba,
        size,
        0,
        0,
        usize::from(size.width),
        usize::from(size.height),
        color,
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_rect(
    rgba: &mut [u8],
    size: DesktopSize,
    x: usize,
    y: usize,
    width: usize,
    height: usize,
    color: [u8; 4],
) {
    let stride = usize::from(size.width) * 4;
    let max_x = (x + width).min(usize::from(size.width));
    let max_y = (y + height).min(usize::from(size.height));
    for pixel_y in y.min(max_y)..max_y {
        for pixel_x in x.min(max_x)..max_x {
            let offset = pixel_y * stride + pixel_x * 4;
            rgba[offset..offset + 4].copy_from_slice(&color);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    rgba: &mut [u8],
    size: DesktopSize,
    x: usize,
    y: usize,
    text: &str,
    scale: usize,
    color: [u8; 4],
) {
    let mut cursor_x = x;
    for character in text.chars() {
        if let Some(glyph) = BASIC_FONTS.get(character) {
            for (glyph_y, row) in glyph.iter().enumerate() {
                for glyph_x in 0..8 {
                    if row & (1 << glyph_x) != 0 {
                        draw_rect(
                            rgba,
                            size,
                            cursor_x + glyph_x * scale,
                            y + glyph_y * scale,
                            scale,
                            scale,
                            color,
                        );
                    }
                }
            }
        }
        cursor_x += 9 * scale;
        if cursor_x >= usize::from(size.width) {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_code_mapping_handles_letters_and_symbols() {
        assert_eq!(scan_code_character(30, false, false), Some('a'));
        assert_eq!(scan_code_character(30, true, false), Some('A'));
        assert_eq!(scan_code_character(30, false, true), Some('A'));
        assert_eq!(scan_code_character(2, true, false), Some('!'));
        assert_eq!(scan_code_character(43, false, false), Some('\\'));
    }

    #[test]
    fn access_frame_matches_the_desktop_size() {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        let size = DesktopSize {
            width: 640,
            height: 480,
        };
        let frame = gate.render_frame(size);
        assert_eq!(frame.width, size.width);
        assert_eq!(frame.height, size.height);
        assert_eq!(frame.rgba.len(), 640 * 480 * 4);
    }
}
