// Release builds on Windows would otherwise open a console window next to
// the app; debug builds keep it for their log output.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod convert;
mod convert_ui;
mod cpu;
mod edit;
mod explorer;
mod export;
mod finder;
mod flac;
mod levels;
#[cfg(target_os = "macos")]
mod memory;
mod meta;
mod playback;
mod probe;
mod rename;
mod rename_ui;
mod save;
mod search;
mod spectrogram;
mod tags;
mod views;
mod wav;

use eframe::egui;

#[cfg(target_os = "macos")]
#[global_allocator]
static MEMORY: memory::Allocator = memory::Allocator;

fn main() -> eframe::Result {
    let initial = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let inbox = finder::Inbox::new();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("soundcheck")
            .with_inner_size([1440.0, 880.0])
            .with_min_inner_size([900.0, 520.0])
            // Empty, so eframe keeps its own logo out of the Dock: putting it
            // there holds up the first frame by about 16 ms.
            .with_icon(egui::IconData::default()),
        ..Default::default()
    };
    eframe::run_native(
        "soundcheck",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc, initial, inbox)))),
    )
}
