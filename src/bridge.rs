use anyhow::{Context, Result};
use ironrdp_server::{KeyboardEvent, MouseButton, MouseEvent};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::platform::{CapturedFrame, DesktopSize};

const MAGIC: [u8; 4] = *b"RDPH";
const PROTOCOL_VERSION: u16 = 2;
const MINIMUM_PROTOCOL_VERSION: u16 = 1;
const HELLO_LENGTH: u16 = 16;
const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

const FRAME: u8 = 0x01;
const KEY_PRESSED: u8 = 0x10;
const KEY_RELEASED: u8 = 0x11;
const KEY_UNICODE_PRESSED: u8 = 0x12;
const KEY_UNICODE_RELEASED: u8 = 0x13;
const MOUSE_MOVE: u8 = 0x20;
const MOUSE_REL_MOVE: u8 = 0x21;
const MOUSE_LEFT_PRESSED: u8 = 0x22;
const MOUSE_LEFT_RELEASED: u8 = 0x23;
const MOUSE_RIGHT_PRESSED: u8 = 0x24;
const MOUSE_RIGHT_RELEASED: u8 = 0x25;
const MOUSE_MIDDLE_PRESSED: u8 = 0x26;
const MOUSE_MIDDLE_RELEASED: u8 = 0x27;
const MOUSE_BUTTON4_PRESSED: u8 = 0x28;
const MOUSE_BUTTON4_RELEASED: u8 = 0x29;
const MOUSE_BUTTON5_PRESSED: u8 = 0x2a;
const MOUSE_BUTTON5_RELEASED: u8 = 0x2b;
const MOUSE_VERTICAL_SCROLL: u8 = 0x2c;
const MOUSE_SCROLL: u8 = 0x2d;
const SET_DISPLAY_SIZE: u8 = 0x30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Handshake {
    version: u16,
    size: DesktopSize,
}

enum InputCommand {
    Keyboard(KeyboardEvent),
    Mouse(MouseEvent),
    SetDisplaySize(DesktopSize),
}

async fn write_handshake<W>(writer: &mut W, size: DesktopSize) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    validate_size(size)?;
    let mut data = Vec::with_capacity(usize::from(HELLO_LENGTH));
    data.extend_from_slice(&MAGIC);
    data.extend_from_slice(&PROTOCOL_VERSION.to_le_bytes());
    data.extend_from_slice(&HELLO_LENGTH.to_le_bytes());
    data.extend_from_slice(&size.width.to_le_bytes());
    data.extend_from_slice(&size.height.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    writer
        .write_all(&data)
        .await
        .context("write agent handshake")
}

async fn read_handshake<R>(reader: &mut R) -> Result<Handshake>
where
    R: AsyncRead + Unpin,
{
    let mut data = [0u8; HELLO_LENGTH as usize];
    reader
        .read_exact(&mut data)
        .await
        .context("read agent handshake")?;
    anyhow::ensure!(data[0..4] == MAGIC, "invalid session bridge magic");
    let version = u16::from_le_bytes([data[4], data[5]]);
    anyhow::ensure!(
        (MINIMUM_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&version),
        "unsupported session bridge protocol version {version}"
    );
    let header_length = u16::from_le_bytes([data[6], data[7]]);
    anyhow::ensure!(
        header_length == HELLO_LENGTH,
        "unsupported session bridge handshake length {header_length}"
    );
    let size = DesktopSize {
        width: u16::from_le_bytes([data[8], data[9]]),
        height: u16::from_le_bytes([data[10], data[11]]),
    };
    validate_size(size)?;
    Ok(Handshake { version, size })
}

async fn write_frame<W>(writer: &mut W, frame: &CapturedFrame) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let size = DesktopSize {
        width: frame.width,
        height: frame.height,
    };
    let expected = frame_bytes(size)?;
    anyhow::ensure!(frame.rgba.len() == expected, "invalid RGBA frame size");
    let payload_length = u32::try_from(frame.rgba.len()).context("frame payload is too large")?;

    writer.write_u8(FRAME).await?;
    writer.write_u16_le(frame.width).await?;
    writer.write_u16_le(frame.height).await?;
    writer.write_u32_le(payload_length).await?;
    writer
        .write_all(&frame.rgba)
        .await
        .context("write desktop frame")
}

