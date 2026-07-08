#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod theme;
mod widgets;

use app::AppState;

fn main() -> eframe::Result {
    env_logger::init();

    let mut state = AppState::default();
    state.refresh_devices();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([900.0, 700.0])
            .with_min_inner_size([400.0, 400.0])
            .with_title("LTC Timecode Generator"),
        ..Default::default()
    };

    eframe::run_native(
        "LTC Timecode Generator",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(state))
        }),
    )
}
