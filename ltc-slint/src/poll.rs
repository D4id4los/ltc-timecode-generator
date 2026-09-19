use std::collections::BTreeMap;
use std::f64::consts::PI;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::converter::{ConversionState, ConversionStatus, FfmpegCapabilities};
use gui_engine::log_buffer::LogBuffer;
use gui_engine::state::AppStateSnapshot;
use gui_engine::timecode;
use gui_engine::{ArcSwap, AudioEvent, SAMPLE_RATE_OPTIONS};
use log::info;
use slint::{ModelRc, SharedString, VecModel};

use crate::toast::{push_toast, ToastItem};
use crate::timecode_helpers::set_tc_segments;
use crate::{AppColors, AppWindow, LogEntry};
use slint::ComponentHandle;
use slint::Global;

const POLL_INTERVAL_MS: u64 = 40;

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
    conv_sanity_msg: Arc<Mutex<String>>,
    conv_output_path: Arc<Mutex<String>>,
    conv_file_groups: Arc<Mutex<BTreeMap<String, Vec<PathBuf>>>>,
    conv_selected_group_idx: Arc<Mutex<isize>>,
    conv_container: Arc<Mutex<String>>,
    conv_video_encoder: Arc<Mutex<String>>,
    conv_audio_encoder: Arc<Mutex<String>>,
    conv_selected_folder: Arc<Mutex<String>>,
    conv_trim_offset_secs: Arc<Mutex<f64>>,
    conv_trim_to_first_ltc: Arc<Mutex<bool>>,
) {
    let ui_weak = ui.as_weak();
    let last_log_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let last_device_key: Arc<Mutex<(usize, String)>> = Arc::new(Mutex::new((0, String::new())));
    let last_ltc_decode_gen: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

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

            // 3.5 LTC decode state sync
            {
                let mut last = last_ltc_decode_gen.lock().unwrap();
                if s.ltc_decode_generation != *last {
                    *last = s.ltc_decode_generation;
                    if s.ltc_is_detecting {
                        ui.set_ltc_status(SharedString::from("detecting"));
                        ui.set_ltc_result_text(SharedString::from(""));
                        ui.set_ltc_error(SharedString::from(""));
                    } else if let Some(ref err) = s.ltc_decode_error {
                        ui.set_ltc_status(SharedString::from("error"));
                        ui.set_ltc_result_text(SharedString::from(""));
                        ui.set_ltc_error(SharedString::from(err));
                    } else if let Some(ref r) = s.ltc_decode_result {
                        let is_detected = matches!(r.status, gui_engine::LtcDecodeStatus::Success | gui_engine::LtcDecodeStatus::LowConfidence);
                        let status_str = match &r.status {
                            gui_engine::LtcDecodeStatus::Success => "success",
                            gui_engine::LtcDecodeStatus::LowConfidence => "low_confidence",
                            gui_engine::LtcDecodeStatus::NoSyncWord => "no_sync",
                            gui_engine::LtcDecodeStatus::Error { .. } => "error",
                        };
                        if is_detected {
                            let offset = r.first_ltc_timecode_secs;
                            *conv_trim_offset_secs.lock().unwrap() = offset;
                            ui.set_trim_offset_secs(offset as f32);
                            *conv_trim_to_first_ltc.lock().unwrap() = true;
                            ui.set_trim_to_first_ltc(true);
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
                        ui.set_ltc_result_text(SharedString::from(format!(
                            "{} | Conf: {:.1}% | Frames: {}/{} | {} | {:.1}ms",
                            fps_str,
                            r.avg_confidence * 100.0,
                            r.valid_frames,
                            r.total_possible_frames,
                            tc_range,
                            r.processing_time_ms,
                        )));
                        ui.set_ltc_error(SharedString::from(""));
                    }
                }
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

            // 20. Converter state sync
            {
                let caps = conv_ffmpeg_caps.lock().unwrap();
                if let Some(ref c) = *caps {
                    ui.set_conv_has_ffmpeg(c.has_ffmpeg);
                    if let Some(ref msg) = c.error_message {
                        ui.set_conv_ffmpeg_error(SharedString::from(msg));
                    }
                }
            }
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
                let out = conv_output_path.lock().unwrap().clone();
                let caps = conv_ffmpeg_caps.lock().unwrap().clone();
                let container = conv_container.lock().unwrap().clone();
                let venc = conv_video_encoder.lock().unwrap().clone();
                let aenc = conv_audio_encoder.lock().unwrap().clone();
                let folder = conv_selected_folder.lock().unwrap().clone();
                let mut msg = String::new();
                match caps {
                    Some(ref c) if !c.has_ffmpeg => {
                        msg = "ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH.".to_string();
                    }
                    Some(_) if out.is_empty() => {
                        msg = "No output file path specified.".to_string();
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
                        let output_path = PathBuf::from(&out);
                        if let Err(e) = gui_engine::converter::conversion_sanity_check(
                            &container, &venc, &aenc, &input_files, &output_path, c,
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