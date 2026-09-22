use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::time::Duration;

use egui::{Color32, FontId, RichText, Ui};
use gui_engine::command::GuiCommand;
use gui_engine::config;
use gui_engine::converter::{
    apply_available_defaults, available_audio_encoders_for_container,
    available_containers, conversion_sanity_check_with_naming, copy_mode_container_for_input,
    evaluate_readiness,
    find_timecode_at_offset, format_blockers, query_ffmpeg_capabilities,
    spawn_conversion, supported_audio_encoders, supported_containers,
    ChannelMap, ConversionPipeline, ConversionState, ConversionStatus,
    ConverterSettings, FfmpegCapabilities, OutputNamingMode, RecordingType, TimecodeMetadata,
};
use gui_engine::video_codecs::{available_video_codecs, describe_chain, normalize_video_codec, supported_video_codecs};
use gui_engine::file_pattern::match_files_all_patterns;
use gui_engine::timecode::{self, FPS_OPTIONS};
use gui_engine::LtcDecodeStatus;

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

            if state.selected_group_idx.is_some() {
                step_header(ui, "L", "VERIFY LTC TRACK", &colors);
                ui.add_space(8.0);
                render_ltc_verification(ui, state);
                ui.add_space(16.0);

                step_header(ui, "2", "CHANNEL SPLITTING OR MAPPING", &colors);
                ui.add_space(8.0);
                render_channel_matrix(ui, state);
                render_split_options(ui, state);
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

/// True when "Leave Video Encoding Untouched" applies. Stream copy is only
/// meaningful for the video pipeline (audio-only recordings have no video
/// stream to copy, and synthetic video must be encoded).
fn copy_mode_active(state: &AppState) -> bool {
    state.leave_video_untouched && state.recording_type == RecordingType::VideoClipSequence
}

// ── Step 1: File selection ─────────────────────────────────────────────

fn render_file_selection(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    // Folder picker (all patterns are applied simultaneously)
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
            let mut dialog = rfd::FileDialog::new();
            if let Some(ref last) = state.selected_folder {
                dialog = dialog.set_directory(last);
            }
            let folder = dialog.pick_folder();
            if let Some(path) = folder {
                state.selected_folder = Some(path.clone());
                state.selected_files = None;
                config::save_input_folder(&path);

                // Apply all patterns simultaneously
                state.file_groups = Some(match_files_all_patterns(&path));
                state.selected_group_idx = None;
                state.ltc_file_idx = 1; // default to track 2
                state.trim_ltc_start = false;

                let guard = state.ffmpeg_probe_started.clone();
                let caps_arc = state.ffmpeg_caps.clone();
                if !guard.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    std::thread::spawn(move || {
                        let result = query_ffmpeg_capabilities();
                        *caps_arc.lock().unwrap() = Some(result);
                    });
                }
            }
        }
    });

    // File group selector (shows all matched groups with type badge)
    let groups_clone = state.file_groups.clone();
    if let Some(ref groups) = groups_clone {
        if groups.is_empty() {
            ui.label(RichText::new("No files matching any known pattern were found in this folder.")
                .font(FontId::proportional(10.0)).color(colors.error_red));
        } else {
            let mut probe_fn: Option<String> = None;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Recording:").font(FontId::proportional(10.0)).color(colors.text_muted));
                let selected_text = state
                    .selected_group_idx
                    .and_then(|idx| groups.get(idx))
                    .map(|g| g.prefix.as_str())
                    .unwrap_or("Select a recording…");
                egui::ComboBox::from_id_salt("group_combo")
                    .selected_text(selected_text)
                    .show_ui(ui, |ui| {
                        for (i, group) in groups.iter().enumerate() {
                            let type_badge = match group.recording_type {
                                RecordingType::MultiTrackAudio => "🎵 AUDIO",
                                RecordingType::VideoClipSequence => "🎬 VIDEO",
                            };
                            let detail = group.files
                                .iter()
                                .map(|f| f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let label = format!(
                                "{}  [{}]  ({} file{}: {})",
                                group.prefix,
                                type_badge,
                                group.files.len(),
                                if group.files.len() == 1 { "" } else { "s" },
                                detail
                            );
                            if ui.selectable_label(false, label).clicked() {
                                state.selected_group_idx = Some(i);
                                let num_ch = group.files.len();
                                state.channel_map = ChannelMap::identity(num_ch);
                                state.recording_type = group.recording_type.clone();
                                state.filename_prefix = group.prefix.clone();
                                state.naming_mode = match group.recording_type {
                                    RecordingType::VideoClipSequence => OutputNamingMode::SourceStems,
                                    RecordingType::MultiTrackAudio => OutputNamingMode::PrefixTemplates,
                                };
                                state.output_folder = state.selected_folder.clone().unwrap_or_default();
                                state.split_tracks = false;
                                state.drop_ltc_track = false;
                                state.per_file_trim_offsets = vec![0.0; num_ch];
                                state.ltc_file_idx = 0;

                                // Schedule probe for video files
                                if group.recording_type == RecordingType::VideoClipSequence && !group.files.is_empty() {
                                    probe_fn = Some(group.files[0].to_string_lossy().to_string());
                                }
                            }
                        }
                    });
            });
            if let Some(path) = probe_fn {
                state.send(GuiCommand::ProbeVideo(path));
            }

            if let Some(idx) = state.selected_group_idx {
                if let Some(group) = groups.get(idx) {
                    ui.add_space(4.0);
                    let pill_frame = egui::Frame::new()
                        .fill(colors.deep_bg)
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 4));
                    pill_frame.show(ui, |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::Vec2::new(4.0, 2.0);
                            for f in &group.files {
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
        RichText::new("Select the track that carries the LTC timecode signal, then click \"Detect LTC\" to verify it can be read successfully.")
            .font(FontId::proportional(9.0))
            .color(colors.text_secondary),
    );
    ui.add_space(6.0);

    let group = state
        .selected_group_idx
        .and_then(|idx| state.file_groups.as_ref()?.get(idx));

    let file_count = group.map(|g| g.files.len()).unwrap_or(0);

    if file_count > 0 {
        // Ensure ltc_file_idx is in range (0-based, default to track 2 = index 1)
        if state.ltc_file_idx >= file_count {
            state.ltc_file_idx = file_count.saturating_sub(1);
        }

        let is_video = state.recording_type == RecordingType::VideoClipSequence;

        // Build channel options from probe (for video) or file list (for audio)
        struct ChannelOption {
            stream: usize,
            channel: usize,
            label: String,
        }

        let channel_options: Vec<ChannelOption> = if is_video {
            if let Some(ref probe) = state.latest.ltc_probe {
                probe
                    .streams
                    .iter()
                    .flat_map(|s| {
                        (0..s.channels).map(move |ch| {
                            let label = if probe.streams.len() > 1 {
                                format!("Stream {} Ch {}", s.stream_index + 1, ch + 1)
                            } else {
                                format!("Track 1 {}", if ch == 0 { "L" } else { "R" })
                            };
                            ChannelOption {
                                stream: s.stream_index,
                                channel: ch,
                                label,
                            }
                        })
                    })
                    .collect()
            } else {
                vec![ChannelOption {
                    stream: 0,
                    channel: 0,
                    label: "Probing…".to_string(),
                }]
            }
        } else {
            group
                .unwrap()
                .files
                .iter()
                .enumerate()
                .map(|(i, f)| ChannelOption {
                    stream: i,
                    channel: 0,
                    label: f.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string(),
                })
                .collect()
        };

        // Clone cmd_tx before the UI closure to avoid borrowing `state`
        // as a whole through method calls while `group` borrows `state.file_groups`.
        let cmd_tx = state.cmd_tx.clone();

        // Ensure selected stream/channel is within range
        if is_video {
            let in_range = channel_options.iter().any(|o| {
                o.stream == state.latest.ltc_selected_stream && o.channel == state.latest.ltc_selected_channel
            });
            if !in_range && !channel_options.is_empty() {
                let _ = cmd_tx.send(GuiCommand::SetLtcDecodeStream(channel_options[0].stream));
                let _ = cmd_tx.send(GuiCommand::SetLtcDecodeChannel(channel_options[0].channel));
            }
        } else {
            if state.ltc_file_idx >= channel_options.len() && !channel_options.is_empty() {
                state.ltc_file_idx = channel_options.len().saturating_sub(1);
            }
        }

        // ── Row 1: Track/channel selection ──
        ui.horizontal(|ui| {
            ui.label(RichText::new("Source:").font(FontId::proportional(10.0)).color(colors.text_muted));

            let current_label = if is_video {
                channel_options
                    .iter()
                    .find(|o| {
                        o.stream == state.latest.ltc_selected_stream
                            && o.channel == state.latest.ltc_selected_channel
                    })
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| "Select…".to_string())
            } else {
                channel_options
                    .get(state.ltc_file_idx)
                    .map(|o| o.label.clone())
                    .unwrap_or_else(|| "Select…".to_string())
            };

            egui::ComboBox::from_id_salt("ltc_file_combo")
                .selected_text(&current_label)
                .width(ui.available_width())
                .show_ui(ui, |ui| {
                    if is_video {
                        for opt in &channel_options {
                            let is_sel = opt.stream == state.latest.ltc_selected_stream
                                && opt.channel == state.latest.ltc_selected_channel;
                            if ui.selectable_label(is_sel, &opt.label).clicked() {
                                let _ = cmd_tx.send(GuiCommand::SetLtcDecodeStream(opt.stream));
                                let _ = cmd_tx.send(GuiCommand::SetLtcDecodeChannel(opt.channel));
                            }
                        }
                    } else {
                        for (i, opt) in channel_options.iter().enumerate() {
                            if ui.selectable_label(i == state.ltc_file_idx, &opt.label).clicked() {
                                state.ltc_file_idx = i;
                            }
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

            if is_detecting {
                ui.ctx().request_repaint_after(Duration::from_millis(100));
                let progress_pct = state.latest.ltc_decode_progress_pct;
                let progress_str = state.latest.ltc_decode_progress_str.clone();
                ui.add(egui::ProgressBar::new(progress_pct).show_percentage().desired_width(140.0));
                ui.add_space(2.0);
                ui.label(RichText::new(&progress_str).font(FontId::monospace(9.0)).color(colors.text_muted));
                ui.add_space(4.0);
                if ui.add(egui::Button::new(RichText::new("✕ CANCEL").font(FontId::proportional(10.0)).color(Color32::WHITE).strong())
                    .fill(Color32::from_rgb(0xDC, 0x26, 0x26)).min_size(egui::vec2(80.0, 22.0))).clicked()
                {
                    state.send(GuiCommand::CancelDecode);
                }
            } else {
                if is_video {
                    let file_path = group.unwrap().files[0].to_string_lossy().to_string();
                    let stream_idx = state.latest.ltc_selected_stream;
                    let channel_idx = state.latest.ltc_selected_channel;
                    if ui.add(egui::Button::new(RichText::new("🔍 Detect LTC").font(FontId::proportional(11.0)).color(Color32::BLACK).strong())
                        .fill(ACCENT).min_size(egui::vec2(100.0, 24.0))).clicked()
                    {
                        state.send(GuiCommand::ParseLtcVideo(file_path, stream_idx, channel_idx));
                    }
                } else {
                    let file_path = group.unwrap().files[state.ltc_file_idx].to_string_lossy().to_string();
                    if ui.add(egui::Button::new(RichText::new("🔍 Detect LTC").font(FontId::proportional(11.0)).color(Color32::BLACK).strong())
                        .fill(ACCENT).min_size(egui::vec2(100.0, 24.0))).clicked()
                    {
                        state.send(GuiCommand::ParseLtcWavFile(file_path));
                    }
                }
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
            ui.label(RichText::new(format!("❌ {}", error)).font(FontId::proportional(10.0)).color(colors.error_red));
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

            // Quality report
            if let Some(ref q) = result.quality {
                ui.add_space(4.0);

                let (grade_color, grade_bg) = if q.score >= 0.95 {
                    (colors.success_green, colors.success_green.linear_multiply(0.12))
                } else if q.score >= 0.80 {
                    (colors.success_green, colors.success_green.linear_multiply(0.08))
                } else if q.score >= 0.60 {
                    (colors.warning_amber, colors.warning_amber.linear_multiply(0.10))
                } else if q.score >= 0.30 {
                    (colors.error_red, colors.error_red.linear_multiply(0.10))
                } else {
                    (colors.error_red, colors.error_red.linear_multiply(0.15))
                };

                let quality_frame = egui::Frame::new()
                    .fill(grade_bg)
                    .corner_radius(4.0)
                    .stroke(egui::Stroke::new(0.5, grade_color.linear_multiply(0.3)))
                    .inner_margin(egui::Margin::symmetric(8, 4));
                quality_frame.show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Quality:")
                            .font(FontId::proportional(9.0))
                            .color(colors.text_muted));
                        ui.label(RichText::new(format!("{:.0}%", q.score * 100.0))
                            .font(FontId::monospace(11.0))
                            .color(grade_color)
                            .strong());
                        ui.label(RichText::new(&q.grade)
                            .font(FontId::proportional(9.0))
                            .color(grade_color));
                    });
                    ui.horizontal(|ui| {
                        let issues = if q.edit_count > 0 {
                            format!("{} edit(s)", q.edit_count)
                        } else if q.glitch_count > 0 || q.gap_count > 0 {
                            format!("{} gap(s), {} glitch(es)", q.gap_count, q.glitch_count)
                        } else if q.missing_frames > 0 {
                            format!("{} missing", q.missing_frames)
                        } else {
                            "perfect".to_string()
                        };
                        ui.label(RichText::new(issues)
                            .font(FontId::monospace(8.0))
                            .color(colors.text_muted));
                        if q.max_drift_secs > 0.01 {
                            ui.label(RichText::new(format!("drift {:.3}s", q.max_drift_secs))
                                .font(FontId::monospace(8.0))
                                .color(colors.text_muted));
                        }
                    });
                });
            }

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

            // Copy Report button
            if !result.timecodes.is_empty() {
                ui.add_space(4.0);
                if ui.button("📋 Copy Report").clicked() {
                    let report = format_ltc_report_text(result);
                    ui.ctx().copy_text(report);
                }
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

fn format_ltc_report_text(result: &gui_engine::LtcDetectionResult) -> String {
    let drop_flag = if result.drop_frame { " DF" } else { "" };
    let fps_str = if result.detected_fps > 0.0 {
        format!("{:.2}{}", result.detected_fps, drop_flag)
    } else {
        "—".to_string()
    };

    let first = result.timecodes.first();
    let last = result.timecodes.last();
    let tc_range = match (first, last) {
        (Some(f), Some(l)) => {
            format!(
                "{} → {}",
                timecode::timecode_to_string(f.timecode, result.drop_frame),
                timecode::timecode_to_string(l.timecode, result.drop_frame),
            )
        }
        _ => "—".to_string(),
    };

    let mut report = String::new();
    report.push_str("LTC Decode Report\n");
    report.push_str("=================\n");
    report.push_str(&format!("Status:          {:?}\n", result.status));
    report.push_str(&format!("FPS:             {}\n", fps_str));
    report.push_str(&format!(
        "Valid frames:   {} / {} ({:.1}%)\n",
        result.valid_frames,
        result.total_possible_frames,
        result.avg_confidence * 100.0
    ));
    report.push_str(&format!("Timecode range:  {}\n", tc_range));
    report.push_str(&format!("Sample rate:     {} Hz\n", result.sample_rate));
    report.push_str(&format!("Audio duration:  {:.2}s\n", result.total_audio_duration_secs));
    report.push_str(&format!("Processing time: {:.1}ms\n", result.processing_time_ms));

    if let Some(ref q) = result.quality {
        report.push_str(&format!(
            "\nQuality Score:  {:.2} / 1.00 ({})\n",
            q.score, q.grade
        ));
        report.push_str(&format!(
            "  Missing:       {} frames, {} gap(s), {} glitch(es), {} edit point(s)\n",
            q.missing_frames, q.gap_count, q.glitch_count, q.edit_count
        ));
        if q.max_drift_secs > 0.01 {
            report.push_str(&format!("  Max drift:    {:.3}s\n", q.max_drift_secs));
        }
    }

    if !result.timecodes.is_empty() {
        report.push_str(&format!(
            "\nTimecodes ({} total):\n",
            result.timecodes.len()
        ));
        for ft in &result.timecodes {
            let tc = timecode::timecode_to_string(ft.timecode, result.drop_frame);
            report.push_str(&format!(
                "  [{:4}] {}  ({:.3}s)\n",
                ft.frame_index, tc, ft.timecode_secs
            ));
        }
    }

    report
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

// ── Split / Drop options ────────────────────────────────────────────────

fn render_split_options(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let ltc_available = state.latest.ltc_decode_result.is_some();
    let is_video = state.recording_type == RecordingType::VideoClipSequence;

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        let prev_split = state.split_tracks;
        ui.add(egui::Checkbox::new(
            &mut state.split_tracks,
            "Split tracks into separate files",
        ));
        if prev_split && !state.split_tracks && state.concat_audio {
            state.concat_audio = false;
        }
        ui.add_enabled(ltc_available, egui::Checkbox::new(
            &mut state.drop_ltc_track,
            "Drop LTC track",
        ));
    });
    if is_video && state.split_tracks {
        ui.horizontal(|ui| {
            ui.add(egui::Checkbox::new(
                &mut state.concat_audio,
                "Concatenate audio tracks across clips (one file per track)",
            ));
        });
        if state.concat_audio {
            ui.label(
                RichText::new("ℹ Audio from all clips will be joined into one file per track (in clip order).")
                    .font(FontId::proportional(9.0))
                    .color(colors.text_secondary),
            );
        }
    }
    if state.split_tracks {
        ui.label(
            RichText::new("ℹ Each input track will be written to its own file. Channel mapping greets are preserved.")
                .font(FontId::proportional(9.0))
                .color(colors.text_secondary),
        );
    }
    if state.drop_ltc_track && state.ltc_file_idx < state.channel_map.num_channels() {
        ui.label(
            RichText::new(format!("ℹ LTC track (channel {}) will be excluded from all output.", state.ltc_file_idx + 1))
                .font(FontId::proportional(9.0))
                .color(colors.text_secondary),
        );
    }
}

// ── Step 3: Output format (Video / Audio columns) ──────────────────────

fn render_output_format(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let caps_opt = state.ffmpeg_caps.lock().unwrap().clone();

    if let Some(ref caps) = caps_opt {
        apply_available_defaults(&mut state.container, &mut state.video_encoder, &mut state.audio_encoder, caps);
    }

    let containers: Vec<(&str, &str)> = if let Some(ref caps) = caps_opt {
        available_containers(caps)
    } else {
        supported_containers()
    };

    let video_encoders = if let Some(ref caps) = caps_opt {
        available_video_codecs(&state.container, caps)
    } else {
        supported_video_codecs()
    };

    let audio_encoders = if let Some(ref caps) = caps_opt {
        available_audio_encoders_for_container(&state.container, caps)
    } else {
        supported_audio_encoders()
    };

    // Clone values to avoid borrow conflicts with FnMut closures
    let cur_container = state.container.clone();
    let cur_video_encoder = state.video_encoder.clone();
    let cur_audio_encoder = state.audio_encoder.clone();

    // Two-column layout: Video Format | Audio Format
    let is_narrow = ui.available_width() < 400.0;
    if is_narrow {
        ui.vertical(|ui| {
            if state.recording_type == RecordingType::VideoClipSequence {
                ui.checkbox(&mut state.leave_video_untouched, "Leave video encoding untouched (stream copy)");
                if copy_mode_active(state) {
                    ui.label(
                        RichText::new("Video is copied without re-encoding (much faster). Cuts snap to the nearest keyframe before the trim point.")
                            .font(FontId::proportional(9.0))
                            .color(colors.text_muted),
                    );
                }
                ui.add_space(4.0);
            }
            ui.label(RichText::new("VIDEO FORMAT").font(FontId::proportional(10.0)).color(colors.text_title).strong());
            ui.add_space(4.0);
            ui.add_enabled_ui(!copy_mode_active(state), |ui| {
                {
                    let mut update_container = |v: &str| {
                        state.container = v.to_string();
                        if let Some(ref caps) = caps_opt {
                            re_select_encoders_for_container(state, caps);
                        }
                    };
                    render_format_row(ui, "Container", &cur_container, &containers, &mut update_container, &colors);
                }
                {
                    let mut update_video = |v: &str| state.video_encoder = v.to_string();
                    render_format_row(ui, "Video codec", &cur_video_encoder, &video_encoders, &mut update_video, &colors);
                }
            });

            if state.recording_type == RecordingType::MultiTrackAudio {
                ui.checkbox(&mut state.generate_synthetic_video, "Generate synthetic video (blue background)");
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            ui.label(RichText::new("AUDIO FORMAT").font(FontId::proportional(10.0)).color(colors.text_title).strong());
            ui.add_space(4.0);
            {
                let mut update_audio = |v: &str| state.audio_encoder = v.to_string();
                render_format_row(ui, "Audio encoder", &cur_audio_encoder, &audio_encoders, &mut update_audio, &colors);
            }
        });
    } else {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                if state.recording_type == RecordingType::VideoClipSequence {
                    ui.checkbox(&mut state.leave_video_untouched, "Leave video encoding untouched (stream copy)");
                    if copy_mode_active(state) {
                        ui.label(
                            RichText::new("Video is copied without re-encoding (much faster). Cuts snap to the nearest keyframe before the trim point.")
                                .font(FontId::proportional(9.0))
                                .color(colors.text_muted),
                        );
                    }
                    ui.add_space(4.0);
                }
                ui.label(RichText::new("VIDEO FORMAT").font(FontId::proportional(10.0)).color(colors.text_title).strong());
                ui.add_space(4.0);
                ui.add_enabled_ui(!copy_mode_active(state), |ui| {
                    {
                        let mut update_container = |v: &str| {
                            state.container = v.to_string();
                            if let Some(ref caps) = caps_opt {
                                re_select_encoders_for_container(state, caps);
                            }
                        };
                        render_format_row(ui, "Container", &cur_container, &containers, &mut update_container, &colors);
                    }
                    {
                        let mut update_video = |v: &str| state.video_encoder = v.to_string();
                        render_format_row(ui, "Video codec", &cur_video_encoder, &video_encoders, &mut update_video, &colors);
                    }
                });
                if state.recording_type == RecordingType::MultiTrackAudio {
                    ui.checkbox(&mut state.generate_synthetic_video, "Generate synthetic video");
                }
            });
            ui.add_space(16.0);
            ui.separator();
            ui.add_space(16.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("AUDIO FORMAT").font(FontId::proportional(10.0)).color(colors.text_title).strong());
                ui.add_space(4.0);
                {
                    let mut update_audio = |v: &str| state.audio_encoder = v.to_string();
                    render_format_row(ui, "Audio encoder", &cur_audio_encoder, &audio_encoders, &mut update_audio, &colors);
                }
            });
        });
    }

    // Sanity check
    if let Some(ref caps) = caps_opt {
        ui.add_space(4.0);
        let input_files: Vec<PathBuf> = state
            .selected_group_idx
            .and_then(|idx| state.file_groups.as_ref()?.get(idx))
            .map(|g| g.files.clone())
            .unwrap_or_default();

        match conversion_sanity_check_with_naming(
            &state.container,
            &state.video_encoder,
            &state.audio_encoder,
            &input_files,
            &state.output_folder,
            &state.filename_prefix,
            caps,
            Some(&state.audio_suffix_template),
            Some(&state.video_suffix_template),
            Some(&state.naming_mode),
            copy_mode_active(state),
        ) {
            Ok(()) => {
                if copy_mode_active(state) {
                    ui.label(
                        RichText::new("✓ Settings are compatible — video stream will be copied (no re-encode)")
                            .font(FontId::proportional(10.0))
                            .color(colors.success_green),
                    );
                } else {
                    let codec = normalize_video_codec(&state.video_encoder);
                    let chain = describe_chain(codec, caps);
                    ui.label(
                        RichText::new(format!("✓ Settings are compatible — {} via {}", codec, chain))
                            .font(FontId::proportional(10.0))
                            .color(colors.success_green),
                    );
                }
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

    if let Some(ref caps) = caps_opt {
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

fn render_format_row(
    ui: &mut Ui,
    label: &str,
    current: &str,
    options: &[(&str, &str)],
    on_change: &mut dyn FnMut(&str),
    colors: &crate::theme::ThemeColors,
) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{}:", label)).font(FontId::proportional(10.0)).color(colors.text_muted));
        egui::ComboBox::from_id_salt(format!("fmt_{}", label))
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (key, desc) in options {
                    if ui.selectable_label(false, format!("{} — {}", key, desc)).clicked() {
                        on_change(key);
                    }
                }
            });
    });
}



