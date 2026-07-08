use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use serde::Serialize;
use std::collections::HashSet;
use std::sync::{Arc, Mutex, mpsc};

struct AudioOutputState {
    sender: Option<mpsc::SyncSender<Vec<f32>>>,
    stream: Option<cpal::Stream>,
}

struct AppState {
    audio: Mutex<Option<AudioOutputState>>,
}

#[derive(Serialize)]
struct AudioDeviceInfo {
    id: String,
    name: String,
    is_default: bool,
}

/// Known ALSA software plugin names to filter out from device enumeration.
/// These are not real hardware devices and should not be shown to the user.
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

    let mut pending_samples: Vec<f32> = Vec::new();
    let mut pending_index: usize = 0;

    let rx_clone = rx.clone();
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

                    if pending_index < pending_samples.len() {
                        *sample = pending_samples[pending_index];
                        pending_index += 1;
                    } else {
                        *sample = 0.0;
                    }
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
        sender: Some(tx),
        stream: Some(stream),
    });

    Ok(())
}

#[tauri::command]
fn push_audio_samples(
    state: tauri::State<'_, AppState>,
    samples: Vec<f32>,
) -> Result<(), String> {
    let audio = state.audio.lock().map_err(|e| format!("State lock error: {}", e))?;
    if let Some(ref output) = *audio {
        if let Some(ref sender) = output.sender {
            let _ = sender.try_send(samples);
        }
    }
    Ok(())
}

#[tauri::command]
fn stop_audio_output(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let mut audio = state.audio.lock().map_err(|e| format!("State lock error: {}", e))?;
    if let Some(output) = audio.take() {
        drop(output.stream);
        drop(output.sender);
    }
    Ok(())
}

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
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}