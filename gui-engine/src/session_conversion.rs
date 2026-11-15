//! Session default conversion (phase 4): persisted conversion defaults
//! profile, the channel-resolution decision core, and the preflight +
//! orchestration runners behind `JobKind::SessionPreflight` /
//! `JobKind::SessionConversion`.
//!
//! Owner decisions (2026-11-06 umbrella log, implemented here as constants
//! or defaults — not re-decided):
//! - The interactive [`crate::state::ConverterUserSettings`] keeps its
//!   per-recording reset behaviour and is **not** persisted; this module's
//!   [`SessionConversionDefaults`] is the separate persisted profile.
//! - Video pipeline default: full re-encode, split tracks, drop the LTC
//!   track, TC metadata from LTC, **no** trim.
//! - Audio pipeline default (TASCAM separate-file-per-track sets): the LTC
//!   file is the TC source **only** — it produces no output (excluded via
//!   `drop_ltc_track` + `ltc_track_channel_index`, the single home of that
//!   rule in `selected_channel_pairs`).
//! - Channel resolution: configured default → per-device pin → auto-scan
//!   remaining candidates; any file still failing after candidate
//!   exhaustion aborts the run with a prominent report.
//! - Encode failure mid-run: continue remaining groups, end in an honest
//!   partial state (the GUI renders it orange); never a silent success.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::converter::{
    ConversionPipeline, ConversionReport, ConverterSettings, RecordingType, TimecodeMetadata,
    DEFAULT_AUDIO_SUFFIX, DEFAULT_PREFIX, DEFAULT_VIDEO_SUFFIX,
};
use crate::job::{JobContext, JobError, UnitProgress};
use audio_core::{LtcDetectionResult, LtcDecodeStatus, Timecode};

// ── Persisted defaults profile ──────────────────────────────────────────

pub const SESSION_DEFAULTS_SCHEMA: &str = "ltc-session-defaults/v1";

const SESSION_DEFAULTS_FILE: &str = "session_defaults.json";

/// Video-pipeline defaults. Absent codec/container overrides fall back to
/// `select_best_combination()` from the live ffmpeg caps at run time — no
/// stale codec is ever pinned in the config file.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct VideoDefaults {
    pub container_override: Option<String>,
    pub video_codec_override: Option<String>,
    pub audio_encoder_override: Option<String>,
    pub split_tracks: bool,
    pub drop_ltc_track: bool,
    pub set_start_from_ltc: bool,
    pub embed_camera_metadata: bool,
    pub filename_prefix: String,
    pub audio_suffix_template: String,
    pub video_suffix_template: String,
}

impl Default for VideoDefaults {
    fn default() -> Self {
        Self {
            container_override: None,
            video_codec_override: None,
            audio_encoder_override: None,
            split_tracks: true,
            drop_ltc_track: true,
            set_start_from_ltc: true,
            embed_camera_metadata: true,
            filename_prefix: DEFAULT_PREFIX.to_string(),
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        }
    }
}

/// Audio-pipeline defaults. The owner's TASCAM rig carries LTC on track 2
/// (index 1) — hence the default.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AudioDefaults {
    pub ltc_file_idx: usize,
}

impl Default for AudioDefaults {
    fn default() -> Self {
        Self { ltc_file_idx: 1 }
    }
}

/// Channel defaults used as the first candidate in the resolution order
/// (configured → pinned → scan).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[derive(Default)]
pub struct ChannelDefaults {
    /// LTC channel within the audio group's designated LTC file.
    pub audio_ltc_channel_override: Option<usize>,
    pub video_ltc_stream: usize,
    pub video_ltc_channel: usize,
}


#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionConversionDefaults {
    pub schema: String,
    pub video: VideoDefaults,
    pub audio: AudioDefaults,
    pub channels: ChannelDefaults,
}

impl Default for SessionConversionDefaults {
    fn default() -> Self {
        Self {
            schema: SESSION_DEFAULTS_SCHEMA.to_string(),
            video: VideoDefaults::default(),
            audio: AudioDefaults::default(),
            channels: ChannelDefaults::default(),
        }
    }
}

fn defaults_path() -> Option<PathBuf> {
    crate::config::config_base_dir()
        .map(|base| base.join(crate::config::CONFIG_DIR).join(SESSION_DEFAULTS_FILE))
}

/// Load the persisted defaults profile; missing or corrupt files fall back
/// to [`SessionConversionDefaults::default`] (corrupt files warn loudly —
/// silently resetting a user's shoot profile would be a trap).
pub fn load_session_defaults() -> SessionConversionDefaults {
    match defaults_path() {
        Some(p) => load_session_defaults_from(&p),
        None => SessionConversionDefaults::default(),
    }
}

