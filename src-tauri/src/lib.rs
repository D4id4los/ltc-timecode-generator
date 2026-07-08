use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct Timecode {
    hours: u32,
    minutes: u32,
    seconds: u32,
    frames: u32,
}

#[derive(Serialize)]
struct AudioDeviceInfo {
    id: String,
    name: String,
    is_default: bool,
}

struct BeepState {
    samples: Vec<f32>,
    index: usize,
}

struct LtcStreamState {
    running: bool,
    tc: Timecode,
    fps: f64,
    drop_frame: bool,
    ltc_channel: String,
    ltc_volume: f32,
    sample_rate: u32,
    last_level: (f32, f32),
    frame_duration: Duration,
    next_frame_time: Instant,
    
    stop_signal: Arc<AtomicBool>,
    scheduler_thread: Option<std::thread::JoinHandle<()>>,
}

struct AudioOutputState {
    sender: mpsc::SyncSender<Vec<f32>>,
    stream: cpal::Stream,
    beep: Arc<Mutex<BeepState>>,
    ltc: Arc<Mutex<LtcStreamState>>,
}

struct AppState {
    audio: Mutex<Option<AudioOutputState>>,
}

// ── Device enumeration ─────────────────────────────────────────────────────

const PLUGIN_KEYWORDS: &[&str] = &[
    "Discard all samples",
    "Rate Converter Plugin",
    "Samplerate Library",
    "Speex Resampler",
    "JACK Audio",
    "Open Sound System",
    "PipeWire Sound Server",
    "PulseAudio Sound Server",
    "Speex DSP",
    "channel upmix",
    "channel downmix",
    "Plugin for",
];

fn is_hardware_device(name: &str) -> bool {
    !PLUGIN_KEYWORDS.iter().any(|kw| name.contains(kw))
}

#[tauri::command]
fn get_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
    let host = cpal::default_host();
    let default_device = host.default_output_device();
    let default_name = default_device.as_ref().map(|d| d.to_string());

    let mut seen = HashSet::new();
    let mut devices: Vec<AudioDeviceInfo> = Vec::new();

    if let Some(ref dev) = default_device {
        let name = dev.to_string();
        if !name.is_empty() {
            seen.insert(name.clone());
            devices.push(AudioDeviceInfo {
                id: String::from("default"),
                name: format!("{} (Default)", name),
                is_default: true,
            });
        }
    }

    for device in host
        .output_devices()
        .map_err(|e| format!("Failed to enumerate output devices: {}", e))?
    {
        let name = device.to_string();
        if name.is_empty() || !is_hardware_device(&name) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        let is_default = default_name.as_deref() == Some(&name);
        devices.push(AudioDeviceInfo {
            id: name.clone(),
            name,
            is_default,
        });
    }

    devices.sort_by(|a, b| {
        b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name))
    });

    Ok(devices)
}

// ── LTC generation (ported from ltcGenerator.ts) ───────────────────────────

fn write_val(bits: &mut [u8; 80], val: u32, start_bit: usize, length: usize) {
    for i in 0..length {
        bits[start_bit + i] = ((val >> i) & 1) as u8;
    }
}

fn get_ltc_bits(tc: &Timecode, drop_frame: bool) -> [u8; 80] {
    let mut bits = [0u8; 80];

    write_val(&mut bits, tc.frames % 10, 0, 4);
    write_val(&mut bits, 0, 4, 4);
    write_val(&mut bits, tc.frames / 10, 8, 2);
    bits[10] = if drop_frame { 1 } else { 0 };
    bits[11] = 0;
    write_val(&mut bits, 0, 12, 4);

    write_val(&mut bits, tc.seconds % 10, 16, 4);
    write_val(&mut bits, 0, 20, 4);
    write_val(&mut bits, tc.seconds / 10, 24, 2);
    bits[26] = 0;
    write_val(&mut bits, 0, 27, 5);

    write_val(&mut bits, tc.minutes % 10, 32, 4);
    write_val(&mut bits, 0, 36, 4);
    write_val(&mut bits, tc.minutes / 10, 40, 3);
    bits[43] = 0;
    write_val(&mut bits, 0, 44, 4);

    write_val(&mut bits, tc.hours % 10, 48, 4);
    write_val(&mut bits, 0, 52, 4);
    write_val(&mut bits, tc.hours / 10, 56, 2);
    bits[58] = 0;
    bits[59] = 0;
    write_val(&mut bits, 0, 60, 4);

    bits[64] = 0;
    bits[65] = 0;
    for i in 66..=77 {
        bits[i] = 1;
    }
    bits[78] = 0;
    bits[79] = 1;

    bits
}

