use std::path::PathBuf;

use audio_core::{ChannelSel, Timecode};

#[derive(Debug, Clone)]
pub enum GuiCommand {
    // ── Transport ────────────────────────────────────────────────────────
    StartLtc,
    StopLtc,
    Reset,
    ToggleLock,
    Clap,

    // ── Timecode / FPS ───────────────────────────────────────────────────
    SetStartTimecode(Timecode),
    SetFpsIndex(usize),

    // ── Audio ────────────────────────────────────────────────────────────
    InitAudio,
    SetSampleRate(u32),
    SetDevice(String),
    RefreshDevices,
    SetLtcChannel(ChannelSel),
    SetBeepChannel(ChannelSel),
    SetLtcVolume(f32),
    SetBeepVolume(f32),
    SetBeepFrequency(f32),
    SetBeepDuration(f32),

    // ── Clapper metadata ─────────────────────────────────────────────────
    SetScene(u32),
    SetTake(u32),
    SetRoll(String),
    SetAutoIncrement(bool),

    // ── Theme ───────────────────────────────────────────────────────────
    SetTheme(bool),
    ToggleTheme,

    // ── Accessibility ───────────────────────────────────────────────────
    /// Global text scale in percent (e.g. 150 = large-text variant).
    SetTextScale(u32),

    // ── Logs ────────────────────────────────────────────────────────────
    ClearLogs,

    // ── Decode FPS ──────────────────────────────────────────────────────
    SetDecodeFpsIndex(usize),

    // ── LTC decode (WAV) ───────────────────────────────────────────────
    ParseLtcWavFile(String),

    // ── LTC decode (video) ─────────────────────────────────────────────
    ProbeVideo(String),
    ParseLtcVideo(String, usize, usize),
    /// Decode LTC from every video clip in a recording group.
    /// `paths` are the full filesystem paths of all clips.
    DecodeLtcVideoGroup {
        paths: Vec<String>,
        stream_index: usize,
        channel_index: usize,
    },
    /// Clear all per-recording decode state: single-file results, group
    /// results, probes, cached subclip scans.  Sent on group re-selection
    /// so stale results from the previous recording are not shown.
    ClearRecordingDecodeState,
    SetLtcDecodeStream(usize),
    SetLtcDecodeChannel(usize),

    // ── Duration probe ─────────────────────────────────────────────────
    /// Probe file durations for converter recording groups.
    /// The engine spawns a background worker that publishes results into
    /// `AppStateSnapshot.file_durations` as they become available.
    ProbeFileDurations(Vec<PathBuf>),

    // ── Converter (engine-owned) ───────────────────────────────────────
    /// Manage converter state in the engine.
    Converter(ConverterCommand),

    // ── Offload / file ingestion (engine-owned) ────────────────────────
    /// Manage offload state in the engine.
    Offload(OffloadCommand),

    // ── Recording sessions (engine-owned) ──────────────────────────────
    /// Manage the recording-session lifecycle (create/load/close/gap).
    Session(SessionCommand),

    // ── Session default conversion (engine-owned) ──────────────────────
    /// Preflight + one-click conversion of all session recordings using
    /// the persisted `SessionConversionDefaults` profile.
    SessionConversion(SessionConversionCommand),

    // ── Shutdown ────────────────────────────────────────────────────────
    CancelDecode,
    Shutdown,

    // ── Steppers ─────────────────────────────────────────────────────────
    SceneUp,
    SceneDown,
    TakeUp,
    TakeDown,
    HourUp,
    HourDown,
    MinuteUp,
    MinuteDown,
    SecondUp,
    SecondDown,
    FrameUp,
    FrameDown,
}

/// Commands for the engine-owned converter subsystem.
/// All user-facing converter options are engine-owned via
/// `ConverterUserSettings`; the GUI sends fine-grained commands
/// for each selection change.
#[derive(Debug, Clone)]
pub enum ConverterCommand {
    // ── Folder / Recording ──────────────────────────────────────────────
    /// Scan a folder and discover recording groups.
    SelectFolder(PathBuf),
    /// Select a recording group by index (resets per-recording state
    /// and prefills naming defaults).
    SelectRecording(usize),

    // ── Pipeline mode ──────────────────────────────────────────────────
    /// Toggle "metadata only — tag + rename, extract audio" mode.
    SetMetadataOnly(bool),
    /// Toggle synthetic video generation (audio-only pipeline).
    SetGenerateSyntheticVideo(bool),
    /// Toggle stream-copy mode ("leave video encoding untouched").
    SetCopyVideo(bool),

