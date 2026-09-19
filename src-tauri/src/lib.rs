use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use audio_core::{list_audio_devices, AudioCore, AudioDeviceInfo, AudioEvent, Timecode};
use gui_engine::converter::{
    self, query_ffmpeg_capabilities, spawn_conversion, ChannelMap, ConversionState,
    ConversionStatus, ConverterSettings, FfmpegCapabilities,
};
use gui_engine::file_pattern::{self, BUILTIN_PATTERNS};

// ── State ─────────────────────────────────────────────────────────────────

struct AudioState {
    audio: Mutex<AudioCore>,
}

struct ConverterManager {
    active: Mutex<Option<ActiveConversion>>,
}

struct ActiveConversion {
    state: gui_engine::converter::SharedConversionState,
    cancel: gui_engine::converter::CancelFlag,
    _handle: JoinHandle<()>,
}

fn lock_err<E: std::fmt::Display>(e: E) -> String {
    format!("State lock error: {}", e)
}

// ── Audio commands ─────────────────────────────────────────────────────────

#[tauri::command]
fn get_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
    list_audio_devices()
}

#[tauri::command]
fn init_audio_output(
    state: tauri::State<'_, AudioState>,
    device_id: String,
    sample_rate: u32,
    buffer_size: u32,
) -> Result<u32, String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.init_output(&device_id, sample_rate, buffer_size)
}

#[tauri::command]
fn start_ltc_stream(
    state: tauri::State<'_, AudioState>,
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
fn stop_ltc_stream(state: tauri::State<'_, AudioState>) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.stop_ltc()
}

#[tauri::command]
fn reset_ltc_stream(
    state: tauri::State<'_, AudioState>,
    tc: Timecode,
) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.reset_ltc(tc)
}

#[tauri::command]
fn get_current_timecode(state: tauri::State<'_, AudioState>) -> Result<Timecode, String> {
    let core = state.audio.lock().map_err(lock_err)?;
    Ok(core.current_timecode())
}

#[tauri::command]
fn play_beep(
    state: tauri::State<'_, AudioState>,
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
    state: tauri::State<'_, AudioState>,
    samples: Vec<f32>,
) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.push_samples(samples);
    Ok(())
}

#[tauri::command]
fn stop_audio_output(state: tauri::State<'_, AudioState>) -> Result<(), String> {
    let core = state.audio.lock().map_err(lock_err)?;
    core.stop_output()
}

#[tauri::command]
fn drain_audio_events(state: tauri::State<'_, AudioState>) -> Vec<AudioEvent> {
    state.audio.lock().map(|c| c.drain_events()).unwrap_or_default()
}

#[tauri::command]
fn get_wake_lock_status(state: tauri::State<'_, AudioState>) -> bool {
    state.audio.lock().map(|c| c.wake_lock_active()).unwrap_or(false)
}

// ── Converter commands ─────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct FileGroupInfo {
    prefix: String,
    files: Vec<String>,
    channel_count: usize,
}

#[derive(serde::Serialize)]
struct ScanResult {
    groups: Vec<FileGroupInfo>,
}

#[tauri::command]
fn scan_folder_for_groups(folder_path: String, pattern_index: usize) -> Result<ScanResult, String> {
    let path = PathBuf::from(&folder_path);
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", folder_path));
    }

    let pattern = BUILTIN_PATTERNS.get(pattern_index).ok_or_else(|| {
        format!("Invalid pattern index: {}", pattern_index)
    })?;

    let groups = file_pattern::match_files_to_groups(&path, pattern);
    let mut result = Vec::new();
    for (prefix, files) in groups {
        let file_names: Vec<String> = files
            .iter()
            .map(|f| f.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string())
            .collect();
        let channel_count = files.len();
        result.push(FileGroupInfo {
            prefix,
            files: file_names,
            channel_count,
        });
    }

    Ok(ScanResult { groups: result })
}

#[tauri::command]
fn check_ffmpeg() -> FfmpegCapabilities {
    query_ffmpeg_capabilities()
}

#[derive(serde::Deserialize)]
struct ConvertRequest {
    input_files: Vec<String>,
    channel_map: Vec<usize>,
    container: String,
    video_encoder: String,
    audio_encoder: String,
    output_path: String,
}

#[derive(serde::Serialize)]
struct ConvertResponse {
    success: bool,
    message: String,
}

#[tauri::command]
fn start_convert(
    state: tauri::State<'_, ConverterManager>,
    request: ConvertRequest,
) -> Result<ConvertResponse, String> {
    let input_files: Vec<PathBuf> = request.input_files.iter().map(PathBuf::from).collect();
    let output_path = PathBuf::from(&request.output_path);
    let channel_map = ChannelMap::from_mapping(request.channel_map);

    let settings = ConverterSettings {
        input_files,
        channel_map,
        container: request.container,
        video_encoder: request.video_encoder,
        audio_encoder: request.audio_encoder,
        output_path,
    };

    let caps = query_ffmpeg_capabilities();
    if let Err(e) = converter::conversion_sanity_check(
        &settings.container,
        &settings.video_encoder,
        &settings.audio_encoder,
        &settings.input_files,
        &settings.output_path,
        &caps,
    ) {
        return Ok(ConvertResponse {
            success: false,
            message: e,
        });
    }

    let conv_state: gui_engine::converter::SharedConversionState =
        Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: gui_engine::converter::CancelFlag = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, conv_state.clone(), cancel.clone());

    let mut active = state.active.lock().map_err(lock_err)?;
    *active = Some(ActiveConversion {
        state: conv_state,
        cancel,
        _handle: handle,
    });

    Ok(ConvertResponse {
        success: true,
        message: "Conversion started".to_string(),
    })
}

#[derive(serde::Serialize)]
struct ConversionProgressInfo {
    status: String,
    progress: f32,
    log: String,
}

#[tauri::command]
fn get_conversion_progress(state: tauri::State<'_, ConverterManager>) -> ConversionProgressInfo {
    let active = state.active.lock().unwrap();
    if let Some(ref conv) = *active {
        let s = conv.state.lock().unwrap();
        let (status, progress) = match &s.status {
            ConversionStatus::Idle => ("idle".to_string(), 0.0),
            ConversionStatus::Running { progress } => ("running".to_string(), *progress),
            ConversionStatus::Completed => ("completed".to_string(), 1.0),
            ConversionStatus::Failed { .. } => ("failed".to_string(), 0.0),
        };
        ConversionProgressInfo {
            status,
            progress,
            log: s.ffmpeg_output.clone(),
        }
    } else {
        ConversionProgressInfo {
            status: "idle".to_string(),
            progress: 0.0,
            log: String::new(),
        }
    }
}

#[tauri::command]
fn cancel_conversion(state: tauri::State<'_, ConverterManager>) {
    let active = state.active.lock().unwrap();
    if let Some(ref conv) = *active {
        conv.cancel.store(true, Ordering::Relaxed);
    }
}

// ── App entry point ────────────────────────────────────────────────────────

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AudioState {
            audio: Mutex::new(AudioCore::new()),
        })
        .manage(ConverterManager {
            active: Mutex::new(None),
        })
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }
            app.handle().plugin(tauri_plugin_dialog::init())?;
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
            drain_audio_events,
            get_wake_lock_status,
            scan_folder_for_groups,
            check_ffmpeg,
            start_convert,
            get_conversion_progress,
            cancel_conversion,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}