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
    /// Clear accumulated group decode results (sent on group re-selection).
    ClearLtcGroupResults,
    SetLtcDecodeStream(usize),
    SetLtcDecodeChannel(usize),

    // ── Duration probe ─────────────────────────────────────────────────
    /// Probe file durations for converter recording groups.
    /// The engine spawns a background worker that publishes results into
    /// `AppStateSnapshot.file_durations` as they become available.
    ProbeFileDurations(Vec<PathBuf>),

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