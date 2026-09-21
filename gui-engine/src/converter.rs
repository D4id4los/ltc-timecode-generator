use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use log::{info, warn};

use audio_core::{FrameTimecode, Timecode};

use crate::ffprobe::VideoAudioProbe;
use crate::video_codecs;

pub const DEFAULT_AUDIO_SUFFIX: &str = "_audio_track{:01d}";
pub const DEFAULT_VIDEO_SUFFIX: &str = "_video_clip{:02d}";

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
//
// Video encoders are selected at the *codec* level ("av1", "h265", …) via
// the registry in [`crate::video_codecs`]; concrete ffmpeg encoders are
// resolved at conversion time (hardware candidates first, software
// fallbacks last). See `video_codecs::supported_video_codecs()` for the
// dropdown source and `video_codecs::available_video_codecs()` for the
// availability-filtered variant.

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

/// Select the best available (container, video_codec, audio_encoder) combination
/// based on ffmpeg capabilities.  Priority: ProRes > DNxHD > H.264 universal > first found.
pub fn select_best_combination(caps: &FfmpegCapabilities) -> (String, String, String) {
    let preferences: &[(&str, &str, &str)] = &[
        ("mov", "prores", "pcm_s24le"),
        ("mxf", "dnxhd", "pcm_s24le"),
        ("mov", "h264", "pcm_s24le"),
        ("mkv", "h264", "pcm_s24le"),
        ("mkv", "h265", "aac"),
        ("mp4", "h264", "aac"),
    ];

    let codec_available = |codec: &str| !video_codecs::resolve_encoder_chain(codec, caps).is_empty();

    for &(container, codec, audio) in preferences {
        let ffmpeg_name = container_to_ffmpeg_format(container);
        if caps.available_formats.contains(ffmpeg_name)
            && caps.available_encoders.contains(audio)
            && container_supports_audio_encoder(container, audio)
            && video_codecs::codec_supports_container(codec, container)
            && codec_available(codec)
        {
            return (container.to_string(), codec.to_string(), audio.to_string());
        }
    }

    // Absolute fallback: any compatible pair
    for (container, _) in supported_containers() {
        let ffmpeg_name = container_to_ffmpeg_format(container);
        if !caps.available_formats.contains(ffmpeg_name) {
            continue;
        }
        for (codec, _) in video_codecs::supported_video_codecs() {
            if !video_codecs::codec_supports_container(codec, container) || !codec_available(codec)
            {
                continue;
            }
            for (audio, _) in supported_audio_encoders() {
                if caps.available_encoders.contains(audio)
                    && container_supports_audio_encoder(container, audio)
                {
                    return (
                        container.to_string(),
                        codec.to_string(),
                        audio.to_string(),
                    );
                }
            }
        }
    }

    // Last resort: raw strings even if not in ffmpeg (will show error later)
    (
        "mkv".to_string(),
        video_codecs::DEFAULT_VIDEO_CODEC.to_string(),
        "pcm_s24le".to_string(),
    )
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
///
/// The `audio_suffix` and `video_suffix` parameters are optional suffix templates
/// to validate. Pass `None` to skip suffix validation.
///
/// `video_codec` is a codec id ("av1", "h265", …); legacy concrete encoder
/// names are accepted and normalized.
pub fn conversion_sanity_check(
    container: &str,
    video_codec: &str,
    audio_encoder: &str,
    input_files: &[PathBuf],
    output_folder: &Path,
    filename_prefix: &str,
    caps: &FfmpegCapabilities,
    audio_suffix: Option<&str>,
    video_suffix: Option<&str>,
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

    if filename_prefix.is_empty() {
        return Err("No output filename prefix specified.".to_string());
    }

    if !output_folder.as_os_str().is_empty() && !output_folder.exists() {
        return Err(format!(
            "Output directory does not exist: {}",
            output_folder.display()
        ));
    }

    if !format_available_in_ffmpeg(container, caps) {
        return Err(format!(
            "Container format '{}' is not supported by your ffmpeg installation. \
             Run `ffmpeg -formats` to see available formats.",
            container
        ));
    }

    // Validate suffix templates (if provided)
    if let Some(suffix) = audio_suffix {
        ConverterSettings::validate_suffix_template(suffix)?;
    }
    if let Some(suffix) = video_suffix {
        ConverterSettings::validate_suffix_template(suffix)?;
    }

    let codec_id = video_codecs::normalize_video_codec(video_codec);
    if video_codecs::find_codec(codec_id).is_none() {
        let known: Vec<&str> = video_codecs::supported_video_codecs()
            .iter()
            .map(|(k, _)| *k)
            .collect();
        return Err(format!(
            "Unknown video codec '{}'. Supported codecs: {}.",
            video_codec,
            known.join(", ")
        ));
    }

    if video_codecs::resolve_encoder_chain(codec_id, caps).is_empty() {
        return Err(format!(
            "No {} encoder is available in your ffmpeg installation \
             (needs one of: {}). Run `ffmpeg -encoders` to see available encoders.",
            codec_id,
            video_codecs::static_encoder_chain(codec_id).join(", ")
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

    if !video_codecs::codec_supports_container(codec_id, container) {
        return Err(format!(
            "Video codec '{}' is not compatible with container format '{}'. \
             {}",
            codec_id,
            container,
            match codec_id {
                "prores" => "ProRes typically requires MOV or MKV containers.",
                "av1" => "AV1 works in MKV, MP4, and MOV containers.",
                "dnxhd" => "DNxHD requires MXF, MOV, or MKV containers.",
                "h264" | "h265" => "H.264/HEVC work in all containers.",
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

// ── Pipeline mode ────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ConversionPipeline {
    /// Input: audio files. Output: pure audio (no video).
    /// When `generate_synthetic_video` is true, wraps in synthetic video.
    AudioOnly {
        generate_synthetic_video: bool,
    },
    /// Input: video files (MP4, MTS). Output: video with original video
    /// transcoded if codec differs from source.
    VideoPassthrough,
}

// ── Recording type ──────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RecordingType {
    MultiTrackAudio,
    VideoClipSequence,
}

// ── Conversion settings ──────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ConverterSettings {
    // ── Pipeline ──
    pub pipeline: ConversionPipeline,

    // ── Input ──
    pub input_files: Vec<PathBuf>,
    pub recording_type: RecordingType,
    pub ltc_track_channel_index: usize,

    // ── Routing ──
    pub channel_map: ChannelMap,
    pub split_tracks: bool,
    pub drop_ltc_track: bool,
    /// For video pipeline: which (absolute_stream, channel) carries LTC.
    /// `None` when no video probe result is available.
    pub ltc_video_source: Option<(usize, usize)>,

    // ── Output Format ──
    pub container: String,
    /// Video *codec* id ("av1", "h265", …). Legacy concrete encoder names
    /// are accepted and normalized via `video_codecs::normalize_video_codec`.
    pub video_encoder: String,
    pub audio_encoder: String,
    /// Concrete ffmpeg encoder resolved from the codec chain at conversion
    /// time (hardware candidates first). Empty until resolved; the arg
    /// builders then prefer it over the static chain head.
    pub resolved_video_encoder: String,

    // ── Output Naming ──
    pub output_folder: PathBuf,
    pub filename_prefix: String,
    pub audio_suffix_template: String,
    pub video_suffix_template: String,

    // ── Trimming & Timecode (per file) ──
    pub trim_to_first_ltc: bool,
    pub trim_offsets_secs: Vec<f64>,
    pub timecode_meta_per_file: Vec<Option<TimecodeMetadata>>,
}

// ── Video output planner ──────────────────────────────────────────────────

/// Describes how audio should be kept in a non-split output.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioKeep {
    /// Pass all audio through unchanged.
    AllAudio,
    /// Keep all audio except the given (stream_index, channel_index) pairs.
    ChannelsExcept(Vec<(usize, usize)>),
}

/// A single step in a video-to-video conversion plan.
#[derive(Clone, Debug, PartialEq)]
pub enum VideoOutputStep {
    /// Split mode: video with no audio.
    VideoOnly { file_idx: usize, output: PathBuf },
    /// Mux mode: video with audio (possibly filtered).
    VideoMux { file_idx: usize, output: PathBuf, keep: AudioKeep },
    /// Extract a single audio channel to a separate file.
    AudioChannel { file_idx: usize, stream_idx: usize, channel_idx: usize, output: PathBuf, format: String },
}

/// Plan the output steps for a video-to-video conversion, given probe results.
///
/// Returns a flat list of steps. The caller executes each step in order.
pub fn plan_video_outputs(settings: &ConverterSettings, probe: &VideoAudioProbe) -> Vec<VideoOutputStep> {
    let ext = extension_for_container(&settings.container);
    let mut steps: Vec<VideoOutputStep> = Vec::new();

    for file_idx in 0..settings.input_files.len() {
        if settings.split_tracks {
            // Video-only step
            let video_out = settings.output_path_for_index("video", file_idx + 1, ext);
            steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out });

            // One AudioChannel per probed channel
            for stream in &probe.streams {
                for ch in 0..stream.channels {
                    let ltc_match = settings.ltc_video_source == Some((stream.stream_index, ch));
                    if settings.drop_ltc_track && ltc_match {
                        continue;
                    }
                    let (fmt, ext) = audio_encoder_to_output_format(&settings.audio_encoder);
                    let audio_idx = steps.iter().filter(|s| matches!(s, VideoOutputStep::AudioChannel { .. })).count() + 1;
                    let audio_out = settings.output_path_for_index("audio", audio_idx, ext);
                    steps.push(VideoOutputStep::AudioChannel {
                        file_idx,
                        stream_idx: stream.stream_index,
                        channel_idx: ch,
                        output: audio_out,
                        format: fmt.to_string(),
                    });
                }
            }
        } else {
            // Mux mode: one file per input
            let video_out = settings.output_path_for_index("video", file_idx + 1, ext);

            // Determine which (stream,channel) pairs to drop
            let drop_pairs: Vec<(usize, usize)> = if settings.drop_ltc_track {
                settings.ltc_video_source.into_iter().collect()
            } else {
                Vec::new()
            };

            if drop_pairs.is_empty() {
                steps.push(VideoOutputStep::VideoMux {
                    file_idx,
                    output: video_out,
                    keep: AudioKeep::AllAudio,
                });
            } else {
                // Count surviving channels across all streams
                let total_channels: usize = probe.streams.iter().map(|s| s.channels).sum();
                let dropped_count: usize = drop_pairs.iter().filter(|(s, c)| {
                    probe.streams.iter().any(|st| st.stream_index == *s && *c < st.channels)
                }).count();

                if dropped_count == total_channels {
                    // All channels dropped → video only with -an
                    steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out });
                } else {
                    steps.push(VideoOutputStep::VideoMux {
                        file_idx,
                        output: video_out,
                        keep: AudioKeep::ChannelsExcept(drop_pairs),
                    });
                }
            }
        }
    }

    steps
}

impl ConverterSettings {
    /// Concrete ffmpeg encoder to pass to `-c:v`.
    ///
    /// Prefers the runtime-resolved encoder; falls back to the codec's first
    /// static candidate. Legacy concrete encoder names in `video_encoder`
    /// map to themselves so old callers keep working unchanged.
    pub fn effective_video_encoder(&self) -> String {
        if !self.resolved_video_encoder.is_empty() {
            return self.resolved_video_encoder.clone();
        }
        let input = self.video_encoder.as_str();
        if video_codecs::is_known_encoder(input) {
            return input.to_string();
        }
        let codec = video_codecs::normalize_video_codec(input);
        video_codecs::static_encoder_chain(codec)
            .into_iter()
            .next()
            .unwrap_or_else(|| codec.to_string())
    }

    /// Build the output path for a single output file given a track/clip index
    /// and an extension derived from the container.
    pub fn output_path_for_index(&self, kind: &str, index: usize, extension: &str) -> PathBuf {
        let suffix = match kind {
            "audio" => self.audio_suffix_template
                .replace("{:01d}", &format!("{:01}", index))
                .replace("{:02d}", &format!("{:02}", index))
                .replace("{:03d}", &format!("{:03}", index)),
            "video" => self.video_suffix_template
                .replace("{:01d}", &format!("{:01}", index))
                .replace("{:02d}", &format!("{:02}", index))
                .replace("{:03d}", &format!("{:03}", index)),
            _ => String::new(),
        };
        let filename = format!("{}{}.{}", self.filename_prefix, suffix, extension);
        self.output_folder.join(filename)
    }

    /// Validate a suffix template: returns an error if the template
    /// contains no recognized placeholder but would produce duplicates.
    pub fn validate_suffix_template(template: &str) -> Result<(), String> {
        // Empty template is valid (no suffix at all)
        if template.is_empty() {
            return Ok(());
        }
        // Check for at least one recognized placeholder
        let has_placeholder = template.contains("{:01d}")
            || template.contains("{:02d}")
            || template.contains("{:03d}");
        if has_placeholder {
            return Ok(());
        }
        // If no placeholder, all output files would get the same suffix,
        // producing identical filenames for different tracks/clips.
        // This is an error when split tracks or multiple clips are expected.
        Err("Suffix template must contain at least one placeholder \
             ({:01d}, {:02d}, or {:03d}) to ensure unique filenames. \
             Examples: _audio_track{:01d} or _clip{:02d}"
            .to_string())
    }

    /// Generate all output paths for this conversion.
    ///
    /// For `VideoPassthrough` pipelines, pass the probe results to get correct
    /// per-channel audio paths.  `num_video_clips` is ignored for `VideoPassthrough`.
    pub fn all_output_paths(&self, num_audio_tracks: usize, num_video_clips: usize, extension: &str) -> Vec<PathBuf> {
        match self.pipeline {
            ConversionPipeline::VideoPassthrough => {
                // For video pipeline, paths are determined by the planner.
                let mut paths = Vec::new();
                for file_idx in 0..self.input_files.len() {
                    let input = &self.input_files[file_idx];
                    if let Ok(probe) = crate::ffprobe::probe_video_audio(input) {
                        let steps = plan_video_outputs(self, &probe);
                        for step in &steps {
                            match step {
                                VideoOutputStep::VideoOnly { output, .. }
                                | VideoOutputStep::VideoMux { output, .. }
                                | VideoOutputStep::AudioChannel { output, .. } => {
                                    paths.push(output.clone());
                                }
                            }
                        }
                    } else {
                        paths.push(self.output_path_for_index("video", file_idx + 1, extension));
                    }
                }
                paths
            }
            _ => {
                // Audio-only pipeline
                let mut paths = Vec::new();
                if self.split_tracks {
                    for i in 0..num_audio_tracks {
                        if self.drop_ltc_track && i == self.ltc_track_channel_index {
                            continue;
                        }
                        paths.push(self.output_path_for_index("audio", i + 1, extension));
                    }
                } else {
                    let filename = format!("{}.{}", self.filename_prefix, extension);
                    paths.push(self.output_folder.join(filename));
                }
                for i in 0..num_video_clips {
                    paths.push(self.output_path_for_index("video", i + 1, extension));
                }
                paths
            }
        }
    }
}

// ── Timecode metadata ────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct TimecodeMetadata {
    pub start: Timecode,
    pub fps: f64,
    pub drop_frame: bool,
}

