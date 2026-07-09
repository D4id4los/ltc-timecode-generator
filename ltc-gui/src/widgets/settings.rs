use egui::{Color32, FontId, RichText, Ui, Vec2, Sense};

use crate::app::{AppState, AudioChannel, FRAME_RATE_OPTIONS};
use crate::theme::ACCENT;

/// Render the settings tab: frame rate, start timecode steppers, audio device, routing, volume.
pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    let frame = egui::Frame::group(ui.style())
        .fill(colors.card_bg)
        .corner_radius(12.0)
        .stroke(egui::Stroke::new(1.5, colors.border_main))
        .inner_margin(egui::Margin::same(16));

    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            // 1. Start Timecode Steppers
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("⚙️ SET STARTING TIMECODE")
                        .font(FontId::proportional(11.0))
                        .color(colors.text_muted)
                        .strong(),
                );
                
                if state.is_playing {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let warn_frame = egui::Frame::new()
                            .fill(Color32::from_rgb(0xF5, 0x9E, 0x0B).linear_multiply(0.1))
                            .stroke(egui::Stroke::new(1.0, Color32::from_rgb(0xF5, 0x9E, 0x0B).linear_multiply(0.2)))
                            .corner_radius(6.0)
                            .inner_margin(egui::Margin::symmetric(10, 4));
                        warn_frame.show(ui, |ui| {
                            ui.label(
                                RichText::new("STOP STREAM TO EDIT STARTING TIME")
                                    .color(Color32::from_rgb(0xF5, 0x9E, 0x0B))
                                    .strong()
                                    .font(FontId::proportional(9.0)),
                            );
                        });
                    });
                }
            });
            ui.add_space(8.0);
            render_timecode_steppers(ui, state);
            ui.add_space(16.0);

            // 2. Select Frame Rate
            ui.label(
                RichText::new("📈 SELECT FRAME RATE")
                    .font(FontId::proportional(11.0))
                    .color(colors.text_muted)
                    .strong(),
            );
            ui.add_space(8.0);
            render_frame_rate(ui, state);
            ui.add_space(16.0);

            // 3. Output Audio Interface
            ui.label(
                RichText::new("🔊 OUTPUT AUDIO INTERFACE SELECTION")
                    .font(FontId::proportional(11.0))
                    .color(colors.text_muted)
                    .strong(),
            );
            ui.add_space(8.0);
            render_audio_device(ui, state);
            ui.add_space(16.0);

            // 4. Audio Routing & Settings
            ui.label(
                RichText::new("🎛️ AUDIO ROUTING & SETTINGS (DUAL-CHANNEL SPLITS)")
                    .font(FontId::proportional(11.0))
                    .color(colors.text_muted)
                    .strong(),
            );
            ui.add_space(8.0);
            render_routing_and_volume(ui, state);
        });
    });
}

fn render_timecode_steppers(ui: &mut Ui, state: &mut AppState) {
    let fps = state.fps();
    let max_frames = fps.fps.ceil() as u32;
    let theme = state.theme;
    let is_playing = state.is_playing;
    let tc = &mut state.start_timecode;
    
    ui.add_enabled_ui(!is_playing, |ui| {
        ui.columns(4, |cols| {
            stepper_card(&mut cols[0], "HOURS", &mut tc.hours, 24, theme);
            stepper_card(&mut cols[1], "MINUTES", &mut tc.minutes, 60, theme);
            stepper_card(&mut cols[2], "SECONDS", &mut tc.seconds, 60, theme);
            stepper_card(&mut cols[3], "FRAMES", &mut tc.frames, max_frames, theme);
        });
    });
}

