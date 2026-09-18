use egui::{Color32, FontId, RichText, Ui, Vec2, Sense};
use gui_engine::command::GuiCommand;
use gui_engine::timecode::FPS_OPTIONS;

use crate::app::AppState;
use crate::theme::ACCENT;

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let playing = state.latest.is_playing;

    let frame = egui::Frame::group(ui.style())
        .fill(colors.card_bg)
        .corner_radius(12.0)
        .stroke(egui::Stroke::new(1.5, colors.border_main))
        .inner_margin(egui::Margin::same(16));
    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            // 1. Start Timecode
            ui.horizontal(|ui| {
                ui.label(RichText::new("SET STARTING TIMECODE").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
                if playing {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let warn = egui::Frame::new()
                            .fill(Color32::from_rgb(0xF5, 0x9E, 0x0B).linear_multiply(0.1))
                            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(0xF5, 0x9E, 0x0B).linear_multiply(0.2)))
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::symmetric(10, 4));
                        warn.show(ui, |ui| {
                            ui.label(RichText::new("STOP STREAM TO EDIT").color(Color32::from_rgb(0xF5, 0x9E, 0x0B)).strong().font(FontId::proportional(9.0)));
                        });
                    });
                }
            });
            ui.add_space(8.0);
            render_timecode_steppers(ui, state);
            ui.add_space(16.0);

            ui.label(RichText::new("SELECT FRAME RATE").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
            ui.add_space(8.0);
            render_frame_rate(ui, state);
            ui.add_space(16.0);

            ui.label(RichText::new("SAMPLE RATE").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
            ui.add_space(8.0);
            render_sample_rate(ui, state);
            ui.add_space(16.0);

            ui.label(RichText::new("OUTPUT AUDIO INTERFACE SELECTION").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
            ui.add_space(8.0);
            render_audio_device(ui, state);
            ui.add_space(16.0);

            ui.label(RichText::new("AUDIO ROUTING & SETTINGS").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
            ui.add_space(8.0);
            render_routing_and_volume(ui, state);
        });
    });
}

fn render_timecode_steppers(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let max_frames = state.latest.fps.ceil() as u32;
    let is_playing = state.latest.is_playing;
    let tc = state.latest.start_timecode;

    ui.add_enabled_ui(!is_playing, |ui| {
        ui.columns(4, |cols| {
            stepper_card_col(&mut cols[0], "HOURS", tc.hours, 24, &colors, || state.send(GuiCommand::HourUp), || state.send(GuiCommand::HourDown));
            stepper_card_col(&mut cols[1], "MINUTES", tc.minutes, 60, &colors, || state.send(GuiCommand::MinuteUp), || state.send(GuiCommand::MinuteDown));
            stepper_card_col(&mut cols[2], "SECONDS", tc.seconds, 60, &colors, || state.send(GuiCommand::SecondUp), || state.send(GuiCommand::SecondDown));
            stepper_card_col(&mut cols[3], "FRAMES", tc.frames, max_frames, &colors, || state.send(GuiCommand::FrameUp), || state.send(GuiCommand::FrameDown));
        });
    });
}

fn stepper_card_col(
    ui: &mut Ui, label: &str, value: u32, _max: u32,
    colors: &crate::theme::ThemeColors,
    on_up: impl FnOnce(), on_down: impl FnOnce(),
) {
    let card = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 8));
    card.show(ui, |ui| {
        ui.vertical_centered(|ui| {
            if ui.button(RichText::new("^").strong()).clicked() { on_up(); }
            ui.add_space(1.0);
            ui.label(RichText::new(format!("{:02}", value)).font(FontId::monospace(20.0)).color(colors.text_title).strong());
            ui.add_space(1.0);
            ui.label(RichText::new(label).font(FontId::proportional(7.5)).color(colors.text_muted).strong());
            ui.add_space(1.0);
            if ui.button(RichText::new("v").strong()).clicked() { on_down(); }
        });
    });
}

