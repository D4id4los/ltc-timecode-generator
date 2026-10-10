use crate::text::{text, TextStyle};
use std::path::PathBuf;
use std::time::Duration;

use egui::{Color32, Ui};
use gui_engine::command::{ConverterCommand, GuiCommand};
use gui_engine::converter::{
    available_audio_encoders_for_container, available_containers, conversion_sanity_check,
    conversion_sanity_check_metadata_only, conversion_sanity_check_metadata_only_pure,
    conversion_sanity_check_pure, evaluate_readiness, format_blockers, preview_output_files,
    start_timecode_from_ltc, supported_audio_encoders, supported_containers, ChannelMap,
    ConversionCheckError, ConversionPipeline, ConverterSettings, OutputKind, RecordingType,
    SanityCheckInput,
};
use gui_engine::duration::{format_duration_secs, group_duration_secs};
use gui_engine::file_pattern::{group_display_key, MatchedGroup};
use gui_engine::timecode::{self, FPS_OPTIONS};
use gui_engine::video_codecs::{available_video_codecs, supported_video_codecs};
use gui_engine::{JobKind, JobPhase, ProbeStatusLabel};

use super::bound;
use crate::app::AppState;
use crate::theme::{ThemeColors, ACCENT};
use crate::widgets::style;

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    // Pre-compute sanity result once per frame (pure — no filesystem access)
    let caps_opt = state.latest.ffmpeg_caps.clone();
    let sanity_result: Option<Result<(), ConversionCheckError>> =
        caps_opt.as_ref().and_then(|caps| {
            let conv = &state.sh.conv;
            let metadata_only = *conv.metadata_only.value();
            let output_folder = conv.output_folder.value().clone();
            let filename_prefix = conv.filename_prefix.value().clone();
            let audio_suffix = conv.audio_suffix_template.value().clone();
            let video_suffix = conv.video_suffix_template.value().clone();
            if metadata_only {
                let input_files = selected_input_files(state);
                Some(conversion_sanity_check_metadata_only_pure(
                    &input_files,
                    &output_folder,
                    &filename_prefix,
                    caps,
                    Some(&audio_suffix),
                    Some(&video_suffix),
                ))
            } else if state.latest.converter.selected_group_idx.is_some() {
                let input_files = selected_input_files(state);
                Some(conversion_sanity_check_pure(SanityCheckInput {
                    container: conv.container.value(),
                    video_codec: conv.video_encoder.value(),
                    audio_encoder: conv.audio_encoder.value(),
                    input_files: &input_files,
                    output_folder: &output_folder,
                    filename_prefix: &filename_prefix,
                    caps,
                    audio_suffix: Some(&audio_suffix),
                    video_suffix: Some(&video_suffix),
                    copy_video: copy_mode_active(state),
                }))
            } else {
                None
            }
        });

    style::content_area(ui, &colors, |ui| {
        step_header(ui, "1", "SELECT FILES", &colors);
        ui.add_space(8.0);
        render_file_selection(ui, state);
        ui.add_space(16.0);

        if state.latest.converter.selected_group_idx.is_some() {
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
        render_output_format(ui, state, sanity_result.as_ref());
        ui.add_space(16.0);

        step_header(ui, "4", "OUTPUT FILE", &colors);
        ui.add_space(8.0);
        render_output_path(ui, state);
        ui.add_space(16.0);

        render_convert_button(ui, state, sanity_result.as_ref());
        ui.add_space(12.0);

        render_conversion_progress(ui, state);
    });
}

fn step_header(ui: &mut Ui, number: &str, label: &str, colors: &crate::theme::ThemeColors) {
    style::step_header(ui, colors, number, label);
}

/// True when "Leave Video Encoding Untouched" applies. Stream copy is only
/// meaningful for the video pipeline (audio-only recordings have no video
/// stream to copy, and synthetic video must be encoded).
fn copy_mode_active(state: &AppState) -> bool {
    !*state.sh.conv.metadata_only.value()
        && *state.sh.conv.copy_video.value()
        && state.latest.converter.selected_recording_type()
            == Some(RecordingType::VideoClipSequence)
}

pub(crate) fn apply_group_selection(state: &mut AppState, groups: &[MatchedGroup], idx: usize) {
    let group = &groups[idx];

    log::info!(
        "Recording selected in UI: idx={}, type={:?}, {} file(s)",
        idx,
        group.recording_type,
        group.files.len(),
    );

    // The engine resets ltc_file_idx (and other settings) when it applies the
    // recording selection — mirror that into the shadow immediately so the
    // combo never flashes the previous recording's track index.
    state.sh.conv.ltc_file_idx.force_adopt(&0);
    state.last_logged_group_decode_gen = 0;

    state.send(GuiCommand::ClearRecordingDecodeState);
    state.send(GuiCommand::Converter(ConverterCommand::SelectRecording(
        idx,
    )));
}

// ── Pure helpers (unit-tested at the bottom of this file) ──────────────

/// Display label for the folder row of the file-selection step.
fn folder_display_label(folder: Option<&std::path::Path>) -> String {
    match folder {
        Some(p) => p.to_string_lossy().to_string(),
        None => String::from("(No folder selected)"),
    }
}

/// Combo-item label for one matched recording group: display key, type badge,
/// file count/detail and aggregated duration.
fn group_combo_label(group: &MatchedGroup, durations: &[Option<f64>]) -> String {
    let type_badge = match group.recording_type {
        RecordingType::MultiTrackAudio => "🎵 AUDIO",
        RecordingType::VideoClipSequence => "🎬 VIDEO",
    };
    let detail = group
        .files
        .iter()
        .map(|f| f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
        .collect::<Vec<_>>()
        .join(", ");
    let dur_text = group_duration_secs(&group.recording_type, durations)
        .map(|s| format!("  ·  {}", format_duration_secs(s)))
        .unwrap_or_default();
    let display_key = group_display_key(&group.prefix, &group.rel_dir);
    format!(
        "{}  [{}]  ({} file{}: {}){}",
        display_key,
        type_badge,
        group.files.len(),
        if group.files.len() == 1 { "" } else { "s" },
        detail,
        dur_text,
    )
}

/// `Some(last)` when `idx` is out of range for `len` entries (and `len` is
/// non-empty), `None` when no clamp is needed.
fn clamped_ltc_file_idx(idx: usize, len: usize) -> Option<usize> {
    if idx >= len && len > 0 {
        Some(len - 1)
    } else {
        None
    }
}

/// Row index of the selected LTC stream/channel within the probe's flat
/// (stream, channel) enumeration, or `None` when not found.
fn ltc_row_index(
    probe: &gui_engine::VideoAudioProbe,
    stream: usize,
    channel: usize,
) -> Option<usize> {
    let mut idx = 0usize;
    for s in &probe.streams {
        for ch in 0..s.channels {
            if s.stream_index == stream && ch == channel {
                return Some(idx);
            }
            idx += 1;
        }
    }
    None
}

// ── Step 1: File selection ─────────────────────────────────────────────

fn render_file_selection(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    // Folder picker (all patterns are applied simultaneously)
    render_folder_row(ui, state, &colors);

    // File group selector (shows all matched groups with type badge)
    let groups_loading = state.latest.job(JobKind::FolderScan).is_active();
    let groups_clone = if groups_loading {
        None
    } else {
        Some(state.latest.converter.groups.clone())
    };
    if groups_clone.is_none() && state.latest.converter.groups_folder.is_some() {
        // Groups async scan in progress or not yet adopted
        if groups_loading {
            ui.label(text(ui, "Scanning folder for recordings…").style(TextStyle::Label, &colors));
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(200));
        } else {
            ui.label(
                text(
                    ui,
                    "No files matching any known pattern were found in this folder.",
                )
                .style(TextStyle::Hint, &colors)
                .color(colors.error_red),
            );
        }
    } else if let Some(ref groups) = groups_clone {
        if groups.is_empty() {
            ui.label(
                text(
                    ui,
                    "No files matching any known pattern were found in this folder.",
                )
                .style(TextStyle::Hint, &colors)
                .color(colors.error_red),
            );
        } else {
            render_group_combo(ui, state, groups, &colors);

            if let Some(idx) = state.latest.converter.selected_group_idx {
                if let Some(group) = groups.get(idx) {
                    render_selected_group_pills(ui, group, &colors);
                }
            }
        }
    }
}

/// Folder picker row: read-only display of the scanned folder + Browse dialog.
fn render_folder_row(ui: &mut Ui, state: &mut AppState, colors: &crate::theme::ThemeColors) {
    ui.horizontal(|ui| {
        ui.label(text(ui, "Folder:").style(TextStyle::Label, colors));
        let folder_label = folder_display_label(state.latest.converter.groups_folder.as_deref());
        let mut display = folder_label;
        ui.add_sized(
            egui::vec2(ui.available_width() - 90.0, 20.0),
            egui::TextEdit::singleline(&mut display)
                .font(crate::text::TextStyle::MonoLabel.font_spec().font_id())
                .interactive(false),
        );
        if ui.button("Browse…").clicked() {
            let mut dialog = rfd::FileDialog::new();
            if let Some(ref last) = state.latest.converter.groups_folder {
                dialog = dialog.set_directory(last);
            }
            let folder = dialog.pick_folder();
            if let Some(path) = folder {
                // Notify engine to scan folder and manage groups (engine runs
                // the scan on a background thread and publishes results via
                // the snapshot's converter.groups / groups_loading flags).
                state.send(GuiCommand::Converter(ConverterCommand::SelectFolder(path)));
                // Auto-select first recording (deferred by engine until scan
                // completes, then applied race-free).
                state.send(GuiCommand::Converter(ConverterCommand::SelectRecording(0)));

                bound::set_value(
                    state,
                    |s| &mut s.sh.conv.set_start_from_ltc,
                    false,
                    |v| GuiCommand::Converter(ConverterCommand::SetStartFromLtc(v)),
                );
            }
        }
    });
}

/// Recording combo: one entry per matched group (display key, type badge,
/// file detail, aggregated duration).
fn render_group_combo(
    ui: &mut Ui,
    state: &mut AppState,
    groups: &[MatchedGroup],
    colors: &crate::theme::ThemeColors,
) {
    ui.horizontal(|ui| {
        ui.label(text(ui, "Recording:").style(TextStyle::Label, colors));
        let selected_text = state
            .latest
            .converter
            .selected_group_idx
            .and_then(|idx| groups.get(idx))
            .map(|g| group_display_key(&g.prefix, &g.rel_dir))
            .unwrap_or_else(|| "Select a recording…".to_string());
        egui::ComboBox::from_id_salt(format!("group_combo_{}", groups.len()))
            .selected_text(selected_text)
            .show_ui(ui, |ui| {
                for (i, group) in groups.iter().enumerate() {
                    let durs: Vec<Option<f64>> = group
                        .files
                        .iter()
                        .map(|f| state.latest.file_durations.get(f).copied().flatten())
                        .collect();
                    let label = group_combo_label(group, &durs);
                    let is_sel = state.latest.converter.selected_group_idx == Some(i);
                    if ui.selectable_label(is_sel, label).clicked() {
                        apply_group_selection(state, groups, i);
                    }
                }
            });
    });
}

/// Pills listing the files of the selected recording group.
fn render_selected_group_pills(
    ui: &mut Ui,
    group: &MatchedGroup,
    colors: &crate::theme::ThemeColors,
) {
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
                    ui.label(text(ui, name).style(TextStyle::MonoValue, colors));
                });
            }
        });
    });
}

// ── LTC Verification ─────────────────────────────────────────────────

/// One selectable entry of the LTC track-source combo (video: stream/channel
/// pair, audio: one entry per file).
#[derive(Clone, Debug, PartialEq, Eq)]
struct ChannelOption {
    stream: usize,
    channel: usize,
    label: String,
    disabled: bool,
}

/// Build the track-source combo entries: from the probe (video) or the file
/// list (audio). `clip_probe_active` yields the "Probing…" placeholder.
fn build_channel_options(
    is_video: bool,
    probe: Option<&gui_engine::VideoAudioProbe>,
    clip_probe_active: bool,
    files: &[PathBuf],
) -> Vec<ChannelOption> {
    if is_video {
        if let Some(probe) = probe {
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
                            disabled: false,
                        }
                    })
                })
                .collect()
        } else if clip_probe_active {
            vec![ChannelOption {
                stream: 0,
                channel: 0,
                label: "Probing…".to_string(),
                disabled: true,
            }]
        } else {
            vec![]
        }
    } else {
        files
            .iter()
            .enumerate()
            .map(|(i, f)| ChannelOption {
                stream: i,
                channel: 0,
                label: f
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("?")
                    .to_string(),
                disabled: false,
            })
            .collect()
    }
}

/// True when `(stream, channel)` is not among `options` and there is at least
/// one option to reset to.
fn needs_decode_selection_reset(options: &[ChannelOption], sel: (usize, usize)) -> bool {
    !options.is_empty() && !options.iter().any(|o| (o.stream, o.channel) == sel)
}

/// One per-clip row of the group decode results panel.
struct GroupResultRow {
    name: String,
    result: Option<gui_engine::LtcDetectionResult>,
    error: Option<String>,
}

/// Zip group decode paths with their per-clip decode states.
fn collect_group_results(
    paths: &[PathBuf],
    results: &[gui_engine::state::ClipDecodeState],
) -> Vec<GroupResultRow> {
    paths
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let name = p
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("?")
                .to_string();
            match results.get(i) {
                Some(gui_engine::state::ClipDecodeState::Done(Ok(r))) => {
                    (name, Some((**r).clone()), None)
                }
                Some(gui_engine::state::ClipDecodeState::Done(Err(e))) => {
                    (name, None, Some(e.clone()))
                }
                _ => (name, None, None),
            }
        })
        .map(|(name, result, error)| GroupResultRow {
            name,
            result,
            error,
        })
        .collect()
}

