use std::path::PathBuf;
use std::sync::atomic::Ordering;

use egui::{Color32, FontId, RichText, Ui};
use gui_engine::timecode::FPS_OPTIONS;
use gui_engine::command::GuiCommand;
use gui_engine::converter::{
    available_audio_encoders_for_container, available_containers,
    available_video_encoders_for_container, conversion_sanity_check,
    query_ffmpeg_capabilities, select_best_combination, spawn_conversion,
    supported_audio_encoders, supported_containers, supported_video_encoders, ChannelMap,
    ConversionState, ConversionStatus, ConverterSettings, FfmpegCapabilities,
};
use gui_engine::file_pattern::{default_output_filename, match_files_to_groups, wrap_user_selected_files, BUILTIN_PATTERNS};

use crate::app::AppState;
use crate::theme::ACCENT;

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    let frame = egui::Frame::group(ui.style())
        .fill(colors.card_bg)
        .corner_radius(12.0)
        .stroke(egui::Stroke::new(1.5, colors.border_main))
        .inner_margin(egui::Margin::same(16));
    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            step_header(ui, "1", "SELECT FILES", &colors);
            ui.add_space(8.0);
            render_file_selection(ui, state);
            ui.add_space(16.0);

            if state.selected_group.is_some() {
                step_header(ui, "L", "VERIFY LTC TRACK", &colors);
                ui.add_space(8.0);
                render_ltc_verification(ui, state);
                ui.add_space(16.0);

                step_header(ui, "2", "CHANNEL MAPPING", &colors);
                ui.add_space(8.0);
                render_channel_matrix(ui, state);
                ui.add_space(16.0);
            }

            step_header(ui, "3", "OUTPUT FORMAT", &colors);
            ui.add_space(8.0);
            render_output_format(ui, state);
            ui.add_space(16.0);

            step_header(ui, "4", "OUTPUT FILE", &colors);
            ui.add_space(8.0);
            render_output_path(ui, state);
            ui.add_space(16.0);

            render_convert_button(ui, state);
            ui.add_space(12.0);

            render_conversion_progress(ui, state);
        });
    });
}

fn step_header(ui: &mut Ui, number: &str, label: &str, colors: &crate::theme::ThemeColors) {
    ui.horizontal(|ui| {
        let badge = egui::Frame::new()
            .fill(ACCENT)
            .corner_radius(4.0)
            .inner_margin(egui::Margin::symmetric(6, 2));
        badge.show(ui, |ui| {
            ui.label(RichText::new(number).font(FontId::monospace(11.0)).color(Color32::BLACK).strong());
        });
        ui.label(RichText::new(label).font(FontId::proportional(12.0)).color(colors.text_title).strong());
    });
}

// ── Step 1: File selection ─────────────────────────────────────────────

