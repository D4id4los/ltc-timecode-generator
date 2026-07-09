use egui::{Color32, FontId, RichText, Ui};

use crate::app::AppState;

/// Render the footer status bar matching the webapp styling.
pub fn render(ui: &mut Ui, state: &AppState) {
    let colors = state.theme.colors();

    let frame = egui::Frame::new()
        .fill(colors.deep_bg)
        .inner_margin(egui::Margin::symmetric(12, 6));

    frame.show(ui, |ui| {
        let width = ui.available_width();
        
        if width > 600.0 {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(16.0, 0.0);
                
                // Left side: Status indicators with small green/amber dots
                // 1. OS Info
                let os_name = std::env::consts::OS.to_uppercase();
                dot_label(ui, &format!("OS: {}", os_name), Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                
                // 2. Audio devices status
                let dev_count = state.devices.len();
                let dev_text = if dev_count > 0 {
                    format!("AUDIO OUT: {} DEVICES FOUND", dev_count)
                } else {
                    "AUDIO OUT: LINE / JACK".to_string()
                };
                dot_label(ui, &dev_text, Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                
                // 3. Audio Core status
                let core_status = if state.is_playing { "AUDIO CORE: RUNNING" } else { "AUDIO CORE: STANDBY" };
                let core_color = if state.is_playing { Color32::from_rgb(0x22, 0xC5, 0x5E) } else { Color32::from_rgb(0xF5, 0x9E, 0x0B) };
                dot_label(ui, core_status, core_color, &colors);
                
                // 4. Wake Lock status
                dot_label(ui, "WAKE LOCK: N/A", colors.text_muted, &colors);

                // Right side: Local system clock and power status
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("SYSTEM TIME: {} | POWER: AC / BATTERY CONNECTED", state.system_time))
                            .font(FontId::monospace(10.0))
                            .color(colors.text_muted)
                            .strong()
                    );
                });
            });
        } else {
            // Stacked responsive layout for narrow displays
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::Vec2::new(12.0, 0.0);
                    let os_name = std::env::consts::OS.to_uppercase();
                    dot_label(ui, &os_name, Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                    
                    let core_status = if state.is_playing { "RUNNING" } else { "STANDBY" };
                    let core_color = if state.is_playing { Color32::from_rgb(0x22, 0xC5, 0x5E) } else { Color32::from_rgb(0xF5, 0x9E, 0x0B) };
                    dot_label(ui, core_status, core_color, &colors);
                });
                
                ui.add_space(4.0);
                ui.label(
                    RichText::new(format!("TIME: {} | POWER: AC", state.system_time))
                        .font(FontId::monospace(9.5))
                        .color(colors.text_muted)
                        .strong()
                );
            });
        }
    });
}

fn dot_label(ui: &mut Ui, text: &str, dot_color: Color32, colors: &crate::theme::ThemeColors) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 0.0);
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(6.0, 6.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 2.5, dot_color);
        ui.label(
            RichText::new(text)
                .font(FontId::monospace(10.0))
                .color(colors.text_title)
                .strong()
        );
    });
}