/// When the container changes, re-select video codec/audio encoders that are
/// compatible with the new container (and available in ffmpeg).
fn re_select_encoders_for_container(state: &mut AppState, caps: &FfmpegCapabilities) {
    let codecs_available: Vec<&str> = available_video_codecs(&state.container, caps)
        .iter()
        .map(|(k, _)| *k)
        .collect();
    let codec = normalize_video_codec(&state.video_encoder);
    if !codecs_available.is_empty() && !codecs_available.contains(&codec) {
        state.video_encoder = codecs_available[0].to_string();
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

    // Output folder
    ui.horizontal(|ui| {
        ui.label(RichText::new("Output folder:").font(FontId::proportional(10.0)).color(colors.text_muted));
        let mut folder_str = state.output_folder.to_string_lossy().to_string();
        if ui
            .add(
                egui::TextEdit::singleline(&mut folder_str)
                    .font(FontId::monospace(10.0))
                    .desired_width(ui.available_width() - 100.0),
            )
            .changed()
        {
            state.output_folder = PathBuf::from(&folder_str);
        }
        if ui.button("Browse…").clicked() {
            let folder = rfd::FileDialog::new()
                .set_directory(&state.output_folder)
                .pick_folder();
            if let Some(path) = folder {
                state.output_folder = path.clone();
                config::save_output_folder(&path);
            }
        }
    });

    // Filename prefix
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Filename prefix:").font(FontId::proportional(10.0)).color(colors.text_muted));
        ui.add_sized(
            egui::vec2(ui.available_width(), 20.0),
            egui::TextEdit::singleline(&mut state.filename_prefix)
                .font(FontId::monospace(10.0)),
        );
    });

    // Audio suffix
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Audio suffix:").font(FontId::proportional(10.0)).color(colors.text_muted));
        ui.add_sized(
            egui::vec2(ui.available_width(), 20.0),
            egui::TextEdit::singleline(&mut state.audio_suffix_template)
                .font(FontId::monospace(10.0)),
        );
    });

    // Video suffix
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Video suffix:").font(FontId::proportional(10.0)).color(colors.text_muted));
        ui.add_sized(
            egui::vec2(ui.available_width(), 20.0),
            egui::TextEdit::singleline(&mut state.video_suffix_template)
                .font(FontId::monospace(10.0)),
        );
    });

    // Preview of output filenames
    let has_group = state.selected_group_idx.is_some();
    if has_group && (state.naming_mode == OutputNamingMode::SourceStems || !state.filename_prefix.is_empty()) {
        ui.add_space(4.0);
        let ext = if copy_mode_active(state) {
            // Container is derived from the input file in copy mode
            selected_input_files(state)
                .first()
                .map(|f| copy_mode_container_for_input(f))
                .unwrap_or("mkv")
                .to_string()
        } else {
            match state.container.as_str() {
                "mp4" | "mov" | "mkv" | "mxf" | "webm" => state.container.clone(),
                _ => "mkv".to_string(),
            }
        };
        let group = state.selected_group_idx
            .and_then(|idx| state.file_groups.as_ref()?.get(idx));
        let num_files = group.map(|g| g.files.len()).unwrap_or(0);
        let is_source_stems = state.naming_mode == OutputNamingMode::SourceStems;
        if is_source_stems && num_files > 0 {
            // Show per-file output names
            let lines: Vec<String> = group.unwrap().files.iter().enumerate().map(|(i, f)| {
                let stem = f.file_stem().and_then(|s| s.to_str()).unwrap_or("?");
                let suffix = state.video_suffix_template
                    .replace("{:01d}", &format!("{:01}", i + 1))
                    .replace("{:02d}", &format!("{:02}", i + 1))
                    .replace("{:03d}", &format!("{:03}", i + 1));
                format!("  {}{}.{}", stem, suffix, ext)
            }).collect();
            ui.label(RichText::new(format!("↳ {} output file(s):", num_files))
                .font(FontId::proportional(9.0)).color(colors.text_secondary));
            for line in lines {
                ui.label(RichText::new(line)
                    .font(FontId::monospace(8.5)).color(colors.text_muted));
            }
        } else {
            let total_probed = state.latest.ltc_probe.as_ref().map(|p| p.total_audio_channels).unwrap_or(0);
            let num_video = if state.recording_type == RecordingType::VideoClipSequence { num_files } else { 0 };
            let preview = if state.split_tracks {
                let n = if state.recording_type == RecordingType::VideoClipSequence {
                    total_probed
                } else {
                    state.channel_map.num_channels()
                };
                let count = if state.drop_ltc_track { n.saturating_sub(1) } else { n };
                let audio_label = if state.concat_audio && state.recording_type == RecordingType::VideoClipSequence {
                    "concatenated audio file(s)"
                } else {
                    "audio file(s)"
                };
                format!(
                    "↳ {} {} + {} video clip(s) in {}",
                    count,
                    audio_label,
                    num_video,
                    state.output_folder.display(),
                )
            } else {
                format!(
                    "↳ 1 audio file + {} video clip(s) in {}",
                    num_video,
                    state.output_folder.display(),
                )
            };
            ui.label(RichText::new(preview).font(FontId::proportional(9.0)).color(colors.text_secondary));
        }
    }

    // Trim to first LTC checkbox (renamed for clarity)
    let ltc_available = !state.latest.ltc_is_detecting
        && (state.latest.ltc_decode_result.is_some() || state.latest.ltc_decode_error.is_some());
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.add_enabled(ltc_available, egui::Checkbox::new(
            &mut state.trim_ltc_start,
            "Cut and Set Start Time to First LTC Frame",
        ));
        if state.trim_ltc_start && state.trim_offset_secs > 0.001 {
            ui.label(
                RichText::new(format!("(trim {:.3}s of silence, set timecode from LTC)", state.trim_offset_secs))
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

fn selected_input_files(state: &AppState) -> Vec<PathBuf> {
    state.selected_group_idx
        .and_then(|idx| state.file_groups.as_ref()?.get(idx))
        .map(|g| g.files.clone())
        .unwrap_or_default()
}

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

    let caps_opt = state.ffmpeg_caps.lock().unwrap().clone();

    let readiness = evaluate_readiness(
        state.selected_group_idx.is_some(),
        state.filename_prefix.is_empty(),
        state.output_folder.as_os_str().is_empty(),
        caps_opt.as_ref(),
    );
    let can_convert = readiness.can_convert;

    let sanity_ok = if can_convert {
        let caps = caps_opt.as_ref().unwrap();
        let input_files = selected_input_files(state);
        conversion_sanity_check_with_naming(
            &state.container,
            &state.video_encoder,
            &state.audio_encoder,
            &input_files,
            &state.output_folder,
            &state.filename_prefix,
            caps,
            Some(&state.audio_suffix_template),
            Some(&state.video_suffix_template),
            Some(&state.naming_mode),
            copy_mode_active(state),
        )
        .is_ok()
    } else {
        false
    };

    let button_label = match state.recording_type {
        RecordingType::MultiTrackAudio => if state.generate_synthetic_video {
            "CONVERT WITH SYNTHETIC VIDEO"
        } else {
            "CONVERT AUDIO FILES"
        },
        RecordingType::VideoClipSequence => if copy_mode_active(state) {
            "CONVERT VIDEO CLIPS (STREAM COPY)"
        } else {
            "CONVERT VIDEO CLIPS"
        },
    };

    ui.add_enabled_ui(can_convert && sanity_ok, |ui| {
        if ui
            .add(
                egui::Button::new(
                    RichText::new(button_label)
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
        ui.label(
            RichText::new(format_blockers(&readiness.blockers))
                .font(FontId::proportional(10.0))
                .color(colors.text_secondary),
        );
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
    let input_files = selected_input_files(state);
    let num_files = input_files.len();

    let trim_secs = if state.trim_ltc_start { state.trim_offset_secs } else { 0.0 };

    // Per-file timecode metadata (same trim offset for all files)
    let trim_offsets: Vec<f64> = if num_files > 0 && state.trim_ltc_start {
        vec![trim_secs; num_files]
    } else {
        vec![0.0; num_files]
    };

    // Per-file timecode metadata from LTC decode result
    let timecode_meta_per_file: Vec<Option<TimecodeMetadata>> = if state.trim_ltc_start && trim_secs > 0.001 {
        let ltc_result = state.latest.ltc_decode_result.as_ref();
        (0..num_files).map(|_| {
            ltc_result.and_then(|r| {
                if !matches!(r.status, LtcDecodeStatus::Success | LtcDecodeStatus::LowConfidence) {
                    return None;
                }
                find_timecode_at_offset(&r.timecodes, trim_secs).map(|tc| TimecodeMetadata {
                    start: tc,
                    fps: r.detected_fps as f64,
                    drop_frame: r.drop_frame,
                })
            })
        }).collect()
    } else {
        vec![None; num_files]
    };

    let pipeline = match state.recording_type {
        RecordingType::MultiTrackAudio => {
            ConversionPipeline::AudioOnly { generate_synthetic_video: state.generate_synthetic_video }
        }
        RecordingType::VideoClipSequence => ConversionPipeline::VideoPassthrough,
    };

    let ltc_video_source = match state.recording_type {
        RecordingType::VideoClipSequence => {
            Some((state.latest.ltc_selected_stream, state.latest.ltc_selected_channel))
        }
        RecordingType::MultiTrackAudio => None,
    };

    let settings = ConverterSettings {
        pipeline,
        input_files,
        recording_type: state.recording_type.clone(),
        ltc_track_channel_index: state.ltc_file_idx,
        channel_map: state.channel_map.clone(),
        split_tracks: state.split_tracks,
        drop_ltc_track: state.drop_ltc_track,
        concat_audio: state.concat_audio,
        ltc_video_source,
        container: state.container.clone(),
        copy_video: copy_mode_active(state),
        video_encoder: state.video_encoder.clone(),
        audio_encoder: state.audio_encoder.clone(),
        resolved_video_encoder: String::new(),
        output_folder: state.output_folder.clone(),
        filename_prefix: state.filename_prefix.clone(),
        audio_suffix_template: state.audio_suffix_template.clone(),
        video_suffix_template: state.video_suffix_template.clone(),
        naming_mode: state.naming_mode.clone(),
        trim_to_first_ltc: state.trim_ltc_start,
        trim_offsets_secs: trim_offsets,
        timecode_meta_per_file,
        resolved_hw_device: None,
    };

    *state.conversion_state.lock().unwrap() = ConversionState::idle();
    state.cancel_flag.store(false, Ordering::Relaxed);

    let cs = state.conversion_state.clone();
    let cf = state.cancel_flag.clone();
    let caps = state.ffmpeg_caps.lock().unwrap().clone();
    state.convert_handle = Some(spawn_conversion(settings, cs, cf, caps.as_ref()));
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
                RichText::new(format!("Files saved to: {}/{}", state.output_folder.display(), state.filename_prefix))
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