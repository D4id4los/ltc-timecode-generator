use egui::{FontId, RichText, Ui};

use crate::app::{AppState, AudioChannel, FRAME_RATE_OPTIONS};
use crate::theme::ACCENT;

/// Render the settings tab: frame rate, start timecode steppers, audio device, routing, volume.
pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    ui.vertical(|ui| {
        // ── Start Timecode Steppers ──
        section_title(ui, "Start Timecode", &colors);
        render_timecode_steppers(ui, state);
        ui.add_space(8.0);

        // ── Frame Rate ──
        section_title(ui, "Frame Rate", &colors);
        render_frame_rate(ui, state);
        ui.add_space(8.0);

        // ── Audio Device ──
        section_title(ui, "Output Audio Interface", &colors);
        render_audio_device(ui, state);
        ui.add_space(8.0);

        // ── Audio Routing & Settings ──
        section_title(ui, "Audio Routing & Settings", &colors);
        render_routing(ui, state);
    });
}

fn section_title(ui: &mut Ui, title: &str, colors: &crate::theme::ThemeColors) {
    ui.label(
        RichText::new(title)
            .font(FontId::proportional(14.0))
            .color(colors.text_title)
            .strong(),
    );
}

fn render_timecode_steppers(ui: &mut Ui, state: &mut AppState) {
    let fps = state.fps();
    let max_frames = fps.fps.ceil() as u32;

    let tc = &mut state.start_timecode;

    ui.horizontal(|ui| {
        stepper_field(ui, "Hours", &mut tc.hours, 24);
        ui.separator();
        stepper_field(ui, "Minutes", &mut tc.minutes, 60);
        ui.separator();
        stepper_field(ui, "Seconds", &mut tc.seconds, 60);
        ui.separator();
        stepper_field(ui, "Frames", &mut tc.frames, max_frames);
    });
}

fn stepper_field(ui: &mut Ui, label: &str, value: &mut u32, max: u32) {
    let visuals = ui.ctx().global_style().visuals.clone();
    let text_color = if visuals.dark_mode {
        egui::Color32::from_rgb(0xFF, 0xFF, 0xFF)
    } else {
        egui::Color32::from_rgb(0x09, 0x09, 0x0B)
    };
    ui.vertical(|ui| {
        ui.label(
            RichText::new(label)
                .font(FontId::proportional(11.0))
                .color(egui::Color32::from_gray(0x80)),
        );
        ui.horizontal(|ui| {
            if ui.button("▲").clicked() {
                *value = (*value + 1) % max;
            }
            ui.label(
                RichText::new(format!("{:02}", *value))
                    .font(FontId::proportional(24.0))
                    .color(text_color)
                    .strong(),
            );
            if ui.button("▼").clicked() {
                *value = if *value == 0 { max - 1 } else { *value - 1 };
            }
        });
    });
}

fn render_frame_rate(ui: &mut Ui, state: &mut AppState) {
    ui.horizontal_wrapped(|ui| {
        for (i, opt) in FRAME_RATE_OPTIONS.iter().enumerate() {
            let selected = i == state.fps_index;
            let btn = if selected {
                egui::Button::new(opt.name).fill(ACCENT.linear_multiply(0.25))
            } else {
                egui::Button::new(opt.name)
            };
            if ui.add(btn).clicked() {
                state.fps_index = i;
            }
        }
    });
    ui.add_space(2.0);
    ui.label(
        RichText::new(state.fps().description)
            .font(FontId::proportional(11.0))
            .color(egui::Color32::from_gray(0x80)),
    );
}

fn render_audio_device(ui: &mut Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        let device_names: Vec<String> = state
            .devices
            .iter()
            .map(|d| {
                if d.is_default {
                    format!("{} (Default)", d.name)
                } else {
                    d.name.clone()
                }
            })
            .collect();

        if device_names.is_empty() {
            ui.label("No devices found — using default");
        } else {
            let selected_text = device_names
                .get(state.selected_device)
                .cloned()
                .unwrap_or_else(|| "Default".to_string());
            egui::ComboBox::from_label("Device")
                .selected_text(selected_text)
                .show_ui(ui, |ui| {
                    for (i, name) in device_names.iter().enumerate() {
                        let mut idx = i;
                        ui.selectable_value(&mut idx, i, name);
                        if idx != i {
                            state.selected_device = idx;
                        }
                    }
                });
        }

        if ui.button("Refresh").clicked() {
            state.refresh_devices();
        }
    });
}

fn render_routing(ui: &mut Ui, state: &mut AppState) {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label("LTC Channel:");
            let mut ltc = state.ltc_channel;
            egui::ComboBox::from_id_salt("ltc_channel")
                .selected_text(state.ltc_channel.label())
                .show_ui(ui, |ui| {
                    for ch in AudioChannel::all() {
                        ui.selectable_value(&mut ltc, *ch, ch.label());
                    }
                });
            state.ltc_channel = ltc;
        });

        ui.horizontal(|ui| {
            ui.label("Beep Channel:");
            let mut beep = state.beep_channel;
            egui::ComboBox::from_id_salt("beep_channel")
                .selected_text(state.beep_channel.label())
                .show_ui(ui, |ui| {
                    for ch in AudioChannel::all() {
                        ui.selectable_value(&mut beep, *ch, ch.label());
                    }
                });
            state.beep_channel = beep;
        });

        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label("LTC Volume:");
            ui.add(
                egui::Slider::new(&mut state.ltc_volume, 0.0..=1.0).show_value(true),
            );
        });

        ui.horizontal(|ui| {
            ui.label("Beep Volume:");
            ui.add(
                egui::Slider::new(&mut state.beep_volume, 0.0..=1.0).show_value(true),
            );
        });

        ui.horizontal(|ui| {
            ui.label("Beep Freq (Hz):");
            ui.add(
                egui::Slider::new(&mut state.beep_frequency, 400.0..=2000.0)
                    .show_value(true),
            );
        });
    });
}
