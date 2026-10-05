//! The icons made from packaging/icon.png: on Windows the exe's own, which
//! Explorer, the Start menu, the taskbar and Settings show, and on Windows
//! and Linux the one the window carries. A Mac's app bundle has its own.

use std::env;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::PathBuf;

const ICON: &str = "packaging/icon.png";

/// The window's icon is this many pixels across: twice the 32 Windows shows
/// it at, for screens scaled up.
const WINDOW: usize = 64;

/// The sizes an exe's icon comes in, for Windows to pick from.
const SIZES: [usize; 8] = [16, 20, 24, 32, 40, 48, 64, 256];

fn main() {
    println!("cargo:rerun-if-changed={ICON}");
    let target = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target == "macos" {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let (rgba, side) = decode();
    fs::write(out.join("window-icon.rgba"), shrink(&rgba, side, WINDOW))
        .expect("the window's icon can be written to OUT_DIR");
    if target == "windows" {
        let ico = out.join("soundcheck.ico");
        fs::write(&ico, ico_file(&rgba, side)).expect("the exe's icon can be written to OUT_DIR");
        winresource::WindowsResource::new()
            .set_icon(ico.to_str().expect("OUT_DIR is a path in text"))
            .compile()
            .expect("the exe's icon and details can be built into it");
    }
}

/// The icon's pixels, four bytes each, and how many across, as it is square.
fn decode() -> (Vec<u8>, usize) {
    let file = File::open(ICON).expect("packaging/icon.png is there");
    let mut reader = png::Decoder::new(BufReader::new(file))
        .read_info()
        .expect("packaging/icon.png is a PNG");
    let mut rgba = vec![0; reader.output_buffer_size().expect("it fits in memory")];
    let info = reader
        .next_frame(&mut rgba)
        .expect("packaging/icon.png reads whole");
    assert!(
        info.color_type == png::ColorType::Rgba
            && info.bit_depth == png::BitDepth::Eight
            && info.width == info.height,
        "packaging/icon.png is a square of 8-bit RGBA"
    );
    rgba.truncate(info.buffer_size());
    (rgba, info.width as usize)
}

/// `rgba`, `from` pixels across, shrunk to `to` across: each new pixel the
/// average of those it covers, weighted by how opaque they are, so that the
/// edges fade out rather than darken.
fn shrink(rgba: &[u8], from: usize, to: usize) -> Vec<u8> {
    let span = |i: usize| (i * from / to, ((i + 1) * from / to).max(i * from / to + 1));
    let mut out = Vec::with_capacity(to * to * 4);
    for y in 0..to {
        let (y0, y1) = span(y);
        for x in 0..to {
            let (x0, x1) = span(x);
            let (mut color, mut alpha) = ([0u64; 3], 0u64);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let pixel = &rgba[(sy * from + sx) * 4..][..4];
                    let a = u64::from(pixel[3]);
                    for (sum, &c) in color.iter_mut().zip(&pixel[..3]) {
                        *sum += u64::from(c) * a;
                    }
                    alpha += a;
                }
            }
            let covered = ((y1 - y0) * (x1 - x0)) as u64;
            for sum in color {
                out.push((sum + alpha / 2).checked_div(alpha).unwrap_or(0) as u8);
            }
            out.push(((alpha + covered / 2) / covered) as u8);
        }
    }
    out
}

/// `rgba`, `side` pixels across, as a PNG.
fn png_of(rgba: &[u8], side: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = png::Encoder::new(&mut out, side as u32, side as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .expect("a PNG header writes to memory");
    writer
        .write_image_data(rgba)
        .expect("a PNG writes to memory");
    writer.finish().expect("a PNG ends in memory");
    out
}

/// An ICO of the icon at every size in SIZES, each kept as a PNG, which
/// Windows reads from Vista on.
fn ico_file(rgba: &[u8], side: usize) -> Vec<u8> {
    let images: Vec<(usize, Vec<u8>)> = SIZES
        .iter()
        .map(|&size| (size, png_of(&shrink(rgba, side, size), size)))
        .collect();
    // Two bytes nothing, two saying it holds icons, two how many.
    let mut ico = vec![0, 0, 1, 0];
    ico.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut at = 6 + 16 * images.len();
    for (size, png) in &images {
        // 0 stands for 256 across.
        let across = u8::try_from(*size).unwrap_or(0);
        ico.extend_from_slice(&[across, across, 0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(png.len() as u32).to_le_bytes());
        ico.extend_from_slice(&(at as u32).to_le_bytes());
        at += png.len();
    }
    for (_, png) in &images {
        ico.extend_from_slice(png);
    }
    ico
}
