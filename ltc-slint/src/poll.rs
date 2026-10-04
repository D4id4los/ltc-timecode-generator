use std::f64::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::converter::{format_blockers, RecordingType};
use gui_engine::{JobKind, JobPhase, UnitState};
use gui_engine::duration::{format_duration_secs, group_duration_secs};
use gui_engine::edit_state::EditState;
use gui_engine::log_buffer::LogBuffer;
use gui_engine::state::AppStateSnapshot;
use gui_engine::timecode;
use gui_engine::{ArcSwap, AudioEvent, ChannelSel, SAMPLE_RATE_OPTIONS};
use log::info;
use slint::{ModelRc, SharedString, VecModel};

use crate::shadows::Shadows;
use crate::toast::{push_toast, ToastItem};
use crate::timecode_helpers::set_tc_segments;
use crate::{AppColors, AppWindow, FileGroupInfo, LogEntry};
use slint::ComponentHandle;
use slint::Global;

const POLL_INTERVAL_MS: u64 = 40;

// ── Pure extractors ─────────────────────────────────────────────────────
// Each moves a slice of the tick closure's logic out verbatim so it can be
// unit-tested against hand-built snapshots (the crate has no UI harness).

/// Map a routing selection to the settings combo index.
fn channel_index(ch: ChannelSel) -> i32 {
    match ch {
        ChannelSel::Left => 0,
        ChannelSel::Right => 1,
        ChannelSel::Both => 2,
    }
}

/// Advance the clapper pulse phase by one poll interval.
fn advance_pulse(phase: f64) -> f64 {
    let advanced = phase + 4.0 * 2.0 * PI * (POLL_INTERVAL_MS as f64 / 1000.0);
    if advanced > PI * 100.0 { 0.0 } else { advanced }
}

/// True when the engine's monotonic `clap_seq` has moved past the last
/// value the GUI animated — the one-shot trigger for the declarative clap
/// animation (see `ui/app.slint`'s `flash-strike` state machine).
fn clap_started(new_seq: u64, last_seen: u64) -> bool {
    new_seq != last_seen
}

/// Device list entry label, marking the system default device.
fn device_display_name(name: &str, is_default: bool) -> String {
    if is_default {
        format!("{} (Default)", name)
    } else {
        name.to_string()
    }
}

/// (status, result_text, error) triple for the LTC decode panel, derived
/// from the engine snapshot's job states and decode snapshot.
fn format_decode_status(s: &AppStateSnapshot) -> (String, String, String) {
    if s.job(JobKind::LtcDecode).is_active() || s.job(JobKind::LtcGroupDecode).is_active() {
        return ("detecting".to_string(), String::new(), String::new());
    }
    if let Some(ref err) = s.decode.error {
        return ("error".to_string(), String::new(), err.clone());
    }
    if let Some(ref r) = s.decode.result {
        let status_str = match &r.status {
            gui_engine::LtcDecodeStatus::Success => "success",
            gui_engine::LtcDecodeStatus::LowConfidence => "low_confidence",
            gui_engine::LtcDecodeStatus::NoSyncWord => "no_sync",
            gui_engine::LtcDecodeStatus::Error { .. } => "error",
        };

        let drop_flag = if r.drop_frame { " DF" } else { "" };
        let fps_str = if r.detected_fps > 0.0 {
            format!("{:.2} fps{}", r.detected_fps, drop_flag)
        } else {
            "—".to_string()
        };
        let first = r.timecodes.first().map(|t| t.timecode);
        let last_tc = r.timecodes.last().map(|t| t.timecode);
        let tc_range = match (first, last_tc) {
            (Some(f), Some(l)) => {
                let sep = if r.drop_frame { ";" } else { ":" };
                format!(
                    "{:02}{sep}{:02}{sep}{:02}{sep}{:02} → {:02}{sep}{:02}{sep}{:02}{sep}{:02}",
                    f.hours, f.minutes, f.seconds, f.frames,
                    l.hours, l.minutes, l.seconds, l.frames,
                )
            }
            _ => "—".to_string(),
        };
        let quality_str = r.quality.as_ref().map(|q| {
            let issues = if q.edit_count > 0 {
                format!("{} edit(s)", q.edit_count)
            } else if q.glitch_count > 0 || q.gap_count > 0 {
                format!("{}/{} gap/glitch", q.gap_count, q.glitch_count)
            } else if q.missing_frames > 0 {
                format!("{} missing", q.missing_frames)
            } else {
                "perfect".to_string()
            };
            format!(" | Quality: {:.0}% ({}) {:.1}% usable, {}",
                q.score * 100.0, q.grade, q.usable_coverage * 100.0, issues)
        }).unwrap_or_default();
        let result_text = format!(
            "{} | Conf: {:.1}% | Frames: {}/{} | {} | {:.1}ms{}",
            fps_str,
            r.avg_confidence * 100.0,
            r.valid_frames,
            r.total_possible_frames,
            tc_range,
            r.processing_time_ms,
            quality_str,
        );
        return (status_str.to_string(), result_text, String::new());
    }
    (String::new(), String::new(), String::new())
}

/// Flat per-(stream, channel) labels for the decode channel list, plus the
/// list row index of the currently selected stream/channel (-1 if none).
fn build_channel_labels(
    probe: &gui_engine::ffprobe::VideoAudioProbe,
    selected_stream: usize,
    selected_channel: usize,
) -> (Vec<String>, i32) {
    let mut channel_names_vec: Vec<String> = Vec::new();
    let mut ltc_row_idx: i32 = -1;
    for s_info in &probe.streams {
        for ch in 0..s_info.channels {
            let idx = channel_names_vec.len();
            let label = if probe.streams.len() > 1 {
                format!("S{} C{}", s_info.stream_index, ch + 1)
            } else if s_info.channels == 2 {
                format!("T1 {}", if ch == 0 { "L" } else { "R" })
            } else {
                format!("Ch {}", ch + 1)
            };
            channel_names_vec.push(label);
            if s_info.stream_index == selected_stream && ch == selected_channel {
                ltc_row_idx = idx as i32;
            }
        }
    }
    (channel_names_vec, ltc_row_idx)
}

