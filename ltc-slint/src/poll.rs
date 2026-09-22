use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::converter::{ConversionState, ConversionStatus, FfmpegCapabilities, OutputNamingMode, select_best_combination};
use gui_engine::log_buffer::LogBuffer;
use gui_engine::state::AppStateSnapshot;
use gui_engine::timecode;
use gui_engine::{ArcSwap, AudioEvent, SAMPLE_RATE_OPTIONS};
use log::info;
use slint::{ModelRc, SharedString, VecModel};

use crate::toast::{push_toast, ToastItem};
use crate::timecode_helpers::set_tc_segments;
use crate::update_converter_options;
use crate::{AppColors, AppWindow, LogEntry};
use slint::ComponentHandle;
use slint::Global;

const POLL_INTERVAL_MS: u64 = 40;

/// Returns `true` if the auto-settings (trim/split/drop) should be applied
/// for this decode result, given the current and last-applied generation.
fn should_auto_apply_ltc_settings(
    decode_generation: u64,
    last_applied_gen: u64,
    status: &gui_engine::LtcDecodeStatus,
) -> bool {
    matches!(status, gui_engine::LtcDecodeStatus::Success | gui_engine::LtcDecodeStatus::LowConfidence)
        && decode_generation != last_applied_gen
}

#[allow(clippy::too_many_arguments)]
pub fn setup_poll_timer(
    ui: &AppWindow,
    engine_state: Arc<ArcSwap<AppStateSnapshot>>,
    toasts: Arc<Mutex<Vec<ToastItem>>>,
    next_toast_id: Arc<Mutex<i32>>,
    log_buffer: Arc<Mutex<LogBuffer>>,
    last_debug_log_count: Arc<Mutex<usize>>,
    pulse_phase: Arc<Mutex<f64>>,
    conv_state: Arc<Mutex<ConversionState>>,
    conv_ffmpeg_caps: Arc<Mutex<Option<FfmpegCapabilities>>>,
    conv_ffmpeg_probing: Arc<Mutex<bool>>,
    conv_sanity_msg: Arc<Mutex<String>>,
    conv_filename_prefix: Arc<Mutex<String>>,
    conv_naming_mode: Arc<Mutex<OutputNamingMode>>,
    conv_file_groups: Arc<Mutex<BTreeMap<String, Vec<PathBuf>>>>,
    conv_selected_group_idx: Arc<Mutex<isize>>,
    conv_container: Arc<Mutex<String>>,
    conv_video_encoder: Arc<Mutex<String>>,
    conv_audio_encoder: Arc<Mutex<String>>,
    conv_copy_video: Arc<Mutex<bool>>,
    conv_selected_folder: Arc<Mutex<String>>,
    conv_trim_offset_secs: Arc<Mutex<f64>>,
    conv_trim_to_first_ltc: Arc<Mutex<bool>>,
    conv_split_tracks: Arc<Mutex<bool>>,
    conv_drop_ltc_track: Arc<Mutex<bool>>,
) {
    let ui_weak = ui.as_weak();
    let last_log_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let last_device_key: Arc<Mutex<(usize, String)>> = Arc::new(Mutex::new((0, String::new())));
    let last_auto_applied_gen: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

    // Latch: track when ffmpeg caps have been mirrored into conv_ffmpeg_caps
    let last_caps_loaded: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));

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

            // 2. System time
            ui.set_system_time(SharedString::from(format!("{} UTC", timecode::chrono_now_string())));

            // 3. Theme sync
            AppColors::get(&ui).set_theme_dark(s.is_dark_theme);

            // 3.5 LTC decode state sync (display unconditional, auto-set once per generation)
            {
                let mut last_auto = last_auto_applied_gen.lock().unwrap();

                if s.ltc_is_detecting {
                    ui.set_ltc_status(SharedString::from("detecting"));
                    ui.set_ltc_result_text(SharedString::from(""));
                    ui.set_ltc_error(SharedString::from(""));
                } else if let Some(ref err) = s.ltc_decode_error {
                    ui.set_ltc_status(SharedString::from("error"));
                    ui.set_ltc_result_text(SharedString::from(""));
                    ui.set_ltc_error(SharedString::from(err));
                } else if let Some(ref r) = s.ltc_decode_result {
                    let status_str = match &r.status {
                        gui_engine::LtcDecodeStatus::Success => "success",
                        gui_engine::LtcDecodeStatus::LowConfidence => "low_confidence",
                        gui_engine::LtcDecodeStatus::NoSyncWord => "no_sync",
                        gui_engine::LtcDecodeStatus::Error { .. } => "error",
                    };

                    // Auto-set: once per decode generation (user unticks survive)
                    if should_auto_apply_ltc_settings(s.ltc_decode_generation, *last_auto, &r.status) {
                        *last_auto = s.ltc_decode_generation;
                        let offset = r.first_ltc_timecode_secs;
                        *conv_trim_offset_secs.lock().unwrap() = offset;
                        ui.set_trim_offset_secs(offset as f32);
                        *conv_trim_to_first_ltc.lock().unwrap() = true;
                        ui.set_trim_to_first_ltc(true);
                        *conv_split_tracks.lock().unwrap() = true;
                        ui.set_conv_split_tracks(true);
                        *conv_drop_ltc_track.lock().unwrap() = true;
                        ui.set_conv_drop_ltc_track(true);
                    }

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
                    ui.set_ltc_status(SharedString::from(status_str));
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
                        format!(" | Quality: {:.0}% ({}) {}", q.score * 100.0, q.grade, issues)
                    }).unwrap_or_default();
                    ui.set_ltc_result_text(SharedString::from(format!(
                        "{} | Conf: {:.1}% | Frames: {}/{} | {} | {:.1}ms{}",
                        fps_str,
                        r.avg_confidence * 100.0,
                        r.valid_frames,
                        r.total_possible_frames,
                        tc_range,
                        r.processing_time_ms,
                        quality_str,
                    )));
                    ui.set_ltc_error(SharedString::from(""));
                }
            }

            // Sync video audio probe info
            if let Some(ref probe) = s.ltc_probe {
                let channel_names: Vec<SharedString> = probe
                    .streams
                    .iter()
                    .flat_map(|s_info| {
                        (0..s_info.channels).map(move |ch| {
                            let label = if probe.streams.len() > 1 {
                                format!("Stream {} Ch {}", s_info.stream_index + 1, ch + 1)
                            } else {
                                format!("Ch {}", ch + 1)
                            };
                            SharedString::from(label)
                        })
                    })
                    .collect();
                ui.set_ltc_channel_names(ModelRc::new(VecModel::from(channel_names)));
            }

            // Sync decode progress every tick during active detection
            if s.ltc_is_detecting {
                ui.set_ltc_decode_progress(s.ltc_decode_progress_pct);
            }

            // 4. Pulse phase animation
            let mut pp = pulse_phase.lock().unwrap();
            *pp += 4.0 * 2.0 * PI * (POLL_INTERVAL_MS as f64 / 1000.0);
            if *pp > PI * 100.0 { *pp = 0.0; }
            ui.set_pulse_phase(*pp as f32);

            // 5. Timecode display
            let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame);
            let ms_str = timecode::timecode_to_ms_string(s.current_timecode, s.fps);
            set_tc_segments(&ui, &tc_str);
            ui.set_ms_text(SharedString::from(ms_str));

            // 6. Transport state
            ui.set_is_playing(s.is_playing);
            ui.set_is_locked(s.is_locked);
            ui.set_wake_lock_active(s.wake_lock_active);
            ui.set_status_message(SharedString::from(&s.status_message));

            // 7. FPS name
            let fps_name = SharedString::from(gui_engine::timecode::FPS_OPTIONS[s.fps_index].name);
            ui.set_fps_name(fps_name);

            // 8. Routing pills
            ui.set_ltc_route(SharedString::from(s.ltc_channel.to_uppercase()));
            ui.set_beep_route(SharedString::from(s.beep_channel.to_uppercase()));

            // 9. Sample rate metadata
            let rate_khz = format!("{:.1}", s.sample_rate as f32 / 1000.0);
            ui.set_sample_rate_khz(SharedString::from(rate_khz));
            let buffer_smp = (s.sample_rate as f64 / s.fps).round() as i32;
            ui.set_buffer_size(buffer_smp);
            ui.set_sample_format(SharedString::from(s.sample_format_name.to_uppercase()));

            // 10. Clapper metadata
            ui.set_scene(s.scene as i32);
            ui.set_take(s.take as i32);
            ui.set_auto_increment(s.auto_increment_take);

            // 11. Arm angle and flash opacity
            ui.set_arm_angle(s.clap_arm_angle);
            ui.set_flash_opacity(s.clap_flash_alpha);

            // 12. Device selection sync
            ui.set_device_index(s.selected_device as i32);

            // 13. Volume / pitch / duration
            ui.set_ltc_volume(s.ltc_volume);
            ui.set_beep_volume(s.beep_volume);
            ui.set_beep_frequency(s.beep_frequency);
            ui.set_beep_duration(s.beep_duration);

            // 14. Start timecode steppers
            ui.set_hour(s.start_timecode.hours as i32);
            ui.set_minute(s.start_timecode.minutes as i32);
            ui.set_second(s.start_timecode.seconds as i32);
            let max_frame = s.fps.round() as i32;
            ui.set_max_frame(if max_frame > 0 { max_frame - 1 } else { 0 });
            ui.set_frame(s.start_timecode.frames as i32);

            // 15. FPS and sample rate index
            ui.set_fps_index(s.fps_index as i32);
            let sr_index = SAMPLE_RATE_OPTIONS.iter()
                .position(|&r| r == s.sample_rate)
                .unwrap_or(0);
            ui.set_sample_rate_index(sr_index as i32);

            // 16. Log entries (only rebuild if log count changed)
            {
                let current_log_count = s.logs.len();
                let mut last = last_log_count.lock().unwrap();
                if current_log_count != *last {
                    let log_entries: Vec<LogEntry> = s.logs
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

            // 17. Device names (only rebuild if device list changed)
            {
                let device_key = (
                    s.devices.len(),
                    s.devices.iter().map(|d| d.id.clone()).collect::<Vec<_>>().join("\n"),
                );
                let mut last = last_device_key.lock().unwrap();
                if device_key != *last {
                    let device_names: Vec<SharedString> = s.devices
                        .iter()
                        .map(|d| {
                            if d.is_default {
                                SharedString::from(format!("{} (Default)", d.name))
                            } else {
                                SharedString::from(d.name.clone())
                            }
                        })
                        .collect();
                    ui.set_device_names(ModelRc::new(VecModel::<SharedString>::from(device_names)));
                    ui.set_device_count(s.devices.len() as i32);
                    *last = device_key;
                }
            }

            // 18. Process events into toasts
            for event in &s.events {
                let (msg, typ) = match event {
                    AudioEvent::StreamError(m) => (m.clone(), "error"),
                    AudioEvent::StreamDied => ("Audio stream died".to_string(), "error"),
                    AudioEvent::StreamRecovering { attempt } => {
                        (format!("Stream recovering (attempt {})", attempt), "warning")
                    }
                    AudioEvent::StreamDead => {
                        ("Audio device unreachable".to_string(), "error")
                    }
                    AudioEvent::RecoveryNeeded { reason } => {
                        (format!("Audio recovery needed: {}", reason), "warning")
                    }
                    AudioEvent::Underrun => ("Audio underrun".to_string(), "warning"),
                    AudioEvent::FramesDropped { total } => {
                        (format!("{} frames dropped", total), "warning")
                    }
                };
                push_toast(&toasts, &next_toast_id, &ui, &msg, typ);
            }

            // 19. Debug log sync
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

            // 20. ffmpeg capability probe state sync
            {
                // Mirror probing flag from engine snapshot every tick
                *conv_ffmpeg_probing.lock().unwrap() = s.ffmpeg_probe_running;
                ui.set_conv_ffmpeg_probing(s.ffmpeg_probe_running);

                // Mirror engine-probed caps into the GUI-side Arc<Mutex> when they first arrive.
                // The latch ensures this runs exactly once per session.
                if s.ffmpeg_caps.is_some() && !*last_caps_loaded.lock().unwrap() {
                    *last_caps_loaded.lock().unwrap() = true;
                    let caps = s.ffmpeg_caps.clone().unwrap();
                    *conv_ffmpeg_caps.lock().unwrap() = Some(caps.clone());
                    ui.set_conv_has_ffmpeg(caps.has_ffmpeg);
                    if let Some(ref msg) = caps.error_message {
                        ui.set_conv_ffmpeg_error(SharedString::from(msg));
                    }
                    // Apply intelligent defaults and update dropdown models
                    let current_container = conv_container.lock().unwrap().clone();
                    let (def_c, def_v, def_a) = select_best_combination(&caps);
                    if current_container != def_c {
                        *conv_container.lock().unwrap() = def_c.clone();
                        ui.set_conv_container(SharedString::from(def_c));
                    }
                    if *conv_video_encoder.lock().unwrap() != def_v {
                        *conv_video_encoder.lock().unwrap() = def_v.clone();
                        ui.set_conv_video_encoder(SharedString::from(def_v));
                    }
                    if *conv_audio_encoder.lock().unwrap() != def_a {
                        *conv_audio_encoder.lock().unwrap() = def_a.clone();
                        ui.set_conv_audio_encoder(SharedString::from(def_a));
                    }
                    let container_guard = conv_container.lock().unwrap();
                    update_converter_options(&ui, &caps, &container_guard);
                }
            }

            // 21. Conversion state sync
            // (renumbered from 20/21/22/... — the "read conv_ffmpeg_caps for has-ffmpeg/error"
            //  block was folded into the mirror logic above)
            {
                let cs = conv_state.lock().unwrap();
                let (status_str, progress) = match &cs.status {
                    ConversionStatus::Idle => ("idle".to_string(), 0.0),
                    ConversionStatus::Running { progress } => ("running".to_string(), *progress),
                    ConversionStatus::Completed => ("completed".to_string(), 1.0),
                    ConversionStatus::Failed { .. } => ("failed".to_string(), 0.0),
                };
                ui.set_conv_status(SharedString::from(status_str));
                ui.set_conv_progress(progress);
                ui.set_conv_log(SharedString::from(cs.ffmpeg_output.clone()));
            }
            if tick % 10 == 0 {
                let caps = conv_ffmpeg_caps.lock().unwrap().clone();
                let container = conv_container.lock().unwrap().clone();
                let venc = conv_video_encoder.lock().unwrap().clone();
                let aenc = conv_audio_encoder.lock().unwrap().clone();
                let copy_video = *conv_copy_video.lock().unwrap();
                let folder = conv_selected_folder.lock().unwrap().clone();
                let filename_prefix = conv_filename_prefix.lock().unwrap().clone();
                let naming_mode = conv_naming_mode.lock().unwrap().clone();
                let mut msg = String::new();
                match caps {
                    Some(ref c) if !c.has_ffmpeg => {
                        msg = "ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH.".to_string();
                    }
                    Some(_) if !naming_mode.is_source_stems() && filename_prefix.is_empty() => {
                        msg = "No input group selected.".to_string();
                    }
                    Some(ref c) => {
                        let sel_idx = *conv_selected_group_idx.lock().unwrap();
                        let input_files: Vec<PathBuf> = if sel_idx >= 0 {
                            let groups = conv_file_groups.lock().unwrap();
                            let keys: Vec<String> = groups.keys().cloned().collect();
                            if (sel_idx as usize) < keys.len() {
                                let prefix = &keys[sel_idx as usize];
                                let files = groups.get(prefix).cloned().unwrap_or_default();
                                files.iter().map(|f| PathBuf::from(&folder).join(f)).collect()
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        };
                        let output_folder = Path::new(&folder);
                        if let Err(e) = gui_engine::converter::conversion_sanity_check_with_naming(
                            &container, &venc, &aenc, &input_files, output_folder, &filename_prefix, c,
                            None, None, Some(&naming_mode), copy_video,
                        ) {
                            msg = e;
                        }
                    }
                    None => {}
                }
                *conv_sanity_msg.lock().unwrap() = msg.clone();
                ui.set_conv_sanity_msg(SharedString::from(msg));
            }

            // 21. Perf diagnostics
            let poll_elapsed = poll_start.elapsed();
            if tick % 250 == 0 {
                info!("[PERF] Poll tick #{}: {}ms", tick, poll_elapsed.as_micros() as f64 / 1000.0);
            }
        },
    );

    Box::leak(Box::new(poll_timer));
}