/// Read the sample rate from a WAV file header.
/// Returns `None` if the file is not a valid WAV or can't be read.
fn read_wav_sample_rate(path: &Path) -> Option<u32> {
    let mut buf = [0u8; 28];
    let mut file = std::fs::File::open(path).ok()?;
    file.read_exact(&mut buf).ok()?;
    if &buf[0..4] != b"RIFF" || &buf[8..12] != b"WAVE" || &buf[12..16] != b"fmt " {
        return None;
    }
    Some(u32::from_le_bytes([buf[24], buf[25], buf[26], buf[27]]))
}

/// Format a Timecode as an ffmpeg `-timecode` argument string.
/// Non-drop frame: `HH:MM:SS:FF`, drop-frame: `HH:MM:SS;FF`.
pub fn format_ffmpeg_timecode(tc: &Timecode, drop_frame: bool) -> String {
    let frame_sep = if drop_frame { ";" } else { ":" };
    format!(
        "{:02}:{:02}:{:02}{}{:02}",
        tc.hours, tc.minutes, tc.seconds, frame_sep, tc.frames
    )
}

/// Binary-search `timecodes` for the `FrameTimecode` closest to `offset_secs`
/// and return its `Timecode` value.  Returns `None` if the slice is empty.
pub fn find_timecode_at_offset(
    timecodes: &[FrameTimecode],
    offset_secs: f64,
) -> Option<Timecode> {
    if timecodes.is_empty() {
        return None;
    }
    let idx = timecodes.binary_search_by(|ft| {
        ft.timecode_secs
            .partial_cmp(&offset_secs)
            .unwrap_or(std::cmp::Ordering::Greater)
    });
    let i = match idx {
        Ok(i) => i,
        Err(i) => i.min(timecodes.len() - 1),
    };
    Some(timecodes[i].timecode)
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

/// Build ffmpeg args for pure-audio-to-audio pipeline (no video stream).
/// Only used for the non-split path; split tracks use `build_split_track_args`.
fn build_audio_to_audio_args(
    settings: &ConverterSettings,
    format: &str,
    timecode_meta: Option<&TimecodeMetadata>,
    sample_rate: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".to_string()];

    // Input files, with per-file trim applied before each `-i`
    for (i, f) in settings.input_files.iter().enumerate() {
        let trim_secs = settings.trim_offsets_secs.get(i).copied().unwrap_or(0.0);
        push_input_with_trim(&mut args, f, trim_secs);
    }

    // No video
    args.push("-vn".to_string());

    // Audio encoder
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    // Timecode metadata for audio-only output
    if let Some(tc) = timecode_meta {
        push_audio_timecode_args(&mut args, tc, format, sample_rate);
    }

    push_output_trailer(&mut args, format);

    args
}

/// Build ffmpeg args for audio-to-synthetic-video pipeline.
fn build_audio_to_synthetic_video_args(settings: &ConverterSettings) -> Vec<String> {
    let num_channels = settings.channel_map.num_channels;
    let mapping = settings.channel_map.mapping();
        let trim_secs = settings.trim_offsets_secs.first().copied().unwrap_or(0.0);

        let mut args: Vec<String> = vec![
        "-y".to_string(),
        "-f".to_string(), "lavfi".to_string(),
        "-i".to_string(), "color=c=blue:s=1280x720:r=25".to_string(),
    ];

    for f in &settings.input_files {
        args.push("-i".to_string());
        args.push(f.to_string_lossy().to_string());
    }

    args.push("-map".to_string());
    args.push("0:v".to_string());

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
    args.push("-filter_complex".to_string());
    args.push(filter_complex);

    for output_ch in 1..=num_channels {
        args.push("-map".to_string());
        args.push(format!("[a{}]", output_ch));
    }

    // Video encoder
    push_video_encoder(&mut args, settings);

    // Audio encoder
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    // Timecode from first file
    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.first() {
        push_timecode_args(&mut args, tc);
    }

    args.push("-shortest".to_string());
    push_output_trailer(&mut args, container_to_ffmpeg_format(&settings.container));

    args
}

/// Build ffmpeg args for a video-only step (no audio).
fn build_video_only_args(settings: &ConverterSettings, file_idx: usize) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings.trim_offsets_secs.get(file_idx).copied().unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push("0:v".to_string());
    args.push("-an".to_string());

    push_video_encoder(&mut args, settings);

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_timecode_args(&mut args, tc);
    }

    push_output_trailer(&mut args, container_to_ffmpeg_format(&settings.container));
    args
}

/// Build ffmpeg args for video-mux step (audio kept, possibly filtered).
fn build_video_mux_args(settings: &ConverterSettings, file_idx: usize, keep: &AudioKeep, probe: &VideoAudioProbe) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings.trim_offsets_secs.get(file_idx).copied().unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push("0:v".to_string());

    match keep {
        AudioKeep::AllAudio => {
            args.push("-map".to_string());
            args.push("0:a?".to_string());
            args.push("-c:a".to_string());
            args.push(settings.audio_encoder.clone());
        }
        AudioKeep::ChannelsExcept(drop_pairs) => {
            // Build a filter_complex that drops specific (stream, channel) pairs
            let mut filter_parts: Vec<String> = Vec::new();
            let mut output_labels: Vec<String> = Vec::new();
            let mut filter_idx = 0;

            for stream in &probe.streams {
                let surviving_channels: Vec<usize> = (0..stream.channels)
                    .filter(|ch| !drop_pairs.contains(&(stream.stream_index, *ch)))
                    .collect();

                if surviving_channels.is_empty() {
                    continue;
                }

                if surviving_channels.len() == stream.channels {
                    // Keep entire stream untouched
                    output_labels.push(format!("0:{}", stream.stream_index));
                } else if surviving_channels.len() == 1 {
                    // Extract single channel with pan
                    let label = format!("a{}", filter_idx);
                    filter_idx += 1;
                    filter_parts.push(format!(
                        "[0:{}]pan=mono|FC=c{}[{}]",
                        stream.stream_index, surviving_channels[0], label
                    ));
                    output_labels.push(format!("[{}]", label));
                } else {
                    // Multiple surviving channels from one stream: need multi-channel pan
                    let ch_maps: Vec<String> = surviving_channels
                        .iter()
                        .enumerate()
                        .map(|(out_ch, in_ch)| format!("c{}={}", out_ch, in_ch))
                        .collect();
                    let label = format!("a{}", filter_idx);
                    filter_idx += 1;
                    let layout = match surviving_channels.len() {
                        1 => "mono",
                        2 => "stereo",
                        _ => "5.1",
                    };
                    let pan = format!("pan={}|{}", layout, ch_maps.join("|"));
                    filter_parts.push(format!(
                        "[0:{}]{}[{}]",
                        stream.stream_index, pan, label
                    ));
                    output_labels.push(format!("[{}]", label));
                }
            }

            if output_labels.is_empty() {
                // No audio survived: drop all
                args.push("-an".to_string());
            } else {
                if !filter_parts.is_empty() {
                    args.push("-filter_complex".to_string());
                    args.push(filter_parts.join(";"));
                }
                for label in &output_labels {
                    args.push("-map".to_string());
                    args.push(label.clone());
                }
                args.push("-c:a".to_string());
                args.push(settings.audio_encoder.clone());
            }
        }
    }

    push_video_encoder(&mut args, settings);

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_timecode_args(&mut args, tc);
    }

    push_output_trailer(&mut args, container_to_ffmpeg_format(&settings.container));
    args
}

