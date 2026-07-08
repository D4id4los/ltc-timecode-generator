use egui::{Color32, FontId, RichText, Ui, Vec2};

use crate::app::AppState;

/// Render the clapper slate tab: animated slate, scene/take/roll fields, sync log.
pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    ui.vertical(|ui| {
        render_slate_board(ui, state);
        ui.add_space(8.0);
        render_scene_take_roll(ui, state);
        ui.add_space(8.0);
        render_clap_button(ui, state);
        ui.add_space(8.0);
        render_sync_log(ui, state, &colors);
    });
}

fn render_slate_board(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let board_bg = Color32::from_rgb(0x14, 0x14, 0x14);

    let frame = egui::Frame::new()
        .fill(board_bg)
        .corner_radius(6.0)
        .stroke(egui::Stroke::new(2.0, colors.border_main))
        .inner_margin(egui::Margin::same(16));

    let available = ui.available_width();
    frame.show(ui, |ui| {
        ui.set_min_width(available - 32.0);
        ui.vertical(|ui| {
            // Clapper arm bar at top
            let arm_height = 20.0f32;
            let (arm_rect, _) = ui.allocate_exact_size(
                Vec2::new(available - 32.0, arm_height * 2.0),
                egui::Sense::hover(),
            );

            let arm_pivot = arm_rect.left_top() + egui::vec2(0.0, arm_height);
            let arm_len = arm_rect.width();
            let angle = state.clap_arm_angle * std::f32::consts::PI / 180.0;
            let arm_end = arm_pivot + egui::vec2(arm_len * angle.cos(), arm_len * angle.sin());

            // Diagonal stripes on the arm
            ui.painter().line_segment(
                [arm_pivot, arm_end],
                egui::Stroke::new(8.0, Color32::from_rgb(0xFF, 0xFF, 0xFF)),
            );

            // Board text
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new("PRODUCTION")
                        .font(FontId::proportional(12.0))
                        .color(colors.text_muted),
                );
            });

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("ROLL: {}", if state.roll.is_empty() { "—" } else { &state.roll }))
                        .font(FontId::proportional(14.0))
                        .color(colors.text_main),
                );
                ui.label(
                    RichText::new(format!("SCENE: {}", state.scene))
                        .font(FontId::proportional(14.0))
                        .color(colors.text_main),
                );
                ui.label(
                    RichText::new(format!("TAKE: {}", state.take))
                        .font(FontId::proportional(14.0))
                        .color(colors.text_main),
                );
            });

            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} FPS", state.fps().name))
                        .font(FontId::proportional(12.0))
                        .color(colors.text_muted),
                );
            });
        });
    });
}

fn render_scene_take_roll(ui: &mut Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        // Scene stepper
        ui.vertical(|ui| {
            ui.label("Scene");
            ui.horizontal(|ui| {
                if ui.button("▲").clicked() {
                    state.scene = state.scene.saturating_add(1);
                }
                if ui.button("▼").clicked() {
                    state.scene = state.scene.saturating_sub(1);
                }
            });
        });

        // Take stepper
        ui.vertical(|ui| {
            ui.label("Take");
            ui.horizontal(|ui| {
                if ui.button("▲").clicked() {
                    state.take = state.take.saturating_add(1);
                }
                if ui.button("▼").clicked() {
                    state.take = state.take.saturating_sub(1);
                }
            });
        });

        // Roll text input
        ui.vertical(|ui| {
            ui.label("Roll");
            ui.text_edit_singleline(&mut state.roll);
        });

        // Auto-increment checkbox
        ui.checkbox(&mut state.auto_increment_take, "Auto-Increment Take");
    });
}

fn render_clap_button(ui: &mut Ui, state: &mut AppState) {
    let available = ui.available_width();
    let btn = egui::Button::new("🎬 CLAP & BEEP")
        .min_size(Vec2::new(available, 40.0))
        .fill(crate::theme::ACCENT.linear_multiply(0.25));
    if ui.add(btn).clicked() {
        state.trigger_clap();
    }
}

fn render_sync_log(ui: &mut Ui, state: &mut AppState, colors: &crate::theme::ThemeColors) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Synchronization Logs")
                .font(FontId::proportional(14.0))
                .color(colors.text_title),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("Clear").clicked() {
                state.logs.clear();
            }
            if ui.button("Copy").clicked() {
                let text = state
                    .logs
                    .iter()
                    .map(|l| format!("{} | {} | {}", l.timestamp, l.timecode, l.note))
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.ctx().copy_text(text);
            }
        });
    });

    egui::ScrollArea::vertical()
        .max_height(200.0)
        .show(ui, |ui| {
            if state.logs.is_empty() {
                ui.label(
                    RichText::new("No sync events logged yet.")
                        .color(colors.text_muted),
                );
            } else {
                for (i, log) in state.logs.iter().enumerate() {
                    ui.horizontal(|ui| {
                        ui.label(
                            RichText::new(format!("#{}", i + 1))
                                .color(colors.text_muted),
                        );
                        ui.label(&log.timecode);
                        ui.label(&log.note);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.label(
                                    RichText::new(&log.timestamp)
                                        .color(colors.text_muted),
                                );
                            },
                        );
                    });
                }
            }
        });
}
