use egui::{Color32, FontId, RichText, Ui, Vec2, Sense};
use gui_engine::command::GuiCommand;

use crate::app::AppState;
use crate::theme::ACCENT;

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let width = ui.available_width();
    if width > 600.0 {
        ui.columns(2, |cols| {
            render_slate_card(&mut cols[0], state);
            render_logs_card(&mut cols[1], state);
        });
    } else {
        ui.vertical(|ui| {
            render_slate_card(ui, state);
            ui.add_space(12.0);
            render_logs_card(ui, state);
        });
    }
}

fn render_slate_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let frame = egui::Frame::group(ui.style())
        .fill(colors.card_bg)
        .corner_radius(12.0)
        .stroke(egui::Stroke::new(1.5, colors.border_main))
        .inner_margin(egui::Margin::same(16));
    frame.show(ui, |ui| {
        let mut auto_inc = state.latest.auto_increment_take;
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("SMART CLAPPER SLATE").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.checkbox(&mut auto_inc, "Auto-Increment Take").changed() {
                        state.send(GuiCommand::SetAutoIncrement(auto_inc));
                    }
                });
            });
            ui.add_space(8.0);
            render_clapper_board_drawing(ui, state);
            ui.add_space(10.0);
            let cards_w = ui.available_width();
            if cards_w > 280.0 {
                ui.columns(3, |cols| {
                    render_roll_card(&mut cols[0], state);
                    render_scene_card(&mut cols[1], state);
                    render_take_card(&mut cols[2], state);
                });
            } else {
                ui.vertical(|ui| {
                    render_roll_card(ui, state);
                    ui.add_space(6.0);
                    render_scene_card(ui, state);
                    ui.add_space(6.0);
                    render_take_card(ui, state);
                });
            }
            ui.add_space(12.0);
            let btn = egui::Button::new(RichText::new("CLAP & BEEP").strong().color(Color32::BLACK))
                .fill(ACCENT)
                .min_size(egui::vec2(ui.available_width(), 44.0));
            if ui.add(btn).clicked() && !state.latest.is_locked {
                state.send(GuiCommand::Clap);
            }
        });
    });
}

fn render_clapper_board_drawing(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = &state.latest;
    let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 100.0), Sense::click());
    if response.clicked() && !s.is_locked {
        state.send(GuiCommand::Clap);
    }
    ui.painter().rect_filled(rect, 8.0, colors.deep_bg);
    ui.painter().rect_stroke(rect, 8.0, egui::Stroke::new(1.0, colors.border_main), egui::StrokeKind::Inside);

    let w = rect.width().min(260.0);
    let h = 20.0;
    let x_offset = rect.left() + (rect.width() - w) / 2.0;
    let y_base = rect.bottom() - 36.0;
    let base_rect = egui::Rect::from_min_size(egui::pos2(x_offset, y_base), egui::vec2(w, h));
    ui.painter().rect_filled(base_rect, 0.0, colors.deep_bg);
    ui.painter().rect_stroke(base_rect, 0.0, egui::Stroke::new(1.0, colors.border_main), egui::StrokeKind::Inside);

    let num_stripes = 6;
    let step = w / num_stripes as f32;
    let skew = h * 0.7;
    let clamp_base_x = |x: f32| x.clamp(x_offset, x_offset + w);
    for i in 0..num_stripes {
        if i % 2 == 1 {
            let x_top_start = i as f32 * step;
            let x_top_end = (i + 1) as f32 * step;
            let x_bot_start = i as f32 * step - skew;
            let x_bot_end = (i + 1) as f32 * step - skew;
            let v1 = egui::pos2(clamp_base_x(x_offset + x_top_start), y_base);
            let v2 = egui::pos2(clamp_base_x(x_offset + x_top_end), y_base);
            let v3 = egui::pos2(clamp_base_x(x_offset + x_bot_end), y_base + h);
            let v4 = egui::pos2(clamp_base_x(x_offset + x_bot_start), y_base + h);
            ui.painter().add(egui::Shape::convex_polygon(vec![v1, v2, v3, v4], ACCENT, egui::Stroke::NONE));
        }
    }

    let pivot = egui::pos2(x_offset + 0.1 * w, y_base);
    let angle = s.clap_arm_angle;
    let cos = angle.cos();
    let sin = angle.sin();
    let rotate = |dx: f32, dy: f32| -> egui::Pos2 {
        egui::pos2(pivot.x + dx * cos - dy * sin, pivot.y + dx * sin + dy * cos)
    };
    let c1 = rotate(-0.1 * w, -h);
    let c2 = rotate(0.9 * w, -h);
    let c3 = rotate(0.9 * w, 0.0);
    let c4 = rotate(-0.1 * w, 0.0);
    ui.painter().add(egui::Shape::convex_polygon(vec![c1, c2, c3, c4], colors.deep_bg, egui::Stroke::new(1.0, colors.border_main)));

    let clamp_arm_x = |x: f32| x.clamp(-0.1 * w, 0.9 * w);
    for i in 0..num_stripes {
        if i % 2 == 1 {
            let x_top_start = -0.1 * w + i as f32 * step;
            let x_top_end = -0.1 * w + (i + 1) as f32 * step;
            let x_bot_start = -0.1 * w + i as f32 * step - skew;
            let x_bot_end = -0.1 * w + (i + 1) as f32 * step - skew;
            let v1 = rotate(clamp_arm_x(x_top_start), -h);
            let v2 = rotate(clamp_arm_x(x_top_end), -h);
            let v3 = rotate(clamp_arm_x(x_bot_end), 0.0);
            let v4 = rotate(clamp_arm_x(x_bot_start), 0.0);
            ui.painter().add(egui::Shape::convex_polygon(vec![v1, v2, v3, v4], ACCENT, egui::Stroke::NONE));
        }
    }
}

