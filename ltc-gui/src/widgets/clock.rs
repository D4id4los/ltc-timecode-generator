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

    // Use vertical_centered to automatically center all children horizontally
    ui.vertical_centered(|ui| {
        ui.label(
            RichText::new(&tc_str)
                .font(FontId::proportional(56.0))
                .color(digit_color)
                .strong(),
        );

        // Milliseconds line
        let ms_text = format!(".{}", ms_str.split('.').last().unwrap_or("000"));
        ui.label(
            RichText::new(ms_text)
                .font(FontId::proportional(18.0))
                .color(colors.text_muted),
        );
    });
}
