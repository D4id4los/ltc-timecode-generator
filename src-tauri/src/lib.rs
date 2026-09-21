use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use audio_core::{
    list_audio_devices, ltc_decoder::LtcDetectionResult, AudioCore, AudioDeviceInfo, AudioEvent,
    Timecode,
};
use gui_engine::converter::{
    self, query_ffmpeg_capabilities, spawn_conversion, ChannelMap, ConversionPipeline,
    ConversionState, ConversionStatus, ConverterSettings, FfmpegCapabilities, RecordingType,
    TimecodeMetadata,
};
use gui_engine::file_pattern::{self, match_files_all_patterns, BUILTIN_PATTERNS, MatchedGroup};

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

// ── LTC detection commands ──────────────────────────────────────────────────

#[tauri::command]
fn detect_ltc_in_file(path: String, fps: f64, drop_frame: bool) -> Result<LtcDetectionResult, String> {
    audio_core::decode_ltc_from_wav(Path::new(&path), fps, drop_frame)
}

#[tauri::command]
fn detect_ltc_in_video(
    path: String,
    stream_index: usize,
    channel_index: usize,
    fps: f64,
    drop_frame: bool,
) -> Result<LtcDetectionResult, String> {
    let video_path = Path::new(&path);
    if !video_path.exists() {
        return Err(format!("Video file not found: {}", path));
    }

    let tmp_dir = std::env::temp_dir();
    let tmp_wav = tmp_dir.join(format!(
        "ltc_extract_{}_{}_{}.wav",
        std::process::id(),
        stream_index,
        channel_index,
    ));

    let channel_filter = format!("pan=mono|FC=c{}", channel_index);

    let output = std::process::Command::new("ffmpeg")
        .args(&[
            "-y",
            "-i", &path,
            "-map", &format!("0:a:{}", stream_index),
            "-af", &channel_filter,
            "-c:a", "pcm_s24le",
            "-f", "wav",
            &tmp_wav.to_string_lossy(),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg for audio extraction: {}", e))?;

    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp_wav);
        return Err(format!(
            "ffmpeg audio extraction failed: stream {} channel {} in {}",
            stream_index, channel_index, path
        ));
    }

    let result = audio_core::decode_ltc_from_wav(&tmp_wav, fps, drop_frame);
    let _ = std::fs::remove_file(&tmp_wav);
    result
}

#[tauri::command]
fn probe_video_audio(path: String) -> Result<gui_engine::ffprobe::VideoAudioProbe, String> {
    gui_engine::ffprobe::probe_video_audio(Path::new(&path))
}

// ── Converter commands ─────────────────────────────────────────────────────

#[derive(serde::Serialize)]
struct FileGroupInfo {
    prefix: String,
    files: Vec<String>,
    channel_count: usize,
    pattern_name: String,
    recording_type: String,
}

#[derive(serde::Serialize)]
struct ScanResult {
    groups: Vec<FileGroupInfo>,
}

#[tauri::command]
fn scan_folder_for_groups(folder_path: String, _pattern_index: usize) -> Result<ScanResult, String> {
    let path = PathBuf::from(&folder_path);
    if !path.is_dir() {
        return Err(format!("Not a directory: {}", folder_path));
    }

    let groups = match_files_all_patterns(&path);
    let result = groups
        .into_iter()
        .map(|g| {
            let file_names: Vec<String> = g
                .files
                .iter()
                .map(|f| f.file_name().and_then(|s| s.to_str()).unwrap_or("?").to_string())
                .collect();
            let rt = match g.recording_type {
                RecordingType::MultiTrackAudio => "MultiTrackAudio",
                RecordingType::VideoClipSequence => "VideoClipSequence",
            };
            FileGroupInfo {
                prefix: g.prefix,
                files: file_names,
                channel_count: g.files.len(),
                pattern_name: g.pattern_name.to_string(),
                recording_type: rt.to_string(),
            }
        })
        .collect();

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
    output_folder: String,
    filename_prefix: String,
    #[serde(default = "default_audio_suffix")]
    audio_suffix_template: String,
    #[serde(default = "default_video_suffix")]
    video_suffix_template: String,
    #[serde(default)]
    pipeline: String,
    #[serde(default)]
    recording_type: String,
    #[serde(default)]
    ltc_track_channel_index: usize,
    #[serde(default)]
    split_tracks: bool,
    #[serde(default)]
    drop_ltc_track: bool,
    #[serde(default)]
    ltc_video_source_stream: i32,
    #[serde(default)]
    ltc_video_source_channel: i32,
    #[serde(default)]
    generate_synthetic_video: bool,
    #[serde(default)]
    trim_to_first_ltc: bool,
    #[serde(default)]
    trim_offsets_secs: Vec<f64>,
    #[serde(default)]
    timecode_hours: Option<u32>,
    #[serde(default)]
    timecode_minutes: Option<u32>,
    #[serde(default)]
    timecode_seconds: Option<u32>,
    #[serde(default)]
    timecode_frames: Option<u32>,
    #[serde(default)]
    timecode_fps: Option<f64>,
    #[serde(default)]
    timecode_drop_frame: Option<bool>,
}

