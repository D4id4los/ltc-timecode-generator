use egui::{Color32, FontId, RichText, Ui};

use crate::app::AppState;

pub fn render(ui: &mut Ui, state: &AppState) {
    let colors = state.theme.colors();
    let s = &state.latest;
    let frame = egui::Frame::new()
        .fill(colors.deep_bg)
        .inner_margin(egui::Margin::symmetric(12, 6));
    frame.show(ui, |ui| {
        let width = ui.available_width();
        if width > 600.0 {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(16.0, 0.0);
                let os = std::env::consts::OS.to_uppercase();
                dot_label(ui, &format!("OS: {}", os), Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                let dev_count = s.devices.len();
                let dev_text = if dev_count > 0 { format!("AUDIO OUT: {} DEVICES FOUND", dev_count) } else { "AUDIO OUT: LINE / JACK".to_string() };
                dot_label(ui, &dev_text, Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                let core_status = if s.is_playing { "AUDIO CORE: RUNNING" } else { "AUDIO CORE: STANDBY" };
                let core_color = if s.is_playing { Color32::from_rgb(0x22, 0xC5, 0x5E) } else { Color32::from_rgb(0xF5, 0x9E, 0x0B) };
                dot_label(ui, core_status, core_color, &colors);
                if !s.sample_format_name.is_empty() {
                    dot_label(ui, &format!("SAMPLE: {}", s.sample_format_name.to_uppercase()), Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                }
                if s.wake_lock_active {
                    dot_label(ui, "WAKE LOCK: ACTIVE", Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                } else {
                    dot_label(ui, "WAKE LOCK: STANDBY", Color32::from_rgb(0x8E, 0x92, 0x99), &colors);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(RichText::new("POWER: AC").font(FontId::monospace(10.0)).color(colors.text_muted).strong());
                });
            });
        } else {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::Vec2::new(12.0, 0.0);
                    let os = std::env::consts::OS.to_uppercase();
                    dot_label(ui, &os, Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                    let core_status = if s.is_playing { "RUNNING" } else { "STANDBY" };
                    let core_color = if s.is_playing { Color32::from_rgb(0x22, 0xC5, 0x5E) } else { Color32::from_rgb(0xF5, 0x9E, 0x0B) };
                    dot_label(ui, core_status, core_color, &colors);
                    if !s.sample_format_name.is_empty() {
                        dot_label(ui, &s.sample_format_name.to_uppercase(), Color32::from_rgb(0x22, 0xC5, 0x5E), &colors);
                    }
                });
                ui.add_space(4.0);
                ui.label(RichText::new("POWER: AC").font(FontId::monospace(9.5)).color(colors.text_muted).strong());
            });
        }
    });
}

fn dot_label(ui: &mut Ui, text: &str, dot_color: Color32, colors: &crate::theme::ThemeColors) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 0.0);
        let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(6.0, 6.0), egui::Sense::hover());
        ui.painter().circle_filled(rect.center(), 2.5, dot_color);
        ui.label(RichText::new(text).font(FontId::monospace(10.0)).color(colors.text_title).strong());
    });
}