async fn read_frame<R>(reader: &mut R) -> Result<CapturedFrame>
where
    R: AsyncRead + Unpin,
{
    let message_type = reader.read_u8().await.context("read frame type")?;
    anyhow::ensure!(
        message_type == FRAME,
        "unexpected agent message {message_type:#x}"
    );
    let size = DesktopSize {
        width: reader.read_u16_le().await?,
        height: reader.read_u16_le().await?,
    };
    let payload_length = reader.read_u32_le().await? as usize;
    let expected = frame_bytes(size)?;
    anyhow::ensure!(
        payload_length == expected,
        "invalid frame payload length {payload_length}; expected {expected}"
    );
    let mut rgba = vec![0u8; payload_length];
    reader
        .read_exact(&mut rgba)
        .await
        .context("read desktop frame")?;
    Ok(CapturedFrame {
        width: size.width,
        height: size.height,
        rgba,
    })
}

fn encode_keyboard(event: &KeyboardEvent) -> Option<Vec<u8>> {
    match event {
        KeyboardEvent::Pressed { code, extended } => {
            Some(vec![KEY_PRESSED, *code, u8::from(*extended)])
        }
        KeyboardEvent::Released { code, extended } => {
            Some(vec![KEY_RELEASED, *code, u8::from(*extended)])
        }
        KeyboardEvent::UnicodePressed(code) => {
            let mut data = vec![KEY_UNICODE_PRESSED];
            data.extend_from_slice(&code.to_le_bytes());
            Some(data)
        }
        KeyboardEvent::UnicodeReleased(code) => {
            let mut data = vec![KEY_UNICODE_RELEASED];
            data.extend_from_slice(&code.to_le_bytes());
            Some(data)
        }
        KeyboardEvent::Synchronize(_) => None,
    }
}

fn encode_mouse(event: &MouseEvent) -> Vec<u8> {
    let mut data = Vec::new();
    match event {
        MouseEvent::Move { x, y } => {
            data.push(MOUSE_MOVE);
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&y.to_le_bytes());
        }
        MouseEvent::RelMove { x, y } => {
            data.push(MOUSE_REL_MOVE);
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&y.to_le_bytes());
        }
        MouseEvent::Button {
            x,
            y,
            button,
            pressed,
        } => {
            // Protocol v1/v2 button messages have no coordinates. Prefix an
            // absolute move so both old and new console agents click exactly
            // where the client reported the button event.
            data.push(MOUSE_MOVE);
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&y.to_le_bytes());
            if let Some(message_type) = bridge_button_type(*button, *pressed) {
                data.push(message_type);
            }
        }
        MouseEvent::ButtonRel {
            x,
            y,
            button,
            pressed,
        } => {
            data.push(MOUSE_REL_MOVE);
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&y.to_le_bytes());
            if let Some(message_type) = bridge_button_type(*button, *pressed) {
                data.push(message_type);
            }
        }
        MouseEvent::VerticalScroll { value } => {
            data.push(MOUSE_VERTICAL_SCROLL);
            data.extend_from_slice(&value.to_le_bytes());
        }
        MouseEvent::HorizontalScroll { value } => {
            data.push(MOUSE_SCROLL);
            data.extend_from_slice(&i32::from(*value).to_le_bytes());
            data.extend_from_slice(&0i32.to_le_bytes());
        }
        MouseEvent::Scroll { x, y } => {
            data.push(MOUSE_SCROLL);
            data.extend_from_slice(&x.to_le_bytes());
            data.extend_from_slice(&y.to_le_bytes());
        }
        _ => {}
    }
    data
}

fn bridge_button_type(button: MouseButton, pressed: bool) -> Option<u8> {
    Some(match (button, pressed) {
        (MouseButton::Left, true) => MOUSE_LEFT_PRESSED,
        (MouseButton::Left, false) => MOUSE_LEFT_RELEASED,
        (MouseButton::Right, true) => MOUSE_RIGHT_PRESSED,
        (MouseButton::Right, false) => MOUSE_RIGHT_RELEASED,
        (MouseButton::Middle, true) => MOUSE_MIDDLE_PRESSED,
        (MouseButton::Middle, false) => MOUSE_MIDDLE_RELEASED,
        (MouseButton::X1, true) => MOUSE_BUTTON4_PRESSED,
        (MouseButton::X1, false) => MOUSE_BUTTON4_RELEASED,
        (MouseButton::X2, true) => MOUSE_BUTTON5_PRESSED,
        (MouseButton::X2, false) => MOUSE_BUTTON5_RELEASED,
        _ => return None,
    })
}

