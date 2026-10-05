use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::camera_meta::CameraInfo;
use crate::converter::{ChannelMap, ConvertBlocker, FfmpegCapabilities, RecordingType};
use crate::ffprobe::VideoAudioProbe;
use crate::file_pattern::MatchedGroup;
use crate::job::{JobKind, JobStatus};
use crate::naming::{DEFAULT_AUDIO_SUFFIX, DEFAULT_PREFIX, DEFAULT_VIDEO_SUFFIX};
use crate::offload::OffloadSnapshot;
use crate::timecode::FPS_OPTIONS;
use audio_core::{AudioDeviceInfo, ChannelSel, LtcDetectionResult, Timecode};

// ── Converter user settings (single source of truth) ─────────────────

/// All user-configurable converter options — engine-owned single source
/// of truth.  GUIs mutate by sending `ConverterCommand` variants; the
/// engine applies side-effects (defaults repair, readiness recompute,
/// config persistence) in the command handler.
#[derive(Clone, Debug, PartialEq)]
pub struct ConverterUserSettings {
    /// "Metadata only — tag + rename, extract audio" mode.
    pub metadata_only: bool,
    /// Audio-only pipeline: generate a synthetic black+silence video.
    pub generate_synthetic_video: bool,
    /// "Leave video encoding untouched" (stream copy).
    pub copy_video: bool,
    /// Split tracks into separate files.
    pub split_tracks: bool,
    /// Drop the LTC track from output.
    pub drop_ltc_track: bool,
    /// Concatenate audio tracks across clips (video groups only).
    pub concat_audio: bool,
    /// Embed start timecode metadata from LTC decode results.
    pub set_start_from_ltc: bool,
    /// Embed camera metadata (make/model, bext originator) in output files.
    pub embed_camera_metadata: bool,
    /// Index of the file/channel carrying LTC (audio groups).
    pub ltc_file_idx: usize,
    /// Input→output channel permutation matrix.
    pub channel_map: ChannelMap,
    /// Output container id, e.g. `"mov"`, `"mkv"`, `"mp4"`.
    pub container: String,
    /// Video codec id, e.g. `"h265"`, `"av1"`, `"h264"`.
    pub video_encoder: String,
    /// Audio encoder id, e.g. `"pcm_s24le"`, `"aac"`.
    pub audio_encoder: String,
    /// Output folder for converted files.
    pub output_folder: PathBuf,
    /// Whether the user has manually set `output_folder` (via `SetOutputFolder`).
    /// When false, the engine re-defaults `output_folder` to the selected
    /// recording's parent dir on each recording selection.
    pub output_folder_user_set: bool,
    /// Filename prefix for output files.
    pub filename_prefix: String,
    /// Naming template for audio output files (e.g. `_audio_track{track:01d}`).
    pub audio_suffix_template: String,
    /// Naming template for video output files (e.g. `_video_clip{clip:01d}`).
    pub video_suffix_template: String,
}

impl ConverterUserSettings {
    pub fn initial() -> Self {
        Self {
            metadata_only: false,
            generate_synthetic_video: false,
            copy_video: false,
            split_tracks: false,
            drop_ltc_track: false,
            concat_audio: false,
            set_start_from_ltc: false,
            embed_camera_metadata: true,
            ltc_file_idx: 0,
            channel_map: ChannelMap::identity(0),
            container: "mov".to_string(),
            video_encoder: "h265".to_string(),
            audio_encoder: "pcm_s24le".to_string(),
            output_folder: PathBuf::new(),
            output_folder_user_set: false,
            filename_prefix: DEFAULT_PREFIX.to_string(),
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        }
    }
}

// ── Converter snapshot (engine-owned) ─────────────────────────────────

