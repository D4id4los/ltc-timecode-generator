use audio_core::{AudioDeviceInfo, AudioEvent, LtcDetectionResult, Timecode};

// ── Clap log entry ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ClapLogItem {
    pub id: String,
    pub timestamp: String,
    pub timecode: String,
    pub milliseconds: String,
    pub note: String,
}

// ── Application state snapshot ─────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct AppStateSnapshot {
    pub generation: u64,

    // Transport
    pub is_playing: bool,
    pub is_locked: bool,
    pub current_timecode: Timecode,
    pub start_timecode: Timecode,

    // FPS
    pub fps_index: usize,
    pub fps: f64,
    pub drop_frame: bool,

    // Audio routing
    pub ltc_channel: String,
    pub beep_channel: String,
    pub ltc_volume: f32,
    pub beep_volume: f32,
    pub beep_frequency: f32,
    pub beep_duration: f32,

    // Audio device
    pub devices: Vec<AudioDeviceInfo>,
    pub selected_device: usize,
    pub audio_initialized: bool,
    pub sample_rate: u32,
    pub sample_format_name: String,
    pub wake_lock_active: bool,

    // Clapper metadata
    pub scene: u32,
    pub take: u32,
    pub roll: String,
    pub auto_increment_take: bool,
    pub logs: Vec<ClapLogItem>,

    // Animation (engine-computed)
    pub clap_flash_alpha: f32,
    pub clap_arm_angle: f32,

    // Theme
    pub is_dark_theme: bool,

    // Status
    pub status_message: String,
    pub system_time: String,

    // Events drained from AudioCore (to be surfaced as toasts by the GUI)
    pub events: Vec<AudioEvent>,

    // LTC file decode
    pub use_libltc: bool,   // decoder selection (set from CLI --decoder flag; false = builtin, true = libltc)
    pub ltc_decode_result: Option<LtcDetectionResult>,
    pub ltc_decode_error: Option<String>,
    pub ltc_is_detecting: bool,
    pub ltc_decode_generation: u64,
}

impl AppStateSnapshot {
    pub fn initial() -> Self {
        let suggest_sr = audio_core::suggest_sample_rate();
        let _suggest_sr_index = audio_core::SAMPLE_RATE_OPTIONS
            .iter()
            .position(|&r| r == suggest_sr)
            .unwrap_or(0);

        AppStateSnapshot {
            generation: 0,
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
            fps: 25.0,
            drop_frame: false,
            ltc_channel: "left".to_string(),
            beep_channel: "right".to_string(),
            ltc_volume: 0.25,
            beep_volume: 0.5,
            beep_frequency: 1000.0,
            beep_duration: 0.5,
            devices: Vec::new(),
            selected_device: 0,
            audio_initialized: false,
            sample_rate: suggest_sr,
            sample_format_name: String::new(),
            wake_lock_active: false,
            scene: 1,
            take: 1,
            roll: "A001".to_string(),
            auto_increment_take: true,
            logs: Vec::new(),
            clap_flash_alpha: 0.0,
            clap_arm_angle: -25.0f32.to_radians(),
            is_dark_theme: false,
            status_message: "Ready".to_string(),
            system_time: String::new(),
            events: Vec::new(),
            use_libltc: false,
            ltc_decode_result: None,
            ltc_decode_error: None,
            ltc_is_detecting: false,
            ltc_decode_generation: 0,
        }
    }
}