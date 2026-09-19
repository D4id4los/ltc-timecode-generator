use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

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

    let encoders = run_ffmpeg_list(&["-encoders", "-hide_banner"]);
    let formats = run_ffmpeg_list(&["-formats", "-hide_banner"]);

    FfmpegCapabilities {
        has_ffmpeg: true,
        available_encoders: encoders,
        available_formats: formats,
        error_message: None,
    }
}

fn run_ffmpeg_list(args: &[&str]) -> BTreeSet<String> {
    let output = Command::new("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .filter_map(|line| {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('-') || trimmed.starts_with("--") {
                        return None;
                    }
                    if trimmed.starts_with("Encoders:") || trimmed.starts_with("File formats:") {
                        return None;
                    }
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 2 {
                        Some(parts[1].to_string())
                    } else {
                        None
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
        ("libsvtav1", "AV1 (SVT-AV1) — good compression, widely supported"),
        ("libx264", "H.264 (x264) — maximum compatibility"),
        ("prores_ks", "ProRes (Kostya) — ideal for Resolve, larger files"),
    ]
}

pub fn supported_audio_encoders() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pcm_s24le", "PCM 24-bit — uncompressed, Resolve-compatible"),
        ("pcm_s16le", "PCM 16-bit — uncompressed, smaller"),
        ("aac", "AAC — compressed, good for MP4"),
        ("libopus", "Opus — modern compressed, MKV only"),
    ]
}

pub fn supported_containers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("mkv", "Matroska MKV — versatile, all codecs"),
        ("mov", "QuickTime MOV — ProRes native, Resolve-friendly"),
        ("mp4", "MPEG-4 MP4 — universal compatibility"),
    ]
}

fn container_supports_video_encoder(container: &str, encoder: &str) -> bool {
    match container {
        "mkv" => matches!(encoder, "libsvtav1" | "libx264" | "prores_ks"),
        "mov" => matches!(encoder, "prores_ks" | "libx264" | "libsvtav1"),
        "mp4" => matches!(encoder, "libx264" | "libsvtav1"),
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
        _ => false,
    }
}

fn encoder_available_in_ffmpeg(encoder: &str, caps: &FfmpegCapabilities) -> bool {
    caps.available_encoders.contains(encoder)
}

fn format_available_in_ffmpeg(format: &str, caps: &FfmpegCapabilities) -> bool {
    caps.available_formats.contains(format)
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
             Common alternatives: libx264 (H.264), libsvtav1 (AV1), prores_ks (ProRes).",
            video_encoder
        ));
    }

    if !encoder_available_in_ffmpeg(audio_encoder, caps) {
        return Err(format!(
            "Audio encoder '{}' is not supported by your ffmpeg installation. \
             Run `ffmpeg -encoders` to see available encoders. \
             Common alternatives: pcm_s24le (PCM 24-bit), aac, libopus.",
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
                "libsvtav1" => "AV1 works in MKV and MP4 containers.",
                "libx264" => "H.264 works in MKV, MP4, and MOV containers.",
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
                "pcm_s24le" | "pcm_s16le" => "Uncompressed PCM works in MKV and MOV containers.",
                "aac" => "AAC works in MKV, MOV, and MP4 containers.",
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

fn build_ffmpeg_args(settings: &ConverterSettings) -> Vec<String> {
    let num_channels = settings.channel_map.num_channels;
    let mapping = settings.channel_map.mapping();

    let mut args: Vec<String> = vec![
        "-y".to_string(),
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        "color=c=blue:s=1280x720:r=25".to_string(),
    ];

    // Audio input files
    for f in &settings.input_files {
        args.push("-i".to_string());
        args.push(f.to_string_lossy().to_string());
    }

    // Map video
    args.push("-map".to_string());
    args.push("0:v".to_string());

    // Filter complex: route each input audio to its output channel label
    let mut filter_parts: Vec<String> = Vec::new();
    for (input_idx, &output_ch) in mapping.iter().enumerate().take(num_channels) {
        filter_parts.push(format!(
            "[{}:a]volume=0dB[a{}]",
            input_idx + 1,
            output_ch + 1,
        ));
    }
    let filter_complex = filter_parts.join(";");

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
                let mut s = state.lock().unwrap();
                s.status = ConversionStatus::Failed {
                    error_log: format!("Failed to spawn ffmpeg: {}", e),
                };
                return;
            }
        };

        let stderr = child.stderr.take().unwrap();
        let reader = std::io::BufReader::new(stderr);
        use std::io::BufRead;
        let mut full_log = String::new();
        let mut progress: f32 = 0.0;
        let duration_re = regex::Regex::new(r"time=(\d+):(\d+):(\d+)\.(\d+)").unwrap();
        let mut estimated_duration_secs: Option<f64> = None;

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

            // Parse progress from ffmpeg stderr
            if let Some(caps) = duration_re.captures(&line) {
                let h: f64 = caps[1].parse().unwrap_or(0.0);
                let m: f64 = caps[2].parse().unwrap_or(0.0);
                let s: f64 = caps[3].parse().unwrap_or(0.0);
                let frac: f64 = caps[4].parse().unwrap_or(0.0) / 100.0;
                let current_secs = h * 3600.0 + m * 60.0 + s + frac;

                // Estimate duration from first frame
                if estimated_duration_secs.is_none() && current_secs > 0.0 {
                    estimated_duration_secs = Some(current_secs * 100.0);
                }

                if let Some(total) = estimated_duration_secs {
                    if total > 0.0 {
                        progress = (current_secs / total).min(1.0) as f32;
                    }
                } else {
                    progress = 0.0;
                }
            }

            {
                let mut s = state.lock().unwrap();
                s.status = ConversionStatus::Running { progress };
                s.ffmpeg_output = tail.clone();
                s.current_line = line.clone();
            }

            // Check for fatal error keywords (no action needed — just let ffmpeg finish)
        }

        let exit_status = child.wait();

        let mut s = state.lock().unwrap();
        match exit_status {
            Ok(status) if status.success() => {
                s.status = ConversionStatus::Completed;
                s.ffmpeg_output = format!("{}\n\n--- CONVERSION COMPLETED SUCCESSFULLY ---", full_log);
            }
            Ok(status) => {
                let code = status.code().map(|c| c.to_string()).unwrap_or("unknown".into());
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
                s.status = ConversionStatus::Failed {
                    error_log: format!("{}\n\n--- FFMPEG ERROR: {} ---", full_log, e),
                };
                s.ffmpeg_output = format!("{}\n\n--- FFMPEG ERROR: {} ---", full_log, e);
            }
        }
    })
}