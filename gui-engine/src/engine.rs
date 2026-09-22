use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use audio_core::{AudioCore, AudioEvent, DecodeConfig, DecodeProgress, LtcDetectionResult, WavChunkReader};
use log::{debug, error, info, warn};

use crate::command::GuiCommand;
use crate::converter::{query_ffmpeg_capabilities, FfmpegCapabilities};
use crate::ffprobe;
use crate::state::{AppStateSnapshot, ClapLogItem};
use crate::timecode;

const TICK_INTERVAL: Duration = Duration::from_millis(40);
const TARGET_ARM_ANGLE: f32 = -25.0 * std::f32::consts::PI / 180.0;
const ARM_SETTLE_EPS: f32 = 1.0 * std::f32::consts::PI / 180.0; // 1 degree
const MAX_RECOVERY_ATTEMPTS: u8 = 3;
const MAX_CLAP_LOGS: usize = 1000;
const BUFFER_SIZE: u32 = 0;

pub fn engine_main(cmd_rx: Receiver<GuiCommand>, state: Arc<ArcSwap<AppStateSnapshot>>, use_libltc: bool) {
    engine_main_with_probe(cmd_rx, state, use_libltc, query_ffmpeg_capabilities)
}

/// Internal message sent from the ffmpeg-capability probe thread.
struct FfmpegProbeResult {
    caps: FfmpegCapabilities,
}