#[cfg(test)]
mod tests {
    use super::should_auto_apply_ltc_settings;

    #[test]
    fn auto_apply_on_success_new_gen() {
        assert!(should_auto_apply_ltc_settings(1, 0, &gui_engine::LtcDecodeStatus::Success));
    }

    #[test]
    fn auto_apply_on_low_confidence_new_gen() {
        assert!(should_auto_apply_ltc_settings(1, 0, &gui_engine::LtcDecodeStatus::LowConfidence));
    }

    #[test]
    fn auto_apply_skips_same_gen() {
        assert!(!should_auto_apply_ltc_settings(1, 1, &gui_engine::LtcDecodeStatus::Success));
    }

    #[test]
    fn auto_apply_skips_nosync() {
        assert!(!should_auto_apply_ltc_settings(1, 0, &gui_engine::LtcDecodeStatus::NoSyncWord));
    }

    #[test]
    fn auto_apply_skips_error() {
        assert!(!should_auto_apply_ltc_settings(1, 0, &gui_engine::LtcDecodeStatus::Error { message: "x".into() }));
    }

    #[test]
    fn auto_apply_noop_when_gen_zero_and_no_decode() {
        // Initial state: gen 0, last_applied 0 → should not apply
        assert!(!should_auto_apply_ltc_settings(0, 0, &gui_engine::LtcDecodeStatus::Success));
    }

    #[test]
    fn auto_apply_reapplies_after_new_generation() {
        // gen=1 → apply; gen=1 → skip; gen=2 → apply again
        assert!(should_auto_apply_ltc_settings(1, 0, &gui_engine::LtcDecodeStatus::Success));
        assert!(!should_auto_apply_ltc_settings(1, 1, &gui_engine::LtcDecodeStatus::Success));
        assert!(should_auto_apply_ltc_settings(2, 1, &gui_engine::LtcDecodeStatus::Success));
    }
}