fn render_file_selection(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    // Pattern selector
    ui.horizontal(|ui| {
        ui.label(RichText::new("Naming pattern:").font(FontId::proportional(10.0)).color(colors.text_muted));
        egui::ComboBox::from_id_salt("pattern_combo")
            .selected_text(BUILTIN_PATTERNS[state.selected_pattern].name)
            .show_ui(ui, |ui| {
                for (i, p) in BUILTIN_PATTERNS.iter().enumerate() {
                    if ui.selectable_label(false, format!("{} — {}", p.name, p.description)).clicked() {
                        state.selected_pattern = i;
                        state.selected_group = None;
                        state.file_groups = None;
                        state.selected_files = None;
                    }
                }
            });
    });

    if ui.available_width() > 100.0 {
        ui.label(RichText::new(BUILTIN_PATTERNS[state.selected_pattern].description).font(FontId::proportional(9.0)).color(colors.text_secondary));
        ui.add_space(4.0);
    }

    if state.selected_pattern == 0 {
        // ── TASCAM: folder picker ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("Folder:").font(FontId::proportional(10.0)).color(colors.text_muted));
            let folder_label = match &state.selected_folder {
                Some(p) => p.to_string_lossy().to_string(),
                None => String::from("(No folder selected)"),
            };
            let mut display = folder_label;
            ui.add_sized(
                egui::vec2(ui.available_width() - 90.0, 20.0),
                egui::TextEdit::singleline(&mut display)
                    .font(FontId::monospace(10.0))
                    .interactive(false),
            );
            if ui.button("Browse…").clicked() {
                let folder = rfd::FileDialog::new().pick_folder();
                if let Some(path) = folder {
                    state.selected_folder = Some(path.clone());
                    state.selected_files = None;
                    let pattern = &BUILTIN_PATTERNS[state.selected_pattern];
                    state.file_groups = Some(match_files_to_groups(&path, pattern));
                    state.selected_group = None;
                    if state.ffmpeg_caps.is_none() {
                        let caps = query_ffmpeg_capabilities();
                        state.ffmpeg_caps = Some(caps);
                    }
                }
            }
        });
    } else {
        // ── * (any): file picker ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("Files:").font(FontId::proportional(10.0)).color(colors.text_muted));
            let file_label = match &state.selected_files {
                Some(files) => format!("{} file(s) selected", files.len()),
                None => String::from("(No files selected)"),
            };
            let mut display = file_label;
            ui.add_sized(
                egui::vec2(ui.available_width() - 90.0, 20.0),
                egui::TextEdit::singleline(&mut display)
                    .font(FontId::monospace(10.0))
                    .interactive(false),
            );
            if ui.button("Browse…").clicked() {
                let files = rfd::FileDialog::new()
                    .add_filter("Audio", &["*"])
                    .pick_files();
                if let Some(paths) = files {
                    if !paths.is_empty() {
                        state.selected_files = Some(paths.clone());
                        state.selected_folder = paths[0].parent().map(|p| p.to_path_buf());
                        state.file_groups = Some(wrap_user_selected_files(paths));
                        state.selected_group = None;
                        if state.ffmpeg_caps.is_none() {
                            let caps = query_ffmpeg_capabilities();
                            state.ffmpeg_caps = Some(caps);
                        }
                    }
                }
            }
        });
    }

    // File group selector
    if let Some(groups) = &state.file_groups {
        if groups.is_empty() {
            let msg = if state.selected_pattern == 0 {
                "No files matching the TASCAM pattern were found in this folder."
            } else {
                "No files selected."
            };
            ui.label(RichText::new(msg).font(FontId::proportional(10.0)).color(colors.error_red));
        } else {
            let is_any_pattern = state.selected_pattern == 1;
            let group_names: Vec<&String> = groups.keys().collect();
            ui.horizontal(|ui| {
                let label = if is_any_pattern { "Selected files:" } else { "Recording:" };
                ui.label(RichText::new(label).font(FontId::proportional(10.0)).color(colors.text_muted));
                let selected_text = state.selected_group.as_deref().unwrap_or(
                    if is_any_pattern { "Select a group…" } else { "Select a recording…" }
                );
                egui::ComboBox::from_id_salt("group_combo")
                    .selected_text(selected_text)
                    .show_ui(ui, |ui| {
                        for name in &group_names {
                            let files = &groups[name.as_str()];
                            let detail = files
                                .iter()
                                .map(|f| f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let label = if is_any_pattern {
                                format!("{}  ({} file{}: {})", name, files.len(), if files.len() == 1 { "" } else { "s" }, detail)
                            } else {
                                format!("{}  ({} ch: {})", name, files.len(), detail)
                            };
                            if ui.selectable_label(false, label).clicked() {
                                state.selected_group = Some(name.to_string());
                                let num_ch = files.len();
                                state.channel_map = ChannelMap::identity(num_ch);
                                let container = state.container.clone();
                                state.output_path =
                                    state.selected_folder.as_ref().map(|p| p.join(default_output_filename(name, &container))).unwrap_or_else(|| PathBuf::from(default_output_filename(name, &container)));
                            }
                        }
                    });
            });

            if let Some(group_name) = &state.selected_group {
                if let Some(files) = groups.get(group_name) {
                    ui.add_space(4.0);
                    let pill_frame = egui::Frame::new()
                        .fill(colors.deep_bg)
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 4));
                    pill_frame.show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 2.0);
                            for f in files {
                                let name = f.file_name().and_then(|s| s.to_str()).unwrap_or("?");
                                let pill = egui::Frame::new()
                                    .fill(colors.card_bg)
                                    .corner_radius(4.0)
                                    .stroke(egui::Stroke::new(0.5, colors.border_main))
                                    .inner_margin(egui::Margin::symmetric(6, 2));
                                pill.show(ui, |ui| {
                                    ui.label(RichText::new(name).font(FontId::monospace(9.0)).color(colors.text_muted));
                                });
                            }
                        });
                    });
                }
            }
        }
    }
}

// ── LTC Verification ─────────────────────────────────────────────────