pub fn load_session_defaults_from(path: &Path) -> SessionConversionDefaults {
    match std::fs::read_to_string(path) {
        Ok(content) => match serde_json::from_str::<SessionConversionDefaults>(&content) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "--- WARNING: corrupt session-defaults profile at {} ({}); using defaults",
                    path.display(),
                    e
                );
                SessionConversionDefaults::default()
            }
        },
        Err(_) => SessionConversionDefaults::default(),
    }
}

/// Persist the defaults profile (atomic temp+rename, engine thread only).
pub fn save_session_defaults(
    defaults: &SessionConversionDefaults,
) -> Result<(), crate::config::SessionStoreError> {
    let path = match defaults_path() {
        Some(p) => p,
        None => {
            return Err(crate::config::SessionStoreError::NoConfigDir);
        }
    };
    save_session_defaults_to(&path, defaults)
}

pub fn save_session_defaults_to(
    path: &Path,
    defaults: &SessionConversionDefaults,
) -> Result<(), crate::config::SessionStoreError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| crate::config::SessionStoreError::Io(e.to_string()))?;
    }
    let content = serde_json::to_string_pretty(defaults)
        .map_err(|e| crate::config::SessionStoreError::Io(e.to_string()))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, content)
        .map_err(|e| crate::config::SessionStoreError::Io(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        crate::config::SessionStoreError::Io(e.to_string())
    })?;
    Ok(())
}

// ── Channel-resolution decision core (pure, table-tested) ──────────────

/// One candidate LTC carrier location: a stream index and a channel within
/// it. Audio WAV groups always use stream 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChannelCandidate {
    pub stream: usize,
    pub channel: usize,
}

impl fmt::Display for ChannelCandidate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "stream {} channel {}", self.stream, self.channel)
    }
}

/// Where a resolved candidate came from — reported per file in the
/// preflight report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelSource {
    /// Came from the persisted configured default.
    Configured,
    /// Pinned earlier in this preflight by a sibling file of the same device.
    Pinned,
    /// Found by scanning the remaining candidates.
    Scanned,
}

/// Ordered candidate list for one file's LTC carrier (pure decision core).
///
/// Resolution order: per-device pin (from a sibling hit) → configured
/// default → auto-scan of the remaining candidates from the probe. Video
/// probes list per-stream channel counts (stream-major order); audio WAVs
/// decode stream 0 and scan its channels. Without a probe (unreadable
/// file) only the configured/pinned candidates are offered — the decode
/// will fail and surface the typed `NoLtc` abort.
pub fn channel_candidate_order(
    kind: RecordingType,
    configured: Option<ChannelCandidate>,
    pinned: Option<ChannelCandidate>,
    stream_channels: &[usize],
) -> Vec<(ChannelCandidate, ChannelSource)> {
    let mut out: Vec<(ChannelCandidate, ChannelSource)> = Vec::new();
    let push = |c: ChannelCandidate, s: ChannelSource, out: &mut Vec<_>| {
        if !out.iter().any(|(existing, _)| *existing == c) {
            out.push((c, s));
        }
    };
    if let Some(p) = pinned {
        push(p, ChannelSource::Pinned, &mut out);
    }
    if let Some(c) = configured {
        push(c, ChannelSource::Configured, &mut out);
    }
    match kind {
        RecordingType::VideoClipSequence => {
            for (stream, &channels) in stream_channels.iter().enumerate() {
                for channel in 0..channels {
                    push(
                        ChannelCandidate { stream, channel },
                        ChannelSource::Scanned,
                        &mut out,
                    );
                }
            }
        }
        RecordingType::MultiTrackAudio => {
            let channels = stream_channels.first().copied().unwrap_or(0);
            for channel in 0..channels {
                push(
                    ChannelCandidate {
                        stream: 0,
                        channel,
                    },
                    ChannelSource::Scanned,
                    &mut out,
                );
            }
        }
    }
    out
}

/// Post-hoc pinning: a successful decode pins its candidate for sibling
/// files sharing the resolved device name. Pure state transition over the
/// pin map.
pub fn record_channel_pin(
    pins: &mut HashMap<String, ChannelCandidate>,
    device: &str,
    candidate: ChannelCandidate,
) {
    pins.insert(device.to_string(), candidate);
}

// ── Preflight report types ──────────────────────────────────────────────

