use std::f64::consts::PI;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::converter::{format_blockers, ConversionStatus, RecordingType};
use gui_engine::duration::{format_duration_secs, group_duration_secs};
use gui_engine::log_buffer::LogBuffer;
use gui_engine::state::{AppStateSnapshot, ConverterUserSettings};
use gui_engine::timecode;
use gui_engine::{ArcSwap, AudioEvent, SAMPLE_RATE_OPTIONS};
use log::info;
use slint::{ModelRc, SharedString, VecModel};

use crate::toast::{push_toast, ToastItem};
use crate::timecode_helpers::set_tc_segments;
use crate::{AppColors, AppWindow, FileGroupInfo, LogEntry};
use slint::ComponentHandle;
use slint::Global;

const POLL_INTERVAL_MS: u64 = 40;

pub fn setup_poll_timer(
    ui: &AppWindow,
    engine_state: Arc<ArcSwap<AppStateSnapshot>>,
    toasts: Arc<Mutex<Vec<ToastItem>>>,
    next_toast_id: Arc<Mutex<i32>>,
    log_buffer: Arc<Mutex<LogBuffer>>,
    last_debug_log_count: Arc<Mutex<usize>>,
    pulse_phase: Arc<Mutex<f64>>,
    conv_settings_cache: Arc<Mutex<ConverterUserSettings>>,
) {
    let ui_weak = ui.as_weak();
    let last_log_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let last_device_key: Arc<Mutex<(usize, String)>> = Arc::new(Mutex::new((0, String::new())));

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

            // 1. System time
            ui.set_system_time(SharedString::from(format!("{} UTC", timecode::chrono_now_string())));

            // 2. Theme sync
            AppColors::get(&ui).set_theme_dark(s.is_dark_theme);

            // 3. Clapper metadata (fix one-way sync gaps)
            ui.set_roll(SharedString::from(s.roll.clone()));
            ui.set_scene(s.scene as i32);
            ui.set_take(s.take as i32);
            ui.set_auto_increment(s.auto_increment_take);

            // 4. LTC decode state sync
            {
                if s.ltc_is_detecting || s.ltc_group_is_detecting {
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
                        format!(" | Quality: {:.0}% ({}) {:.1}% usable, {}",
                            q.score * 100.0, q.grade, q.usable_coverage * 100.0, issues)
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
                } else {
                    ui.set_ltc_status(SharedString::from(""));
                    ui.set_ltc_result_text(SharedString::from(""));
                    ui.set_ltc_error(SharedString::from(""));
                }
            }

            // 5. LTC decode FPS index sync (fix one-way gap)
            ui.set_decode_fps_index(s.decode_fps_index as i32);

            // 6. Video audio probe info
            if let Some(ref probe) = s.ltc_probe {
                let mut channel_names_vec: Vec<SharedString> = Vec::new();
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
                        channel_names_vec.push(SharedString::from(label));
                        if s_info.stream_index == s.ltc_selected_stream && ch == s.ltc_selected_channel {
                            ltc_row_idx = idx as i32;
                        }
                    }
                }
                ui.set_ltc_channel_names(ModelRc::new(VecModel::from(channel_names_vec)));
                ui.set_conv_ltc_row_index(ltc_row_idx);
            } else {
                ui.set_ltc_channel_names(ModelRc::new(VecModel::from(Vec::<SharedString>::new())));
                ui.set_conv_ltc_row_index(-1);
            }

            // 7. Probe-status flags for video recordings
            if ui.get_conv_is_video_recording() {
                ui.set_conv_ltc_probe_loading(s.converter.probes_loading);
                ui.set_conv_ltc_probe_failed(s.ltc_probe.is_none() && !s.converter.probes_loading);
            } else {
                ui.set_conv_ltc_probe_loading(false);
                ui.set_conv_ltc_probe_failed(false);
            }

            // 8. Sync decode progress
            if s.ltc_is_detecting || s.ltc_group_is_detecting {
                ui.set_ltc_decode_progress(s.ltc_decode_progress_pct);
            }

            // 9. Pulse phase animation
            let mut pp = pulse_phase.lock().unwrap();
            *pp += 4.0 * 2.0 * PI * (POLL_INTERVAL_MS as f64 / 1000.0);
            if *pp > PI * 100.0 { *pp = 0.0; }
            ui.set_pulse_phase(*pp as f32);

            // 10. Timecode display
            let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame);
            let ms_str = timecode::timecode_to_ms_string(s.current_timecode, s.fps);
            set_tc_segments(&ui, &tc_str);
            ui.set_ms_text(SharedString::from(ms_str));

            // 11. Transport state
            ui.set_is_playing(s.is_playing);
            ui.set_is_locked(s.is_locked);
            ui.set_wake_lock_active(s.wake_lock_active);
            ui.set_status_message(SharedString::from(&s.status_message));

            // 12. FPS name
            let fps_name = SharedString::from(gui_engine::timecode::FPS_OPTIONS[s.fps_index].name);
            ui.set_fps_name(fps_name);

            // 13. Routing pills
            ui.set_ltc_route(SharedString::from(s.ltc_channel.to_uppercase()));
            ui.set_beep_route(SharedString::from(s.beep_channel.to_uppercase()));

            // 14. LTC/beep channel indices (fix one-way gaps)
            {
                let ltc_idx = match s.ltc_channel.as_str() {
                    "left" => 0,
                    "right" => 1,
                    _ => 2,
                };
                ui.set_ltc_channel_index(ltc_idx);
                let beep_idx = match s.beep_channel.as_str() {
                    "left" => 0,
                    "right" => 1,
                    _ => 2,
                };
                ui.set_beep_channel_index(beep_idx);
            }

            // 15. Sample rate metadata
            let rate_khz = format!("{:.1}", s.sample_rate as f32 / 1000.0);
            ui.set_sample_rate_khz(SharedString::from(rate_khz));
            let buffer_smp = (s.sample_rate as f64 / s.fps).round() as i32;
            ui.set_buffer_size(buffer_smp);
            ui.set_sample_format(SharedString::from(s.sample_format_name.to_uppercase()));

            // 16. Arm angle and flash opacity
            ui.set_arm_angle(s.clap_arm_angle);
            ui.set_flash_opacity(s.clap_flash_alpha);

            // 17. Device selection sync
            ui.set_device_index(s.selected_device as i32);

            // 18. Volume / pitch / duration
            ui.set_ltc_volume(s.ltc_volume);
            ui.set_beep_volume(s.beep_volume);
            ui.set_beep_frequency(s.beep_frequency);
            ui.set_beep_duration(s.beep_duration);

            // 19. Start timecode steppers
            ui.set_hour(s.start_timecode.hours as i32);
            ui.set_minute(s.start_timecode.minutes as i32);
            ui.set_second(s.start_timecode.seconds as i32);
            let max_frame = s.fps.round() as i32;
            ui.set_max_frame(if max_frame > 0 { max_frame - 1 } else { 0 });
            ui.set_frame(s.start_timecode.frames as i32);

            // 20. FPS and sample rate index
            ui.set_fps_index(s.fps_index as i32);
            let sr_index = SAMPLE_RATE_OPTIONS.iter()
                .position(|&r| r == s.sample_rate)
                .unwrap_or(0);
            ui.set_sample_rate_index(sr_index as i32);

            // 21. Converter user settings sync — copy all fields from engine to cache + Slint
            {
                let mut cache = conv_settings_cache.lock().unwrap();
                *cache = s.converter.settings.clone();

                ui.set_conv_container(SharedString::from(s.converter.settings.container.clone()));
                ui.set_conv_video_encoder(SharedString::from(s.converter.settings.video_encoder.clone()));
                ui.set_conv_audio_encoder(SharedString::from(s.converter.settings.audio_encoder.clone()));
                ui.set_conv_split_tracks(s.converter.settings.split_tracks);
                ui.set_conv_drop_ltc_track(s.converter.settings.drop_ltc_track);
                ui.set_conv_concat_audio(s.converter.settings.concat_audio);
                ui.set_conv_generate_synthetic_video(s.converter.settings.generate_synthetic_video);
                ui.set_conv_copy_video(s.converter.settings.copy_video);
                ui.set_conv_metadata_only(s.converter.settings.metadata_only);
                ui.set_set_start_from_ltc(s.converter.settings.set_start_from_ltc);
                ui.set_conv_filename_prefix(SharedString::from(s.converter.settings.filename_prefix.clone()));
                ui.set_conv_audio_suffix_template(SharedString::from(
                    s.converter.settings.audio_suffix_template.clone(),
                ));
                ui.set_conv_video_suffix_template(SharedString::from(
                    s.converter.settings.video_suffix_template.clone(),
                ));
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

            // 22. ffmpeg capability probe state sync
            {
                ui.set_conv_ffmpeg_probing(s.ffmpeg_probe_running);
                if let Some(ref caps) = s.ffmpeg_caps {
                    ui.set_conv_has_ffmpeg(caps.has_ffmpeg);
                    if let Some(ref msg) = caps.error_message {
                        ui.set_conv_ffmpeg_error(SharedString::from(msg));
                    }
                }
            }

            // 23. Converter dropdown models
            {
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

            // 24. Converter group + file info sync
            {
                let groups = &s.converter.groups;
                let selected_idx = s.converter.selected_group_idx;
                ui.set_conv_selected_group_idx(selected_idx.map(|i| i as i32).unwrap_or(-1));

                let group_model: Vec<FileGroupInfo> = groups.iter().map(|g| {
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
                        duration_text: SharedString::from(dur_text),
                    }
                }).collect();
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
                        ui.set_ltc_file_idx(s.converter.settings.ltc_file_idx as i32);
                        ui.set_ltc_file_names(ModelRc::new(VecModel::<SharedString>::from(ltc_file_names)));
                        ui.set_conv_filename_prefix(SharedString::from(s.converter.settings.filename_prefix.clone()));
                    }
                }

                if let Some(ref folder) = s.converter.groups_folder {
                    ui.set_conv_selected_folder(SharedString::from(folder.to_string_lossy().as_ref()));
                }
            }

            // 25. Conversion state sync (from engine-owned snapshot)
            {
                let cs = &s.converter.conversion_state;
                let status_str = match &cs.status {
                    ConversionStatus::Idle => "idle".to_string(),
                    ConversionStatus::Running { progress } => {
                        format!("running {:.0}%", progress * 100.0)
                    }
                    ConversionStatus::Completed => "completed".to_string(),
                    ConversionStatus::Failed { .. } => "failed".to_string(),
                };
                let progress = match &cs.status {
                    ConversionStatus::Running { progress } => *progress,
                    _ => 0.0,
                };
                ui.set_conv_status(SharedString::from(status_str));
                ui.set_conv_progress(progress);
                ui.set_conv_log(SharedString::from(cs.ffmpeg_output.clone()));
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

            // 27. Log entries (only rebuild if log count changed)
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

            // 29. Process events into toasts
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

            // 31. Offload state sync
            {
                let off = &s.offload;
                ui.set_off_parent_folder(SharedString::from(
                    off.parent_folder.as_ref().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
                ));
                ui.set_off_parent_name(SharedString::from(off.parent_name.clone()));
                ui.set_off_scanning(off.scanning);
                ui.set_off_running(off.running);
                ui.set_off_overall_progress(off.overall_progress);
                ui.set_off_error(SharedString::from(off.error.clone().unwrap_or_default()));

                let card_infos: Vec<crate::OffloadCardInfo> = off.cards.iter().map(|card| {
                    crate::OffloadCardInfo {
                        mount: SharedString::from(card.mount.to_string_lossy().as_ref()),
                        volume_label: SharedString::from(card.volume_label.clone()),
                        device_name: SharedString::from(card.device_name.clone()),
                        name_source: SharedString::from(format!("{:?}", card.name_source)),
                        media_file_count: card.media_file_count as i32,
                        total_bytes: SharedString::from(format_bytes(card.total_bytes)),
                    }
                }).collect();
                ui.set_off_cards(ModelRc::new(VecModel::<crate::OffloadCardInfo>::from(card_infos)));

                let device_status: Vec<crate::OffloadDeviceStatus> = off.device_progress.iter().map(|dp| {
                    let state_text = match dp.state {
                        gui_engine::offload::OffloadDeviceState::Pending => "Pending",
                        gui_engine::offload::OffloadDeviceState::Copying => "Copying",
                        gui_engine::offload::OffloadDeviceState::Done => "Done",
                        gui_engine::offload::OffloadDeviceState::Failed(_) => "Failed",
                        gui_engine::offload::OffloadDeviceState::Skipped => "Skipped",
                    };
                    let error_str = match &dp.state {
                        gui_engine::offload::OffloadDeviceState::Failed(e) => e.clone(),
                        _ => String::new(),
                    };
                    crate::OffloadDeviceStatus {
                        device_name: SharedString::from(dp.device_name.clone()),
                        state_text: SharedString::from(state_text),
                        files_total: dp.files_total as i32,
                        files_done: dp.files_done as i32,
                        progress: if dp.files_total > 0 { dp.files_done as f32 / dp.files_total as f32 } else { 0.0 },
                        error: SharedString::from(error_str),
                    }
                }).collect();
                ui.set_off_device_progress(ModelRc::new(VecModel::<crate::OffloadDeviceStatus>::from(device_status)));

                let completed: Vec<SharedString> = off.completed_devices.iter()
                    .map(|d| SharedString::from(d.clone()))
                    .collect();
                ui.set_off_completed_devices(ModelRc::new(VecModel::<SharedString>::from(completed)));
            }

            // 32. Perf diagnostics
            let poll_elapsed = poll_start.elapsed();
            if tick % 250 == 0 {
                info!("[PERF] Poll tick #{}: {}ms", tick, poll_elapsed.as_micros() as f64 / 1000.0);
            }
        },
    );

    Box::leak(Box::new(poll_timer));
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