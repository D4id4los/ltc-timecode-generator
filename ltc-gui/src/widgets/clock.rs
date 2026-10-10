use crate::text::{text, TextStyle};
use egui::Ui;
use gui_engine::timecode;

use crate::app::{centered_horizontal_row, AppState};
use crate::theme::ACCENT;

pub fn render(ui: &mut Ui, state: &AppState) {
    let colors = state.theme.colors();
    let s = std::sync::Arc::clone(&state.latest);

    let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame());
    let ms_str = timecode::timecode_to_ms_string(s.current_timecode, s.fps());

    let parts: Vec<&str> = tc_str.split([':', ';']).collect();
    let digit_color = if s.is_playing {
        colors.text_title
    } else {
        colors.text_muted
    };
    let sep = if s.drop_frame() { ";" } else { ":" };

    let width = ui.available_width();
    let digit_font_size = (width / 8.0).clamp(24.0, 56.0);
    let sep_font_size = digit_font_size;
    let ms_font_size = (digit_font_size * 0.25).clamp(9.5, 11.0);
    let estimated_digit_width = width * 0.7;

    ui.vertical(|ui| {
        if parts.len() == 4 {
            centered_horizontal_row(ui, "large_clock_row", estimated_digit_width, |ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(2.0, 0.0);
                ui.label(
                    text(ui, parts[0])
                        .style(TextStyle::Display, &colors)
                        .size(digit_font_size)
                        .color(digit_color),
                );
                ui.label(
                    text(ui, sep)
                        .style(TextStyle::Display, &colors)
                        .size(sep_font_size)
                        .color(colors.clock_sep),
                );
                ui.label(
                    text(ui, parts[1])
                        .style(TextStyle::Display, &colors)
                        .size(digit_font_size)
                        .color(digit_color),
                );
                ui.label(
                    text(ui, sep)
                        .style(TextStyle::Display, &colors)
                        .size(sep_font_size)
                        .color(colors.clock_sep),
                );
                ui.label(
                    text(ui, parts[2])
                        .style(TextStyle::Display, &colors)
                        .size(digit_font_size)
                        .color(digit_color),
                );
                ui.label(
                    text(ui, sep)
                        .style(TextStyle::Display, &colors)
                        .size(sep_font_size)
                        .color(ACCENT),
                );
                ui.label(
                    text(ui, parts[3])
                        .style(TextStyle::Display, &colors)
                        .size(digit_font_size)
                        .color(ACCENT),
                );
            });
        } else {
            centered_horizontal_row(ui, "large_clock_fallback", estimated_digit_width, |ui| {
                ui.label(
                    text(ui, &tc_str)
                        .style(TextStyle::Display, &colors)
                        .size(digit_font_size)
                        .color(ACCENT),
                );
            });
        }
        ui.add_space(4.0);
        centered_horizontal_row(ui, "ms_match_row", 200.0, |ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::new(6.0, 0.0);
            ui.label(
                text(ui, "MS MATCH:")
                    .style(TextStyle::Status, &colors)
                    .size(ms_font_size)
                    .color(colors.text_muted),
            );
            ui.label(
                text(ui, ms_str)
                    .style(TextStyle::Status, &colors)
                    .size(ms_font_size)
                    .color(colors.text_title),
            );
        });
    });
}