/// Engine-managed converter state: groups, probes, conversion status,
/// user settings (single source of truth), and derived UI data.
#[derive(Clone, Debug, PartialEq)]
pub struct ConverterSnapshot {
    /// File groups discovered in the current converter folder.
    /// Empty when no folder has been selected.
    pub groups: Vec<MatchedGroup>,
    /// Index of the selected group, or `None`.
    pub selected_group_idx: Option<usize>,
    /// Per-file audio/video probes for the selected recording.
    /// Index-aligned with the group's files.
    pub probes: Vec<Option<VideoAudioProbe>>,
    /// Per-file camera metadata for the selected recording.
    /// Index-aligned with the group's files. Populated alongside probes.
    pub camera_meta: Vec<Option<CameraInfo>>,
    /// Detected device name for the selected recording (from XAVC sniff,
    /// exiftool/ffprobe, filename pattern, or `"unknown"`).  `None` while
    /// probes are loading or when no recording is selected.
    pub device_name: Option<String>,
    /// Generation counter — incremented on each recording selection so
    /// GUIs can discard stale probe results.
    pub probes_generation: u64,
    /// The folder being (or last) scanned.
    pub groups_folder: Option<PathBuf>,
    /// Engine-owned user settings — sole source of truth.
    pub settings: ConverterUserSettings,
    /// Readiness blockers — recomputed by the engine on settings/groups/
    /// caps changes.  Empty list means conversion is ready.
    pub readiness: Vec<ConvertBlocker>,
    /// Warning when an output file collides with an input file.
    pub collision_warning: Option<String>,
    /// Warning when two or more planned output files share the same name.
    pub duplicate_output_warning: Option<String>,
}

impl ConverterSnapshot {
    pub fn is_recording_selected(&self) -> bool {
        self.selected_group_idx.is_some()
    }

    pub fn selected_recording_type(&self) -> Option<RecordingType> {
        self.selected_group_idx
            .and_then(|idx| self.groups.get(idx).map(|g| g.recording_type.clone()))
    }
}

// ── LTC group decode per-clip state ─────────────────────────────────────

/// Per-clip state of a batch LTC group decode; index-aligned with
/// `AppStateSnapshot::ltc_group_paths`.  The decode result is boxed to keep
/// the pending variant cheap (the Vec is republished every engine tick).
#[derive(Clone, Debug, PartialEq)]
pub enum ClipDecodeState {
    /// Decode still in flight for this clip.
    Pending,
    /// Clip finished; carries the decode result or the error message.
    Done(Result<Box<LtcDetectionResult>, String>),
}

impl ClipDecodeState {
    /// True once the clip finished (successfully or not).
    pub fn is_done(&self) -> bool {
        matches!(self, ClipDecodeState::Done(_))
    }

    /// The decoded result, if the clip finished successfully.
    pub fn ok(&self) -> Option<&LtcDetectionResult> {
        match self {
            ClipDecodeState::Done(Ok(r)) => Some(&**r),
            _ => None,
        }
    }
}

// ── Clap log entry ──────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct ClapLogItem {
    /// Monotonic clap counter (presentation fields are pre-formatted strings
    /// so both GUIs share a single formatting site in the engine).
    pub id: u64,
    pub timestamp: String,
    pub timecode: String,
    pub milliseconds: String,
    pub note: String,
}

// ── Decode snapshot (engine-owned) ─────────────────────────────────────

/// Engine-managed LTC-decode state: decode FPS, video probe, single-file
/// and batch group-decode results.  `generation` counters are engine
/// latches (also used by integration tests as progress handles); no GUI
/// renders them.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeSnapshot {
    /// Index into `FPS_OPTIONS` used when decoding.
    pub fps_index: usize,
    /// Video/audio stream probe of the selected file (from `ProbeVideo`
    /// or the first clip probe of a video group).
    pub probe: Option<VideoAudioProbe>,
    /// Selected audio stream for video decode.
    pub selected_stream: usize,
    /// Selected channel within the selected stream.
    pub selected_channel: usize,
    /// Single-file decode result.
    pub result: Option<LtcDetectionResult>,
    /// Last decode/probe error message, if any.
    pub error: Option<String>,
    /// Bumped whenever single-file decode state resets; used by the engine
    /// to latch auto-apply and discard stale results.
    pub generation: u64,
    /// Paths of the batch being decoded, index-aligned with `group_results`.
    pub group_paths: Vec<PathBuf>,
    /// Per-clip batch decode state (index matches `group_paths`).
    pub group_results: Vec<ClipDecodeState>,
    /// Bumped whenever the batch decode resets (same role as `generation`).
    pub group_generation: u64,
}