fn default_audio_suffix() -> String { "_audio_track{:01d}".to_string() }
fn default_video_suffix() -> String { "_video_clip{:02d}".to_string() }

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
    let channel_map = ChannelMap::from_mapping(request.channel_map);

    let pipeline = match request.recording_type.as_str() {
        "VideoClipSequence" => ConversionPipeline::VideoPassthrough,
        _ => ConversionPipeline::AudioOnly { generate_synthetic_video: request.generate_synthetic_video },
    };

    let recording_type = match request.recording_type.as_str() {
        "VideoClipSequence" => RecordingType::VideoClipSequence,
        _ => RecordingType::MultiTrackAudio,
    };

    // Build per-file timecode metadata
    let timecode_meta_per_file: Vec<Option<TimecodeMetadata>> = if let (Some(h), Some(m), Some(s), Some(f), Some(fps), Some(df)) = (
        request.timecode_hours,
        request.timecode_minutes,
        request.timecode_seconds,
        request.timecode_frames,
        request.timecode_fps,
        request.timecode_drop_frame,
    ) {
        let meta = TimecodeMetadata {
            start: Timecode { hours: h, minutes: m, seconds: s, frames: f },
            fps,
            drop_frame: df,
        };
        (0..input_files.len()).map(|_| Some(meta.clone())).collect()
    } else {
        vec![None; input_files.len()]
    };

    let trim_offsets = if request.trim_offsets_secs.is_empty() {
        vec![0.0; input_files.len()]
    } else {
        request.trim_offsets_secs
    };

    let ltc_video_source = match recording_type {
        RecordingType::VideoClipSequence if request.ltc_video_source_stream >= 0 => {
            Some((request.ltc_video_source_stream as usize, request.ltc_video_source_channel as usize))
        }
        _ => None,
    };

    let settings = ConverterSettings {
        pipeline,
        input_files,
        recording_type,
        ltc_track_channel_index: request.ltc_track_channel_index,
        channel_map,
        split_tracks: request.split_tracks,
        drop_ltc_track: request.drop_ltc_track,
        ltc_video_source,
        container: request.container,
        video_encoder: request.video_encoder,
        audio_encoder: request.audio_encoder,
        output_folder: PathBuf::from(&request.output_folder),
        filename_prefix: request.filename_prefix,
        audio_suffix_template: request.audio_suffix_template,
        video_suffix_template: request.video_suffix_template,
        trim_to_first_ltc: request.trim_to_first_ltc,
        trim_offsets_secs: trim_offsets,
        timecode_meta_per_file,
    };

    let caps = query_ffmpeg_capabilities();
    if let Err(e) = converter::conversion_sanity_check(
        &settings.container,
        &settings.video_encoder,
        &settings.audio_encoder,
        &settings.input_files,
        &settings.output_folder,
        &settings.filename_prefix,
        &caps,
        Some(&settings.audio_suffix_template),
        Some(&settings.video_suffix_template),
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
            detect_ltc_in_file,
            detect_ltc_in_video,
            probe_video_audio,
            start_convert,
            get_conversion_progress,
            cancel_conversion,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}