    // ── Track handling ─────────────────────────────────────────────────
    /// Toggle split-tracks mode.
    SetSplitTracks(bool),
    /// Toggle dropping the LTC track from output.
    SetDropLtcTrack(bool),
    /// Toggle concatenating audio across clips.
    SetConcatAudio(bool),
    /// Toggle embedding start timecode from LTC decode results.
    SetStartFromLtc(bool),
    /// Toggle embedding camera metadata (make/model, bext originator) in outputs.
    SetEmbedCameraMetadata(bool),
    /// Set which file/channel carries LTC (audio groups).
    SetLtcFileIndex(usize),
    /// Swap two cells in the channel map.
    SwapChannelMapCells(usize, usize),

    // ── Per-file selection ─────────────────────────────────────────────
    /// Toggle selection of one file in a recording group. Deselected files
    /// are excluded from the conversion (they flow through
    /// `plan_output_paths` via the assembled settings' input subset).
    SetFileSelected {
        group: usize,
        path: PathBuf,
        selected: bool,
    },
    /// Select or deselect all files of a recording group.
    SetAllGroupFiles { group: usize, selected: bool },

    // ── Format / Codec ──────────────────────────────────────────────────
    /// Set the output container id (e.g. "mov", "mkv", "mp4").
    SetContainer(String),
    /// Set the video codec id (e.g. "h265", "av1").
    SetVideoCodec(String),
    /// Set the audio encoder id (e.g. "pcm_s24le", "aac").
    SetAudioEncoder(String),

    // ── Output paths ────────────────────────────────────────────────────
    /// Set the output folder.
    SetOutputFolder(PathBuf),
    /// Set the filename prefix for output files.
    SetFilenamePrefix(String),
    /// Set the audio suffix template.
    SetAudioSuffixTemplate(String),
    /// Set the video suffix template.
    SetVideoSuffixTemplate(String),

    // ── Conversion lifecycle ───────────────────────────────────────────
    /// Start conversion with current settings.
    StartConversion,
    /// Cancel an in-flight conversion.
    CancelConversion,
}

/// Commands for the engine-owned offload / file-ingestion subsystem.
#[derive(Debug, Clone)]
pub enum OffloadCommand {
    /// Rescan all mounted media cards and update the card list.
    ScanCards,
    /// Set the parent folder (base directory) for this offload session.
    SetParentFolder(PathBuf),
    /// Set the parent folder name (e.g. the ISO date subfolder).
    SetParentName(String),
    /// Override the device folder name for a detected card by index.
    SetDeviceName(usize, String),
    /// Override the device folder name for a detected card by mount path.
    /// Stable across rescans that reorder the card list (unlike the index
    /// variant), so GUIs should prefer this one.
    SetDeviceNameByMount(PathBuf, String),
    /// Toggle selection of a single file on a card.
    SetFileSelected(usize, usize, bool),
    /// Select or deselect all files on a card.
    SetAllFilesSelected(usize, bool),
    /// Select only files from the latest recording day on a card.
    SelectLatestDay(usize),
    /// Start copying files from all pending cards.
    StartOffload,
    /// Cancel an in-flight copy operation.
    CancelOffload,
}

/// Commands for the engine-owned recording-session lifecycle.
#[derive(Debug, Clone)]
pub enum SessionCommand {
    /// Create a session from the current transport state: records fps,
    /// drop-frame and the current start TC; makes it the active session.
    Create { name: String },
    /// Load a persisted session: stops playback, applies the session's
    /// fps/drop-frame, and sets start/resume TC to last emitted + gap.
    Load { id: String },
    /// Close the active session (persists last-emitted first). Playback
    /// state itself is untouched; a playing stream keeps playing.
    Close,
    /// Set the per-session continue gap in seconds (applied on load).
    SetContinueGapSeconds(u64),
}

/// Commands for the session default conversion subsystem (phase 4):
/// preflight-decode every recording in the active session's offload
/// destinations, report per-file resolved LTC channels, and run the
/// owner's default pipelines. Never auto-run — button + preflight report.
#[derive(Debug, Clone)]
pub enum SessionConversionCommand {
    /// Preflight-decode every recording of the active session's offload
    /// destinations (channel resolution: configured → pinned → scan).
    RunPreflight,
    /// Run the default pipelines over the preflight-validated groups.
    /// Ignored while preflight failures are unresolved (missing LTC is a
    /// real error — fix channel defaults or convert that recording
    /// manually).
    StartRun,
    /// Cancel the in-flight preflight or run.
    Cancel,
    /// Clear the preflight report (unblocks the UI after acknowledging).
    ClearPreflight,
    // ── Defaults setters (persisted profile) ────────────────────────────
    SetAudioLtcChannelOverride(Option<usize>),
    SetVideoLtcStream(usize),
    SetVideoLtcChannel(usize),
    SetAudioLtcFileIdx(usize),
    SetVideoContainerOverride(Option<String>),
    SetVideoCodecOverride(Option<String>),
    SetAudioEncoderOverride(Option<String>),
    SetSplitTracks(bool),
    SetDropLtcTrack(bool),
    SetStartFromLtc(bool),
    SetEmbedCameraMetadata(bool),
}
