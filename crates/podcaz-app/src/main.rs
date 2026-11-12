// En release no se abre consola junto a la ventana.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio_io;
mod midi_in;

use eframe::egui;

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("PODCAZ")
            .with_inner_size([1320.0, 860.0])
            .with_min_inner_size([1000.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "PODCAZ",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