fn encode_display_size(size: DesktopSize) -> Vec<u8> {
    let mut data = Vec::with_capacity(5);
    data.push(SET_DISPLAY_SIZE);
    data.extend_from_slice(&size.width.to_le_bytes());
    data.extend_from_slice(&size.height.to_le_bytes());
    data
}

async fn read_input<R>(reader: &mut R) -> Result<InputCommand>
where
    R: AsyncRead + Unpin,
{
    let message_type = reader.read_u8().await.context("read input event type")?;
    let event = match message_type {
        KEY_PRESSED | KEY_RELEASED => {
            let code = reader.read_u8().await?;
            let extended = reader.read_u8().await? != 0;
            let event = if message_type == KEY_PRESSED {
                KeyboardEvent::Pressed { code, extended }
            } else {
                KeyboardEvent::Released { code, extended }
            };
            InputCommand::Keyboard(event)
        }
        KEY_UNICODE_PRESSED => {
            InputCommand::Keyboard(KeyboardEvent::UnicodePressed(reader.read_u16_le().await?))
        }
        KEY_UNICODE_RELEASED => {
            InputCommand::Keyboard(KeyboardEvent::UnicodeReleased(reader.read_u16_le().await?))
        }
        MOUSE_MOVE => InputCommand::Mouse(MouseEvent::Move {
            x: reader.read_u16_le().await?,
            y: reader.read_u16_le().await?,
        }),
        MOUSE_REL_MOVE => InputCommand::Mouse(MouseEvent::RelMove {
            x: reader.read_i32_le().await?,
            y: reader.read_i32_le().await?,
        }),
        MOUSE_LEFT_PRESSED => InputCommand::Mouse(bridge_button(MouseButton::Left, true)),
        MOUSE_LEFT_RELEASED => InputCommand::Mouse(bridge_button(MouseButton::Left, false)),
        MOUSE_RIGHT_PRESSED => InputCommand::Mouse(bridge_button(MouseButton::Right, true)),
        MOUSE_RIGHT_RELEASED => InputCommand::Mouse(bridge_button(MouseButton::Right, false)),
        MOUSE_MIDDLE_PRESSED => InputCommand::Mouse(bridge_button(MouseButton::Middle, true)),
        MOUSE_MIDDLE_RELEASED => InputCommand::Mouse(bridge_button(MouseButton::Middle, false)),
        MOUSE_BUTTON4_PRESSED => InputCommand::Mouse(bridge_button(MouseButton::X1, true)),
        MOUSE_BUTTON4_RELEASED => InputCommand::Mouse(bridge_button(MouseButton::X1, false)),
        MOUSE_BUTTON5_PRESSED => InputCommand::Mouse(bridge_button(MouseButton::X2, true)),
        MOUSE_BUTTON5_RELEASED => InputCommand::Mouse(bridge_button(MouseButton::X2, false)),
        MOUSE_VERTICAL_SCROLL => InputCommand::Mouse(MouseEvent::VerticalScroll {
            value: reader.read_i16_le().await?,
        }),
        MOUSE_SCROLL => InputCommand::Mouse(MouseEvent::Scroll {
            x: reader.read_i32_le().await?,
            y: reader.read_i32_le().await?,
        }),
        SET_DISPLAY_SIZE => {
            let size = DesktopSize {
                width: reader.read_u16_le().await?,
                height: reader.read_u16_le().await?,
            };
            validate_size(size)?;
            InputCommand::SetDisplaySize(size)
        }
        _ => anyhow::bail!("unknown input event type {message_type:#x}"),
    };
    Ok(event)
}

fn bridge_button(button: MouseButton, pressed: bool) -> MouseEvent {
    // The preceding bridge message moves the pointer. A zero relative delta
    // applies the button without teleporting it back to (0, 0).
    MouseEvent::ButtonRel {
        x: 0,
        y: 0,
        button,
        pressed,
    }
}

fn validate_size(size: DesktopSize) -> Result<()> {
    anyhow::ensure!(size.width > 0 && size.height > 0, "desktop size is zero");
    let _ = frame_bytes(size)?;
    Ok(())
}

