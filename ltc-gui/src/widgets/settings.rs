use egui::{Color32, FontId, RichText, Ui, Vec2, Sense};
use gui_engine::command::GuiCommand;
use gui_engine::{timecode::FPS_OPTIONS, ChannelSel};

use crate::app::AppState;
use crate::theme::ACCENT;
use super::bound;

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
    let max_frames = state.latest.fps().ceil() as u32;
    let is_playing = state.latest.is_playing;
    let tc = state.latest.start_timecode;

    ui.add_enabled_ui(!is_playing, |ui| {
        ui.columns(4, |cols| {
            stepper_card_col(&mut cols[0], "HOURS", tc.hours, 24, &colors, GuiCommand::HourUp, GuiCommand::HourDown, state);
            stepper_card_col(&mut cols[1], "MINUTES", tc.minutes, 60, &colors, GuiCommand::MinuteUp, GuiCommand::MinuteDown, state);
            stepper_card_col(&mut cols[2], "SECONDS", tc.seconds, 60, &colors, GuiCommand::SecondUp, GuiCommand::SecondDown, state);
            stepper_card_col(&mut cols[3], "FRAMES", tc.frames, max_frames, &colors, GuiCommand::FrameUp, GuiCommand::FrameDown, state);
        });
    });
}

fn stepper_card_col(
    ui: &mut Ui, label: &str, value: u32, _max: u32,
    colors: &crate::theme::ThemeColors,
    cmd_up: GuiCommand, cmd_down: GuiCommand, state: &mut AppState,
) {
    let card = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 8));
    card.show(ui, |ui| {
        ui.vertical_centered(|ui| {
            if ui.button(RichText::new("^").strong()).clicked() { state.send(cmd_up.clone()); }
            ui.add_space(1.0);
            ui.label(RichText::new(format!("{:02}", value)).font(FontId::monospace(20.0)).color(colors.text_title).strong());
            ui.add_space(1.0);
            ui.label(RichText::new(label).font(FontId::proportional(7.5)).color(colors.text_muted).strong());
            ui.add_space(1.0);
            if ui.button(RichText::new("v").strong()).clicked() { state.send(cmd_down.clone()); }
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
    bound::sync(state, |s| &mut s.sh.sample_rate, selected_rate);
    let selected_rate = *state.sh.sample_rate.value();
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
                        bound::select_value(state, |s| &mut s.sh.sample_rate, selected_rate, rate, GuiCommand::SetSampleRate);
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
                    bound::select_value(state, |s| &mut s.sh.sample_rate, selected_rate, rate, GuiCommand::SetSampleRate);
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
                    let selected = state.sh.selected_device.value().clone();
                    let selected_text = selected
                        .as_ref()
                        .and_then(|id| state.latest.devices.iter().find(|d| &d.id == id))
                        .map(|d| if d.is_default { format!("{} (Default)", d.name) } else { d.name.clone() })
                        .unwrap_or_else(|| "Default".to_string());
                    ui.label(RichText::new("Interface:").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
                    egui::ComboBox::from_id_salt("settings_device_combo")
                        .selected_text(&selected_text)
                        .show_ui(ui, |ui| {
                            for (i, dev) in state.latest.devices.clone().into_iter().enumerate() {
                                let active = selected.as_ref() == Some(&dev.id);
                                if ui.selectable_label(active, &device_names[i]).clicked() {
                                    bound::select_value(
                                        state,
                                        |s| &mut s.sh.selected_device,
                                        selected.clone(),
                                        Some(dev.id.clone()),
                                        |id| GuiCommand::SetDevice(id.unwrap_or_default()),
                                    );
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
    let ltc_truth = state.latest.ltc_channel;
    let beep_truth = state.latest.beep_channel;
    bound::sync(state, |s| &mut s.sh.ltc_channel, ltc_truth);
    bound::sync(state, |s| &mut s.sh.beep_channel, beep_truth);
    let ltc_ch = *state.sh.ltc_channel.value();
    let beep_ch = *state.sh.beep_channel.value();

    const CHANNEL_CHOICES: [(ChannelSel, &str); 3] = [
        (ChannelSel::Left, "Left"),
        (ChannelSel::Right, "Right"),
        (ChannelSel::Both, "Both"),
    ];

    ui.label(RichText::new("LTC OUTPUT").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        for (val, lbl) in CHANNEL_CHOICES {
            let active = ltc_ch == val;
            let btn = if active {
                egui::Button::new(RichText::new(lbl).strong().color(Color32::BLACK)).fill(ACCENT)
            } else {
                egui::Button::new(RichText::new(lbl)).stroke(egui::Stroke::new(0.5, colors.border_main)).fill(colors.card_bg)
            };
            if ui.add(btn).clicked() {
                bound::select_value(state, |s| &mut s.sh.ltc_channel, ltc_truth, val, GuiCommand::SetLtcChannel);
            }
        }
    });

    ui.add_space(8.0);
    ui.label(RichText::new("CLAPPER OUTPUT").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        for (val, lbl) in CHANNEL_CHOICES {
            let active = beep_ch == val;
            let btn = if active {
                egui::Button::new(RichText::new(lbl).strong().color(Color32::BLACK)).fill(ACCENT)
            } else {
                egui::Button::new(RichText::new(lbl)).stroke(egui::Stroke::new(0.5, colors.border_main)).fill(colors.card_bg)
            };
            if ui.add(btn).clicked() {
                bound::select_value(state, |s| &mut s.sh.beep_channel, beep_truth, val, GuiCommand::SetBeepChannel);
            }
        }
    });
}

fn render_sliders(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    let truth = state.latest.ltc_volume;
    ui.horizontal(|ui| {
        ui.label(RichText::new("LTC VOL").font(FontId::monospace(9.0)).color(colors.text_muted));
        bound::slider(ui, state, |s| &mut s.sh.ltc_volume, truth, 0.0..=1.0, Some(0.01), GuiCommand::SetLtcVolume);
        let shown = *state.sh.ltc_volume.value();
        ui.label(RichText::new(format!("{}%", (shown * 100.0).round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    let truth = state.latest.beep_volume;
    ui.horizontal(|ui| {
        ui.label(RichText::new("BEEP VOL").font(FontId::monospace(9.0)).color(colors.text_muted));
        bound::slider(ui, state, |s| &mut s.sh.beep_volume, truth, 0.0..=1.0, Some(0.01), GuiCommand::SetBeepVolume);
        let shown = *state.sh.beep_volume.value();
        ui.label(RichText::new(format!("{}%", (shown * 100.0).round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    let truth = state.latest.beep_frequency;
    ui.horizontal(|ui| {
        ui.label(RichText::new("PITCH").font(FontId::monospace(9.0)).color(colors.text_muted));
        bound::slider(ui, state, |s| &mut s.sh.beep_frequency, truth, 400.0..=2000.0, None, GuiCommand::SetBeepFrequency);
        let shown = *state.sh.beep_frequency.value();
        ui.label(RichText::new(format!("{} Hz", shown.round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    let truth = state.latest.beep_duration;
    ui.horizontal(|ui| {
        ui.label(RichText::new("DUR").font(FontId::monospace(9.0)).color(colors.text_muted));
        bound::slider(ui, state, |s| &mut s.sh.beep_duration, truth, 0.05..=2.0, Some(0.05), GuiCommand::SetBeepDuration);
        let shown = *state.sh.beep_duration.value();
        ui.label(RichText::new(format!("{:.0} ms", shown * 1000.0)).font(FontId::monospace(10.0)).color(colors.text_title));
    });
}
