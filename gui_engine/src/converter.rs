use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use log::{debug, info, warn};

// ── Channel mapping ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ChannelMap {
    num_channels: usize,
    mapping: Vec<usize>,
}

impl ChannelMap {
    pub fn identity(n: usize) -> Self {
        ChannelMap {
            num_channels: n,
            mapping: (0..n).collect(),
        }
    }

    /// Construct from a raw permutation vector.
    /// Each element `i` specifies which output channel input `i` maps to.
    /// The vector must contain every value 0..n exactly once.
    pub fn from_mapping(mapping: Vec<usize>) -> Self {
        let n = mapping.len();
        ChannelMap {
            num_channels: n,
            mapping,
        }
    }

    pub fn num_channels(&self) -> usize {
        self.num_channels
    }

    pub fn get(&self, input: usize) -> usize {
        self.mapping[input]
    }

    pub fn mapping(&self) -> &[usize] {
        &self.mapping
    }

    pub fn swap(&mut self, input_row: usize, target_output: usize) {
        if input_row >= self.num_channels || target_output >= self.num_channels {
            return;
        }
        let swapped_input = self
            .mapping
            .iter()
            .position(|&out| out == target_output)
            .unwrap_or(input_row);
        self.mapping.swap(input_row, swapped_input);
    }
}

// ── ffmpeg capabilities ──────────────────────────────────────────────────

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FfmpegCapabilities {
    pub has_ffmpeg: bool,
    pub available_encoders: BTreeSet<String>,
    pub available_formats: BTreeSet<String>,
    pub error_message: Option<String>,
}

pub fn query_ffmpeg_capabilities() -> FfmpegCapabilities {
    let version_ok = Command::new("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match version_ok {
        Ok(status) if status.success() => {}
        Ok(_) => {
            return FfmpegCapabilities {
                has_ffmpeg: false,
                available_encoders: BTreeSet::new(),
                available_formats: BTreeSet::new(),
                error_message: Some(
                    "ffmpeg found but returned a non-zero exit status".to_string(),
                ),
            };
        }
        Err(e) => {
            let msg = format!(
                "ffmpeg not found in PATH. Please install ffmpeg to use the converter. \
                 Error: {}",
                e
            );
            log::warn!("{}", msg);
            return FfmpegCapabilities {
                has_ffmpeg: false,
                available_encoders: BTreeSet::new(),
                available_formats: BTreeSet::new(),
                error_message: Some(msg),
            };
        }
    }

    let encoders = run_ffmpeg_list(&["-encoders", "-hide_banner"], |flags| {
        let f = flags.as_bytes();
        !f.is_empty() && (f[0] == b'V' || f[0] == b'A')
    });
    let formats = run_ffmpeg_list(&["-formats", "-hide_banner"], |flags| {
        flags.contains('E')
    });

    FfmpegCapabilities {
        has_ffmpeg: true,
        available_encoders: encoders,
        available_formats: formats,
        error_message: None,
    }
}

fn run_ffmpeg_list(args: &[&str], filter_fn: fn(&str) -> bool) -> BTreeSet<String> {
    let output = Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .flat_map(|line| {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('-') || trimmed.starts_with("--") {
                        return Vec::new().into_iter();
                    }
                    if trimmed.starts_with("Encoders:")
                        || trimmed.starts_with("Formats:")
                        || trimmed.starts_with("File formats:")
                    {
                        return Vec::new().into_iter();
                    }
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 2 && filter_fn(parts[0]) {
                        parts[1]
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect::<Vec<_>>()
                            .into_iter()
                    } else {
                        Vec::new().into_iter()
                    }
                })
                .collect()
        }
        _ => BTreeSet::new(),
    }
}

// ── Codec / container compatibility ──────────────────────────────────────

pub fn supported_video_encoders() -> Vec<(&'static str, &'static str)> {
    vec![
        ("prores_ks", "ProRes (Kostya) — ideal for Resolve, larger files"),
        ("libx264", "H.264 (x264) — maximum compatibility"),
        ("libx265", "H.265/HEVC (x265) — efficient, Resolve-compatible"),
        ("libsvtav1", "AV1 (SVT-AV1) — good compression, widely supported"),
        ("dnxhd", "DNxHD — broadcast codec, ideal for MXF"),
    ]
}