fn stepper_card(ui: &mut Ui, label: &str, value: &mut u32, max: u32, theme: crate::theme::Theme) {
    let colors = theme.colors();
    let card = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 8));

    card.show(ui, |ui| {
        ui.set_min_height(96.0);
        ui.vertical_centered(|ui| {
            // Up Button
            let up_btn = egui::Button::new(RichText::new("^").strong())
                .fill(Color32::TRANSPARENT);
            if ui.add(up_btn).clicked() {
                *value = (*value + 1) % max;
            }

            ui.add_space(2.0);
            
            // Value
            ui.label(
                RichText::new(format!("{:02}", *value))
                    .font(FontId::monospace(24.0))
                    .color(colors.text_title)
                    .strong(),
            );
            
            // Label
            ui.label(
                RichText::new(label)
                    .font(FontId::proportional(8.0))
                    .color(colors.text_muted)
                    .strong(),
            );

            ui.add_space(2.0);

            // Down Button
            let down_btn = egui::Button::new(RichText::new("v").strong())
                .fill(Color32::TRANSPARENT);
            if ui.add(down_btn).clicked() {
                *value = if *value == 0 { max - 1 } else { *value - 1 };
            }
        });
    });
}

fn render_frame_rate(ui: &mut Ui, state: &mut AppState) {
    let width = ui.available_width();
    let is_playing = state.is_playing;
    
    ui.add_enabled_ui(!is_playing, |ui| {
        if width > 520.0 {
            ui.columns(5, |cols| {
                for (i, opt) in FRAME_RATE_OPTIONS.iter().enumerate() {
                    frame_rate_card(&mut cols[i], i, opt, state);
                }
            });
        } else {
            // Flow layout
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 8.0);
                let col_w = (width - 24.0) / 2.0;
                for (i, opt) in FRAME_RATE_OPTIONS.iter().enumerate() {
                    ui.scope(|ui| {
                        ui.set_max_width(col_w);
                        frame_rate_card(ui, i, opt, state);
                    });
                }
            });
        }
    });
}

fn frame_rate_card(ui: &mut Ui, index: usize, opt: &crate::app::FrameRateOption, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_selected = index == state.fps_index;
    
    let card = egui::Frame::new()
        .fill(if is_selected { ACCENT.linear_multiply(0.08) } else { colors.deep_bg })
        .stroke(egui::Stroke::new(
            if is_selected { 1.5 } else { 1.0 },
            if is_selected { ACCENT } else { colors.border_main }
        ))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(10));

    let response = card.show(ui, |ui| {
        ui.set_min_height(60.0);
        ui.vertical(|ui| {
            ui.label(
                RichText::new(opt.name)
                    .font(FontId::monospace(13.0))
                    .color(if is_selected { ACCENT } else { colors.text_title })
                    .strong(),
            );
            ui.add_space(2.0);
            ui.label(
                RichText::new(opt.description)
                    .font(FontId::proportional(9.0))
                    .color(colors.text_muted),
            );
        });
    }).response;

    let click_sense = if state.is_playing { Sense::hover() } else { Sense::click() };
    if ui.interact(response.rect, response.id, click_sense).clicked() {
        state.fps_index = index;
    }
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
                    ui.label(RichText::new("No devices found — using default output").font(FontId::monospace(12.0)).color(colors.text_muted));
                } else {
                    let selected_text = device_names
                        .get(state.selected_device)
                        .cloned()
                        .unwrap_or_else(|| "Default".to_string());
                    
                    ui.label(RichText::new("Interface:").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
                    egui::ComboBox::from_id_salt("settings_device_combo")
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

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh List").clicked() {
                        state.refresh_devices();
                    }
                });
            });

            ui.add_space(8.0);
            
            // Helpful note inside device selection
            let info_frame = egui::Frame::new()
                .fill(colors.card_bg)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 6));
            
            info_frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(6.0, 6.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 3.0, ACCENT);
                    ui.label(
                        RichText::new("Active Mode: Sends SMPTE Linear Timecode audio directly to mixers, USB-DAC, or sync adapters.")
                            .font(FontId::proportional(10.0))
                            .color(colors.text_muted)
                    );
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
        .inner_margin(egui::Margin::same(16));

    container.show(ui, |ui| {
        if width > 500.0 {
            ui.columns(2, |cols| {
                render_routing_column(&mut cols[0], state);
                render_volume_column(&mut cols[1], state);
            });
        } else {
            ui.vertical(|ui| {
                render_routing_column(ui, state);
                ui.add_space(12.0);
                render_volume_column(ui, state);
            });
        }
        
        ui.add_space(12.0);
        
        // Pro tip box
        let tip_frame = egui::Frame::new()
            .fill(colors.card_bg)
            .stroke(egui::Stroke::new(1.0, colors.border_main))
            .corner_radius(8.0)
            .inner_margin(egui::Margin::same(12));
        
        tip_frame.show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("💡 PRO-TIP FOR INDIE SHOOTS:").font(FontId::proportional(10.5)).color(colors.text_title).strong());
                });
                ui.add_space(4.0);
                ui.label(
                    RichText::new("Map LTC to Right and Clapper to Left. Route the stereo output of your \
                                  device into camera mic input. It writes LTC on Channel 1 (for auto-sync in Resolve) \
                                  and a clear clapper scratch beep on Channel 2!")
                        .font(FontId::proportional(10.0))
                        .color(colors.text_muted)
                );
            });
        });
    });
}