/// (icon, color) badge for a decode status.
fn status_badge(
    status: &gui_engine::LtcDecodeStatus,
    colors: &crate::theme::ThemeColors,
) -> (&'static str, Color32) {
    match status {
        gui_engine::LtcDecodeStatus::Success => ("✅", colors.success_green),
        gui_engine::LtcDecodeStatus::LowConfidence => ("⚠️", colors.warning_amber),
        gui_engine::LtcDecodeStatus::NoSyncWord => ("❌", colors.error_red),
        gui_engine::LtcDecodeStatus::Error { .. } => ("❌", colors.error_red),
    }
}

fn render_ltc_verification(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    ui.label(
        text(ui, "Select the track that carries the LTC timecode signal, then click \"Detect LTC\" to verify it can be read successfully.")
            .style(TextStyle::Description, &colors)
            .color(colors.text_secondary),
    );
    ui.add_space(6.0);

    let group = state
        .latest
        .converter
        .selected_group_idx
        .and_then(|idx| state.latest.converter.groups.get(idx))
        .cloned();

    let file_count = group.as_ref().map(|g| g.files.len()).unwrap_or(0);

    if file_count > 0 {
        // Clamp out-of-range track index to the last file.
        if let Some(clamped) = clamped_ltc_file_idx(*state.sh.conv.ltc_file_idx.value(), file_count)
        {
            bound::select_value(
                state,
                |s| &mut s.sh.conv.ltc_file_idx,
                state.latest.converter.settings.ltc_file_idx,
                clamped,
                |v| GuiCommand::Converter(ConverterCommand::SetLtcFileIndex(v)),
            );
        }

        let is_video = state.latest.converter.selected_recording_type()
            == Some(RecordingType::VideoClipSequence);

        // Build channel options from probe (for video) or file list (for audio)
        let channel_options: Vec<ChannelOption> = build_channel_options(
            is_video,
            state.latest.decode.probe.as_ref(),
            state.latest.job(JobKind::ClipProbe).is_active(),
            group.as_ref().map(|g| g.files.as_slice()).unwrap_or(&[]),
        );

        // Ensure selected stream/channel is within range. A single
        // corrective send when out of range — the shadow protects the
        // correction from re-sending while it awaits the engine echo.
        if is_video {
            let sel = (
                *state.sh.decode_stream.value(),
                *state.sh.decode_channel.value(),
            );
            bound::sync(
                state,
                |s| &mut s.sh.decode_stream,
                state.latest.decode.selected_stream,
            );
            bound::sync(
                state,
                |s| &mut s.sh.decode_channel,
                state.latest.decode.selected_channel,
            );
            if needs_decode_selection_reset(&channel_options, sel) {
                bound::set_value(
                    state,
                    |s| &mut s.sh.decode_stream,
                    channel_options[0].stream,
                    GuiCommand::SetLtcDecodeStream,
                );
                bound::set_value(
                    state,
                    |s| &mut s.sh.decode_channel,
                    channel_options[0].channel,
                    GuiCommand::SetLtcDecodeChannel,
                );
            }
        } else if let Some(clamped) =
            clamped_ltc_file_idx(*state.sh.conv.ltc_file_idx.value(), channel_options.len())
        {
            bound::select_value(
                state,
                |s| &mut s.sh.conv.ltc_file_idx,
                state.latest.converter.settings.ltc_file_idx,
                clamped,
                |v| GuiCommand::Converter(ConverterCommand::SetLtcFileIndex(v)),
            );
        }

        // ── Row 1: Track/channel selection ──
        render_track_source_row(ui, state, is_video, &channel_options, &colors);

        ui.add_space(6.0);

        // ── Row 2: Decode FPS selector + Detect button ──
        render_fps_detect_row(ui, state, is_video, group.as_ref(), &colors);
    }

    ui.add_space(4.0);

    // Show group decode results (video clip groups)
    let is_video_group =
        state.latest.converter.selected_recording_type() == Some(RecordingType::VideoClipSequence);
    let group_results: Vec<GroupResultRow> = collect_group_results(
        &state.latest.decode.group_paths,
        &state.latest.decode.group_results,
    );

    if is_video_group && !group_results.is_empty() {
        log_group_decode_view(state, &group_results);
        // Per-clip expandable LTC detection results
        render_group_decode_results(ui, state, &group_results, &colors);
    } else {
        // Show single-file decode result (audio-only, or single-file video)
        render_single_decode_result(ui, state, &colors);
    }
}

/// Row 1 of the LTC verification panel: track/channel source combo.
fn render_track_source_row(
    ui: &mut Ui,
    state: &mut AppState,
    is_video: bool,
    channel_options: &[ChannelOption],
    colors: &crate::theme::ThemeColors,
) {
    ui.horizontal(|ui| {
        ui.label(text(ui, "Source:").style(TextStyle::Label, colors));

        let current_label = if is_video {
            let sel = (
                *state.sh.decode_stream.value(),
                *state.sh.decode_channel.value(),
            );
            channel_options
                .iter()
                .find(|o| (o.stream, o.channel) == sel)
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "Select…".to_string())
        } else {
            channel_options
                .get(*state.sh.conv.ltc_file_idx.value())
                .map(|o| o.label.clone())
                .unwrap_or_else(|| "Select…".to_string())
        };

        let combo_salt = format!(
            "ltc_file_combo_{}_{}",
            if is_video { "video" } else { "audio" },
            channel_options.len(),
        );
        egui::ComboBox::from_id_salt(combo_salt)
            .selected_text(&current_label)
            .width(ui.available_width())
            .show_ui(ui, |ui| {
                if is_video {
                    let sel = (
                        *state.sh.decode_stream.value(),
                        *state.sh.decode_channel.value(),
                    );
                    for opt in channel_options {
                        let is_sel = (opt.stream, opt.channel) == sel;
                        if ui.selectable_label(is_sel, &opt.label).clicked() && !opt.disabled {
                            bound::set_value(
                                state,
                                |s| &mut s.sh.decode_stream,
                                opt.stream,
                                GuiCommand::SetLtcDecodeStream,
                            );
                            bound::set_value(
                                state,
                                |s| &mut s.sh.decode_channel,
                                opt.channel,
                                GuiCommand::SetLtcDecodeChannel,
                            );
                        }
                    }
                } else {
                    let cur = *state.sh.conv.ltc_file_idx.value();
                    for (i, opt) in channel_options.iter().enumerate() {
                        if ui.selectable_label(i == cur, &opt.label).clicked() {
                            bound::select_value(
                                state,
                                |s| &mut s.sh.conv.ltc_file_idx,
                                state.latest.converter.settings.ltc_file_idx,
                                i,
                                |v| GuiCommand::Converter(ConverterCommand::SetLtcFileIndex(v)),
                            );
                        }
                    }
                }
            });
    });
}

/// Row 2 of the LTC verification panel: decode FPS selector and the
/// Detect/Cancel button (with progress bar while detecting).
fn render_fps_detect_row(
    ui: &mut Ui,
    state: &mut AppState,
    is_video: bool,
    group: Option<&MatchedGroup>,
    colors: &crate::theme::ThemeColors,
) {
    let decode_fps_truth = state.latest.decode.fps_index;
    bound::sync(state, |s| &mut s.sh.decode_fps_index, decode_fps_truth);
    let decode_fps_sel = *state.sh.decode_fps_index.value();
    ui.horizontal(|ui| {
        ui.label(text(ui, "FPS:").style(TextStyle::Label, colors));
        for (i, opt) in FPS_OPTIONS.iter().enumerate() {
            let is_sel = i == decode_fps_sel;
            if style::option_chip(
                ui,
                colors,
                opt.name,
                crate::text::TextStyle::MonoValue.font_spec(),
                is_sel,
                true,
                egui::vec2(0.0, 22.0),
            )
            .clicked()
            {
                bound::select_value(
                    state,
                    |s| &mut s.sh.decode_fps_index,
                    decode_fps_truth,
                    i,
                    GuiCommand::SetDecodeFpsIndex,
                );
            }
        }

        ui.add_space(8.0);

        let is_detecting = state.latest.job(JobKind::LtcDecode).is_active();
        let is_group_detecting = state.latest.job(JobKind::LtcGroupDecode).is_active();
        let any_detecting = is_detecting || is_group_detecting;

        if any_detecting {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
            // Read the fraction from the job that is actually running: a
            // group detect (LtcGroupDecode) must not render the idle/stale
            // single-decode job's fraction (and vice versa).
            let active_kind = if is_detecting {
                JobKind::LtcDecode
            } else {
                JobKind::LtcGroupDecode
            };
            let active_job = state.latest.job(active_kind);
            let progress_pct = active_job.fraction();
            let progress_str = active_job.message().to_string();
            ui.add(
                egui::ProgressBar::new(progress_pct)
                    .show_percentage()
                    .desired_width(140.0),
            );
            ui.add_space(2.0);
            ui.label(text(ui, &progress_str).style(TextStyle::MonoValue, colors));
            ui.add_space(4.0);
            if style::action_button(
                ui,
                colors,
                style::ActionStyle::Danger,
                "✕ CANCEL",
                crate::text::TextStyle::Label.font_spec(),
                egui::vec2(80.0, 22.0),
                true,
            )
            .clicked()
            {
                state.send(GuiCommand::CancelDecode);
            }
        } else if is_video {
            let stream_idx = state.latest.decode.selected_stream;
            let channel_idx = state.latest.decode.selected_channel;
            if style::action_button(
                ui,
                colors,
                style::ActionStyle::Primary,
                "🔍 Detect LTC All Clips",
                crate::text::TextStyle::Control.font_spec(),
                egui::vec2(140.0, 24.0),
                true,
            )
            .clicked()
            {
                let paths: Vec<String> = group
                    .unwrap()
                    .files
                    .iter()
                    .map(|f| f.to_string_lossy().to_string())
                    .collect();
                state.send(GuiCommand::DecodeLtcVideoGroup {
                    paths,
                    stream_index: stream_idx,
                    channel_index: channel_idx,
                });
            }
        } else {
            let file_path = group.unwrap().files[*state.sh.conv.ltc_file_idx.value()]
                .to_string_lossy()
                .to_string();
            if style::action_button(
                ui,
                colors,
                style::ActionStyle::Primary,
                "🔍 Detect LTC",
                crate::text::TextStyle::Control.font_spec(),
                egui::vec2(100.0, 24.0),
                true,
            )
            .clicked()
            {
                state.send(GuiCommand::ParseLtcWavFile(file_path));
            }
        }
    });
}

/// Change-gated diagnostics log of the group decode view (Windows triage).
fn log_group_decode_view(state: &mut AppState, group_results: &[GroupResultRow]) {
    let gen = state.latest.decode.group_generation;
    if gen == state.last_logged_group_decode_gen {
        return;
    }
    state.last_logged_group_decode_gen = gen;
    let some_count = group_results.iter().filter(|r| r.result.is_some()).count();
    let err_count = group_results.iter().filter(|r| r.error.is_some()).count();
    let none_count = group_results
        .iter()
        .filter(|r| r.result.is_none() && r.error.is_none())
        .count();
    log::info!(
        "GROUP VIEW: paths={} results={} errors={} detecting={} done/total={}/{} gen={} none={} recording={:?}",
        group_results.len(), some_count, err_count,
        state.latest.job(JobKind::LtcGroupDecode).is_active(),
        state.latest.job(JobKind::LtcGroupDecode).units().len(), state.latest.decode.group_results.len(), gen,
        none_count,
        state.latest.converter.selected_recording_type(),
    );
}

/// Per-clip expandable LTC detection results (video clip groups).
fn render_group_decode_results(
    ui: &mut Ui,
    state: &mut AppState,
    group_results: &[GroupResultRow],
    colors: &crate::theme::ThemeColors,
) {
    let scroll_frame = egui::Frame::new()
        .fill(colors.nested_bg)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(0.5, colors.border_main))
        .inner_margin(egui::Margin::symmetric(4, 2));
    scroll_frame.show(ui, |ui| {
        egui::ScrollArea::vertical()
            .id_salt(crate::ids::ltc_group_results_scroll())
            .max_height(260.0)
            .show(ui, |ui| {
                for (i, row) in group_results.iter().enumerate() {
                    let (name, result_opt, error_opt) = (&row.name, &row.result, &row.error);
                    match (result_opt, error_opt) {
                        (Some(result), _) => {
                            let (header_icon, header_color) = status_badge(&result.status, colors);
                            let summary = format_clip_ltc_summary(result);
                            let salt = format!("clip_{}", i);
                            egui::collapsing_header::CollapsingState::load_with_default_open(
                                ui.ctx(),
                                egui::Id::new(format!("ltc_clip_{}", i)),
                                false,
                            )
                            .show_header(ui, |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        text(ui, format!("{} {}", header_icon, name))
                                            .style(TextStyle::Status, colors)
                                            .color(header_color),
                                    );
                                    ui.label(
                                        text(ui, format!("  {}", summary))
                                            .style(TextStyle::Description, colors),
                                    );
                                });
                            })
                            .body(|ui| {
                                render_ltc_result(ui, state, result, &salt);
                            });
                        }
                        (None, Some(e)) => {
                            let pill_frame = egui::Frame::new()
                                .fill(Color32::from_rgb(0x44, 0x11, 0x11))
                                .corner_radius(4.0)
                                .stroke(egui::Stroke::new(0.5, colors.border_main))
                                .inner_margin(egui::Margin::symmetric(6, 3));
                            pill_frame.show(ui, |ui| {
                                ui.label(
                                    text(ui, format!("❌ {}: {}", name, e))
                                        .style(TextStyle::Hint, colors)
                                        .color(colors.error_red),
                                );
                            });
                        }
                        (None, None) => {
                            if state.latest.job(JobKind::LtcGroupDecode).is_active() {
                                let pill_frame = egui::Frame::new()
                                    .fill(colors.card_bg)
                                    .corner_radius(4.0)
                                    .stroke(egui::Stroke::new(0.5, colors.border_main))
                                    .inner_margin(egui::Margin::symmetric(6, 3));
                                pill_frame.show(ui, |ui| {
                                    ui.label(
                                        text(ui, format!("⏳ {}: decoding…", name))
                                            .style(TextStyle::Description, colors),
                                    );
                                });
                            } else {
                                let pill_frame = egui::Frame::new()
                                    .fill(colors.card_bg)
                                    .corner_radius(4.0)
                                    .stroke(egui::Stroke::new(0.5, colors.border_main))
                                    .inner_margin(egui::Margin::symmetric(6, 3));
                                pill_frame.show(ui, |ui| {
                                    ui.label(
                                        text(ui, format!("❓ {}: no result", name))
                                            .style(TextStyle::Description, colors),
                                    );
                                });
                            }
                        }
                    }
                    ui.add_space(2.0);
                }
            });
    });
}