/// Build ffmpeg args for extracting a single audio channel from a video file.
///
/// `sample_rate` should come from the probed audio stream so the BWF
/// `time_reference` is computed with the source's real rate.
fn build_video_track_extract_args(
    settings: &ConverterSettings,
    file_idx: usize,
    stream_idx: usize,
    channel_idx: usize,
    format: &str,
    sample_rate: u32,
) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings.trim_offsets_secs.get(file_idx).copied().unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push(format!("0:{}", stream_idx));
    args.push("-af".to_string());
    args.push(format!("pan=mono|FC=c{}", channel_idx));

    if format == "wav" {
        args.push("-c:a".to_string());
        args.push("pcm_s24le".to_string());
    } else if format == "adts" {
        args.push("-c:a".to_string());
        args.push("aac".to_string());
    } else {
        args.push("-c:a".to_string());
        args.push(settings.audio_encoder.clone());
    }

    // Timecode metadata — identical treatment to the audio-to-audio path.
    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_audio_timecode_args(&mut args, tc, format, sample_rate);
    }

    push_output_trailer(&mut args, format);
    args
}

/// Dispatch to the correct video arg-builder based on step kind.
fn build_video_to_video_args(settings: &ConverterSettings, step: &VideoOutputStep, probe: &VideoAudioProbe) -> Vec<String> {
    match step {
        VideoOutputStep::VideoOnly { file_idx, .. } => {
            build_video_only_args(settings, *file_idx)
        }
        VideoOutputStep::VideoMux { file_idx, keep, .. } => {
            build_video_mux_args(settings, *file_idx, keep, probe)
        }
        VideoOutputStep::AudioChannel { file_idx, stream_idx, channel_idx, format, .. } => {
            let sample_rate = probe
                .streams
                .iter()
                .find(|s| s.stream_index == *stream_idx)
                .map(|s| s.sample_rate)
                .unwrap_or(48000);
            build_video_track_extract_args(settings, *file_idx, *stream_idx, *channel_idx, format, sample_rate)
        }
    }
}

/// Push `-ss <trim>` (when significant) followed by `-i <file>`.
fn push_input_with_trim(args: &mut Vec<String>, file: &Path, trim_secs: f64) {
    if trim_secs > 0.001 {
        args.push("-ss".to_string());
        args.push(format!("{:.3}", trim_secs));
    }
    args.push("-i".to_string());
    args.push(file.to_string_lossy().to_string());
}

/// Push the standard output trailer: progress reporting and muxer format.
fn push_output_trailer(args: &mut Vec<String>, format: &str) {
    args.push("-progress".to_string());
    args.push("pipe:2".to_string());
    args.push("-f".to_string());
    args.push(format.to_string());
}

/// Push the video encoding args: `-c:v <resolved encoder>` plus codec-level
/// args (e.g. `-tag:v hvc1` for HEVC) and the resolved encoder's own args
/// (e.g. `-pix_fmt yuv420p`), from the codec registry.
fn push_video_encoder(args: &mut Vec<String>, settings: &ConverterSettings) {
    let codec_id = video_codecs::normalize_video_codec(&settings.video_encoder);
    let encoder = settings.effective_video_encoder();
    args.push("-c:v".to_string());
    args.push(encoder.clone());
    for (key, value) in video_codecs::codec_args(codec_id) {
        args.push(format!("-{}", key));
        args.push((*value).to_string());
    }
    for (key, value) in video_codecs::candidate_args(&encoder) {
        args.push(format!("-{}", key));
        args.push((*value).to_string());
    }
}

fn push_timecode_args(args: &mut Vec<String>, tc: &TimecodeMetadata) {
    let tc_str = format_ffmpeg_timecode(&tc.start, tc.drop_frame);
    args.push("-timecode".to_string());
    args.push(tc_str);
    args.push("-write_tmcd".to_string());
    args.push("1".to_string());
    args.push("-r".to_string());
    args.push(format!("{:.3}", tc.fps));
}

/// Compute the BWF `time_reference` value (sample offset since midnight)
/// for a timecode at the given sample rate.
pub fn time_reference_samples(tc: &TimecodeMetadata, sample_rate: u32) -> u64 {
    let total_secs = tc.start.hours as f64 * 3600.0
        + tc.start.minutes as f64 * 60.0
        + tc.start.seconds as f64
        + tc.start.frames as f64 / tc.fps;
    (total_secs * sample_rate as f64).round() as u64
}

/// Push timecode metadata args for an audio-only output file.
/// Single source of truth, shared by the audio-to-audio path and the
/// video-path per-channel extraction so both produce identical metadata.
/// For WAV format: uses BWF bext chunk with `time_reference` for DaVinci Resolve compatibility.
/// For other formats: uses generic `-timecode` metadata tag.
fn push_audio_timecode_args(args: &mut Vec<String>, tc: &TimecodeMetadata, format: &str, sample_rate: u32) {
    let tc_str = format_ffmpeg_timecode(&tc.start, tc.drop_frame);
    if format == "wav" {
        let time_reference = time_reference_samples(tc, sample_rate);
        args.push("-write_bext".to_string());
        args.push("1".to_string());
        args.push("-metadata".to_string());
        args.push(format!("time_reference={}", time_reference));
    }
    args.push("-timecode".to_string());
    args.push(tc_str);
}

// ── Encoder fallback ─────────────────────────────────────────────────────

/// Failure of a single ffmpeg step, classified for the encoder fallback.
#[derive(Clone, Debug, PartialEq)]
pub enum StepFailure {
    /// ffmpeg exited before producing any output — typically an encoder
    /// initialization failure (the encoder is listed by `ffmpeg -encoders`
    /// but the hardware/driver is missing). Safe to retry with the next
    /// candidate in the chain.
    EncoderInit(String),
    /// Failure after output was produced, a spawn error, or user
    /// cancellation. Not retryable with a different encoder.
    Fatal(String),
}

/// Publishes the terminal `Failed` state. Failure text is accumulated in
/// `overall_log` by `run_ffmpeg_process` before this is called.
fn mark_conversion_failed(state: &SharedConversionState, overall_log: &str) {
    let mut s = state.lock().unwrap();
    s.status = ConversionStatus::Failed {
        error_log: overall_log.to_string(),
    };
    s.ffmpeg_output = overall_log.to_string();
}

/// Tracks the concrete video encoder chain for one conversion: hardware
/// candidates first, software fallbacks last. Encoders that failed to
/// initialize are memoized so later steps skip them; the first successful
/// encoder is pinned for the rest of the run.
struct EncoderFallback {
    chain: Vec<String>,
    failed: BTreeSet<String>,
    resolved: Option<String>,
}

impl EncoderFallback {
    fn new(chain: Vec<String>) -> Self {
        EncoderFallback {
            chain,
            failed: BTreeSet::new(),
            resolved: None,
        }
    }

    /// Candidates still worth trying: the pinned encoder once one succeeded,
    /// otherwise the chain minus already-failed entries.
    fn remaining(&self) -> Vec<String> {
        if let Some(resolved) = &self.resolved {
            return vec![resolved.clone()];
        }
        self.chain
            .iter()
            .filter(|e| !self.failed.contains(*e))
            .cloned()
            .collect()
    }

    fn note_success(&mut self, encoder: &str) {
        self.resolved = Some(encoder.to_string());
    }

    fn note_failure(&mut self, encoder: &str) {
        self.failed.insert(encoder.to_string());
    }

    fn resolved(&self) -> Option<&str> {
        self.resolved.as_deref()
    }
}