/// One group (recording) discovered in a session offload destination.
#[derive(Clone, Debug, PartialEq)]
pub struct PreflightGroup {
    /// Destination root the group was discovered under (also the output
    /// folder: session deliverables are written next to their sources).
    pub dest: PathBuf,
    pub prefix: String,
    pub kind: RecordingType,
    pub files: Vec<PathBuf>,
    /// Audio groups only: index of the designated LTC carrier file.
    pub ltc_file_idx: usize,
}

/// Per-file preflight outcome.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PreflightFileResult {
    pub path: PathBuf,
    pub device: String,
    pub resolved: ChannelCandidate,
    pub source: ChannelSource,
    /// First decoded LTC frame (the start TC embedded during conversion).
    pub start_timecode: Option<Timecode>,
    pub confidence: Option<f32>,
    pub valid_frames: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PreflightGroupResult {
    pub dest: PathBuf,
    pub prefix: String,
    pub kind: RecordingType,
    /// The LTC carrier file that was decoded (audio groups: the designated
    /// LTC file; video groups: the first clip).
    pub ltc_carrier: PathBuf,
    /// Frame rate measured by the LTC decode (drives the TC metadata).
    pub detected_fps: f32,
    pub drop_frame: bool,
    /// One result per file in the group — every file of a group shares the
    /// resolved channel (the LTC signal is common to the recording set).
    pub files: Vec<PreflightFileResult>,
}

impl PreflightGroupResult {
    /// Start TC of the group's LTC decode (identical across files).
    pub fn start_timecode(&self) -> Option<Timecode> {
        self.files.first().and_then(|f| f.start_timecode)
    }
}

/// Typed preflight failure. Missing LTC is a real error: the run aborts.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum PreflightFailure {
    /// Every candidate channel failed to produce a coherent LTC decode.
    NoLtc {
        file: PathBuf,
        tried: Vec<ChannelCandidate>,
    },
    /// The file could not be probed/decoded at all.
    Decode {
        file: PathBuf,
        error: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct SessionPreflightReport {
    pub groups: Vec<PreflightGroupResult>,
    pub failures: Vec<PreflightFailure>,
}

impl SessionPreflightReport {
    pub fn has_failures(&self) -> bool {
        !self.failures.is_empty()
    }

    /// Total file count across groups (for progress and GUI summaries).
    pub fn file_count(&self) -> usize {
        self.groups.iter().map(|g| g.files.len()).sum()
    }
}

// ── Preflight decode seam ───────────────────────────────────────────────

/// Typed decode error crossing the preflight runner (R1: the runner
/// branches on cancel-vs-fail).
#[derive(Clone, Debug, PartialEq)]
pub enum PreflightDecodeError {
    Failed(String),
    Cancelled,
}

/// Everything the injected decode work needs. `unit` is the group's progress
/// unit — the real implementation forwards chunk-level decode granularity
/// into it (progress-granularity contract); fakes must sample/advance it
/// *inside* the seam to stay deterministic.
pub struct PreflightDecodeRequest<'a> {
    pub path: &'a Path,
    pub kind: RecordingType,
    pub stream_index: usize,
    pub channel: usize,
    pub fps: f64,
    pub drop_frame: bool,
    pub cancel: &'a AtomicBool,
    pub unit: Option<UnitProgress>,
}

pub type PreflightDecodeFn<'a> =
    &'a mut dyn FnMut(&PreflightDecodeRequest<'_>) -> Result<LtcDetectionResult, PreflightDecodeError>;