/// Single-file decode outcome: error frame and/or the full result panel.
fn render_single_decode_result(
    ui: &mut Ui,
    state: &mut AppState,
    colors: &crate::theme::ThemeColors,
) {
    let decode_result = state.latest.decode.result.clone();
    let decode_error = state.latest.decode.error.clone();

    if let Some(ref error) = decode_error {
        let error_frame = egui::Frame::new()
            .fill(Color32::from_rgb(0x44, 0x11, 0x11))
            .corner_radius(6.0)
            .stroke(egui::Stroke::new(1.0, colors.error_red))
            .inner_margin(egui::Margin::symmetric(10, 6));
        error_frame.show(ui, |ui| {
            ui.label(
                text(ui, format!("❌ {}", error))
                    .style(TextStyle::Hint, colors)
                    .color(colors.error_red),
            );
        });
    }

    if let Some(result) = decode_result {
        render_ltc_result(ui, state, &result, "single");
    }
}

fn format_clip_ltc_summary(result: &gui_engine::LtcDetectionResult) -> String {
    let drop_flag = if result.drop_frame { " DF" } else { "" };
    let fps_str = if result.detected_fps > 0.0 {
        format!("{:.2}{drop_flag} fps", result.detected_fps)
    } else {
        "—".to_string()
    };
    match &result.status {
        gui_engine::LtcDecodeStatus::Success | gui_engine::LtcDecodeStatus::LowConfidence => {
            let tc = result
                .timecodes
                .first()
                .map(|ftc| {
                    let sep = if result.drop_frame { ";" } else { ":" };
                    format!(
                        "{:02}{sep}{:02}{sep}{:02}{sep}{:02}",
                        ftc.timecode.hours,
                        ftc.timecode.minutes,
                        ftc.timecode.seconds,
                        ftc.timecode.frames
                    )
                })
                .unwrap_or_else(|| "—".to_string());
            format!(
                "{} · {} · trim {:.3}s",
                tc, fps_str, result.first_ltc_timecode_secs
            )
        }
        gui_engine::LtcDecodeStatus::NoSyncWord => "No LTC found".to_string(),
        gui_engine::LtcDecodeStatus::Error { message } => format!("Error: {}", message),
    }
}

/// "29.97 fps (Drop Frame)" style display of the detected rate.
fn fps_display(detected_fps: f32, drop_frame: bool) -> String {
    let drop_flag = if drop_frame { " (Drop Frame)" } else { "" };
    if detected_fps > 0.0 {
        format!("{:.2} fps{}", detected_fps, drop_flag)
    } else {
        "—".to_string()
    }
}

/// "HH:MM:SS:FF → HH:MM:SS:FF" summary of the decoded timecode range.
fn tc_range_summary(
    first: Option<&gui_engine::FrameTimecode>,
    last: Option<&gui_engine::FrameTimecode>,
    drop_frame: bool,
) -> String {
    match (first, last) {
        (Some(f), Some(l)) => {
            let sep = if drop_frame { ";" } else { ":" };
            format!(
                "{:02}{sep}{:02}{sep}{:02}{sep}{:02} → {:02}{sep}{:02}{sep}{:02}{sep}{:02}",
                f.timecode.hours,
                f.timecode.minutes,
                f.timecode.seconds,
                f.timecode.frames,
                l.timecode.hours,
                l.timecode.minutes,
                l.timecode.seconds,
                l.timecode.frames,
            )
        }
        _ => "—".to_string(),
    }
}

/// (text color, background color) for the quality-grade badge at `score`.
fn quality_grade_colors(score: f64, colors: &crate::theme::ThemeColors) -> (Color32, Color32) {
    if score >= 0.95 {
        (
            colors.success_green,
            colors.success_green.linear_multiply(0.12),
        )
    } else if score >= 0.80 {
        (
            colors.success_green,
            colors.success_green.linear_multiply(0.08),
        )
    } else if score >= 0.60 {
        (
            colors.warning_amber,
            colors.warning_amber.linear_multiply(0.10),
        )
    } else if score >= 0.30 {
        (colors.error_red, colors.error_red.linear_multiply(0.10))
    } else {
        (colors.error_red, colors.error_red.linear_multiply(0.15))
    }
}

/// One-line issue summary for the quality badge ("N edit(s)", gaps/glitches,
/// missing frames, or "perfect").
fn quality_issues_text(
    edit_count: u32,
    glitch_count: u32,
    gap_count: u32,
    missing_frames: u32,
) -> String {
    if edit_count > 0 {
        format!("{} edit(s)", edit_count)
    } else if glitch_count > 0 || gap_count > 0 {
        format!("{} gap(s), {} glitch(es)", gap_count, glitch_count)
    } else if missing_frames > 0 {
        format!("{} missing", missing_frames)
    } else {
        "perfect".to_string()
    }
}

fn render_ltc_result(
    ui: &mut Ui,
    state: &mut AppState,
    result: &gui_engine::LtcDetectionResult,
    id_salt: &str,
) {
    let colors = state.theme.colors();

    let (status_icon, status_color, status_text) = match &result.status {
        gui_engine::LtcDecodeStatus::Success => {
            ("✅", colors.success_green, "LTC detected successfully")
        }
        gui_engine::LtcDecodeStatus::LowConfidence => (
            "⚠️",
            colors.warning_amber,
            "LTC detected with low confidence",
        ),
        gui_engine::LtcDecodeStatus::NoSyncWord => {
            ("❌", colors.error_red, "No LTC timecode found")
        }
        gui_engine::LtcDecodeStatus::Error { message } => {
            let error_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x44, 0x11, 0x11))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.error_red))
                .inner_margin(egui::Margin::symmetric(10, 6));
            error_frame.show(ui, |ui| {
                ui.label(
                    text(ui, format!("❌ Detection error: {}", message))
                        .style(TextStyle::Hint, &colors)
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
                text(ui, format!("{} {}", status_icon, status_text))
                    .style(TextStyle::Status, &colors)
                    .color(status_color),
            );
            ui.add_space(4.0);

            let fps_str = fps_display(result.detected_fps, result.drop_frame);

            let first_tc = result.timecodes.first();
            let last_tc = result.timecodes.last();
            let tc_summary = tc_range_summary(first_tc, last_tc, result.drop_frame);

            render_result_grid(ui, result, &fps_str, &tc_summary, id_salt, &colors);

            // Quality report
            if let Some(ref q) = result.quality {
                ui.add_space(4.0);
                render_quality_badge(ui, q, &colors);
            }

            // Collapsible timecode list
            if !result.timecodes.is_empty() {
                ui.add_space(4.0);
                render_timecode_list(ui, result, id_salt, &colors);
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
                render_debug_details(ui, result, id_salt, &colors);
            }
        });
    });
}

/// The 2-column stats grid of a decoded LTC result.
fn render_result_grid(
    ui: &mut Ui,
    result: &gui_engine::LtcDetectionResult,
    fps_str: &str,
    tc_summary: &str,
    id_salt: &str,
    colors: &crate::theme::ThemeColors,
) {
    let grid = egui::Grid::new(format!("ltc_result_grid_{}", id_salt))
        .num_columns(2)
        .spacing([8.0, 2.0])
        .striped(false);
    grid.show(ui, |ui| {
        ui.label(text(ui, "Detected rate:").style(TextStyle::Label, colors));
        ui.label(
            text(ui, fps_str)
                .style(TextStyle::MonoReadout, colors)
                .color(colors.text_title)
                .bold(),
        );
        ui.end_row();

        ui.label(text(ui, "Confidence:").style(TextStyle::Label, colors));
        ui.label(
            text(ui, format!("{:.1}%", result.avg_confidence * 100.0))
                .style(TextStyle::MonoReadout, colors)
                .color(colors.text_title)
                .bold(),
        );
        ui.end_row();

        ui.label(text(ui, "Valid frames:").style(TextStyle::Label, colors));
        ui.label(
            text(
                ui,
                format!("{} / {}", result.valid_frames, result.total_possible_frames),
            )
            .style(TextStyle::MonoReadout, colors)
            .color(colors.text_title)
            .bold(),
        );
        ui.end_row();

        ui.label(text(ui, "Timecode range:").style(TextStyle::Label, colors));
        ui.label(
            text(ui, tc_summary)
                .style(TextStyle::MonoReadout, colors)
                .color(colors.text_title)
                .bold(),
        );
        ui.end_row();

        ui.label(text(ui, "Sample rate:").style(TextStyle::Label, colors));
        ui.label(
            text(ui, format!("{} Hz", result.sample_rate))
                .style(TextStyle::MonoReadout, colors)
                .color(colors.text_title)
                .bold(),
        );
        ui.end_row();

        ui.label(text(ui, "Audio duration:").style(TextStyle::Label, colors));
        ui.label(
            text(ui, format!("{:.2}s", result.total_audio_duration_secs))
                .style(TextStyle::MonoReadout, colors)
                .color(colors.text_title)
                .bold(),
        );
        ui.end_row();

        ui.label(text(ui, "Processing time:").style(TextStyle::Label, colors));
        ui.label(
            text(ui, format!("{:.1} ms", result.processing_time_ms))
                .style(TextStyle::MonoReadout, colors)
                .color(colors.text_title)
                .bold(),
        );
        ui.end_row();
    });
}

/// Tinted quality-grade badge with score, grade and issue summary.
fn render_quality_badge(
    ui: &mut Ui,
    q: &gui_engine::LtcQualityReport,
    colors: &crate::theme::ThemeColors,
) {
    let (grade_color, grade_bg) = quality_grade_colors(q.score, colors);

    let quality_frame = egui::Frame::new()
        .fill(grade_bg)
        .corner_radius(4.0)
        .stroke(egui::Stroke::new(0.5, grade_color.linear_multiply(0.3)))
        .inner_margin(egui::Margin::symmetric(8, 4));
    quality_frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(text(ui, "Quality:").style(TextStyle::Description, colors));
            ui.label(
                text(ui, format!("{:.0}%", q.score * 100.0))
                    .style(TextStyle::MonoReadout, colors)
                    .bold()
                    .color(grade_color),
            );
            ui.label(
                text(ui, q.grade.as_str())
                    .style(TextStyle::MonoValue, colors)
                    .color(grade_color),
            );
        });
        ui.horizontal(|ui| {
            let issues =
                quality_issues_text(q.edit_count, q.glitch_count, q.gap_count, q.missing_frames);
            ui.label(
                text(
                    ui,
                    format!("{:.1}% usable, {}", q.usable_coverage * 100.0, issues),
                )
                .style(TextStyle::MonoValue, colors),
            );
            if q.max_drift_secs > 0.01 {
                ui.label(
                    text(
                        ui,
                        format!("drift {:.2} frames", q.worst_block_drift_frames),
                    )
                    .style(TextStyle::MonoValue, colors),
                );
            }
        });
    });
}

/// Collapsible scrollable list of all decoded timecodes.
fn render_timecode_list(
    ui: &mut Ui,
    result: &gui_engine::LtcDetectionResult,
    id_salt: &str,
    colors: &crate::theme::ThemeColors,
) {
    egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        egui::Id::new(format!("ltc_timecode_list_{}", id_salt)),
        false,
    )
    .show_header(ui, |ui| {
        ui.label(
            text(
                ui,
                format!("Show {} decoded timecodes", result.timecodes.len()),
            )
            .style(TextStyle::Description, colors),
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
                .id_salt(crate::ids::clip_timecodes_scroll(id_salt))
                .max_height(120.0)
                .show(ui, |ui| {
                    for ftc in &result.timecodes {
                        let sep = if result.drop_frame { ";" } else { ":" };
                        ui.label(
                            text(
                                ui,
                                format!(
                                    "[{:4}] {:02}{sep}{:02}{sep}{:02}{sep}{:02}  (+{:.3}s)",
                                    ftc.frame_index,
                                    ftc.timecode.hours,
                                    ftc.timecode.minutes,
                                    ftc.timecode.seconds,
                                    ftc.timecode.frames,
                                    ftc.timecode_secs,
                                ),
                            )
                            .style(TextStyle::MonoValue, colors),
                        );
                    }
                });
        });
    });
}

