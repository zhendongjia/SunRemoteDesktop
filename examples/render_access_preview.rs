use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ironrdp_server::KeyboardEvent;
use sun_remote_desktop::access::AccessGate;
use sun_remote_desktop::platform::{CapturedFrame, DesktopSize};

fn main() -> Result<()> {
    let output = PathBuf::from("target/access-previews");
    std::fs::create_dir_all(&output)?;
    for size in [
        DesktopSize {
            width: 1600,
            height: 900,
        },
        DesktopSize {
            width: 1136,
            height: 544,
        },
        DesktopSize {
            width: 640,
            height: 480,
        },
    ] {
        let gate = AccessGate::new(PathBuf::from("unused.toml"));
        gate.set_display_sizes(
            size,
            DesktopSize {
                width: 2560,
                height: 1600,
            },
        );
        for character in "jzd".encode_utf16() {
            gate.handle_keyboard(&KeyboardEvent::UnicodePressed(character));
        }
        gate.handle_keyboard(&KeyboardEvent::Pressed {
            code: 15,
            extended: false,
        });
        for character in "example".encode_utf16() {
            gate.handle_keyboard(&KeyboardEvent::UnicodePressed(character));
        }
        write_bmp(
            &output.join(format!("login-{}x{}.bmp", size.width, size.height)),
            &gate.render_frame(size),
        )?;

        let generation = gate.begin_validation("jzd");
        gate.finish_validation(generation, "jzd", Ok(true));
        write_bmp(
            &output.join(format!("resolution-{}x{}.bmp", size.width, size.height)),
            &gate.render_frame(size),
        )?;
    }
    println!("rendered access previews to {}", output.display());
    Ok(())
}

fn write_bmp(path: &Path, frame: &CapturedFrame) -> Result<()> {
    let pixel_bytes: u32 = frame
        .rgba
        .len()
        .try_into()
        .context("preview bitmap is too large")?;
    let file_size = 14u32 + 40 + pixel_bytes;
    let mut bmp = Vec::with_capacity(file_size as usize);
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&file_size.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&54u32.to_le_bytes());
    bmp.extend_from_slice(&40u32.to_le_bytes());
    bmp.extend_from_slice(&i32::from(frame.width).to_le_bytes());
    bmp.extend_from_slice(&(-i32::from(frame.height)).to_le_bytes());
    bmp.extend_from_slice(&1u16.to_le_bytes());
    bmp.extend_from_slice(&32u16.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&pixel_bytes.to_le_bytes());
    bmp.extend_from_slice(&2_835u32.to_le_bytes());
    bmp.extend_from_slice(&2_835u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    bmp.extend_from_slice(&0u32.to_le_bytes());
    for pixel in frame.rgba.as_chunks::<4>().0 {
        bmp.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    std::fs::write(path, bmp).with_context(|| format!("write {}", path.display()))
}