/// Real decode work: audio WAVs decode stream 0 with a pinned channel via
/// the builtin chunked decoder (24-bit location WAVs cannot cross the libltc
/// 16-bit boundary); video files extract the candidate channel at 16-bit and
/// decode via libltc (fast bulk path — the step-0 extraction mode).
pub fn preflight_decode_real(req: &PreflightDecodeRequest<'_>) -> Result<LtcDetectionResult, PreflightDecodeError> {
    match req.kind {
        RecordingType::MultiTrackAudio => {
            let path = req.path.to_path_buf();
            let cancel = Arc::new(AtomicBool::new(req.cancel.load(Ordering::Relaxed)));
            let dp = audio_core::DecodeProgress::with_atomics(
                1,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                cancel.clone(),
            );
            let bridge = req
                .unit
                .clone()
                .map(|unit| crate::decode::bridge_decode_progress(dp.clone(), unit, None));
            let result = audio_core::decode_ltc_chunked_channel(
                &path,
                /* use_libltc */ false,
                req.fps,
                req.drop_frame,
                audio_core::DecodeConfig::default(),
                &dp,
                Some(req.channel),
            );
            // No chunked decode ran for tiny files (total 1): release the bridge.
            dp.chunks_completed.store(1, Ordering::Relaxed);
            if let Some(h) = bridge {
                let _ = h.join();
            }
            result.map_err(|e| match e {
                audio_core::LtcDecodeError::Cancelled => PreflightDecodeError::Cancelled,
                other => PreflightDecodeError::Failed(other.to_string()),
            })
        }
        RecordingType::VideoClipSequence => {
            crate::decode::decode_video_channel(
                crate::decode::VideoDecodeRequest {
                    path: req.path,
                    stream_index: req.stream_index,
                    channel_index: req.channel,
                    // 16-bit extraction + libltc: the fast bulk path.
                    use_libltc: true,
                    decode_fps: req.fps,
                    decode_drop_frame: req.drop_frame,
                    cancel: None,
                },
                /* capture_gen */ 0,
                &|_frac| {},
                req.unit.clone(),
            )
            .map_err(|e| match e {
                audio_core::LtcDecodeError::Cancelled => PreflightDecodeError::Cancelled,
                other => PreflightDecodeError::Failed(other.to_string()),
            })
        }
    }
}

/// Groups the session's offload destinations into preflight groups
/// (per-destination recording-group scan). Order is stable: destinations in
/// session order, groups in scan order.
pub fn collect_preflight_groups(
    destinations: &[PathBuf],
    audio_ltc_file_idx: usize,
) -> Vec<PreflightGroup> {
    let mut groups = Vec::new();
    for dest in destinations {
        for g in crate::file_pattern::match_files_all_patterns(dest) {
            groups.push(PreflightGroup {
                dest: dest.clone(),
                prefix: g.prefix,
                kind: g.recording_type,
                files: g.files,
                ltc_file_idx: audio_ltc_file_idx,
            });
        }
    }
    groups
}

/// Byte-weighted unit specs for a preflight/conversion run: one unit per
/// group, weighted by total input size (equal weights only when sizes are
/// unknown/zero — mirrors `offload_copy_unit_specs`).
pub fn session_unit_specs(groups: &[PreflightGroup]) -> Vec<crate::job::UnitSpec> {
    let sizes: Vec<u64> = groups
        .iter()
        .map(|g| {
            g.files
                .iter()
                .filter_map(|f| std::fs::metadata(f).ok())
                .map(|m| m.len())
                .sum()
        })
        .collect();
    let total: u64 = sizes.iter().sum();
    if total == 0 {
        return groups
            .iter()
            .map(|g| crate::job::UnitSpec {
                weight: 1.0 / groups.len().max(1) as f32,
                label: g.prefix.clone(),
            })
            .collect();
    }
    groups
        .iter()
        .zip(sizes.iter())
        .map(|(g, &size)| crate::job::UnitSpec {
            weight: (size as f32 / total as f32).max(0.001),
            label: g.prefix.clone(),
        })
        .collect()
}

