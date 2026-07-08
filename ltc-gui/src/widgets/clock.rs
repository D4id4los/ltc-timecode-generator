use egui::{FontId, RichText, Ui};

use crate::app::{timecode_to_ms_string, timecode_to_string, AppState};

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

    // Measure the clock text width for centering
    let galley = ui.ctx().fonts_mut(|f| {
        f.layout_job(egui::text::LayoutJob::single_section(
            tc_str.clone(),
            egui::TextFormat {
                font_id: FontId::proportional(56.0),
                color: digit_color,
                ..Default::default()
            },
        ))
    });
    let text_width = galley.size().x;

    // Center the clock
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            let available = ui.available_width();
            let offset = ((available - text_width) / 2.0).max(0.0);
            ui.add_space(offset);
            ui.label(
                RichText::new(&tc_str)
                    .font(FontId::proportional(56.0))
                    .color(digit_color)
                    .strong(),
            );
        });

        // Milliseconds line
        ui.horizontal(|ui| {
            let ms_text = format!(".{}", ms_str.split('.').last().unwrap_or("000"));
            let ms_galley = ui.ctx().fonts_mut(|f| {
                f.layout_job(egui::text::LayoutJob::single_section(
                    ms_text.clone(),
                    egui::TextFormat {
                        font_id: FontId::proportional(18.0),
                        color: colors.text_muted,
                        ..Default::default()
                    },
                ))
            });
            let available = ui.available_width();
            let offset = ((available - ms_galley.size().x) / 2.0).max(0.0);
            ui.add_space(offset);
            ui.label(
                RichText::new(ms_text)
                    .font(FontId::proportional(18.0))
                    .color(colors.text_muted),
            );
        });
    });
}