/// Collapsible list of decoder debug detail lines.
fn render_debug_details(
    ui: &mut Ui,
    result: &gui_engine::LtcDetectionResult,
    id_salt: &str,
    colors: &crate::theme::ThemeColors,
) {
    egui::collapsing_header::CollapsingState::load_with_default_open(
        ui.ctx(),
        egui::Id::new(format!("ltc_debug_details_{}", id_salt)),
        false,
    )
    .show_header(ui, |ui| {
        ui.label(text(ui, "Show debug details").style(TextStyle::Description, colors));
    })
    .body(|ui| {
        for detail in &result.details {
            ui.label(text(ui, detail).style(TextStyle::MonoValue, colors));
        }
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
    report.push_str(&format!(
        "Audio duration:  {:.2}s\n",
        result.total_audio_duration_secs
    ));
    report.push_str(&format!(
        "Processing time: {:.1}ms\n",
        result.processing_time_ms
    ));

    if let Some(ref q) = result.quality {
        report.push_str(&format!(
            "\nQuality Score:  {:.2} / 1.00 ({})\n",
            q.score, q.grade
        ));
        report.push_str(&format!(
            "  Usable:        {:.1}% ({} block(s), {} backward jump(s))\n",
            q.usable_coverage * 100.0,
            q.block_count,
            q.backward_jump_count
        ));
        report.push_str(&format!(
            "  Missing:       {} frames, {} gap(s), {} glitch(es), {} edit point(s)\n",
            q.missing_frames, q.gap_count, q.glitch_count, q.edit_count
        ));
        if q.max_drift_secs > 0.01 {
            report.push_str(&format!(
                "  Max drift:    {:.3}s ({:.2} frames)\n",
                q.max_drift_secs, q.worst_block_drift_frames
            ));
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

fn sync_channel_map_from_probe(state: &mut AppState) {
    let is_video =
        state.latest.converter.selected_recording_type() == Some(RecordingType::VideoClipSequence);
    let expected = if is_video {
        state
            .latest
            .decode
            .probe
            .as_ref()
            .map(|p| p.total_audio_channels)
            .unwrap_or(0)
    } else {
        // Audio: the group's file count = channel count
        state
            .latest
            .converter
            .selected_group_idx
            .and_then(|idx| state.latest.converter.groups.get(idx))
            .map(|g| g.files.len())
            .unwrap_or(0)
    };
    bound::sync_channel_map(state, expected);
}

/// Build channel labels for the matrix rows. For video, show track labels
/// (e.g. "T1 L", "T1 R", "S2 C1"). For audio, show "CH n".
fn channel_row_labels(state: &AppState) -> Vec<String> {
    if state.latest.converter.selected_recording_type() == Some(RecordingType::VideoClipSequence) {
        if let Some(ref probe) = state.latest.decode.probe {
            let mut labels = Vec::new();
            for s in &probe.streams {
                for ch in 0..s.channels {
                    let label = if probe.streams.len() > 1 {
                        format!("S{} C{}", s.stream_index, ch + 1)
                    } else if s.channels == 2 {
                        format!("T1 {}", if ch == 0 { "L" } else { "R" })
                    } else {
                        format!("Ch {}", ch + 1)
                    };
                    labels.push(label);
                }
            }
            return labels;
        }
    }
    state
        .latest
        .converter
        .selected_group_idx
        .and_then(|idx| state.latest.converter.groups.get(idx))
        .map(|g| {
            (0..g.files.len())
                .map(|i| format!("CH {}", i + 1))
                .collect()
        })
        .unwrap_or_default()
}

/// Precomputed draw/hit-test geometry for one channel-matrix cell.
struct CellPos {
    cx: f32,
    cy: f32,
    row: usize,
    col: usize,
}

/// Pure geometry of the channel-matrix painter area (positions relative to
/// the painter origin).
struct MatrixLayout {
    cell_size: f32,
    cell_radius: f32,
    total_width: f32,
    total_height: f32,
    cells: Vec<CellPos>,
    headers: Vec<(f32, f32, String)>,
    row_labels: Vec<(f32, f32, String)>,
    row_is_ltc: Vec<bool>,
}

/// Scaled geometry inputs for [`matrix_layout`] — the design-time pixel
/// sizes multiplied by the text scale, plus a row-label gutter guaranteed
/// wide enough for the widest label. The header/row-label fonts grow with
/// the text scale (`MonoValue` preset), so the geometry must keep pace or
/// headers overlap and labels run into the radio buttons.
struct MatrixMetrics {
    cell_size: f32,
    header_height: f32,
    label_width: f32,
}

#[cfg(test)]
impl MatrixMetrics {
    /// Design-time geometry at 100 % text scale (test fixture; production
    /// callers derive the metrics from the live text scale via
    /// [`matrix_metrics`]).
    fn design() -> Self {
        Self {
            cell_size: 36.0,
            header_height: 24.0,
            label_width: 44.0,
        }
    }
}

/// Scale the design geometry by the text scale and widen the label gutter to
/// fit the widest `● <label>` row label (measured, not guessed — the label
/// font scales, so a fixed gutter would clip or collide at higher scales).
fn matrix_metrics(ui: &Ui, row_labels: &[String]) -> MatrixMetrics {
    let scale = crate::text::text_scale();
    let font = crate::text::TextStyle::MonoValue.font_spec();
    let widest_label = row_labels
        .iter()
        // The LTC-row bullet prefix is the wider of the two prefixes used in
        // `draw_matrix_headers`; measuring all labels with it is a safe bound.
        .map(|label| {
            let galley = egui::WidgetText::from(font.with(format!("● {label}"))).into_galley(
                ui,
                Some(egui::TextWrapMode::Extend),
                f32::INFINITY,
                egui::FontSelection::Default,
            );
            galley.size().x
        })
        .fold(0.0_f32, f32::max);
    MatrixMetrics {
        cell_size: 36.0 * scale,
        header_height: 24.0 * scale,
        label_width: (44.0 * scale).max(widest_label + 8.0),
    }
}

/// Compute header, row-label and cell positions for an `n × n` matrix.
fn matrix_layout(
    n: usize,
    row_labels: &[String],
    ltc_row: Option<usize>,
    metrics: &MatrixMetrics,
) -> MatrixLayout {
    let MatrixMetrics {
        cell_size,
        header_height,
        label_width,
    } = *metrics;
    let cell_radius = cell_size * (10.0 / 36.0);
    let total_width = label_width + cell_size * n as f32 + 8.0;
    let total_height = header_height + cell_size * n as f32 + 8.0;

    // Precompute positions
    let mut cells: Vec<CellPos> = Vec::new();
    let mut headers: Vec<(f32, f32, String)> = Vec::new();
    let mut row_label_data: Vec<(f32, f32, String)> = Vec::new();
    let mut row_is_ltc: Vec<bool> = Vec::new();

    for col in 0..n {
        let x = label_width + col as f32 * cell_size + cell_size / 2.0;
        let y = header_height / 2.0;
        headers.push((x, y, format!("OUT {}", col + 1)));
    }

    for row in 0..n {
        let y0 = header_height + row as f32 * cell_size;
        let label = row_labels
            .get(row)
            .cloned()
            .unwrap_or_else(|| format!("CH {}", row + 1));
        let is_ltc = ltc_row == Some(row);
        row_label_data.push((4.0, y0 + cell_size / 2.0, label));
        row_is_ltc.push(is_ltc);

        for col in 0..n {
            let cx = label_width + col as f32 * cell_size + cell_size / 2.0;
            let cy = y0 + cell_size / 2.0;
            cells.push(CellPos { cx, cy, row, col });
        }
    }

    MatrixLayout {
        cell_size,
        cell_radius,
        total_width,
        total_height,
        cells,
        headers,
        row_labels: row_label_data,
        row_is_ltc,
    }
}

fn render_channel_matrix(ui: &mut Ui, state: &mut AppState) {
    sync_channel_map_from_probe(state);
    let colors = state.theme.colors();
    let is_video =
        state.latest.converter.selected_recording_type() == Some(RecordingType::VideoClipSequence);
    let n = state.sh.conv.channel_map.value().num_channels();

    if n == 0 {
        if is_video && state.latest.decode.probe.is_none() {
            let label = gui_engine::job::probe_status_label(
                state.latest.job(JobKind::ClipProbe).is_active(),
                true,
            );
            let color = if matches!(label, ProbeStatusLabel::ProbeFailed) {
                colors.error_red
            } else {
                colors.text_muted
            };
            ui.label(
                text(ui, label.to_string())
                    .style(TextStyle::Hint, &colors)
                    .color(color),
            );
        } else {
            let label = ProbeStatusLabel::NoChannels.to_string();
            ui.label(text(ui, label).style(TextStyle::Label, &colors));
        }
        return;
    }

    ui.label(
        text(ui, "Click a radio button to swap the input channel (row) with the channel currently mapped to the selected output (column).")
            .style(TextStyle::Description, &colors)
            .color(colors.text_secondary),
    );
    ui.add_space(6.0);

    let row_labels = channel_row_labels(state);
    let ltc_row = if is_video {
        state.latest.decode.probe.as_ref().and_then(|probe| {
            ltc_row_index(
                probe,
                state.latest.decode.selected_stream,
                state.latest.decode.selected_channel,
            )
        })
    } else {
        None
    };

    let layout = matrix_layout(n, &row_labels, ltc_row, &matrix_metrics(ui, &row_labels));

    // Allocate the entire matrix area
    let (response, painter) = ui.allocate_painter(
        egui::vec2(layout.total_width, layout.total_height),
        egui::Sense::hover(),
    );
    let origin = response.rect.left_top();

    // Interact first: hover/click state must be known before painting so
    // cells can show their hover affordance this frame.
    let map = state.sh.conv.channel_map.value().clone();
    let cell_hover = handle_matrix_clicks(ui, state, origin, &layout, &map);

    draw_matrix_headers(ui, &painter, origin, &layout, &colors);
    draw_matrix_cells(&painter, origin, &layout, &colors, &map, &cell_hover);
}

/// Header text and row labels (LTC row highlighted with accent).
fn draw_matrix_headers(
    _ui: &Ui,
    painter: &egui::Painter,
    origin: egui::Pos2,
    layout: &MatrixLayout,
    colors: &ThemeColors,
) {
    for (x, y, text) in &layout.headers {
        painter.text(
            egui::pos2(origin.x + x, origin.y + y),
            egui::Align2::CENTER_CENTER,
            text.as_str(),
            crate::text::TextStyle::MonoValue.font_spec().font_id(),
            colors.text_muted,
        );
    }

    for (i, (x, y, text)) in layout.row_labels.iter().enumerate() {
        let color = if layout.row_is_ltc[i] {
            ACCENT
        } else {
            colors.text_title
        };
        let prefix = if layout.row_is_ltc[i] { "● " } else { "  " };
        painter.text(
            egui::pos2(origin.x + x, origin.y + y),
            egui::Align2::LEFT_CENTER,
            format!("{}{}", prefix, text),
            crate::text::TextStyle::MonoValue.font_spec().font_id(),
            color,
        );
    }
}

/// The radio-button circles; selection, LTC-row highlighting and hover
/// affordance (hover flags come from the interact pass in
/// [`handle_matrix_clicks`], which runs before painting).
fn draw_matrix_cells(
    painter: &egui::Painter,
    origin: egui::Pos2,
    layout: &MatrixLayout,
    colors: &ThemeColors,
    map: &ChannelMap,
    cell_hover: &[bool],
) {
    for (i, cell) in layout.cells.iter().enumerate() {
        let cx = origin.x + cell.cx;
        let cy = origin.y + cell.cy;
        let is_selected = map.get(cell.row) == cell.col;
        let is_ltc_row = layout.row_is_ltc[cell.row];
        let is_hovered = cell_hover.get(i).copied().unwrap_or(false) && !is_selected;

        let v = style::matrix_cell_visuals(colors, is_selected, is_hovered, is_ltc_row);
        let radius = layout.cell_radius;
        painter.circle_stroke(egui::pos2(cx, cy), radius, v.ring);
        painter.circle_filled(egui::pos2(cx, cy), radius - 2.0, v.core);
    }
}

/// Click-to-swap: clicking an unselected cell swaps its input channel (row)
/// with the channel currently mapped to that output (column). Returns the
/// per-cell hover flags (in `layout.cells` order) for the paint pass.
fn handle_matrix_clicks(
    ui: &mut Ui,
    state: &mut AppState,
    origin: egui::Pos2,
    layout: &MatrixLayout,
    map: &ChannelMap,
) -> Vec<bool> {
    let mut cell_hover = vec![false; layout.cells.len()];
    for (i, cell) in layout.cells.iter().enumerate() {
        let cx = origin.x + cell.cx;
        let cy = origin.y + cell.cy;
        let is_selected = map.get(cell.row) == cell.col;
        let hitbox = egui::Rect::from_center_size(
            egui::pos2(cx, cy),
            egui::vec2(layout.cell_size, layout.cell_size),
        );

        let click_id = egui::Id::new(("chan_map", cell.row, cell.col));
        let resp = ui.interact(hitbox, click_id, egui::Sense::click());
        if !is_selected {
            cell_hover[i] = resp.hovered();
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.clicked() {
                let row = cell.row;
                let col = cell.col;
                let mut new_map = map.clone();
                new_map.swap(row, col);
                bound::set_value(
                    state,
                    |s| &mut s.sh.conv.channel_map,
                    new_map,
                    move |_| GuiCommand::Converter(ConverterCommand::SwapChannelMapCells(row, col)),
                );
            }
        }
    }
    cell_hover
}

// ── Split / Drop options ────────────────────────────────────────────────

fn render_split_options(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_video =
        state.latest.converter.selected_recording_type() == Some(RecordingType::VideoClipSequence);
    let ltc_available = if is_video {
        state
            .latest
            .decode
            .group_results
            .iter()
            .any(|r| r.is_done())
    } else {
        state.latest.decode.result.is_some()
    };

    ui.add_space(8.0);
    let split_truth = state.latest.converter.settings.split_tracks;
    let drop_truth = state.latest.converter.settings.drop_ltc_track;
    let concat_truth = state.latest.converter.settings.concat_audio;
    ui.horizontal(|ui| {
        let split_resp = bound::checkbox(
            ui,
            state,
            |s| &mut s.sh.conv.split_tracks,
            split_truth,
            "Split tracks into separate files",
            true,
            |v| GuiCommand::Converter(ConverterCommand::SetSplitTracks(v)),
        );
        // Cascade: unchecking split also unchecks concatenate.
        if split_resp.changed()
            && !*state.sh.conv.split_tracks.value()
            && *state.sh.conv.concat_audio.value()
        {
            bound::set_value(
                state,
                |s| &mut s.sh.conv.concat_audio,
                false,
                |v| GuiCommand::Converter(ConverterCommand::SetConcatAudio(v)),
            );
        }
        bound::checkbox(
            ui,
            state,
            |s| &mut s.sh.conv.drop_ltc_track,
            drop_truth,
            "Drop LTC track",
            ltc_available,
            |v| GuiCommand::Converter(ConverterCommand::SetDropLtcTrack(v)),
        );
    });
    if is_video && *state.sh.conv.split_tracks.value() {
        ui.horizontal(|ui| {
            bound::checkbox(
                ui,
                state,
                |s| &mut s.sh.conv.concat_audio,
                concat_truth,
                "Concatenate audio tracks across clips (one file per track)",
                true,
                |v| GuiCommand::Converter(ConverterCommand::SetConcatAudio(v)),
            );
        });
        if *state.sh.conv.concat_audio.value() {
            ui.label(
                text(ui, "ℹ Audio from all clips will be joined into one file per track (in clip order).")
                    .style(TextStyle::Description, &colors)
                    .color(colors.text_secondary),
            );
        }
    }
    if *state.sh.conv.split_tracks.value() {
        ui.label(
            text(ui, "ℹ Each input track will be written to its own file. Channel mapping greets are preserved.")
                .style(TextStyle::Description, &colors)
                .color(colors.text_secondary),
        );
    }
    if *state.sh.conv.drop_ltc_track.value()
        && *state.sh.conv.ltc_file_idx.value() < state.sh.conv.channel_map.value().num_channels()
    {
        ui.label(
            text(
                ui,
                format!(
                    "ℹ LTC track (channel {}) will be excluded from all output.",
                    state.sh.conv.ltc_file_idx.value() + 1
                ),
            )
            .style(TextStyle::Description, &colors)
            .color(colors.text_secondary),
        );
    }
}

// ── Step 3: Output format (Video / Audio columns) ──────────────────────

/// Label suffix for hardware-accelerated codec entries.
fn codec_label(label: String, hw: bool) -> String {
    if hw {
        format!("{label} [HW accel. available]")
    } else {
        label
    }
}

/// Container dropdown options: probed when caps are available, static
/// supported list otherwise.
fn container_options(caps: Option<&gui_engine::FfmpegCapabilities>) -> Vec<(String, String)> {
    if let Some(caps) = caps {
        available_containers(caps)
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    } else {
        supported_containers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
}

/// Video codec dropdown options for `container`: probed codec list (with HW
/// annotation) when caps are available, static supported list otherwise.
fn video_codec_options(
    caps: Option<&gui_engine::FfmpegCapabilities>,
    container: &str,
) -> Vec<(String, String)> {
    if let Some(caps) = caps {
        available_video_codecs(container, caps)
            .into_iter()
            .map(|(id, label, hw)| (id, codec_label(label, hw)))
            .collect()
    } else {
        supported_video_codecs()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
}

/// Audio encoder dropdown options for `container`: probed when caps are
/// available, static supported list otherwise.
fn audio_encoder_options(
    caps: Option<&gui_engine::FfmpegCapabilities>,
    container: &str,
) -> Vec<(String, String)> {
    if let Some(caps) = caps {
        available_audio_encoders_for_container(container, caps)
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    } else {
        supported_audio_encoders()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }
}

fn render_output_format(
    ui: &mut Ui,
    state: &mut AppState,
    sanity: Option<&Result<(), ConversionCheckError>>,
) {
    let colors = state.theme.colors();
    let caps_opt = state.latest.ffmpeg_caps.clone();

    // While the ffmpeg capability probe is still running, show a placeholder.
    if caps_opt.is_none() && state.latest.job(JobKind::FfmpegCapProbe).is_active() {
        ui.label(text(ui, "Probing ffmpeg capabilities…").style(TextStyle::Label, &colors));
        if state.latest.converter.selected_recording_type()
            == Some(RecordingType::VideoClipSequence)
        {
            render_copy_video_checkbox(ui, state);
        }
        return;
    }

    // NOTE: engine-side repairs (caps probe completion, container change)
    // are authoritative; shadows adopt them via `bound::` sync. No GUI-side
    // defaults repair here — it would fight the engine's own re-selection.

    let containers: Vec<(String, String)> = container_options(caps_opt.as_ref());

    let cur_container = state.sh.conv.container.value().clone();
    let video_encoders: Vec<(String, String)> =
        video_codec_options(caps_opt.as_ref(), &cur_container);

    let audio_encoders: Vec<(String, String)> =
        audio_encoder_options(caps_opt.as_ref(), &cur_container);

    let cur_video_encoder = state.sh.conv.video_encoder.value().clone();
    let cur_audio_encoder = state.sh.conv.audio_encoder.value().clone();

    // Two-column layout: Video Format | Audio Format
    let is_narrow = ui.available_width() < 400.0;
    if is_narrow {
        ui.vertical(|ui| {
            if state.latest.converter.selected_recording_type()
                == Some(RecordingType::VideoClipSequence)
            {
                render_copy_video_checkbox(ui, state);
                ui.add_space(4.0);
            }
            render_video_format_rows(
                ui,
                state,
                !copy_mode_active(state),
                &cur_container,
                &containers,
                &cur_video_encoder,
                &video_encoders,
            );

            if state.latest.converter.selected_recording_type()
                == Some(RecordingType::MultiTrackAudio)
            {
                render_generate_synthetic_checkbox(
                    ui,
                    state,
                    "Generate synthetic video (blue background)",
                );
            }

            ui.add_space(8.0);
            ui.separator();
            ui.add_space(8.0);
            render_audio_format_rows(
                ui,
                state,
                true,
                &cur_audio_encoder,
                &audio_encoders,
                &colors,
            );
        });
    } else {
        ui.horizontal(|ui| {
            // Cap the left pane: egui gives unconstrained checkboxes/hint
            // labels nearly the full row width before wrapping, and the row
            // (left + separator + right) then inflates the tab card past the
            // header/footer width. Capped labels break onto new lines
            // instead (the checkbox label wraps via the vertical layout's
            // wrap mode).
            let max_left = (ui.available_width() * 0.5).max(300.0);
            ui.vertical(|ui| {
                ui.set_max_width(max_left);
                if state.latest.converter.selected_recording_type()
                    == Some(RecordingType::VideoClipSequence)
                {
                    render_copy_video_checkbox(ui, state);
                    ui.add_space(4.0);
                }
                render_metadata_only_checkbox(ui, state, &colors);
                render_embed_camera_checkbox(ui, state);
                render_camera_metadata_desc(ui, state, &colors);
                render_video_format_rows(
                    ui,
                    state,
                    !copy_mode_active(state) && !*state.sh.conv.metadata_only.value(),
                    &cur_container,
                    &containers,
                    &cur_video_encoder,
                    &video_encoders,
                );
                if state.latest.converter.selected_recording_type()
                    == Some(RecordingType::MultiTrackAudio)
                {
                    render_generate_synthetic_checkbox(ui, state, "Generate synthetic video");
                }
            });
            ui.add_space(16.0);
            ui.separator();
            ui.add_space(16.0);
            ui.vertical(|ui| {
                render_audio_format_rows(
                    ui,
                    state,
                    !*state.sh.conv.metadata_only.value(),
                    &cur_audio_encoder,
                    &audio_encoders,
                    &colors,
                );
            });
        });
    }

    render_format_warnings(ui, state, sanity, &colors);
    render_caps_error_note(ui, caps_opt.as_ref(), &colors);
}

/// "Leave video encoding untouched" bound checkbox with the copy-mode hint.
fn render_copy_video_checkbox(ui: &mut Ui, state: &mut AppState) {
    let truth = state.latest.converter.settings.copy_video;
    bound::checkbox(
        ui,
        state,
        |s| &mut s.sh.conv.copy_video,
        truth,
        "Leave video encoding untouched (stream copy)",
        true,
        |v| GuiCommand::Converter(ConverterCommand::SetCopyVideo(v)),
    );
    if copy_mode_active(state) {
        let colors = state.theme.colors();
        ui.label(
            text(ui, "Video is copied without re-encoding (much faster). Cuts snap to the nearest keyframe before the trim point.")
                .style(TextStyle::Description, &colors),
        );
    }
}

/// "Generate synthetic video" bound checkbox (`label` differs between the
/// narrow and wide layouts).
fn render_generate_synthetic_checkbox(ui: &mut Ui, state: &mut AppState, label: &str) {
    let truth = state.latest.converter.settings.generate_synthetic_video;
    bound::checkbox(
        ui,
        state,
        |s| &mut s.sh.conv.generate_synthetic_video,
        truth,
        label,
        true,
        |v| GuiCommand::Converter(ConverterCommand::SetGenerateSyntheticVideo(v)),
    );
}

/// "Metadata only" bound checkbox with the in-place-tagging hint (wide
/// layout only).
fn render_metadata_only_checkbox(
    ui: &mut Ui,
    state: &mut AppState,
    colors: &crate::theme::ThemeColors,
) {
    let meta_truth = state.latest.converter.settings.metadata_only;
    bound::checkbox(
        ui,
        state,
        |s| &mut s.sh.conv.metadata_only,
        meta_truth,
        "Metadata only (tag + rename, extract audio)",
        true,
        |v| GuiCommand::Converter(ConverterCommand::SetMetadataOnly(v)),
    );
    if *state.sh.conv.metadata_only.value() {
        ui.label(
            text(ui, "Originals are tagged in place with the start timecode and renamed. Audio is extracted to the output folder. No re-encoding.")
                .style(TextStyle::Description, colors),
        );
        ui.add_space(4.0);
    }
}

/// "Embed camera metadata" bound checkbox (wide layout only).
fn render_embed_camera_checkbox(ui: &mut Ui, state: &mut AppState) {
    let embed_truth = state.latest.converter.settings.embed_camera_metadata;
    bound::checkbox(
        ui,
        state,
        |s| &mut s.sh.conv.embed_camera_metadata,
        embed_truth,
        "Embed camera metadata (make/model, lens, serial, timestamp, gamma, exposure)",
        true,
        |v| GuiCommand::Converter(ConverterCommand::SetEmbedCameraMetadata(v)),
    );
}

/// One-line make/model description of the probed camera, if any.
fn render_camera_metadata_desc(
    ui: &mut Ui,
    state: &mut AppState,
    colors: &crate::theme::ThemeColors,
) {
    let camera = state
        .latest
        .converter
        .camera_meta
        .first()
        .and_then(|c| c.as_ref());
    if let Some(c) = camera {
        let desc = match (&c.make, &c.model) {
            (Some(m), Some(n)) => format!("{} {}", m, n),
            (Some(m), None) => m.clone(),
            (None, Some(n)) => n.clone(),
            (None, None) => String::new(),
        };
        if !desc.is_empty() {
            ui.label(text(ui, desc).style(TextStyle::Description, colors));
        }
    }
}

/// "VIDEO FORMAT" header + container/codec rows (rows gated by `enabled`).
fn render_video_format_rows(
    ui: &mut Ui,
    state: &mut AppState,
    enabled: bool,
    cur_container: &str,
    containers: &[(String, String)],
    cur_video_encoder: &str,
    video_encoders: &[(String, String)],
) {
    let colors = state.theme.colors();
    ui.label(
        text(ui, "VIDEO FORMAT")
            .style(TextStyle::Heading, &colors)
            .color(colors.text_title),
    );
    ui.add_space(4.0);
    ui.add_enabled_ui(enabled, |ui| {
        {
            let container_truth = state.latest.converter.settings.container.clone();
            let mut update_container = |v: &str| {
                bound::select_value(
                    state,
                    |s| &mut s.sh.conv.container,
                    container_truth.clone(),
                    v.to_string(),
                    |c| GuiCommand::Converter(ConverterCommand::SetContainer(c)),
                );
                // Encoder re-selection for the new container is done
                // engine-side (apply_available_defaults on SetContainer).
            };
            render_format_row(
                ui,
                "Container",
                cur_container,
                containers,
                &mut update_container,
                &colors,
            );
        }
        {
            let video_truth = state.latest.converter.settings.video_encoder.clone();
            let mut update_video = |v: &str| {
                bound::select_value(
                    state,
                    |s| &mut s.sh.conv.video_encoder,
                    video_truth.clone(),
                    v.to_string(),
                    |c| GuiCommand::Converter(ConverterCommand::SetVideoCodec(c)),
                );
            };
            render_format_row(
                ui,
                "Video codec",
                cur_video_encoder,
                video_encoders,
                &mut update_video,
                &colors,
            );
        }
    });
}

/// "AUDIO FORMAT" header + encoder row (gated while metadata-only is on).
fn render_audio_format_rows(
    ui: &mut Ui,
    state: &mut AppState,
    enabled: bool,
    cur_audio_encoder: &str,
    audio_encoders: &[(String, String)],
    colors: &crate::theme::ThemeColors,
) {
    ui.label(
        text(ui, "AUDIO FORMAT")
            .style(TextStyle::Heading, colors)
            .color(colors.text_title),
    );
    ui.add_space(4.0);
    ui.add_enabled_ui(enabled, |ui| {
        let audio_truth = state.latest.converter.settings.audio_encoder.clone();
        let mut update_audio = |v: &str| {
            bound::select_value(
                state,
                |s| &mut s.sh.conv.audio_encoder,
                audio_truth.clone(),
                v.to_string(),
                |c| GuiCommand::Converter(ConverterCommand::SetAudioEncoder(c)),
            );
        };
        render_format_row(
            ui,
            "Audio encoder",
            cur_audio_encoder,
            audio_encoders,
            &mut update_audio,
            colors,
        );
    });
}

/// Tinted amber warning note frame.
fn render_warning_note(ui: &mut Ui, text: &str, colors: &crate::theme::ThemeColors) {
    let (fill, stroke, _) = style::badge_colors(colors, style::BadgeTone::Warning);
    let warning_area = egui::Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 6));
    warning_area.show(ui, |ui| {
        ui.label(
            crate::text::text(ui, text)
                .style(TextStyle::Hint, colors)
                .color(colors.warning_amber),
        );
    });
}

/// Sanity-check failure, collision and duplicate-output warnings. A passing
/// sanity check renders nothing: the engine never lets the user select an
/// incompatible format combination, so there is no compatibility feedback
/// line to show.
fn render_format_warnings(
    ui: &mut Ui,
    state: &mut AppState,
    sanity: Option<&Result<(), ConversionCheckError>>,
    colors: &crate::theme::ThemeColors,
) {
    // Sanity check (pre-computed in render() — pure, no filesystem IO)
    if let Some(s) = sanity {
        if let Err(msg) = s {
            ui.add_space(4.0);
            render_warning_note(ui, &format!("⚠ {}", msg), colors);
        }

        // Non-blocking collision warning (never disables the Convert button)
        if let Some(ref note) = state.latest.converter.collision_warning {
            ui.add_space(4.0);
            render_warning_note(ui, &format!("ℹ {}", note), colors);
        }
        // Non-blocking duplicate output warning (never disables the Convert button)
        if let Some(ref note) = state.latest.converter.duplicate_output_warning {
            ui.add_space(4.0);
            render_warning_note(ui, &format!("⚠ {}", note), colors);
        }
    }
}

/// Red error note when ffmpeg is missing entirely.
fn render_caps_error_note(
    ui: &mut Ui,
    caps: Option<&gui_engine::FfmpegCapabilities>,
    colors: &crate::theme::ThemeColors,
) {
    if let Some(caps) = caps {
        if !caps.has_ffmpeg {
            ui.add_space(4.0);
            let (fill, stroke, _) = style::badge_colors(colors, style::BadgeTone::Danger);
            let error_frame = egui::Frame::new()
                .fill(fill)
                .stroke(stroke)
                .corner_radius(6.0)
                .inner_margin(egui::Margin::symmetric(10, 6));
            error_frame.show(ui, |ui| {
                if let Some(msg) = &caps.error_message {
                    ui.label(
                        text(ui, format!("✗ {}", msg))
                            .style(TextStyle::Hint, colors)
                            .color(colors.error_red),
                    );
                }
            });
        }
    }
}

fn render_format_row(
    ui: &mut Ui,
    label: &str,
    current: &str,
    options: &[(String, String)],
    on_change: &mut dyn FnMut(&str),
    colors: &crate::theme::ThemeColors,
) {
    ui.horizontal(|ui| {
        ui.label(text(ui, format!("{}:", label)).style(TextStyle::Label, colors));
        egui::ComboBox::from_id_salt(format!("fmt_{}_{}", label, options.len()))
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (key, desc) in options {
                    let is_sel = key == current;
                    if ui
                        .selectable_label(is_sel, format!("{} — {}", key, desc))
                        .clicked()
                    {
                        on_change(key);
                    }
                }
            });
    });
}

// ── Step 4: Output file path ────────────────────────────────────────────

/// Whether an LTC decode result is available so "Set Start Time from LTC"
/// can be applied (drives the checkbox's enabled state).
fn ltc_available_for_start_apply(
    decoding: bool,
    is_video_group: bool,
    group_has_results: bool,
    single_done: bool,
) -> bool {
    !decoding
        && if is_video_group {
            group_has_results
        } else {
            single_done
        }
}

/// Display name of a preview output path relative to the output folder,
/// falling back to the full path (or "?") when it cannot be stripped/shown.
fn preview_display_name(path: &std::path::Path, output_folder: &std::path::Path) -> String {
    path.strip_prefix(output_folder)
        .ok()
        .and_then(|p| p.to_str())
        .unwrap_or_else(|| path.to_str().unwrap_or("?"))
        .to_string()
}

fn render_output_path(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    render_naming_template_rows(ui, state, &colors);
    render_output_preview(ui, state, &colors);

    // Set start time from LTC checkbox
    let is_video_group =
        state.latest.converter.selected_recording_type() == Some(RecordingType::VideoClipSequence);
    let group_has_results = state
        .latest
        .decode
        .group_results
        .iter()
        .any(gui_engine::state::ClipDecodeState::is_done);
    let single_done = state.latest.decode.result.is_some() || state.latest.decode.error.is_some();
    let decoding = state.latest.job(JobKind::LtcDecode).is_active()
        || state.latest.job(JobKind::LtcGroupDecode).is_active();
    let ltc_available =
        ltc_available_for_start_apply(decoding, is_video_group, group_has_results, single_done);
    ui.add_space(4.0);
    render_set_start_row(ui, state, is_video_group, ltc_available, &colors);
}

/// Output-folder, filename-prefix and audio/video suffix template rows.
fn render_naming_template_rows(
    ui: &mut Ui,
    state: &mut AppState,
    colors: &crate::theme::ThemeColors,
) {
    // Output folder — bound text field + Browse dialog (programmatic write)
    ui.horizontal(|ui| {
        ui.label(text(ui, "Output folder:").style(TextStyle::Label, colors));
        let truth = state.latest.converter.settings.output_folder.clone();
        let folder_width = ui.available_width() - 100.0;
        bound::path_text(
            ui,
            state,
            |s| &mut s.sh.conv.output_folder,
            &truth,
            |v| GuiCommand::Converter(ConverterCommand::SetOutputFolder(v)),
            move |edit| {
                edit.font(crate::text::TextStyle::MonoLabel.font_spec().font_id())
                    .desired_width(folder_width)
            },
        );
        if ui.button("Browse…").clicked() {
            let folder = rfd::FileDialog::new()
                .set_directory(state.sh.conv.output_folder.value())
                .pick_folder();
            if let Some(path) = folder {
                bound::set_value(
                    state,
                    |s| &mut s.sh.conv.output_folder,
                    path,
                    |p| GuiCommand::Converter(ConverterCommand::SetOutputFolder(p)),
                );
            }
        }
    });

    // Filename prefix
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        ui.label(text(ui, "Filename prefix:").style(TextStyle::Label, colors));
        let truth = state.latest.converter.settings.filename_prefix.clone();
        bound::text(
            ui,
            state,
            |s| &mut s.sh.conv.filename_prefix,
            &truth,
            |v| GuiCommand::Converter(ConverterCommand::SetFilenamePrefix(v)),
            |edit| edit.font(crate::text::TextStyle::MonoLabel.font_spec().font_id()),
        );
    });

    // Audio suffix
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(text(ui, "Audio suffix:").style(TextStyle::Label, colors));
        let truth = state
            .latest
            .converter
            .settings
            .audio_suffix_template
            .clone();
        bound::text(
            ui,
            state,
            |s| &mut s.sh.conv.audio_suffix_template,
            &truth,
            |v| GuiCommand::Converter(ConverterCommand::SetAudioSuffixTemplate(v)),
            |edit| edit.font(crate::text::TextStyle::MonoLabel.font_spec().font_id()),
        );
    });

    // Video suffix
    ui.add_space(2.0);
    ui.horizontal(|ui| {
        ui.label(text(ui, "Video suffix:").style(TextStyle::Label, colors));
        let truth = state
            .latest
            .converter
            .settings
            .video_suffix_template
            .clone();
        bound::text(
            ui,
            state,
            |s| &mut s.sh.conv.video_suffix_template,
            &truth,
            |v| GuiCommand::Converter(ConverterCommand::SetVideoSuffixTemplate(v)),
            |edit| edit.font(crate::text::TextStyle::MonoLabel.font_spec().font_id()),
        );
    });
}