/// Decode one group's LTC carrier across the candidate order. Returns the
/// per-file results (the group's files share the resolved channel) or the
/// typed failure after candidate exhaustion.
fn decode_group(
    group: &PreflightGroup,
    device: &str,
    configured: Option<ChannelCandidate>,
    pinned: Option<ChannelCandidate>,
    stream_channels: &[usize],
    ctx: &JobContext,
    unit_idx: usize,
    fps: f64,
    drop_frame: bool,
    decode: PreflightDecodeFn<'_>,
) -> Result<(Vec<PreflightFileResult>, f32), PreflightFailure> {
    let carrier = match group.kind {
        RecordingType::MultiTrackAudio => group
            .files
            .get(group.ltc_file_idx)
            .or_else(|| group.files.first()),
        RecordingType::VideoClipSequence => group.files.first(),
    }
    .ok_or_else(|| PreflightFailure::Decode {
        file: group.dest.join(&group.prefix),
        error: "group has no files".to_string(),
    })?
    .clone();

    let candidates =
        channel_candidate_order(group.kind.clone(), configured, pinned, stream_channels);
    if candidates.is_empty() {
        // Probe failure: no candidates could be derived at all.
        return Err(PreflightFailure::Decode {
            file: carrier.clone(),
            error: "could not probe channels".to_string(),
        });
    }
    let mut tried = Vec::new();
    let mut last_hard_error: Option<String> = None;
    let mut any_decode_ran = false;
    for (candidate, source) in &candidates {
        if ctx.cancel.is_cancelled() {
            return Err(PreflightFailure::Decode {
                file: carrier.clone(),
                error: "cancelled".to_string(),
            });
        }
        tried.push(*candidate);
        let carrier_channels = stream_channels
            .get(candidate.stream)
            .copied()
            .unwrap_or(0);
        let _ = carrier_channels;
        let req = PreflightDecodeRequest {
            path: &carrier,
            kind: group.kind.clone(),
            stream_index: candidate.stream,
            channel: candidate.channel,
            fps,
            drop_frame,
            cancel: ctx.cancel.inner(),
            unit: Some(ctx.progress.unit(unit_idx)),
        };
        match decode(&req) {
            Ok(result) => {
                any_decode_ran = true;
                if matches!(result.status, LtcDecodeStatus::Success) {
                    let start = result.timecodes.first().map(|t| t.timecode);
                    let files = group
                        .files
                        .iter()
                        .map(|path| PreflightFileResult {
                            path: path.clone(),
                            device: device.to_string(),
                            resolved: *candidate,
                            source: *source,
                            start_timecode: start,
                            confidence: Some(result.avg_confidence),
                            valid_frames: result.valid_frames as usize,
                        })
                        .collect();
                    return Ok((files, result.detected_fps));
                }
            }
            Err(PreflightDecodeError::Cancelled) => {
                return Err(PreflightFailure::Decode {
                    file: carrier.clone(),
                    error: "cancelled".to_string(),
                });
            }
            Err(PreflightDecodeError::Failed(e)) => {
                log::debug!(
                    "preflight: candidate {} failed for '{}': {}",
                    candidate,
                    carrier.display(),
                    e
                );
                last_hard_error = Some(e);
            }
        }
    }
    if !any_decode_ran && last_hard_error.is_some() {
        // Every candidate hard-failed (unreadable/corrupt) — a different
        // failure class from "decoded fine but no LTC present".
        return Err(PreflightFailure::Decode {
            file: carrier,
            error: last_hard_error.unwrap(),
        });
    }
    Err(PreflightFailure::NoLtc {
        file: carrier,
        tried,
    })
}

/// Probe helper: channel count per stream for a file (pure I/O shell —
/// WAV header read for audio, ffprobe for video).
pub fn probe_stream_channels(path: &Path, kind: &RecordingType) -> Vec<usize> {
    match kind {
        RecordingType::MultiTrackAudio => {
            match audio_core::WavChunkReader::open(path) {
                Ok((reader, _start)) => {
                    let channels = reader.channels();
                    // The WAV decodes as one implicit stream with N channels.
                    vec![channels]
                }
                Err(_) => Vec::new(),
            }
        }
        RecordingType::VideoClipSequence => {
            match crate::ffprobe::probe_video_audio(path) {
                Ok(probe) => probe.streams.iter().map(|s| s.channels).collect(),
                Err(_) => Vec::new(),
            }
        }
    }
}

/// Run the session preflight over the session's offload destinations.
///
/// Units: one per group (byte-weighted); per-file decode granularity is
/// forwarded into the group's unit by the decode seam. Any group whose LTC
/// carrier cannot be resolved after candidate exhaustion produces a typed
/// [`PreflightFailure::NoLtc`] — the run stays blocked until the user fixes
/// channel defaults or converts that recording manually.
pub fn run_session_preflight_with(
    ctx: &JobContext,
    groups: &[PreflightGroup],
    defaults: &SessionConversionDefaults,
    fps: f64,
    drop_frame: bool,
    decode: PreflightDecodeFn<'_>,
    device_of: &dyn Fn(&[PathBuf]) -> String,
) -> Result<crate::job::JobFinal, JobError> {
    let mut pins: HashMap<String, ChannelCandidate> = HashMap::new();
    let mut report = SessionPreflightReport::default();

    for (unit_idx, group) in groups.iter().enumerate() {
        let device = device_of(&group.files);
        let stream_channels = probe_stream_channels(
            carrier_of(group),
            &group.kind,
        );
        let configured = configured_candidate(defaults, &group.kind);
        let pinned = pins.get(&device).copied();
        ctx.progress
            .unit(unit_idx)
            .set_message(format!("decoding LTC ({})", group.prefix));

        match decode_group(
            group,
            &device,
            configured,
            pinned,
            &stream_channels,
            ctx,
            unit_idx,
            fps,
            drop_frame,
            decode,
        ) {
            Ok((files, detected)) => {
                if let Some(first) = files.first() {
                    record_channel_pin(&mut pins, &device, first.resolved);
                }
                report.groups.push(PreflightGroupResult {
                    dest: group.dest.clone(),
                    prefix: group.prefix.clone(),
                    kind: group.kind.clone(),
                    ltc_carrier: carrier_of(group).to_path_buf(),
                    detected_fps: detected,
                    drop_frame,
                    files,
                });
                ctx.progress.unit(unit_idx).set_message("LTC resolved");
            }
            Err(failure) => {
                report.failures.push(failure);
            }
        }
        ctx.progress.unit(unit_idx).finish();
    }

    if ctx.cancel.is_cancelled() {
        return Err(JobError::Cancelled);
    }
    Ok(crate::job::JobFinal::SessionPreflight {
        report: Arc::new(report),
    })
}