fn render_frame_rate(ui: &mut Ui, state: &mut AppState) {
    let width = ui.available_width();
    let is_playing = state.latest.is_playing;
    ui.add_enabled_ui(!is_playing, |ui| {
        if width > 520.0 {
            ui.columns(5, |cols| {
                for (i, opt) in FPS_OPTIONS.iter().enumerate() {
                    frame_rate_card(&mut cols[i], i, opt, state);
                }
            });
        } else {
            let usable = ((width - 32.0) / 2.0).max(1.0);
            if usable < 90.0 || width < 260.0 {
                for (i, opt) in FPS_OPTIONS.iter().enumerate() {
                    frame_rate_card(ui, i, opt, state);
                    ui.add_space(6.0);
                }
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 8.0);
                    for (i, opt) in FPS_OPTIONS.iter().enumerate() {
                        ui.scope(|ui| { ui.set_max_width(usable); frame_rate_card(ui, i, opt, state); });
                    }
                });
            }
        }
    });
}

fn frame_rate_card(ui: &mut Ui, index: usize, opt: &gui_engine::timecode::FpsOption, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_selected = index == state.latest.fps_index;
    let is_playing = state.latest.is_playing;
    let card = egui::Frame::new()
        .fill(if is_selected { ACCENT.linear_multiply(0.08) } else { colors.deep_bg })
        .stroke(egui::Stroke::new(if is_selected { 1.5 } else { 1.0 }, if is_selected { ACCENT } else { colors.border_main }))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(10));
    let response = card.show(ui, |ui| {
        ui.set_min_height(60.0);
        ui.vertical(|ui| {
            ui.label(RichText::new(opt.name).font(FontId::monospace(13.0)).color(if is_selected { ACCENT } else { colors.text_title }).strong());
            ui.add_space(2.0);
            ui.label(RichText::new(opt.description).font(FontId::proportional(9.0)).color(colors.text_muted));
        });
    }).response;
    let click_sense = if is_playing { Sense::hover() } else { Sense::click() };
    if ui.interact(response.rect, response.id, click_sense).clicked() {
        state.send(GuiCommand::SetFpsIndex(index));
    }
}

fn render_sample_rate(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_playing = state.latest.is_playing;
    let rates = gui_engine::SAMPLE_RATE_OPTIONS;
    let selected_rate = state.latest.sample_rate;
    ui.add_enabled_ui(!is_playing, |ui| {
        let width = ui.available_width();
        if width > 300.0 {
            ui.horizontal(|ui| {
                for &rate in rates {
                    let is_selected = rate == selected_rate;
                    let btn = if is_selected {
                        egui::Button::new(RichText::new(format!("{} Hz", rate)).strong().color(Color32::BLACK)).fill(ACCENT)
                    } else {
                        egui::Button::new(RichText::new(format!("{} Hz", rate))).stroke(egui::Stroke::new(0.5, colors.border_main)).fill(colors.card_bg)
                    };
                    if ui.add(btn).clicked() {
                        state.send(GuiCommand::SetSampleRate(rate));
                    }
                }
            });
        } else {
            for &rate in rates {
                let is_selected = rate == selected_rate;
                let btn = if is_selected {
                    egui::Button::new(RichText::new(format!("{} Hz", rate)).strong().color(Color32::BLACK)).fill(ACCENT)
                } else {
                    egui::Button::new(RichText::new(format!("{} Hz", rate))).stroke(egui::Stroke::new(0.5, colors.border_main)).fill(colors.card_bg)
                };
                if ui.add(btn).clicked() {
                    state.send(GuiCommand::SetSampleRate(rate));
                }
                ui.add_space(6.0);
            }
        }
    });
}

fn render_audio_device(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let card = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(16));
    card.show(ui, |ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                let device_names: Vec<String> = state.latest.devices.iter().map(|d| {
                    if d.is_default { format!("{} (Default)", d.name) } else { d.name.clone() }
                }).collect();
                if device_names.is_empty() {
                    ui.label(RichText::new("No devices found — using default output").font(FontId::monospace(12.0)).color(colors.text_muted));
                } else {
                    let selected_text = device_names.get(state.latest.selected_device).cloned().unwrap_or_else(|| "Default".to_string());
                    ui.label(RichText::new("Interface:").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
                    egui::ComboBox::from_id_salt("settings_device_combo")
                        .selected_text(&selected_text)
                        .show_ui(ui, |ui| {
                            for (i, _name) in device_names.iter().enumerate() {
                                if ui.selectable_label(false, &device_names[i]).clicked() {
                                    state.send(GuiCommand::SetDevice(i));
                                }
                            }
                        });
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh List").clicked() {
                        state.send(GuiCommand::RefreshDevices);
                    }
                });
            });
            ui.add_space(8.0);
            let info_frame = egui::Frame::new()
                .fill(colors.card_bg)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 6));
            info_frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(6.0, 6.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.0, ACCENT);
                    ui.label(RichText::new("Sends SMPTE Linear Timecode audio to mixers, USB-DAC, or sync adapters.").font(FontId::proportional(10.0)).color(colors.text_muted));
                });
            });
        });
    });
}