// ── Clapper snapshot (engine-owned) ────────────────────────────────────

/// Engine-managed clapper-board state and clap log.  The continuous clap
/// animation (flash/arm decay) is GUI-local since Phase 5: the engine only
/// publishes [`ClapperSnapshot::clap_seq`], a monotonic event-like counter
/// each GUI watches to start its own animation.
#[derive(Clone, Debug, PartialEq)]
pub struct ClapperSnapshot {
    pub scene: u32,
    pub take: u32,
    pub roll: String,
    pub auto_increment_take: bool,
    /// Clap log, newest first, capped at `MAX_CLAP_LOGS`.
    pub logs: Vec<ClapLogItem>,
    /// Monotonic counter, incremented once per clap. Event-like, not state:
    /// a GUI that observes a value different from the last one it animated
    /// starts its local clap animation. If a GUI somehow misses two claps
    /// between frames it animates once for `seq != last` — acceptable
    /// because claps are human-paced and the animation is cosmetic.
    pub clap_seq: u64,
}

// ── Status channels ─────────────────────────────────────────────────────

/// Which subsystem last wrote to [`StatusChannels`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusChannel {
    Audio,
    Decode,
    Converter,
    Offload,
}

/// Per-subsystem status messages, replacing the former single
/// `status_message` string that every subsystem wrote to (last writer
/// won, so a conversion progress note could clobber a device error).
/// Each channel is owned by the subsystem that produces it; a footer
/// wanting "the last thing that happened" renders [`StatusChannels::message`].
#[derive(Clone, Debug, PartialEq)]
pub struct StatusChannels {
    /// Last audio/transport event ("Streaming LTC", "Device scan failed", …).
    pub audio: String,
    /// Last decode event ("Parse failed: …", decode summaries).  Overlaps
    /// `DecodeSnapshot.error` only in that both are human-readable; the
    /// error field stays as typed per-decode state.
    pub decode: String,
    /// Last converter event.  During a conversion the job's own
    /// `jobs[Conversion].message` carries progress; this only records
    /// lifecycle events (started/completed/failed/canceled).
    pub converter: String,
    /// Last offload event (completed/canceled summaries).
    pub offload: String,
    /// Channel written most recently — drives [`StatusChannels::message`].
    pub last: StatusChannel,
}

impl StatusChannels {
    pub fn initial() -> Self {
        Self {
            audio: "Ready".to_string(),
            decode: String::new(),
            converter: String::new(),
            offload: String::new(),
            last: StatusChannel::Audio,
        }
    }

    pub fn set_audio(&mut self, msg: impl Into<String>) {
        self.audio = msg.into();
        self.last = StatusChannel::Audio;
    }

    pub fn set_decode(&mut self, msg: impl Into<String>) {
        self.decode = msg.into();
        self.last = StatusChannel::Decode;
    }

    pub fn set_converter(&mut self, msg: impl Into<String>) {
        self.converter = msg.into();
        self.last = StatusChannel::Converter;
    }

    pub fn set_offload(&mut self, msg: impl Into<String>) {
        self.offload = msg.into();
        self.last = StatusChannel::Offload;
    }

    /// The most recently written channel's message — the footer's
    /// "last thing that happened" view.
    pub fn message(&self) -> &str {
        match self.last {
            StatusChannel::Audio => &self.audio,
            StatusChannel::Decode => &self.decode,
            StatusChannel::Converter => &self.converter,
            StatusChannel::Offload => &self.offload,
        }
    }
}