fn render_ltc_verification(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    ui.label(
        RichText::new("Select the mono file that carries the LTC timecode signal, then click \"Detect LTC\" to verify it can be read successfully.")
            .font(FontId::proportional(9.0))
            .color(colors.text_secondary),
    );
    ui.add_space(6.0);

    let files: Vec<PathBuf> = state
        .selected_group
        .as_ref()
        .and_then(|g| state.file_groups.as_ref()?.get(g))
        .cloned()
        .unwrap_or_default();

    let file_names: Vec<String> = files
        .iter()
        .map(|f| f.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string())
        .collect();

    if !file_names.is_empty() {
        // Ensure ltc_file_idx is in range
        if state.ltc_file_idx >= file_names.len() {
            state.ltc_file_idx = file_names.len().saturating_sub(1);
        }

        let selected_name = file_names[state.ltc_file_idx].clone();

        // ── Row 1: Track selection dropdown ──
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("ltc_file_combo")
                .selected_text(&selected_name)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    for (i, name) in file_names.iter().enumerate() {
                        if ui.selectable_label(false, name).clicked() {
                            state.ltc_file_idx = i;
                        }
                    }
                });
        });

        ui.add_space(6.0);

        // ── Row 2: Decode FPS selector + Detect button ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("FPS:").font(FontId::proportional(10.0)).color(colors.text_muted));
            for (i, opt) in FPS_OPTIONS.iter().enumerate() {
                let is_sel = i == state.latest.decode_fps_index;
                let btn = egui::Button::new(
                    RichText::new(opt.name).font(FontId::monospace(9.0)).color(if is_sel { Color32::BLACK } else { colors.text_muted })
                )
                .fill(if is_sel { ACCENT } else { colors.deep_bg })
                .min_size(egui::vec2(0.0, 22.0));
                if ui.add(btn).clicked() {
                    state.send(GuiCommand::SetDecodeFpsIndex(i));
                }
            }

            ui.add_space(8.0);

            let is_detecting = state.latest.ltc_is_detecting;
            let button_label = if is_detecting {
                "⏳ Detecting…"
            } else {
                "🔍 Detect LTC"
            };

            let button_enabled = !is_detecting;
            if ui
                .add_enabled(
                    button_enabled,
                    egui::Button::new(RichText::new(button_label).font(FontId::proportional(11.0)).color(Color32::BLACK).strong())
                        .fill(if button_enabled { ACCENT } else { colors.deep_bg })
                        .min_size(egui::vec2(100.0, 24.0)),
                )
                .clicked()
            {
                let file_path = files[state.ltc_file_idx].to_string_lossy().to_string();
                state.send(GuiCommand::ParseLtcFile(file_path));
            }
        });
    }

    ui.add_space(4.0);

    let decode_result = state.latest.ltc_decode_result.clone();
    let decode_error = state.latest.ltc_decode_error.clone();

    if let Some(ref error) = decode_error {
        let error_frame = egui::Frame::new()
            .fill(Color32::from_rgb(0x44, 0x11, 0x11))
            .corner_radius(6.0)
            .stroke(egui::Stroke::new(1.0, colors.error_red))
            .inner_margin(egui::Margin::symmetric(10, 6));
        error_frame.show(ui, |ui| {
            ui.label(
                RichText::new(format!("❌ {}", error))
                    .font(FontId::proportional(10.0))
                    .color(colors.error_red),
            );
        });
    }

    if let Some(result) = decode_result {
        render_ltc_result(ui, state, &result);
    }
}