fn render_routing_column(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    ui.vertical(|ui| {
        // LTC route
        ui.label(RichText::new("LTC AUDIO OUTPUT ROUTE").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for ch in AudioChannel::all() {
                let is_active = state.ltc_channel == *ch;
                let btn = if is_active {
                    egui::Button::new(RichText::new(ch.label()).strong().color(Color32::BLACK)).fill(ACCENT)
                } else {
                    egui::Button::new(RichText::new(ch.label()).color(colors.text_muted)).fill(colors.card_bg)
                };
                if ui.add(btn).clicked() {
                    state.ltc_channel = *ch;
                }
            }
        });
        
        ui.add_space(12.0);

        // Clapper route
        ui.label(RichText::new("DIGITAL CLAPPER ROUTE").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            for ch in AudioChannel::all() {
                let is_active = state.beep_channel == *ch;
                let btn = if is_active {
                    egui::Button::new(RichText::new(ch.label()).strong().color(Color32::BLACK)).fill(ACCENT)
                } else {
                    egui::Button::new(RichText::new(ch.label()).color(colors.text_muted)).fill(colors.card_bg)
                };
                if ui.add(btn).clicked() {
                    state.beep_channel = *ch;
                }
            }
        });
    });
}

fn render_volume_column(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    ui.vertical(|ui| {
        // LTC volume slider
        ui.horizontal(|ui| {
            ui.label(RichText::new("🔊 LTC VOLUME LEVEL").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{}%", (state.ltc_volume * 100.0).round())).font(FontId::monospace(11.0)).color(colors.text_title).strong());
            });
        });
        ui.add(egui::Slider::new(&mut state.ltc_volume, 0.0..=1.0).show_value(false));

        ui.add_space(10.0);

        // Beep Volume slider
        ui.horizontal(|ui| {
            ui.label(RichText::new("🎵 BEEP LEVEL (VOLUME)").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{}%", (state.beep_volume * 100.0).round())).font(FontId::monospace(11.0)).color(colors.text_title).strong());
            });
        });
        ui.add(egui::Slider::new(&mut state.beep_volume, 0.0..=1.0).show_value(false));

        ui.add_space(10.0);

        // Beep Frequency slider
        ui.horizontal(|ui| {
            ui.label(RichText::new("⚡ BEEP PITCH (FREQUENCY)").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("{} Hz", state.beep_frequency.round())).font(FontId::monospace(11.0)).color(colors.text_title).strong());
            });
        });
        ui.add(egui::Slider::new(&mut state.beep_frequency, 400.0..=2000.0).show_value(false));
    });
}
