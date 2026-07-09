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

    // Split the timecode string (e.g. "01:00:00:00")
    let parts: Vec<&str> = tc_str.split(|c| c == ':' || c == ';').collect();

    let digit_color = if state.is_playing {
        colors.text_title
    } else {
        colors.text_muted
    };

    let sep = if fps.drop_frame { ";" } else { ":" };
    
    // Dynamically scale font size based on available window width
    let width = ui.available_width();
    let digit_font_size = (width / 8.0).clamp(24.0, 56.0);
    let sep_font_size = digit_font_size;
    let ms_font_size = (digit_font_size * 0.25).clamp(9.5, 11.0);

    // Estimate total rendered width for proper centering on narrow screens
    let estimated_digit_width = width * 0.7;

    ui.vertical(|ui| {
        if parts.len() == 4 {
            // Render large glowing segmented clock
            crate::app::centered_horizontal_row(ui, "large_clock_row", estimated_digit_width, |ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(2.0, 0.0);
                
                // Hours
                ui.label(RichText::new(parts[0]).font(FontId::proportional(digit_font_size)).color(digit_color).strong());
                
                // Separator 1 (grey)
                ui.label(RichText::new(sep).font(FontId::proportional(sep_font_size)).color(colors.clock_sep).strong());
                
                // Minutes
                ui.label(RichText::new(parts[1]).font(FontId::proportional(digit_font_size)).color(digit_color).strong());
                
                // Separator 2 (grey)
                ui.label(RichText::new(sep).font(FontId::proportional(sep_font_size)).color(colors.clock_sep).strong());
                
                // Seconds
                ui.label(RichText::new(parts[2]).font(FontId::proportional(digit_font_size)).color(digit_color).strong());
                
                // Separator 3 (orange)
                ui.label(RichText::new(sep).font(FontId::proportional(sep_font_size)).color(ACCENT).strong());
                
                // Frames (orange)
                ui.label(RichText::new(parts[3]).font(FontId::proportional(digit_font_size)).color(ACCENT).strong());
            });
        } else {
            // Fallback clock label
            crate::app::centered_horizontal_row(ui, "large_clock_fallback", estimated_digit_width, |ui| {
                ui.label(
                    RichText::new(&tc_str)
                        .font(FontId::proportional(digit_font_size))
                        .color(ACCENT)
                        .strong(),
                );
            });
        }

        ui.add_space(4.0);

        // High-precision millisecond display (MS MATCH: HH:MM:SS.mmm)
        crate::app::centered_horizontal_row(ui, "ms_match_row", 200.0, |ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::new(6.0, 0.0);
            ui.label(
                RichText::new("MS MATCH:")
                    .font(FontId::proportional(ms_font_size))
                    .color(colors.text_muted)
                    .strong(),
            );
            ui.label(
                RichText::new(ms_str)
                    .font(FontId::proportional(ms_font_size))
                    .color(colors.text_title)
                    .strong(),
            );
        });
    });
}