fn render_ltc_result(ui: &mut Ui, state: &mut AppState, result: &gui_engine::LtcDetectionResult) {
    let colors = state.theme.colors();

    let (status_icon, status_color, status_text) = match &result.status {
        gui_engine::LtcDecodeStatus::Success => ("✅", colors.success_green, "LTC detected successfully"),
        gui_engine::LtcDecodeStatus::LowConfidence => ("⚠️", colors.warning_amber, "LTC detected with low confidence"),
        gui_engine::LtcDecodeStatus::NoSyncWord => ("❌", colors.error_red, "No LTC timecode found"),
        gui_engine::LtcDecodeStatus::Error { message } => {
            let error_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x44, 0x11, 0x11))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.error_red))
                .inner_margin(egui::Margin::symmetric(10, 6));
            error_frame.show(ui, |ui| {
                ui.label(
                    RichText::new(format!("❌ Detection error: {}", message))
                        .font(FontId::proportional(10.0))
                        .color(colors.error_red),
                );
            });
            return;
        }
    };

    let bg_color = status_color.linear_multiply(0.08);
    let border_color = status_color.linear_multiply(0.2);

    let result_frame = egui::Frame::new()
        .fill(bg_color)
        .corner_radius(6.0)
        .stroke(egui::Stroke::new(1.0, border_color))
        .inner_margin(egui::Margin::symmetric(10, 6));
    result_frame.show(ui, |ui| {
        ui.vertical(|ui| {
            ui.label(
                RichText::new(format!("{} {}", status_icon, status_text))
                    .font(FontId::proportional(11.0))
                    .color(status_color)
                    .strong(),
            );
            ui.add_space(4.0);

            let drop_flag = if result.drop_frame { " (Drop Frame)" } else { "" };
            let fps_str = if result.detected_fps > 0.0 {
                format!("{:.2} fps{}", result.detected_fps, drop_flag)
            } else {
                "—".to_string()
            };

            let first_tc = result.timecodes.first();
            let last_tc = result.timecodes.last();
            let tc_summary = match (first_tc, last_tc) {
                (Some(f), Some(l)) => {
                    let sep = if result.drop_frame { ";" } else { ":" };
                    format!(
                        "{:02}{sep}{:02}{sep}{:02}{sep}{:02} → {:02}{sep}{:02}{sep}{:02}{sep}{:02}",
                        f.timecode.hours, f.timecode.minutes, f.timecode.seconds, f.timecode.frames,
                        l.timecode.hours, l.timecode.minutes, l.timecode.seconds, l.timecode.frames,
                    )
                }
                _ => "—".to_string(),
            };

            let grid = egui::Grid::new("ltc_result_grid")
                .num_columns(2)
                .spacing([8.0, 2.0])
                .striped(false);
            grid.show(ui, |ui| {
                ui.label(RichText::new("Detected rate:").font(FontId::proportional(10.0)).color(colors.text_muted));
                ui.label(RichText::new(fps_str).font(FontId::monospace(10.0)).color(colors.text_title).strong());
                ui.end_row();

                ui.label(RichText::new("Confidence:").font(FontId::proportional(10.0)).color(colors.text_muted));
                ui.label(
                    RichText::new(format!("{:.1}%", result.avg_confidence * 100.0))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_title)
                        .strong(),
                );
                ui.end_row();

                ui.label(RichText::new("Valid frames:").font(FontId::proportional(10.0)).color(colors.text_muted));
                ui.label(
                    RichText::new(format!("{} / {}", result.valid_frames, result.total_possible_frames))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_title)
                        .strong(),
                );
                ui.end_row();

                ui.label(RichText::new("Timecode range:").font(FontId::proportional(10.0)).color(colors.text_muted));
                ui.label(RichText::new(&tc_summary).font(FontId::monospace(10.0)).color(colors.text_title).strong());
                ui.end_row();

                ui.label(RichText::new("Sample rate:").font(FontId::proportional(10.0)).color(colors.text_muted));
                ui.label(
                    RichText::new(format!("{} Hz", result.sample_rate))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_title)
                        .strong(),
                );
                ui.end_row();

                ui.label(RichText::new("Audio duration:").font(FontId::proportional(10.0)).color(colors.text_muted));
                ui.label(
                    RichText::new(format!("{:.2}s", result.total_audio_duration_secs))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_title)
                        .strong(),
                );
                ui.end_row();

                ui.label(RichText::new("Processing time:").font(FontId::proportional(10.0)).color(colors.text_muted));
                ui.label(
                    RichText::new(format!("{:.1} ms", result.processing_time_ms))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_title)
                        .strong(),
                );
                ui.end_row();
            });

            // Collapsible timecode list
            if !result.timecodes.is_empty() {
                ui.add_space(4.0);
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    egui::Id::new("ltc_timecode_list"),
                    false,
                )
                .show_header(ui, |ui| {
                    ui.label(
                        RichText::new(format!("Show {} decoded timecodes", result.timecodes.len()))
                            .font(FontId::proportional(9.0))
                            .color(colors.text_muted),
                    );
                })
                .body(|ui| {
                    let scroll_frame = egui::Frame::new()
                        .fill(colors.deep_bg)
                        .corner_radius(4.0)
                        .stroke(egui::Stroke::new(0.5, colors.border_main))
                        .inner_margin(egui::Margin::symmetric(6, 4));
                    scroll_frame.show(ui, |ui| {
                        egui::ScrollArea::vertical()
                            .max_height(120.0)
                            .show(ui, |ui| {
                                for ftc in &result.timecodes {
                                    let sep = if result.drop_frame { ";" } else { ":" };
                                    ui.label(
                                        RichText::new(format!(
                                            "[{:4}] {:02}{sep}{:02}{sep}{:02}{sep}{:02}  (+{:.3}s)",
                                            ftc.frame_index,
                                            ftc.timecode.hours, ftc.timecode.minutes,
                                            ftc.timecode.seconds, ftc.timecode.frames,
                                            ftc.timecode_secs,
                                        ))
                                        .font(FontId::monospace(9.0))
                                        .color(colors.text_muted),
                                    );
                                }
                            });
                    });
                });
            }

            // Collapsible debug details
            if !result.details.is_empty() {
                ui.add_space(2.0);
                egui::collapsing_header::CollapsingState::load_with_default_open(
                    ui.ctx(),
                    egui::Id::new("ltc_debug_details"),
                    false,
                )
                .show_header(ui, |ui| {
                    ui.label(
                        RichText::new("Show debug details")
                            .font(FontId::proportional(9.0))
                            .color(colors.text_muted),
                    );
                })
                .body(|ui| {
                    for detail in &result.details {
                        ui.label(
                            RichText::new(detail)
                                .font(FontId::monospace(8.0))
                                .color(colors.text_muted),
                        );
                    }
                });
            }
        });
    });
}