/// Run one video-producing ffmpeg step, walking the encoder candidate chain
/// on encoder-initialization failures (listed but non-functional hardware
/// encoders). Returns `true` on success; on `false` the terminal `Failed`
/// state has already been published.
#[allow(clippy::too_many_arguments)]
fn run_video_step_with_fallback(
    settings: &mut ConverterSettings,
    fallback: &mut EncoderFallback,
    build_args: &mut dyn FnMut(&ConverterSettings) -> Vec<String>,
    output: &Path,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    step_progress_weight: f32,
    overall_progress: &mut f32,
    overall_log: &mut String,
    total_steps: usize,
    current_step: usize,
) -> bool {
    let candidates = fallback.remaining();
    if candidates.is_empty() {
        let msg = "no video encoder candidate available".to_string();
        warn!("{}", msg);
        overall_log.push_str(&format!("\n\n--- {} ---", msg));
        mark_conversion_failed(state, overall_log);
        return false;
    }

    let mut attempt = 0;
    while attempt < candidates.len() {
        let encoder = candidates[attempt].clone();
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        settings.resolved_video_encoder = encoder.clone();
        let args = build_args(settings);
        match run_ffmpeg_process(
            &args,
            output,
            state,
            cancel,
            step_progress_weight,
            overall_progress,
            overall_log,
            total_steps,
            current_step,
        ) {
            Ok(()) => {
                fallback.note_success(&encoder);
                return true;
            }
            Err(StepFailure::Fatal(_)) => {
                mark_conversion_failed(state, overall_log);
                return false;
            }
            Err(StepFailure::EncoderInit(_)) => {
                fallback.note_failure(&encoder);
                attempt += 1;
                if attempt < candidates.len() {
                    let msg = format!(
                        "--- encoder '{}' failed to initialize; falling back to '{}' ---",
                        encoder, candidates[attempt]
                    );
                    warn!("{} (output: {})", msg, output.display());
                    overall_log.push_str(&format!("\n--- {} ---\n", msg));
                }
            }
        }
    }

    // All candidates failed to initialize.
    let msg = format!(
        "all encoder candidates for codec '{}' failed to initialize ({})",
        video_codecs::normalize_video_codec(&settings.video_encoder),
        fallback
            .failed
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    warn!("{}", msg);
    overall_log.push_str(&format!("\n\n--- {} ---", msg));
    mark_conversion_failed(state, overall_log);
    false
}

// ── Spawn conversion ─────────────────────────────────────────────────────

/// Spawn a conversion on a background thread.
///
/// `caps` (typically probed once by the GUI) resolves the selected video
/// codec into an ordered concrete-encoder chain: hardware candidates first,
/// software fallbacks last. When no capability info is available the full
/// static chain is used and the runtime fallback sorts it out.
pub fn spawn_conversion(
    settings: ConverterSettings,
    state: SharedConversionState,
    cancel: CancelFlag,
    caps: Option<&FfmpegCapabilities>,
) -> JoinHandle<()> {
    // Resolve the codec → encoder chain on the caller thread (cheap, and
    // avoids moving the `caps` borrow into the spawned thread).
    let codec_id = video_codecs::normalize_video_codec(&settings.video_encoder).to_string();
    let mut chain: Vec<String> = caps
        .filter(|c| c.has_ffmpeg)
        .map(|c| video_codecs::resolve_encoder_chain(&codec_id, c))
        .unwrap_or_default();
    if chain.is_empty() {
        // No capability info (or stale): try the full static chain and
        // let the runtime fallback sort it out.
        chain = video_codecs::static_encoder_chain(&codec_id);
    }

    std::thread::spawn(move || {
        let mut settings = settings;
        let mut fallback = EncoderFallback::new(chain);
        info!(
            "Encoder chain for codec '{}': {}",
            codec_id,
            fallback.remaining().join(" → ")
        );

        let (output_format, output_extension) = match settings.pipeline {
            ConversionPipeline::AudioOnly { generate_synthetic_video: false } => {
                let (fmt, ext) = audio_encoder_to_output_format(&settings.audio_encoder);
                (fmt.to_string(), ext.to_string())
            }
            _ => {
                let ext = extension_for_container(&settings.container);
                (container_to_ffmpeg_format(&settings.container).to_string(), ext.to_string())
            }
        };
        let extension = &output_extension;
        let mut total_steps = match settings.pipeline {
            ConversionPipeline::VideoPassthrough => settings.input_files.len(), // placeholder, updated by run_video_to_video
            _ => if settings.split_tracks { settings.channel_map.num_channels() } else { 1 },
        };
        let mut overall_progress: f32 = 0.0;
        let mut overall_log = String::new();
        let input_count = settings.input_files.len();

        info!(
            "Starting conversion: {} input(s), pipeline={:?}, split={}, drop_ltc={}, video codec={}, audio encoder={}",
            input_count,
            settings.pipeline,
            settings.split_tracks,
            settings.drop_ltc_track,
            settings.video_encoder,
            settings.audio_encoder,
        );

        {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Running { progress: 0.0 };
            s.ffmpeg_output = format!("Pipeline: {:?}, {} steps", settings.pipeline, total_steps);
            s.current_line = String::new();
        }

        if cancel.load(Ordering::Relaxed) {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Failed { error_log: "Canceled before start".into() };
            return;
        }

        match settings.pipeline {
            ConversionPipeline::AudioOnly { generate_synthetic_video: false } => {
                run_audio_to_audio(&settings, &output_format, extension, &state, &cancel, total_steps, &mut overall_progress, &mut overall_log);
            }
            ConversionPipeline::AudioOnly { generate_synthetic_video: true } => {
                run_audio_to_synthetic_video(&mut settings, extension, &mut fallback, &state, &cancel, &mut overall_progress, &mut overall_log);
            }
            ConversionPipeline::VideoPassthrough => {
                run_video_to_video(&mut settings, extension, &mut fallback, &state, &cancel, &mut total_steps, &mut overall_progress, &mut overall_log);
            }
        }

        // Only set Completed if not already Failed (run_* functions set Failed on error)
        let final_status = {
            let s = state.lock().unwrap();
            s.status.clone()
        };
        if matches!(final_status, ConversionStatus::Failed { .. }) {
            info!("Conversion failed - see log for details.");
        } else {
            let encoder_line = fallback
                .resolved()
                .map(|e| format!("\nVideo encoder used: {}", e))
                .unwrap_or_default();
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Completed;
            s.ffmpeg_output = format!(
                "{}\n\n--- CONVERSION COMPLETED SUCCESSFULLY ---{}",
                overall_log, encoder_line
            );
        }
    })
}

fn extension_for_container(container: &str) -> &str {
    match container {
        "mov" => "mov",
        "mkv" => "mkv",
        "mp4" => "mp4",
        "mxf" => "mxf",
        _ => container,
    }
}

fn audio_encoder_to_output_format(encoder: &str) -> (&str, &str) {
    match encoder {
        "pcm_s24le" | "pcm_s16le" => ("wav", "wav"),
        "aac" => ("adts", "aac"),
        "libopus" => ("opus", "opus"),
        _ => ("wav", "wav"),
    }
}

fn run_audio_to_audio(
    settings: &ConverterSettings,
    format: &str,
    extension: &str,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    total_steps: usize,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    let sample_rate = settings
        .input_files
        .first()
        .and_then(|p| read_wav_sample_rate(p))
        .unwrap_or(48000);

    if settings.split_tracks {
        for track_idx in 0..settings.channel_map.num_channels() {
            if settings.drop_ltc_track && track_idx == settings.ltc_track_channel_index {
                continue;
            }
            if cancel.load(Ordering::Relaxed) { break; }

            let output_path = settings.output_path_for_index("audio", track_idx + 1, extension);
            let mapping = settings.channel_map.mapping();
            let input_idx = mapping.iter().position(|&o| o == track_idx).unwrap_or(track_idx);
            let tc = settings
                .timecode_meta_per_file
                .get(input_idx)
                .and_then(|m| m.as_ref());
            let step_args = build_split_track_args(settings, format, track_idx, tc, sample_rate);
            let step_progress = 1.0 / total_steps as f32;
            if run_ffmpeg_process(&step_args, &output_path, state, cancel, step_progress, overall_progress, overall_log, total_steps, 1).is_err() {
                mark_conversion_failed(state, overall_log);
                return;
            }
            *overall_progress += step_progress;
        }
    } else {
        let tc = settings
            .timecode_meta_per_file
            .first()
            .and_then(|m| m.as_ref());
        let base_args = build_audio_to_audio_args(settings, format, tc, sample_rate);
        let output_path = settings.output_path_for_index("audio", 0, extension);
        if run_ffmpeg_process(&base_args, &output_path, state, cancel, 1.0, overall_progress, overall_log, 1, 1).is_err() {
            mark_conversion_failed(state, overall_log);
        }
    }
}

fn build_split_track_args(
    settings: &ConverterSettings,
    format: &str,
    track_idx: usize,
    timecode_meta: Option<&TimecodeMetadata>,
    sample_rate: u32,
) -> Vec<String> {
    let mapping = settings.channel_map.mapping();
    let input_idx = mapping.iter().position(|&o| o == track_idx).unwrap_or(track_idx);
    let mut args: Vec<String> = vec!["-y".to_string()];
    let trim_secs = settings.trim_offsets_secs.get(input_idx).copied().unwrap_or(0.0);

    if input_idx < settings.input_files.len() {
        push_input_with_trim(&mut args, &settings.input_files[input_idx], trim_secs);
    }
    args.push("-vn".to_string());
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    if let Some(tc) = timecode_meta {
        push_audio_timecode_args(&mut args, tc, format, sample_rate);
    }

    push_output_trailer(&mut args, format);
    args
}

#[allow(clippy::too_many_arguments)]
fn run_audio_to_synthetic_video(
    settings: &mut ConverterSettings,
    extension: &str,
    fallback: &mut EncoderFallback,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    let output_path = settings.output_path_for_index("video", 1, extension);
    let mut build_args =
        |s: &ConverterSettings| build_audio_to_synthetic_video_args(s);
    run_video_step_with_fallback(
        settings,
        fallback,
        &mut build_args,
        &output_path,
        state,
        cancel,
        1.0,
        overall_progress,
        overall_log,
        1,
        1,
    );
}

#[allow(clippy::too_many_arguments)]
fn run_video_to_video(
    settings: &mut ConverterSettings,
    extension: &str,
    fallback: &mut EncoderFallback,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    _total_steps: &mut usize,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    // Phase 1: probe each file and build the step plan (encoder args are
    // built lazily per attempt so the fallback can substitute encoders).
    let mut planned: Vec<(VideoOutputStep, VideoAudioProbe)> = Vec::new();

    for file_idx in 0..settings.input_files.len() {
        if cancel.load(Ordering::Relaxed) { break; }

        let input = &settings.input_files[file_idx];
        let probe = crate::ffprobe::probe_video_audio(input);

        match probe {
            Ok(probe) => {
                for s in plan_video_outputs(settings, &probe) {
                    planned.push((s, probe.clone()));
                }
            }
            Err(e) => {
                // Probe failure: warn, treat as no-audio, produce video-only output
                warn!("Probe failed for '{}': {} — treating as no-audio", input.display(), e);
                let output_path = settings.output_path_for_index("video", file_idx + 1, extension);
                let step = VideoOutputStep::VideoOnly { file_idx, output: output_path };
                planned.push((
                    step,
                    VideoAudioProbe {
                        streams: Vec::new(),
                        total_audio_channels: 0,
                        is_video_file: true,
                    },
                ));
            }
        }
    }

    // Recalculate total steps from actual plan
    *_total_steps = planned.len();

    // Phase 2: execute steps with encoder fallback
    for (step_idx, (step, probe)) in planned.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { break; }
        let output = match step {
            VideoOutputStep::VideoOnly { output, .. }
            | VideoOutputStep::VideoMux { output, .. }
            | VideoOutputStep::AudioChannel { output, .. } => output.clone(),
        };
        let step_progress = 1.0 / planned.len().max(1) as f32;
        let mut build_args =
            |s: &ConverterSettings| build_video_to_video_args(s, step, probe);
        let ok = run_video_step_with_fallback(
            settings,
            fallback,
            &mut build_args,
            &output,
            state,
            cancel,
            step_progress,
            overall_progress,
            overall_log,
            planned.len(),
            step_idx + 1,
        );
        *overall_progress += step_progress;
        if !ok {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_ffmpeg_process(
    args: &[String],
    output: &Path,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    step_progress_weight: f32,
    overall_progress: &mut f32,
    overall_log: &mut String,
    total_steps: usize,
    current_step: usize,
) -> Result<(), StepFailure> {
    let step_label = format!("[{}/{}]", current_step, total_steps);
    info!("{} Spawning ffmpeg with {} args → {}", step_label, args.len(), output.display());

    let full_args: Vec<String> = args.iter().cloned().chain(std::iter::once(output.to_string_lossy().to_string())).collect();
    let args_str = format!("{} ffmpeg \\\n  {}", step_label, full_args.join(" \\\n  "));
    {
        let mut s = state.lock().unwrap();
        s.ffmpeg_output = args_str.clone();
        s.current_line = args_str;
    }

    let mut child = match Command::new("ffmpeg")
        .args(&full_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let err_msg = format!("{} Failed to spawn ffmpeg: {}", step_label, e);
            warn!("{}", err_msg);
            overall_log.push_str(&format!("\n\n--- {} ---", err_msg));
            return Err(StepFailure::Fatal(err_msg));
        }
    };

    let stderr = child.stderr.take().unwrap();
    let reader = std::io::BufReader::new(stderr);
    use std::io::BufRead;
    let mut local_log = String::new();
    let mut step_progress: f32 = 0.0;
    // Whether ffmpeg actually produced encoded output; distinguishes
    // encoder-init failures (nothing produced → retryable) from real
    // transcoding failures (progress was made → fatal).
    let mut produced_output = false;
    let out_time_re = regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap();
    let duration_re = regex::Regex::new(r"Duration: (\d+):(\d+):(\d+)\.(\d+)").unwrap();
    let mut total_duration_secs: Option<f64> = None;

    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            overall_log.push_str(&format!("{} --- CANCELLED ---\n", step_label));
            overall_log.push_str("\n\n--- CANCELLED BY USER ---");
            return Err(StepFailure::Fatal("cancelled by user".to_string()));
        }

        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        local_log.push_str(&line);
        local_log.push('\n');

        if total_duration_secs.is_none() {
            if let Some(caps) = duration_re.captures(&line) {
                let h: f64 = caps[1].parse().unwrap_or(0.0);
                let m: f64 = caps[2].parse().unwrap_or(0.0);
                let s: f64 = caps[3].parse().unwrap_or(0.0);
                let frac: f64 = caps[4].parse().unwrap_or(0.0) / 100.0;
                if h > 0.0 || m > 0.0 || s > 0.0 || frac > 0.0 {
                    total_duration_secs = Some(h * 3600.0 + m * 60.0 + s + frac);
                }
            }
        }

        if let Some(caps) = out_time_re.captures(&line) {
            let h: f64 = caps[1].parse().unwrap_or(0.0);
            let m: f64 = caps[2].parse().unwrap_or(0.0);
            let s: f64 = caps[3].parse().unwrap_or(0.0);
            let frac: f64 = caps[4].parse().unwrap_or(0.0) / 1_000_000.0;
            let current_secs = h * 3600.0 + m * 60.0 + s + frac;

            if current_secs > 0.0 {
                produced_output = true;
            }

            if let Some(total) = total_duration_secs {
                if total > 0.0 {
                    step_progress = (current_secs / total).min(1.0) as f32;
                }
            } else if current_secs > 0.0 {
                let heuristic = current_secs * 100.0;
                step_progress = (current_secs / heuristic).min(1.0) as f32;
            }
        }

        if line.trim() == "progress=end" {
            step_progress = 1.0;
            produced_output = true;
        }

        let combined = *overall_progress + step_progress * step_progress_weight;
        {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Running { progress: combined.min(1.0) };
            s.current_line = line.clone();
        }
    }

    let exit_status = child.wait();
    overall_log.push_str(&local_log);

    match exit_status {
        Ok(status) if status.success() => {
            // Ensure progress is reported as 1.0 even if ffmpeg completed too fast for progress tracking
            *overall_progress += step_progress_weight;
            info!("{} Step completed: {}", step_label, output.display());
            Ok(())
        }
        Ok(status) => {
            let code = status.code().map(|c| c.to_string()).unwrap_or("unknown".into());
            warn!("{} ffmpeg exited with code {}: {}", step_label, code, output.display());
            overall_log.push_str(&format!("\n\n--- FFMPEG EXITED WITH CODE {} ---", code));
            if produced_output {
                Err(StepFailure::Fatal(format!("ffmpeg exited with code {}", code)))
            } else {
                Err(StepFailure::EncoderInit(format!(
                    "ffmpeg exited with code {} before producing output",
                    code
                )))
            }
        }
        Err(e) => {
            warn!("{} ffmpeg error: {}", step_label, e);
            overall_log.push_str(&format!("\n\n--- FFMPEG ERROR: {} ---", e));
            Err(StepFailure::Fatal(e.to_string()))
        }
    }
}

