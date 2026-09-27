use std::path::PathBuf;

use audio_core::Timecode;

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
    SetDevice(usize),
    RefreshDevices,
    SetLtcChannel(String),
    SetBeepChannel(String),
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
    DecodeLtcVideoGroup { paths: Vec<String>, stream_index: usize, channel_index: usize },
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
#[derive(Debug, Clone)]
pub enum ConverterCommand {
    /// Scan a folder and discover recording groups.
    SelectFolder(PathBuf),
    /// Select a recording group by index (resets per-recording state).
    SelectRecording(usize),
    /// Toggle trim to first LTC frame.
    SetTrimEnabled(bool),
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
    /// Start copying files from all pending cards.
    StartOffload,
    /// Cancel an in-flight copy operation.
    CancelOffload,
}