/// Like [`engine_main`] but accepts an injectable capability-probe function
/// for testing.
pub fn engine_main_with_probe<F>(
    cmd_rx: Receiver<GuiCommand>,
    state: Arc<ArcSwap<AppStateSnapshot>>,
    use_libltc: bool,
    probe_fn: F,
) where
    F: FnOnce() -> FfmpegCapabilities + Send + 'static,
{
    let mut current = AppStateSnapshot::initial();
    current.use_libltc = use_libltc;
    current.ffmpeg_probe_running = true;
    let core = AudioCore::new();
    let mut last_tick = Instant::now();

    let mut recovery_attempts: u8 = 0;
    let mut log_id_counter: u64 = 0;
    let mut last_device_id: Option<String> = None;
    let mut previous_device: Option<usize> = None;

    // Internal result channel for async decode operations
    let (decode_result_tx, decode_result_rx) =
        std::sync::mpsc::channel::<LtcDecodeResult>();

    // Internal result channel for ffmpeg capability probe
    let (caps_tx, caps_rx) =
        std::sync::mpsc::channel::<FfmpegProbeResult>();

    // Spawn the ffmpeg capability probe on a background thread
    std::thread::Builder::new()
        .name("ffmpeg-probe".into())
        .spawn(move || {
            let caps = probe_fn();
            let _ = caps_tx.send(FfmpegProbeResult { caps });
        })
        .expect("failed to spawn ffmpeg-probe thread");

    // Chunked decode progress / cancel tracking
    let mut decode_cancel: Option<Arc<AtomicBool>> = None;
    let mut decode_progress: Option<(usize, Arc<AtomicUsize>)> = None;

    loop {
        let now = Instant::now();
        let dt = (now - last_tick).as_secs_f32();
        last_tick = now;

        // 1. Drain all pending commands
        loop {
            match cmd_rx.try_recv() {
                Ok(GuiCommand::Shutdown) => {
                    let _ = core.stop_ltc();
                    let _ = core.stop_output();
                    info!("Engine shutdown via Shutdown command");
                    return;
                }
                Ok(cmd) => {
                    process_command(
                        cmd,
                        &core,
                        &mut current,
                        &mut recovery_attempts,
                        &mut log_id_counter,
                        &mut last_device_id,
                        &mut previous_device,
                        &decode_result_tx,
                        &mut decode_cancel,
                        &mut decode_progress,
                    );
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    info!("Engine shutdown via channel disconnect");
                    let _ = core.stop_ltc();
                    let _ = core.stop_output();
                    return;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        // 1.5 Drain async decode results
        loop {
            match decode_result_rx.try_recv() {
                Ok(LtcDecodeResult { path, generation, result }) => {
                    // Only accept result if generation matches (discard stale results
                    // from rapid re-clicks)
                    if generation == current.ltc_decode_generation {
                        current.ltc_is_detecting = false;
                        decode_cancel = None;
                        decode_progress = None;
                        current.ltc_decode_progress_pct = 1.0;
                        current.ltc_decode_progress_str = String::new();
                        match result {
                            Ok(r) => {
                                let first_offset = r.first_ltc_timecode_secs;
                                let tc0_secs = r.timecodes.first().map(|t| t.timecode_secs).unwrap_or(-1.0);
                                info!(
                                    "LTC decode result received: path={}, status={:?}, fps={:.2}, valid={}/{}, \
                                     confidence={:.1}%, first_ltc_timecode_secs={:.3}s, timecodes[0].secs={:.3}s, \
                                     diff={:.6}s, audio_duration={:.2}s",
                                    path, r.status, r.detected_fps, r.valid_frames, r.total_possible_frames,
                                    r.avg_confidence * 100.0, first_offset, tc0_secs,
                                    (first_offset - tc0_secs).abs(), r.total_audio_duration_secs,
                                );

                                current.ltc_decode_result = Some(r.clone());
                                current.ltc_decode_error = None;
                                let summary = format!(
                                    "LTC decode: {} frames (confidence {:.1}%, {} fps{})",
                                    r.valid_frames,
                                    r.avg_confidence * 100.0,
                                    r.detected_fps,
                                    if r.drop_frame { " DF" } else { "" },
                                );
                                current.status_message = summary;
                                info!("LTC decode completed: {}", path);
                            }
                            Err(e) => {
                                let is_cancel = e == "Decode canceled by user";
                                current.ltc_decode_result = None;
                                current.ltc_decode_error = if is_cancel { None } else { Some(e.clone()) };
                                current.status_message = if is_cancel {
                                    "Decode canceled".to_string()
                                } else {
                                    format!("Parse failed: {}", e)
                                };
                                if !is_cancel {
                                    error!("LTC decode failed: {} — {}", path, e);
                                } else {
                                    info!("LTC decode canceled: {}", path);
                                }
                            }
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    warn!("LTC decode result channel disconnected");
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        // 1.6 Drain ffmpeg capability probe result
        drain_ffmpeg_probe_result(&caps_rx, &mut current);

        // 1.7 Poll chunked decode progress
        if current.ltc_is_detecting {
            if let Some((total, ref completed)) = decode_progress {
                let done = completed.load(Ordering::Relaxed);
                let pct = if total > 0 { done as f32 / total as f32 } else { 0.0 };
                current.ltc_decode_progress_pct = pct;
                current.ltc_decode_progress_str = format!("Chunk {}/{}", done.min(total), total);
            }
        } else {
            decode_progress = None;
            current.ltc_decode_progress_pct = 0.0;
            current.ltc_decode_progress_str = String::new();
        }

        // 2. Poll current timecode if playing
        if current.is_playing {
            current.current_timecode = core.current_timecode();
        }

        // 3. Drain events from AudioCore
        for event in core.drain_events() {
            handle_event(event, &core, &mut current, &mut recovery_attempts, &mut last_device_id);
        }

        // 4. Animation: flash alpha decays at 2.0/s
        if current.clap_flash_alpha > 0.0 {
            current.clap_flash_alpha = (current.clap_flash_alpha - dt * 2.0).max(0.0);
        }

        // 5. Animation: arm angle exponential decay toward rest position at 4.0/s
        current.clap_arm_angle += (TARGET_ARM_ANGLE - current.clap_arm_angle)
            * (1.0 - (-4.0 * dt).exp());

        // 5.5 Determine whether clap animation is still visibly in progress.
        // The GUI uses this flag to decide whether to render at 60 fps
        // (for smooth animation) or to throttle to a lower rate.
        current.clap_animating = current.clap_flash_alpha > 0.0
            || (current.clap_arm_angle - TARGET_ARM_ANGLE).abs() > ARM_SETTLE_EPS;

        // 6. System time
        current.system_time = timecode::chrono_now_string();

        // 7. Publish state
        current.generation += 1;
        state.store(Arc::new(current.clone()));

        // 8. Sleep until next tick
        let next_tick = last_tick + TICK_INTERVAL;
        if let Some(sleep_dur) = next_tick.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep_dur);
        }
    }
}

/// Internal message sent from a spawned decode thread back to the engine loop.
struct LtcDecodeResult {
    path: String,
    generation: u64,
    result: Result<LtcDetectionResult, String>,
}

#[allow(clippy::too_many_arguments)]
fn process_command(
    cmd: GuiCommand,
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    recovery_attempts: &mut u8,
    log_id_counter: &mut u64,
    last_device_id: &mut Option<String>,
    previous_device: &mut Option<usize>,
    decode_result_tx: &Sender<LtcDecodeResult>,
    decode_cancel: &mut Option<Arc<AtomicBool>>,
    decode_progress: &mut Option<(usize, Arc<AtomicUsize>)>,
) {
    match cmd {
        GuiCommand::StartLtc => {
            ensure_audio_init(core, state, last_device_id, recovery_attempts);
            if !state.audio_initialized {
                state.status_message = "Cannot start — audio not initialized".to_string();
                return;
            }
            match core.start_ltc(
                state.start_timecode,
                state.fps,
                state.drop_frame,
                state.ltc_channel.clone(),
                state.ltc_volume,
            ) {
                Ok(()) => {
                    state.is_playing = true;
                    state.status_message = "Streaming LTC".to_string();
                    state.current_timecode = state.start_timecode;
                }
                Err(e) => {
                    error!("Failed to start LTC: {}", e);
                    state.status_message = format!("Start failed: {}", e);
                }
            }
        }

        GuiCommand::StopLtc => {
            let _ = core.stop_ltc();
            state.is_playing = false;
            state.wake_lock_active = false;
            state.status_message = "Stopped".to_string();
        }

        GuiCommand::Reset => {
            let _ = core.reset_ltc(state.start_timecode);
            state.current_timecode = state.start_timecode;
            state.status_message = "Reset".to_string();
        }

        GuiCommand::ToggleLock => {
            state.is_locked = !state.is_locked;
        }

        GuiCommand::Clap => {
            let _ = core.play_beep(
                state.sample_rate,
                state.beep_frequency,
                state.beep_duration,
                state.beep_volume,
                &state.beep_channel,
            );
            state.clap_flash_alpha = 1.0;
            state.clap_arm_angle = 0.0;

            let tc_str = timecode::timecode_to_string(state.current_timecode, state.drop_frame);
            let ms_str =
                timecode::timecode_to_ms_string(state.current_timecode, state.fps);
            *log_id_counter += 1;
            let ts = timecode::chrono_now_string();
            let note = format!("Scene {}", state.scene);

            state.logs.push(ClapLogItem {
                id: format!("{}", log_id_counter),
                timestamp: ts,
                timecode: tc_str,
                milliseconds: ms_str,
                note,
            });
            while state.logs.len() > MAX_CLAP_LOGS {
                state.logs.remove(0);
            }

            if state.auto_increment_take {
                state.take = state.take.saturating_add(1);
            }
            state.status_message = "Clap!".to_string();
        }

        GuiCommand::SetStartTimecode(tc) => {
            state.start_timecode = tc;
        }

        GuiCommand::SetFpsIndex(index) => {
            if index < timecode::FPS_OPTIONS.len() {
                state.fps_index = index;
                state.fps = timecode::FPS_OPTIONS[index].fps;
                state.drop_frame = timecode::FPS_OPTIONS[index].drop_frame;
            }
        }

        GuiCommand::SetSampleRate(rate) => {
            state.sample_rate = rate;
        }

        GuiCommand::SetDevice(index) => {
            if state.is_playing {
                let _ = core.stop_ltc();
                state.is_playing = false;
            }

            let _ = core.stop_output();
            state.audio_initialized = false;

            *previous_device = Some(state.selected_device);
            state.selected_device = index;

            if !try_init_device(core, state, last_device_id, recovery_attempts) {
                // Revert to previous device
                if let Some(prev) = previous_device {
                    state.selected_device = *prev;
                    if try_init_device(core, state, last_device_id, recovery_attempts) {
                        state.status_message =
                            "Device selection reverted to previous".to_string();
                    }
                }
            }
        }

        GuiCommand::RefreshDevices => {
            match audio_core::list_audio_devices() {
                Ok(devices) => {
                    state.devices = devices;
                    if state.selected_device >= state.devices.len() {
                        state.selected_device = 0;
                    }
                    state.status_message =
                        format!("{} devices found", state.devices.len());
                }
                Err(e) => {
                    error!("Failed to list devices: {}", e);
                    state.status_message = format!("Device scan failed: {}", e);
                }
            }
        }

        GuiCommand::InitAudio => {
            ensure_audio_init(core, state, last_device_id, recovery_attempts);
        }

        GuiCommand::SetLtcChannel(ch) => {
            state.ltc_channel = ch;
        }
        GuiCommand::SetBeepChannel(ch) => {
            state.beep_channel = ch;
        }
        GuiCommand::SetLtcVolume(vol) => {
            state.ltc_volume = vol;
        }
        GuiCommand::SetBeepVolume(vol) => {
            state.beep_volume = vol;
        }
        GuiCommand::SetBeepFrequency(freq) => {
            state.beep_frequency = freq;
        }
        GuiCommand::SetBeepDuration(dur) => {
            state.beep_duration = dur;
        }
        GuiCommand::SetScene(scene) => {
            state.scene = scene;
        }
        GuiCommand::SetTake(take) => {
            state.take = take;
        }
        GuiCommand::SetRoll(roll) => {
            state.roll = roll;
        }
        GuiCommand::SetAutoIncrement(val) => {
            state.auto_increment_take = val;
        }
        GuiCommand::SetTheme(dark) => {
            state.is_dark_theme = dark;
        }
        GuiCommand::ToggleTheme => {
            state.is_dark_theme = !state.is_dark_theme;
        }
        GuiCommand::ClearLogs => {
            state.logs.clear();
        }

        GuiCommand::SetDecodeFpsIndex(index) => {
            if let Some(opt) = timecode::FPS_OPTIONS.get(index) {
                state.decode_fps_index = index;
                state.decode_fps = opt.fps;
                state.decode_drop_frame = opt.drop_frame;
                info!("Decode FPS set to: {} (index={})", opt.name, index);
            }
        }

        GuiCommand::CancelDecode => {
            if let Some(ref cancel) = decode_cancel {
                info!("CancelDecode: signaling cancel flag");
                cancel.store(true, Ordering::Relaxed);
            }
        }

        GuiCommand::ProbeVideo(path) => {
            info!("Probing video file for audio streams: {}", path);
            match ffprobe::probe_video_audio(Path::new(&path)) {
                Ok(probe) => {
                    state.ltc_probe = Some(probe.clone());
                    state.ltc_selected_stream = 0;
                    state.ltc_selected_channel = 0;
                    state.ltc_decode_is_video = true;
                    state.status_message = format!(
                        "Video probed: {} audio stream(s), {} total channel(s)",
                        probe.streams.len(),
                        probe.total_audio_channels,
                    );
                    info!("Probe succeeded: {} streams, {} channels — {}", probe.streams.len(), probe.total_audio_channels, path);
                }
                Err(e) => {
                    state.ltc_probe = None;
                    state.ltc_decode_error = Some(e.clone());
                    state.ltc_decode_is_video = false;
                    state.status_message = format!("Video probe failed: {}", e);
                    error!("Video probe failed: {} — {}", path, e);
                }
            }
        }

        GuiCommand::ParseLtcVideo(path, stream_index, channel_index) => {
            let decoder_name = if state.use_libltc { "libltc" } else { "builtin" };
            info!(
                "LTC video decode requested: {} (stream={}, channel={}, decoder={}, fps={})",
                path, stream_index, channel_index, decoder_name, state.decode_fps,
            );

            state.ltc_is_detecting = true;
            state.ltc_decode_result = None;
            state.ltc_decode_error = None;
            state.ltc_decode_generation = state.ltc_decode_generation.wrapping_add(1);
            state.status_message = format!(
                "Extracting audio from: {} stream={} ch={}",
                path, stream_index, channel_index,
            );

            let capture_gen = state.ltc_decode_generation;
            let use_libltc = state.use_libltc;
            let decode_fps = state.decode_fps;
            let decode_drop_frame = state.decode_drop_frame;

            // Hardening: validate the selection against the probe data so a
            // stale or out-of-range GUI state fails fast with a clear message
            // instead of invoking ffmpeg on a nonexistent stream.
            if let Some(ref probe) = state.ltc_probe {
                let available: Vec<usize> =
                    probe.streams.iter().map(|s| s.stream_index).collect();
                let validation_error = match probe
                    .streams
                    .iter()
                    .find(|s| s.stream_index == stream_index)
                {
                    None => Some(format!(
                        "Stream {} not found in '{}' (available audio streams: {:?})",
                        stream_index, path, available
                    )),
                    Some(s) if channel_index >= s.channels => Some(format!(
                        "Channel {} out of range for stream {} in '{}' ({} channels available)",
                        channel_index, stream_index, path, s.channels
                    )),
                    Some(_) => None,
                };
                if let Some(e) = validation_error {
                    error!("LTC video decode rejected: {}", e);
                    state.ltc_is_detecting = false;
                    state.ltc_decode_error = Some(e.clone());
                    state.status_message = format!("Parse failed: {}", e);
                    return;
                }
            }

            let tx = decode_result_tx.clone();

            std::thread::spawn(move || {
                let tmp_dir = std::env::temp_dir();
                let tmp_wav = tmp_dir.join(format!(
                    "ltc_extract_{}_{}_{}_{}.wav",
                    std::process::id(),
                    capture_gen,
                    stream_index,
                    channel_index,
                ));

                if let Err(e) = ffprobe::extract_audio_channel(
                    Path::new(&path),
                    stream_index,
                    channel_index,
                    &tmp_wav,
                ) {
                    let _ = tx.send(LtcDecodeResult {
                        path,
                        generation: capture_gen,
                        result: Err(e),
                    });
                    return;
                }

                let wav_path = tmp_wav.clone();

                let result = match WavChunkReader::open(&wav_path) {
                    Ok((reader, _start)) => {
                        let total_mono = reader.total_mono_samples();
                        drop(reader);
                        let config = DecodeConfig::default();
                        let chunk_mono =
                            (config.chunk_size_bytes / 3) as usize; // pcm_s24le = 3 bytes/sample
                        let overlap_samples =
                            (config.overlap_seconds * 48000.0) as usize;
                        let chunk_mono = chunk_mono.max(overlap_samples * 2);

                        if total_mono <= chunk_mono + overlap_samples {
                            audio_core::decode_ltc_with_decoder(
                                &wav_path, use_libltc, decode_fps, decode_drop_frame,
                            )
                        } else {
                            audio_core::decode_ltc_chunked(
                                &wav_path, use_libltc, decode_fps, decode_drop_frame,
                                config, &DecodeProgress::new(1),
                            )
                        }
                    }
                    Err(e) => Err(format!("Failed to open extracted WAV: {}", e)),
                };

                let _ = std::fs::remove_file(&tmp_wav);

                let _ = tx.send(LtcDecodeResult {
                    path,
                    generation: capture_gen,
                    result,
                });
            });
        }

        GuiCommand::SetLtcDecodeStream(idx) => {
            state.ltc_selected_stream = idx;
        }

        GuiCommand::SetLtcDecodeChannel(idx) => {
            state.ltc_selected_channel = idx;
        }

        GuiCommand::ParseLtcWavFile(path) => {
            let decoder_name = if state.use_libltc { "libltc" } else { "builtin" };
            info!("LTC decode requested for: {} (decoder: {}, fps: {})", path, decoder_name, state.decode_fps);

            // Quick open to calculate chunk count
            let (chunk_count, _sr, _ch, _spec) = match WavChunkReader::open(Path::new(&path)) {
                Ok((reader, _start)) => {
                    let total_mono = reader.total_mono_samples();
                    let sr = reader.sample_rate();
                    let ch = reader.channels();
                    let spec = *reader.spec();
                    let bytes_per_mono = (ch as u64) * (spec.bits_per_sample as u64 / 8);
                    let config = DecodeConfig::default();
                    let chunk_mono = (config.chunk_size_bytes / bytes_per_mono.max(1)) as usize;
                    let overlap_samples = (config.overlap_seconds * sr as f64) as usize;
                    let chunk_mono = chunk_mono.max(overlap_samples * 2);

                    if total_mono <= chunk_mono + overlap_samples {
                        (1usize, sr, ch, spec)
                    } else {
                        let mut count = 0usize;
                        let mut pos = 0usize;
                        while pos < total_mono {
                            count += 1;
                            let end = (pos + chunk_mono).min(total_mono);
                            if end >= total_mono { break; }
                            let next = end.saturating_sub(overlap_samples);
                            if next <= pos || next >= total_mono { break; }
                            pos = next;
                        }
                        (count, sr, ch, spec)
                    }
                }
                Err(e) => {
                    error!("Failed to open WAV for chunked decode: {}", e);
                    state.ltc_is_detecting = false;
                    state.ltc_decode_error = Some(e.clone());
                    state.status_message = format!("Parse failed: {}", e);
                    return;
                }
            };

            state.ltc_is_detecting = true;
            state.ltc_decode_result = None;
            state.ltc_decode_error = None;
            state.ltc_decode_generation = state.ltc_decode_generation.wrapping_add(1);
            state.status_message = format!(
                "Decoding LTC from: {} [{}] at {:.2} fps ({} chunks)",
                path, decoder_name, state.decode_fps, chunk_count
            );

            let progress = DecodeProgress::new(chunk_count);
            *decode_cancel = Some(progress.cancel_flag.clone());
            *decode_progress = Some((chunk_count, progress.chunks_completed.clone()));

            let capture_gen = state.ltc_decode_generation;
            let tx = decode_result_tx.clone();
            let use_libltc = state.use_libltc;
            let decode_fps = state.decode_fps;
            let decode_drop_frame = state.decode_drop_frame;

            info!("Spawning chunked decode ({} chunks, decoder={}, fps={})",
                chunk_count, decoder_name, decode_fps);

            std::thread::spawn(move || {
                debug!("LTC chunked decode thread spawned for gen={}: {}",
                    capture_gen, path);

                if chunk_count <= 1 {
                    // Small file: use single-threaded decode
                    let result = audio_core::decode_ltc_with_decoder(
                        Path::new(&path), use_libltc, decode_fps, decode_drop_frame,
                    );
                    progress.chunks_completed.store(1, Ordering::Relaxed);
                    let _ = tx.send(LtcDecodeResult {
                        path,
                        generation: capture_gen,
                        result,
                    });
                } else {
                    let config = DecodeConfig::default();
                    let result = audio_core::decode_ltc_chunked(
                        Path::new(&path), use_libltc, decode_fps, decode_drop_frame,
                        config, &progress,
                    );
                    let _ = tx.send(LtcDecodeResult {
                        path,
                        generation: capture_gen,
                        result,
                    });
                }
            });
        }

        GuiCommand::SceneUp => {
            state.scene = state.scene.saturating_add(1);
        }
        GuiCommand::SceneDown => {
            state.scene = state.scene.saturating_sub(1);
        }
        GuiCommand::TakeUp => {
            state.take = state.take.saturating_add(1);
        }
        GuiCommand::TakeDown => {
            state.take = state.take.saturating_sub(1);
        }
        GuiCommand::HourUp => { stepper_hour(state, 1); }
        GuiCommand::HourDown => { stepper_hour(state, -1); }
        GuiCommand::MinuteUp => { stepper_minute(state, 1); }
        GuiCommand::MinuteDown => { stepper_minute(state, -1); }
        GuiCommand::SecondUp => { stepper_second(state, 1); }
        GuiCommand::SecondDown => { stepper_second(state, -1); }
        GuiCommand::FrameUp => { stepper_frame(state, 1); }
        GuiCommand::FrameDown => { stepper_frame(state, -1); }

        GuiCommand::Shutdown => {
            // Handled in the command drain loop before reaching process_command
        }
    }
}

// ── Stepper helpers ─────────────────────────────────────────────────────

fn stepper_hour(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    tc.hours = (tc.hours as i32 + delta).rem_euclid(24) as u32;
    state.start_timecode = tc;
}

fn stepper_minute(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    tc.minutes = (tc.minutes as i32 + delta).rem_euclid(60) as u32;
    state.start_timecode = tc;
}

fn stepper_second(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    tc.seconds = (tc.seconds as i32 + delta).rem_euclid(60) as u32;
    state.start_timecode = tc;
}

fn stepper_frame(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    let max_frame = (state.fps.round() as u32).saturating_sub(1);
    tc.frames = (tc.frames as i32 + delta).rem_euclid(max_frame as i32 + 1) as u32;
    state.start_timecode = tc;
}

// ── Audio init ──────────────────────────────────────────────────────────

fn ensure_audio_init(
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    last_device_id: &mut Option<String>,
    recovery_attempts: &mut u8,
) -> bool {
    if state.audio_initialized {
        return true;
    }

    let device_id = if state.selected_device < state.devices.len() {
        state.devices[state.selected_device].id.clone()
    } else if !state.devices.is_empty() {
        state.selected_device = 0;
        state.devices[0].id.clone()
    } else {
        String::new()
    };

    let max_attempts = 3;
    let mut last_error = String::new();

    for attempt in 1..=max_attempts {
        match core.init_output(&device_id, state.sample_rate, BUFFER_SIZE) {
            Ok(actual_rate) => {
                state.sample_rate = actual_rate;
                state.audio_initialized = true;
                state.sample_format_name = core.sample_format_name();
                *last_device_id = Some(device_id.clone());
                *recovery_attempts = 0;
                info!("Audio initialized at {} Hz on device {}", actual_rate, device_id);
                return true;
            }
            Err(e) => {
                last_error = e;
                if audio_core::is_permanent_device_error(&last_error) {
                    error!("Permanent audio error: {}", last_error);
                    break;
                }
                if attempt < max_attempts {
                    let delay = Duration::from_millis(50 * (1 << (attempt - 1)));
                    warn!(
                        "Audio init attempt {}/{} failed ({}), retrying in {:?}",
                        attempt, max_attempts, last_error, delay
                    );
                    std::thread::sleep(delay);
                }
            }
        }
    }

    error!("Audio init failed after {} attempts: {}", max_attempts, last_error);
    state.status_message = format!("Audio init failed: {}", last_error);
    state.events.push(AudioEvent::StreamError(last_error.clone()));
    false
}

fn try_init_device(
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    last_device_id: &mut Option<String>,
    recovery_attempts: &mut u8,
) -> bool {
    let device_id = if state.selected_device < state.devices.len() {
        state.devices[state.selected_device].id.clone()
    } else {
        return false;
    };

    match core.init_output(&device_id, state.sample_rate, BUFFER_SIZE) {
        Ok(actual_rate) => {
            state.sample_rate = actual_rate;
            state.audio_initialized = true;
            state.sample_format_name = core.sample_format_name();
            *last_device_id = Some(device_id);
            *recovery_attempts = 0;
            info!("Device switched, audio at {} Hz", actual_rate);
            true
        }
        Err(e) => {
            error!("Device init failed: {}", e);
            state.audio_initialized = false;
            false
        }
    }
}

// ── Event handling ──────────────────────────────────────────────────────

fn handle_event(
    event: AudioEvent,
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    recovery_attempts: &mut u8,
    last_device_id: &mut Option<String>,
) {
    let event_str = match &event {
        AudioEvent::StreamError(msg) => format!("Audio stream error: {}", msg),
        AudioEvent::StreamDied => "Audio stream died".to_string(),
        AudioEvent::StreamRecovering { attempt } => {
            format!("Stream recovering (attempt {})", attempt)
        }
        AudioEvent::StreamDead => "Audio stream permanently dead".to_string(),
        AudioEvent::RecoveryNeeded { reason } => {
            format!("Recovery needed: {}", reason)
        }
        AudioEvent::Underrun => "Audio underrun".to_string(),
        AudioEvent::FramesDropped { total } => format!("{} frames dropped", total),
    };

    warn!("{}", event_str);

    match event {
        AudioEvent::StreamDead => {
            // Full teardown-and-recreate: drop the orphaned cpal::Stream,
            // wait for OS driver cleanup, then re-init and restart if playing.
            // The scheduler watchdog already exhausted 3 soft-recovery attempts
            // before emitting StreamDead, so this is the final hard reset.
            state.status_message = "Stream dead — performing hard reset".to_string();
            attempt_recovery(core, state, recovery_attempts, last_device_id);
        }
        AudioEvent::RecoveryNeeded { .. } | AudioEvent::StreamDied => {
            if *recovery_attempts < MAX_RECOVERY_ATTEMPTS {
                *recovery_attempts += 1;
                state.status_message =
                    format!("Recovery attempt {}/{}", recovery_attempts, MAX_RECOVERY_ATTEMPTS);
                attempt_recovery(core, state, recovery_attempts, last_device_id);
            } else {
                state.is_playing = false;
                state.status_message = "Recovery exhausted".to_string();
            }
        }
        _ => {}
    }

    state.events.push(event);
}

fn attempt_recovery(
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    recovery_attempts: &mut u8,
    last_device_id: &mut Option<String>,
) {
    let was_playing = state.is_playing;
    let stored_tc = state.current_timecode;

    let _ = core.stop_ltc();
    let _ = core.stop_output();
    state.audio_initialized = false;
    state.is_playing = false;

    // Allow 150ms for the OS audio driver to release the hardware lock
    // (ALSA/PulseAudio/PipeWire cleanup after dropping the cpal::Stream)
    std::thread::sleep(Duration::from_millis(150));

    if ensure_audio_init(core, state, last_device_id, recovery_attempts)
        && was_playing
    {
        let _ = core.reset_ltc(stored_tc);
        match core.start_ltc(
            stored_tc,
            state.fps,
            state.drop_frame,
            state.ltc_channel.clone(),
            state.ltc_volume,
        ) {
            Ok(()) => {
                state.is_playing = true;
                state.current_timecode = stored_tc;
                info!("Recovery succeeded");
            }
            Err(e) => {
                error!("Recovery start_ltc failed: {}", e);
            }
        }
    }
}

/// Drain the ffmpeg capability probe result from the background thread.
/// Returns `true` if the probe thread disconnected without sending a result
/// (genuine probe failure), `false` otherwise.
fn drain_ffmpeg_probe_result(
    caps_rx: &std::sync::mpsc::Receiver<FfmpegProbeResult>,
    current: &mut AppStateSnapshot,
) -> bool {
    if !current.ffmpeg_probe_running {
        return false;
    }
    match caps_rx.try_recv() {
        Ok(FfmpegProbeResult { caps }) => {
            current.ffmpeg_caps = Some(caps.clone());
            current.ffmpeg_probe_running = false;
            info!(
                "ffmpeg capability probe complete: {} encoder(s), {} format(s), hw_vaapi={}, hw_vulkan={}",
                caps.available_encoders.len(),
                caps.available_formats.len(),
                caps.hw.vaapi_device.is_some(),
                caps.hw.vulkan_available,
            );
            false
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            // Probe thread exited without sending — treat as no ffmpeg
            warn!("ffmpeg capability probe thread disconnected unexpectedly");
            current.ffmpeg_probe_running = false;
            true
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converter::HwDeviceCapabilities;
    use crate::state::AppStateSnapshot;
    use audio_core::Timecode;
    use std::collections::BTreeSet;

    fn setup_state() -> AppStateSnapshot {
        AppStateSnapshot::initial()
    }

    // ── stepper_hour ──────────────────────────────────────────────────────

    #[test]
    fn test_stepper_hour_up() {
        let mut state = setup_state();
        state.start_timecode.hours = 0;
        stepper_hour(&mut state, 1);
        assert_eq!(state.start_timecode.hours, 1);
    }

    #[test]
    fn test_stepper_hour_down() {
        let mut state = setup_state();
        state.start_timecode.hours = 5;
        stepper_hour(&mut state, -1);
        assert_eq!(state.start_timecode.hours, 4);
    }

    #[test]
    fn test_stepper_hour_wrap_forward() {
        let mut state = setup_state();
        state.start_timecode.hours = 23;
        stepper_hour(&mut state, 1);
        assert_eq!(state.start_timecode.hours, 0);
    }

    #[test]
    fn test_stepper_hour_wrap_backward() {
        let mut state = setup_state();
        state.start_timecode.hours = 0;
        stepper_hour(&mut state, -1);
        assert_eq!(state.start_timecode.hours, 23);
    }

    #[test]
    fn test_stepper_hour_multiple_steps() {
        let mut state = setup_state();
        state.start_timecode.hours = 22;
        stepper_hour(&mut state, 5);
        assert_eq!(state.start_timecode.hours, 3);
    }

    #[test]
    fn test_stepper_hour_other_fields_unchanged() {
        let mut state = setup_state();
        state.start_timecode = Timecode { hours: 5, minutes: 30, seconds: 15, frames: 10 };
        stepper_hour(&mut state, 1);
        assert_eq!(state.start_timecode.minutes, 30);
        assert_eq!(state.start_timecode.seconds, 15);
        assert_eq!(state.start_timecode.frames, 10);
    }

    // ── stepper_minute ────────────────────────────────────────────────────

    #[test]
    fn test_stepper_minute_up() {
        let mut state = setup_state();
        state.start_timecode.minutes = 0;
        stepper_minute(&mut state, 1);
        assert_eq!(state.start_timecode.minutes, 1);
    }

    #[test]
    fn test_stepper_minute_down() {
        let mut state = setup_state();
        state.start_timecode.minutes = 30;
        stepper_minute(&mut state, -1);
        assert_eq!(state.start_timecode.minutes, 29);
    }

    #[test]
    fn test_stepper_minute_wrap_forward() {
        let mut state = setup_state();
        state.start_timecode.minutes = 59;
        stepper_minute(&mut state, 1);
        assert_eq!(state.start_timecode.minutes, 0);
    }

    #[test]
    fn test_stepper_minute_wrap_backward() {
        let mut state = setup_state();
        state.start_timecode.minutes = 0;
        stepper_minute(&mut state, -1);
        assert_eq!(state.start_timecode.minutes, 59);
    }

    #[test]
    fn test_stepper_minute_large_delta() {
        let mut state = setup_state();
        state.start_timecode.minutes = 5;
        stepper_minute(&mut state, 100);
        assert_eq!(state.start_timecode.minutes, 45);
    }

    // ── stepper_second ────────────────────────────────────────────────────

    #[test]
    fn test_stepper_second_up() {
        let mut state = setup_state();
        state.start_timecode.seconds = 0;
        stepper_second(&mut state, 1);
        assert_eq!(state.start_timecode.seconds, 1);
    }

    #[test]
    fn test_stepper_second_wrap_forward() {
        let mut state = setup_state();
        state.start_timecode.seconds = 59;
        stepper_second(&mut state, 1);
        assert_eq!(state.start_timecode.seconds, 0);
    }

    #[test]
    fn test_stepper_second_wrap_backward() {
        let mut state = setup_state();
        state.start_timecode.seconds = 0;
        stepper_second(&mut state, -1);
        assert_eq!(state.start_timecode.seconds, 59);
    }

    #[test]
    fn test_stepper_second_down() {
        let mut state = setup_state();
        state.start_timecode.seconds = 30;
        stepper_second(&mut state, -5);
        assert_eq!(state.start_timecode.seconds, 25);
    }

    // ── stepper_frame ─────────────────────────────────────────────────────

    #[test]
    fn test_stepper_frame_up() {
        let mut state = setup_state();
        state.fps = 25.0;
        state.start_timecode.frames = 0;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 1);
    }

    #[test]
    fn test_stepper_frame_wrap_forward_25fps() {
        let mut state = setup_state();
        state.fps = 25.0;
        state.start_timecode.frames = 24;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0);
    }

    #[test]
    fn test_stepper_frame_wrap_forward_30fps() {
        let mut state = setup_state();
        state.fps = 30.0;
        state.start_timecode.frames = 29;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0);
    }

    #[test]
    fn test_stepper_frame_wrap_forward_24fps() {
        let mut state = setup_state();
        state.fps = 24.0;
        state.start_timecode.frames = 23;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0);
    }

    #[test]
    fn test_stepper_frame_down() {
        let mut state = setup_state();
        state.fps = 25.0;
        state.start_timecode.frames = 15;
        stepper_frame(&mut state, -1);
        assert_eq!(state.start_timecode.frames, 14);
    }

    #[test]
    fn test_stepper_frame_wrap_backward_25fps() {
        let mut state = setup_state();
        state.fps = 25.0;
        state.start_timecode.frames = 0;
        stepper_frame(&mut state, -1);
        assert_eq!(state.start_timecode.frames, 24);
    }

    #[test]
    fn test_stepper_frame_wrap_backward_30fps() {
        let mut state = setup_state();
        state.fps = 30.0;
        state.start_timecode.frames = 0;
        stepper_frame(&mut state, -1);
        assert_eq!(state.start_timecode.frames, 29);
    }

    #[test]
    fn test_stepper_frame_fps_changes_max() {
        let mut state = setup_state();
        state.fps = 25.0;
        state.start_timecode.frames = 24;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0, "25fps: 24→0");

        state.fps = 30.0;
        state.start_timecode.frames = 29;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0, "30fps: 29→0");

        state.fps = 24.0;
        state.start_timecode.frames = 23;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0, "24fps: 23→0");
    }

    #[test]
    fn test_stepper_frame_large_delta() {
        let mut state = setup_state();
        state.fps = 25.0;
        state.start_timecode.frames = 5;
        stepper_frame(&mut state, 50);
        assert_eq!(state.start_timecode.frames, 5);
    }

    // ── ProcessCommand: basic command effects on state ────────────────────

    #[test]
    fn test_process_command_toggle_lock() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        // ToggleLock on: false → true
        process_command(
            GuiCommand::ToggleLock, &core, &mut state, &mut recovery,
            &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None,
        );
        assert!(state.is_locked);
    }

    #[test]
    fn test_process_command_toggle_lock_twice() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::ToggleLock, &core, &mut state, &mut recovery,
            &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        process_command(GuiCommand::ToggleLock, &core, &mut state, &mut recovery,
            &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(!state.is_locked);
    }

    #[test]
    fn test_process_command_set_fps_index_valid() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetFpsIndex(4), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.fps_index, 4);
        assert_eq!(state.fps, 30.0);
        assert!(!state.drop_frame);
    }

    #[test]
    fn test_process_command_set_fps_index_drop_frame() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetFpsIndex(3), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.fps_index, 3);
        assert!((state.fps - 29.97).abs() < 0.01);
        assert!(state.drop_frame);
    }

    #[test]
    fn test_process_command_set_fps_index_out_of_range() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetFpsIndex(99), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.fps_index, 1);
        assert_eq!(state.fps, 25.0);
    }

    #[test]
    fn test_process_command_set_theme() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetTheme(true), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(state.is_dark_theme);

        process_command(GuiCommand::SetTheme(false), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(!state.is_dark_theme);
    }

    #[test]
    fn test_process_command_toggle_theme() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::ToggleTheme, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(state.is_dark_theme, "toggle from initial false → true");

        process_command(GuiCommand::ToggleTheme, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(!state.is_dark_theme, "toggle again true → false");
    }

    #[test]
    fn test_process_command_clear_logs() {
        let mut state = setup_state();
        state.logs.push(ClapLogItem {
            id: "1".into(), timestamp: "12:00:00".into(),
            timecode: "01:00:00:00".into(), milliseconds: "0".into(), note: "test".into(),
        });
        state.logs.push(ClapLogItem {
            id: "2".into(), timestamp: "12:00:01".into(),
            timecode: "01:00:00:01".into(), milliseconds: "0".into(), note: "test2".into(),
        });
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::ClearLogs, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(state.logs.is_empty());
    }

    #[test]
    fn test_process_command_set_ltc_channel() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetLtcChannel("both".into()), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.ltc_channel, "both");
    }

    #[test]
    fn test_process_command_set_beep_volume() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetBeepVolume(0.75), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!((state.beep_volume - 0.75).abs() < 1e-6);
    }

    #[test]
    fn test_process_command_set_start_timecode() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();
        let tc = Timecode { hours: 10, minutes: 20, seconds: 30, frames: 15 };

        process_command(GuiCommand::SetStartTimecode(tc), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.start_timecode, tc);
    }

    #[test]
    fn test_process_command_set_scene_take_roll() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetScene(42), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.scene, 42);

        process_command(GuiCommand::SetTake(7), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.take, 7);

        process_command(GuiCommand::SetRoll("B002".into()), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.roll, "B002");
    }

    #[test]
    fn test_process_command_scene_up_down() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        state.scene = 5;
        process_command(GuiCommand::SceneUp, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.scene, 6);

        process_command(GuiCommand::SceneDown, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.scene, 5);
    }

    #[test]
    fn test_process_command_take_up_down() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        state.take = 3;
        process_command(GuiCommand::TakeUp, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.take, 4);

        process_command(GuiCommand::TakeDown, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.take, 3);
    }

    #[test]
    fn test_process_command_scene_down_at_zero() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        state.scene = 0;
        process_command(GuiCommand::SceneDown, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.scene, 0, "scene should not go below 0");
    }

    #[test]
    fn test_process_command_take_down_at_zero() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        state.take = 0;
        process_command(GuiCommand::TakeDown, &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.take, 0, "take should not go below 0");
    }

    #[test]
    fn test_process_command_set_sample_rate() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetSampleRate(48000), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.sample_rate, 48000);
    }

    #[test]
    fn test_process_command_set_auto_increment() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetAutoIncrement(false), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(!state.auto_increment_take);

        process_command(GuiCommand::SetAutoIncrement(true), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!(state.auto_increment_take);
    }

    #[test]
    fn test_process_command_set_decode_fps_index() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetDecodeFpsIndex(4), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert_eq!(state.decode_fps_index, 4);
        assert_eq!(state.decode_fps, 30.0);
        assert!(!state.decode_drop_frame);
    }

    #[test]
    fn test_process_command_set_decode_fps_index_drop_frame() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetDecodeFpsIndex(3), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        assert!((state.decode_fps - 29.97).abs() < 0.01);
        assert!(state.decode_drop_frame);
    }

    #[test]
    fn test_process_command_set_decode_fps_index_out_of_range() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let (tx, _rx) = std::sync::mpsc::channel();

        process_command(GuiCommand::SetDecodeFpsIndex(99), &core, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &tx,
            &mut None, &mut None);
        // Should not change since index is out of range
        assert_eq!(state.decode_fps_index, 1);
    }

    // ── drain_ffmpeg_probe_result ─────────────────────────────────────────

    #[test]
    fn test_drain_ffmpeg_probe_receives_result_and_clears_no_disconnect() {
        let (tx, rx) = std::sync::mpsc::channel::<FfmpegProbeResult>();

        let caps = FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: BTreeSet::new(),
            available_formats: BTreeSet::new(),
            hw: HwDeviceCapabilities::default(),
            error_message: None,
        };
        tx.send(FfmpegProbeResult { caps: caps.clone() }).unwrap();
        drop(tx);

        let mut state = AppStateSnapshot::initial();
        state.ffmpeg_probe_running = true;

        let unexpected = drain_ffmpeg_probe_result(&rx, &mut state);

        assert!(!unexpected, "should NOT report disconnect when result was received");
        assert!(state.ffmpeg_caps.is_some(), "caps should be stored");
        assert!(!state.ffmpeg_probe_running, "probe flag should be cleared");
    }

    #[test]
    fn test_drain_ffmpeg_probe_disconnect_without_result_still_warns() {
        let (tx, rx) = std::sync::mpsc::channel::<FfmpegProbeResult>();
        drop(tx);

        let mut state = AppStateSnapshot::initial();
        state.ffmpeg_probe_running = true;

        let unexpected = drain_ffmpeg_probe_result(&rx, &mut state);

        assert!(unexpected, "should report disconnect when thread died without sending");
        assert!(state.ffmpeg_caps.is_none(), "caps should NOT be stored");
        assert!(!state.ffmpeg_probe_running, "probe flag should be cleared");
    }

    #[test]
    fn test_drain_ffmpeg_probe_empty_noop_when_not_running() {
        let (_tx, rx) = std::sync::mpsc::channel::<FfmpegProbeResult>();
        let mut state = AppStateSnapshot::initial();
        state.ffmpeg_probe_running = false;

        let unexpected = drain_ffmpeg_probe_result(&rx, &mut state);

        assert!(!unexpected);
        assert!(state.ffmpeg_caps.is_none());
    }
}