/// Preview of the output filenames derived from the current settings.
fn render_output_preview(ui: &mut Ui, state: &mut AppState, colors: &crate::theme::ThemeColors) {
    let has_group = state.latest.converter.selected_group_idx.is_some();
    if has_group && !state.sh.conv.filename_prefix.value().is_empty() {
        ui.add_space(4.0);
        let settings = current_converter_settings(state);
        let previews = preview_output_files(&settings, state.latest.decode.probe.as_ref());

        if !previews.is_empty() {
            let count = previews.len();
            ui.label(
                text(ui, format!("↳ {} output file(s):", count))
                    .style(TextStyle::Description, colors)
                    .color(colors.text_secondary),
            );
            for preview in &previews {
                let display_name = preview_display_name(
                    preview.path.as_path(),
                    state.sh.conv.output_folder.value(),
                );
                let icon = match preview.kind {
                    OutputKind::Video => "🎬",
                    OutputKind::Audio => "🔊",
                };
                ui.label(
                    text(ui, format!("  {} {}", icon, display_name))
                        .style(TextStyle::MonoValue, colors),
                );
            }
        }
    }
}

/// "Set Start Time from LTC" checkbox row with the resolved start timecode
/// (or a "Detect LTC first" hint while nothing is decoded).
fn render_set_start_row(
    ui: &mut Ui,
    state: &mut AppState,
    is_video_group: bool,
    ltc_available: bool,
    colors: &crate::theme::ThemeColors,
) {
    ui.horizontal(|ui| {
        let truth = state.latest.converter.settings.set_start_from_ltc;
        bound::checkbox(
            ui,
            state,
            |s| &mut s.sh.conv.set_start_from_ltc,
            truth,
            "Set Start Time from LTC",
            ltc_available,
            |v| GuiCommand::Converter(ConverterCommand::SetStartFromLtc(v)),
        );
        if *state.sh.conv.set_start_from_ltc.value() {
            let tc_text = if is_video_group {
                state
                    .latest
                    .decode
                    .group_results
                    .first()
                    .and_then(|r| r.ok())
                    .and_then(start_timecode_from_ltc)
                    .map(|m| gui_engine::converter::format_ffmpeg_timecode(&m.start, m.drop_frame))
                    .unwrap_or_default()
            } else {
                state
                    .latest
                    .decode
                    .result
                    .as_ref()
                    .and_then(start_timecode_from_ltc)
                    .map(|m| gui_engine::converter::format_ffmpeg_timecode(&m.start, m.drop_frame))
                    .unwrap_or_default()
            };
            if !tc_text.is_empty() {
                ui.label(
                    text(ui, format!("(no cut; starts at {})", tc_text))
                        .style(TextStyle::Label, colors),
                );
            }
        }
        if !ltc_available && !state.latest.job(JobKind::LtcDecode).is_active() {
            ui.label(text(ui, "(Detect LTC first)").style(TextStyle::Label, colors));
        }
    });
}