// ── Converter readiness (pure, UI-agnostic gating) ──────────────────────

#[derive(Clone, Debug, PartialEq)]
pub enum ConvertBlocker {
    NoRecording,
    NoPrefix,
    NoOutputFolder,
    FfmpegNotQueried,
    FfmpegMissing(Option<String>),
}

#[derive(Clone, Debug)]
pub struct ConvertReadiness {
    pub can_convert: bool,
    pub blockers: Vec<ConvertBlocker>,
}

pub fn evaluate_readiness(
    has_group: bool,
    prefix_empty: bool,
    output_folder_empty: bool,
    caps: Option<&FfmpegCapabilities>,
) -> ConvertReadiness {
    let mut blockers = Vec::new();
    if !has_group {
        blockers.push(ConvertBlocker::NoRecording);
    }
    if prefix_empty {
        blockers.push(ConvertBlocker::NoPrefix);
    }
    if output_folder_empty {
        blockers.push(ConvertBlocker::NoOutputFolder);
    }
    match caps {
        None => blockers.push(ConvertBlocker::FfmpegNotQueried),
        Some(c) if !c.has_ffmpeg => blockers.push(ConvertBlocker::FfmpegMissing(c.error_message.clone())),
        Some(_) => {}
    }
    ConvertReadiness {
        can_convert: blockers.is_empty(),
        blockers,
    }
}

pub fn format_blockers(blockers: &[ConvertBlocker]) -> String {
    let imperatives: Vec<&str> = blockers.iter().filter_map(|b| match b {
        ConvertBlocker::NoRecording => Some("select a recording"),
        ConvertBlocker::NoPrefix => Some("set a filename prefix"),
        ConvertBlocker::NoOutputFolder => Some("choose an output folder"),
        _ => None,
    }).collect();

    let ffmpeg_messages: Vec<String> = blockers.iter().filter_map(|b| match b {
        ConvertBlocker::FfmpegNotQueried => {
            Some("ffmpeg availability is being checked…".to_string())
        }
        ConvertBlocker::FfmpegMissing(msg) => {
            let base = "ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH.";
            match msg {
                Some(detail) if !detail.is_empty() => Some(format!("{} ({})", base, detail)),
                _ => Some(base.to_string()),
            }
        }
        _ => None,
    }).collect();

    let mut parts: Vec<String> = Vec::new();
    if !imperatives.is_empty() {
        parts.push(format!("To convert, please {}.", imperatives.join(", ")));
    }
    parts.extend(ffmpeg_messages);
    parts.join(" ")
}