fn increment_timecode(tc: &Timecode, fps: f64, drop_frame: bool) -> Timecode {
    let max_frames = fps.ceil() as u32;
    let mut h = tc.hours;
    let mut m = tc.minutes;
    let mut s = tc.seconds;
    let mut f = tc.frames + 1;

    if f >= max_frames {
        f = 0;
        s += 1;
        if s >= 60 {
            s = 0;
            m += 1;
            if m >= 60 {
                m = 0;
                h += 1;
                if h >= 24 {
                    h = 0;
                }
            }
            if drop_frame && m % 10 != 0 {
                f = 2;
            }
        }
    }

    Timecode {
        hours: h,
        minutes: m,
        seconds: s,
        frames: f,
    }
}

fn generate_ltc_frame_samples(
    tc: &Timecode,
    fps: f64,
    drop_frame: bool,
    sample_rate: u32,
    volume: f32,
    last_level: &mut (f32, f32),
) -> Vec<f32> {
    let frame_duration = 1.0 / fps;
    let total_samples = (sample_rate as f64 * frame_duration).round() as usize;
    let mut data = vec![0.0f32; total_samples];

    let bits = get_ltc_bits(tc, drop_frame);

    let mut raw = vec![0.0f32; total_samples];
    let mut current_level = last_level.0;

    let samples_per_bit = total_samples as f64 / 80.0;

    for b in 0..80 {
        let start_sample = (b as f64 * samples_per_bit).round() as usize;
        let end_sample = ((b + 1) as f64 * samples_per_bit).round() as usize;
        let mid_sample = ((b as f64 + 0.5) * samples_per_bit).round() as usize;
        let bit_val = bits[b];

        current_level = -current_level;

        for s in start_sample..mid_sample.min(total_samples) {
            raw[s] = current_level;
        }

        if bit_val == 1 {
            current_level = -current_level;
        }

        for s in mid_sample..end_sample.min(total_samples) {
            raw[s] = current_level;
        }
    }

    last_level.0 = current_level;

    let alpha = 0.35f32;
    let mut last_y = last_level.1;
    for i in 0..total_samples {
        last_y += alpha * (raw[i] - last_y);
        data[i] = last_y * volume;
    }

    last_level.1 = last_y;

    data
}

fn mono_to_stereo(mono: &[f32], channel: &str, volume: f32) -> Vec<f32> {
    let play_left = channel == "both" || channel == "left";
    let play_right = channel == "both" || channel == "right";
    let mut stereo = Vec::with_capacity(mono.len() * 2);

    for &val in mono {
        let v = val * volume;
        stereo.push(if play_left { v } else { 0.0 });
        stereo.push(if play_right { v } else { 0.0 });
    }

    stereo
}

fn generate_beep_samples(
    sample_rate: u32,
    frequency: f32,
    duration: f32,
    volume: f32,
    channel: &str,
) -> Vec<f32> {
    let num_samples = (sample_rate as f32 * duration) as usize;
    let attack = (sample_rate as f32 * 0.005) as usize;
    let release = (sample_rate as f32 * 0.02) as usize;
    let mut samples = Vec::with_capacity(num_samples * 2);

    let play_left = channel == "both" || channel == "left";
    let play_right = channel == "both" || channel == "right";

    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let val = (t * frequency * 2.0 * std::f32::consts::PI).sin() * volume;

        let envelope = if i < attack {
            i as f32 / attack.max(1) as f32
        } else if i > num_samples.saturating_sub(release) {
            (num_samples - i) as f32 / release.max(1) as f32
        } else {
            1.0
        };

        let sample = val * envelope;

        samples.push(if play_left { sample } else { 0.0 });
        samples.push(if play_right { sample } else { 0.0 });
    }

    samples
}

// ── LTC scheduler thread ───────────────────────────────────────────────────