fn carrier_of(group: &PreflightGroup) -> &Path {
    match group.kind {
        RecordingType::MultiTrackAudio => group
            .files
            .get(group.ltc_file_idx)
            .or_else(|| group.files.first()),
        RecordingType::VideoClipSequence => group.files.first(),
    }
    .unwrap_or(&group.dest)
}

fn configured_candidate(
    defaults: &SessionConversionDefaults,
    kind: &RecordingType,
) -> Option<ChannelCandidate> {
    match kind {
        RecordingType::MultiTrackAudio => defaults
            .channels
            .audio_ltc_channel_override
            .map(|channel| ChannelCandidate { stream: 0, channel }),
        RecordingType::VideoClipSequence => Some(ChannelCandidate {
            stream: defaults.channels.video_ltc_stream,
            channel: defaults.channels.video_ltc_channel,
        }),
    }
}

// ── Conversion orchestration ────────────────────────────────────────────

/// Per-group outcome of a session conversion run. `ok = false` with a
/// recorded error is the honest partial (orange) state — the run itself
/// still terminates successfully.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SessionRunGroupOutcome {
    pub prefix: String,
    pub dest: PathBuf,
    pub ok: bool,
    pub error: Option<String>,
    pub encoder_used: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Default)]
pub struct SessionRunSummary {
    pub outcomes: Vec<SessionRunGroupOutcome>,
    /// True when at least one group failed (orange partial rendering).
    pub partial: bool,
}

impl SessionRunSummary {
    pub fn completed_count(&self) -> usize {
        self.outcomes.iter().filter(|o| o.ok).count()
    }
}

/// Conversion settings for one preflight-validated group, ready to run.
#[derive(Clone, Debug)]
pub struct SessionGroupJob {
    pub group: PreflightGroupResult,
    pub settings: ConverterSettings,
}

/// Assemble the per-group [`ConverterSettings`] from the defaults profile
/// and the preflight results.
///
/// `(resolved_container, resolved_video_codec, resolved_audio_encoder)` are
/// the live `select_best_combination()` values for absent overrides.
/// Audio groups carry the owner-decision constants (split + drop-LTC + TC
/// from LTC); video groups use the configured video defaults. Trim is never
/// applied (owner workflow trims nothing).
pub fn assemble_session_group_settings(
    group: &PreflightGroupResult,
    defaults: &SessionConversionDefaults,
    resolved: (String, String, String),
) -> ConverterSettings {
    let (container, video_codec, audio_encoder) = resolved;
    match group.kind {
        RecordingType::MultiTrackAudio => {
            let n = group.files.len();
            ConverterSettings {
                pipeline: ConversionPipeline::AudioOnly {
                    generate_synthetic_video: false,
                },
                input_files: group.files.iter().map(|f| f.path.clone()).collect(),
                recording_type: RecordingType::MultiTrackAudio,
                // Owner decision: the LTC file is the TC source only —
                // excluded from outputs via drop_ltc_track.
                ltc_track_channel_index: defaults.audio.ltc_file_idx.min(n.saturating_sub(1)),
                channel_map: crate::converter::ChannelMap::identity(n),
                split_tracks: true,
                drop_ltc_track: true,
                ltc_video_source: None,
                container,
                copy_video: false,
                video_encoder: video_codec,
                audio_encoder,
                resolved_video_encoder: String::new(),
                resolved_hw_device: None,
                output_folder: group.dest.clone(),
                filename_prefix: defaults.video.filename_prefix.clone(),
                audio_suffix_template: defaults.video.audio_suffix_template.clone(),
                video_suffix_template: defaults.video.video_suffix_template.clone(),
                set_start_from_ltc: true,
                embed_camera_metadata: defaults.video.embed_camera_metadata,
                trim_offsets_secs: vec![0.0; n],
                timecode_meta_per_file: group_timecode_meta(group),
                camera_meta_per_file: vec![None; n],
                device_name: Some(device_label(group)),
                concat_audio: false,
            }
        }
        RecordingType::VideoClipSequence => {
            let n = group.files.len();
            let resolved_ch = group.files.first().map(|f| f.resolved);
            ConverterSettings {
                pipeline: ConversionPipeline::VideoPassthrough,
                input_files: group.files.iter().map(|f| f.path.clone()).collect(),
                recording_type: RecordingType::VideoClipSequence,
                ltc_track_channel_index: 0,
                channel_map: crate::converter::ChannelMap::identity(n),
                split_tracks: defaults.video.split_tracks,
                drop_ltc_track: defaults.video.drop_ltc_track,
                ltc_video_source: resolved_ch.map(|c| (c.stream, c.channel)),
                container: defaults
                    .video
                    .container_override
                    .clone()
                    .unwrap_or(container),
                copy_video: false,
                video_encoder: defaults
                    .video
                    .video_codec_override
                    .clone()
                    .unwrap_or(video_codec),
                audio_encoder: defaults
                    .video
                    .audio_encoder_override
                    .clone()
                    .unwrap_or(audio_encoder),
                resolved_video_encoder: String::new(),
                resolved_hw_device: None,
                output_folder: group.dest.clone(),
                filename_prefix: defaults.video.filename_prefix.clone(),
                audio_suffix_template: defaults.video.audio_suffix_template.clone(),
                video_suffix_template: defaults.video.video_suffix_template.clone(),
                set_start_from_ltc: defaults.video.set_start_from_ltc,
                embed_camera_metadata: defaults.video.embed_camera_metadata,
                trim_offsets_secs: vec![0.0; n],
                timecode_meta_per_file: group_timecode_meta(group),
                camera_meta_per_file: vec![None; n],
                device_name: Some(device_label(group)),
                concat_audio: false,
            }
        }
    }
}