// ── Convert button ─────────────────────────────────────────────────────

fn selected_input_files(state: &AppState) -> Vec<PathBuf> {
    state
        .latest
        .converter
        .selected_group_idx
        .and_then(|idx| state.latest.converter.groups.get(idx))
        .map(|g| g.files.clone())
        .unwrap_or_default()
}

fn render_convert_button(
    ui: &mut Ui,
    state: &mut AppState,
    sanity: Option<&Result<(), ConversionCheckError>>,
) {
    let colors = state.theme.colors();
    let is_running = state.latest.job(JobKind::Conversion).is_active();

    if is_running {
        if style::action_button(
            ui,
            &colors,
            style::ActionStyle::Danger,
            "■ CANCEL CONVERSION",
            crate::text::TextStyle::Control.font_spec(),
            egui::vec2(ui.available_width(), 42.0),
            true,
        )
        .clicked()
        {
            state.send(GuiCommand::Converter(ConverterCommand::CancelConversion));
        }
        return;
    }

    let caps_opt = state.latest.ffmpeg_caps.clone();

    let readiness = evaluate_readiness(
        state.latest.converter.selected_group_idx.is_some(),
        state.sh.conv.filename_prefix.value().is_empty(),
        state.sh.conv.output_folder.value().as_os_str().is_empty(),
        caps_opt.as_ref(),
    );
    let can_convert = readiness.can_convert;

    let sanity_ok = sanity.map(|r| r.is_ok()).unwrap_or(false);

    let button_label = if *state.sh.conv.metadata_only.value() {
        "TAG + EXTRACT (METADATA ONLY)"
    } else {
        match state.latest.converter.selected_recording_type() {
            Some(RecordingType::MultiTrackAudio) => {
                if *state.sh.conv.generate_synthetic_video.value() {
                    "CONVERT WITH SYNTHETIC VIDEO"
                } else {
                    "CONVERT AUDIO FILES"
                }
            }
            Some(RecordingType::VideoClipSequence) => {
                if copy_mode_active(state) {
                    "CONVERT VIDEO CLIPS (STREAM COPY)"
                } else {
                    "CONVERT VIDEO CLIPS"
                }
            }
            _ => "CONVERT",
        }
    };

    ui.add_enabled_ui(can_convert && sanity_ok, |ui| {
        if style::action_button(
            ui,
            &colors,
            style::ActionStyle::Primary,
            button_label,
            crate::text::TextStyle::Control.font_spec(),
            egui::vec2(ui.available_width(), 42.0),
            can_convert && sanity_ok,
        )
        .clicked()
        {
            start_conversion(state);
        }
    });

    if !can_convert {
        ui.add_space(2.0);
        ui.label(
            text(ui, format_blockers(&readiness.blockers))
                .style(TextStyle::Hint, &colors)
                .color(colors.text_secondary),
        );
    } else if !sanity_ok {
        ui.add_space(2.0);
        ui.label(
            text(ui, "Fix the compatibility issue above before converting.")
                .style(TextStyle::Hint, &colors)
                .color(colors.warning_amber),
        );
    }
}

/// Build a [`ConverterSettings`] from the current GUI state, using default
/// (empty) trim and timecode metadata so filenames are computed correctly for
/// the preview without needing an LTC decode result.
fn current_converter_settings(state: &AppState) -> ConverterSettings {
    let conv = &state.sh.conv;
    let input_files = selected_input_files(state);
    let pipeline = if *conv.metadata_only.value() {
        ConversionPipeline::MetadataOnly
    } else {
        match state.latest.converter.selected_recording_type() {
            Some(RecordingType::MultiTrackAudio) => ConversionPipeline::AudioOnly {
                generate_synthetic_video: *conv.generate_synthetic_video.value(),
            },
            Some(RecordingType::VideoClipSequence) => ConversionPipeline::VideoPassthrough,
            None => ConversionPipeline::AudioOnly {
                generate_synthetic_video: false,
            },
        }
    };
    let ltc_video_source = match state.latest.converter.selected_recording_type() {
        Some(RecordingType::VideoClipSequence) => Some((
            state.latest.decode.selected_stream,
            state.latest.decode.selected_channel,
        )),
        _ => None,
    };
    ConverterSettings {
        pipeline,
        input_files,
        recording_type: state
            .latest
            .converter
            .selected_recording_type()
            .unwrap_or(RecordingType::MultiTrackAudio),
        ltc_track_channel_index: *conv.ltc_file_idx.value(),
        channel_map: conv.channel_map.value().clone(),
        split_tracks: *conv.split_tracks.value(),
        drop_ltc_track: *conv.drop_ltc_track.value(),
        concat_audio: *conv.concat_audio.value(),
        ltc_video_source,
        container: conv.container.value().clone(),
        copy_video: copy_mode_active(state),
        video_encoder: conv.video_encoder.value().clone(),
        audio_encoder: conv.audio_encoder.value().clone(),
        resolved_video_encoder: String::new(),
        output_folder: conv.output_folder.value().clone(),
        filename_prefix: conv.filename_prefix.value().clone(),
        audio_suffix_template: conv.audio_suffix_template.value().clone(),
        video_suffix_template: conv.video_suffix_template.value().clone(),
        set_start_from_ltc: *conv.set_start_from_ltc.value(),
        embed_camera_metadata: *conv.embed_camera_metadata.value(),
        trim_offsets_secs: vec![0.0; selected_input_files(state).len()],
        timecode_meta_per_file: vec![None; selected_input_files(state).len()],
        camera_meta_per_file: vec![None; selected_input_files(state).len()],
        device_name: state.latest.converter.device_name.clone(),
        resolved_hw_device: None,
    }
}