fn render_roll_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = &state.latest;
    let card = egui::Frame::new()
        .fill(colors.nested_bg)
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .inner_margin(egui::Margin::symmetric(12, 10));
    card.show(ui, |ui| {
        ui.set_min_height(76.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("ROLL").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            ui.add_space(4.0);
            let mut roll = s.roll.clone();
            if ui.add(egui::TextEdit::singleline(&mut roll).font(FontId::monospace(14.0)).text_color(colors.text_title).margin(egui::Margin::symmetric(4, 4))).lost_focus() {
                state.send(GuiCommand::SetRoll(roll));
            }
        });
    });
}

fn render_scene_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = &state.latest;
    let card = egui::Frame::new()
        .fill(colors.nested_bg)
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .inner_margin(egui::Margin::symmetric(10, 8));
    card.show(ui, |ui| {
        ui.set_min_height(76.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("SCENE").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            ui.label(RichText::new(format!("{}", s.scene)).font(FontId::monospace(18.0)).color(colors.text_title).strong());
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 0.0);
                ui.add_space((ui.available_width() - 44.0) / 2.0);
                if ui.button(RichText::new("-").strong()).clicked() {
                    state.send(GuiCommand::SceneDown);
                }
                if ui.button(RichText::new("+").strong()).clicked() {
                    state.send(GuiCommand::SceneUp);
                }
            });
        });
    });
}

fn render_take_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = &state.latest;
    let card = egui::Frame::new()
        .fill(colors.nested_bg)
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .inner_margin(egui::Margin::symmetric(10, 8));
    card.show(ui, |ui| {
        ui.set_min_height(76.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new("TAKE").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            ui.label(RichText::new(format!("{}", s.take)).font(FontId::monospace(18.0)).color(ACCENT).strong());
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 0.0);
                ui.add_space((ui.available_width() - 44.0) / 2.0);
                if ui.button(RichText::new("-").strong()).clicked() {
                    state.send(GuiCommand::TakeDown);
                }
                if ui.button(RichText::new("+").strong()).clicked() {
                    state.send(GuiCommand::TakeUp);
                }
            });
        });
    });
}

fn render_logs_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = &state.latest;
    let frame = egui::Frame::group(ui.style())
        .fill(colors.card_bg)
        .corner_radius(12.0)
        .stroke(egui::Stroke::new(1.5, colors.border_main))
        .inner_margin(egui::Margin::same(16));
    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("SYNCHRONIZATION LOGS").font(FontId::proportional(11.0)).color(colors.text_muted).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Clear").clicked() {
                        // Clear is GUI-only (just clear the local view)
                    }
                    if ui.button("Copy").clicked() {
                        let text = s.logs.iter()
                            .map(|l| format!("[{}] LTC: {} | MS: {} | {}", l.timestamp, l.timecode, l.milliseconds, l.note))
                            .collect::<Vec<_>>().join("\n");
                        ui.ctx().copy_text(text);
                    }
                });
            });
            ui.add_space(8.0);
            let logs_frame = egui::Frame::new()
                .fill(colors.deep_bg)
                .corner_radius(8.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::same(10));
            logs_frame.show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(200.0).min_scrolled_height(176.0).show(ui, |ui| {
                    if s.logs.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(40.0);
                            ui.label(RichText::new("No clapper marks recorded yet.").color(colors.text_muted).strong());
                            ui.label(RichText::new("Tap CLAP & BEEP to capture markings.").font(FontId::proportional(10.0)).color(colors.text_muted));
                        });
                    } else {
                        for log in s.logs.iter() {
                            let item_frame = egui::Frame::new()
                                .fill(colors.nested_bg)
                                .corner_radius(6.0)
                                .stroke(egui::Stroke::new(1.0, colors.border_main))
                                .inner_margin(egui::Margin::same(8));
                            item_frame.show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new(&log.note).strong().color(ACCENT));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(RichText::new(&log.timestamp).font(FontId::proportional(10.0)).color(colors.text_muted));
                                        });
                                    });
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("LTC Timecode:").color(colors.text_muted).font(FontId::proportional(10.5)));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(RichText::new(&log.timecode).strong().color(colors.text_title));
                                        });
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(RichText::new("Milliseconds:").color(colors.text_muted).font(FontId::proportional(10.5)));
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(RichText::new(&log.milliseconds).strong().color(ACCENT));
                                        });
                                    });
                                });
                            });
                            ui.add_space(4.0);
                        }
                    }
                });
            });
        });
    });
}