// ── Step 2: Channel mapping matrix ──────────────────────────────────────

fn render_channel_matrix(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let n = state.channel_map.num_channels();

    if n == 0 {
        ui.label(RichText::new("No channels to map.").font(FontId::proportional(10.0)).color(colors.text_muted));
        return;
    }

    ui.label(RichText::new("Click a radio button to swap the input channel (row) with the channel currently mapped to the selected output (column).").font(FontId::proportional(9.0)).color(colors.text_secondary));
    ui.add_space(6.0);

    let cell_size = 36.0;
    let label_width = 40.0;
    let header_height = 24.0;
    let total_width = label_width + cell_size * n as f32 + 8.0;
    let total_height = header_height + cell_size * n as f32 + 8.0;

    // Precompute positions
    struct CellPos { cx: f32, cy: f32, row: usize, col: usize }
    let mut cells: Vec<CellPos> = Vec::new();
    let mut header_positions: Vec<(f32, f32, String)> = Vec::new();
    let mut row_labels: Vec<(f32, f32, String)> = Vec::new();

    for col in 0..n {
        let x = label_width + col as f32 * cell_size + cell_size / 2.0;
        let y = header_height / 2.0;
        header_positions.push((x, y, format!("OUT {}", col + 1)));
    }

    for row in 0..n {
        let y0 = header_height + row as f32 * cell_size;
        row_labels.push((4.0, y0 + cell_size / 2.0, format!("CH {}", row + 1)));

        for col in 0..n {
            let cx = label_width + col as f32 * cell_size + cell_size / 2.0;
            let cy = y0 + cell_size / 2.0;
            cells.push(CellPos { cx, cy, row, col });
        }
    }

    // Allocate the entire matrix area
    let (response, painter) = ui.allocate_painter(egui::vec2(total_width, total_height), egui::Sense::hover());
    let origin = response.rect.left_top();

    // Draw headers
    for (x, y, text) in &header_positions {
        painter.text(
            egui::pos2(origin.x + x, origin.y + y),
            egui::Align2::CENTER_CENTER,
            text.as_str(),
            FontId::monospace(9.0),
            colors.text_muted,
        );
    }

    // Draw row labels
    for (x, y, text) in &row_labels {
        painter.text(
            egui::pos2(origin.x + x, origin.y + y),
            egui::Align2::LEFT_CENTER,
            text.as_str(),
            FontId::monospace(10.0),
            colors.text_title,
        );
    }

    // Draw radio buttons
    for cell in &cells {
        let cx = origin.x + cell.cx;
        let cy = origin.y + cell.cy;
        let is_selected = state.channel_map.get(cell.row) == cell.col;

        let radius = 10.0;
        let stroke = if is_selected {
            egui::Stroke::new(2.5, ACCENT)
        } else {
            egui::Stroke::new(1.0, colors.border_main)
        };
        let fill = if is_selected {
            ACCENT.linear_multiply(0.3)
        } else {
            colors.deep_bg
        };
        painter.circle_stroke(egui::pos2(cx, cy), radius, stroke);
        painter.circle_filled(egui::pos2(cx, cy), radius - 2.0, fill);
    }

    // Handle clicks (separate from painter)
    drop(painter);
    for cell in &cells {
        let cx = origin.x + cell.cx;
        let cy = origin.y + cell.cy;
        let is_selected = state.channel_map.get(cell.row) == cell.col;
        let hitbox = egui::Rect::from_center_size(egui::pos2(cx, cy), egui::vec2(cell_size, cell_size));

        // Use response's interact_rect to sense clicks on sub-regions
        let click_id = egui::Id::new(("chan_map", cell.row, cell.col));
        let clicked = ui.interact(hitbox, click_id, egui::Sense::click()).clicked();
        if clicked && !is_selected {
            state.channel_map.swap(cell.row, cell.col);
        }
    }
}

// ── Step 3: Output format ──────────────────────────────────────────────