// ── Application state snapshot ─────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct AppStateSnapshot {
    // Command acknowledgement counter — bumped by the engine once per
    // command drained from the command channel, before it is applied.
    // GUIs use it to confirm that a sent command has landed: a shadow
    // edit sent at seq N is "applied" once `applied_command_seq >= N`.
    // The GUI is the sole producer on the command channel, so seqs are
    // assigned monotonically by the sender and compared with `>=`.
    pub applied_command_seq: u64,

    // Transport
    pub is_playing: bool,
    pub is_locked: bool,
    pub current_timecode: Timecode,
    pub start_timecode: Timecode,

    // FPS (stored as an index into `FPS_OPTIONS`; derive the value via `fps()`)
    pub fps_index: usize,

    // Audio routing
    pub ltc_channel: ChannelSel,
    pub beep_channel: ChannelSel,
    pub ltc_volume: f32,
    pub beep_volume: f32,
    pub beep_frequency: f32,
    /// Beep tone duration in seconds.
    pub beep_duration: f32,

    // Audio device
    pub devices: Vec<AudioDeviceInfo>,
    /// Selected output device by stable id (`AudioDeviceInfo::id`).
    /// `None` = automatic (default/first available device).
    pub selected_device: Option<String>,
    /// Engine-internal working flag: whether the audio output stream was
    /// successfully initialized.  Not rendered by any GUI.
    pub audio_initialized: bool,
    /// Soft-recovery attempt counter of the audio-stream recovery ladder
    /// (`handle_event`/`recovery_action`), published so tests can assert the
    /// ladder with typed values instead of status text. Reset to 0 by a
    /// successful audio (re-)init. Not rendered by any GUI.
    pub audio_recovery_attempts: u8,
    pub sample_rate: u32,
    pub sample_format_name: String,

    // Clapper (metadata, log, engine-computed clap animation)
    pub clapper: ClapperSnapshot,

    // Theme
    pub is_dark_theme: bool,

    // Status (per-subsystem channels; see `StatusChannels`)
    pub status: StatusChannels,

    // LTC decode (probe, single-file and batch results)
    pub decode: DecodeSnapshot,

    // ffmpeg capability probe (engine-owned, async)
    pub ffmpeg_caps: Option<FfmpegCapabilities>,

    // Unified job status map
    pub jobs: HashMap<JobKind, JobStatus>,

    // File duration probe (engine-owned, async)
    /// Per-file durations for converter-scope files (converter groups),
    /// keyed by full path. `None` meaning the file could not be probed
    /// (unreadable, no ffprobe, etc).
    ///
    /// Kept separate from `offload.file_durations` on purpose: the two
    /// maps have different lifetimes (cleared on `ProbeFileDurations` vs
    /// `OffloadScan`) even though a `DurationProbe` job may fill both.
    pub file_durations: HashMap<PathBuf, Option<f64>>,
    // Engine-owned converter state
    pub converter: ConverterSnapshot,

    // Engine-owned offload / file-ingestion state
    pub offload: OffloadSnapshot,
}

impl AppStateSnapshot {
    pub fn initial() -> Self {
        let suggest_sr = audio_core::suggest_sample_rate();

        AppStateSnapshot {
            applied_command_seq: 0,
            is_playing: false,
            is_locked: false,
            current_timecode: Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            start_timecode: Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            fps_index: 1,
            ltc_channel: ChannelSel::Left,
            beep_channel: ChannelSel::Right,
            ltc_volume: 0.25,
            beep_volume: 0.5,
            beep_frequency: 1000.0,
            beep_duration: 0.5,
            devices: Vec::new(),
            selected_device: None,
            audio_initialized: false,
            audio_recovery_attempts: 0,
            sample_rate: suggest_sr,
            sample_format_name: String::new(),
            clapper: ClapperSnapshot {
                scene: 1,
                take: 1,
                roll: "A001".to_string(),
                auto_increment_take: true,
                logs: Vec::new(),
                clap_seq: 0,
            },
            is_dark_theme: false,
            status: StatusChannels::initial(),
            decode: DecodeSnapshot {
                fps_index: 1,
                probe: None,
                selected_stream: 0,
                selected_channel: 0,
                result: None,
                error: None,
                generation: 0,
                group_paths: Vec::new(),
                group_results: Vec::new(),
                group_generation: 0,
            },
            ffmpeg_caps: None,
            jobs: HashMap::new(),
            file_durations: HashMap::new(),

            converter: ConverterSnapshot {
                groups: Vec::new(),
                selected_group_idx: None,
                probes: Vec::new(),
                camera_meta: Vec::new(),
                device_name: None,
                probes_generation: 0,
                groups_folder: None,
                settings: ConverterUserSettings::initial(),
                readiness: Vec::new(),
                collision_warning: None,
                duplicate_output_warning: None,
            },
            offload: OffloadSnapshot::initial(),
        }
    }

