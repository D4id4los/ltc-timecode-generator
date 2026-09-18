use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use audio_core::{AudioCore, AudioEvent};
use log::{error, info, warn};

use crate::command::GuiCommand;
use crate::state::{AppStateSnapshot, ClapLogItem};
use crate::timecode;

const TICK_INTERVAL: Duration = Duration::from_millis(40);
const TARGET_ARM_ANGLE: f32 = -25.0 * std::f32::consts::PI / 180.0;
const MAX_RECOVERY_ATTEMPTS: u8 = 3;
const MAX_CLAP_LOGS: usize = 1000;
const BUFFER_SIZE: u32 = 0;

pub fn engine_main(cmd_rx: Receiver<GuiCommand>, state: Arc<ArcSwap<AppStateSnapshot>>) {
    let mut current = AppStateSnapshot::initial();
    let core = AudioCore::new();
    let mut last_tick = Instant::now();

    let mut recovery_attempts: u8 = 0;
    let mut log_id_counter: u64 = 0;
    let mut last_device_id: Option<String> = None;
    let mut previous_device: Option<usize> = None;

    loop {
        let now = Instant::now();
        let dt = (now - last_tick).as_secs_f32();
        last_tick = now;

        // 1. Drain all pending commands
        while let Ok(cmd) = cmd_rx.try_recv() {
            process_command(
                cmd,
                &core,
                &mut current,
                &mut recovery_attempts,
                &mut log_id_counter,
                &mut last_device_id,
                &mut previous_device,
            );
        }

        // 2. Poll current timecode if playing
        if current.is_playing {
            current.current_timecode = core.current_timecode();
        }

        // 3. Drain events from AudioCore
        for event in core.drain_events() {
            handle_event(event, &core, &mut current, &mut recovery_attempts);
        }

        // 4. Animation: flash alpha decays at 2.0/s
        if current.clap_flash_alpha > 0.0 {
            current.clap_flash_alpha = (current.clap_flash_alpha - dt * 2.0).max(0.0);
        }

        // 5. Animation: arm angle exponential decay toward rest position at 4.0/s
        current.clap_arm_angle += (TARGET_ARM_ANGLE - current.clap_arm_angle)
            * (1.0 - (-4.0 * dt).exp());

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

#[allow(clippy::too_many_arguments)]
fn process_command(
    cmd: GuiCommand,
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    recovery_attempts: &mut u8,
    log_id_counter: &mut u64,
    last_device_id: &mut Option<String>,
    previous_device: &mut Option<usize>,
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
        GuiCommand::ClearLogs => {
            state.logs.clear();
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
            state.is_playing = false;
            state.status_message = "Stream dead".to_string();
        }
        AudioEvent::RecoveryNeeded { .. } | AudioEvent::StreamDied => {
            if *recovery_attempts < MAX_RECOVERY_ATTEMPTS {
                *recovery_attempts += 1;
                state.status_message =
                    format!("Recovery attempt {}/{}", recovery_attempts, MAX_RECOVERY_ATTEMPTS);
                attempt_recovery(core, state, recovery_attempts);
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
) {
    let was_playing = state.is_playing;
    let stored_tc = state.current_timecode;

    let _ = core.stop_ltc();
    let _ = core.stop_output();
    state.audio_initialized = false;
    state.is_playing = false;

    if ensure_audio_init(core, state, &mut None, recovery_attempts)
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