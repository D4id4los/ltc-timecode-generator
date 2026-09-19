use crate::AppColors;
use slint::Global;

pub fn set_theme_palette(ui: &crate::AppWindow) {
    let d = &gui_engine::theme::DARK;
    let l = &gui_engine::theme::LIGHT;
    let c = &AppColors::get(ui);
    let c32 = |r: u8, g: u8, b: u8| slint::Color::from_rgb_u8(r, g, b);

    c.set_dark_accent(c32(d.accent.0, d.accent.1, d.accent.2));
    c.set_light_accent(c32(l.accent.0, l.accent.1, l.accent.2));
    c.set_dark_app_bg(c32(d.app_bg.0, d.app_bg.1, d.app_bg.2));
    c.set_light_app_bg(c32(l.app_bg.0, l.app_bg.1, l.app_bg.2));
    c.set_dark_card_bg(c32(d.card_bg.0, d.card_bg.1, d.card_bg.2));
    c.set_light_card_bg(c32(l.card_bg.0, l.card_bg.1, l.card_bg.2));
    c.set_dark_deep_bg(c32(d.deep_bg.0, d.deep_bg.1, d.deep_bg.2));
    c.set_light_deep_bg(c32(l.deep_bg.0, l.deep_bg.1, l.deep_bg.2));
    c.set_dark_nested_bg(c32(d.nested_bg.0, d.nested_bg.1, d.nested_bg.2));
    c.set_light_nested_bg(c32(l.nested_bg.0, l.nested_bg.1, l.nested_bg.2));
    c.set_dark_text_main(c32(d.text_main.0, d.text_main.1, d.text_main.2));
    c.set_light_text_main(c32(l.text_main.0, l.text_main.1, l.text_main.2));
    c.set_dark_text_title(c32(d.text_title.0, d.text_title.1, d.text_title.2));
    c.set_light_text_title(c32(l.text_title.0, l.text_title.1, l.text_title.2));
    c.set_dark_text_muted(c32(d.text_muted.0, d.text_muted.1, d.text_muted.2));
    c.set_light_text_muted(c32(l.text_muted.0, l.text_muted.1, l.text_muted.2));
    c.set_dark_border_main(c32(d.border_main.0, d.border_main.1, d.border_main.2));
    c.set_light_border_main(c32(l.border_main.0, l.border_main.1, l.border_main.2));
    c.set_dark_green(c32(d.success_green.0, d.success_green.1, d.success_green.2));
    c.set_light_green(c32(l.success_green.0, l.success_green.1, l.success_green.2));
    c.set_dark_red(c32(d.error_red.0, d.error_red.1, d.error_red.2));
    c.set_light_red(c32(l.error_red.0, l.error_red.1, l.error_red.2));
    c.set_dark_amber(c32(d.warning_amber.0, d.warning_amber.1, d.warning_amber.2));
    c.set_light_amber(c32(l.warning_amber.0, l.warning_amber.1, l.warning_amber.2));
    c.set_dark_clock_sep(c32(d.clock_sep.0, d.clock_sep.1, d.clock_sep.2));
    c.set_light_clock_sep(c32(l.clock_sep.0, l.clock_sep.1, l.clock_sep.2));
}