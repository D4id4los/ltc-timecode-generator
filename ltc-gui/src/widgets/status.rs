use egui::{Color32, FontId, RichText, Ui};
use gui_engine::state::AppStateSnapshot;

use crate::app::AppState;
use crate::theme::ThemeColors;

pub fn render(ui: &mut Ui, state: &AppState) {
    let colors = state.theme.colors();
    let s = std::sync::Arc::clone(&state.latest);
    let frame = egui::Frame::new()
        .fill(colors.deep_bg)
        .inner_margin(egui::Margin::symmetric(12, 6));
    frame.show(ui, |ui| {
        let width = ui.available_width();
        if width > 600.0 {
            render_wide_status(ui, &s, &colors);
        } else {
            render_narrow_status(ui, &s, &colors);
        }
    });
}

/// Pure core-status pill: `(label, dot color)` — running is green, standby
/// amber. `wide` selects the prefixed long-form label.
fn core_status(is_playing: bool, wide: bool) -> (&'static str, Color32) {
    let color = if is_playing {
        Color32::from_rgb(0x22, 0xC5, 0x5E)
    } else {
        Color32::from_rgb(0xF5, 0x9E, 0x0B)
    };
    let text = match (wide, is_playing) {
        (true, true) => "AUDIO CORE: RUNNING",
        (true, false) => "AUDIO CORE: STANDBY",
        (false, true) => "RUNNING",
        (false, false) => "STANDBY",
    };
    (text, color)
}

/// Pure audio-output pill text: device count when known, jack fallback otherwise.
fn audio_out_text(device_count: usize) -> String {
    if device_count > 0 {
        format!("AUDIO OUT: {} DEVICES FOUND", device_count)
    } else {
        "AUDIO OUT: LINE / JACK".to_string()
    }
}

/// Wide (> 600 px) status row.
fn render_wide_status(ui: &mut Ui, s: &AppStateSnapshot, colors: &ThemeColors) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = egui::Vec2::new(16.0, 0.0);
        let os = std::env::consts::OS.to_uppercase();
        dot_label(ui, &format!("OS: {}", os), Color32::from_rgb(0x22, 0xC5, 0x5E), colors);
        let dev_text = audio_out_text(s.devices.len());
        dot_label(ui, &dev_text, Color32::from_rgb(0x22, 0xC5, 0x5E), colors);
        let (core_text, core_color) = core_status(s.is_playing, true);
        dot_label(ui, core_text, core_color, colors);
        if !s.sample_format_name.is_empty() {
            dot_label(ui, &format!("SAMPLE: {}", s.sample_format_name.to_uppercase()), Color32::from_rgb(0x22, 0xC5, 0x5E), colors);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(RichText::new("POWER: AC").font(FontId::monospace(10.0)).color(colors.text_muted).strong());
        });
    });
}

/// Narrow (≤ 600 px) stacked status layout.
fn render_narrow_status(ui: &mut Ui, s: &AppStateSnapshot, colors: &ThemeColors) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::new(12.0, 0.0);
            let os = std::env::consts::OS.to_uppercase();
            dot_label(ui, &os, Color32::from_rgb(0x22, 0xC5, 0x5E), colors);
            let (core_text, core_color) = core_status(s.is_playing, false);
            dot_label(ui, core_text, core_color, colors);
            if !s.sample_format_name.is_empty() {
                dot_label(ui, &s.sample_format_name.to_uppercase(), Color32::from_rgb(0x22, 0xC5, 0x5E), colors);
            }
        });
        ui.add_space(4.0);
        ui.label(RichText::new("POWER: AC").font(FontId::monospace(9.5)).color(colors.text_muted).strong());
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

#[cfg(test)]
mod tests {
    use super::*;
    use gui_engine::state::AppStateSnapshot;

    fn snapshot(playing: bool, devices: usize) -> AppStateSnapshot {
        let mut s = AppStateSnapshot::initial();
        s.is_playing = playing;
        for i in 0..devices {
            s.devices.push(gui_engine::AudioDeviceInfo {
                id: format!("dev{}", i),
                name: format!("Device {}", i),
                is_default: i == 0,
                formats: Vec::new(),
                channels_min: 0,
                channels_max: 0,
                sample_rate_min: 0,
                sample_rate_max: 0,
                buffer_min: 0,
                buffer_max: 0,
            });
        }
        s
    }

    #[test]
    fn core_status_colors_run_green_and_standby_amber() {
        let green = Color32::from_rgb(0x22, 0xC5, 0x5E);
        let amber = Color32::from_rgb(0xF5, 0x9E, 0x0B);
        assert_eq!(core_status(true, true).1, green);
        assert_eq!(core_status(false, true).1, amber);
        assert_eq!(core_status(true, false).1, green);
        assert_eq!(core_status(false, false).1, amber);
    }

    #[test]
    fn core_status_labels_differ_only_by_width_prefix() {
        // test-lint: allow(text-pin): formatter output is the contract
        let (wide_run, _) = core_status(true, true);
        let (narrow_run, _) = core_status(true, false);
        assert!(wide_run.contains(narrow_run), "wide label must extend the narrow one");
        let (wide_idle, _) = core_status(false, true);
        let (narrow_idle, _) = core_status(false, false);
        assert!(wide_idle.contains(narrow_idle));
    }

    #[test]
    fn audio_out_text_counts_devices_or_falls_back_to_jack() {
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(audio_out_text(0), "AUDIO OUT: LINE / JACK");
        assert_eq!(audio_out_text(3), "AUDIO OUT: 3 DEVICES FOUND");
    }

    #[test]
    fn status_layouts_agree_on_playing_state_from_snapshot() {
        // The wide/narrow split must not change the core-status decision.
        let s = snapshot(true, 2);
        assert_eq!(core_status(s.is_playing, true).1, core_status(s.is_playing, false).1);
    }
}