fn device_label(group: &PreflightGroupResult) -> String {
    group
        .files
        .first()
        .map(|f| f.device.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

fn group_timecode_meta(group: &PreflightGroupResult) -> Vec<Option<TimecodeMetadata>> {
    group
        .files
        .iter()
        .map(|f| {
            f.start_timecode.map(|start| TimecodeMetadata {
                start,
                fps: group.detected_fps.max(1.0) as f64,
                drop_frame: group.drop_frame,
            })
        })
        .collect()
}

/// Reporter mapping one conversion group's progress into that group's unit
/// of the session run tracker. Same step-fraction semantics as
/// `JobConversionReport` (in-flight fractions forwarded *during* the step),
/// redirected from unit 0 to the group's unit.
struct GroupedConversionReport<'a> {
    ctx: &'a JobContext,
    unit_idx: usize,
    step_weight: AtomicU32,
    overall: AtomicU32,
    failed: std::sync::atomic::AtomicBool,
    failure: Mutex<Option<String>>,
}

impl<'a> GroupedConversionReport<'a> {
    fn new(ctx: &'a JobContext, unit_idx: usize) -> Self {
        Self {
            ctx,
            unit_idx,
            step_weight: AtomicU32::new(0),
            overall: AtomicU32::new(0),
            failed: std::sync::atomic::AtomicBool::new(false),
            failure: Mutex::new(None),
        }
    }

    fn overall_f(&self) -> f32 {
        self.overall.load(Ordering::Relaxed) as f32 / 1000.0
    }

    fn unit(&self) -> UnitProgress {
        self.ctx.progress.unit(self.unit_idx)
    }
}

impl<'a> ConversionReport for GroupedConversionReport<'a> {
    fn is_cancelled(&self) -> bool {
        self.ctx.cancel.is_cancelled()
    }

    fn step_weight(&self) -> f32 {
        self.step_weight.load(Ordering::Relaxed) as f32 / 1000.0
    }

    fn set_step_weight(&self, w: f32) {
        self.step_weight
            .store((w.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }

    fn report_step_fraction(&self, step_fraction: f32, _line: &str) {
        let combined = (self.overall_f() + step_fraction * self.step_weight()).min(1.0);
        self.unit().set_fraction(combined);
    }

    fn advance_step(&self) {
        let next = (self.overall_f() + self.step_weight()).min(1.0);
        self.overall
            .store((next * 1000.0) as u32, Ordering::Relaxed);
        self.unit().set_fraction(next);
    }

    fn set_log(&self, text: &str) {
        self.ctx.progress.push_log(text);
    }

    fn append_log(&self, text: &str) {
        self.ctx.progress.push_log(text);
    }

    fn set_message(&self, msg: &str) {
        self.ctx.progress.set_message(msg);
    }

    fn mark_failed(&self, log: &str) {
        self.failed.store(true, Ordering::Relaxed);
        *self.failure.lock().unwrap() = Some(log.to_string());
        self.ctx.progress.push_log(log);
    }

    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    fn mark_completed(&self, summary: &str) {
        self.unit().set_fraction(1.0);
        self.ctx
            .progress
            .push_log(format!("--- COMPLETED ---{summary}"));
    }

    fn set_unit_count(&self, n: usize) {
        self.ctx.progress.grow_to(n);
    }

    fn unit(&self, idx: usize) -> Option<UnitProgress> {
        if idx < self.ctx.progress.snapshot().units.len() {
            Some(self.ctx.progress.unit(idx))
        } else {
            None
        }
    }

    fn cancel_atomic(&self) -> Option<&std::sync::atomic::AtomicBool> {
        Some(self.ctx.cancel.inner())
    }
}

/// Run one group's conversion through the real pipeline. Returns
/// `(encoder_used, Ok(()))` on success, the failure reason on a failed
/// group. Thin shell over `run_conversion` + [`GroupedConversionReport`].
fn run_session_group_real(
    ctx: &JobContext,
    unit_idx: usize,
    settings: ConverterSettings,
    caps: Option<&crate::converter::FfmpegCapabilities>,
) -> (Option<String>, Result<(), String>) {
    let report = GroupedConversionReport::new(ctx, unit_idx);
    let (encoder_used, _metadata_only) =
        crate::converter::run_conversion(&report, settings, caps);
    if report.is_failed() {
        let reason = report
            .failure
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| "conversion failed".to_string());
        (encoder_used, Err(reason))
    } else {
        ctx.progress.unit(unit_idx).finish();
        (encoder_used, Ok(()))
    }
}

/// Seam shape for injected group runs (contract tests). Receives the
/// group's unit index and unit and must advance the fraction *inside* the
/// fake.
pub type SessionGroupRunFn<'a> = &'a mut dyn FnMut(
    usize,
    &UnitProgress,
    &ConverterSettings,
    Option<&crate::converter::FfmpegCapabilities>,
) -> Result<Option<String>, JobError>;

/// Run the session conversion over preflight-validated groups.
///
/// Encode failure mid-run records an honest per-group failure and
/// **continues** with the remaining groups (orange partial); the run as a
/// whole completes. Cancellation aborts with the typed `JobError::Cancelled`.
pub fn run_session_conversion_with(
    ctx: &JobContext,
    jobs: Vec<SessionGroupJob>,
    caps: Option<crate::converter::FfmpegCapabilities>,
    run_group: SessionGroupRunFn<'_>,
) -> Result<crate::job::JobFinal, JobError> {
    let mut summary = SessionRunSummary::default();
    for (unit_idx, job) in jobs.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            return Err(JobError::Cancelled);
        }
        ctx.progress
            .unit(unit_idx)
            .set_message(format!("converting {}", job.group.prefix));
        let result = run_group(
            unit_idx,
            &ctx.progress.unit(unit_idx),
            &job.settings,
            caps.as_ref(),
        );
        match result {
            Ok(encoder_used) => {
                ctx.progress.unit(unit_idx).finish();
                summary.outcomes.push(SessionRunGroupOutcome {
                    prefix: job.group.prefix.clone(),
                    dest: job.group.dest.clone(),
                    ok: true,
                    error: None,
                    encoder_used,
                });
            }
            Err(JobError::Cancelled) => return Err(JobError::Cancelled),
            Err(JobError::Failed(e)) => {
                summary.outcomes.push(SessionRunGroupOutcome {
                    prefix: job.group.prefix.clone(),
                    dest: job.group.dest.clone(),
                    ok: false,
                    error: Some(e),
                    encoder_used: None,
                });
            }
        }
    }
    summary.partial = summary.outcomes.iter().any(|o| !o.ok);
    Ok(crate::job::JobFinal::SessionConversion {
        summary: Arc::new(summary),
    })
}

/// Real group runner for [`run_session_conversion_with`].
pub fn session_group_runner_real(
    ctx: &JobContext,
) -> impl FnMut(
    usize,
    &UnitProgress,
    &ConverterSettings,
    Option<&crate::converter::FfmpegCapabilities>,
) -> Result<Option<String>, JobError> + '_ {
    move |unit_idx, _unit, settings, caps| {
        let (encoder, result) = run_session_group_real(ctx, unit_idx, settings.clone(), caps);
        result.map(|()| encoder).map_err(JobError::Failed)
    }
}

#[cfg(test)]
mod tests;