fn start_conversion(state: &mut AppState) {
    let caps_opt = state.latest.ffmpeg_caps.clone();
    let conv = &state.sh.conv;
    let output_folder = conv.output_folder.value().clone();
    let filename_prefix = conv.filename_prefix.value().clone();
    let audio_suffix = conv.audio_suffix_template.value().clone();
    let video_suffix = conv.video_suffix_template.value().clone();
    if let Some(ref caps) = caps_opt {
        let input_files = selected_input_files(state);
        let r = if *conv.metadata_only.value() {
            conversion_sanity_check_metadata_only(
                &input_files,
                &output_folder,
                &filename_prefix,
                caps,
                Some(&audio_suffix),
                Some(&video_suffix),
            )
        } else {
            conversion_sanity_check(SanityCheckInput {
                container: conv.container.value(),
                video_codec: conv.video_encoder.value(),
                audio_encoder: conv.audio_encoder.value(),
                input_files: &input_files,
                output_folder: &output_folder,
                filename_prefix: &filename_prefix,
                caps,
                audio_suffix: Some(&audio_suffix),
                video_suffix: Some(&video_suffix),
                copy_video: copy_mode_active(state),
            })
        };
        if let Err(e) = r {
            log::error!("Conversion refused by preflight check: {}", e);
            return;
        }
    }
    state.send(GuiCommand::Converter(ConverterCommand::StartConversion));
}

// ── Progress & log display ──────────────────────────────────────────────

fn render_conversion_progress(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let job = state.latest.job(JobKind::Conversion).clone();

    match job.phase() {
        JobPhase::Running | JobPhase::Indeterminate => {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(100));
            ui.label(
                text(ui, "Converting…")
                    .style(TextStyle::Heading, &colors)
                    .color(colors.text_title),
            );
            ui.add_space(4.0);
            let pb = egui::ProgressBar::new(job.fraction())
                .show_percentage()
                .desired_width(ui.available_width());
            ui.add(pb);
            ui.add_space(4.0);

            let log_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x0D, 0x0D, 0x0F))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::symmetric(8, 4));
            log_frame.show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(crate::ids::ffmpeg_log_running_scroll())
                    .max_height(120.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.label(
                            text(ui, job.log())
                                .style(TextStyle::MonoValue, &colors)
                                .color(Color32::from_rgb(0x88, 0xCC, 0x88)),
                        );
                    });
            });
        }
        JobPhase::Succeeded => {
            ui.label(
                text(ui, "✓ Conversion completed successfully!")
                    .style(TextStyle::Status, &colors)
                    .color(colors.success_green),
            );
            ui.add_space(4.0);
            ui.label(
                text(
                    ui,
                    format!(
                        "Files saved to: {}/{}",
                        state.sh.conv.output_folder.value().display(),
                        state.sh.conv.filename_prefix.value(),
                    ),
                )
                .style(TextStyle::Label, &colors),
            );
            ui.add_space(4.0);

            let log_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x0D, 0x0D, 0x0F))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::symmetric(8, 4));
            log_frame.show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(crate::ids::ffmpeg_log_completed_scroll())
                    .max_height(80.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.label(
                            text(ui, job.log())
                                .style(TextStyle::MonoValue, &colors)
                                .color(Color32::from_rgb(0x88, 0xCC, 0x88)),
                        );
                    });
            });

            if ui.button("Start New Conversion").clicked() {}
        }
        JobPhase::Failed => {
            let error_text = job.error.clone().unwrap_or_else(|| job.log().to_string());

            let error_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x44, 0x11, 0x11))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.error_red))
                .inner_margin(egui::Margin::symmetric(10, 6));
            error_frame.show(ui, |ui| {
                ui.label(
                    text(ui, "✗ CONVERSION FAILED")
                        .style(TextStyle::Status, &colors)
                        .color(colors.error_red),
                );
            });

            ui.add_space(4.0);

            let log_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x0D, 0x0D, 0x0F))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::symmetric(8, 4));
            log_frame.show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(crate::ids::ffmpeg_log_failed_scroll())
                    .max_height(200.0)
                    .stick_to_bottom(false)
                    .show(ui, |ui| {
                        ui.label(
                            text(ui, &error_text)
                                .style(TextStyle::MonoValue, &colors)
                                .color(Color32::from_rgb(0xFF, 0x66, 0x66)),
                        );
                    });
            });

            ui.horizontal(|ui| {
                if ui.button("Copy Error Log").clicked() {
                    ui.ctx().copy_text(error_text.clone());
                }
                if ui.button("Try Again").clicked() {}
            });
        }
        JobPhase::Cancelled => {
            ui.label(
                text(ui, "■ Conversion canceled")
                    .style(TextStyle::Status, &colors)
                    .color(colors.warning_amber),
            );
            ui.add_space(4.0);

            let log_frame = egui::Frame::new()
                .fill(Color32::from_rgb(0x0D, 0x0D, 0x0F))
                .corner_radius(6.0)
                .stroke(egui::Stroke::new(1.0, colors.border_main))
                .inner_margin(egui::Margin::symmetric(8, 4));
            log_frame.show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt(crate::ids::ffmpeg_log_completed_scroll())
                    .max_height(80.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.label(
                            text(ui, job.log())
                                .style(TextStyle::MonoValue, &colors)
                                .color(Color32::from_rgb(0x88, 0xCC, 0x88)),
                        );
                    });
            });
        }
        JobPhase::Idle => {}
    }
}

#[cfg(test)]
mod tests {
    use gui_engine::{FrameTimecode, LtcDecodeStatus, LtcDetectionResult, Timecode};

    fn make_result(
        status: LtcDecodeStatus,
        fps: f32,
        timecodes: Vec<FrameTimecode>,
        first_secs: f64,
        drop_frame: bool,
    ) -> LtcDetectionResult {
        let confidence = if matches!(&status, LtcDecodeStatus::Success) {
            0.95
        } else {
            0.5
        };
        LtcDetectionResult {
            status,
            detected_fps: fps,
            drop_frame,
            total_possible_frames: timecodes.len() as u32,
            valid_frames: timecodes.len() as u32,
            timecodes,
            avg_confidence: confidence,
            details: Vec::new(),
            total_audio_duration_secs: 10.0,
            sample_rate: 48000,
            processing_time_ms: 15.0,
            first_ltc_timecode_secs: first_secs,
            quality: None,
            chunk_summaries: Vec::new(),
        }
    }

    fn tc_frame(h: u32, m: u32, s: u32, f: u32, secs: f64) -> FrameTimecode {
        FrameTimecode {
            frame_index: secs as u32,
            timecode: Timecode {
                hours: h,
                minutes: m,
                seconds: s,
                frames: f,
            },
            timecode_secs: secs,
        }
    }

    #[test]
    fn summary_success_with_timecodes() {
        let tcs = vec![tc_frame(1, 0, 0, 0, 0.0), tc_frame(1, 0, 0, 24, 0.96)];
        let result = make_result(LtcDecodeStatus::Success, 25.0, tcs, 0.0, false);
        let s = super::format_clip_ltc_summary(&result);
        assert!(
            s.contains("01:00:00:00"),
            "expected start TC in summary, got: {}",
            s
        );
        assert!(
            s.contains("25.00 fps"),
            "expected fps in summary, got: {}",
            s
        );
        assert!(
            s.contains("trim 0.000"),
            "expected trim in summary, got: {}",
            s
        );
    }

    #[test]
    fn summary_drop_frame() {
        let tcs = vec![tc_frame(1, 0, 0, 0, 0.0)];
        let result = make_result(LtcDecodeStatus::Success, 29.97, tcs, 0.5, true);
        let s = super::format_clip_ltc_summary(&result);
        assert!(
            s.contains("01;00;00;00"),
            "expected DF sep in summary, got: {}",
            s
        );
        assert!(s.contains("DF"), "expected DF flag in summary, got: {}", s);
        assert!(
            s.contains("trim 0.500"),
            "expected trim in summary, got: {}",
            s
        );
    }

    #[test]
    fn summary_low_confidence() {
        let tcs = vec![tc_frame(2, 10, 30, 15, 50.0)];
        let result = make_result(LtcDecodeStatus::LowConfidence, 24.0, tcs, 50.0, false);
        let s = super::format_clip_ltc_summary(&result);
        assert!(s.contains("02:10:30:15"), "expected start TC, got: {}", s);
        assert!(s.contains("24.00"), "expected fps, got: {}", s);
        assert!(s.contains("trim 50.000"), "expected trim, got: {}", s);
    }

    #[test]
    fn summary_no_sync_word() {
        let result = make_result(LtcDecodeStatus::NoSyncWord, 0.0, vec![], 0.0, false);
        let s = super::format_clip_ltc_summary(&result);
        assert_eq!(s, "No LTC found");
    }

    #[test]
    fn summary_error_status() {
        let result = LtcDetectionResult {
            status: LtcDecodeStatus::Error {
                message: "permission denied".into(),
            },
            detected_fps: 0.0,
            drop_frame: false,
            total_possible_frames: 0,
            valid_frames: 0,
            timecodes: vec![],
            avg_confidence: 0.0,
            details: vec![],
            total_audio_duration_secs: 0.0,
            sample_rate: 0,
            processing_time_ms: 0.0,
            first_ltc_timecode_secs: 0.0,
            quality: None,
            chunk_summaries: Vec::new(),
        };
        let s = super::format_clip_ltc_summary(&result);
        assert_eq!(s, "Error: permission denied");
    }

    #[test]
    fn summary_empty_timecodes_fallback() {
        let result = make_result(LtcDecodeStatus::Success, 25.0, vec![], 0.0, false);
        let s = super::format_clip_ltc_summary(&result);
        assert!(
            s.contains("—"),
            "expected dash fallback for empty timecodes, got: {}",
            s
        );
        assert!(s.contains("25.00 fps"), "expected fps, got: {}", s);
    }

    // ── Pure extractor tests ─────────────────────────────────────────────

    use std::path::PathBuf;

    use gui_engine::converter::RecordingType;
    use gui_engine::file_pattern::MatchedGroup;
    use gui_engine::state::ClipDecodeState;
    use gui_engine::{AudioStreamInfo, VideoAudioProbe};

    use crate::theme::{Theme, ThemeColors};

    fn test_colors() -> ThemeColors {
        Theme::Dark.colors()
    }

    fn probe(streams: &[(usize, usize)]) -> VideoAudioProbe {
        VideoAudioProbe {
            streams: streams
                .iter()
                .map(|&(idx, ch)| AudioStreamInfo {
                    stream_index: idx,
                    channels: ch,
                    codec_name: "pcm_s16le".to_string(),
                    sample_rate: 48000,
                })
                .collect(),
            total_audio_channels: streams.iter().map(|&(_, ch)| ch).sum(),
            is_video_file: true,
        }
    }

    fn group_of(recording_type: RecordingType, files: &[&str]) -> MatchedGroup {
        MatchedGroup {
            prefix: "mygroup".to_string(),
            rel_dir: String::new(),
            files: files.iter().map(PathBuf::from).collect(),
            recording_type,
        }
    }

    #[test]
    fn channel_options_video_multi_stream_labels() {
        let p = probe(&[(0, 1), (1, 2)]);
        let opts = super::build_channel_options(true, Some(&p), false, &[]);
        assert_eq!(opts.len(), 3);
        assert_eq!(opts[0].label, "Stream 1 Ch 1");
        assert_eq!(opts[1].label, "Stream 2 Ch 1");
        assert_eq!(opts[2].label, "Stream 2 Ch 2");
        assert_eq!(
            opts[1],
            super::ChannelOption {
                stream: 1,
                channel: 0,
                label: "Stream 2 Ch 1".to_string(),
                disabled: false,
            }
        );
    }

