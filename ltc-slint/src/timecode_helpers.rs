use slint::SharedString;

use crate::AppWindow;

pub fn split_timecode_segments(tc_str: &str) -> [String; 7] {
    let parts: Vec<&str> = tc_str.split([':', ';']).collect();
    let sep = if tc_str.contains(';') { ';' } else { ':' };
    [
        parts[0].to_string(),
        ":".to_string(),
        parts[1].to_string(),
        ":".to_string(),
        parts[2].to_string(),
        sep.to_string(),
        parts[3].to_string(),
    ]
}

pub fn set_tc_segments(ui: &AppWindow, tc_str: &str) {
    let [hh, sep1, mm, sep2, ss, sep3, ff] = split_timecode_segments(tc_str);
    ui.set_tc_hh(SharedString::from(hh));
    ui.set_tc_sep1(SharedString::from(sep1));
    ui.set_tc_mm(SharedString::from(mm));
    ui.set_tc_sep2(SharedString::from(sep2));
    ui.set_tc_ss(SharedString::from(ss));
    ui.set_tc_sep3(SharedString::from(sep3));
    ui.set_tc_ff(SharedString::from(ff));
}