/// Pure version of the default-selection logic, extracted for testability.
/// Replaces container/video codec/audio_encoder with `select_best_combination`
/// defaults if the current selection is not available in `caps`.
/// `video_encoder` holds a codec id; legacy concrete encoder names are
/// normalized to their codec id first.
pub fn apply_available_defaults(
    container: &mut String,
    video_encoder: &mut String,
    audio_encoder: &mut String,
    caps: &FfmpegCapabilities,
) {
    let codec = video_codecs::normalize_video_codec(video_encoder);
    if codec != video_encoder.as_str() {
        *video_encoder = codec.to_string();
    }

    let containers: Vec<&str> = available_containers(caps).iter().map(|(k, _)| *k).collect();
    if !containers.contains(&container.as_str()) {
        let (c, v, a) = select_best_combination(caps);
        *container = c;
        *video_encoder = v;
        *audio_encoder = a;
        return;
    }
    let codecs: Vec<&str> = video_codecs::available_video_codecs(container, caps)
        .iter().map(|(k, _)| *k).collect();
    let auds: Vec<&str> =
        available_audio_encoders_for_container(container, caps).iter().map(|(k, _)| *k).collect();
    if !codecs.contains(&video_encoder.as_str()) || !auds.contains(&audio_encoder.as_str()) {
        let (c, v, a) = select_best_combination(caps);
        *container = c;
        *video_encoder = v;
        *audio_encoder = a;
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffprobe::AudioStreamInfo;
    use std::path::PathBuf;

    fn make_settings_audio_only() -> ConverterSettings {
        ConverterSettings {
            pipeline: ConversionPipeline::AudioOnly { generate_synthetic_video: false },
            input_files: vec![
                PathBuf::from("/tmp/input1.wav"),
                PathBuf::from("/tmp/input2.wav"),
            ],
            recording_type: RecordingType::MultiTrackAudio,
            ltc_track_channel_index: 1,
            channel_map: ChannelMap::identity(2),
            split_tracks: false,
            drop_ltc_track: false,
            ltc_video_source: None,
            container: "mkv".to_string(),
            video_encoder: "h264".to_string(),
            audio_encoder: "pcm_s24le".to_string(),
            resolved_video_encoder: String::new(),
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "output".to_string(),
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
            trim_to_first_ltc: false,
            trim_offsets_secs: vec![0.0; 2],
            timecode_meta_per_file: vec![None; 2],
        }
    }

    fn make_settings_synthetic_video(trim: f64) -> ConverterSettings {
        let mut s = make_settings_audio_only();
        s.pipeline = ConversionPipeline::AudioOnly { generate_synthetic_video: true };
        s.trim_offsets_secs = vec![trim; 2];
        s
    }

    fn make_caps(has_ffmpeg: bool, encoders: BTreeSet<&str>, formats: BTreeSet<&str>) -> FfmpegCapabilities {
        FfmpegCapabilities {
            has_ffmpeg,
            available_encoders: encoders.into_iter().map(String::from).collect(),
            available_formats: formats.into_iter().map(String::from).collect(),
            error_message: None,
        }
    }

    // ── evaluate_readiness / format_blockers ────────────────────────────

    #[test]
    fn test_readiness_unqueried_caps_is_not_missing() {
        let r = evaluate_readiness(true, false, false, None);
        assert!(!r.can_convert);
        assert_eq!(r.blockers, vec![ConvertBlocker::FfmpegNotQueried]);
    }

    #[test]
    fn test_readiness_ready_when_all_met() {
        let caps = make_caps(true, BTreeSet::from(["libx264"]), BTreeSet::from(["matroska"]));
        let r = evaluate_readiness(true, false, false, Some(&caps));
        assert!(r.can_convert);
        assert!(r.blockers.is_empty());
    }

    #[test]
    fn test_readiness_missing_ffmpeg() {
        let caps = make_caps(false, BTreeSet::new(), BTreeSet::new());
        let r = evaluate_readiness(true, false, false, Some(&caps));
        assert!(!r.can_convert);
        assert_eq!(r.blockers, vec![ConvertBlocker::FfmpegMissing(None)]);
    }

    #[test]
    fn test_readiness_missing_ffmpeg_with_error() {
        let caps = FfmpegCapabilities {
            has_ffmpeg: false,
            available_encoders: BTreeSet::new(),
            available_formats: BTreeSet::new(),
            error_message: Some("ffmpeg found but returned non-zero exit status".to_string()),
        };
        let r = evaluate_readiness(true, false, false, Some(&caps));
        assert_eq!(r.blockers, vec![ConvertBlocker::FfmpegMissing(Some("ffmpeg found but returned non-zero exit status".to_string()))]);
    }

    #[test]
    fn test_readiness_no_group() {
        let caps = make_caps(true, BTreeSet::from(["libx264"]), BTreeSet::from(["matroska"]));
        let r = evaluate_readiness(false, false, false, Some(&caps));
        assert!(!r.can_convert);
        assert_eq!(r.blockers, vec![ConvertBlocker::NoRecording]);
    }

    #[test]
    fn test_readiness_no_prefix() {
        let caps = make_caps(true, BTreeSet::from(["libx264"]), BTreeSet::from(["matroska"]));
        let r = evaluate_readiness(true, true, false, Some(&caps));
        assert!(!r.can_convert);
        assert_eq!(r.blockers, vec![ConvertBlocker::NoPrefix]);
    }

    #[test]
    fn test_readiness_no_output_folder() {
        let caps = make_caps(true, BTreeSet::from(["libx264"]), BTreeSet::from(["matroska"]));
        let r = evaluate_readiness(true, false, true, Some(&caps));
        assert!(!r.can_convert);
        assert_eq!(r.blockers, vec![ConvertBlocker::NoOutputFolder]);
    }

    #[test]
    fn test_readiness_multiple_blockers() {
        let r = evaluate_readiness(false, true, true, None);
        assert!(!r.can_convert);
        assert_eq!(r.blockers.len(), 4);
        assert!(r.blockers.contains(&ConvertBlocker::NoRecording));
        assert!(r.blockers.contains(&ConvertBlocker::NoPrefix));
        assert!(r.blockers.contains(&ConvertBlocker::NoOutputFolder));
        assert!(r.blockers.contains(&ConvertBlocker::FfmpegNotQueried));
    }

    #[test]
    fn test_format_blockers_imperative_only() {
        let blockers = vec![ConvertBlocker::NoRecording, ConvertBlocker::NoPrefix];
        let msg = format_blockers(&blockers);
        assert_eq!(msg, "To convert, please select a recording, set a filename prefix.");
    }

    #[test]
    fn test_format_blockers_ffmpeg_not_queried() {
        let blockers = vec![ConvertBlocker::FfmpegNotQueried];
        let msg = format_blockers(&blockers);
        assert_eq!(msg, "ffmpeg availability is being checked…");
    }

    #[test]
    fn test_format_blockers_ffmpeg_missing() {
        let blockers = vec![ConvertBlocker::FfmpegMissing(None)];
        let msg = format_blockers(&blockers);
        assert!(msg.contains("ffmpeg is not available"));
        assert!(msg.contains("install ffmpeg"));
    }

    #[test]
    fn test_format_blockers_mixed() {
        let blockers = vec![ConvertBlocker::NoRecording, ConvertBlocker::FfmpegNotQueried];
        let msg = format_blockers(&blockers);
        assert!(msg.starts_with("To convert, please select a recording."));
        assert!(msg.contains("ffmpeg availability is being checked"));
    }

    // ── apply_available_defaults ────────────────────────────────────────

    #[test]
    fn test_apply_defaults_replaces_invalid_container() {
        let caps = make_caps(true, BTreeSet::from(["prores_ks", "libx264", "pcm_s24le"]), BTreeSet::from(["mov", "matroska"]));
        let mut c = "mxf".to_string();
        let mut v = "h264".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mov");
        assert_eq!(v, "prores");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_apply_defaults_replaces_missing_encoder() {
        let caps = make_caps(true, BTreeSet::from(["libx264", "pcm_s24le"]), BTreeSet::from(["matroska"]));
        let mut c = "mkv".to_string();
        let mut v = "av1".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mkv");
        assert_eq!(v, "h264");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_apply_defaults_normalizes_legacy_encoder_names() {
        let caps = make_caps(true, BTreeSet::from(["libx264", "pcm_s24le"]), BTreeSet::from(["matroska"]));
        let mut c = "mkv".to_string();
        let mut v = "libx264".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mkv");
        assert_eq!(v, "h264", "legacy 'libx264' must normalize to codec id 'h264'");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_apply_defaults_keeps_valid_selection() {
        let caps = make_caps(true, BTreeSet::from(["prores_ks", "pcm_s24le"]), BTreeSet::from(["mov"]));
        let mut c = "mov".to_string();
        let mut v = "prores".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mov");
        assert_eq!(v, "prores");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_format_ffmpeg_timecode_non_drop() {
        let tc = Timecode { hours: 1, minutes: 23, seconds: 45, frames: 16 };
        assert_eq!(format_ffmpeg_timecode(&tc, false), "01:23:45:16");
    }

    #[test]
    fn test_format_ffmpeg_timecode_drop() {
        let tc = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 29 };
        assert_eq!(format_ffmpeg_timecode(&tc, true), "23:59:59;29");
    }

    #[test]
    fn test_format_ffmpeg_timecode_zero() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        assert_eq!(format_ffmpeg_timecode(&tc, false), "00:00:00:00");
        assert_eq!(format_ffmpeg_timecode(&tc, true), "00:00:00;00");
    }

    #[test]
    fn test_find_timecode_at_offset_exact() {
        let tcs = vec![
            FrameTimecode { frame_index: 0, timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 0.0 },
            FrameTimecode { frame_index: 25, timecode: Timecode { hours: 1, minutes: 0, seconds: 1, frames: 0 }, timecode_secs: 1.0 },
            FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 2, frames: 0 }, timecode_secs: 2.0 },
        ];
        let found = find_timecode_at_offset(&tcs, 1.0).unwrap();
        assert_eq!(found.hours, 1);
        assert_eq!(found.minutes, 0);
        assert_eq!(found.seconds, 1);
    }

    #[test]
    fn test_find_timecode_at_offset_closest() {
        let tcs = vec![
            FrameTimecode { frame_index: 0, timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 0.0 },
            FrameTimecode { frame_index: 25, timecode: Timecode { hours: 1, minutes: 0, seconds: 1, frames: 0 }, timecode_secs: 1.0 },
            FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 2, frames: 0 }, timecode_secs: 2.0 },
        ];
        // 1.9 is closer to idx 2 (2.0) than idx 1 (1.0)
        let found = find_timecode_at_offset(&tcs, 1.9).unwrap();
        assert_eq!(found.seconds, 2);
    }

    #[test]
    fn test_find_timecode_at_offset_empty() {
        assert!(find_timecode_at_offset(&[], 42.0).is_none());
    }

    #[test]
    fn test_find_timecode_at_offset_before_first() {
        let tcs = vec![
            FrameTimecode { frame_index: 0, timecode: Timecode { hours: 1, minutes: 0, seconds: 5, frames: 0 }, timecode_secs: 5.0 },
            FrameTimecode { frame_index: 1, timecode: Timecode { hours: 1, minutes: 0, seconds: 6, frames: 0 }, timecode_secs: 6.0 },
        ];
        let found = find_timecode_at_offset(&tcs, 0.0).unwrap();
        assert_eq!(found.seconds, 5);
    }

    #[test]
    fn test_build_synthetic_video_args_contains_timecode() {
        let mut s = make_settings_synthetic_video(0.0);
        s.timecode_meta_per_file[0] = Some(TimecodeMetadata {
            start: Timecode { hours: 10, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        });
        let args = build_audio_to_synthetic_video_args(&s);
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "10:00:00:00");
    }

    #[test]
    fn test_build_synthetic_video_args_drop_frame() {
        let mut s = make_settings_synthetic_video(0.0);
        s.timecode_meta_per_file[0] = Some(TimecodeMetadata {
            start: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps: 29.97,
            drop_frame: true,
        });
        let args = build_audio_to_synthetic_video_args(&s);
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "01:00:00;00");
    }

    #[test]
    fn test_build_synthetic_video_args_contains_filter_complex() {
        let args = build_audio_to_synthetic_video_args(&make_settings_synthetic_video(1.500));
        assert!(args.contains(&"-filter_complex".to_string()));
        let fc_idx = args.iter().position(|a| a == "-filter_complex").unwrap();
        let fc = &args[fc_idx + 1];
        assert!(fc.contains("atrim=start=1.500"));
    }