pub fn supported_audio_encoders() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pcm_s24le", "PCM 24-bit — uncompressed, Resolve-compatible"),
        ("pcm_s16le", "PCM 16-bit — uncompressed, smaller"),
        ("aac", "AAC — compressed, good for MP4"),
        ("libopus", "Opus — modern compressed, MKV/MOV only"),
    ]
}

pub fn supported_containers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("mov", "QuickTime MOV — ProRes native, Resolve-friendly"),
        ("mkv", "Matroska MKV — versatile, all codecs"),
        ("mp4", "MPEG-4 MP4 — universal compatibility"),
        ("mxf", "MXF (Material eXchange Format) — professional broadcast"),
    ]
}

fn container_supports_video_encoder(container: &str, encoder: &str) -> bool {
    match container {
        "mkv" => matches!(
            encoder,
            "libsvtav1" | "libx264" | "libx265" | "prores_ks" | "dnxhd"
        ),
        "mov" => matches!(
            encoder,
            "prores_ks" | "libx264" | "libx265" | "libsvtav1" | "dnxhd"
        ),
        "mp4" => matches!(encoder, "libx264" | "libx265" | "libsvtav1"),
        "mxf" => matches!(encoder, "dnxhd" | "libx264" | "libx265"),
        _ => false,
    }
}

fn container_supports_audio_encoder(container: &str, encoder: &str) -> bool {
    match container {
        "mkv" => matches!(encoder, "pcm_s24le" | "pcm_s16le" | "aac" | "libopus"),
        "mov" => matches!(
            encoder,
            "pcm_s24le" | "pcm_s16le" | "aac" | "libopus"
        ),
        "mp4" => matches!(encoder, "pcm_s24le" | "pcm_s16le" | "aac"),
        "mxf" => matches!(encoder, "pcm_s24le" | "pcm_s16le" | "aac"),
        _ => false,
    }
}

/// Returns the intersection of ffmpeg-available video encoders
/// that are also compatible with the given container.
pub fn available_video_encoders_for_container<'a>(
    container: &str,
    caps: &FfmpegCapabilities,
) -> Vec<(&'a str, &'a str)> {
    supported_video_encoders()
        .into_iter()
        .filter(|(key, _)| {
            container_supports_video_encoder(container, key)
                && caps.available_encoders.contains(*key)
        })
        .collect()
}

/// Returns the intersection of ffmpeg-available audio encoders
/// that are also compatible with the given container.
pub fn available_audio_encoders_for_container<'a>(
    container: &str,
    caps: &FfmpegCapabilities,
) -> Vec<(&'a str, &'a str)> {
    supported_audio_encoders()
        .into_iter()
        .filter(|(key, _)| {
            container_supports_audio_encoder(container, key)
                && caps.available_encoders.contains(*key)
        })
        .collect()
}

/// Returns the subset of supported containers that are available in this ffmpeg.
pub fn available_containers<'a>(caps: &FfmpegCapabilities) -> Vec<(&'a str, &'a str)> {
    supported_containers()
        .into_iter()
        .filter(|(key, _)| {
            let ffmpeg_name = container_to_ffmpeg_format(key);
            caps.available_formats.contains(ffmpeg_name)
        })
        .collect()
}

/// Select the best available (container, video_encoder, audio_encoder) combination
/// based on ffmpeg capabilities.  Priority: ProRes > DNxHD > H.264 universal > first found.
pub fn select_best_combination(caps: &FfmpegCapabilities) -> (String, String, String) {
    let preferences: &[(&str, &str, &str)] = &[
        ("mov", "prores_ks", "pcm_s24le"),
        ("mxf", "dnxhd", "pcm_s24le"),
        ("mov", "libx264", "pcm_s24le"),
        ("mkv", "libx264", "pcm_s24le"),
        ("mkv", "libx265", "aac"),
        ("mp4", "libx264", "aac"),
    ];

    for &(container, video, audio) in preferences {
        let ffmpeg_name = container_to_ffmpeg_format(container);
        if caps.available_formats.contains(ffmpeg_name)
            && caps.available_encoders.contains(video)
            && caps.available_encoders.contains(audio)
            && container_supports_video_encoder(container, video)
            && container_supports_audio_encoder(container, audio)
        {
            return (container.to_string(), video.to_string(), audio.to_string());
        }
    }

    // Absolute fallback: any compatible pair
    for (container, _) in supported_containers() {
        let ffmpeg_name = container_to_ffmpeg_format(container);
        if !caps.available_formats.contains(ffmpeg_name) {
            continue;
        }
        for (video, _) in supported_video_encoders() {
            if !caps.available_encoders.contains(video)
                || !container_supports_video_encoder(container, video)
            {
                continue;
            }
            for (audio, _) in supported_audio_encoders() {
                if caps.available_encoders.contains(audio)
                    && container_supports_audio_encoder(container, audio)
                {
                    return (
                        container.to_string(),
                        video.to_string(),
                        audio.to_string(),
                    );
                }
            }
        }
    }

    // Last resort: raw strings even if not in ffmpeg (will show error later)
    ("mkv".to_string(), "libx264".to_string(), "pcm_s24le".to_string())
}

