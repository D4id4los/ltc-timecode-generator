use egui::{FontId, RichText, Ui};

use crate::app::AppState;

/// Render the footer status bar: OS, devices, audio status, system time, battery.
pub fn render(ui: &mut Ui, state: &AppState) {
    let colors = state.theme.colors();

    let frame = egui::Frame::new()
        .fill(colors.deep_bg)
        .inner_margin(egui::Margin::symmetric(8, 4));

    frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!("OS: {}", std::env::consts::OS))
                    .font(FontId::proportional(11.0))
                    .color(colors.text_muted),
            );
            ui.separator();
            ui.label(
                RichText::new(format!("Audio Devices: {}", state.devices.len()))
                    .font(FontId::proportional(11.0))
                    .color(colors.text_muted),
            );
            ui.separator();
            ui.label(
                RichText::new(&state.status_message)
                    .font(FontId::proportional(11.0))
                    .color(colors.text_muted),
            );
            ui.separator();
            ui.label(
                RichText::new("[Space] Play/Stop  [C] Clap  [R] Reset  [L] Lock")
                    .font(FontId::proportional(10.0))
                    .color(colors.text_muted),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(
                    RichText::new(&state.system_time)
                        .font(FontId::proportional(11.0))
                        .color(colors.text_muted),
                );
                ui.separator();
                let dot_color = if state.is_playing {
                    crate::theme::ACCENT
                } else {
                    colors.text_muted
                };
                let (rect, _) = ui.allocate_exact_size(
                    egui::Vec2::new(8.0, 8.0),
                    egui::Sense::hover(),
                );
                ui.painter().circle_filled(rect.center(), 3.0, dot_color);
                ui.label(
                    RichText::new(if state.is_playing { "LIVE" } else { "IDLE" })
                        .font(FontId::proportional(11.0))
                        .color(colors.text_muted),
                );
            });
        });
    });
}