/// Converter file-group list model from the snapshot.
fn build_group_model(s: &AppStateSnapshot) -> Vec<FileGroupInfo> {
    s.converter.groups.iter().map(|g| {
        let durs: Vec<Option<f64>> = g.files.iter()
            .map(|f| {
                let full_path = s.converter.groups_folder.as_ref().map(|p| p.join(f));
                full_path.and_then(|p| s.file_durations.get(&p).copied().flatten())
            })
            .collect();
        let dur_text = group_duration_secs(
            &g.recording_type, &durs,
        ).map(format_duration_secs).unwrap_or_default();
        FileGroupInfo {
            prefix: SharedString::from(g.prefix.clone()),
            files: ModelRc::new(VecModel::<SharedString>::from(
                g.files.iter().map(|f| {
                    SharedString::from(f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                }).collect::<Vec<_>>()
            )),
            channel_count: g.files.len() as i32,
            is_audio_recording: matches!(g.recording_type, RecordingType::MultiTrackAudio),
            duration_text: SharedString::from(dur_text),
        }
    }).collect()
}

/// Conversion status line for the converter panel ("idle", "running N%", …).
fn conversion_status_text(jc: &gui_engine::JobStatus) -> String {
    match jc.phase() {
        JobPhase::Idle => "idle".to_string(),
        JobPhase::Running | JobPhase::Indeterminate => {
            format!("running {:.0}%", jc.fraction() * 100.0)
        }
        JobPhase::Succeeded => "completed".to_string(),
        JobPhase::Cancelled => "cancelled".to_string(),
        JobPhase::Failed => "failed".to_string(),
    }
}

/// Per-file rows for one offload card.
fn build_offload_file_infos(
    card: &gui_engine::offload::SdCardInfo,
    off: &gui_engine::offload::OffloadSnapshot,
) -> Vec<crate::OffloadFileInfo> {
    card.files.iter()
        .zip(card.selected.iter())
        .map(|(f, &sel)| {
            let date_text = f.modified
                .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                .unwrap_or_default();
            let dur_text = off.file_durations.get(&f.path)
                .and_then(|opt| *opt)
                .map(format_duration_secs)
                .unwrap_or_else(|| {
                    if off.file_durations.contains_key(&f.path) {
                        "—".to_string()
                    } else {
                        "…".to_string()
                    }
                });
            crate::OffloadFileInfo {
                name: SharedString::from(f.name.clone()),
                date_text: SharedString::from(date_text),
                duration_text: SharedString::from(dur_text),
                size_text: SharedString::from(format_bytes(f.size_bytes)),
                selected: sel,
            }
        }).collect()
}

/// Card list model for the offload panel.
fn build_offload_card_infos(off: &gui_engine::offload::OffloadSnapshot) -> Vec<crate::OffloadCardInfo> {
    off.cards.iter().map(|card| {
        let files = build_offload_file_infos(card, off);
        crate::OffloadCardInfo {
            mount: SharedString::from(card.mount.to_string_lossy().as_ref()),
            volume_label: SharedString::from(card.volume_label.clone()),
            device_name: SharedString::from(card.device_name.clone()),
            name_source: SharedString::from(format!("{:?}", card.name_source)),
            media_file_count: card.media_file_count as i32,
            total_bytes: SharedString::from(format_bytes(card.total_bytes)),
            files: ModelRc::new(VecModel::<crate::OffloadFileInfo>::from(files)),
            selected_count: card.selected_count as i32,
            selected_bytes: SharedString::from(format_bytes(card.selected_bytes)),
        }
    }).collect()
}

/// Per-device copy-progress rows for the offload panel.
fn build_offload_device_status(
    off: &gui_engine::offload::OffloadSnapshot,
    copy_job: &gui_engine::JobStatus,
) -> Vec<crate::OffloadDeviceStatus> {
    off.device_totals.iter().enumerate().map(|(i, dt)| {
        let unit = copy_job.units().get(i);
        let state_text = match unit.map(|u| u.state) {
            Some(UnitState::Pending) => "Pending",
            Some(UnitState::Running) => "Copying",
            Some(UnitState::Done) => "Done",
            Some(UnitState::Failed) => "Failed",
            Some(UnitState::Skipped) => "Skipped",
            None => "Pending",
        };
        let fraction = unit.map(|u| u.fraction).unwrap_or(0.0);
        let bytes_done = (dt.bytes_total as f64 * fraction as f64) as u64;
        let bytes_text = if dt.bytes_total > 0 {
            format!("{} / {}", format_bytes(bytes_done), format_bytes(dt.bytes_total))
        } else {
            String::new()
        };
        let files_done = (dt.files_total as f32 * fraction) as i32;
        let current_file = unit.map(|u| u.message.clone()).unwrap_or_default();
        let error_str = match unit.map(|u| u.state) {
            Some(UnitState::Failed) => current_file.clone(),
            _ => String::new(),
        };
        crate::OffloadDeviceStatus {
            device_name: SharedString::from(dt.name.clone()),
            state_text: SharedString::from(state_text),
            files_total: dt.files_total as i32,
            files_done,
            progress: fraction,
            bytes_text: SharedString::from(bytes_text),
            current_file: SharedString::from(current_file),
            error: SharedString::from(error_str),
        }
    }).collect()
}

/// Map an audio event to its toast (message, severity) pair.
fn audio_event_to_notification(event: &AudioEvent) -> (String, &'static str) {
    match event {
        AudioEvent::StreamError(m) => (m.clone(), "error"),
        AudioEvent::StreamDied => ("Audio stream died".to_string(), "error"),
        AudioEvent::StreamRecovering { attempt } => {
            (format!("Stream recovering (attempt {})", attempt), "warning")
        }
        AudioEvent::StreamDead => ("Audio device unreachable".to_string(), "error"),
        AudioEvent::RecoveryNeeded { reason } => {
            (format!("Audio recovery needed: {}", reason), "warning")
        }
        AudioEvent::Underrun => ("Audio underrun".to_string(), "warning"),
        AudioEvent::FramesDropped { total } => {
            (format!("{} frames dropped", total), "warning")
        }
    }
}

// ── Poll context ────────────────────────────────────────────────────────
// Everything the tick closure captures, packaged so `setup_poll_timer`
// takes exactly two parameters.

pub(crate) struct PollContext {
    engine_state: Arc<ArcSwap<AppStateSnapshot>>,
    event_rx: std::sync::mpsc::Receiver<AudioEvent>,
    toasts: Arc<Mutex<Vec<ToastItem>>>,
    next_toast_id: Arc<Mutex<i32>>,
    log_buffer: Arc<Mutex<LogBuffer>>,
    last_debug_log_count: Arc<Mutex<usize>>,
    pulse_phase: Arc<Mutex<f64>>,
    shadows: Arc<Mutex<Shadows>>,
    last_log_count: Arc<Mutex<usize>>,
    last_device_key: Arc<Mutex<(usize, String)>>,
    /// Last `clapper.clap_seq` the GUI animated (seeded from the snapshot so
    /// a mid-session start does not fire for an old clap).
    last_clap_seq: Arc<Mutex<u64>>,
}

impl PollContext {
    pub(crate) fn new(
        engine_state: Arc<ArcSwap<AppStateSnapshot>>,
        event_rx: std::sync::mpsc::Receiver<AudioEvent>,
        toasts: Arc<Mutex<Vec<ToastItem>>>,
        next_toast_id: Arc<Mutex<i32>>,
        log_buffer: Arc<Mutex<LogBuffer>>,
        last_debug_log_count: Arc<Mutex<usize>>,
        pulse_phase: Arc<Mutex<f64>>,
        shadows: Arc<Mutex<Shadows>>,
    ) -> Self {
        let initial_clap_seq = engine_state.load().clapper.clap_seq;
        Self {
            engine_state,
            event_rx,
            toasts,
            next_toast_id,
            log_buffer,
            last_debug_log_count,
            pulse_phase,
            shadows,
            last_log_count: Arc::new(Mutex::new(0)),
            last_device_key: Arc::new(Mutex::new((0, String::new()))),
            last_clap_seq: Arc::new(Mutex::new(initial_clap_seq)),
        }
    }
}

// ── Per-tick sync functions ─────────────────────────────────────────────
// Each moves one block (or a small group of related blocks) of the former
// tick-closure body out verbatim.

fn sync_clock_and_theme(ui: &AppWindow, s: &AppStateSnapshot) {
    // 1. System time
    ui.set_system_time(SharedString::from(format!("{} UTC", timecode::chrono_now_string())));

    // 2. Theme sync
    AppColors::get(ui).set_theme_dark(s.is_dark_theme);
}

fn sync_clapper_metadata(ui: &AppWindow, s: &AppStateSnapshot, applied_seq: u64, now: Instant, sh: &mut Shadows) {
    // 3. Clapper metadata (fix one-way sync gaps). Shadow-backed:
    //    the roll push is skipped while an edit awaits its ack (the
    //    ROLL TextInput is two-way bound to the `roll` property);
    //    auto-increment likewise until its toggle is acked.
    if let Some(roll) = sh.roll.sync_and_push(
        &s.clapper.roll,
        ui.get_roll_editing(),
        applied_seq,
        now,
    ) {
        ui.set_roll(SharedString::from(roll));
    }
    ui.set_scene(s.clapper.scene as i32);
    ui.set_take(s.clapper.take as i32);
    push_shadow(&mut sh.auto_increment, &s.clapper.auto_increment_take, applied_seq, now,
        |v| ui.set_auto_increment(v));
}

fn sync_decode_state(ui: &AppWindow, s: &AppStateSnapshot) {
    // 4. LTC decode state sync
    {
        let (status, result_text, error) = format_decode_status(s);
        ui.set_ltc_status(SharedString::from(status));
        ui.set_ltc_result_text(SharedString::from(result_text));
        ui.set_ltc_error(SharedString::from(error));
    }
}

fn sync_decode_fps_shadow(ui: &AppWindow, s: &AppStateSnapshot, applied_seq: u64, now: Instant, sh: &mut Shadows) {
    // 5. LTC decode FPS index sync (shadow-backed, gated on ack)
    push_shadow(&mut sh.decode_fps_index, &s.decode.fps_index, applied_seq, now,
        |v| ui.set_decode_fps_index(v as i32));
}

fn sync_channel_labels(ui: &AppWindow, s: &AppStateSnapshot) {
    // 6. Video audio probe info
    if let Some(ref probe) = s.decode.probe {
        let (labels, ltc_row_idx) = build_channel_labels(
            probe, s.decode.selected_stream, s.decode.selected_channel,
        );
        ui.set_ltc_channel_names(ModelRc::new(VecModel::from(
            labels.into_iter().map(SharedString::from).collect::<Vec<_>>(),
        )));
        ui.set_conv_ltc_row_index(ltc_row_idx);
    } else {
        ui.set_ltc_channel_names(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
        ui.set_conv_ltc_row_index(-1);
    }
}

fn sync_probe_flags(ui: &AppWindow, s: &AppStateSnapshot) {
    // 7. Probe-status flags for video recordings
    if ui.get_conv_is_video_recording() {
        ui.set_conv_ltc_probe_loading(s.job(JobKind::ClipProbe).is_active());
        ui.set_conv_ltc_probe_failed(s.decode.probe.is_none() && !s.job(JobKind::ClipProbe).is_active());
    } else {
        ui.set_conv_ltc_probe_loading(false);
        ui.set_conv_ltc_probe_failed(false);
    }
}

fn sync_decode_progress(ui: &AppWindow, s: &AppStateSnapshot) {
    // 8. Sync decode progress
    if s.job(JobKind::LtcDecode).is_active() || s.job(JobKind::LtcGroupDecode).is_active() {
        ui.set_ltc_decode_progress(s.job(JobKind::LtcDecode).fraction());
    }
}

fn sync_pulse(ui: &AppWindow, pulse_phase: &Mutex<f64>) {
    // 9. Pulse phase animation
    let mut pp = pulse_phase.lock().unwrap();
    *pp = advance_pulse(*pp);
    ui.set_pulse_phase(*pp as f32);
}

fn sync_transport_display(ui: &AppWindow, s: &AppStateSnapshot) {
    // 10. Timecode display
    let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame());
    let ms_str = timecode::timecode_to_ms_string(s.current_timecode, s.fps());
    set_tc_segments(ui, &tc_str);
    ui.set_ms_text(SharedString::from(ms_str));

    // 11. Transport state
    ui.set_is_playing(s.is_playing);
    ui.set_is_locked(s.is_locked);
    ui.set_status_message(SharedString::from(s.status.message().to_string()));

    // 12. FPS name
    let fps_name = SharedString::from(gui_engine::timecode::FPS_OPTIONS[s.fps_index].name);
    ui.set_fps_name(fps_name);

    // 13. Routing pills
    ui.set_ltc_route(SharedString::from(s.ltc_channel.as_str().to_uppercase()));
    ui.set_beep_route(SharedString::from(s.beep_channel.as_str().to_uppercase()));

    // 14. LTC/beep channel indices (fix one-way gaps)
    {
        ui.set_ltc_channel_index(channel_index(s.ltc_channel));
        ui.set_beep_channel_index(channel_index(s.beep_channel));
    }

    // 15. Sample rate metadata
    let rate_khz = format!("{:.1}", s.sample_rate as f32 / 1000.0);
    ui.set_sample_rate_khz(SharedString::from(rate_khz));
    let buffer_smp = (s.sample_rate as f64 / s.fps()).round() as i32;
    ui.set_buffer_size(buffer_smp);
    ui.set_sample_format(SharedString::from(s.sample_format_name.to_uppercase()));

}

fn sync_clap_strike(ui: &AppWindow, s: &AppStateSnapshot, last_clap_seq: &Mutex<u64>) {
    let mut last = last_clap_seq.lock().unwrap();
    if clap_started(s.clapper.clap_seq, *last) {
        *last = s.clapper.clap_seq;
        ui.set_flash_strike(true);
    } else if ui.get_flash_strike() {
        ui.set_flash_strike(false);
    }
}

fn sync_device_selection(ui: &AppWindow, s: &AppStateSnapshot, applied_seq: u64, now: Instant, sh: &mut Shadows) {
    // 17. Device selection sync
    let dev_idx = s.selected_device.as_ref()
        .and_then(|id| s.devices.iter().position(|d| &d.id == id))
        .map(|i| i as i32)
        .unwrap_or(-1);
    ui.set_device_index(dev_idx);

    // 18. Volume / pitch / duration (shadow-backed, gated on ack —
    //     avoids fighting the user mid-drag)
    push_shadow(&mut sh.ltc_volume, &s.ltc_volume, applied_seq, now,
        |v| ui.set_ltc_volume(v));
    push_shadow(&mut sh.beep_volume, &s.beep_volume, applied_seq, now,
        |v| ui.set_beep_volume(v));
    push_shadow(&mut sh.beep_frequency, &s.beep_frequency, applied_seq, now,
        |v| ui.set_beep_frequency(v));
    push_shadow(&mut sh.beep_duration, &s.beep_duration, applied_seq, now,
        |v| ui.set_beep_duration(v));

    // 19. Start timecode steppers
    ui.set_hour(s.start_timecode.hours as i32);
    ui.set_minute(s.start_timecode.minutes as i32);
    ui.set_second(s.start_timecode.seconds as i32);
    let max_frame = s.fps().round() as i32;
    ui.set_max_frame(if max_frame > 0 { max_frame - 1 } else { 0 });
    ui.set_frame(s.start_timecode.frames as i32);

    // 20. FPS and sample rate index (shadow-backed, gated on ack)
    push_shadow(&mut sh.fps_index, &s.fps_index, applied_seq, now,
        |v| ui.set_fps_index(v as i32));
    let sr_truth = SAMPLE_RATE_OPTIONS.iter()
        .position(|&r| r == s.sample_rate)
        .unwrap_or(0);
    push_shadow(&mut sh.sample_rate, &sr_truth, applied_seq, now,
        |v| ui.set_sample_rate_index(v as i32));
}

fn sync_converter_settings(ui: &AppWindow, s: &AppStateSnapshot, applied_seq: u64, now: Instant, sh: &mut Shadows) {
    // 21. Converter user settings sync — shadow-backed, gated on ack
    {
        push_shadow(&mut sh.conv.container, &s.converter.settings.container, applied_seq, now,
            |v| ui.set_conv_container(SharedString::from(v)));
        push_shadow(&mut sh.conv.video_encoder, &s.converter.settings.video_encoder, applied_seq, now,
            |v| ui.set_conv_video_encoder(SharedString::from(v)));
        push_shadow(&mut sh.conv.audio_encoder, &s.converter.settings.audio_encoder, applied_seq, now,
            |v| ui.set_conv_audio_encoder(SharedString::from(v)));
        push_shadow(&mut sh.conv.split_tracks, &s.converter.settings.split_tracks, applied_seq, now,
            |v| ui.set_conv_split_tracks(v));
        push_shadow(&mut sh.conv.drop_ltc_track, &s.converter.settings.drop_ltc_track, applied_seq, now,
            |v| ui.set_conv_drop_ltc_track(v));
        push_shadow(&mut sh.conv.concat_audio, &s.converter.settings.concat_audio, applied_seq, now,
            |v| ui.set_conv_concat_audio(v));
        push_shadow(&mut sh.conv.generate_synthetic_video, &s.converter.settings.generate_synthetic_video, applied_seq, now,
            |v| ui.set_conv_generate_synthetic_video(v));
        push_shadow(&mut sh.conv.copy_video, &s.converter.settings.copy_video, applied_seq, now,
            |v| ui.set_conv_copy_video(v));
        push_shadow(&mut sh.conv.metadata_only, &s.converter.settings.metadata_only, applied_seq, now,
            |v| ui.set_conv_metadata_only(v));
        push_shadow(&mut sh.conv.embed_camera_metadata, &s.converter.settings.embed_camera_metadata, applied_seq, now,
            |v| ui.set_conv_embed_camera_meta(v));
        push_shadow(&mut sh.conv.set_start_from_ltc, &s.converter.settings.set_start_from_ltc, applied_seq, now,
            |v| ui.set_set_start_from_ltc(v));
        // Text fields: skip the push while an edit awaits its ack,
        // and never re-push the text the user already typed.
        let editing = ui.get_conv_text_editing();
        if let Some(v) = sh.conv.filename_prefix.sync_and_push(
            &s.converter.settings.filename_prefix, editing, applied_seq, now,
        ) {
            ui.set_conv_filename_prefix(SharedString::from(v));
        }
        if let Some(v) = sh.conv.audio_suffix_template.sync_and_push(
            &s.converter.settings.audio_suffix_template, editing, applied_seq, now,
        ) {
            ui.set_conv_audio_suffix_template(SharedString::from(v));
        }
        if let Some(v) = sh.conv.video_suffix_template.sync_and_push(
            &s.converter.settings.video_suffix_template, editing, applied_seq, now,
        ) {
            ui.set_conv_video_suffix_template(SharedString::from(v));
        }
        ui.set_conv_output_folder(SharedString::from(
            s.converter.settings.output_folder.to_string_lossy().as_ref(),
        ));

        // Channel map
        if s.converter.settings.channel_map.num_channels() > 0 {
            let n = s.converter.settings.channel_map.num_channels();
            let map_vec: Vec<i32> = (0..n)
                .map(|i| s.converter.settings.channel_map.get(i) as i32)
                .collect();
            ui.set_conv_channel_map(ModelRc::new(VecModel::from(map_vec)));
        }
    }
}

fn sync_ffmpeg_caps(ui: &AppWindow, s: &AppStateSnapshot) {
    // 22. ffmpeg capability probe state sync
    {
        ui.set_conv_ffmpeg_probing(s.job(JobKind::FfmpegCapProbe).is_active());
        if let Some(ref caps) = s.ffmpeg_caps {
            ui.set_conv_has_ffmpeg(caps.has_ffmpeg);
            if let Some(ref msg) = caps.error_message {
                ui.set_conv_ffmpeg_error(SharedString::from(msg));
            }
        }
    }
}

/// Former per-tick block 23 (converter dropdown option models): these are
/// static data, so they are set once instead of on every tick.
fn set_static_converter_models(ui: &AppWindow) {
    let container_options: Vec<SharedString> = gui_engine::converter::supported_containers()
        .iter().map(|(k, _)| SharedString::from(*k)).collect();
    ui.set_conv_container_options(ModelRc::new(VecModel::<SharedString>::from(container_options)));

    let video_options: Vec<SharedString> = gui_engine::video_codecs::supported_video_codecs()
        .iter().map(|(k, desc)| SharedString::from(format!("{} — {}", k, desc))).collect();
    ui.set_conv_video_encoder_options(ModelRc::new(VecModel::<SharedString>::from(video_options)));

    let audio_options: Vec<SharedString> = gui_engine::converter::supported_audio_encoders()
        .iter().map(|(k, _)| SharedString::from(*k)).collect();
    ui.set_conv_audio_encoder_options(ModelRc::new(VecModel::<SharedString>::from(audio_options)));
}

fn sync_converter_groups(ui: &AppWindow, s: &AppStateSnapshot, applied_seq: u64, now: Instant, sh: &mut Shadows) {
    // 24. Converter group + file info sync
    {
        let groups = &s.converter.groups;
        let selected_idx = s.converter.selected_group_idx;
        ui.set_conv_selected_group_idx(selected_idx.map(|i| i as i32).unwrap_or(-1));

        let group_model: Vec<FileGroupInfo> = build_group_model(s);
        ui.set_conv_file_groups(ModelRc::new(VecModel::<FileGroupInfo>::from(group_model)));

        if let Some(idx) = selected_idx {
            if idx < groups.len() {
                let files = &groups[idx].files;
                let ltc_file_names: Vec<SharedString> = files.iter().map(|f| {
                    SharedString::from(f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                }).collect();
                let is_audio = matches!(groups[idx].recording_type, RecordingType::MultiTrackAudio);
                ui.set_conv_is_video_recording(!is_audio);
                ui.set_conv_num_channels(if is_audio { files.len() as i32 } else { 0 });
                push_shadow(&mut sh.conv.ltc_file_idx, &s.converter.settings.ltc_file_idx, applied_seq, now,
                    |v| ui.set_ltc_file_idx(v as i32));
                ui.set_ltc_file_names(ModelRc::new(VecModel::<SharedString>::from(ltc_file_names)));
            }
        }

        if let Some(ref folder) = s.converter.groups_folder {
            ui.set_conv_selected_folder(SharedString::from(folder.to_string_lossy().as_ref()));
        }
    }
}

fn sync_conversion_status(ui: &AppWindow, s: &AppStateSnapshot) {
    // 25. Conversion state sync (from engine-owned snapshot via JobStatus)
    {
        let jc = s.job(JobKind::Conversion);
        ui.set_conv_status(SharedString::from(conversion_status_text(jc)));
        ui.set_conv_progress(jc.fraction());
        ui.set_conv_log(SharedString::from(jc.log().to_string()));
    }

    // 26. Readiness / sanity message
    {
        let msg = if s.converter.readiness.is_empty() {
            String::new()
        } else {
            format_blockers(&s.converter.readiness)
        };
        ui.set_conv_sanity_msg(SharedString::from(msg));
        ui.set_conv_collision_warning(SharedString::from(
            s.converter.collision_warning.as_deref().unwrap_or(""),
        ));
        ui.set_conv_duplicate_output_warning(SharedString::from(
            s.converter.duplicate_output_warning.as_deref().unwrap_or(""),
        ));
    }
}

fn sync_clapper_logs(ui: &AppWindow, s: &AppStateSnapshot, last_log_count: &Mutex<usize>) {
    // 27. Log entries (only rebuild if log count changed)
    {
        let current_log_count = s.clapper.logs.len();
        let mut last = last_log_count.lock().unwrap();
        if current_log_count != *last {
            let log_entries: Vec<LogEntry> = s.clapper.logs
                .iter()
                .map(|l| LogEntry {
                    timestamp: SharedString::from(&l.timestamp),
                    timecode: SharedString::from(&l.timecode),
                    milliseconds: SharedString::from(&l.milliseconds),
                    note: SharedString::from(&l.note),
                })
                .collect();
            ui.set_logs(ModelRc::new(VecModel::<LogEntry>::from(log_entries)));
            *last = current_log_count;
        }
    }
}

fn sync_device_names(ui: &AppWindow, s: &AppStateSnapshot, last_device_key: &Mutex<(usize, String)>) {
    // 28. Device names (only rebuild if device list changed)
    {
        let device_key = (
            s.devices.len(),
            s.devices.iter().map(|d| d.id.clone()).collect::<Vec<_>>().join("\n"),
        );
        let mut last = last_device_key.lock().unwrap();
        if device_key != *last {
            let device_names: Vec<SharedString> = s.devices
                .iter()
                .map(|d| SharedString::from(device_display_name(&d.name, d.is_default)))
                .collect();
            ui.set_device_names(ModelRc::new(VecModel::<SharedString>::from(device_names)));
            ui.set_device_count(s.devices.len() as i32);
            *last = device_key;
        }
    }
}

fn drain_audio_events(
    event_rx: &std::sync::mpsc::Receiver<AudioEvent>,
    toasts: &Arc<Mutex<Vec<ToastItem>>>,
    next_toast_id: &Arc<Mutex<i32>>,
    ui: &AppWindow,
) {
    // 29. Process events into toasts — drained from the
    // audio-event channel, not the snapshot (one-shot mailbox).
    while let Ok(event) = event_rx.try_recv() {
        let (msg, typ) = audio_event_to_notification(&event);
        push_toast(toasts, next_toast_id, ui, &msg, typ);
    }
}

fn sync_debug_log(
    ui: &AppWindow,
    log_buffer: &Mutex<LogBuffer>,
    last_debug_log_count: &Mutex<usize>,
) {
    // 30. Debug log sync
    if ui.get_show_debug_log() {
        let entries = log_buffer.lock().unwrap();
        let current_count = entries.entries.len();
        let mut last_count = last_debug_log_count.lock().unwrap();
        if current_count != *last_count {
            let model: Vec<SharedString> = entries.entries.iter()
                .map(|s| SharedString::from(s.as_str()))
                .collect();
            drop(entries);
            ui.set_debug_log_entries(ModelRc::new(VecModel::<SharedString>::from(model)));
            *last_count = current_count;
        }
    }
}

fn sync_offload(ui: &AppWindow, s: &AppStateSnapshot, applied_seq: u64, now: Instant, sh: &mut Shadows) {
    // 31. Offload state sync
    {
        let off = &s.offload;
        ui.set_off_parent_folder(SharedString::from(
            off.parent_folder.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
        ));
        if let Some(v) = sh.parent_name.sync_and_push(&off.parent_name, false, applied_seq, now) {
            ui.set_off_parent_name(SharedString::from(v));
        }
        let scan_job = s.job(JobKind::OffloadScan);
        ui.set_off_scanning(scan_job.is_active());
        ui.set_off_scan_status(SharedString::from(scan_job.message().to_string()));
        let copy_job = s.job(JobKind::OffloadCopy);
        ui.set_off_running(copy_job.is_active());
        ui.set_off_overall_progress(copy_job.fraction());
        let speed_bps = copy_job.speed().unwrap_or(0.0);
        ui.set_off_speed_text(SharedString::from(
            if speed_bps > 0.0 {
                format!("{} / s", format_bytes(speed_bps as u64))
            } else {
                String::new()
            },
        ));
        ui.set_off_error(SharedString::from(off.error.clone().unwrap_or_default()));

        let card_infos: Vec<crate::OffloadCardInfo> = build_offload_card_infos(off);
        ui.set_off_cards(ModelRc::new(VecModel::<crate::OffloadCardInfo>::from(card_infos)));

        let device_status: Vec<crate::OffloadDeviceStatus> =
            build_offload_device_status(off, copy_job);
        ui.set_off_device_progress(ModelRc::new(VecModel::<crate::OffloadDeviceStatus>::from(device_status)));

        let completed: Vec<SharedString> = off.completed_devices.iter()
            .map(|d| SharedString::from(d.clone()))
            .collect();
        ui.set_off_completed_devices(ModelRc::new(VecModel::<SharedString>::from(completed)));
    }
}

pub fn setup_poll_timer(
    ui: &AppWindow,
    ctx: PollContext,
) {
    set_static_converter_models(ui);

    let PollContext {
        engine_state,
        event_rx,
        toasts,
        next_toast_id,
        log_buffer,
        last_debug_log_count,
        pulse_phase,
        shadows,
        last_log_count,
        last_device_key,
        last_clap_seq,
    } = ctx;

    let ui_weak = ui.as_weak();

    let poll_timer = slint::Timer::default();
    poll_timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(POLL_INTERVAL_MS),
        move || {
            let poll_start = Instant::now();
            static POLL_COUNTER: AtomicU64 = AtomicU64::new(0);
            let tick = POLL_COUNTER.fetch_add(1, Ordering::Relaxed);

            let ui = match ui_weak.upgrade() {
                Some(u) => u,
                None => return,
            };

            let s = engine_state.load();
            let applied_seq = s.applied_command_seq;
            let now = Instant::now();
            let mut sh = shadows.lock().unwrap();

            sync_clock_and_theme(&ui, &s);
            sync_clapper_metadata(&ui, &s, applied_seq, now, &mut sh);
            sync_decode_state(&ui, &s);
            sync_decode_fps_shadow(&ui, &s, applied_seq, now, &mut sh);
            sync_channel_labels(&ui, &s);
            sync_probe_flags(&ui, &s);
            sync_decode_progress(&ui, &s);
            sync_pulse(&ui, &pulse_phase);
            sync_transport_display(&ui, &s);
            sync_clap_strike(&ui, &s, &last_clap_seq);
            sync_device_selection(&ui, &s, applied_seq, now, &mut sh);
            sync_converter_settings(&ui, &s, applied_seq, now, &mut sh);
            sync_ffmpeg_caps(&ui, &s);
            sync_converter_groups(&ui, &s, applied_seq, now, &mut sh);
            sync_conversion_status(&ui, &s);
            sync_clapper_logs(&ui, &s, &last_log_count);
            sync_device_names(&ui, &s, &last_device_key);
            drain_audio_events(&event_rx, &toasts, &next_toast_id, &ui);
            sync_debug_log(&ui, &log_buffer, &last_debug_log_count);
            sync_offload(&ui, &s, applied_seq, now, &mut sh);

            // 32. Perf diagnostics
            let poll_elapsed = poll_start.elapsed();
            if tick % 250 == 0 {
                info!("[PERF] Poll tick #{}: {}ms", tick, poll_elapsed.as_micros() as f64 / 1000.0);
            }
        },
    );

    Box::leak(Box::new(poll_timer));
}

fn push_shadow<T: PartialEq + Clone>(
    es: &mut EditState<T>,
    truth: &T,
    applied_seq: u64,
    now: Instant,
    set: impl FnOnce(T),
) {
    es.set_focused(false);
    es.sync(truth, applied_seq, now);
    if !es.is_pending() {
        set(es.value().clone());
    }
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut size = bytes as f64;
    let mut unit_idx = 0;
    while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
        size /= 1024.0;
        unit_idx += 1;
    }
    if unit_idx == 0 {
        format!("{} {}", bytes, UNITS[unit_idx])
    } else {
        format!("{:.1} {}", size, UNITS[unit_idx])
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use slint::Model;
    use gui_engine::ffprobe::{AudioStreamInfo, VideoAudioProbe};
    use gui_engine::job::{JobStatus, ProgressSnapshot, UnitSnapshot};
    use gui_engine::{
        DeviceNameSource, FrameTimecode, LtcDecodeStatus, LtcDetectionResult, OffloadDeviceTotals,
        OffloadFileInfo, SdCardInfo, Timecode,
    };

    fn result_with(status: LtcDecodeStatus) -> LtcDetectionResult {
        let mut r = LtcDetectionResult::error("unused");
        r.status = status;
        r.detected_fps = 25.0;
        r.drop_frame = false;
        r.total_possible_frames = 100;
        r.valid_frames = 90;
        r.avg_confidence = 0.9;
        r.processing_time_ms = 12.0;
        r.timecodes.push(FrameTimecode {
            frame_index: 0,
            timecode: Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 },
            timecode_secs: 0.0,
        });
        r.timecodes.push(FrameTimecode {
            frame_index: 89,
            timecode: Timecode { hours: 1, minutes: 2, seconds: 6, frames: 19 },
            timecode_secs: 3.5,
        });
        r
    }

    #[test]
    fn channel_index_maps_all_routes() {
        assert_eq!(channel_index(ChannelSel::Left), 0);
        assert_eq!(channel_index(ChannelSel::Right), 1);
        assert_eq!(channel_index(ChannelSel::Both), 2);
    }

    #[test]
    fn advance_pulse_increments_and_wraps() {
        let step = 4.0 * 2.0 * PI * (POLL_INTERVAL_MS as f64 / 1000.0);
        assert!((advance_pulse(0.0) - step).abs() < 1e-9);
        assert_eq!(advance_pulse(PI * 100.0 + 1.0), 0.0);
    }

    #[test]
    fn clap_started_fires_only_on_seq_change() {
        assert!(!clap_started(0, 0), "same seq must not retrigger");
        assert!(clap_started(1, 0), "bumped seq triggers once");
        assert!(!clap_started(1, 1), "already-animated seq must not retrigger");
        assert!(clap_started(3, 2), "missed claps still trigger (animates once)");
    }

    #[test]
    fn device_display_name_marks_default() {
        assert_eq!(device_display_name("sink", false), "sink");
        assert_eq!(device_display_name("sink", true), "sink (Default)");
    }

    #[test]
    fn format_bytes_uses_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
    }

    #[test]
    fn decode_status_detecting_wins_while_jobs_run() {
        let mut s = AppStateSnapshot::initial();
        s.jobs.insert(
            JobKind::LtcDecode,
            JobStatus::running("decoding"),
        );
        s.decode.error = Some("stale".to_string());
        let (status, result_text, error) = format_decode_status(&s);
        assert_eq!(status, "detecting");
        assert!(result_text.is_empty());
        assert!(error.is_empty());
    }

    #[test]
    fn decode_status_reports_error_over_result() {
        let mut s = AppStateSnapshot::initial();
        s.decode.result = Some(result_with(LtcDecodeStatus::Success));
        s.decode.error = Some("boom".to_string());
        let (status, result_text, error) = format_decode_status(&s);
        assert_eq!(status, "error");
        assert!(result_text.is_empty());
        assert_eq!(error, "boom");
    }

    #[test]
    fn decode_status_success_summarises_fps_range_and_frames() {
        let mut s = AppStateSnapshot::initial();
        s.decode.result = Some(result_with(LtcDecodeStatus::Success));
        let (status, result_text, error) = format_decode_status(&s);
        assert_eq!(status, "success");
        assert!(error.is_empty());
        // structural facts: fps, frame count, and the HH:MM:SS:FF range
        assert!(result_text.contains("25.00 fps"));
        assert!(result_text.contains("Frames: 90/100"));
        assert!(result_text.contains("01:02:03:04 → 01:02:06:19"));
    }

    #[test]
    fn decode_status_drop_frame_range_uses_semicolons() {
        let mut s = AppStateSnapshot::initial();
        let mut r = result_with(LtcDecodeStatus::Success);
        r.drop_frame = true;
        s.decode.result = Some(r);
        let (_, result_text, _) = format_decode_status(&s);
        assert!(result_text.contains("01;02;03;04 → 01;02;06;19"));
    }

    #[test]
    fn decode_status_idle_is_all_empty() {
        let s = AppStateSnapshot::initial();
        let (status, result_text, error) = format_decode_status(&s);
        assert!(status.is_empty() && result_text.is_empty() && error.is_empty());
    }

    fn probe(streams: &[(usize, usize)]) -> VideoAudioProbe {
        VideoAudioProbe {
            streams: streams.iter().map(|&(i, ch)| AudioStreamInfo {
                stream_index: i,
                channels: ch,
                codec_name: "pcm".into(),
                sample_rate: 48_000,
            }).collect(),
            total_audio_channels: streams.iter().map(|&(_, c)| c).sum(),
            is_video_file: true,
        }
    }

    #[test]
    fn channel_labels_multistream_numbering_and_selection_row() {
        let p = probe(&[(0, 2), (1, 2)]);
        let (labels, row) = build_channel_labels(&p, 1, 1);
        assert_eq!(labels, vec!["S0 C1", "S0 C2", "S1 C1", "S1 C2"]);
        assert_eq!(row, 3);
    }

    #[test]
    fn channel_labels_stereo_single_stream_uses_lr() {
        let p = probe(&[(0, 2)]);
        let (labels, row) = build_channel_labels(&p, 0, 1);
        assert_eq!(labels, vec!["T1 L", "T1 R"]);
        assert_eq!(row, 1);
    }

    #[test]
    fn channel_labels_unselected_is_minus_one() {
        let p = probe(&[(0, 2)]);
        let (_, row) = build_channel_labels(&p, 5, 5);
        assert_eq!(row, -1);
    }

    #[test]
    fn conversion_status_text_covers_all_phases() {
        let mut js = JobStatus::idle();
        assert_eq!(conversion_status_text(&js), "idle");
        js.progress = ProgressSnapshot {
            phase: JobPhase::Running,
            fraction: 0.25,
            message: String::new(),
            speed: None,
            units: Vec::new(),
            log: String::new(),
        };
        assert_eq!(conversion_status_text(&js), "running 25%");
        js.progress.phase = JobPhase::Succeeded;
        assert_eq!(conversion_status_text(&js), "completed");
        js.progress.phase = JobPhase::Cancelled;
        assert_eq!(conversion_status_text(&js), "cancelled");
        js.progress.phase = JobPhase::Failed;
        assert_eq!(conversion_status_text(&js), "failed");
    }

    #[test]
    fn group_model_lists_files_and_marks_audio_recordings() {
        let mut s = AppStateSnapshot::initial();
        s.converter.groups.push(gui_engine::file_pattern::MatchedGroup {
            prefix: "take1".into(),
            rel_dir: String::new(),
            files: vec![std::path::PathBuf::from("take1S1.wav")],
            recording_type: RecordingType::MultiTrackAudio,
        });
        s.converter.groups.push(gui_engine::file_pattern::MatchedGroup {
            prefix: "MVI_0001".into(),
            rel_dir: String::new(),
            files: vec![std::path::PathBuf::from("MVI_0001.MP4")],
            recording_type: RecordingType::VideoClipSequence,
        });
        let model = build_group_model(&s);
        assert_eq!(model.len(), 2);
        assert!(model[0].is_audio_recording);
        assert_eq!(model[0].channel_count, 1);
        assert_eq!(model[0].prefix.as_str(), "take1");
        assert!(!model[1].is_audio_recording);
    }

    fn card() -> SdCardInfo {
        SdCardInfo {
            mount: std::path::PathBuf::from("/media/card"),
            volume_label: "EOS".into(),
            device_name: "cam-a".into(),
            name_source: DeviceNameSource::VolumeLabel,
            media_file_count: 1,
            total_bytes: 2048,
            files: vec![OffloadFileInfo {
                path: std::path::PathBuf::from("/media/card/a.wav"),
                name: "a.wav".into(),
                size_bytes: 2048,
                modified: None,
            }],
            selected: vec![true],
            selected_count: 1,
            selected_bytes: 2048,
        }
    }

    #[test]
    fn offload_card_infos_carry_selection_and_sizes() {
        let mut off = gui_engine::offload::OffloadSnapshot::initial();
        off.cards.push(card());
        let infos = build_offload_card_infos(&off);
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].selected_count, 1);
        assert_eq!(infos[0].total_bytes.as_str(), "2.0 KiB");
        assert_eq!(infos[0].files.row_count(), 1);
        assert!(infos[0].files.row_data(0).unwrap().selected);
    }

    #[test]
    fn offload_file_duration_text_distinguishes_pending_from_failed() {
        let mut off = gui_engine::offload::OffloadSnapshot::initial();
        let c = card();
        off.cards.push(c.clone());
        off.file_durations.insert(c.files[0].path.clone(), None); // probed, failed
        let infos = build_offload_card_infos(&off);
        // failed probe renders the em dash; unprobed (absent key) renders ellipsis
        assert_eq!(infos[0].files.row_data(0).unwrap().duration_text.as_str(), "—");
        let mut off2 = gui_engine::offload::OffloadSnapshot::initial();
        off2.cards.push(card());
        let infos2 = build_offload_card_infos(&off2);
        assert_eq!(infos2[0].files.row_data(0).unwrap().duration_text.as_str(), "…");
    }

    #[test]
    fn offload_device_status_blends_totals_with_units() {
        let mut off = gui_engine::offload::OffloadSnapshot::initial();
        off.device_totals.push(OffloadDeviceTotals {
            name: "cam-a".into(),
            files_total: 10,
            bytes_total: 1000,
        });
        let mut js = JobStatus::idle();
        let (name, state, bytes, progress) = {
            let rows = build_offload_device_status(&off, &js);
            let r = &rows[0];
            (r.device_name.clone(), r.state_text.clone(), r.bytes_text.clone(), r.progress)
        };
        assert_eq!(name.as_str(), "cam-a");
        assert_eq!(state, "Pending");
        assert_eq!(bytes.as_str(), "0 B / 1000 B");
        assert_eq!(progress, 0.0);

        js.progress = ProgressSnapshot {
            phase: JobPhase::Running,
            fraction: 0.5,
            message: String::new(),
            speed: None,
            units: vec![UnitSnapshot {
                label: "cam-a".into(),
                message: "f3".into(),
                fraction: 0.5,
                state: UnitState::Running,
            }],
            log: String::new(),
        };
        let rows = build_offload_device_status(&off, &js);
        assert_eq!(rows[0].state_text, "Copying");
        assert_eq!(rows[0].files_done, 5);
        assert_eq!(rows[0].bytes_text.as_str(), "500 B / 1000 B");
    }

    #[test]
    fn offload_device_status_failed_unit_surfaces_error() {
        let mut off = gui_engine::offload::OffloadSnapshot::initial();
        off.device_totals.push(OffloadDeviceTotals {
            name: "cam-b".into(),
            files_total: 1,
            bytes_total: 0,
        });
        let js = JobStatus {
            progress: ProgressSnapshot {
                phase: JobPhase::Failed,
                fraction: 0.0,
                message: String::new(),
                speed: None,
                units: vec![UnitSnapshot {
                    label: "cam-b".into(),
                    message: "disk full".into(),
                    fraction: 0.0,
                    state: UnitState::Failed,
                }],
                log: String::new(),
            },
            error: None,
        };
        let rows = build_offload_device_status(&off, &js);
        assert_eq!(rows[0].state_text, "Failed");
        assert_eq!(rows[0].error.as_str(), "disk full");
    }

    #[test]
    fn audio_event_notifications_map_severity() {
        let (msg, sev) = audio_event_to_notification(&AudioEvent::Underrun);
        assert_eq!(sev, "warning");
        assert!(!msg.is_empty());
        let (msg, sev) = audio_event_to_notification(&AudioEvent::StreamError("x".into()));
        assert_eq!(sev, "error");
        assert_eq!(msg, "x");
    }
}