fn render_routing_and_volume(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let width = ui.available_width();
    let container = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(8));
    container.show(ui, |ui| {
        if width > 500.0 {
            ui.columns(2, |cols| {
                cols[0].vertical(|ui| { render_routing_buttons(ui, state); });
                cols[1].vertical(|ui| { render_sliders(ui, state); });
            });
        } else {
            render_routing_buttons(ui, state);
            ui.add_space(8.0);
            render_sliders(ui, state);
        }
    });
}

fn render_routing_buttons(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let ltc_ch = &state.latest.ltc_channel;
    let beep_ch = &state.latest.beep_channel;

    ui.label(RichText::new("LTC OUTPUT").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        for (lbl, val) in &[("Left", "left"), ("Right", "right"), ("Both", "both")] {
            let active = ltc_ch.as_str() == *val;
            let btn = if active {
                egui::Button::new(RichText::new(*lbl).strong().color(Color32::BLACK)).fill(ACCENT)
            } else {
                egui::Button::new(RichText::new(*lbl)).stroke(egui::Stroke::new(0.5, colors.border_main)).fill(colors.card_bg)
            };
            if ui.add(btn).clicked() {
                state.send(GuiCommand::SetLtcChannel(val.to_string()));
            }
        }
    });

    ui.add_space(8.0);
    ui.label(RichText::new("CLAPPER OUTPUT").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        for (lbl, val) in &[("Left", "left"), ("Right", "right"), ("Both", "both")] {
            let active = beep_ch.as_str() == *val;
            let btn = if active {
                egui::Button::new(RichText::new(*lbl).strong().color(Color32::BLACK)).fill(ACCENT)
            } else {
                egui::Button::new(RichText::new(*lbl)).stroke(egui::Stroke::new(0.5, colors.border_main)).fill(colors.card_bg)
            };
            if ui.add(btn).clicked() {
                state.send(GuiCommand::SetBeepChannel(val.to_string()));
            }
        }
    });
}

fn render_sliders(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    let mut vol = state.latest.ltc_volume;
    ui.horizontal(|ui| {
        ui.label(RichText::new("LTC VOL").font(FontId::monospace(9.0)).color(colors.text_muted));
        if ui.add(egui::Slider::new(&mut vol, 0.0..=1.0).step_by(0.01).show_value(false)).changed() {
            state.send(GuiCommand::SetLtcVolume(vol));
        }
        ui.label(RichText::new(format!("{}%", (vol * 100.0).round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    let mut beep_vol = state.latest.beep_volume;
    ui.horizontal(|ui| {
        ui.label(RichText::new("BEEP VOL").font(FontId::monospace(9.0)).color(colors.text_muted));
        if ui.add(egui::Slider::new(&mut beep_vol, 0.0..=1.0).step_by(0.01).show_value(false)).changed() {
            state.send(GuiCommand::SetBeepVolume(beep_vol));
        }
        ui.label(RichText::new(format!("{}%", (beep_vol * 100.0).round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    let mut freq = state.latest.beep_frequency;
    ui.horizontal(|ui| {
        ui.label(RichText::new("PITCH").font(FontId::monospace(9.0)).color(colors.text_muted));
        if ui.add(egui::Slider::new(&mut freq, 400.0..=2000.0).show_value(false)).changed() {
            state.send(GuiCommand::SetBeepFrequency(freq));
        }
        ui.label(RichText::new(format!("{} Hz", freq.round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    let mut dur = state.latest.beep_duration;
    ui.horizontal(|ui| {
        ui.label(RichText::new("DUR").font(FontId::monospace(9.0)).color(colors.text_muted));
        if ui.add(egui::Slider::new(&mut dur, 0.05..=2.0).step_by(0.05).show_value(false)).changed() {
            state.send(GuiCommand::SetBeepDuration(dur));
        }
        ui.label(RichText::new(format!("{:.0} ms", dur * 1000.0)).font(FontId::monospace(10.0)).color(colors.text_title));
    });
}