fn render_output_format(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let caps_clone = state.ffmpeg_caps.clone();

    // When ffmpeg caps become available, apply intelligent defaults
    if let Some(ref caps) = caps_clone {
        use_available_defaults(state, caps);
    }

    let containers: Vec<(&str, &str)> = if let Some(ref caps) = caps_clone {
        available_containers(caps)
    } else {
        supported_containers()
    };

    ui.horizontal(|ui| {
        ui.label(RichText::new("Container:").font(FontId::proportional(10.0)).color(colors.text_muted));
        egui::ComboBox::from_id_salt("container_combo")
            .selected_text(&state.container)
            .show_ui(ui, |ui| {
                for (key, desc) in &containers {
                    if ui.selectable_label(false, format!("{} — {}", key, desc)).clicked() {
                        state.container = key.to_string();
                        // Re-select encoders compatible with the new container
                        if let Some(ref caps) = caps_clone {
                            re_select_encoders_for_container(state, caps);
                        }
                        if let Some(group) = &state.selected_group {
                            if let Some(folder) = &state.selected_folder {
                                state.output_path = folder.join(default_output_filename(group, &state.container));
                            }
                        }
                    }
                }
            });
    });

    ui.add_space(4.0);
    let video_encoders = if let Some(ref caps) = caps_clone {
        available_video_encoders_for_container(&state.container, caps)
    } else {
        supported_video_encoders()
    };

    ui.horizontal(|ui| {
        ui.label(RichText::new("Video encoder:").font(FontId::proportional(10.0)).color(colors.text_muted));
        egui::ComboBox::from_id_salt("video_enc_combo")
            .selected_text(&state.video_encoder)
            .show_ui(ui, |ui| {
                for (key, desc) in &video_encoders {
                    if ui.selectable_label(false, format!("{} — {}", key, desc)).clicked() {
                        state.video_encoder = key.to_string();
                    }
                }
            });
    });

    ui.add_space(4.0);
    let audio_encoders = if let Some(ref caps) = caps_clone {
        available_audio_encoders_for_container(&state.container, caps)
    } else {
        supported_audio_encoders()
    };

    ui.horizontal(|ui| {
        ui.label(RichText::new("Audio encoder:").font(FontId::proportional(10.0)).color(colors.text_muted));
        egui::ComboBox::from_id_salt("audio_enc_combo")
            .selected_text(&state.audio_encoder)
            .show_ui(ui, |ui| {
                for (key, desc) in &audio_encoders {
                    if ui.selectable_label(false, format!("{} — {}", key, desc)).clicked() {
                        state.audio_encoder = key.to_string();
                    }
                }
            });
    });

    if let Some(ref caps) = caps_clone {
        ui.add_space(4.0);
        let input_files: Vec<PathBuf> = state
            .selected_group
            .as_ref()
            .and_then(|g| state.file_groups.as_ref()?.get(g))
            .cloned()
            .unwrap_or_default();

        match conversion_sanity_check(
            &state.container,
            &state.video_encoder,
            &state.audio_encoder,
            &input_files,
            &state.output_path,
            caps,
        ) {
            Ok(()) => {
                ui.label(RichText::new("✓ Settings are compatible.").font(FontId::proportional(10.0)).color(colors.success_green));
            }
            Err(msg) => {
                let warning_area = egui::Frame::new()
                    .fill(Color32::from_rgb(0xF5, 0x9E, 0x0B).linear_multiply(0.08))
                    .stroke(egui::Stroke::new(1.0, Color32::from_rgb(0xF5, 0x9E, 0x0B).linear_multiply(0.2)))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(10, 6));
                warning_area.show(ui, |ui| {
                    ui.label(RichText::new(format!("⚠ {}", msg)).font(FontId::proportional(10.0)).color(colors.warning_amber));
                });
            }
        }
    }

    if let Some(ref caps) = caps_clone {
        if !caps.has_ffmpeg {
            ui.add_space(4.0);
            let error_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0xEF, 0x44, 0x44).linear_multiply(0.08))
                .stroke(egui::Stroke::new(1.0, Color32::from_rgb(0xEF, 0x44, 0x44).linear_multiply(0.2)))
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 6));
            error_frame.show(ui, |ui| {
                if let Some(msg) = &caps.error_message {
                    ui.label(RichText::new(format!("✗ {}", msg)).font(FontId::proportional(10.0)).color(colors.error_red));
                }
            });
        }
    }
}

/// Apply `select_best_combination` defaults when ffmpeg caps are first loaded.
/// Skips if the current selection is already a valid combination.
fn use_available_defaults(state: &mut AppState, caps: &FfmpegCapabilities) {
    let containers: Vec<&str> = available_containers(caps).iter().map(|(k, _)| *k).collect();
    if !containers.contains(&state.container.as_str()) {
        let (c, v, a) = select_best_combination(caps);
        state.container = c;
        state.video_encoder = v;
        state.audio_encoder = a;
        return;
    }
    let vids: Vec<&str> =
        available_video_encoders_for_container(&state.container, caps).iter().map(|(k, _)| *k).collect();
    let auds: Vec<&str> =
        available_audio_encoders_for_container(&state.container, caps).iter().map(|(k, _)| *k).collect();
    if !vids.contains(&state.video_encoder.as_str()) || !auds.contains(&state.audio_encoder.as_str()) {
        let (c, v, a) = select_best_combination(caps);
        state.container = c;
        state.video_encoder = v;
        state.audio_encoder = a;
    }
}