fn encoder_available_in_ffmpeg(encoder: &str, caps: &FfmpegCapabilities) -> bool {
    caps.available_encoders.contains(encoder)
}

fn container_to_ffmpeg_format(container: &str) -> &str {
    match container {
        "mkv" => "matroska",
        _ => container,
    }
}

fn format_available_in_ffmpeg(format: &str, caps: &FfmpegCapabilities) -> bool {
    let ffmpeg_name = container_to_ffmpeg_format(format);
    caps.available_formats.contains(ffmpeg_name)
}

/// Returns `Ok(())` or an user-facing error explaining *why* the combination
/// is invalid.
pub fn conversion_sanity_check(
    container: &str,
    video_encoder: &str,
    audio_encoder: &str,
    input_files: &[PathBuf],
    output_path: &Path,
    caps: &FfmpegCapabilities,
) -> Result<(), String> {
    if !caps.has_ffmpeg {
        return Err("ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH."
            .to_string());
    }

    if input_files.is_empty() {
        return Err("No input files selected.".to_string());
    }

    for f in input_files {
        if !f.exists() {
            return Err(format!("Input file does not exist: {}", f.display()));
        }
    }

    if output_path.as_os_str().is_empty() {
        return Err("No output file path specified.".to_string());
    }

    if let Some(parent) = output_path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(format!(
                "Output directory does not exist: {}",
                parent.display()
            ));
        }
    }

    if !format_available_in_ffmpeg(container, caps) {
        return Err(format!(
            "Container format '{}' is not supported by your ffmpeg installation. \
             Run `ffmpeg -formats` to see available formats.",
            container
        ));
    }

    if !encoder_available_in_ffmpeg(video_encoder, caps) {
        return Err(format!(
            "Video encoder '{}' is not supported by your ffmpeg installation. \
             Run `ffmpeg -encoders` to see available encoders. \
             Common alternatives: prores_ks (ProRes), libx264 (H.264), libx265 (HEVC), \
             libsvtav1 (AV1), dnxhd (DNxHD).",
            video_encoder
        ));
    }

    if !encoder_available_in_ffmpeg(audio_encoder, caps) {
        return Err(format!(
            "Audio encoder '{}' is not supported by your ffmpeg installation. \
             Run `ffmpeg -encoders` to see available encoders. \
             Common alternatives: pcm_s24le (PCM 24-bit), pcm_s16le (PCM 16-bit), aac, libopus.",
            audio_encoder
        ));
    }

    if !container_supports_video_encoder(container, video_encoder) {
        return Err(format!(
            "Video encoder '{}' is not compatible with container format '{}'. \
             {}",
            video_encoder,
            container,
            match video_encoder {
                "prores_ks" => "ProRes typically requires MOV or MKV containers.",
                "libsvtav1" => "AV1 works in MKV, MP4, and MOV containers.",
                "libx264" => "H.264 works in all containers.",
                "libx265" => "HEVC works in all containers.",
                "dnxhd" => "DNxHD requires MXF, MOV, or MKV containers.",
                _ => "",
            }
        ));
    }

    if !container_supports_audio_encoder(container, audio_encoder) {
        return Err(format!(
            "Audio encoder '{}' is not compatible with container format '{}'. \
             {}",
            audio_encoder,
            container,
            match audio_encoder {
                "libopus" => "Opus is only supported in MKV and MOV containers.",
                "pcm_s24le" | "pcm_s16le" => "Uncompressed PCM works in all containers.",
                "aac" => "AAC works in all containers.",
                _ => "",
            }
        ));
    }

    Ok(())
}