fn ltc_scheduler_thread(
    sender: mpsc::SyncSender<Vec<f32>>,
    ltc: Arc<Mutex<LtcStreamState>>,
    stop_signal: Arc<AtomicBool>,
) {
    loop {
        if stop_signal.load(Ordering::Relaxed) {
            return;
        }

        let (should_run, tc, fps, drop_frame, sample_rate, ltc_channel, ltc_volume, frame_dur) = {
            let state = ltc.lock().unwrap();
            if !state.running {
                drop(state);
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let now = Instant::now();
            if now < state.next_frame_time {
                let sleep = state.next_frame_time - now;
                drop(state);
                std::thread::sleep(sleep);
                continue;
            }

            let tc = state.tc;
            let fps = state.fps;
            let drop_frame = state.drop_frame;
            let sample_rate = state.sample_rate;
            let ltc_channel = state.ltc_channel.clone();
            let ltc_volume = state.ltc_volume;
            let frame_dur = state.frame_duration;

            (true, tc, fps, drop_frame, sample_rate, ltc_channel, ltc_volume, frame_dur)
        };

        if !should_run {
            continue;
        }

        let mut last_level = {
            let state = ltc.lock().unwrap();
            state.last_level
        };

        let mono = generate_ltc_frame_samples(&tc, fps, drop_frame, sample_rate, 1.0, &mut last_level);
        let stereo = mono_to_stereo(&mono, &ltc_channel, ltc_volume);

        let _ = sender.try_send(stereo);

        {
            let mut state = ltc.lock().unwrap();
            state.last_level = last_level;
            state.tc = increment_timecode(&state.tc, fps, drop_frame);
            state.next_frame_time += frame_dur;
        }
    }
}

// ── Tauri commands ─────────────────────────────────────────────────────────

#[tauri::command]
fn init_audio_output(
    state: tauri::State<'_, AppState>,
    device_id: String,
    sample_rate: u32,
    buffer_size: u32,
) -> Result<(), String> {
    let host = cpal::default_host();
    let device = if device_id.is_empty() || device_id == "default" {
        host.default_output_device()
            .ok_or_else(|| "No default output device available".to_string())?
    } else {
        host.output_devices()
            .map_err(|e| format!("Failed to enumerate devices: {}", e))?
            .find(|d| d.to_string() == device_id)
            .ok_or_else(|| format!("Output device '{}' not found", device_id))?
    };

    let buf_size = if buffer_size > 0 {
        cpal::BufferSize::Fixed(buffer_size)
    } else {
        cpal::BufferSize::Default
    };

    let config = cpal::StreamConfig {
        channels: 2,
        sample_rate,
        buffer_size: buf_size,
    };

    let (tx, rx) = mpsc::sync_channel::<Vec<f32>>(128);
    let rx = Arc::new(Mutex::new(rx));
    let beep = Arc::new(Mutex::new(BeepState {
        samples: Vec::new(),
        index: 0,
    }));
    let ltc = Arc::new(Mutex::new(LtcStreamState {
        running: false,
        tc: Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
        fps: 25.0,
        drop_frame: false,
        ltc_channel: String::from("both"),
        ltc_volume: 0.7,
        sample_rate,
        last_level: (1.0, 1.0),
        frame_duration: Duration::from_millis(40),
        next_frame_time: Instant::now(),
        stop_signal: Arc::new(AtomicBool::new(false)),
        scheduler_thread: None,
    }));

    let mut pending_samples: Vec<f32> = Vec::new();
    let mut pending_index: usize = 0;

    let rx_clone = rx.clone();
    let beep_clone = beep.clone();
    let err_handler = move |err: cpal::Error| {
        eprintln!("Audio output stream error: {}", err);
    };

    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                for sample in data.iter_mut() {
                    if pending_index >= pending_samples.len() {
                        if let Ok(rx) = rx_clone.lock() {
                            if let Ok(samples) = rx.try_recv() {
                                pending_samples = samples;
                                pending_index = 0;
                            }
                        }
                    }

                    let ltc_val = if pending_index < pending_samples.len() {
                        let val = pending_samples[pending_index];
                        pending_index += 1;
                        val
                    } else {
                        0.0
                    };

                    let beep_val = if let Ok(mut beep) = beep_clone.lock() {
                        if beep.index < beep.samples.len() {
                            let val = beep.samples[beep.index];
                            beep.index += 1;
                            val
                        } else {
                            0.0
                        }
                    } else {
                        0.0
                    };

                    *sample = ltc_val + beep_val;
                }
            },
            err_handler,
            None,
        )
        .map_err(|e| format!("Failed to build audio output stream: {}", e))?;

    stream
        .play()
        .map_err(|e| format!("Failed to play audio output stream: {}", e))?;

    let mut audio = state.audio.lock().map_err(|e| format!("State lock error: {}", e))?;
    *audio = Some(AudioOutputState {
        sender: tx,
        stream,
        beep,
        ltc,
    });

    Ok(())
}