/// When the container changes, re-select video/audio encoders that are
/// compatible with the new container (and available in ffmpeg).
fn re_select_encoders_for_container(state: &mut AppState, caps: &FfmpegCapabilities) {
    let video_available: Vec<&str> = available_video_encoders_for_container(&state.container, caps)
        .iter()
        .map(|(k, _)| *k)
        .collect();
    if !video_available.is_empty() && !video_available.contains(&state.video_encoder.as_str()) {
        state.video_encoder = video_available[0].to_string();
    }

    let audio_available: Vec<&str> = available_audio_encoders_for_container(&state.container, caps)
        .iter()
        .map(|(k, _)| *k)
        .collect();
    if !audio_available.is_empty() && !audio_available.contains(&state.audio_encoder.as_str()) {
        state.audio_encoder = audio_available[0].to_string();
    }
}

// ── Step 4: Output file path ────────────────────────────────────────────

fn render_output_path(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    ui.horizontal(|ui| {
        ui.label(RichText::new("Save as:").font(FontId::proportional(10.0)).color(colors.text_muted));
        let mut path_str = state.output_path.to_string_lossy().to_string();
        if ui
            .add(
                egui::TextEdit::singleline(&mut path_str)
                    .font(FontId::monospace(10.0))
                    .desired_width(ui.available_width() - 100.0),
            )
            .changed()
        {
            state.output_path = PathBuf::from(&path_str);
        }
        if ui.button("Browse…").clicked() {
            let default_name = state
                .output_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("output.mkv");
            let file = rfd::FileDialog::new()
                .set_file_name(default_name)
                .save_file();
            if let Some(path) = file {
                state.output_path = path;
            }
        }
    });

    // Trim to first LTC checkbox
    let ltc_available = !state.latest.ltc_is_detecting
        && (state.latest.ltc_decode_result.is_some() || state.latest.ltc_decode_error.is_some());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_enabled(ltc_available, egui::Checkbox::new(
            &mut state.trim_ltc_start,
            "Cut start to first LTC frame",
        ));
        if state.trim_ltc_start && state.trim_offset_secs > 0.001 {
            ui.label(
                RichText::new(format!("(trim {:.3}s of silence)", state.trim_offset_secs))
                    .font(FontId::proportional(10.0))
                    .color(colors.text_muted),
            );
        }
        if !ltc_available && !state.latest.ltc_is_detecting {
            ui.label(
                RichText::new("(Detect LTC first)")
                    .font(FontId::proportional(10.0))
                    .color(colors.text_muted),
            );
        }
    });
}

// ── Convert button ─────────────────────────────────────────────────────

fn render_convert_button(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_running = matches!(
        state.conversion_state.lock().unwrap().status,
        ConversionStatus::Running { .. }
    );

    if is_running {
        if ui
            .add(
                egui::Button::new(
                    RichText::new("■ CANCEL CONVERSION")
                        .font(FontId::proportional(13.0))
                        .color(Color32::WHITE)
                        .strong(),
                )
                .fill(Color32::from_rgb(0xDC, 0x26, 0x26))
                .min_size(egui::vec2(ui.available_width(), 42.0)),
            )
            .clicked()
        {
            state.cancel_flag.store(true, Ordering::Relaxed);
        }
        return;
    }

    let can_convert = state.selected_group.is_some()
        && !state.output_path.as_os_str().is_empty()
        && state.ffmpeg_caps.as_ref().map(|c| c.has_ffmpeg).unwrap_or(false);

    let sanity_ok = if can_convert {
        let caps = state.ffmpeg_caps.as_ref().unwrap();
        let input_files: Vec<PathBuf> = state
            .selected_group
            .as_ref()
            .and_then(|g| state.file_groups.as_ref()?.get(g))
            .cloned()
            .unwrap_or_default();
        conversion_sanity_check(
            &state.container,
            &state.video_encoder,
            &state.audio_encoder,
            &input_files,
            &state.output_path,
            caps,
        )
        .is_ok()
    } else {
        false
    };

    ui.add_enabled_ui(can_convert && sanity_ok, |ui| {
        if ui
            .add(
                egui::Button::new(
                    RichText::new("CONVERT TO MULTI-AUDIO VIDEO")
                        .font(FontId::proportional(13.0))
                        .color(Color32::BLACK)
                        .strong(),
                )
                .fill(ACCENT)
                .min_size(egui::vec2(ui.available_width(), 42.0)),
            )
            .clicked()
        {
            start_conversion(state);
        }
    });

    if !can_convert {
        ui.add_space(2.0);
        let mut reasons: Vec<&str> = Vec::new();
        if state.selected_group.is_none() {
            reasons.push("select a recording");
        }
        if state.output_path.as_os_str().is_empty() {
            reasons.push("set an output file path");
        }
        if !state.ffmpeg_caps.as_ref().map(|c| c.has_ffmpeg).unwrap_or(false) {
            reasons.push("ffmpeg is not available");
        }
        if !reasons.is_empty() {
            ui.label(
                RichText::new(format!("To convert, please {}.", reasons.join(", ")))
                    .font(FontId::proportional(10.0))
                    .color(colors.text_secondary),
            );
        }
    } else if !sanity_ok {
        ui.add_space(2.0);
        ui.label(
            RichText::new("Fix the compatibility issue above before converting.")
                .font(FontId::proportional(10.0))
                .color(colors.warning_amber),
        );
    }
}

