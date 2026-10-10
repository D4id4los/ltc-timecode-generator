use crate::text::text;
use egui::{Sense, Ui, Vec2};
use gui_engine::command::GuiCommand;
use std::time::Instant;

use super::bound;
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
    super::style::content_area(ui, &colors, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                text(ui, "SMART CLAPPER SLATE")
                    .size(11.0)
                    .color(colors.text_muted)
                    .bold(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let truth = state.latest.clapper.auto_increment_take;
                bound::checkbox(
                    ui,
                    state,
                    |s| &mut s.sh.auto_increment,
                    truth,
                    "Auto-Increment Take",
                    true,
                    GuiCommand::SetAutoIncrement,
                );
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
        if super::style::action_button(
            ui,
            &colors,
            super::style::ActionStyle::Primary,
            "CLAP & BEEP",
            crate::text::font(13.0),
            egui::vec2(ui.available_width(), 44.0),
            true,
        )
        .clicked()
            && !state.latest.is_locked
        {
            state.send(GuiCommand::Clap);
        }
    });
}

fn render_clapper_board_drawing(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = std::sync::Arc::clone(&state.latest);
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 100.0), Sense::click());
    let response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() && !s.is_locked {
        state.send(GuiCommand::Clap);
    }
    ui.painter().rect_filled(
        rect,
        8.0,
        super::style::hover_fill(colors.deep_bg, response.hovered()),
    );
    ui.painter().rect_stroke(
        rect,
        8.0,
        egui::Stroke::new(
            if response.hovered() { 1.5 } else { 1.0 },
            if response.hovered() {
                ACCENT.linear_multiply(0.7)
            } else {
                colors.border_main
            },
        ),
        egui::StrokeKind::Inside,
    );

    let w = rect.width().min(260.0);
    let h = 20.0;
    let x_offset = rect.left() + (rect.width() - w) / 2.0;
    let y_base = rect.bottom() - 36.0;
    let base_rect = egui::Rect::from_min_size(egui::pos2(x_offset, y_base), egui::vec2(w, h));
    ui.painter().rect_filled(base_rect, 0.0, colors.deep_bg);
    ui.painter().rect_stroke(
        base_rect,
        0.0,
        egui::Stroke::new(1.0, colors.border_main),
        egui::StrokeKind::Inside,
    );

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
            ui.painter().add(egui::Shape::convex_polygon(
                vec![v1, v2, v3, v4],
                ACCENT,
                egui::Stroke::NONE,
            ));
        }
    }

    let pivot = egui::pos2(x_offset + 0.1 * w, y_base);
    // GUI-local animation: sample the local animator's arm curve (the
    // snapshot no longer carries a continuous arm angle). No animator =
    // arm at rest.
    let angle = state
        .clap_anim
        .map(|a| crate::clap_anim::arm_angle_at(a.elapsed(Instant::now())))
        .unwrap_or(crate::clap_anim::TARGET_ARM_ANGLE);
    let cos = angle.cos();
    let sin = angle.sin();
    let rotate = |dx: f32, dy: f32| -> egui::Pos2 {
        egui::pos2(pivot.x + dx * cos - dy * sin, pivot.y + dx * sin + dy * cos)
    };
    let c1 = rotate(-0.1 * w, -h);
    let c2 = rotate(0.9 * w, -h);
    let c3 = rotate(0.9 * w, 0.0);
    let c4 = rotate(-0.1 * w, 0.0);
    ui.painter().add(egui::Shape::convex_polygon(
        vec![c1, c2, c3, c4],
        colors.deep_bg,
        egui::Stroke::new(1.0, colors.border_main),
    ));

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
            ui.painter().add(egui::Shape::convex_polygon(
                vec![v1, v2, v3, v4],
                ACCENT,
                egui::Stroke::NONE,
            ));
        }
    }
}

fn render_roll_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let card = egui::Frame::new()
        .fill(colors.nested_bg)
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .inner_margin(egui::Margin::symmetric(12, 10));
    card.show(ui, |ui| {
        ui.set_min_height(76.0);
        ui.vertical_centered(|ui| {
            ui.label(text(ui, "ROLL").size(9.0).color(colors.text_muted).bold());
            ui.add_space(4.0);
            let truth = state.latest.clapper.roll.clone();
            bound::text(
                ui,
                state,
                |s| &mut s.sh.roll,
                &truth,
                GuiCommand::SetRoll,
                |edit| {
                    edit.font(crate::text::font(14.0).mono().font_id())
                        .text_color(colors.text_title)
                        .margin(egui::Margin::symmetric(4, 4))
                },
            );
        });
    });
}

