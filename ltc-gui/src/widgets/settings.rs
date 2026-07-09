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
        ui.vertical_centered(|ui| {
            // Up Button
            let up_btn = egui::Button::new(RichText::new("^").strong())
                .fill(Color32::TRANSPARENT);
            if ui.add(up_btn).clicked() {
                *value = (*value + 1) % max;
            }

            ui.add_space(1.0);
            
            // Value
            ui.label(
                RichText::new(format!("{:02}", *value))
                    .font(FontId::monospace(20.0))
                    .color(colors.text_title)
                    .strong(),
            );
            
            ui.add_space(1.0);

            // Label
            ui.label(
                RichText::new(label)
                    .font(FontId::proportional(7.5))
                    .color(colors.text_muted)
                    .strong(),
            );

            ui.add_space(1.0);

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
            // Flow layout with safe fallback for very narrow windows
            let usable = ((width - 32.0) / 2.0).max(1.0);
            if usable < 90.0 || width < 260.0 {
                // Single column for very narrow windows
                for (i, opt) in FRAME_RATE_OPTIONS.iter().enumerate() {
                    frame_rate_card(ui, i, opt, state);
                    ui.add_space(6.0);
                }
            } else {
                // Two-column flow
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 8.0);
                    for (i, opt) in FRAME_RATE_OPTIONS.iter().enumerate() {
                        ui.scope(|ui| {
                            ui.set_max_width(usable);
                            frame_rate_card(ui, i, opt, state);
                        });
                    }
                });
            }
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

    let pad = ((width as i32 / 50).max(4)) as f32;

    let container = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same((pad as i32).min(i8::MAX as i32) as i8));

    const SPACE_TIGHT: f32 = 6.0;
    const SPACE_NORM: f32 = 10.0;

    container.show(ui, |ui| {
        if width > 500.0 {
            ui.columns(2, |cols| {
                cols[0].vertical(|ui| {
                    render_routing_buttons(ui, state, SPACE_TIGHT, SPACE_NORM);
                });
                cols[1].vertical(|ui| {
                    render_sliders(ui, state, SPACE_TIGHT);
                });
            });
        } else {
            render_routing_buttons(ui, state, SPACE_TIGHT, SPACE_NORM);
            ui.add_space(SPACE_NORM);
            render_sliders(ui, state, SPACE_TIGHT);
        }
    });

    ui.add_space(8.0);

    let tip_pad = (width as i32 / 120).max(4) as f32;
    let tip_frame = egui::Frame::new()
        .fill(colors.card_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::same((tip_pad as i32).min(i8::MAX as i32) as i8));

    tip_frame.show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(8.0, 8.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.0, ACCENT);
            ui.label(
                RichText::new("LTC Right, Clapper Left into camera mic — Channel 1 Resolve, Channel 2 scratch.")
                    .font(FontId::proportional(8.5))
                    .color(colors.text_muted)
            );
        });
    });
}

fn render_routing_buttons(ui: &mut Ui, state: &mut AppState, space_tight: f32, space_norm: f32) {
    let colors = state.theme.colors();
    let width = ui.available_width();
    let pills_horizontal = width > 300.0;

    render_pill_set(ui, "LTC OUTPUT", AudioChannel::all(), &mut state.ltc_channel, &colors, pills_horizontal, space_tight);
    ui.add_space(space_norm);
    render_pill_set(ui, "CLAPPER OUTPUT", AudioChannel::all(), &mut state.beep_channel, &colors, pills_horizontal, space_tight);
}

fn render_pill_set(
    ui: &mut Ui,
    label: &str,
    channels: &[AudioChannel],
    active_channel: &mut AudioChannel,
    colors: &crate::theme::ThemeColors,
    horizontal: bool,
    space_tight: f32,
) {
    ui.label(RichText::new(label).font(FontId::proportional(9.0)).color(colors.text_muted).strong());
    ui.add_space(space_tight);

    if horizontal {
        ui.horizontal(|ui| {
            for ch in channels {
                let active = *active_channel == *ch;
                let btn = if active {
                    egui::Button::new(RichText::new(ch.label()).strong().color(Color32::BLACK)).fill(ACCENT)
                } else {
                    egui::Button::new(RichText::new(ch.label()))
                        .stroke(egui::Stroke::new(0.5, colors.border_main))
                        .fill(colors.card_bg)
                };
                if ui.add(btn).clicked() { *active_channel = *ch; }
            }
        });
    } else {
        for ch in channels {
            let active = *active_channel == *ch;
            let btn = if active {
                egui::Button::new(RichText::new(ch.label()).strong().color(Color32::BLACK)).fill(ACCENT)
            } else {
                egui::Button::new(RichText::new(ch.label()))
                    .stroke(egui::Stroke::new(0.5, colors.border_main))
                    .fill(colors.card_bg)
            };
            if ui.add(btn).clicked() { *active_channel = *ch; }
            ui.add_space(space_tight);
        }
    }
}

fn render_sliders(ui: &mut Ui, state: &mut AppState, space_tight: f32) {
    let colors = state.theme.colors();

    ui.horizontal(|ui| {
        ui.label(RichText::new("LTC VOL").font(FontId::monospace(9.0)).color(colors.text_muted));
        ui.add(egui::Slider::new(&mut state.ltc_volume, 0.0..=1.0).show_value(false));
        ui.label(RichText::new(format!("{}%", (state.ltc_volume * 100.0).round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    ui.add_space(space_tight);

    ui.horizontal(|ui| {
        ui.label(RichText::new("BEEP VOL").font(FontId::monospace(9.0)).color(colors.text_muted));
        ui.add(egui::Slider::new(&mut state.beep_volume, 0.0..=1.0).show_value(false));
        ui.label(RichText::new(format!("{}%", (state.beep_volume * 100.0).round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });

    ui.add_space(space_tight);

    ui.horizontal(|ui| {
        ui.label(RichText::new("PITCH").font(FontId::monospace(9.0)).color(colors.text_muted));
        ui.add(egui::Slider::new(&mut state.beep_frequency, 400.0..=2000.0).show_value(false));
        ui.label(RichText::new(format!("{} Hz", state.beep_frequency.round())).font(FontId::monospace(10.0)).color(colors.text_title));
    });
}

// Stub functions retained for compatibility (no longer invoked from render())
fn _render_routing_column(_ui: &mut Ui, _state: &mut AppState) {}
fn _render_volume_column(_ui: &mut Ui, _state: &mut AppState) {}
