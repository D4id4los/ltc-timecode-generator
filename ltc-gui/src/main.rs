#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod theme;
mod widgets;

fn main() {
    let cli = gui_engine::cli::parse_args();

    match gui_engine::cli::process_cli(cli) {
        gui_engine::cli::CliOutcome::Done => {}
        gui_engine::cli::CliOutcome::RunGui { cmd_tx, state } => {
            let log_buffer = gui_engine::log_buffer::init_logger(
                "ltc_gui=trace,audio_core=trace,info",
            )
            .expect("Failed to initialize logger");

            let app = app::AppState::new(cmd_tx, state, log_buffer);

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
                Box::new(move |_cc| Ok(Box::new(app))),
            )
            .expect("eframe error");
        }
    }
}