fn start_conversion(state: &mut AppState) {
    let input_files: Vec<PathBuf> = state
        .selected_group
        .as_ref()
        .and_then(|g| state.file_groups.as_ref()?.get(g))
        .cloned()
        .unwrap_or_default();

    let settings = ConverterSettings {
        input_files,
        channel_map: state.channel_map.clone(),
        container: state.container.clone(),
        video_encoder: state.video_encoder.clone(),
        audio_encoder: state.audio_encoder.clone(),
        output_path: state.output_path.clone(),
        trim_start_secs: if state.trim_ltc_start { state.trim_offset_secs } else { 0.0 },
    };

    *state.conversion_state.lock().unwrap() = ConversionState::idle();
    state.cancel_flag.store(false, Ordering::Relaxed);

    let cs = state.conversion_state.clone();
    let cf = state.cancel_flag.clone();
    state.convert_handle = Some(spawn_conversion(settings, cs, cf));
}

// ── Progress & log display ──────────────────────────────────────────────

fn render_conversion_progress(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let cs = state.conversion_state.lock().unwrap();
    let status = &cs.status;

    if matches!(status, ConversionStatus::Running { .. }) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }

    match status {
        ConversionStatus::Idle => {}
        ConversionStatus::Running { progress } => {
            let progress_f32 = *progress;
            drop(cs);

            ui.label(RichText::new("Converting…").font(FontId::proportional(11.0)).color(colors.text_title).strong());
            ui.add_space(4.0);
            let pb = egui::ProgressBar::new(progress_f32)
                .show_percentage()
                .desired_width(ui.available_width());
            ui.add(pb);
            ui.add_space(4.0);

            let log_text;
            {
                let cs2 = state.conversion_state.lock().unwrap();
                log_text = cs2.ffmpeg_output.clone();
            }
            let log_height = 120.0;
            let log_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x0D, 0x0D, 0x0F))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::symmetric(8, 4));
            log_frame.show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(log_height)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(&log_text)
                                .font(FontId::monospace(9.0))
                                .color(Color32::from_rgb(0x88, 0xCC, 0x88)),
                        );
                    });
            });
        }
        ConversionStatus::Completed => {
            drop(cs);
            let log_text;
            {
                let cs2 = state.conversion_state.lock().unwrap();
                log_text = cs2.ffmpeg_output.clone();
            }

            ui.label(RichText::new("✓ Conversion completed successfully!").font(FontId::proportional(12.0)).color(colors.success_green).strong());
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("File saved to: {}", state.output_path.display()))
                    .font(FontId::proportional(10.0))
                    .color(colors.text_muted),
            );
            ui.add_space(4.0);

            let log_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x0D, 0x0D, 0x0F))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::symmetric(8, 4));
            log_frame.show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(80.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(&log_text)
                                .font(FontId::monospace(9.0))
                                .color(Color32::from_rgb(0x88, 0xCC, 0x88)),
                        );
                    });
            });

            if ui.button("Start New Conversion").clicked() {
                *state.conversion_state.lock().unwrap() = ConversionState::idle();
            }
        }
        ConversionStatus::Failed { error_log } => {
            let error_text = error_log.clone();
            drop(cs);

            let error_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x44, 0x11, 0x11))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.error_red))
                .inner_margin(egui::Margin::symmetric(10, 6));
            error_frame.show(ui, |ui| {
                ui.label(RichText::new("✗ CONVERSION FAILED").font(FontId::proportional(12.0)).color(colors.error_red).strong());
            });

            ui.add_space(4.0);

            let log_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x0D, 0x0D, 0x0F))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::symmetric(8, 4));
            log_frame.show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .stick_to_bottom(false)
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(&error_text)
                                .font(FontId::monospace(9.0))
                                .color(Color32::from_rgb(0xFF, 0x66, 0x66)),
                        );
                    });
            });

            ui.horizontal(|ui| {
                if ui.button("Copy Error Log").clicked() {
                    ui.ctx().copy_text(error_text.clone());
                }
                if ui.button("Try Again").clicked() {
                    *state.conversion_state.lock().unwrap() = ConversionState::idle();
                }
            });
        }
    }
}