#[test]
    fn test_build_synthetic_video_args_channel_count() {
        let mut s = make_settings_synthetic_video(0.0);
        s.channel_map = ChannelMap::identity(4);
        let args = build_audio_to_synthetic_video_args(&s);
        let maps: Vec<&String> = args.iter().filter(|a| a.starts_with("[a") && a.ends_with(']')).collect();
        assert_eq!(maps.len(), 4, "should have 4 audio output maps for 4 channels");
    }

    #[test]
    fn test_build_audio_to_audio_args_no_timecode() {
        let s = make_settings_audio_only();
        let args = build_audio_to_audio_args(&s, "wav", None, 48000);
        assert!(!args.contains(&"-timecode".to_string()), "no -timecode when metadata absent");
        assert!(!args.contains(&"-write_bext".to_string()), "no -write_bext when metadata absent");
    }

    #[test]
    fn test_build_audio_to_audio_args_wav_timecode() {
        let s = make_settings_audio_only();
        let tc = TimecodeMetadata {
            start: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        };
        let args = build_audio_to_audio_args(&s, "wav", Some(&tc), 48000);
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "01:00:00:00");
        let bext_pos = args.iter().position(|a| a == "-write_bext").unwrap();
        assert_eq!(args[bext_pos + 1], "1");
        let ref_pos = args.iter().position(|a| a == "time_reference=172800000").unwrap_or(0);
        assert!(ref_pos > 0, "should have time_reference for 1 hour at 48kHz");
    }

    #[test]
    fn test_build_audio_to_audio_args_wav_timecode_drop_frame() {
        let s = make_settings_audio_only();
        let tc = TimecodeMetadata {
            start: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps: 29.97,
            drop_frame: true,
        };
        let args = build_audio_to_audio_args(&s, "wav", Some(&tc), 48000);
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "01:00:00;00");
        assert!(args.contains(&"-write_bext".to_string()));
    }

    #[test]
    fn test_build_audio_to_audio_args_mov_format_no_bext() {
        let s = make_settings_audio_only();
        let tc = TimecodeMetadata {
            start: Timecode { hours: 10, minutes: 30, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        };
        let args = build_audio_to_audio_args(&s, "mov", Some(&tc), 48000);
        // For MOV: -timecode should be present, but no -write_bext (BWF is WAV-only)
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "10:30:00:00");
        assert!(!args.contains(&"-write_bext".to_string()), "no bext for MOV");
    }

    #[test]
    fn test_build_split_track_args_wav_timecode() {
        let mut s = make_settings_audio_only();
        s.split_tracks = true;
        s.channel_map = ChannelMap::identity(2);
        let tc = TimecodeMetadata {
            start: Timecode { hours: 2, minutes: 15, seconds: 30, frames: 12 },
            fps: 24.0,
            drop_frame: false,
        };
        let args = build_split_track_args(&s, "wav", 0, Some(&tc), 44100);
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "02:15:30:12");
        assert!(args.contains(&"-write_bext".to_string()));
        let total_secs: f64 = 2.0 * 3600.0 + 15.0 * 60.0 + 30.0 + 12.0 / 24.0;
        let expected_ref = (total_secs * 44100.0).round() as u64;
        let ref_string = format!("time_reference={}", expected_ref);
        let ref_found = args.iter().any(|a| a.contains(&ref_string));
        assert!(ref_found, "time_reference should be calculated for 2:15:30:12 at 44.1kHz, got expected={}", expected_ref);
    }

    fn make_probe(stream_index: usize, channels: usize, sample_rate: u32) -> VideoAudioProbe {
        VideoAudioProbe {
            streams: vec![AudioStreamInfo {
                stream_index,
                channels,
                codec_name: "pcm_s24le".to_string(),
                sample_rate,
            }],
            total_audio_channels: channels,
            is_video_file: true,
        }
    }

    #[test]
    fn test_build_video_track_extract_args_wav_timecode() {
        let mut s = make_video_settings();
        s.timecode_meta_per_file[0] = Some(TimecodeMetadata {
            start: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        });
        let args = build_video_track_extract_args(&s, 0, 1, 0, "wav", 44100);
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "01:00:00:00");
        let bext_pos = args.iter().position(|a| a == "-write_bext").unwrap();
        assert_eq!(args[bext_pos + 1], "1");
        // 1 hour at the probed 44.1 kHz rate — not the old 48 kHz hardcode
        assert!(
            args.contains(&"time_reference=158760000".to_string()),
            "expected time_reference=158760000 (1h @ 44.1kHz), args: {:?}",
            args
        );
    }

    #[test]
    fn test_build_video_track_extract_args_no_timecode() {
        let s = make_video_settings();
        let args = build_video_track_extract_args(&s, 0, 1, 0, "wav", 48000);
        assert!(!args.contains(&"-timecode".to_string()), "no -timecode when metadata absent");
        assert!(!args.contains(&"-write_bext".to_string()), "no -write_bext when metadata absent");
    }

    #[test]
    fn test_build_video_track_extract_args_adts_timecode() {
        let mut s = make_video_settings();
        s.timecode_meta_per_file[0] = Some(TimecodeMetadata {
            start: Timecode { hours: 10, minutes: 30, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        });
        let args = build_video_track_extract_args(&s, 0, 1, 0, "adts", 48000);
        let tc_pos = args.iter().position(|a| a == "-timecode").unwrap();
        assert_eq!(args[tc_pos + 1], "10:30:00:00");
        assert!(!args.contains(&"-write_bext".to_string()), "no bext for non-WAV");
    }

    #[test]
    fn test_build_video_to_video_args_uses_probed_sample_rate() {
        let mut s = make_video_settings();
        s.timecode_meta_per_file[0] = Some(TimecodeMetadata {
            start: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        });
        let probe = make_probe(1, 4, 44100);
        let step = VideoOutputStep::AudioChannel {
            file_idx: 0,
            stream_idx: 1,
            channel_idx: 0,
            output: PathBuf::from("/tmp/out.wav"),
            format: "wav".to_string(),
        };
        let args = build_video_to_video_args(&s, &step, &probe);
        assert!(
            args.contains(&"time_reference=158760000".to_string()),
            "time_reference must use the probed 44.1 kHz rate, args: {:?}",
            args
        );
    }

    #[test]
    fn test_output_path_for_index_audio_01d() {
        let s = make_settings_audio_only();
        let path = s.output_path_for_index("audio", 1, "wav");
        assert_eq!(path.to_string_lossy(), "/tmp/output_audio_track1.wav");
    }

    #[test]
    fn test_output_path_for_index_audio_02d() {
        let s = make_settings_audio_only();
        let path = s.output_path_for_index("audio", 10, "wav");
        assert_eq!(path.to_string_lossy(), "/tmp/output_audio_track10.wav");
    }

    #[test]
    fn test_output_path_for_index_video_02d() {
        let s = make_settings_audio_only();
        let path = s.output_path_for_index("video", 2, "mov");
        assert_eq!(path.to_string_lossy(), "/tmp/output_video_clip02.mov");
    }

    #[test]
    fn test_all_output_paths_split_tracks() {
        let mut s = make_settings_audio_only();
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_track_channel_index = 1;
        let paths = s.all_output_paths(4, 0, "wav");
        // LTC track (index 1) should be dropped, so 3 audio paths
        assert_eq!(paths.len(), 3, "expected 3 paths (4 tracks - 1 LTC dropped)");
        assert!(paths[0].to_string_lossy().ends_with("audio_track1.wav"));
        assert!(paths[1].to_string_lossy().ends_with("audio_track3.wav"));
        assert!(paths[2].to_string_lossy().ends_with("audio_track4.wav"));
    }

    #[test]
    fn test_all_output_paths_with_video_clips() {
        let mut s = make_settings_audio_only();
        s.split_tracks = false;
        let paths = s.all_output_paths(2, 3, "mkv");
        // 1 audio file (multi-track) + 3 video clips = 4
        assert_eq!(paths.len(), 4, "expected 4 paths");
        assert!(paths[0].to_string_lossy().ends_with("output.mkv"));
        assert!(paths[1].to_string_lossy().ends_with("video_clip01.mkv"));
        assert!(paths[2].to_string_lossy().ends_with("video_clip02.mkv"));
        assert!(paths[3].to_string_lossy().ends_with("video_clip03.mkv"));
    }

    #[test]
    fn test_conversion_pipeline_audio_only_default() {
        let p = ConversionPipeline::AudioOnly { generate_synthetic_video: false };
        assert_eq!(p, ConversionPipeline::AudioOnly { generate_synthetic_video: false });
        assert_ne!(p, ConversionPipeline::AudioOnly { generate_synthetic_video: true });
    }

    #[test]
    fn test_recording_type_debug() {
        let rt = RecordingType::MultiTrackAudio;
        assert_eq!(format!("{:?}", rt), "MultiTrackAudio");
        let rt = RecordingType::VideoClipSequence;
        assert_eq!(format!("{:?}", rt), "VideoClipSequence");
    }

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

    // ── Container/codec compatibility ───────────────────────────────────

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
        assert_eq!((c.as_str(), v.as_str(), a.as_str()), ("mov", "prores", "pcm_s24le"));
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
    fn test_available_video_codecs_for_container_filters_correctly() {
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
        let available = video_codecs::available_video_codecs("mov", &caps);
        let keys: Vec<&str> = available.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"h264"));
        assert!(keys.contains(&"h265"));
        assert!(!keys.contains(&"prores"), "prores_ks not installed → codec hidden");
    }

    // ── Encoder fallback tracking ────────────────────────────────────────

    #[test]
    fn test_encoder_fallback_remaining_skips_failed() {
        let mut fb = EncoderFallback::new(vec!["av1_nvenc".into(), "libsvtav1".into(), "libaom-av1".into()]);
        assert_eq!(fb.remaining(), vec!["av1_nvenc", "libsvtav1", "libaom-av1"]);
        fb.note_failure("av1_nvenc");
        assert_eq!(fb.remaining(), vec!["libsvtav1", "libaom-av1"]);
        fb.note_failure("libsvtav1");
        assert_eq!(fb.remaining(), vec!["libaom-av1"]);
    }

    #[test]
    fn test_encoder_fallback_pins_resolved_encoder() {
        let mut fb = EncoderFallback::new(vec!["av1_nvenc".into(), "libsvtav1".into()]);
        fb.note_success("av1_nvenc");
        assert_eq!(fb.remaining(), vec!["av1_nvenc"], "pinned encoder is reused");
        assert_eq!(fb.resolved(), Some("av1_nvenc"));
    }

    #[test]
    fn test_encoder_fallback_exhausted_chain() {
        let mut fb = EncoderFallback::new(vec!["av1_nvenc".into()]);
        fb.note_failure("av1_nvenc");
        assert!(fb.remaining().is_empty());
        assert_eq!(fb.resolved(), None);
    }

    // ── effective_video_encoder / push_video_encoder ─────────────────────

    #[test]
    fn test_effective_video_encoder_prefers_resolved() {
        let mut s = make_video_settings();
        s.video_encoder = "av1".to_string();
        s.resolved_video_encoder = "av1_nvenc".to_string();
        assert_eq!(s.effective_video_encoder(), "av1_nvenc");
    }

    #[test]
    fn test_effective_video_encoder_codec_uses_static_head() {
        let mut s = make_video_settings();
        s.video_encoder = "h265".to_string();
        assert_eq!(s.effective_video_encoder(), "hevc_nvenc");
    }

    #[test]
    fn test_effective_video_encoder_legacy_name_passthrough() {
        let mut s = make_video_settings();
        s.video_encoder = "libx264".to_string();
        assert_eq!(s.effective_video_encoder(), "libx264");
    }

    #[test]
    fn test_push_video_encoder_codec_and_candidate_args() {
        let mut s = make_video_settings();
        s.video_encoder = "h265".to_string();
        s.resolved_video_encoder = "libx265".to_string();
        let mut args = Vec::new();
        push_video_encoder(&mut args, &s);
        let cv = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv + 1], "libx265");
        assert!(args.contains(&"-pix_fmt".to_string()));
        assert!(args.contains(&"yuv420p".to_string()));
        let tag = args.iter().position(|a| a == "-tag:v").unwrap();
        assert_eq!(args[tag + 1], "hvc1", "HEVC codec-level arg applies regardless of encoder");
    }

    #[test]
    fn test_push_video_encoder_hardware_candidate() {
        let mut s = make_video_settings();
        s.video_encoder = "av1".to_string();
        s.resolved_video_encoder = "av1_nvenc".to_string();
        let mut args = Vec::new();
        push_video_encoder(&mut args, &s);
        let cv = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv + 1], "av1_nvenc");
        assert!(!args.contains(&"-pix_fmt".to_string()), "HW candidates negotiate pix_fmt themselves");
    }

    #[test]
    fn test_push_video_encoder_prores_args() {
        let mut s = make_video_settings();
        s.video_encoder = "prores".to_string();
        s.resolved_video_encoder = "prores_ks".to_string();
        s.container = "mov".to_string();
        let mut args = Vec::new();
        push_video_encoder(&mut args, &s);
        let cv = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv + 1], "prores_ks");
        let prof = args.iter().position(|a| a == "-profile:v").unwrap();
        assert_eq!(args[prof + 1], "0");
        assert!(args.contains(&"yuv422p10le".to_string()));
    }

    // ── sanity check (codec-based) ───────────────────────────────────────

    #[test]
    fn test_sanity_check_no_encoder_for_codec() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("in.wav");
        std::fs::write(&input, b"RIFF").unwrap();
        let caps = make_caps(true, BTreeSet::from(["libx264", "pcm_s24le"]), BTreeSet::from(["matroska"]));
        let err = conversion_sanity_check(
            "mkv", "av1", "pcm_s24le",
            &[input], tmp.path(), "out", &caps, None, None,
        ).unwrap_err();
        assert!(err.contains("No av1 encoder is available"), "got: {}", err);
        assert!(err.contains("av1_nvenc"), "error should name expected encoders");
    }

    #[test]
    fn test_sanity_check_codec_container_mismatch() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("in.wav");
        std::fs::write(&input, b"RIFF").unwrap();
        let caps = make_caps(true, BTreeSet::from(["libsvtav1", "pcm_s24le"]), BTreeSet::from(["mxf", "matroska"]));
        let err = conversion_sanity_check(
            "mxf", "av1", "pcm_s24le",
            &[input], tmp.path(), "out", &caps, None, None,
        ).unwrap_err();
        assert!(err.contains("not compatible with container"), "got: {}", err);
    }

    #[test]
    fn test_sanity_check_legacy_encoder_name_ok() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("in.wav");
        std::fs::write(&input, b"RIFF").unwrap();
        let caps = make_caps(true, BTreeSet::from(["libx264", "pcm_s24le"]), BTreeSet::from(["matroska"]));
        assert!(conversion_sanity_check(
            "mkv", "libx264", "pcm_s24le",
            &[input], tmp.path(), "out", &caps, None, None,
        ).is_ok(), "legacy concrete encoder names must keep passing");
    }

    // ── Video pipeline helpers ────────────────────────────────────────────

    fn make_video_settings() -> ConverterSettings {
        ConverterSettings {
            pipeline: ConversionPipeline::VideoPassthrough,
            input_files: vec![PathBuf::from("/tmp/test.mp4")],
            recording_type: RecordingType::VideoClipSequence,
            ltc_track_channel_index: 0,
            channel_map: ChannelMap::identity(1),
            split_tracks: false,
            drop_ltc_track: false,
            ltc_video_source: None,
            container: "mkv".to_string(),
            video_encoder: "h264".to_string(),
            audio_encoder: "pcm_s24le".to_string(),
            resolved_video_encoder: String::new(),
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "output".to_string(),
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
            trim_to_first_ltc: false,
            trim_offsets_secs: vec![0.0],
            timecode_meta_per_file: vec![None],
        }
    }

    fn make_stereo_probe() -> VideoAudioProbe {
        VideoAudioProbe {
            streams: vec![
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 1,
                    channels: 2,
                    codec_name: "aac".to_string(),
                    sample_rate: 48000,
                },
            ],
            total_audio_channels: 2,
            is_video_file: true,
        }
    }

    fn make_mono_probe() -> VideoAudioProbe {
        VideoAudioProbe {
            streams: vec![
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 1,
                    channels: 1,
                    codec_name: "aac".to_string(),
                    sample_rate: 48000,
                },
            ],
            total_audio_channels: 1,
            is_video_file: true,
        }
    }

    fn make_multi_stream_probe() -> VideoAudioProbe {
        VideoAudioProbe {
            streams: vec![
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 1,
                    channels: 2,
                    codec_name: "aac".to_string(),
                    sample_rate: 48000,
                },
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 2,
                    channels: 1,
                    codec_name: "pcm_s16le".to_string(),
                    sample_rate: 48000,
                },
            ],
            total_audio_channels: 3,
            is_video_file: true,
        }
    }

    // ── planner tests ─────────────────────────────────────────────────────

    #[test]
    fn test_plan_split_drop_stereo_ltc_at_1_0() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        let probe = make_stereo_probe();
        let steps = plan_video_outputs(&s, &probe);
        // VideoOnly + AudioChannel for ch1 (ch0 dropped)
        assert_eq!(steps.len(), 2);
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
        assert!(matches!(steps[1], VideoOutputStep::AudioChannel { channel_idx: 1, .. }));
    }

    #[test]
    fn test_plan_split_no_drop_stereo() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.drop_ltc_track = false;
        let probe = make_stereo_probe();
        let steps = plan_video_outputs(&s, &probe);
        // VideoOnly + AudioChannel ch0 + AudioChannel ch1
        assert_eq!(steps.len(), 3);
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
        assert!(matches!(steps[1], VideoOutputStep::AudioChannel { channel_idx: 0, .. }));
        assert!(matches!(steps[2], VideoOutputStep::AudioChannel { channel_idx: 1, .. }));
    }

    #[test]
    fn test_plan_split_drop_mono_ltc_at_1_0() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        let probe = make_mono_probe();
        let steps = plan_video_outputs(&s, &probe);
        // VideoOnly only (the only channel is dropped)
        assert_eq!(steps.len(), 1);
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
    }

    #[test]
    fn test_plan_multi_stream_global_channel_numbering() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((2, 0)); // drop stream 2 ch 0 (the mono stream)
        let probe = make_multi_stream_probe();
        let steps = plan_video_outputs(&s, &probe);
        // VideoOnly + AudioChannel(stream=1,ch=0) + AudioChannel(stream=1,ch=1) = 3
        assert_eq!(steps.len(), 3);
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
        assert!(matches!(steps[1], VideoOutputStep::AudioChannel { stream_idx: 1, channel_idx: 0, .. }));
        assert!(matches!(steps[2], VideoOutputStep::AudioChannel { stream_idx: 1, channel_idx: 1, .. }));
    }

    #[test]
    fn test_plan_no_split_no_drop() {
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let steps = plan_video_outputs(&s, &probe);
        assert_eq!(steps.len(), 1);
        assert!(matches!(steps[0], VideoOutputStep::VideoMux { keep: AudioKeep::AllAudio, .. }));
    }

    #[test]
    fn test_plan_no_split_drop_stereo() {
        let mut s = make_video_settings();
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        let probe = make_stereo_probe();
        let steps = plan_video_outputs(&s, &probe);
        assert_eq!(steps.len(), 1);
        match &steps[0] {
            VideoOutputStep::VideoMux { keep: AudioKeep::ChannelsExcept(pairs), .. } => {
                assert_eq!(pairs.len(), 1);
                assert_eq!(pairs[0], (1, 0));
            }
            other => panic!("expected VideoMux(ChannelsExcept), got {:?}", other),
        }
    }

    #[test]
    fn test_plan_no_split_drop_mono() {
        let mut s = make_video_settings();
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        let probe = make_mono_probe();
        let steps = plan_video_outputs(&s, &probe);
        assert_eq!(steps.len(), 1);
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
    }

    // ── arg builder tests ─────────────────────────────────────────────────

    #[test]
    fn test_build_video_only_has_map_v_and_an() {
        let s = make_video_settings();
        let args = build_video_only_args(&s, 0);
        assert!(args.contains(&"-map".to_string()));
        let map_pos = args.iter().position(|a| a == "-map").unwrap();
        assert_eq!(args[map_pos + 1], "0:v");
        assert!(args.contains(&"-an".to_string()), "video-only must have -an");
        assert!(!args.contains(&"-c:a".to_string()), "no -c:a in video-only");
        assert!(!args.contains(&"0:a?".to_string()), "no 0:a? in video-only");
    }

    #[test]
    fn test_build_video_mux_no_drop_has_audio_map() {
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let args = build_video_mux_args(&s, 0, &AudioKeep::AllAudio, &probe);
        assert!(args.contains(&"-map".to_string()));
        let map_v_pos = args.iter().position(|a| a == "0:v").expect("expected 0:v map");
        assert!(map_v_pos > 0);
        assert!(args.contains(&"0:a?".to_string()), "mux must map audio");
        assert!(args.contains(&"-c:a".to_string()), "mux must have -c:a");
    }

    #[test]
    fn test_build_video_mux_drop_stereo_has_pan_filter() {
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let keep = AudioKeep::ChannelsExcept(vec![(1, 0)]);
        let args = build_video_mux_args(&s, 0, &keep, &probe);
        // Should have -filter_complex with pan=mono|FC=c1
        let fc_pos = args.iter().position(|a| a == "-filter_complex");
        assert!(fc_pos.is_some(), "expected -filter_complex for dropped channel: {:?}", args);
        let fc = &args[fc_pos.unwrap() + 1];
        assert!(fc.contains("pan=mono|FC=c1"), "filter should keep channel 1, got: {}", fc);
        assert!(fc.contains("[0:1]"), "filter should reference stream 1");
        // Should have -map for filtered label
        assert!(args.contains(&"[a0]".to_string()), "should map filtered output");
    }

    #[test]
    fn test_build_video_track_extract_args_wav() {
        let s = make_video_settings();
        let args = build_video_track_extract_args(&s, 0, 1, 0, "wav", 48000);
        let map_pos = args.iter().position(|a| a == "-map").unwrap();
        assert_eq!(args[map_pos + 1], "0:1");
        let af_pos = args.iter().position(|a| a == "-af").unwrap();
        assert_eq!(args[af_pos + 1], "pan=mono|FC=c0");
        let codec_pos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[codec_pos + 1], "pcm_s24le");
        let f_pos = args.iter().position(|a| a == "-f").unwrap();
        assert_eq!(args[f_pos + 1], "wav");
    }

    #[test]
    fn test_build_video_track_extract_args_aac() {
        let s = make_video_settings();
        let args = build_video_track_extract_args(&s, 0, 2, 1, "adts", 48000);
        let map_pos = args.iter().position(|a| a == "-map").unwrap();
        assert_eq!(args[map_pos + 1], "0:2");
        let af_pos = args.iter().position(|a| a == "-af").unwrap();
        assert_eq!(args[af_pos + 1], "pan=mono|FC=c1");
        let codec_pos = args.iter().position(|a| a == "-c:a").unwrap();
        assert_eq!(args[codec_pos + 1], "aac");
        let f_pos = args.iter().position(|a| a == "-f").unwrap();
        assert_eq!(args[f_pos + 1], "adts");
    }

    #[test]
    fn test_build_video_mux_all_audio_regression() {
        // Regression guard: no split, no drop → same args as original behavior
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let args = build_video_mux_args(&s, 0, &AudioKeep::AllAudio, &probe);
        assert!(args.contains(&"-map".to_string()));
        assert!(args.contains(&"0:a?".to_string()));
        assert!(args.contains(&"-c:a".to_string()));
    }
}