fn render_scene_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = std::sync::Arc::clone(&state.latest);
    let card = egui::Frame::new()
        .fill(colors.nested_bg)
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .inner_margin(egui::Margin::symmetric(10, 8));
    card.show(ui, |ui| {
        ui.set_min_height(76.0);
        ui.vertical_centered(|ui| {
            ui.label(text(ui, "SCENE").size(9.0).color(colors.text_muted).bold());
            ui.label(
                text(ui, format!("{}", s.clapper.scene))
                    .mono()
                    .size(18.0)
                    .color(colors.text_title)
                    .bold(),
            );
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 0.0);
                ui.add_space((ui.available_width() - 44.0) / 2.0);
                if ui
                    .button(
                        text(ui, "-")
                            .bold()
                            .color(ui.style().visuals.strong_text_color()),
                    )
                    .clicked()
                {
                    state.send(GuiCommand::SceneDown);
                }
                if ui
                    .button(
                        text(ui, "+")
                            .bold()
                            .color(ui.style().visuals.strong_text_color()),
                    )
                    .clicked()
                {
                    state.send(GuiCommand::SceneUp);
                }
            });
        });
    });
}

fn render_take_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = std::sync::Arc::clone(&state.latest);
    let card = egui::Frame::new()
        .fill(colors.nested_bg)
        .corner_radius(8.0)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .inner_margin(egui::Margin::symmetric(10, 8));
    card.show(ui, |ui| {
        ui.set_min_height(76.0);
        ui.vertical_centered(|ui| {
            ui.label(text(ui, "TAKE").size(9.0).color(colors.text_muted).bold());
            ui.label(
                text(ui, format!("{}", s.clapper.take))
                    .mono()
                    .size(18.0)
                    .color(ACCENT)
                    .bold(),
            );
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 0.0);
                ui.add_space((ui.available_width() - 44.0) / 2.0);
                if ui
                    .button(
                        text(ui, "-")
                            .bold()
                            .color(ui.style().visuals.strong_text_color()),
                    )
                    .clicked()
                {
                    state.send(GuiCommand::TakeDown);
                }
                if ui
                    .button(
                        text(ui, "+")
                            .bold()
                            .color(ui.style().visuals.strong_text_color()),
                    )
                    .clicked()
                {
                    state.send(GuiCommand::TakeUp);
                }
            });
        });
    });
}

fn render_logs_card(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = std::sync::Arc::clone(&state.latest);
    super::style::content_area(ui, &colors, |ui| {
        ui.horizontal(|ui| {
            ui.label(
                text(ui, "SYNCHRONIZATION LOGS")
                    .size(11.0)
                    .color(colors.text_muted)
                    .bold(),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Clear").clicked() {
                    // Clear is GUI-only (just clear the local view)
                }
                if ui.button("Copy").clicked() {
                    let text = s
                        .clapper
                        .logs
                        .iter()
                        .map(|l| {
                            format!(
                                "[{}] LTC: {} | MS: {} | {}",
                                l.timestamp, l.timecode, l.milliseconds, l.note
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
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
            egui::ScrollArea::vertical()
                .id_salt(crate::ids::clap_log_scroll())
                .max_height(200.0)
                .min_scrolled_height(176.0)
                .show(ui, |ui| {
                    if s.clapper.logs.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.add_space(40.0);
                            ui.label(
                                text(ui, "No clapper marks recorded yet.")
                                    .color(colors.text_muted)
                                    .bold(),
                            );
                            ui.label(
                                text(ui, "Tap CLAP & BEEP to capture markings.")
                                    .size(10.0)
                                    .color(colors.text_muted),
                            );
                        });
                    } else {
                        for log in s.clapper.logs.iter() {
                            let item_frame = egui::Frame::new()
                                .fill(colors.nested_bg)
                                .corner_radius(6.0)
                                .stroke(egui::Stroke::new(1.0, colors.border_main))
                                .inner_margin(egui::Margin::same(8));
                            item_frame.show(ui, |ui| {
                                ui.vertical(|ui| {
                                    ui.horizontal(|ui| {
                                        ui.label(text(ui, &log.note).bold().color(ACCENT));
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    text(ui, &log.timestamp)
                                                        .size(10.0)
                                                        .color(colors.text_muted),
                                                );
                                            },
                                        );
                                    });
                                    ui.add_space(4.0);
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            text(ui, "LTC Timecode:")
                                                .color(colors.text_muted)
                                                .size(10.5),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    text(ui, &log.timecode)
                                                        .bold()
                                                        .color(colors.text_title),
                                                );
                                            },
                                        );
                                    });
                                    ui.horizontal(|ui| {
                                        ui.label(
                                            text(ui, "Milliseconds:")
                                                .color(colors.text_muted)
                                                .size(10.5),
                                        );
                                        ui.with_layout(
                                            egui::Layout::right_to_left(egui::Align::Center),
                                            |ui| {
                                                ui.label(
                                                    text(ui, &log.milliseconds)
                                                        .bold()
                                                        .color(ACCENT),
                                                );
                                            },
                                        );
                                    });
                                });
                            });
                            ui.add_space(4.0);
                        }
                    }
                });
        });
    });
}