#[tauri::command]
fn start_ltc_stream(
    state: tauri::State<'_, AppState>,
    tc: Timecode,
    fps: f64,
    drop_frame: bool,
    ltc_channel: String,
    ltc_volume: f32,
) -> Result<(), String> {
    let audio = state
        .audio
        .lock()
        .map_err(|e| format!("State lock error: {}", e))?;
    let output = audio
        .as_ref()
        .ok_or_else(|| "Audio not initialized".to_string())?;

    let mut ltc = output
        .ltc
        .lock()
        .map_err(|e| format!("LTC state lock error: {}", e))?;

    let frame_duration_ns = (1.0 / fps * 1_000_000_000.0) as u64;
    let frame_duration = Duration::from_nanos(frame_duration_ns);
    *ltc = LtcStreamState {
        running: true,
        tc,
        fps,
        drop_frame,
        ltc_channel,
        ltc_volume,
        sample_rate: ltc.sample_rate,
        last_level: (1.0, 1.0),
        frame_duration,
        next_frame_time: Instant::now() + Duration::from_millis(100),
        stop_signal: Arc::new(AtomicBool::new(false)),
        scheduler_thread: None,
    };

    let stop_signal = ltc.stop_signal.clone();
    let sender = output.sender.clone();
    let ltc_clone = output.ltc.clone();

    let handle = std::thread::Builder::new()
        .name("ltc-scheduler".into())
        .spawn(move || {
            ltc_scheduler_thread(sender, ltc_clone, stop_signal);
        })
        .map_err(|e| format!("Failed to spawn LTC scheduler thread: {}", e))?;

    ltc.scheduler_thread = Some(handle);

    Ok(())
}

#[tauri::command]
fn stop_ltc_stream(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let audio = state
        .audio
        .lock()
        .map_err(|e| format!("State lock error: {}", e))?;
    if let Some(ref output) = *audio {
        let mut ltc = output
            .ltc
            .lock()
            .map_err(|e| format!("LTC state lock error: {}", e))?;
        ltc.running = false;
        ltc.stop_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = ltc.scheduler_thread.take() {
            let _ = handle.join();
        }
    }
    Ok(())
}

#[tauri::command]
fn reset_ltc_stream(
    state: tauri::State<'_, AppState>,
    tc: Timecode,
) -> Result<(), String> {
    let audio = state
        .audio
        .lock()
        .map_err(|e| format!("State lock error: {}", e))?;
    if let Some(ref output) = *audio {
        let mut ltc = output
            .ltc
            .lock()
            .map_err(|e| format!("LTC state lock error: {}", e))?;
        ltc.tc = tc;
        ltc.last_level = (1.0, 1.0);
        ltc.next_frame_time = Instant::now() + Duration::from_millis(50);
    }
    Ok(())
}

#[tauri::command]
fn get_current_timecode(state: tauri::State<'_, AppState>) -> Result<Timecode, String> {
    let audio = state
        .audio
        .lock()
        .map_err(|e| format!("State lock error: {}", e))?;
    if let Some(ref output) = *audio {
        let ltc = output
            .ltc
            .lock()
            .map_err(|e| format!("LTC state lock error: {}", e))?;
        Ok(ltc.tc)
    } else {
        Ok(Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        })
    }
}

#[tauri::command]
fn play_beep(
    state: tauri::State<'_, AppState>,
    sample_rate: u32,
    frequency: f32,
    duration: f32,
    volume: f32,
    channel: String,
) -> Result<(), String> {
    let audio = state
        .audio
        .lock()
        .map_err(|e| format!("State lock error: {}", e))?;
    if let Some(ref output) = *audio {
        let samples = generate_beep_samples(sample_rate, frequency, duration, volume, &channel);
        let mut beep = output
            .beep
            .lock()
            .map_err(|e| format!("Beep lock error: {}", e))?;
        beep.samples = samples;
        beep.index = 0;
    }
    Ok(())
}

#[tauri::command]
fn push_audio_samples(
    state: tauri::State<'_, AppState>,
    samples: Vec<f32>,
) -> Result<(), String> {
    let audio = state.audio.lock().map_err(|e| format!("State lock error: {}", e))?;
    if let Some(ref output) = *audio {
        let _ = output.sender.try_send(samples);
    }
    Ok(())
}

#[tauri::command]
fn stop_audio_output(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut audio = state.audio.lock().map_err(|e| format!("State lock error: {}", e))?;
    if let Some(output) = audio.take() {
        let mut ltc = output.ltc.lock().unwrap();
        ltc.running = false;
        ltc.stop_signal.store(true, Ordering::Relaxed);
        if let Some(handle) = ltc.scheduler_thread.take() {
            let _ = handle.join();
        }
        drop(output.stream);
    }
    Ok(())
}

// ── App entry point ────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState {
            audio: Mutex::new(None),
        })
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_audio_devices,
            init_audio_output,
            push_audio_samples,
            stop_audio_output,
            play_beep,
            start_ltc_stream,
            stop_ltc_stream,
            reset_ltc_stream,
            get_current_timecode,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}