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

/// Hardware device info resolved for a concrete encoder at conversion time.
#[derive(Clone, Debug)]
pub enum ResolvedHwDevice {
    Vaapi { device_path: String },
    Vulkan,
}

impl ResolvedHwDevice {
    /// Prelude args: `-init_hw_device <type>[=name[:device]]` +
    /// `-filter_hw_device <name>` — must appear before `-i`.
    fn prelude_args(&self) -> Vec<String> {
        match self {
            ResolvedHwDevice::Vaapi { device_path } => vec![
                "-init_hw_device".to_string(),
                format!("vaapi=vaapi0:{}", device_path),
                "-filter_hw_device".to_string(),
                "vaapi0".to_string(),
            ],
            ResolvedHwDevice::Vulkan => vec![
                "-init_hw_device".to_string(),
                "vulkan=vulkan0".to_string(),
                "-filter_hw_device".to_string(),
                "vulkan0".to_string(),
            ],
        }
    }
}

/// Hardware-device availability context carried into the encoder fallback
/// loop.  Derived from [`FfmpegCapabilities::hw`] at spawn time.
#[derive(Clone, Debug, Default)]
struct HwDeviceContext {
    vaapi_device: Option<String>,
    vulkan_available: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct HwDeviceCapabilities {
    /// First probed VAAPI DRM render node path (e.g. `/dev/dri/renderD128`).
    /// `None` when no VAAPI device was found or when the platform is not Linux.
    pub vaapi_device: Option<String>,
    /// Whether a Vulkan device was successfully initialized via ffmpeg.
    pub vulkan_available: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FfmpegCapabilities {
    pub has_ffmpeg: bool,
    pub available_encoders: BTreeSet<String>,
    pub available_formats: BTreeSet<String>,
    pub error_message: Option<String>,
    #[serde(default)]
    pub hw: HwDeviceCapabilities,
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
                hw: HwDeviceCapabilities::default(),
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
                hw: HwDeviceCapabilities::default(),
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

    let hw = crate::hw_device::discover("ffmpeg", &encoders);

    FfmpegCapabilities {
        has_ffmpeg: true,
        available_encoders: encoders,
        available_formats: formats,
        error_message: None,
        hw,
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

/// Output container for stream-copy mode, derived from the input file's
/// extension. Same-container copies are the most faithful; MPEG-TS based
/// recordings (mts/m2ts/ts) remux into MP4 so `-timecode` metadata is
/// available, and everything else lands in Matroska which accepts nearly
/// every codec combination.
pub fn copy_mode_container_for_input(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "mp4" | "m4v" => "mp4",
        "mov" => "mov",
        "mkv" => "mkv",
        "mxf" => "mxf",
        "mts" | "m2ts" | "m2t" | "ts" => "mp4",
        _ => "mkv",
    }
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
    sanity_check_impl(
        container, video_codec, audio_encoder, input_files, output_folder,
        filename_prefix, caps, audio_suffix, video_suffix, None, false,
    )
}

/// Stream-copy variant of [`conversion_sanity_check`]: the video stream is
/// not re-encoded, so the video codec selection is irrelevant and its
/// availability/compatibility checks are skipped.
pub fn conversion_sanity_check_copy(
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
    sanity_check_impl(
        container, video_codec, audio_encoder, input_files, output_folder,
        filename_prefix, caps, audio_suffix, video_suffix, None, true,
    )
}

pub fn conversion_sanity_check_with_naming(
    container: &str,
    video_codec: &str,
    audio_encoder: &str,
    input_files: &[PathBuf],
    output_folder: &Path,
    filename_prefix: &str,
    caps: &FfmpegCapabilities,
    audio_suffix: Option<&str>,
    video_suffix: Option<&str>,
    naming_mode: Option<&OutputNamingMode>,
    copy_video: bool,
) -> Result<(), String> {
    sanity_check_impl(
        container, video_codec, audio_encoder, input_files, output_folder,
        filename_prefix, caps, audio_suffix, video_suffix, naming_mode, copy_video,
    )
}

#[allow(clippy::too_many_arguments)]
fn sanity_check_impl(
    container: &str,
    video_codec: &str,
    audio_encoder: &str,
    input_files: &[PathBuf],
    output_folder: &Path,
    filename_prefix: &str,
    caps: &FfmpegCapabilities,
    audio_suffix: Option<&str>,
    video_suffix: Option<&str>,
    naming_mode: Option<&OutputNamingMode>,
    copy_video: bool,
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

    let prefix_required = naming_mode.map_or(true, |m| !m.is_source_stems());
    if prefix_required && filename_prefix.is_empty() {
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
    let nm = naming_mode.unwrap_or(&OutputNamingMode::PrefixTemplates);
    if let Some(suffix) = audio_suffix {
        ConverterSettings::validate_suffix_template_for_mode(suffix, nm)?;
    }
    if let Some(suffix) = video_suffix {
        ConverterSettings::validate_suffix_template_for_mode(suffix, nm)?;
    }

    if !copy_video {
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
    }

    if !encoder_available_in_ffmpeg(audio_encoder, caps) {
        return Err(format!(
            "Audio encoder '{}' is not supported by your ffmpeg installation. \
             Run `ffmpeg -encoders` to see available encoders. \
             Common alternatives: pcm_s24le (PCM 24-bit), pcm_s16le (PCM 16-bit), aac, libopus.",
            audio_encoder
        ));
    }

    if !copy_video && !video_codecs::codec_supports_container(
        video_codecs::normalize_video_codec(video_codec),
        container,
    ) {
        let codec_id = video_codecs::normalize_video_codec(video_codec);
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

// ── Output naming mode ───────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum OutputNamingMode {
    /// Use `filename_prefix` + suffix templates (existing behavior).
    PrefixTemplates,
    /// For each video clip, derive the base name from the source file's stem.
    SourceStems,
}

impl OutputNamingMode {
    pub fn is_source_stems(&self) -> bool {
        matches!(self, OutputNamingMode::SourceStems)
    }
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
    /// Stream-copy mode ("Leave Video Encoding Untouched"): the video stream
    /// is remuxed without re-encoding. The container is derived from the
    /// input file and the video codec selection is ignored. Cuts snap to the
    /// nearest video keyframe at-or-before the trim offset.
    /// Only meaningful for `ConversionPipeline::VideoPassthrough`.
    pub copy_video: bool,
    /// Video *codec* id ("av1", "h265", …). Legacy concrete encoder names
    /// are accepted and normalized via `video_codecs::normalize_video_codec`.
    pub video_encoder: String,
    pub audio_encoder: String,
    /// Concrete ffmpeg encoder resolved from the codec chain at conversion
    /// time (hardware candidates first). Empty until resolved; the arg
    /// builders then prefer it over the static chain head.
    pub resolved_video_encoder: String,
    /// Hardware device info resolved together with `resolved_video_encoder`.
    /// `None` for software encoders, stream-copy mode, or when no hardware
    /// device is available (the candidate will be skipped).
    pub resolved_hw_device: Option<ResolvedHwDevice>,

    // ── Output Naming ──
    pub output_folder: PathBuf,
    pub filename_prefix: String,
    pub audio_suffix_template: String,
    pub video_suffix_template: String,
    pub naming_mode: OutputNamingMode,

    // ── Trimming & Timecode (per file) ──
    pub trim_to_first_ltc: bool,
    pub trim_offsets_secs: Vec<f64>,
    pub timecode_meta_per_file: Vec<Option<TimecodeMetadata>>,

    // ── Concatenation ──
    /// When true and `split_tracks` + `VideoClipSequence`: produce one audio
    /// file per track concatenated across all clips instead of per-clip files.
    pub concat_audio: bool,
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
    /// Concatenate a single audio track across multiple video clips (one output per track).
    AudioChannelConcat {
        segments: Vec<(usize, usize, usize)>,  // (file_idx, stream_idx, channel_idx) per clip
        output: PathBuf,
        format: String,
        sample_rate: u32,
    },
}

impl VideoOutputStep {
    /// Convenience accessor for the output path.
    pub fn output(&self) -> &std::path::Path {
        match self {
            VideoOutputStep::VideoOnly { output, .. }
            | VideoOutputStep::VideoMux { output, .. }
            | VideoOutputStep::AudioChannel { output, .. }
            | VideoOutputStep::AudioChannelConcat { output, .. } => output,
        }
    }
}

/// Plan the output steps for a single file in a video-to-video conversion.
///
/// Returns a flat list of steps for `file_idx` only. Audio channels are
/// numbered per-file (track 1 = first surviving channel of this file).
fn plan_video_outputs_for_file(settings: &ConverterSettings, file_idx: usize, probe: &VideoAudioProbe) -> Vec<VideoOutputStep> {
    let ext = extension_for_container(&settings.container);
    let mut steps: Vec<VideoOutputStep> = Vec::new();

    if settings.split_tracks {
        let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
        steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out });

        for stream in &probe.streams {
            for ch in 0..stream.channels {
                let ltc_match = settings.ltc_video_source == Some((stream.stream_index, ch));
                if settings.drop_ltc_track && ltc_match {
                    continue;
                }
                let (fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
                let audio_idx = steps.iter().filter(|s| matches!(s, VideoOutputStep::AudioChannel { .. })).count() + 1;
                let audio_out = settings.output_path_for_file("audio", file_idx, audio_idx, aext);
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
        let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);

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
            let total_channels: usize = probe.streams.iter().map(|s| s.channels).sum();
            let dropped_count: usize = drop_pairs.iter().filter(|(s, c)| {
                probe.streams.iter().any(|st| st.stream_index == *s && *c < st.channels)
            }).count();

            if dropped_count == total_channels {
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

    steps
}

/// Plan the output steps for a video-to-video conversion, given probe results.
///
/// Iterates all input files with the **same** probe (uniform layout assumed).
/// For per‑file probes call [`plan_video_outputs_for_file`] in a loop instead.
pub fn plan_video_outputs(settings: &ConverterSettings, probe: &VideoAudioProbe) -> Vec<VideoOutputStep> {
    let mut steps = Vec::new();
    for file_idx in 0..settings.input_files.len() {
        steps.extend(plan_video_outputs_for_file(settings, file_idx, probe));
    }
    steps
}

/// Build the flat channel list for a probe: `(stream_idx, channel_idx)` in
/// stream-then-channel order. Returns `None` if the probe has no audio.
fn probe_channel_list(probe: &VideoAudioProbe) -> Option<Vec<(usize, usize)>> {
    let mut channels = Vec::new();
    for s in &probe.streams {
        for ch in 0..s.channels {
            channels.push((s.stream_index, ch));
        }
    }
    if channels.is_empty() { None } else { Some(channels) }
}

/// Plan audio-concatenation steps for a `VideoClipSequence` with
/// `split_tracks = true` and `concat_audio = true`.
///
/// Returns a `(Vec<VideoOutputStep>, String)` where the string is a
/// warning log (empty when consistent or already handled).
/// When channel layouts or sample rates differ across clips, falls back to
/// per-clip `AudioChannel` steps and populates the warning string.
pub fn plan_concat_outputs(
    settings: &ConverterSettings,
    all_probes: &[Option<VideoAudioProbe>],
) -> (Vec<VideoOutputStep>, String) {
    let (fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
    let mut steps: Vec<VideoOutputStep> = Vec::new();
    let mut warnings = String::new();

    // Build per-clip channel lists, skipping probes without audio.
    let clip_channels: Vec<Option<Vec<(usize, usize)>>> = all_probes
        .iter()
        .map(|p| p.as_ref().and_then(probe_channel_list))
        .collect();

    // Determine the reference layout from the first clip with audio.
    let reference = match clip_channels.iter().find_map(|c| c.as_ref()) {
        Some(r) => r.clone(),
        None => return (steps, warnings), // no audio anywhere → no concat steps
    };

    // Determine sample rate from first clip's first stream.
    let sample_rate: u32 = all_probes
        .iter()
        .find_map(|p| p.as_ref())
        .and_then(|p| p.streams.first())
        .map(|s| s.sample_rate)
        .unwrap_or(48000);

    // Verify consistency and build segments
    let num_tracks = reference.len();
    let mut tracks_segments: Vec<Vec<(usize, usize, usize)>> = vec![Vec::new(); num_tracks];

    let mut consistent = true;
    for (file_idx, opt_cl) in clip_channels.iter().enumerate() {
        match opt_cl {
            Some(cl) => {
                // Check layout matches reference
                if cl.len() != num_tracks {
                    consistent = false;
                    break;
                }
                // Check stream-channel pairs match (order-sensitive)
                if cl.iter().zip(&reference).any(|(a, b)| a != b) {
                    consistent = false;
                    break;
                }
                // Check sample rate
                if let Some(p) = &all_probes[file_idx] {
                    if let Some(s) = p.streams.first() {
                        if s.sample_rate != sample_rate {
                            consistent = false;
                            break;
                        }
                    }
                }
                // Add segments for each track
                for track_idx in 0..num_tracks {
                    let (stream_idx, channel_idx) = cl[track_idx];
                    tracks_segments[track_idx].push((file_idx, stream_idx, channel_idx));
                }
            }
            None => {
                // Clip has no audio — still need to contribute nothing for
                // this track. We must either skip it from concat or pad.
                // For simplicity: skip the clip from all tracks.
                // This creates a gap but avoids issues. Log warning.
                warnings.push_str(&format!(
                    "Warning: clip {} has no audio; excluded from concatenation.\n",
                    settings.input_files[file_idx].display()
                ));
                // Don't mark as inconsistent — just skip.
            }
        }
    }

    if !consistent {
        // Layout mismatch — fall back to per-clip AudioChannel steps
        warnings.push_str(
            "Warning: audio channel layouts differ across clips — falling back to per-clip audio outputs.\n"
        );
        let fallback: Vec<VideoOutputStep> = all_probes
            .iter()
            .enumerate()
            .filter_map(|(fi, p)| p.as_ref().map(|probe| (fi, probe)))
            .flat_map(|(fi, probe)| {
                let mut file_steps = Vec::new();
                for s in &probe.streams {
                    for ch in 0..s.channels {
                        let ltc_match = settings.ltc_video_source == Some((s.stream_index, ch));
                        if settings.drop_ltc_track && ltc_match {
                            continue;
                        }
                        let audio_idx = file_steps.len() + 1;
                        let audio_out = settings.output_path_for_file("audio", fi, audio_idx, aext);
                        file_steps.push(VideoOutputStep::AudioChannel {
                            file_idx: fi,
                            stream_idx: s.stream_index,
                            channel_idx: ch,
                            output: audio_out,
                            format: fmt.to_string(),
                        });
                    }
                }
                file_steps
            })
            .collect();
        return (fallback, warnings);
    }

    // Build one AudioChannelConcat step per surviving track
    for track_idx in 0..num_tracks {
        // Check if this track should be dropped (LTC track)
        if settings.drop_ltc_track {
            let (ref_stream, ref_ch) = reference[track_idx];
            if settings.ltc_video_source == Some((ref_stream, ref_ch)) {
                continue;
            }
        }
        let segments: Vec<(usize, usize, usize)> = tracks_segments[track_idx].to_vec();
        if segments.is_empty() {
            continue;
        }
        let audio_out = settings.output_path_for_file("audio", 0, track_idx + 1, aext);
        steps.push(VideoOutputStep::AudioChannelConcat {
            segments,
            output: audio_out,
            format: fmt.to_string(),
            sample_rate,
        });
    }

    (steps, warnings)
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
        self.output_path_for_file(kind, 0, index, extension)
    }

    /// Build the output path for a single output file given a source-file index,
    /// a track/clip index, and an extension derived from the container.
    ///
    /// In `SourceStems` mode the base name comes from the source file's stem;
    /// in `PrefixTemplates` mode the `filename_prefix` field is used.
    /// If the computed output path would collide with an input file path,
    /// `_conv` is appended before the extension.
    pub fn output_path_for_file(&self, kind: &str, file_idx: usize, index: usize, extension: &str) -> PathBuf {
        let base = self.output_base_for_file(file_idx);
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
        let filename = format!("{}{}.{}", base, suffix, extension);
        let path = self.output_folder.join(&filename);
        if self.input_files.iter().any(|input| input == &path) {
            let alt = format!("{}_conv{}.{}", base, suffix, extension);
            self.output_folder.join(&alt)
        } else {
            path
        }
    }

    /// Return the base name (stem without extension / index suffix) for a
    /// given source file index, respecting the naming mode.
    pub fn output_base_for_file(&self, file_idx: usize) -> String {
        match self.naming_mode {
            OutputNamingMode::SourceStems => {
                self.input_files.get(file_idx)
                    .and_then(|p| p.file_stem())
                    .and_then(|s| s.to_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| self.filename_prefix.clone())
            }
            OutputNamingMode::PrefixTemplates => self.filename_prefix.clone(),
        }
    }

    /// Validate a suffix template: returns an error if the template
    /// contains no recognized placeholder but would produce duplicates.
    /// When `naming_mode` is `SourceStems`, the placeholder requirement is
    /// waived because source-file stems guarantee uniqueness across clips.
    pub fn validate_suffix_template(template: &str) -> Result<(), String> {
        Self::validate_suffix_template_for_mode(template, &OutputNamingMode::PrefixTemplates)
    }

    fn validate_suffix_template_for_mode(template: &str, naming_mode: &OutputNamingMode) -> Result<(), String> {
        if template.is_empty() {
            return Ok(());
        }
        if naming_mode.is_source_stems() {
            return Ok(());
        }
        let has_placeholder = template.contains("{:01d}")
            || template.contains("{:02d}")
            || template.contains("{:03d}");
        if has_placeholder {
            return Ok(());
        }
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

                let use_concat = self.concat_audio
                    && self.split_tracks
                    && self.recording_type == RecordingType::VideoClipSequence;

                if use_concat {
                    // Probe all files, emit video paths + concat audio paths
                    let mut probes: Vec<Option<VideoAudioProbe>> = Vec::new();
                    for file_idx in 0..self.input_files.len() {
                        let input = &self.input_files[file_idx];
                        match crate::ffprobe::probe_video_audio(input) {
                            Ok(probe) => {
                                // Emit video path (split mode → VideoOnly per file)
                                let ext = extension_for_container(&self.container);
                                let video_out = self.output_path_for_file("video", file_idx, file_idx + 1, ext);
                                paths.push(video_out);
                                probes.push(Some(probe));
                            }
                            Err(_) => {
                                paths.push(self.output_path_for_file("video", file_idx, file_idx + 1, extension));
                                probes.push(None);
                            }
                        }
                    }
                    let (concat_steps, _warning) = plan_concat_outputs(self, &probes);
                    for step in &concat_steps {
                        if let VideoOutputStep::AudioChannelConcat { output, .. } = step {
                            paths.push(output.clone());
                        }
                    }
                } else {
                    // Normal per-file planning: one call per file, no duplicates
                    for file_idx in 0..self.input_files.len() {
                        let input = &self.input_files[file_idx];
                        if let Ok(probe) = crate::ffprobe::probe_video_audio(input) {
                            for step in plan_video_outputs_for_file(self, file_idx, &probe) {
                                paths.push(step.output().to_path_buf());
                            }
                        } else {
                            paths.push(self.output_path_for_file("video", file_idx, file_idx + 1, extension));
                        }
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

// ── Preview helper ─────────────────────────────────────────────────────────

/// Kind of an output file in the preview.
#[derive(Clone, Debug, PartialEq)]
pub enum OutputKind {
    Video,
    Audio,
}

/// A single output file in the preview, with its kind and full path.
#[derive(Clone, Debug)]
pub struct PreviewOutput {
    pub kind: OutputKind,
    pub path: PathBuf,
}

/// Compute the list of output files that a conversion would produce,
/// **without** running any I/O (no ffprobe, no ffmpeg).
///
/// - For `VideoPassthrough` pipelines the single `probe` is reused for every
///   clip (uniform channel layout is assumed — the normal case for same‑model
///   cameras). This mirrors the expectation that channel counts are identical.
/// - When `probe` is `None` only video outputs are listed (the layout is
///   unknown).
/// - In `copy_video` mode the container is derived from the first input file
///   (matching `prepare_copy_mode`).
/// - For `AudioOnly` pipelines the channel‑map and `drop_ltc_track` settings
///   are used directly; no ffprobe probe is needed.
pub fn preview_output_files(settings: &ConverterSettings, probe: Option<&VideoAudioProbe>) -> Vec<PreviewOutput> {
    match settings.pipeline {
        ConversionPipeline::VideoPassthrough => {
            let mut settings = settings.clone();
            if settings.copy_video {
                if let Some(first) = settings.input_files.first() {
                    settings.container = copy_mode_container_for_input(first).to_string();
                }
            }
            let ext = extension_for_container(&settings.container);
            let use_concat = settings.concat_audio
                && settings.split_tracks
                && settings.recording_type == RecordingType::VideoClipSequence;
            let mut previews = Vec::new();

            if use_concat {
                for file_idx in 0..settings.input_files.len() {
                    let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
                    previews.push(PreviewOutput { kind: OutputKind::Video, path: video_out });
                }

                let probe_cloned = probe.cloned();
                let probes: Vec<Option<VideoAudioProbe>> = (0..settings.input_files.len())
                    .map(|_| probe_cloned.clone())
                    .collect();
                let (concat_steps, _warning) = plan_concat_outputs(&settings, &probes);
                for step in &concat_steps {
                    if let VideoOutputStep::AudioChannelConcat { output, .. } = step {
                        previews.push(PreviewOutput { kind: OutputKind::Audio, path: output.clone() });
                    }
                }
            } else if let Some(probe) = probe {
                for file_idx in 0..settings.input_files.len() {
                    for step in plan_video_outputs_for_file(&settings, file_idx, probe) {
                        previews.push(PreviewOutput {
                            kind: match step {
                                VideoOutputStep::AudioChannel { .. }
                                | VideoOutputStep::AudioChannelConcat { .. } => OutputKind::Audio,
                                _ => OutputKind::Video,
                            },
                            path: step.output().to_path_buf(),
                        });
                    }
                }
            } else {
                for file_idx in 0..settings.input_files.len() {
                    let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
                    previews.push(PreviewOutput { kind: OutputKind::Video, path: video_out });
                }
            }

            previews
        }
        ConversionPipeline::AudioOnly { generate_synthetic_video } => {
            let (_fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
            let mut previews = Vec::new();

            if settings.split_tracks {
                for i in 0..settings.channel_map.num_channels() {
                    if settings.drop_ltc_track && i == settings.ltc_track_channel_index {
                        continue;
                    }
                    previews.push(PreviewOutput {
                        kind: OutputKind::Audio,
                        path: settings.output_path_for_index("audio", i + 1, aext),
                    });
                }
            } else {
                let audio_path = settings.output_folder.join(format!("{}.{}", settings.filename_prefix, aext));
                previews.push(PreviewOutput { kind: OutputKind::Audio, path: audio_path });
            }

            if generate_synthetic_video {
                let ext = extension_for_container(&settings.container);
                previews.push(PreviewOutput {
                    kind: OutputKind::Video,
                    path: settings.output_path_for_index("video", 1, ext),
                });
            }

            previews
        }
    }
}

/// Informational (non-blocking) note when planned output filenames would
/// exactly collide with an input file. The collision is mitigated by
/// [`ConverterSettings::output_path_for_file`] which inserts `_conv` into
/// the filename automatically. Returns `None` when no collision exists.
///
/// The naming logic mirrors [`ConverterSettings::output_path_for_file`] and
/// [`plan_video_outputs`]: for each input file the base name is derived from
/// the naming mode, the video suffix template is expanded with index `i+1`,
/// and the extension comes from the container (or per-input-file
/// `copy_mode_container_for_input` when `copy_video` is true).
pub fn output_collision_warning(
    input_files: &[PathBuf],
    output_folder: &Path,
    filename_prefix: &str,
    naming_mode: &OutputNamingMode,
    video_suffix_template: &str,
    container: &str,
    copy_video: bool,
) -> Option<String> {
    let mut colliding: Vec<(String, String)> = Vec::new();

    for (i, input) in input_files.iter().enumerate() {
        let base = match naming_mode {
            OutputNamingMode::SourceStems => match input.file_stem().and_then(|s| s.to_str()) {
                Some(stem) => stem.to_string(),
                None => continue,
            },
            OutputNamingMode::PrefixTemplates => {
                if filename_prefix.is_empty() {
                    continue;
                }
                filename_prefix.to_string()
            }
        };

        let suffix = video_suffix_template
            .replace("{:01d}", &format!("{:01}", i + 1))
            .replace("{:02d}", &format!("{:02}", i + 1))
            .replace("{:03d}", &format!("{:03}", i + 1));

        let ext = if copy_video {
            copy_mode_container_for_input(input)
        } else {
            extension_for_container(container)
        };

        let planned = output_folder.join(format!("{}{}.{}", base, suffix, ext));

        if input_files.contains(&planned) {
            let mitigated = output_folder.join(format!("{}_conv{}.{}", base, suffix, ext));
            colliding.push((
                planned.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                mitigated.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
            ));
        }
    }

    if colliding.is_empty() {
        None
    } else {
        let details: String = colliding.iter()
            .map(|(orig, mitigated)| format!("'{}' will be written as '{}'", orig, mitigated))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "Output folder is the same as the input folder. {} \
             Consider choosing a different output folder.",
            details
        ))
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

/// Step a `Timecode` back by one frame at the given frame rate.
///
/// Drop-frame aware (SMPTE 12M-1): frames 0 and 1 do not exist at the start
/// of minutes whose number is not divisible by 10, so stepping back from
/// frame 2 of such a minute lands on the last frame of the previous minute.
/// The inverse of `audio_core::increment_timecode`.
fn decrement_timecode_frame(tc: &Timecode, fps: f64, drop_frame: bool) -> Timecode {
    let max_frames = fps.ceil() as u32;
    let mut h = tc.hours;
    let mut m = tc.minutes;
    let mut s = tc.seconds;
    let mut f = tc.frames;

    // Drop-frame skipped frames: (m % 10 != 0, s == 0, f <= 1) does not exist.
    if drop_frame && s == 0 && m % 10 != 0 && f <= 1 {
        if m > 0 {
            m -= 1;
        } else {
            m = 59;
            h = if h == 0 { 23 } else { h - 1 };
        }
        return Timecode { hours: h, minutes: m, seconds: 59, frames: max_frames - 1 };
    }

    if f > 0 {
        f -= 1;
    } else if s > 0 {
        s -= 1;
        f = max_frames - 1;
    } else {
        if m > 0 {
            m -= 1;
        } else {
            m = 59;
            h = if h == 0 { 23 } else { h - 1 };
        }
        s = 59;
        f = max_frames - 1;
    }

    Timecode { hours: h, minutes: m, seconds: s, frames: f }
}

/// Shift a `Timecode` back by `delta_secs` worth of frames at the given
/// frame rate (drop-frame aware). Used to re-anchor start-timecode metadata
/// when a stream-copy trim is snapped to an earlier video keyframe.
pub fn shift_timecode_back(tc: &Timecode, delta_secs: f64, fps: f64, drop_frame: bool) -> Timecode {
    let mut out = *tc;
    if delta_secs <= 0.0 || fps <= 0.0 {
        return out;
    }
    let mut frames = (delta_secs * fps).round() as u64;
    while frames > 0 {
        out = decrement_timecode_frame(&out, fps, drop_frame);
        frames -= 1;
    }
    out
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
    ];
    push_hw_device_prelude(&mut args, settings);
    args.extend([
        "-f".to_string(), "lavfi".to_string(),
        "-i".to_string(), "color=c=blue:s=1280x720:r=25".to_string(),
    ]);

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
        push_timecode_args(&mut args, tc, true);
    }

    args.push("-shortest".to_string());
    push_output_trailer(&mut args, container_to_ffmpeg_format(&settings.container));

    args
}

/// Push the video encoding args for the selected mode: stream copy
/// (`-c:v copy`, no encoder-specific args) when `settings.copy_video` is set,
/// otherwise the resolved encoder chain from the codec registry.
fn push_video_codec_args(args: &mut Vec<String>, settings: &ConverterSettings) {
    if settings.copy_video {
        args.push("-c:v".to_string());
        args.push("copy".to_string());
        return;
    }
    push_video_encoder(args, settings);
}

/// Build ffmpeg args for a video-only step (no audio).
fn build_video_only_args(settings: &ConverterSettings, file_idx: usize) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings.trim_offsets_secs.get(file_idx).copied().unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_hw_device_prelude(&mut args, settings);
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push("0:v".to_string());
    args.push("-an".to_string());

    push_video_codec_args(&mut args, settings);

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_timecode_args(&mut args, tc, !settings.copy_video);
    }

    push_output_trailer(&mut args, container_to_ffmpeg_format(&settings.container));
    args
}

/// Build ffmpeg args for video-mux step (audio kept, possibly filtered).
fn build_video_mux_args(settings: &ConverterSettings, file_idx: usize, keep: &AudioKeep, probe: &VideoAudioProbe) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings.trim_offsets_secs.get(file_idx).copied().unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_hw_device_prelude(&mut args, settings);
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push("0:v".to_string());

    match keep {
        AudioKeep::AllAudio => {
            args.push("-map".to_string());
            args.push("0:a?".to_string());
            args.push("-c:a".to_string());
            if settings.copy_video {
                // Untouched mode: audio is stream-copied alongside the video.
                args.push("copy".to_string());
            } else {
                args.push(settings.audio_encoder.clone());
            }
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
                // Channel filtering requires decoding the audio, so it is
                // re-encoded even in copy mode; the video stream stays copied.
                args.push("-c:a".to_string());
                args.push(settings.audio_encoder.clone());
            }
        }
    }

    push_video_codec_args(&mut args, settings);

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_timecode_args(&mut args, tc, !settings.copy_video);
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

/// Build ffmpeg args for a concatenated audio track across multiple clips.
/// Each clip contributes one channel extracted via pan, then all are joined
/// with the concat filter. Produces a single audio output per call.
fn build_concat_audio_args(
    settings: &ConverterSettings,
    segments: &[(usize, usize, usize)],
    format: &str,
    sample_rate: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".to_string()];

    // Input files with per-clip trim
    for &(file_idx, _stream_idx, _channel_idx) in segments.iter() {
        let trim_secs = settings.trim_offsets_secs.get(file_idx).copied().unwrap_or(0.0);
        push_input_with_trim(&mut args, &settings.input_files[file_idx], trim_secs);
    }

    let n = segments.len();
    let mut filter_parts: Vec<String> = Vec::new();

    // Per-clip: pan the specific (stream, channel) to mono
    for (i, &(_file_idx, stream_idx, channel_idx)) in segments.iter().enumerate() {
        filter_parts.push(format!(
            "[{}:{}]pan=mono|FC=c{}[a{}]",
            i, stream_idx, channel_idx, i
        ));
    }

    // Concat all audio segments
    let concat_inputs: String = (0..n).map(|i| format!("[a{}]", i)).collect::<Vec<_>>().join("");
    filter_parts.push(format!(
        "{}concat=n={}:v=0:a=1[out]",
        concat_inputs, n
    ));

    args.push("-filter_complex".to_string());
    args.push(filter_parts.join(";"));
    args.push("-map".to_string());
    args.push("[out]".to_string());
    args.push("-vn".to_string());
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    // Timecode metadata from clip 0
    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.first() {
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
        VideoOutputStep::AudioChannelConcat { .. } => {
            panic!("AudioChannelConcat must be executed directly via run_ffmpeg_process, not through build_video_to_video_args")
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
/// Push pre-input hw-device args (`-init_hw_device` / `-filter_hw_device`)
/// when the resolved encoder requires hw-frame plumbing.
///
/// Must be called **before** the first `-i` in the arg list.
/// No-op in stream-copy mode or when no hw device is resolved.
fn push_hw_device_prelude(args: &mut Vec<String>, settings: &ConverterSettings) {
    if settings.copy_video {
        return;
    }
    if let Some(ref hw) = settings.resolved_hw_device {
        args.extend(hw.prelude_args());
    }
}

/// Push the video encoding args: `-c:v <resolved encoder>` plus codec-level
/// args (e.g. `-tag:v hvc1` for HEVC) and the resolved encoder's own args
/// (e.g. `-pix_fmt yuv420p`), from the codec registry.
///
/// When the resolved encoder requires hw-frame plumbing (VAAPI / Vulkan),
/// also pushes `-vf format=nv12,hwupload`.
fn push_video_encoder(args: &mut Vec<String>, settings: &ConverterSettings) {
    let codec_id = video_codecs::normalize_video_codec(&settings.video_encoder);
    let encoder = settings.effective_video_encoder();
    args.push("-c:v".to_string());
    args.push(encoder.clone());

    // Check if this encoder needs hw-frame upload
    let needs_hw_upload = video_codecs::hw_frames_for(&encoder).is_some();

    for (key, value) in video_codecs::codec_args(codec_id) {
        args.push(format!("-{}", key));
        args.push((*value).to_string());
    }
    for (key, value) in video_codecs::candidate_args(&encoder) {
        args.push(format!("-{}", key));
        args.push((*value).to_string());
    }
    if needs_hw_upload {
        args.push("-vf".to_string());
        args.push("format=nv12,hwupload".to_string());
    }
}

/// Push timecode metadata args for a video output file.
///
/// When `force_frame_rate` is true (encode mode) the stream's frame rate is
/// also pinned via `-r`; in stream-copy mode the original timing must be
/// preserved, so `-r` is omitted.
fn push_timecode_args(args: &mut Vec<String>, tc: &TimecodeMetadata, force_frame_rate: bool) {
    let tc_str = format_ffmpeg_timecode(&tc.start, tc.drop_frame);
    args.push("-timecode".to_string());
    args.push(tc_str);
    args.push("-write_tmcd".to_string());
    args.push("1".to_string());
    if force_frame_rate {
        args.push("-r".to_string());
        args.push(format!("{:.3}", tc.fps));
    }
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
///
/// Also holds a [`HwDeviceContext`] so candidates that require hw-frame
/// plumbing (VAAPI / Vulkan) are skipped immediately when no device is
/// available, without invoking ffmpeg.
struct EncoderFallback {
    chain: Vec<String>,
    failed: BTreeSet<String>,
    resolved: Option<String>,
    hw_ctx: HwDeviceContext,
}

impl EncoderFallback {
    fn new_with_hw(chain: Vec<String>, hw_ctx: HwDeviceContext) -> Self {
        EncoderFallback {
            chain,
            failed: BTreeSet::new(),
            resolved: None,
            hw_ctx,
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

/// Returns the [`ResolvedHwDevice`] for a candidate encoder, or `None` when
/// the candidate does not require hw frames or when no suitable device is
/// known.  This is called in the fallback loop before building args.
fn resolve_device_for_candidate(encoder: &str, ctx: &HwDeviceContext) -> Option<ResolvedHwDevice> {
    match video_codecs::hw_frames_for(encoder) {
        Some(video_codecs::HwFramePath::Vaapi) => {
            ctx.vaapi_device.clone().map(|path| ResolvedHwDevice::Vaapi { device_path: path })
        }
        Some(video_codecs::HwFramePath::Vulkan) if ctx.vulkan_available => {
            Some(ResolvedHwDevice::Vulkan)
        }
        _ => None,
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
        settings.resolved_hw_device = resolve_device_for_candidate(&encoder, &fallback.hw_ctx);

        // Skip hw-frames candidates when no device is available (fast demotion
        // without spawning ffmpeg).
        if video_codecs::hw_frames_for(&encoder).is_some() && settings.resolved_hw_device.is_none() {
            let msg = format!(
                "--- no hardware device available for '{}'; skipping ---",
                encoder
            );
            warn!("{}", msg);
            overall_log.push_str(&format!("\n--- {} ---\n", msg));
            fallback.note_failure(&encoder);
            attempt += 1;
            continue;
        }

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

// ── Stream-copy preparation ─────────────────────────────────────────────

/// Prepare `ConverterSettings` for a stream-copy conversion:
///
/// 1. Derive the output container from the first input file (all clips in a
///    group share a container; a mixed group logs a warning).
/// 2. Snap each non-zero trim offset to the nearest video keyframe
///    at-or-before it (`ffprobe` packet scan) so `-c copy` cuts land exactly
///    on the snap point.
/// 3. Re-anchor the per-file start timecode to the snapped offset so the
///    embedded timecode matches the actual first video frame.
fn prepare_copy_mode(settings: &mut ConverterSettings) {
    if let Some(first) = settings.input_files.first() {
        let container = copy_mode_container_for_input(first);
        let mixed = settings
            .input_files
            .iter()
            .any(|f| copy_mode_container_for_input(f) != container);
        if mixed {
            warn!(
                "Mixed input containers in copy mode; using '{}' for all outputs",
                container
            );
        }
        settings.container = container.to_string();
    }

    for i in 0..settings.trim_offsets_secs.len() {
        let raw = settings.trim_offsets_secs[i];
        if raw <= 0.001 {
            continue;
        }
        let Some(path) = settings.input_files.get(i) else {
            continue;
        };
        let snapped = crate::ffprobe::snap_trim_to_keyframe(path, raw);
        let delta = raw - snapped;
        if delta <= 0.001 {
            continue;
        }
        info!(
            "Copy mode: trim for '{}' snapped {:.3}s → {:.3}s (keyframe)",
            path.display(),
            raw,
            snapped
        );
        settings.trim_offsets_secs[i] = snapped;
        if let Some(Some(meta)) = settings.timecode_meta_per_file.get_mut(i) {
            meta.start = shift_timecode_back(&meta.start, delta, meta.fps, meta.drop_frame);
        }
    }
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
    // avoids moving the `caps` borrow into the spawned thread). Skipped in
    // stream-copy mode where no encoder is used at all.
    let copy_mode_requested = settings.copy_video
        && matches!(settings.pipeline, ConversionPipeline::VideoPassthrough);
    let codec_id = video_codecs::normalize_video_codec(&settings.video_encoder).to_string();
    let mut chain: Vec<String> = if copy_mode_requested {
        Vec::new()
    } else {
        caps.filter(|c| c.has_ffmpeg)
            .map(|c| video_codecs::resolve_encoder_chain(&codec_id, c))
            .unwrap_or_default()
    };
    if chain.is_empty() && !copy_mode_requested {
        // No capability info (or stale): try the full static chain and
        // let the runtime fallback sort it out.
        chain = video_codecs::static_encoder_chain(&codec_id);
    }

    // Extract hw context from capabilities for the spawned thread.
    let hw_ctx = caps
        .filter(|c| c.has_ffmpeg)
        .map(|c| HwDeviceContext {
            vaapi_device: c.hw.vaapi_device.clone(),
            vulkan_available: c.hw.vulkan_available,
        })
        .unwrap_or_default();

    std::thread::spawn(move || {
        let mut settings = settings;
        let copy_mode = settings.copy_video
            && matches!(settings.pipeline, ConversionPipeline::VideoPassthrough);
        if copy_mode {
            prepare_copy_mode(&mut settings);
        }
        let mut fallback = EncoderFallback::new_with_hw(chain, hw_ctx);
        if copy_mode {
            info!(
                "Video stream copy mode: video will not be re-encoded \
                 (container '{}', video codec selection ignored)",
                settings.container
            );
        } else {
            info!(
                "Encoder chain for codec '{}': {}",
                codec_id,
                fallback.remaining().join(" → ")
            );
        }

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
            let encoder_line = if copy_mode {
                "\nVideo stream: copied (no re-encode)".to_string()
            } else {
                fallback
                    .resolved()
                    .map(|e| format!("\nVideo encoder used: {}", e))
                    .unwrap_or_default()
            };
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
    _extension: &str,
    fallback: &mut EncoderFallback,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    _total_steps: &mut usize,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    // Phase 1: probe each file
    let mut probes: Vec<Option<VideoAudioProbe>> = Vec::new();

    for file_idx in 0..settings.input_files.len() {
        if cancel.load(Ordering::Relaxed) { break; }
        let input = &settings.input_files[file_idx];
        match crate::ffprobe::probe_video_audio(input) {
            Ok(probe) => {
                probes.push(Some(probe));
            }
            Err(e) => {
                warn!("Probe failed for '{}': {} — treating as no-audio", input.display(), e);
                probes.push(None);
            }
        }
    }

    // Phase 2: build step plan
    let mut steps: Vec<StepEntry> = Vec::new();

    let use_concat = settings.concat_audio
        && settings.split_tracks
        && settings.recording_type == RecordingType::VideoClipSequence;

    if use_concat {
        let ext = extension_for_container(&settings.container);
        // Video-only steps per file
        for file_idx in 0..settings.input_files.len() {
            if cancel.load(Ordering::Relaxed) { break; }
            let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
            steps.push(StepEntry::Video(VideoOutputStep::VideoOnly { file_idx, output: video_out }));
        }

        // Concat audio steps
        let (concat_steps, warning) = plan_concat_outputs(settings, &probes);
        if !warning.is_empty() {
            warn!("{}", warning.trim());
            overall_log.push_str(&format!("\n--- {}\n", warning.trim()));
        }
        for cs in concat_steps {
            steps.push(StepEntry::AudioOnly(cs));
        }
    } else {
        // Normal per-file planning (no concat): one call per probe → one
        // file planned per call (uses plan_video_outputs_for_file), no duplicates.
        for (file_idx, probe_opt) in probes.iter().enumerate() {
            let probe = match probe_opt {
                Some(p) => p.clone(),
                None => VideoAudioProbe {
                    streams: Vec::new(),
                    total_audio_channels: 0,
                    is_video_file: true,
                },
            };
            for s in plan_video_outputs_for_file(settings, file_idx, &probe) {
                steps.push(StepEntry::Video(s));
            }
        }
    }

    // Recalculate total steps
    *_total_steps = steps.len();

    // Phase 3: execute steps
    for (step_idx, entry) in steps.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { break; }

        match entry {
            StepEntry::Video(step) => {
                let output = match step {
                    VideoOutputStep::VideoOnly { output, .. }
                    | VideoOutputStep::VideoMux { output, .. }
                    | VideoOutputStep::AudioChannel { output, .. }
                    | VideoOutputStep::AudioChannelConcat { output, .. } => output.clone(),
                };
                let step_progress = 1.0 / steps.len().max(1) as f32;
                // Find the probe for this step — for AudioChannel steps we
                // need the file's probe. For concat steps, any probe works
                // for the purpose of build_video_to_video_args (but concat
                // steps should never reach this path).
                let probe = probes.first().cloned().flatten().unwrap_or(VideoAudioProbe {
                    streams: Vec::new(),
                    total_audio_channels: 0,
                    is_video_file: true,
                });
                let mut build_args =
                    |s: &ConverterSettings| build_video_to_video_args(s, step, &probe);
                // Copy mode runs without the encoder fallback: no encoder is
                // involved, so any failure is fatal for the run.
                let ok = if settings.copy_video {
                    match run_ffmpeg_process(
                        &build_args(settings),
                        &output,
                        state,
                        cancel,
                        step_progress,
                        overall_progress,
                        overall_log,
                        steps.len(),
                        step_idx + 1,
                    ) {
                        Ok(()) => true,
                        Err(_) => false,
                    }
                } else {
                    run_video_step_with_fallback(
                        settings,
                        fallback,
                        &mut build_args,
                        &output,
                        state,
                        cancel,
                        step_progress,
                        overall_progress,
                        overall_log,
                        steps.len(),
                        step_idx + 1,
                    )
                };
                *overall_progress += step_progress;
                if !ok {
                    if settings.copy_video {
                        mark_conversion_failed(state, overall_log);
                    }
                    break;
                }
            }
            StepEntry::AudioOnly(step) => {
                let (output, args) = match step {
                    VideoOutputStep::AudioChannelConcat { segments, output, format, sample_rate } => {
                        (output.clone(), build_concat_audio_args(settings, segments, format, *sample_rate))
                    }
                    _ => unreachable!(),
                };
                let step_progress = 1.0 / steps.len().max(1) as f32;
                if run_ffmpeg_process(&args, &output, state, cancel, step_progress, overall_progress, overall_log, steps.len(), step_idx + 1).is_err() {
                    mark_conversion_failed(state, overall_log);
                    break;
                }
                *overall_progress += step_progress;
            }
        }
    }
}

/// Internal: discriminates steps that go through the video encoder fallback
/// from steps that are audio-only and go directly to run_ffmpeg_process.
enum StepEntry {
    Video(VideoOutputStep),
    AudioOnly(VideoOutputStep),
}

/// Minimum output file size (in bytes) that suggests ffmpeg actually produced
/// real encoded/copied content (not just a muxer header).  Muxer headers are
/// typically ≪ 4 KiB (WAV = 44 B, mkv ~1 KiB, mp4 ftyp = few hundred B).
const MIN_PRODUCED_OUTPUT_BYTES: u64 = 4096;

/// Parse an `out_time=` progress line from `-progress pipe:2` output.
/// Returns `Some(duration_seconds)` when the line contains a valid
/// `out_time=HH:MM:SS.ssssss` value (including 0.0), and `None` for
/// `N/A`, suffix keys (`out_time_us`, `out_time_ms`), or unrelated lines.
fn parse_out_time(line: &str) -> Option<f64> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap());
    let caps = re.captures(line)?;
    let raw = caps.get(0).map(|m| m.as_str()).unwrap_or("");
    // out_time_us= or out_time_ms= must not match: the regex pattern matches
    // "out_time=" but "out_time_us=" also starts with "out_time=…" — check
    // the suffix to reject those variants.
    let tail = &raw["out_time".len()..];
    if !tail.starts_with('=') {
        return None;
    }
    let h: f64 = caps[1].parse().unwrap_or(0.0);
    let m: f64 = caps[2].parse().unwrap_or(0.0);
    let s: f64 = caps[3].parse().unwrap_or(0.0);
    let frac: f64 = caps[4].parse().unwrap_or(0.0) / 1_000_000.0;
    Some(h * 3600.0 + m * 60.0 + s + frac)
}

/// Classify an ffmpeg step failure as retryable (`EncoderInit`) or
/// terminal (`Fatal`).  Considers both `produced_output` (from log
/// parsing) and a file-size sanity check so encoder-init failures that
/// leave a header-only (or zero-length) file are correctly retried.
fn classify_step_failure(produced_output: bool, output: &Path, code: &str) -> StepFailure {
    // File-size sanity check: even without log-progress, a real output file
    // strongly suggests that the encoder did produce some data before failing.
    let file_output = std::fs::metadata(output)
        .map(|m| m.len())
        .unwrap_or(0)
        >= MIN_PRODUCED_OUTPUT_BYTES;
    if file_output && !produced_output {
        info!(
            "classify_step_failure: produced_output=false but output file is {} bytes — treating as Fatal",
            std::fs::metadata(output).map(|m| m.len()).unwrap_or(0)
        );
    }
    if produced_output || file_output {
        StepFailure::Fatal(format!("ffmpeg exited with code {}", code))
    } else {
        StepFailure::EncoderInit(format!(
            "ffmpeg exited with code {} before producing output",
            code
        ))
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

        if let Some(current_secs) = parse_out_time(&line) {
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
            // Do NOT set produced_output here — ffmpeg emits progress=end
            // even on encoder-init failures that produced no output.
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
            let classification = classify_step_failure(produced_output, output, &code);
            if matches!(classification, StepFailure::EncoderInit(_)) {
                let _ = std::fs::remove_file(output);
            }
            Err(classification)
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
            copy_video: false,
            video_encoder: "h264".to_string(),
            audio_encoder: "pcm_s24le".to_string(),
            resolved_video_encoder: String::new(),
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "output".to_string(),
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
            naming_mode: OutputNamingMode::PrefixTemplates,
            trim_to_first_ltc: false,
            trim_offsets_secs: vec![0.0; 2],
            timecode_meta_per_file: vec![None; 2],
            concat_audio: false,
            resolved_hw_device: None,
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
            hw: HwDeviceCapabilities::default(),
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
            hw: HwDeviceCapabilities::default(),
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
    fn test_parse_out_time_happy_path() {
        // ffmpeg -progress output uses 6‑digit fractional microseconds
        assert_eq!(parse_out_time("out_time=00:00:01.200000"), Some(1.2));
        assert_eq!(parse_out_time("out_time=00:01:02.030000"), Some(62.03));
        assert!((parse_out_time("out_time=00:00:00.000000").unwrap() - 0.0).abs() < 1e-12);
    }

    #[test]
    fn test_parse_out_time_na() {
        assert_eq!(parse_out_time("out_time=N/A"), None);
    }

    #[test]
    fn test_parse_out_time_key_suffixes() {
        // _us and _ms variants must NOT match
        assert_eq!(parse_out_time("out_time_us=N/A"), None);
        assert_eq!(parse_out_time("out_time_ms=0"), None);
    }

    #[test]
    fn test_parse_out_time_progress_lines() {
        assert_eq!(parse_out_time("progress=end"), None);
        assert_eq!(parse_out_time("progress=continue"), None);
    }

    #[test]
    fn test_parse_out_time_stderr_time_not_mistaken() {
        // stderr "time=" / "time:" lines must not match
        assert_eq!(parse_out_time("time=00:01:23.45"), None);
        assert_eq!(parse_out_time("size=    1024kB time=00:00:04.56"), None);
    }

    #[test]
    fn test_parse_out_time_empty() {
        assert_eq!(parse_out_time(""), None);
    }

    #[test]
    fn test_classify_step_failure_no_output_file_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("nonexistent.mkv");
        assert!(matches!(
            classify_step_failure(false, &p, "187"),
            StepFailure::EncoderInit(_)
        ));
    }

    #[test]
    fn test_classify_step_failure_no_output_zero_byte() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("empty.mkv");
        std::fs::write(&p, b"").unwrap();
        assert!(matches!(
            classify_step_failure(false, &p, "187"),
            StepFailure::EncoderInit(_)
        ));
    }

    #[test]
    fn test_classify_step_failure_no_output_header_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("header.mkv");
        // 44 bytes — typical WAV header size, well under 4 KiB
        std::fs::write(&p, [0u8; 44]).unwrap();
        assert!(matches!(
            classify_step_failure(false, &p, "187"),
            StepFailure::EncoderInit(_)
        ));
    }

    #[test]
    fn test_classify_step_failure_real_output_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("real.mkv");
        // 8 KiB — above the 4 KiB threshold, suggests real data
        std::fs::write(&p, [0u8; 8192]).unwrap();
        assert!(matches!(
            classify_step_failure(false, &p, "1"),
            StepFailure::Fatal(_)
        ));
    }

    #[test]
    fn test_classify_step_failure_produced_output_trumps_no_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("missing_but_produced.mkv");
        assert!(matches!(
            classify_step_failure(true, &p, "1"),
            StepFailure::Fatal(_)
        ));
    }

    #[test]
    fn regress_nvenc_progress_end_is_retryable() {
        // Reproduce the exact stderr lines from the user's failing
        // av1_nvenc run: out_time=N/A, progress=end, 0-byte output.
        // Without the fix this was misclassified as Fatal.
        let lines = &[
            "frame=    0 fps=0.0 q=0.0 Lsize=       0KiB time=N/A bitrate=N/A speed=N/A",
            "out_time=N/A",
            "out_time_us=N/A",
            "progress=end",
            "Conversion failed!",
        ];
        let mut produced = false;
        for line in lines {
            if let Some(secs) = parse_out_time(line) {
                if secs > 0.0 {
                    produced = true;
                }
            }
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("failed.mkv");
        std::fs::write(&p, b"").unwrap();
        assert!(!produced, "no out_time with value > 0 should have been parsed");
        assert!(matches!(
            classify_step_failure(produced, &p, "187"),
            StepFailure::EncoderInit(_)
        ));
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
            hw: HwDeviceCapabilities::default(),
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
            hw: HwDeviceCapabilities::default(),
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
            hw: HwDeviceCapabilities::default(),
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
        let mut fb = EncoderFallback::new_with_hw(vec!["av1_nvenc".into(), "libsvtav1".into(), "libaom-av1".into()], HwDeviceContext::default());
        assert_eq!(fb.remaining(), vec!["av1_nvenc", "libsvtav1", "libaom-av1"]);
        fb.note_failure("av1_nvenc");
        assert_eq!(fb.remaining(), vec!["libsvtav1", "libaom-av1"]);
        fb.note_failure("libsvtav1");
        assert_eq!(fb.remaining(), vec!["libaom-av1"]);
    }

    #[test]
    fn test_encoder_fallback_pins_resolved_encoder() {
        let mut fb = EncoderFallback::new_with_hw(vec!["av1_nvenc".into(), "libsvtav1".into()], HwDeviceContext::default());
        fb.note_success("av1_nvenc");
        assert_eq!(fb.remaining(), vec!["av1_nvenc"], "pinned encoder is reused");
        assert_eq!(fb.resolved(), Some("av1_nvenc"));
    }

    #[test]
    fn test_encoder_fallback_exhausted_chain() {
        let mut fb = EncoderFallback::new_with_hw(vec!["av1_nvenc".into()], HwDeviceContext::default());
        fb.note_failure("av1_nvenc");
        assert!(fb.remaining().is_empty());
        assert_eq!(fb.resolved(), None);
    }

    #[test]
    fn test_resolve_device_for_candidate_vaapi_available() {
        let ctx = HwDeviceContext {
            vaapi_device: Some("/dev/dri/renderD128".to_string()),
            vulkan_available: false,
        };
        let hw = resolve_device_for_candidate("h264_vaapi", &ctx).unwrap();
        assert!(matches!(hw, ResolvedHwDevice::Vaapi { device_path } if device_path == "/dev/dri/renderD128"));
    }

    #[test]
    fn test_resolve_device_for_candidate_vaapi_unavailable() {
        let ctx = HwDeviceContext {
            vaapi_device: None,
            vulkan_available: false,
        };
        assert!(resolve_device_for_candidate("h264_vaapi", &ctx).is_none());
    }

    #[test]
    fn test_resolve_device_for_candidate_vulkan_available() {
        let ctx = HwDeviceContext {
            vaapi_device: None,
            vulkan_available: true,
        };
        let hw = resolve_device_for_candidate("h264_vulkan", &ctx).unwrap();
        assert!(matches!(hw, ResolvedHwDevice::Vulkan));
    }

    #[test]
    fn test_resolve_device_for_candidate_vulkan_unavailable() {
        let ctx = HwDeviceContext {
            vaapi_device: None,
            vulkan_available: false,
        };
        assert!(resolve_device_for_candidate("h264_vulkan", &ctx).is_none());
    }

    #[test]
    fn test_resolve_device_for_candidate_software_encoder() {
        let ctx = HwDeviceContext {
            vaapi_device: Some("/dev/dri/renderD128".to_string()),
            vulkan_available: true,
        };
        // Software encoders return None regardless of available devices.
        assert!(resolve_device_for_candidate("libx264", &ctx).is_none());
        assert!(resolve_device_for_candidate("av1_nvenc", &ctx).is_none());
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

    // ── hw-device prelude (push_hw_device_prelude) ────────────────────────

    #[test]
    fn test_push_hw_device_prelude_vaapi() {
        let mut s = make_video_settings();
        s.resolved_hw_device = Some(ResolvedHwDevice::Vaapi {
            device_path: "/dev/dri/renderD128".to_string(),
        });
        let mut args = vec!["-y".to_string()];
        push_hw_device_prelude(&mut args, &s);
        assert!(args.contains(&"-init_hw_device".to_string()));
        let init = args.iter().position(|a| a == "-init_hw_device").unwrap();
        assert_eq!(args[init + 1], "vaapi=vaapi0:/dev/dri/renderD128");
        let filter = args.iter().position(|a| a == "-filter_hw_device").unwrap();
        assert_eq!(args[filter + 1], "vaapi0");
        // Prelude comes after -y, before any -i
        assert_eq!(args[0], "-y");
    }

    #[test]
    fn test_push_hw_device_prelude_vulkan() {
        let mut s = make_video_settings();
        s.resolved_hw_device = Some(ResolvedHwDevice::Vulkan);
        let mut args = vec!["-y".to_string()];
        push_hw_device_prelude(&mut args, &s);
        let init = args.iter().position(|a| a == "-init_hw_device").unwrap();
        assert_eq!(args[init + 1], "vulkan=vulkan0");
        let filter = args.iter().position(|a| a == "-filter_hw_device").unwrap();
        assert_eq!(args[filter + 1], "vulkan0");
    }

    #[test]
    fn test_push_hw_device_prelude_copy_mode_noop() {
        let mut s = make_video_settings();
        s.copy_video = true;
        s.resolved_hw_device = Some(ResolvedHwDevice::Vaapi {
            device_path: "/dev/dri/renderD128".to_string(),
        });
        let mut args = vec!["-y".to_string()];
        push_hw_device_prelude(&mut args, &s);
        // Only -y, nothing added
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn test_push_hw_device_prelude_none_noop() {
        let mut s = make_video_settings();
        s.resolved_hw_device = None;
        let mut args = vec!["-y".to_string()];
        push_hw_device_prelude(&mut args, &s);
        assert_eq!(args.len(), 1);
    }

    #[test]
    fn test_push_video_encoder_hwupload_for_hw_frames_candidate() {
        let mut s = make_video_settings();
        s.video_encoder = "h264".to_string();
        s.resolved_video_encoder = "h264_vaapi".to_string();
        let mut args = Vec::new();
        push_video_encoder(&mut args, &s);
        let cv = args.iter().position(|a| a == "-c:v").unwrap();
        assert_eq!(args[cv + 1], "h264_vaapi");
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert_eq!(args[vf + 1], "format=nv12,hwupload");
    }

    #[test]
    fn test_push_video_encoder_no_hwupload_for_software_candidate() {
        let mut s = make_video_settings();
        s.video_encoder = "h264".to_string();
        s.resolved_video_encoder = "libx264".to_string();
        let mut args = Vec::new();
        push_video_encoder(&mut args, &s);
        assert!(args.contains(&"-c:v".to_string()));
        assert!(!args.contains(&"-vf".to_string()));
    }

    #[test]
    fn test_push_video_encoder_hwupload_for_vulkan() {
        let mut s = make_video_settings();
        s.video_encoder = "av1".to_string();
        s.resolved_video_encoder = "av1_vulkan".to_string();
        let mut args = Vec::new();
        push_video_encoder(&mut args, &s);
        let vf = args.iter().position(|a| a == "-vf").unwrap();
        assert_eq!(args[vf + 1], "format=nv12,hwupload");
    }

    #[test]
    fn test_build_video_only_args_prelude_before_input() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("test.mp4");
        std::fs::write(&input, b"dummy").unwrap();
        let mut s = make_video_settings();
        s.input_files = vec![input.clone()];
        s.resolved_video_encoder = "h264_vaapi".to_string();
        s.resolved_hw_device = Some(ResolvedHwDevice::Vaapi {
            device_path: "/dev/dri/renderD128".to_string(),
        });
        // Use the builder that actually processes the input
        let args = build_video_only_args(&s, 0);
        let y_pos = args.iter().position(|a| a == "-y").unwrap();
        let init_pos = args.iter().position(|a| a == "-init_hw_device").unwrap();
        let i_pos = args.iter().position(|a| a == "-i").unwrap();
        // -init_hw_device comes after -y but before -i
        assert!(y_pos < init_pos, "-y must come before -init_hw_device");
        assert!(init_pos < i_pos, "-init_hw_device must come before -i");
    }

    #[test]
    fn test_build_video_mux_args_prelude_before_input() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("test.mp4");
        std::fs::write(&input, b"dummy").unwrap();
        let mut s = make_video_settings();
        s.input_files = vec![input.clone()];
        s.resolved_video_encoder = "h264_vaapi".to_string();
        s.resolved_hw_device = Some(ResolvedHwDevice::Vaapi {
            device_path: "/dev/dri/renderD128".to_string(),
        });
        // Minimal probe: empty streams since we keep AllAudio
        let probe = VideoAudioProbe {
            total_audio_channels: 1,
            is_video_file: true,
            streams: vec![],
        };
        let args = build_video_mux_args(&s, 0, &AudioKeep::AllAudio, &probe);
        let y_pos = args.iter().position(|a| a == "-y").unwrap();
        let init_pos = args.iter().position(|a| a == "-init_hw_device").unwrap();
        let i_pos = args.iter().position(|a| a == "-i").unwrap();
        assert!(y_pos < init_pos, "-y before -init_hw_device");
        assert!(init_pos < i_pos, "-init_hw_device before -i");
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
            copy_video: false,
            video_encoder: "h264".to_string(),
            audio_encoder: "pcm_s24le".to_string(),
            resolved_video_encoder: String::new(),
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "output".to_string(),
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
            naming_mode: OutputNamingMode::PrefixTemplates,
            trim_to_first_ltc: false,
            trim_offsets_secs: vec![0.0],
            timecode_meta_per_file: vec![None],
            concat_audio: false,
            resolved_hw_device: None,
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

    // ── per-file planner tests ───────────────────────────────────────────

    #[test]
    fn test_plan_video_outputs_for_file_no_duplicates() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip_A.mov"),
            PathBuf::from("/tmp/clip_B.mov"),
        ];
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        let probe = make_stereo_probe();

        // Per-file → file 0: VideoOnly + 1×AudioChannel
        let steps_0 = plan_video_outputs_for_file(&s, 0, &probe);
        assert_eq!(steps_0.len(), 2, "file 0: video + 1 audio");
        assert!(matches!(steps_0[0], VideoOutputStep::VideoOnly { file_idx: 0, .. }));
        assert!(matches!(steps_0[1], VideoOutputStep::AudioChannel { file_idx: 0, channel_idx: 1, .. }));

        // Per-file → file 1: VideoOnly + 1×AudioChannel (no carries from file 0)
        let steps_1 = plan_video_outputs_for_file(&s, 1, &probe);
        assert_eq!(steps_1.len(), 2, "file 1: video + 1 audio");
        assert!(matches!(steps_1[0], VideoOutputStep::VideoOnly { file_idx: 1, .. }));
        assert!(matches!(steps_1[1], VideoOutputStep::AudioChannel { file_idx: 1, channel_idx: 1, .. }));

        // Combined via wrapper: 4 total, no duplicates
        let combined = plan_video_outputs(&s, &probe);
        assert_eq!(combined.len(), 4, "2 files × 2 steps = 4");
        assert_eq!(combined.iter().filter(|s| matches!(s, VideoOutputStep::VideoOnly { .. })).count(), 2);
        assert_eq!(combined.iter().filter(|s| matches!(s, VideoOutputStep::AudioChannel { .. })).count(), 2);
    }

    #[test]
    fn test_plan_per_file_audio_numbering() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip_A.mov"),
            PathBuf::from("/tmp/clip_B.mov"),
        ];
        s.split_tracks = true;
        s.naming_mode = OutputNamingMode::SourceStems;
        let probe = make_stereo_probe();

        // No LTC drop → both channels survive per file.
        // Per-file numbering: each clip gets _audio_track1 and _audio_track2.
        let steps_0 = plan_video_outputs_for_file(&s, 0, &probe);
        let steps_1 = plan_video_outputs_for_file(&s, 1, &probe);
        // Audio step output names: clip_A_audio_track1.wav / clip_A_audio_track2.wav
        // and clip_B_audio_track1.wav / clip_B_audio_track2.wav
        if let VideoOutputStep::AudioChannel { output, .. } = &steps_0[1] {
            let name = output.file_stem().and_then(|n| n.to_str()).unwrap_or("");
            assert!(name.contains("_audio_track1"), "file 0 first audio = track1, got: {}", name);
        }
        if let VideoOutputStep::AudioChannel { output, .. } = &steps_0[2] {
            let name = output.file_stem().and_then(|n| n.to_str()).unwrap_or("");
            assert!(name.contains("_audio_track2"), "file 0 second audio = track2, got: {}", name);
        }

        // File 1 also numbers from track1 (per-file, not global).
        assert_eq!(steps_1.len(), 3, "file 1: 1 video + 2 audio = 3");
        if let VideoOutputStep::AudioChannel { output, .. } = &steps_1[1] {
            let name = output.file_stem().and_then(|n| n.to_str()).unwrap_or("");
            assert!(name.contains("_audio_track1"), "file 1 first audio = track1, got: {}", name);
        }
    }

    // ── preview_output_files tests ──────────────────────────────────────

    #[test]
    fn test_preview_video_split_stereo_drop_ltc() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip_A.mov"),
            PathBuf::from("/tmp/clip_B.mov"),
        ];
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        s.naming_mode = OutputNamingMode::SourceStems;
        let probe = make_stereo_probe();

        let previews = preview_output_files(&s, Some(&probe));
        // 2 files × (1 video + 1 audio) = 4
        assert_eq!(previews.len(), 4, "expected 4 preview outputs (2 video + 2 audio)");

        let video_count = previews.iter().filter(|p| p.kind == OutputKind::Video).count();
        let audio_count = previews.iter().filter(|p| p.kind == OutputKind::Audio).count();
        assert_eq!(video_count, 2, "2 video outputs");
        assert_eq!(audio_count, 2, "2 audio outputs (one per file)");
    }

    #[test]
    fn test_preview_video_split_no_drop() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip_A.mov"),
            PathBuf::from("/tmp/clip_B.mov"),
        ];
        s.split_tracks = true;
        s.naming_mode = OutputNamingMode::SourceStems;
        let probe = make_stereo_probe();

        let previews = preview_output_files(&s, Some(&probe));
        // 2 files × (1 video + 2 audio) = 6
        assert_eq!(previews.len(), 6);
    }

    #[test]
    fn test_preview_video_no_split() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip_A.mov"),
            PathBuf::from("/tmp/clip_B.mov"),
        ];
        let probe = make_stereo_probe();

        let previews = preview_output_files(&s, Some(&probe));
        // 2 video mux files (1 per input, no split)
        assert_eq!(previews.len(), 2);
        assert!(previews.iter().all(|p| p.kind == OutputKind::Video));
    }

    #[test]
    fn test_preview_video_no_probe_fallback() {
        let s = make_video_settings();
        let previews = preview_output_files(&s, None);
        // No probe → video-only fallback: 1 video file
        assert_eq!(previews.len(), 1);
        assert!(matches!(previews[0].kind, OutputKind::Video));
    }

    #[test]
    fn test_preview_copy_mode_container() {
        let mut s = make_video_settings();
        s.input_files = vec![PathBuf::from("/tmp/clip.mov")];
        s.copy_video = true;
        s.naming_mode = OutputNamingMode::SourceStems;
        let probe = make_stereo_probe();

        let previews = preview_output_files(&s, Some(&probe));
        assert!(!previews.is_empty());
        let name = previews[0].path.to_str().unwrap_or("");
        // copy-mode container derived from .mov input = "mov"
        assert!(name.ends_with(".mov"), "expected .mov in copy mode, got: {}", name);
    }

    #[test]
    fn test_preview_concat_audio() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip_A.mov"),
            PathBuf::from("/tmp/clip_B.mov"),
        ];
        s.split_tracks = true;
        s.concat_audio = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        let probe = make_stereo_probe();

        let previews = preview_output_files(&s, Some(&probe));
        // 2 video files + 1 concatenated audio track
        assert!(previews.len() >= 3, "concat: 2 video + 1 audio, got {}", previews.len());
        let audio_count = previews.iter().filter(|p| p.kind == OutputKind::Audio).count();
        assert_eq!(audio_count, 1, "concat → 1 audio output");
    }

    #[test]
    fn test_preview_audio_only_split() {
        let mut s = make_settings_audio_only();
        s.split_tracks = true;
        s.drop_ltc_track = false;

        let previews = preview_output_files(&s, None);
        // 2 audio tracks, no video
        assert_eq!(previews.len(), 2);
        assert!(previews.iter().all(|p| p.kind == OutputKind::Audio));
    }

    #[test]
    fn test_preview_audio_only_split_drop_ltc() {
        let mut s = make_settings_audio_only();
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_track_channel_index = 1;

        let previews = preview_output_files(&s, None);
        // 2 channels, drop channel 1 → 1 audio output
        assert_eq!(previews.len(), 1);
        assert!(matches!(previews[0].kind, OutputKind::Audio));
    }

    #[test]
    fn test_preview_audio_only_synthetic_video() {
        let mut s = make_settings_audio_only();
        s.pipeline = ConversionPipeline::AudioOnly { generate_synthetic_video: true };
        s.split_tracks = false;

        let previews = preview_output_files(&s, None);
        // 1 audio + 1 synthetic video
        assert_eq!(previews.len(), 2, "expected audio + synthetic video, got {}", previews.len());
        let audio_count = previews.iter().filter(|p| p.kind == OutputKind::Audio).count();
        let video_count = previews.iter().filter(|p| p.kind == OutputKind::Video).count();
        assert_eq!(audio_count, 1);
        assert_eq!(video_count, 1);
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

    // ── Stream-copy mode ("Leave Video Encoding Untouched") ─────────────

    fn make_copy_settings() -> ConverterSettings {
        let mut s = make_video_settings();
        s.copy_video = true;
        s
    }

    fn codec_after_flag<'a>(args: &'a [String], flag: &str) -> Option<&'a String> {
        args.iter()
            .position(|a| a == flag)
            .and_then(|i| args.get(i + 1))
    }

    #[test]
    fn test_copy_mode_container_for_input() {
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.mp4")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.m4v")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/CLIP.MOV")), "mov");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.mkv")), "mkv");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.MXF")), "mxf");
        // MPEG-TS based recordings remux into MP4 (keeps -timecode support)
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.mts")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.M2TS")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.ts")), "mp4");
        // Everything else lands in Matroska
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.avi")), "mkv");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.webm")), "mkv");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/noext")), "mkv");
    }

    #[test]
    fn test_shift_timecode_back_ndf() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
        // 1.5 s at 25 fps = 37.5 frames → rounds to 38
        let out = shift_timecode_back(&tc, 1.5, 25.0, false);
        assert_eq!(format_ffmpeg_timecode(&out, false), "00:59:58:12");
    }

    #[test]
    fn test_shift_timecode_back_zero_is_identity() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 12 };
        let out = shift_timecode_back(&tc, 0.0, 25.0, false);
        assert_eq!(out, tc);
        let out = shift_timecode_back(&tc, -3.0, 25.0, false);
        assert_eq!(out, tc);
    }

    #[test]
    fn test_shift_timecode_back_drop_frame_skips_nonexistent() {
        // 29.97 DF: minute 1 starts at frame 2 — frames 0/1 don't exist.
        // Shifting back 2 frames from 01:01:00;02 must land on 01:00:59;29,
        // never on the nonexistent 01:01:00;00.
        let tc = Timecode { hours: 1, minutes: 1, seconds: 0, frames: 2 };
        let out = shift_timecode_back(&tc, 2.0 / 29.97, 29.97, true);
        assert_eq!(format_ffmpeg_timecode(&out, true), "01:00:59;29");
    }

    #[test]
    fn test_shift_timecode_back_drop_frame_minute_start() {
        // 01:00:00;00 minus one frame → 00:59:59;29 (minute 0 keeps frame 0)
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
        let out = shift_timecode_back(&tc, 1.0 / 29.97, 29.97, true);
        assert_eq!(format_ffmpeg_timecode(&out, true), "00:59:59;29");
    }

    #[test]
    fn test_shift_timecode_back_wraps_midnight() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let out = shift_timecode_back(&tc, 1.0 / 25.0, 25.0, false);
        assert_eq!(format_ffmpeg_timecode(&out, false), "23:59:59:24");
    }

    #[test]
    fn test_shift_timecode_back_roundtrip_with_increment() {
        // Decrement must be the exact inverse of audio_core::increment_timecode
        let mut tc = Timecode { hours: 12, minutes: 34, seconds: 56, frames: 7 };
        for _ in 0..200 {
            tc = audio_core::increment_timecode(&tc, 25.0, false);
        }
        let out = shift_timecode_back(&tc, 200.0 / 25.0, 25.0, false);
        assert_eq!(out, Timecode { hours: 12, minutes: 34, seconds: 56, frames: 7 });
    }

    #[test]
    fn test_build_video_only_args_copy_mode() {
        let s = make_copy_settings();
        let args = build_video_only_args(&s, 0);
        assert_eq!(codec_after_flag(&args, "-c:v"), Some(&"copy".to_string()),
            "copy mode must stream-copy the video, args: {:?}", args);
        assert!(!args.contains(&"-r".to_string()),
            "output -r must not be forced on a copied stream");
        // Timecode metadata is still written in copy mode
        let s2 = make_copy_settings();
        let mut s2 = s2;
        s2.timecode_meta_per_file[0] = Some(TimecodeMetadata {
            start: Timecode { hours: 9, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        });
        let args = build_video_only_args(&s2, 0);
        assert_eq!(codec_after_flag(&args, "-c:v"), Some(&"copy".to_string()));
        let tc_pos = args.iter().position(|a| a == "-timecode").expect("timecode kept in copy mode");
        assert_eq!(args[tc_pos + 1], "09:00:00:00");
        assert!(args.contains(&"-write_tmcd".to_string()));
        assert!(!args.contains(&"-r".to_string()));
        // No encoder-specific pixel format args
        assert!(!args.contains(&"yuv420p".to_string()));
    }

    #[test]
    fn test_build_video_mux_args_copy_mode_all_audio_copies_audio() {
        let s = make_copy_settings();
        let probe = make_stereo_probe();
        let args = build_video_mux_args(&s, 0, &AudioKeep::AllAudio, &probe);
        assert_eq!(codec_after_flag(&args, "-c:v"), Some(&"copy".to_string()));
        assert_eq!(codec_after_flag(&args, "-c:a"), Some(&"copy".to_string()),
            "unfiltered audio must be stream-copied too, args: {:?}", args);
        assert!(!args.contains(&"-r".to_string()));
    }

    #[test]
    fn test_build_video_mux_args_copy_mode_drop_channel_reencodes_audio_only() {
        let mut s = make_copy_settings();
        s.audio_encoder = "pcm_s24le".to_string();
        let probe = make_stereo_probe();
        let keep = AudioKeep::ChannelsExcept(vec![(1, 0)]);
        let args = build_video_mux_args(&s, 0, &keep, &probe);
        assert_eq!(codec_after_flag(&args, "-c:v"), Some(&"copy".to_string()),
            "video must stay copied even when audio is filtered");
        assert_eq!(codec_after_flag(&args, "-c:a"), Some(&"pcm_s24le".to_string()),
            "channel filtering requires re-encoding only the audio");
        assert!(args.contains(&"-filter_complex".to_string()));
    }

    #[test]
    fn test_build_video_only_args_encode_mode_unchanged() {
        // Regression: without copy_video the encoder path is untouched
        let mut s = make_video_settings();
        s.resolved_video_encoder = "libx264".to_string();
        let args = build_video_only_args(&s, 0);
        assert_eq!(codec_after_flag(&args, "-c:v"), Some(&"libx264".to_string()));
        assert!(args.contains(&"-pix_fmt".to_string()));
    }

    #[test]
    fn test_sanity_check_copy_mode_skips_video_codec_validation() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("in.mp4");
        std::fs::write(&input, b"fake").unwrap();
        // caps with no video encoders at all — encode mode would fail
        let caps = make_caps(true, BTreeSet::from(["pcm_s24le"]), BTreeSet::from(["mp4"]));

        // Copy mode: unknown codec id is irrelevant, conversion is valid
        assert!(conversion_sanity_check(
            "mp4", "weird-codec", "pcm_s24le",
            std::slice::from_ref(&input), tmp.path(), "out", &caps, None, None,
        ).is_err(), "encode mode with unknown codec must fail");

        assert!(conversion_sanity_check_copy(
            "mp4", "weird-codec", "pcm_s24le",
            std::slice::from_ref(&input), tmp.path(), "out", &caps, None, None,
        ).is_ok(), "copy mode must not validate the video codec");

        // Copy mode still validates the container against ffmpeg
        assert!(conversion_sanity_check_copy(
            "webp", "weird-codec", "pcm_s24le",
            std::slice::from_ref(&input), tmp.path(), "out", &caps, None, None,
        ).is_err(), "copy mode must still reject unavailable containers");
    }

    // ── Output naming mode tests ──────────────────────────────────────────

    #[test]
    fn test_output_base_for_file_prefix_mode() {
        let s = ConverterSettings {
            filename_prefix: "session1".to_string(),
            naming_mode: OutputNamingMode::PrefixTemplates,
            input_files: vec![
                PathBuf::from("/tmp/C0001.MP4"),
                PathBuf::from("/tmp/C0002.MP4"),
            ],
            ..make_settings_audio_only()
        };
        assert_eq!(s.output_base_for_file(0), "session1");
        assert_eq!(s.output_base_for_file(1), "session1");
        assert_eq!(s.output_base_for_file(99), "session1");
    }

    #[test]
    fn test_output_base_for_file_source_stems_mode() {
        let s = ConverterSettings {
            filename_prefix: "session1".to_string(),
            naming_mode: OutputNamingMode::SourceStems,
            input_files: vec![
                PathBuf::from("/tmp/C0001.MP4"),
                PathBuf::from("/tmp/C0002.MP4"),
                PathBuf::from("/tmp/C0003.MP4"),
            ],
            ..make_settings_audio_only()
        };
        assert_eq!(s.output_base_for_file(0), "C0001");
        assert_eq!(s.output_base_for_file(1), "C0002");
        assert_eq!(s.output_base_for_file(2), "C0003");
    }

    #[test]
    fn test_output_path_for_file_source_stems_naming() {
        let s = ConverterSettings {
            filename_prefix: "ignored".to_string(),
            naming_mode: OutputNamingMode::SourceStems,
            input_files: vec![
                PathBuf::from("/tmp/C0001.MP4"),
                PathBuf::from("/tmp/C0002.MP4"),
            ],
            output_folder: PathBuf::from("/out"),
            container: "mkv".to_string(),
            video_suffix_template: "_clip{:02d}".to_string(),
            ..make_settings_audio_only()
        };
        let ext = "mkv";
        let p0 = s.output_path_for_file("video", 0, 1, ext);
        assert!(p0.to_string_lossy().ends_with("C0001_clip01.mkv"),
            "expected C0001-clip01, got {}", p0.display());
        let p1 = s.output_path_for_file("video", 1, 2, ext);
        assert!(p1.to_string_lossy().ends_with("C0002_clip02.mkv"),
            "expected C0002-clip02, got {}", p1.display());
    }

    #[test]
    fn test_output_path_for_file_prefix_mode_unchanged() {
        // PrefixTemplates mode must produce the same name as before
        let s = ConverterSettings {
            filename_prefix: "session".to_string(),
            naming_mode: OutputNamingMode::PrefixTemplates,
            input_files: vec![PathBuf::from("/tmp/C0001.MP4")],
            output_folder: PathBuf::from("/out"),
            container: "mkv".to_string(),
            video_suffix_template: "_video_clip{:02d}".to_string(),
            ..make_settings_audio_only()
        };
        let p0 = s.output_path_for_file("video", 0, 1, "mkv");
        assert!(p0.to_string_lossy().ends_with("session_video_clip01.mkv"),
            "expected session_video_clip01.mkv, got {}", p0.display());
    }

    #[test]
    fn test_output_path_collision_fallback() {
        // Input file collides with computed output path when suffix is empty
        // in source-stems mode and output folder == input folder.
        let dir = tempfile::tempdir().unwrap();
        let input_path = dir.path().join("C0001.mp4");
        std::fs::write(&input_path, b"dummy").unwrap();
        let out_dir = input_path.parent().unwrap().to_path_buf();

        let s = ConverterSettings {
            filename_prefix: String::new(),
            naming_mode: OutputNamingMode::SourceStems,
            input_files: vec![input_path.clone()],
            output_folder: out_dir,
            container: "mp4".to_string(),
            video_suffix_template: String::new(),
            audio_suffix_template: String::new(),
            ..make_settings_audio_only()
        };
        let path = s.output_path_for_file("video", 0, 1, "mp4");
        let name = path.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.contains("_conv"), "expected _conv suffix, got {}", name);
    }

    #[test]
    fn test_validate_suffix_template_source_stems_relaxed() {
        // In SourceStems mode, a template without placeholders is valid
        assert!(ConverterSettings::validate_suffix_template_for_mode("_sync", &OutputNamingMode::SourceStems).is_ok());
        assert!(ConverterSettings::validate_suffix_template_for_mode("", &OutputNamingMode::SourceStems).is_ok());
        // In PrefixTemplates mode, no-placeholder templates should still fail
        assert!(ConverterSettings::validate_suffix_template_for_mode("_sync", &OutputNamingMode::PrefixTemplates).is_err());
    }

    #[test]
    fn test_plan_video_mux_source_stems_naming() {
        let mut s = make_video_settings();
        s.naming_mode = OutputNamingMode::SourceStems;
        s.input_files = vec![
            PathBuf::from("/tmp/C0001.MP4"),
            PathBuf::from("/tmp/C0002.MP4"),
        ];
        let probe = make_stereo_probe();
        let steps = plan_video_outputs(&s, &probe);
        assert_eq!(steps.len(), 2, "2 clips → 2 mux steps");
        if let VideoOutputStep::VideoMux { file_idx, output, .. } = &steps[0] {
            assert_eq!(*file_idx, 0);
            assert!(output.to_string_lossy().contains("C0001"), "step 0 named after C0001: {}", output.display());
        } else {
            panic!("expected VideoMux step 0");
        }
        if let VideoOutputStep::VideoMux { file_idx, output, .. } = &steps[1] {
            assert_eq!(*file_idx, 1);
            assert!(output.to_string_lossy().contains("C0002"), "step 1 named after C0002: {}", output.display());
        } else {
            panic!("expected VideoMux step 1");
        }
    }

    #[test]
    fn test_plan_video_split_source_stems_naming() {
        let mut s = make_video_settings();
        s.naming_mode = OutputNamingMode::SourceStems;
        s.split_tracks = true;
        s.input_files = vec![
            PathBuf::from("/tmp/C0001.MP4"),
            PathBuf::from("/tmp/C0002.MP4"),
        ];
        let probe = make_stereo_probe();
        let steps = plan_video_outputs(&s, &probe);
        // 2 clips × (1 VideoOnly + 2 AudioChannel) = 6 steps
        assert_eq!(steps.len(), 6);
        // Step 0: VideoOnly for C0001
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { file_idx: 0, .. }));
        assert!(steps[0].output().to_string_lossy().contains("C0001"), "video-only 0 stems from C0001");
        // Step 3: VideoOnly for C0002
        assert!(matches!(steps[3], VideoOutputStep::VideoOnly { file_idx: 1, .. }));
        assert!(steps[3].output().to_string_lossy().contains("C0002"), "video-only 1 stems from C0002");
        // Audio channels should also use per-file stems
        assert!(steps[1].output().to_string_lossy().contains("C0001"), "audio 0 stems from C0001");
        assert!(steps[4].output().to_string_lossy().contains("C0002"), "audio 3 stems from C0002");
    }

    // ── Concat planner tests ──────────────────────────────────────────────

    #[test]
    fn test_plan_concat_stereo_no_drop() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.concat_audio = true;
        s.input_files = vec![
            PathBuf::from("/tmp/C0001.MP4"),
            PathBuf::from("/tmp/C0002.MP4"),
        ];
        let probe = make_stereo_probe();
        let all_probes = vec![Some(probe.clone()), Some(probe)];
        let (steps, warning) = plan_concat_outputs(&s, &all_probes);
        assert!(warning.is_empty(), "expected no warning, got: {}", warning);
        // 2 concat steps for 2 tracks (ch0, ch1), no video steps here
        assert_eq!(steps.len(), 2);
        for (i, step) in steps.iter().enumerate() {
            match step {
                VideoOutputStep::AudioChannelConcat { segments, output, .. } => {
                    assert_eq!(segments.len(), 2, "track {} should have 2 segments", i);
                    // Each segment references the correct file index
                    assert_eq!(segments[0].0, 0);
                    assert_eq!(segments[1].0, 1);
                    assert!(output.to_string_lossy().contains(&format!("audio_track{}", i + 1)),
                        "output should have track {}: {}", i + 1, output.display());
                }
                other => panic!("expected AudioChannelConcat, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_plan_concat_drop_ltc_track() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.concat_audio = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0)); // drop channel 0 (stream 1, ch 0)
        s.input_files = vec![
            PathBuf::from("/tmp/C0001.MP4"),
            PathBuf::from("/tmp/C0002.MP4"),
        ];
        let probe = make_stereo_probe();
        let all_probes = vec![Some(probe.clone()), Some(probe)];
        let (steps, _warning) = plan_concat_outputs(&s, &all_probes);
        // Only 1 surviving channel (ch1), so 1 concat step
        assert_eq!(steps.len(), 1);
        match &steps[0] {
            VideoOutputStep::AudioChannelConcat { segments, .. } => {
                assert_eq!(segments.len(), 2);
                // Both segments should reference channel_idx == 1
                assert_eq!(segments[0].2, 1);
                assert_eq!(segments[1].2, 1);
            }
            other => panic!("expected AudioChannelConcat, got {:?}", other),
        }
    }

    #[test]
    fn test_plan_concat_inconsistent_layout_fallback() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.concat_audio = true;
        s.input_files = vec![
            PathBuf::from("/tmp/C0001.MP4"),
            PathBuf::from("/tmp/C0002.MP4"),
        ];
        let stereo = make_stereo_probe();
        let mono = make_mono_probe();
        let all_probes = vec![Some(stereo), Some(mono)];
        let (steps, warning) = plan_concat_outputs(&s, &all_probes);
        // Should fall back to per-clip AudioChannel steps
        assert!(!warning.is_empty(), "expected fallback warning");
        assert!(warning.contains("falling back"), "warning should mention fallback");
        for step in &steps {
            assert!(
                matches!(step, VideoOutputStep::AudioChannel { .. }),
                "expected AudioChannel fallback, got {:?}", step
            );
        }
    }

    // ── Concat arg builder tests ───────────────────────────────────────────

    #[test]
    fn test_build_concat_audio_args_basic() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/C0001.MP4"),
            PathBuf::from("/tmp/C0002.MP4"),
        ];
        s.trim_offsets_secs = vec![1.5, 2.0];
        let segments = vec![(0, 1, 0), (1, 1, 0)];
        let args = build_concat_audio_args(&s, &segments, "wav", 48000);
        // Should have two input files with -ss
        let input_positions: Vec<usize> = args.iter().enumerate()
            .filter(|(_, a)| a.contains("-i"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(input_positions.len(), 2, "should have 2 -i inputs");
        // Should have -ss before each input
        assert!(args.contains(&"-ss".to_string()), "should have at least one -ss");
        let ss_positions: Vec<usize> = args.iter().enumerate()
            .filter(|(_, a)| *a == "-ss")
            .map(|(i, _)| i)
            .collect();
        assert_eq!(ss_positions.len(), 2, "should have 2 -ss flags");
        assert!(
            args[ss_positions[0] + 1].contains("1.5"),
            "first -ss should be 1.5, got: {}", args[ss_positions[0] + 1]
        );
        assert!(
            args[ss_positions[1] + 1].contains("2.0"),
            "second -ss should be 2.0, got: {}", args[ss_positions[1] + 1]
        );
        // Should have -filter_complex with pan and concat
        let fc_pos = args.iter().position(|a| a == "-filter_complex").expect("should have -filter_complex");
        let fc = &args[fc_pos + 1];
        assert!(fc.contains("pan=mono|FC=c0"), "should have pan for ch0");
        assert!(fc.contains("[0:1]"), "should reference first input's stream 1");
        assert!(fc.contains("[1:1]"), "should reference second input's stream 1");
        assert!(fc.contains("concat=n=2:v=0:a=1"), "should concat 2 segments");
        assert!(fc.contains("[out]"), "filter_complex should produce [out]");
        // Should have -map [out]
        let map_pos = args.iter().position(|a| a == "-map").expect("should have -map");
        assert_eq!(args[map_pos + 1], "[out]");
        assert!(args.contains(&"-vn".to_string()), "should have -vn for audio-only");
        assert!(args.contains(&"-c:a".to_string()), "should have -c:a");
        // Should have -f wav
        let f_pos = args.iter().position(|a| a == "-f").expect("should have -f");
        assert_eq!(args[f_pos + 1], "wav");
    }

    #[test]
    fn test_build_concat_audio_args_timecode() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/C0001.MP4"),
            PathBuf::from("/tmp/C0002.MP4"),
        ];
        s.timecode_meta_per_file[0] = Some(TimecodeMetadata {
            start: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        });
        let segments = vec![(0, 1, 0), (1, 1, 0)];
        let args = build_concat_audio_args(&s, &segments, "wav", 48000);
        // Should have timecode metadata from clip 0
        let tc_pos = args.iter().position(|a| a == "-timecode").expect("should have -timecode");
        assert_eq!(args[tc_pos + 1], "01:00:00:00");
        assert!(args.contains(&"-write_bext".to_string()), "WAV should have BWF");
    }

    // ── same-folder collision / output_collision_warning ─────────────────

    #[test]
    fn test_sanity_check_same_folder_source_stems_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("C0001.MP4");
        std::fs::write(&input, b"dummy").unwrap();
        let caps = make_caps(true, BTreeSet::from(["libx264", "pcm_s24le"]), BTreeSet::from(["matroska"]));
        let result = conversion_sanity_check_with_naming(
            "mkv", "h264", "pcm_s24le",
            &[input], tmp.path(), "prefix", &caps,
            Some("_audio_track{:01d}"), Some("_video_clip{:02d}"),
            Some(&OutputNamingMode::SourceStems), false,
        );
        assert!(result.is_ok(),
            "SourceStems + same folder + non-empty suffix should be Ok, got: {:?}",
            result.err());
    }

    #[test]
    fn test_collision_warning_none_when_suffix_differs() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("C0001.MP4");
        std::fs::write(&input, b"dummy").unwrap();
        let msg = output_collision_warning(
            &[input], tmp.path(), "prefix",
            &OutputNamingMode::SourceStems, "_video_clip{:02d}",
            "mkv", false,
        );
        assert!(msg.is_none(),
            "non-empty suffix should prevent collision warning, got: {:?}", msg);
    }

    #[test]
    fn test_collision_warning_some_on_true_collision() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("C0001.mp4");
        std::fs::write(&input, b"dummy").unwrap();
        let msg = output_collision_warning(
            std::slice::from_ref(&input), tmp.path(), "prefix",
            &OutputNamingMode::SourceStems, "",
            "mp4", true,
        );
        assert!(msg.is_some(), "empty suffix + same ext + copy mode should warn");
        let text = msg.unwrap();
        assert!(text.contains("_conv"), "message should mention _conv mitigation, got: {}", text);
        assert!(text.contains("C0001.mp4"), "message should name the colliding file, got: {}", text);
    }

    #[test]
    fn test_collision_warning_prefix_mode_no_false_positive() {
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("C0001.MP4");
        std::fs::write(&input, b"dummy").unwrap();
        let msg = output_collision_warning(
            &[input], tmp.path(), "out",
            &OutputNamingMode::PrefixTemplates, "_video_clip{:02d}",
            "mkv", false,
        );
        assert!(msg.is_none(),
            "PrefixTemplates with non-colliding prefix should be None, got: {:?}", msg);
    }

    #[test]
    fn test_collision_warning_copy_mode_ext_no_false() {
        // MTS input → copy mode maps ext to mp4; output name won't match .mts input
        let tmp = tempfile::tempdir().unwrap();
        let input = tmp.path().join("C0001.MTS");
        std::fs::write(&input, b"dummy").unwrap();
        let msg = output_collision_warning(
            &[input], tmp.path(), "prefix",
            &OutputNamingMode::SourceStems, "_video_clip{:02d}",
            "mkv", true,
        );
        assert!(msg.is_none(),
            "MTS in copy mode → planned ext is mp4, no collision, got: {:?}", msg);
    }
}