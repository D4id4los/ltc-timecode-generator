#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod cli;
mod log_buffer;
mod theme;
mod widgets;

use app::AppState;
use clap::Parser;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = cli::Cli::parse();

    if cli.list_devices {
        cli::list_devices_and_exit();
    }

    if cli.output_to_file.is_some() {
        return cli::generate_wav(cli);
    }

    if cli.headless {
        return cli::run_headless(cli);
    }

    let log_buffer = log_buffer::init_logger("ltc_gui=trace,audio_core=trace,info")
        .expect("Failed to initialize logger");

    let mut state = AppState::new(log_buffer);
    state.refresh_devices();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_maximized(true)
            .with_inner_size([500.0, 400.0])
            .with_min_inner_size([300.0, 300.0])
            .with_title("LTC Timecode Generator"),
        glow_options: eframe::egui_glow::GlowConfiguration {
            vsync: false,
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        "LTC Timecode Generator",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(state))
        }),
    )
    .map_err(|e| e.into())
}