    #[test]
    fn channel_options_video_single_stream_stereo_labels() {
        let p = probe(&[(0, 2)]);
        let opts = super::build_channel_options(true, Some(&p), false, &[]);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].label, "Track 1 L");
        assert_eq!(opts[1].label, "Track 1 R");
    }

    #[test]
    fn channel_options_probing_placeholder() {
        let opts = super::build_channel_options(true, None, true, &[]);
        assert_eq!(opts.len(), 1);
        assert!(opts[0].disabled);
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(opts[0].label, "Probing…");
    }

    #[test]
    fn channel_options_video_idle_without_probe_is_empty() {
        let opts = super::build_channel_options(true, None, false, &[]);
        assert!(opts.is_empty());
    }

    #[test]
    fn channel_options_audio_from_files() {
        let files = vec![PathBuf::from("/x/a.wav"), PathBuf::from("/x/b.wav")];
        let opts = super::build_channel_options(false, None, false, &files);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].stream, 0);
        assert_eq!(opts[1].stream, 1);
        assert!(opts.iter().all(|o| o.channel == 0 && !o.disabled));
        assert_eq!(opts[0].label, "a.wav");
        assert_eq!(opts[1].label, "b.wav");
    }

    #[test]
    fn decode_selection_reset_needed_when_sel_absent() {
        let opts = vec![
            super::ChannelOption {
                stream: 0,
                channel: 0,
                label: String::new(),
                disabled: false,
            },
            super::ChannelOption {
                stream: 0,
                channel: 1,
                label: String::new(),
                disabled: false,
            },
        ];
        assert!(super::needs_decode_selection_reset(&opts, (1, 0)));
        assert!(!super::needs_decode_selection_reset(&opts, (0, 1)));
        assert!(
            !super::needs_decode_selection_reset(&[], (0, 0)),
            "no options → no reset"
        );
    }

    #[test]
    fn ltc_file_idx_clamped_only_when_out_of_range() {
        assert_eq!(super::clamped_ltc_file_idx(3, 2), Some(1));
        assert_eq!(super::clamped_ltc_file_idx(1, 2), None);
        assert_eq!(
            super::clamped_ltc_file_idx(0, 0),
            None,
            "empty group must not clamp"
        );
    }

    #[test]
    fn collect_group_results_mixed_states() {
        let paths = vec![
            PathBuf::from("/x/one.wav"),
            PathBuf::from("/x/two.wav"),
            PathBuf::from("/x/three.wav"),
        ];
        let results = vec![
            ClipDecodeState::Done(Ok(Box::new(make_result(
                LtcDecodeStatus::Success,
                25.0,
                vec![tc_frame(1, 0, 0, 0, 0.0)],
                0.0,
                false,
            )))),
            ClipDecodeState::Done(Err("decode blew up".to_string())),
            ClipDecodeState::Pending,
        ];
        let rows = super::collect_group_results(&paths, &results);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "one.wav");
        assert!(rows[0].result.is_some() && rows[0].error.is_none());
        assert!(rows[1].result.is_none());
        assert_eq!(rows[1].error.as_deref(), Some("decode blew up"));
        assert!(rows[2].result.is_none() && rows[2].error.is_none());
    }

    #[test]
    fn collect_group_results_pending_outlives_paths() {
        // results shorter than paths → trailing entries are pending rows
        let rows = super::collect_group_results(&[PathBuf::from("/x/a.wav")], &[]);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].result.is_none() && rows[0].error.is_none());
    }

    #[test]
    fn status_badge_maps_all_statuses() {
        let colors = test_colors();
        let (icon, color) = super::status_badge(&LtcDecodeStatus::Success, &colors);
        assert_eq!(icon, "✅");
        assert_eq!(color, colors.success_green);
        let (icon, color) = super::status_badge(&LtcDecodeStatus::LowConfidence, &colors);
        assert_eq!(icon, "⚠️");
        assert_eq!(color, colors.warning_amber);
        let (icon, color) = super::status_badge(&LtcDecodeStatus::NoSyncWord, &colors);
        assert_eq!(icon, "❌");
        assert_eq!(color, colors.error_red);
        let (icon, color) = super::status_badge(
            &LtcDecodeStatus::Error {
                message: "x".into(),
            },
            &colors,
        );
        assert_eq!(icon, "❌");
        assert_eq!(color, colors.error_red);
    }

    #[test]
    fn fps_display_formats_drop_frame_and_dash() {
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(super::fps_display(25.0, false), "25.00 fps");
        assert_eq!(super::fps_display(29.97, true), "29.97 fps (Drop Frame)");
        assert_eq!(super::fps_display(0.0, false), "—");
    }

    #[test]
    fn tc_range_summary_formats_pair_and_dash() {
        let a = tc_frame(1, 2, 3, 4, 0.0);
        let b = tc_frame(5, 6, 7, 8, 100.0);
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            super::tc_range_summary(Some(&a), Some(&b), false),
            "01:02:03:04 → 05:06:07:08"
        );
        assert_eq!(
            super::tc_range_summary(Some(&a), Some(&b), true),
            "01;02;03;04 → 05;06;07;08"
        );
        assert_eq!(super::tc_range_summary(None, None, false), "—");
    }

    #[test]
    fn quality_grade_colors_thresholds() {
        let colors = test_colors();
        let (fg, _) = super::quality_grade_colors(0.97, &colors);
        assert_eq!(fg, colors.success_green);
        let (fg, _) = super::quality_grade_colors(0.85, &colors);
        assert_eq!(fg, colors.success_green);
        let (fg, _) = super::quality_grade_colors(0.70, &colors);
        assert_eq!(fg, colors.warning_amber);
        let (fg, _) = super::quality_grade_colors(0.40, &colors);
        assert_eq!(fg, colors.error_red);
        let (fg, _) = super::quality_grade_colors(0.10, &colors);
        assert_eq!(fg, colors.error_red);
        // Backgrounds dim toward the grade color, never brighter than it
        let (_, bg) = super::quality_grade_colors(0.97, &colors);
        assert_ne!(bg, colors.success_green);
    }

    #[test]
    fn quality_issues_text_priorities() {
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(super::quality_issues_text(2, 0, 0, 0), "2 edit(s)");
        assert_eq!(
            super::quality_issues_text(0, 3, 1, 5),
            "1 gap(s), 3 glitch(es)"
        );
        assert_eq!(
            super::quality_issues_text(0, 0, 7, 5),
            "7 gap(s), 0 glitch(es)"
        );
        assert_eq!(super::quality_issues_text(0, 0, 0, 9), "9 missing");
        assert_eq!(super::quality_issues_text(0, 0, 0, 0), "perfect");
    }

    #[test]
    fn codec_label_annotates_hw_only() {
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            super::codec_label("h265".to_string(), true),
            "h265 [HW accel. available]"
        );
        assert_eq!(super::codec_label("h265".to_string(), false), "h265");
    }

    #[test]
    fn option_builders_fall_back_to_supported_lists_without_caps() {
        let containers = super::container_options(None);
        assert!(!containers.is_empty());
        assert!(containers
            .iter()
            .all(|(k, v)| !k.is_empty() && !v.is_empty()));

        let video = super::video_codec_options(None, "mkv");
        assert!(!video.is_empty());
        assert!(video.iter().all(|(k, v)| !k.is_empty() && !v.is_empty()));
        assert!(
            video.iter().all(|(_, v)| !v.contains("HW accel.")),
            "static fallback has no HW annotation"
        );

        let audio = super::audio_encoder_options(None, "mkv");
        assert!(!audio.is_empty());
        assert!(audio.iter().all(|(k, v)| !k.is_empty() && !v.is_empty()));
    }

    #[test]
    fn option_builders_use_probed_lists_with_caps() {
        let mut caps = gui_engine::FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: Default::default(),
            available_formats: Default::default(),
            error_message: None,
            hw: Default::default(),
            ffmpeg_version: None,
        };
        caps.available_formats.insert("matroska".to_string());
        caps.available_encoders.insert("libx264".to_string());
        caps.available_encoders.insert("aac".to_string());

        let containers = super::container_options(Some(&caps));
        assert!(containers.iter().any(|(k, _)| k == "mkv"));

        let video = super::video_codec_options(Some(&caps), "mkv");
        assert!(
            video.iter().any(|(k, _)| k == "h264"),
            "probed encoder must surface as its codec"
        );

        let audio = super::audio_encoder_options(Some(&caps), "mkv");
        assert!(audio.iter().any(|(k, _)| k == "aac"));
    }

    #[test]
    fn group_combo_label_composes_key_badge_count_detail_duration() {
        let g = group_of(RecordingType::MultiTrackAudio, &["a.wav", "b.wav"]);
        // test-lint: allow(text-pin): formatter output is the contract
        let label = super::group_combo_label(&g, &[Some(60.0), Some(60.0)]);
        assert!(
            label.starts_with("mygroup"),
            "display key first, got: {}",
            label
        );
        assert!(label.contains("AUDIO"), "type badge, got: {}", label);
        assert!(
            label.contains("(2 files: a.wav, b.wav)"),
            "count + detail, got: {}",
            label
        );
        assert!(
            label.contains("·"),
            "aggregated duration separator, got: {}",
            label
        );

        let no_dur = super::group_combo_label(&g, &[None, None]);
        assert!(
            !no_dur.contains("·"),
            "no durations → no duration segment, got: {}",
            no_dur
        );

        let video =
            super::group_combo_label(&group_of(RecordingType::VideoClipSequence, &["c.mp4"]), &[]);
        assert!(video.contains("VIDEO"), "video badge, got: {}", video);
        assert!(
            video.contains("(1 file: c.mp4)"),
            "singular file count, got: {}",
            video
        );
    }

    #[test]
    fn folder_display_label_some_and_none() {
        assert_eq!(
            super::folder_display_label(Some(std::path::Path::new("/tmp/cards"))),
            "/tmp/cards"
        );
        assert_eq!(super::folder_display_label(None), "(No folder selected)");
    }

    #[test]
    fn ltc_row_index_finds_selected_pair_across_streams() {
        let p = probe(&[(0, 2), (1, 2)]);
        assert_eq!(super::ltc_row_index(&p, 0, 1), Some(1));
        assert_eq!(super::ltc_row_index(&p, 1, 0), Some(2));
        assert_eq!(super::ltc_row_index(&p, 1, 5), None);
        assert_eq!(super::ltc_row_index(&p, 7, 0), None);
    }

    #[test]
    fn matrix_layout_geometry_and_ltc_flag() {
        let layout = super::matrix_layout(
            2,
            &["A".to_string(), "B".to_string()],
            Some(1),
            &super::MatrixMetrics::design(),
        );
        assert_eq!(layout.cells.len(), 4);
        assert_eq!(layout.headers.len(), 2);
        assert_eq!(layout.row_labels.len(), 2);
        assert_eq!(layout.headers[0].2, "OUT 1");
        assert_eq!(layout.headers[1].2, "OUT 2");
        assert_eq!(layout.row_is_ltc, vec![false, true]);
        assert_eq!(layout.total_width, 44.0 + 36.0 * 2.0 + 8.0);
        assert_eq!(layout.total_height, 24.0 + 36.0 * 2.0 + 8.0);
        // cell (row 1, col 0) center: x = 44 + 0*36 + 18, y = 24 + 1*36 + 18
        let cell = layout
            .cells
            .iter()
            .find(|c| c.row == 1 && c.col == 0)
            .unwrap();
        assert_eq!((cell.cx, cell.cy), (62.0, 78.0));
        // header y is the header band midpoint
        assert_eq!(layout.headers[0].1, 12.0);
        // radio radius derives from the cell size (design: 10 px)
        assert_eq!(layout.cell_radius, 10.0);
    }

    #[test]
    fn matrix_layout_falls_back_to_default_row_labels() {
        let layout = super::matrix_layout(1, &[], None, &super::MatrixMetrics::design());
        assert_eq!(layout.row_labels[0].2, "CH 1");
        assert!(!layout.row_is_ltc[0]);
    }

    #[test]
    fn matrix_layout_scales_geometry_with_metrics() {
        let metrics = super::MatrixMetrics {
            cell_size: 54.0,
            header_height: 36.0,
            label_width: 80.0,
        };
        let layout = super::matrix_layout(2, &[], None, &metrics);
        assert_eq!(layout.total_width, 80.0 + 54.0 * 2.0 + 8.0);
        assert_eq!(layout.total_height, 36.0 + 54.0 * 2.0 + 8.0);
        let cell = layout
            .cells
            .iter()
            .find(|c| c.row == 1 && c.col == 0)
            .unwrap();
        assert_eq!((cell.cx, cell.cy), (80.0 + 27.0, 36.0 + 54.0 + 27.0));
        // the radio radius tracks the scaled cell size
        assert_eq!(layout.cell_radius, 54.0 * (10.0 / 36.0));
    }

    /// Headless egui pass — `Context::run_ui` needs no display backend
    /// (same harness as `widgets/style.rs` tests). Installs the app fonts so
    /// the custom mono family used by the matrix labels is measurable.
    fn run_headless_ui(width: f32, body: impl FnMut(&mut egui::Ui)) {
        let ctx = egui::Context::default();
        crate::theme::install_fonts(&ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, 400.0),
            )),
            ..Default::default()
        };
        let _ = ctx.run_ui(input, body);
    }

    #[test]
    fn matrix_metrics_gutter_grows_to_fit_measured_row_labels() {
        // matrix_metrics multiplies by the process-global text scale; pin it
        // so parallel scale-mutating tests cannot skew the design values.
        let _lock = crate::text::SCALE_LOCK.lock().unwrap();
        crate::text::set_text_scale(100);
        run_headless_ui(800.0, |ui| {
            // The design gutter already fits short labels at 100 % scale.
            let short = super::matrix_metrics(ui, &["CH 1".to_string()]);
            assert_eq!(short.label_width, 44.0);
            // A long label must widen the gutter past the design minimum so
            // it cannot run into the first radio column.
            let long = super::matrix_metrics(ui, &["S12 C8".to_string(), "T1 L".to_string()]);
            let widest = {
                let font = crate::text::TextStyle::MonoValue.font_spec();
                let galley = egui::WidgetText::from(font.with("● S12 C8")).into_galley(
                    ui,
                    Some(egui::TextWrapMode::Extend),
                    f32::INFINITY,
                    egui::FontSelection::Default,
                );
                galley.size().x
            };
            assert!(
                long.label_width >= widest + 8.0,
                "gutter {} must fit measured label {} + padding",
                long.label_width,
                widest
            );
        });
    }

    #[test]
    fn ltc_available_for_start_apply_truth_table() {
        // video group: needs group results, no active decode
        assert!(super::ltc_available_for_start_apply(
            false, true, true, false
        ));
        assert!(!super::ltc_available_for_start_apply(
            false, true, false, true
        ));
        assert!(!super::ltc_available_for_start_apply(
            true, true, true, true
        ));
        // single file: result or error suffices
        assert!(super::ltc_available_for_start_apply(
            false, false, false, true
        ));
        assert!(!super::ltc_available_for_start_apply(
            false, false, true, false
        ));
        assert!(!super::ltc_available_for_start_apply(
            true, false, false, true
        ));
    }

    #[test]
    fn preview_display_name_strips_output_folder_with_fallback() {
        let folder = std::path::Path::new("/out");
        assert_eq!(
            super::preview_display_name(std::path::Path::new("/out/dev_clip1_tr1.wav"), folder),
            "dev_clip1_tr1.wav",
        );
        // path outside the output folder → full path fallback
        assert_eq!(
            super::preview_display_name(std::path::Path::new("/elsewhere/x.wav"), folder),
            "/elsewhere/x.wav",
        );
    }
}