// ── Conversion settings ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ConverterSettings {
    pub input_files: Vec<PathBuf>,
    pub channel_map: ChannelMap,
    pub container: String,
    pub video_encoder: String,
    pub audio_encoder: String,
    pub output_path: PathBuf,
    pub trim_start_secs: f64,
}

// ── Conversion progress / state ──────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum ConversionStatus {
    Idle,
    Running { progress: f32 },
    Completed,
    Failed { error_log: String },
}

#[derive(Clone, Debug)]
pub struct ConversionState {
    pub status: ConversionStatus,
    pub ffmpeg_output: String,
    pub current_line: String,
}

impl ConversionState {
    pub fn idle() -> Self {
        ConversionState {
            status: ConversionStatus::Idle,
            ffmpeg_output: String::new(),
            current_line: String::new(),
        }
    }
}

/// Shared progress handle used by the ffmpeg thread and read by the GUI.
pub type SharedConversionState = Arc<Mutex<ConversionState>>;

/// Atomic flag to request cancellation of a running conversion.
pub type CancelFlag = Arc<AtomicBool>;

// ── ffmpeg argument construction ─────────────────────────────────────────

pub(crate) fn build_ffmpeg_args(settings: &ConverterSettings) -> Vec<String> {
    let num_channels = settings.channel_map.num_channels;
    let mapping = settings.channel_map.mapping();
    let trim_secs = settings.trim_start_secs;

    debug!(
        "Building ffmpeg args: {} input files, {} output channels, trim_start={:.3}s",
        settings.input_files.len(),
        num_channels,
        trim_secs,
    );
    for (i, f) in settings.input_files.iter().enumerate() {
        debug!("  input[{}]: {}", i, f.display());
    }

    let mut args: Vec<String> = vec![
        "-y".to_string(),
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        "color=c=blue:s=1280x720:r=25".to_string(),
    ];

    // Audio input files (without -ss — atrim in filter complex handles trimming)
    for f in &settings.input_files {
        args.push("-i".to_string());
        args.push(f.to_string_lossy().to_string());
    }

    // Map video
    args.push("-map".to_string());
    args.push("0:v".to_string());

    // Filter complex: route each input audio to its output channel label,
    // with sample-accurate trimming via atrim when enabled
    let trim_enabled = trim_secs > 0.001;
    let mut filter_parts: Vec<String> = Vec::new();
    for (input_idx, &output_ch) in mapping.iter().enumerate().take(num_channels) {
        let idx = input_idx + 1;
        let trim_filter = if trim_enabled {
            format!("atrim=start={:.3}", trim_secs)
        } else {
            String::new()
        };
        if trim_filter.is_empty() {
            filter_parts.push(format!("[{}:a]volume=0dB[a{}]", idx, output_ch + 1));
        } else {
            filter_parts.push(format!(
                "[{}:a]{}[trimmed{}];[trimmed{}]volume=0dB[a{}]",
                idx, trim_filter, idx, idx, output_ch + 1,
            ));
        }
    }
    let filter_complex = filter_parts.join(";");
    debug!("  filter_complex: {}", filter_complex);

    args.push("-filter_complex".to_string());
    args.push(filter_complex);

    // Map filtered audio in output channel order
    for output_ch in 1..=num_channels {
        args.push("-map".to_string());
        args.push(format!("[a{}]", output_ch));
    }

    // Video encoder
    match settings.video_encoder.as_str() {
        "libsvtav1" => {
            args.push("-c:v".to_string());
            args.push("libsvtav1".to_string());
            args.push("-pix_fmt".to_string());
            args.push("yuv420p".to_string());
        }
        "prores_ks" => {
            args.push("-c:v".to_string());
            args.push("prores_ks".to_string());
            args.push("-profile:v".to_string());
            args.push("0".to_string());
            args.push("-pix_fmt".to_string());
            args.push("yuv422p10le".to_string());
        }
        "libx264" => {
            args.push("-c:v".to_string());
            args.push("libx264".to_string());
            args.push("-pix_fmt".to_string());
            args.push("yuv420p".to_string());
        }
        "libx265" => {
            args.push("-c:v".to_string());
            args.push("libx265".to_string());
            args.push("-pix_fmt".to_string());
            args.push("yuv420p".to_string());
            args.push("-tag:v".to_string());
            args.push("hvc1".to_string());
        }
        "dnxhd" => {
            args.push("-c:v".to_string());
            args.push("dnxhd".to_string());
            args.push("-pix_fmt".to_string());
            args.push("yuv422p".to_string());
            args.push("-profile:v".to_string());
            args.push("dnxhd".to_string());
            args.push("-b:v".to_string());
            args.push("36M".to_string());
        }
        _ => {
            args.push("-c:v".to_string());
            args.push(settings.video_encoder.clone());
        }
    }

    // Audio encoder
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    // Shortest: end when shortest input ends
    args.push("-shortest".to_string());

    // Machine-readable progress to stderr (uses \n line endings)
    args.push("-progress".to_string());
    args.push("pipe:2".to_string());

    // Explicit container format
    let container = container_to_ffmpeg_format(&settings.container).to_string();
    args.push("-f".to_string());
    args.push(container);

    // Output file
    args.push(settings.output_path.to_string_lossy().to_string());

    args
}

