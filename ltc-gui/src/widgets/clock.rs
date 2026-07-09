use egui::{FontId, RichText, Ui};

use crate::app::{timecode_to_ms_string, timecode_to_string, AppState};
use crate::theme::ACCENT;

/// Render the master studio time clock — large glowing timecode digits.
pub fn render(ui: &mut Ui, state: &AppState) {
    let colors = state.theme.colors();
    let tc = state.current_timecode;
    let fps = state.fps();

    let tc_str = timecode_to_string(tc, fps.drop_frame);
    let ms_str = timecode_to_ms_string(tc, fps.fps);

    let digit_color = if state.is_playing {
        colors.text_title
    } else {
        colors.text_muted
    };

    let sep = if fps.drop_frame { ";" } else { ":" };
    
    // Split the timecode string (e.g. "01:00:00:00")
    let parts: Vec<&str> = tc_str.split(|c| c == ':' || c == ';').collect();

    ui.vertical(|ui| {
        if parts.len() == 4 {
            // Render large glowing segmented clock
            crate::app::centered_horizontal_row(ui, "large_clock_row", 340.0, |ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(3.0, 0.0);
                
                // Hours
                ui.label(RichText::new(parts[0]).font(FontId::proportional(56.0)).color(digit_color).strong());
                
                // Separator 1 (grey)
                ui.label(RichText::new(sep).font(FontId::proportional(56.0)).color(colors.clock_sep).strong());
                
                // Minutes
                ui.label(RichText::new(parts[1]).font(FontId::proportional(56.0)).color(digit_color).strong());
                
                // Separator 2 (grey)
                ui.label(RichText::new(sep).font(FontId::proportional(56.0)).color(colors.clock_sep).strong());
                
                // Seconds
                ui.label(RichText::new(parts[2]).font(FontId::proportional(56.0)).color(digit_color).strong());
                
                // Separator 3 (orange)
                ui.label(RichText::new(sep).font(FontId::proportional(56.0)).color(ACCENT).strong());
                
                // Frames (orange)
                ui.label(RichText::new(parts[3]).font(FontId::proportional(56.0)).color(ACCENT).strong());
            });
        } else {
            // Fallback clock label
            crate::app::centered_horizontal_row(ui, "large_clock_fallback", 320.0, |ui| {
                ui.label(
                    RichText::new(&tc_str)
                        .font(FontId::proportional(56.0))
                        .color(ACCENT)
                        .strong(),
                );
            });
        }

        ui.add_space(4.0);

        // High-precision millisecond display (MS MATCH: HH:MM:SS.mmm)
        crate::app::centered_horizontal_row(ui, "ms_match_row", 220.0, |ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::new(6.0, 0.0);
            ui.label(
                RichText::new("MS MATCH:")
                    .font(FontId::proportional(11.0))
                    .color(colors.text_muted)
                    .strong(),
            );
            ui.label(
                RichText::new(ms_str)
                    .font(FontId::proportional(11.0))
                    .color(colors.text_title)
                    .strong(),
            );
        });
    });
}
