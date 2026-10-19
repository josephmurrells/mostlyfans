use std::path::{Path, PathBuf};

use embed_manifest::manifest::ExecutionLevel;
use embed_manifest::{embed_manifest, new_manifest};

#[allow(dead_code)]
mod glyph {
    include!("src/glyph.rs");
}

const ICON_SIZES: [u32; 5] = [16, 24, 32, 48, 64];

fn bitmap(size: u32) -> Vec<u8> {
    let rgba = glyph::fan_rgba(size, glyph::BLUE);
    let mask_row = size.div_ceil(32) * 4;
    let mut bytes = Vec::new();
    for value in [40, size, size * 2] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&32_u16.to_le_bytes());
    bytes.extend_from_slice(&[0; 24]);
    for y in (0..size).rev() {
        for x in 0..size {
            let index = ((y * size + x) * 4) as usize;
            let [r, g, b, a] = [
                rgba[index],
                rgba[index + 1],
                rgba[index + 2],
                rgba[index + 3],
            ];
            bytes.extend_from_slice(&[b, g, r, a]);
        }
    }
    bytes.extend(std::iter::repeat_n(0, (mask_row * size) as usize));
    bytes
}

fn icon_file(path: &Path) {
    let images: Vec<(u32, Vec<u8>)> = ICON_SIZES
        .iter()
        .map(|size| (*size, bitmap(*size)))
        .collect();
    let mut bytes = Vec::new();
    for value in [0_u16, 1, images.len() as u16] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let mut offset = 6 + 16 * images.len() as u32;
    for (size, data) in &images {
        let edge = *size as u8;
        bytes.extend_from_slice(&[edge, edge, 0, 0]);
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &images {
        bytes.extend_from_slice(data);
    }
    std::fs::write(path, bytes).expect("the icon could not be written");
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/glyph.rs");
    println!("cargo:rerun-if-env-changed=PROFILE");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let level = if std::env::var("PROFILE").as_deref() == Ok("release") {
        ExecutionLevel::RequireAdministrator
    } else {
        ExecutionLevel::AsInvoker
    };
    embed_manifest(new_manifest("FanForge").requested_execution_level(level))
        .expect("the application manifest could not be embedded");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let icon = out.join("fanforge.ico");
    icon_file(&icon);
    let script = out.join("fanforge.rc");
    let escaped = icon.display().to_string().replace('\\', "\\\\");
    std::fs::write(&script, format!("1 ICON \"{escaped}\"\n"))
        .expect("the resource script could not be written");
    embed_resource::compile(&script, embed_resource::NONE)
        .manifest_optional()
        .expect("the icon resource could not be compiled");
}