// ── Spawn conversion ─────────────────────────────────────────────────────

pub fn spawn_conversion(
    settings: ConverterSettings,
    state: SharedConversionState,
    cancel: CancelFlag,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let args = build_ffmpeg_args(&settings);
        let output = settings.output_path.clone();
        let input_count = settings.input_files.len();

        info!(
            "Starting conversion: {} input(s) → {}, trim={:.3}s, encoders={}/{}",
            input_count,
            output.display(),
            settings.trim_start_secs,
            settings.video_encoder,
            settings.audio_encoder,
        );

        {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Running { progress: 0.0 };
            s.ffmpeg_output = format!("ffmpeg \\\n  {}", args.join(" \\\n  "));
            s.current_line = String::new();
        }

        let mut child = match Command::new("ffmpeg")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let err_msg = format!("Failed to spawn ffmpeg: {}", e);
                warn!("Conversion spawn failed: {}", err_msg);
                let mut s = state.lock().unwrap();
                s.status = ConversionStatus::Failed {
                    error_log: err_msg,
                };
                return;
            }
        };

        let stderr = child.stderr.take().unwrap();
        let reader = std::io::BufReader::new(stderr);
        use std::io::BufRead;
        let mut full_log = String::new();
        let mut progress: f32 = 0.0;
        let out_time_re = regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap();
        let duration_re = regex::Regex::new(r"Duration: (\d+):(\d+):(\d+)\.(\d+)").unwrap();
        let mut total_duration_secs: Option<f64> = None;
        let mut last_logged_pct: u8 = 0;

        for line in reader.lines() {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let mut s = state.lock().unwrap();
                s.status = ConversionStatus::Failed {
                    error_log: format!("{}\n\n--- CANCELLED BY USER ---", full_log),
                };
                s.ffmpeg_output = full_log.clone();
                return;
            }

            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };

            full_log.push_str(&line);
            full_log.push('\n');

            // Keep only last ~50 lines
            let lines: Vec<&str> = full_log.lines().collect();
            let tail: String = if lines.len() > 50 {
                lines[lines.len() - 50..].join("\n")
            } else {
                full_log.clone()
            };

            // Capture total duration from ffmpeg's input metadata
            if total_duration_secs.is_none() {
                if let Some(caps) = duration_re.captures(&line) {
                    let h: f64 = caps[1].parse().unwrap_or(0.0);
                    let m: f64 = caps[2].parse().unwrap_or(0.0);
                    let s: f64 = caps[3].parse().unwrap_or(0.0);
                    let frac: f64 = caps[4].parse().unwrap_or(0.0) / 100.0;
                    if h > 0.0 || m > 0.0 || s > 0.0 || frac > 0.0 {
                        total_duration_secs = Some(h * 3600.0 + m * 60.0 + s + frac);
                        debug!("Detected total duration: {:.3}s", total_duration_secs.unwrap());
                    }
                }
            }

            // Parse current position from ffmpeg's machine-readable progress
            if let Some(caps) = out_time_re.captures(&line) {
                let h: f64 = caps[1].parse().unwrap_or(0.0);
                let m: f64 = caps[2].parse().unwrap_or(0.0);
                let s: f64 = caps[3].parse().unwrap_or(0.0);
                let frac: f64 = caps[4].parse().unwrap_or(0.0) / 1_000_000.0;
                let current_secs = h * 3600.0 + m * 60.0 + s + frac;

                if let Some(total) = total_duration_secs {
                    if total > 0.0 {
                        progress = (current_secs / total).min(1.0) as f32;
                    }
                } else {
                    // No Duration metadata available — use linear estimate from out_time
                    // as a fallback (progress will jump to 100% on completion)
                    if current_secs > 0.0 {
                        let heuristic = current_secs * 100.0;
                        progress = (current_secs / heuristic).min(1.0) as f32;
                    }
                }

                // Log progress at ~10% intervals
                let pct = (progress * 100.0) as u8;
                let bucket = (pct / 10) * 10;
                if bucket > 0 && bucket != last_logged_pct {
                    last_logged_pct = bucket;
                    debug!("Conversion progress: {}%", pct);
                }
            }

            // Detect progress=end sentinel (ffmpeg -progress signals encoding complete)
            if line.trim() == "progress=end" {
                progress = 1.0;
            }

            {
                let mut s = state.lock().unwrap();
                s.status = ConversionStatus::Running { progress };
                s.ffmpeg_output = tail.clone();
                s.current_line = line.clone();
            }
        }

        let exit_status = child.wait();

        {
            let mut s = state.lock().unwrap();
            match exit_status {
                Ok(status) if status.success() => {
                    info!("Conversion completed successfully: {}", output.display());
                    s.status = ConversionStatus::Completed;
                    s.ffmpeg_output = format!("{}\n\n--- CONVERSION COMPLETED SUCCESSFULLY ---", full_log);
                }
                Ok(status) => {
                    let code = status.code().map(|c| c.to_string()).unwrap_or("unknown".into());
                    warn!("Conversion failed (exit code {}): {}", code, output.display());
                    s.status = ConversionStatus::Failed {
                        error_log: format!(
                            "{}\n\n--- FFMPEG EXITED WITH CODE {} ---",
                            full_log, code
                        ),
                    };
                    s.ffmpeg_output = format!(
                        "{}\n\n--- FFMPEG EXITED WITH CODE {} ---",
                        full_log, code
                    );
                }
                Err(e) => {
                    warn!("Conversion error: {} — {}", e, output.display());
                    s.status = ConversionStatus::Failed {
                        error_log: format!("{}\n\n--- FFMPEG ERROR: {} ---", full_log, e),
                    };
                    s.ffmpeg_output = format!("{}\n\n--- FFMPEG ERROR: {} ---", full_log, e);
                }
            }
        }
    })
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    // ── build_ffmpeg_args tests ─────────────────────────────────────────

    fn make_settings(trim: f64) -> ConverterSettings {
        ConverterSettings {
            input_files: vec![
                PathBuf::from("/tmp/input1.wav"),
                PathBuf::from("/tmp/input2.wav"),
            ],
            channel_map: ChannelMap::identity(2),
            container: "mkv".to_string(),
            video_encoder: "libx264".to_string(),
            audio_encoder: "pcm_s24le".to_string(),
            output_path: PathBuf::from("/tmp/output.mkv"),
            trim_start_secs: trim,
        }
    }

    #[test]
    fn test_build_args_contains_progress_flag() {
        let args = build_ffmpeg_args(&make_settings(0.0));
        let pos = args.iter().position(|a| a == "-progress");
        assert!(pos.is_some(), "args should contain -progress flag");
        if let Some(p) = pos {
            assert_eq!(args.get(p + 1), Some(&"pipe:2".to_string()));
        }
    }

    #[test]
    fn test_build_args_contains_expected_structure() {
        let args = build_ffmpeg_args(&make_settings(0.0));
        assert!(args.contains(&"-y".to_string()));
        assert!(args.contains(&"-shortest".to_string()));
        assert!(args.contains(&"-filter_complex".to_string()));
        assert!(args.contains(&"-f".to_string()));
        assert!(args.contains(&"matroska".to_string()));
        assert!(args.contains(&"/tmp/output.mkv".to_string()));
        // Input files
        assert!(args.contains(&"/tmp/input1.wav".to_string()));
        assert!(args.contains(&"/tmp/input2.wav".to_string()));
    }

    #[test]
    fn test_build_args_trim_enabled() {
        let args = build_ffmpeg_args(&make_settings(1.500));
        let fc_idx = args.iter().position(|a| a == "-filter_complex").unwrap();
        let fc = &args[fc_idx + 1];
        assert!(fc.contains("atrim=start=1.500"), "filter complex should contain atrim when trim > 0");
    }

    #[test]
    fn test_build_args_trim_disabled() {
        let args = build_ffmpeg_args(&make_settings(0.0));
        let fc_idx = args.iter().position(|a| a == "-filter_complex").unwrap();
        let fc = &args[fc_idx + 1];
        assert!(!fc.contains("atrim"), "filter complex should NOT contain atrim when trim == 0");
    }

    #[test]
    fn test_build_args_channel_count() {
        let mut s = make_settings(0.0);
        s.channel_map = ChannelMap::identity(4);
        let args = build_ffmpeg_args(&s);
        // Should have -map [a1] through -map [a4]
        let maps: Vec<&String> = args.iter().filter(|a| a.starts_with("[a") && a.ends_with(']')).collect();
        assert_eq!(maps.len(), 4, "should have 4 audio output maps for 4 channels");
    }

    // ── Regex parsing tests ─────────────────────────────────────────────

    #[test]
    fn test_out_time_regex_matches() {
        let re = regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap();
        let caps = re.captures("out_time=00:01:23.456789").unwrap();
        let h: f64 = caps[1].parse::<f64>().unwrap();
        let m: f64 = caps[2].parse::<f64>().unwrap();
        let s: f64 = caps[3].parse::<f64>().unwrap();
        let frac: f64 = caps[4].parse::<f64>().unwrap() / 1_000_000.0;
        let secs = h * 3600.0 + m * 60.0 + s + frac;
        assert!((secs - 83.456789).abs() < 1e-9);
    }

    #[test]
    fn test_out_time_regex_zero() {
        let re = regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap();
        let caps = re.captures("out_time=00:00:00.000000").unwrap();
        let h: f64 = caps[1].parse::<f64>().unwrap();
        let m: f64 = caps[2].parse::<f64>().unwrap();
        let s: f64 = caps[3].parse::<f64>().unwrap();
        let frac: f64 = caps[4].parse::<f64>().unwrap() / 1_000_000.0;
        let secs = h * 3600.0 + m * 60.0 + s + frac;
        assert!((secs - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_out_time_regex_does_not_match_stderr_time() {
        let re = regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap();
        // stderr uses "time=" not "out_time=" — regex must NOT match
        assert!(re.captures("time=00:01:23.45").is_none());
    }

    #[test]
    fn test_duration_regex_matches() {
        let re = regex::Regex::new(r"Duration: (\d+):(\d+):(\d+)\.(\d+)").unwrap();
        let caps = re.captures("  Duration: 00:01:30.00, start: 0.000000, bitrate: 1411 kb/s").unwrap();
        let h: f64 = caps[1].parse::<f64>().unwrap();
        let m: f64 = caps[2].parse::<f64>().unwrap();
        let s: f64 = caps[3].parse::<f64>().unwrap();
        let frac: f64 = caps[4].parse::<f64>().unwrap() / 100.0;
        let secs = h * 3600.0 + m * 60.0 + s + frac;
        assert!((secs - 90.0).abs() < 1e-6);
    }

    #[test]
    fn test_duration_regex_zero_duration() {
        let re = regex::Regex::new(r"Duration: (\d+):(\d+):(\d+)\.(\d+)").unwrap();
        let caps = re.captures("  Duration: 00:00:00.00, start: 0.000000").unwrap();
        let h: f64 = caps[1].parse::<f64>().unwrap();
        let m: f64 = caps[2].parse::<f64>().unwrap();
        let s: f64 = caps[3].parse::<f64>().unwrap();
        let frac: f64 = caps[4].parse::<f64>().unwrap() / 100.0;
        let secs = h * 3600.0 + m * 60.0 + s + frac;
        assert!((secs - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_duration_regex_none_on_input_without_duration() {
        let re = regex::Regex::new(r"Duration: (\d+):(\d+):(\d+)\.(\d+)").unwrap();
        // lavfi-generated streams have Duration: N/A
        assert!(re.captures("  Duration: N/A, start: 0.000000").is_none());
    }

    #[test]
    fn test_progress_end_line_detected() {
        let line = "progress=end";
        assert_eq!(line.trim(), "progress=end");
    }

    #[test]
    fn test_progress_continue_not_mistaken_for_end() {
        let line = "progress=continue";
        assert_ne!(line.trim(), "progress=end");
    }

    #[test]
    fn test_non_matching_lines_do_not_trigger_out_time() {
        let re = regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap();
        assert!(re.captures("frame=  123 fps= 45").is_none());
        assert!(re.captures("size=    1024kB time=00:00:04.56").is_none());
        assert!(re.captures("").is_none());
    }

    // ── ConversionState tests ───────────────────────────────────────────

    #[test]
    fn test_conversion_state_idle_initial() {
        let s = ConversionState::idle();
        assert_eq!(s.status, ConversionStatus::Idle);
        assert!(s.ffmpeg_output.is_empty());
        assert!(s.current_line.is_empty());
    }

    // ── Container/encoder compatibility ─────────────────────────────────

    #[test]
    fn test_container_supports_video_encoder_valid() {
        assert!(container_supports_video_encoder("mkv", "libx264"));
        assert!(container_supports_video_encoder("mov", "prores_ks"));
        assert!(container_supports_video_encoder("mp4", "libx264"));
        assert!(container_supports_video_encoder("mkv", "libx265"));
        assert!(container_supports_video_encoder("mov", "dnxhd"));
        assert!(container_supports_video_encoder("mxf", "dnxhd"));
        assert!(container_supports_video_encoder("mxf", "libx264"));
        assert!(container_supports_video_encoder("mxf", "libx265"));
    }

    #[test]
    fn test_container_rejects_incompatible_video_encoder() {
        assert!(!container_supports_video_encoder("mp4", "prores_ks"));
        assert!(!container_supports_video_encoder("mp4", "dnxhd"));
        assert!(!container_supports_video_encoder("mxf", "prores_ks"));
        assert!(!container_supports_video_encoder("mxf", "libsvtav1"));
        assert!(!container_supports_video_encoder("mkv", "nonexistent"));
    }

    #[test]
    fn test_container_supports_audio_encoder_valid() {
        assert!(container_supports_audio_encoder("mkv", "pcm_s24le"));
        assert!(container_supports_audio_encoder("mov", "libopus"));
        assert!(container_supports_audio_encoder("mp4", "aac"));
        assert!(container_supports_audio_encoder("mxf", "pcm_s24le"));
        assert!(container_supports_audio_encoder("mxf", "aac"));
    }

    #[test]
    fn test_container_rejects_incompatible_audio_encoder() {
        assert!(!container_supports_audio_encoder("mp4", "libopus"));
        assert!(!container_supports_audio_encoder("mxf", "libopus"));
        assert!(!container_supports_audio_encoder("mp4", "nonexistent"));
    }

    #[test]
    fn test_container_to_ffmpeg_format() {
        assert_eq!(container_to_ffmpeg_format("mkv"), "matroska");
        assert_eq!(container_to_ffmpeg_format("mov"), "mov");
        assert_eq!(container_to_ffmpeg_format("mp4"), "mp4");
        assert_eq!(container_to_ffmpeg_format("mxf"), "mxf");
    }

    #[test]
    fn test_select_best_combination_prefers_prores() {
        let caps = FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: BTreeSet::from([
                "prores_ks".into(),
                "libx264".into(),
                "pcm_s24le".into(),
            ]),
            available_formats: BTreeSet::from(["mov".into(), "matroska".into(), "mp4".into()]),
            error_message: None,
        };
        let (c, v, a) = select_best_combination(&caps);
        assert_eq!((c.as_str(), v.as_str(), a.as_str()), ("mov", "prores_ks", "pcm_s24le"));
    }

    #[test]
    fn test_select_best_combination_falls_back_to_dnxhd() {
        let caps = FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: BTreeSet::from([
                "dnxhd".into(),
                "libx264".into(),
                "pcm_s24le".into(),
            ]),
            available_formats: BTreeSet::from(["mxf".into(), "matroska".into()]),
            error_message: None,
        };
        let (c, v, a) = select_best_combination(&caps);
        assert_eq!((c.as_str(), v.as_str(), a.as_str()), ("mxf", "dnxhd", "pcm_s24le"));
    }

    #[test]
    fn test_available_video_encoders_for_container_filters_correctly() {
        let caps = FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: BTreeSet::from([
                "libx264".into(),
                "libx265".into(),
                "pcm_s24le".into(),
            ]),
            available_formats: BTreeSet::from(["mov".into()]),
            error_message: None,
        };
        let available = available_video_encoders_for_container("mov", &caps);
        let keys: Vec<&str> = available.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"libx264"));
        assert!(keys.contains(&"libx265"));
        assert!(!keys.contains(&"prores_ks"));
    }
}