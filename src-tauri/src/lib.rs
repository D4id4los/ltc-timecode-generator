use audio_core::{list_audio_devices, AudioCore, AudioDeviceInfo, Timecode};

struct AppState {
    audio: std::sync::Mutex<AudioCore>,
}

fn lock_err<E: std::fmt::Display>(e: E) -> String {
    format!("State lock error: {}", e)
}

// ── Tauri commands ─────────────────────────────────────────────────────────

#[tauri::command]
fn get_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
    list_audio_devices()
}

#[tauri::command]
fn init_audio_output(
    state: tauri::State<'_, AppState>,
    device_id: String,
    sample_rate: u32,
    buffer_size: u32,
) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.init_output(&device_id, sample_rate, buffer_size)
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
    let core = state.audio.lock().map_err(lock_err)?;
    core.start_ltc(tc, fps, drop_frame, ltc_channel, ltc_volume)
}

#[tauri::command]
fn stop_ltc_stream(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.stop_ltc()
}

#[tauri::command]
fn reset_ltc_stream(
    state: tauri::State<'_, AppState>,
    tc: Timecode,
) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.reset_ltc(tc)
}

#[tauri::command]
fn get_current_timecode(state: tauri::State<'_, AppState>) -> Result<Timecode, String> {
    let core = state.audio.lock().map_err(lock_err)?;
    Ok(core.current_timecode())
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
    let core = state.audio.lock().map_err(lock_err)?;
    core.play_beep(sample_rate, frequency, duration, volume, &channel)
}

#[tauri::command]
fn push_audio_samples(
    state: tauri::State<'_, AppState>,
    samples: Vec<f32>,
) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.push_samples(samples);
    Ok(())
}

#[tauri::command]
fn stop_audio_output(state: tauri::State<'_, AppState>) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.stop_output()
}

// ── App entry point ────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState {
            audio: std::sync::Mutex::new(AudioCore::new()),
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