    pub fn job(&self, kind: JobKind) -> &JobStatus {
        self.jobs.get(&kind).unwrap_or_else(|| job_idle_default())
    }

    /// Generate-side frame rate, derived from `fps_index`.
    pub fn fps(&self) -> f64 {
        FPS_OPTIONS[self.fps_index].fps
    }

    /// Generate-side drop-frame flag, derived from `fps_index`.
    pub fn drop_frame(&self) -> bool {
        FPS_OPTIONS[self.fps_index].drop_frame
    }

    /// Decode-side frame rate, derived from `decode.fps_index`.
    pub fn decode_fps(&self) -> f64 {
        FPS_OPTIONS[self.decode.fps_index].fps
    }

    /// Decode-side drop-frame flag, derived from `decode.fps_index`.
    pub fn decode_drop_frame(&self) -> bool {
        FPS_OPTIONS[self.decode.fps_index].drop_frame
    }
}

fn job_idle_default() -> &'static JobStatus {
    static IDLE: OnceLock<JobStatus> = OnceLock::new();
    IDLE.get_or_init(JobStatus::idle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn initial_state() -> AppStateSnapshot {
        AppStateSnapshot::initial()
    }

    // ── Transport defaults ────────────────────────────────────────────────

    #[test]
    fn test_initial_is_playing_false() {
        assert!(!initial_state().is_playing);
    }

    #[test]
    fn test_initial_is_locked_false() {
        assert!(!initial_state().is_locked);
    }

    #[test]
    fn test_initial_start_timecode() {
        let s = initial_state();
        assert_eq!(s.start_timecode.hours, 1);
        assert_eq!(s.start_timecode.minutes, 0);
        assert_eq!(s.start_timecode.seconds, 0);
        assert_eq!(s.start_timecode.frames, 0);
    }

    #[test]
    fn test_initial_current_timecode_matches_start() {
        let s = initial_state();
        assert_eq!(s.current_timecode, s.start_timecode);
    }

    // ── FPS defaults ──────────────────────────────────────────────────────

    #[test]
    fn test_initial_fps() {
        let s = initial_state();
        assert_eq!(s.fps_index, 1);
        assert_eq!(s.fps(), 25.0);
        assert!(!s.drop_frame());
    }

    #[test]
    fn test_fps_accessors_track_index() {
        let mut s = initial_state();
        s.fps_index = 0;
        assert_eq!(s.fps(), 24.0);
        assert!(!s.drop_frame());
        s.fps_index = 3; // 29.97 DF
        assert!((s.fps() - 29.97).abs() < 0.01);
        assert!(s.drop_frame());
        s.decode.fps_index = 4; // 30
        assert_eq!(s.decode_fps(), 30.0);
        assert!(!s.decode_drop_frame());
    }

    // ── Audio routing defaults ────────────────────────────────────────────

    #[test]
    fn test_initial_audio_routing() {
        let s = initial_state();
        assert_eq!(s.ltc_channel, ChannelSel::Left);
        assert_eq!(s.beep_channel, ChannelSel::Right);
        assert!((s.ltc_volume - 0.25).abs() < 1e-6);
        assert!((s.beep_volume - 0.5).abs() < 1e-6);
        assert!((s.beep_frequency - 1000.0).abs() < 1e-6);
        assert!((s.beep_duration - 0.5).abs() < 1e-6);
    }

    // ── Audio device defaults ─────────────────────────────────────────────

    #[test]
    fn test_initial_audio_state() {
        let s = initial_state();
        assert!(s.devices.is_empty());
        assert!(s.selected_device.is_none());
        assert!(!s.audio_initialized);
        assert!(s.sample_format_name.is_empty());
    }

    #[test]
    fn test_initial_sample_rate_is_valid() {
        let s = initial_state();
        assert!(
            audio_core::SAMPLE_RATE_OPTIONS.contains(&s.sample_rate),
            "sample_rate {} should be one of {:?}",
            s.sample_rate,
            audio_core::SAMPLE_RATE_OPTIONS,
        );
    }

    // ── Clapper defaults ──────────────────────────────────────────────────

    #[test]
    fn test_initial_clapper() {
        let s = initial_state();
        assert_eq!(s.clapper.scene, 1);
        assert_eq!(s.clapper.take, 1);
        assert_eq!(s.clapper.roll, "A001");
        assert!(s.clapper.auto_increment_take);
    }

    #[test]
    fn test_initial_logs_empty() {
        assert!(initial_state().clapper.logs.is_empty());
    }

    // ── Animation defaults ────────────────────────────────────────────────

    #[test]
    fn test_initial_clap_seq_zero() {
        assert_eq!(initial_state().clapper.clap_seq, 0);
    }

    // ── Theme defaults ────────────────────────────────────────────────────

    #[test]
    fn test_initial_theme_light() {
        assert!(!initial_state().is_dark_theme);
    }

    // ── Status defaults ───────────────────────────────────────────────────

    #[test]
    fn test_initial_status() {
        let s = initial_state();
        // Audio owns the initial last-writer channel; every other channel
        // starts empty.
        assert_eq!(s.status.last, StatusChannel::Audio);
        assert!(!s.status.audio.is_empty());
        assert!(s.status.decode.is_empty());
        assert!(s.status.converter.is_empty());
        assert!(s.status.offload.is_empty());
    }

    #[test]
    fn test_status_message_follows_last_writer() {
        let mut s = StatusChannels::initial();
        s.set_decode("d1");
        assert_eq!(s.last, StatusChannel::Decode);
        assert_eq!(s.message(), "d1");
        s.set_audio("a1");
        assert_eq!(s.last, StatusChannel::Audio);
        assert_eq!(s.message(), "a1");
        s.set_converter("c1");
        assert_eq!(s.last, StatusChannel::Converter);
        assert_eq!(s.message(), "c1");
        s.set_offload("o1");
        assert_eq!(s.last, StatusChannel::Offload);
        assert_eq!(s.message(), "o1");
        // Channels are independent — later writes never clobber earlier ones.
        assert_eq!(s.decode, "d1");
        assert_eq!(s.audio, "a1");
        assert_eq!(s.converter, "c1");
    }

    // ── LTC decode defaults ───────────────────────────────────────────────

    #[test]
    fn test_initial_decode_state() {
        let s = initial_state();
        assert_eq!(s.decode.fps_index, 1);
        assert_eq!(s.decode_fps(), 25.0);
        assert!(!s.decode_drop_frame());
        assert!(s.decode.result.is_none());
        assert!(s.decode.error.is_none());
        assert_eq!(s.decode.generation, 0);
    }

    // ── LTC group decode defaults ──────────────────────────────────────────

    #[test]
    fn test_initial_group_decode_state() {
        let s = initial_state();
        assert_eq!(s.decode.group_generation, 0);
        assert!(s.decode.group_paths.is_empty());
        assert!(s.decode.group_results.is_empty());
    }

    #[test]
    fn test_initial_probe_is_none() {
        let s = initial_state();
        assert!(s.decode.probe.is_none());
    }

    // ── Clone produces independent copy ───────────────────────────────────

    #[test]
    fn test_state_clone_is_independent() {
        let mut a = AppStateSnapshot::initial();
        let mut b = a.clone();
        a.clapper.scene = 99;
        b.clapper.take = 88;
        assert_eq!(a.clapper.scene, 99);
        assert_eq!(b.clapper.scene, 1);
        assert_eq!(a.clapper.take, 1);
        assert_eq!(b.clapper.take, 88);
    }
}
