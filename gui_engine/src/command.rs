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

    // ── Parsing / shutdown ──────────────────────────────────────────────
    ParseLtcFile(String),
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