fn frame_bytes(size: DesktopSize) -> Result<usize> {
    let length = usize::from(size.width)
        .checked_mul(usize::from(size.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .context("desktop frame size overflow")?;
    anyhow::ensure!(
        length <= MAX_FRAME_BYTES,
        "desktop frame exceeds the {MAX_FRAME_BYTES}-byte bridge limit"
    );
    Ok(length)
}

#[cfg(windows)]
pub mod windows;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn handshake_round_trip() {
        let size = DesktopSize {
            width: 1920,
            height: 1080,
        };
        let (mut writer, mut reader) = tokio::io::duplex(64);
        write_handshake(&mut writer, size).await.unwrap();
        let handshake = read_handshake(&mut reader).await.unwrap();
        assert_eq!(handshake.version, PROTOCOL_VERSION);
        assert_eq!(handshake.size, size);
    }

    #[tokio::test]
    async fn accepts_the_previous_bridge_version_for_in_place_upgrades() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC);
        data.extend_from_slice(&MINIMUM_PROTOCOL_VERSION.to_le_bytes());
        data.extend_from_slice(&HELLO_LENGTH.to_le_bytes());
        data.extend_from_slice(&640u16.to_le_bytes());
        data.extend_from_slice(&480u16.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        writer.write_all(&data).await.unwrap();
        let handshake = read_handshake(&mut reader).await.unwrap();
        assert_eq!(handshake.version, MINIMUM_PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn rejects_an_unknown_protocol_version() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let mut data = Vec::new();
        data.extend_from_slice(&MAGIC);
        data.extend_from_slice(&(PROTOCOL_VERSION + 1).to_le_bytes());
        data.extend_from_slice(&HELLO_LENGTH.to_le_bytes());
        data.extend_from_slice(&640u16.to_le_bytes());
        data.extend_from_slice(&480u16.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        writer.write_all(&data).await.unwrap();
        let error = read_handshake(&mut reader).await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unsupported session bridge protocol")
        );
    }

    #[tokio::test]
    async fn frame_round_trip() {
        let frame = CapturedFrame {
            width: 4,
            height: 2,
            rgba: (0..32).collect(),
        };
        let (mut writer, mut reader) = tokio::io::duplex(128);
        write_frame(&mut writer, &frame).await.unwrap();
        let decoded = read_frame(&mut reader).await.unwrap();
        assert_eq!(decoded.width, frame.width);
        assert_eq!(decoded.height, frame.height);
        assert_eq!(decoded.rgba, frame.rgba);
    }

    #[tokio::test]
    async fn input_round_trip() {
        let keyboard = KeyboardEvent::Pressed {
            code: 42,
            extended: true,
        };
        let mouse = MouseEvent::Scroll { x: -2, y: 3 };
        let click = MouseEvent::Button {
            x: 321,
            y: 654,
            button: MouseButton::Left,
            pressed: true,
        };
        let size = DesktopSize {
            width: 1600,
            height: 900,
        };
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer
            .write_all(&encode_keyboard(&keyboard).unwrap())
            .await
            .unwrap();
        writer.write_all(&encode_mouse(&mouse)).await.unwrap();
        writer.write_all(&encode_mouse(&click)).await.unwrap();
        writer.write_all(&encode_display_size(size)).await.unwrap();

        match read_input(&mut reader).await.unwrap() {
            InputCommand::Keyboard(KeyboardEvent::Pressed { code, extended }) => {
                assert_eq!(code, 42);
                assert!(extended);
            }
            _ => panic!("unexpected keyboard event"),
        }
        match read_input(&mut reader).await.unwrap() {
            InputCommand::Mouse(MouseEvent::Scroll { x, y }) => {
                assert_eq!((x, y), (-2, 3));
            }
            _ => panic!("unexpected mouse event"),
        }
        assert!(matches!(
            read_input(&mut reader).await.unwrap(),
            InputCommand::Mouse(MouseEvent::Move { x: 321, y: 654 })
        ));
        assert!(matches!(
            read_input(&mut reader).await.unwrap(),
            InputCommand::Mouse(MouseEvent::ButtonRel {
                x: 0,
                y: 0,
                button: MouseButton::Left,
                pressed: true,
            })
        ));
        match read_input(&mut reader).await.unwrap() {
            InputCommand::SetDisplaySize(actual) => assert_eq!(actual, size),
            _ => panic!("unexpected display-size event"),
        }
    }
}
