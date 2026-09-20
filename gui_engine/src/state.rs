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

    // LTC decode FPS
    pub decode_fps_index: usize,
    pub decode_fps: f64,
    pub decode_drop_frame: bool,

    // LTC file decode
    pub use_libltc: bool,   // decoder selection (set from CLI --decoder flag; false = builtin, true = libltc)
    pub ltc_decode_result: Option<LtcDetectionResult>,
    pub ltc_decode_error: Option<String>,
    pub ltc_is_detecting: bool,
    pub ltc_decode_generation: u64,

    // Chunked decode progress
    pub ltc_decode_progress_pct: f32,       // 0.0..1.0
    pub ltc_decode_progress_str: String,    // "Chunk 3/12..."
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
            decode_fps_index: 1,
            decode_fps: 25.0,
            decode_drop_frame: false,
            use_libltc: false,
            ltc_decode_result: None,
            ltc_decode_error: None,
            ltc_is_detecting: false,
            ltc_decode_generation: 0,
            ltc_decode_progress_pct: 0.0,
            ltc_decode_progress_str: String::new(),
        }
    }
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
        assert_eq!(s.fps, 25.0);
        assert!(!s.drop_frame);
    }

    // ── Audio routing defaults ────────────────────────────────────────────

    #[test]
    fn test_initial_audio_routing() {
        let s = initial_state();
        assert_eq!(s.ltc_channel, "left");
        assert_eq!(s.beep_channel, "right");
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
        assert_eq!(s.selected_device, 0);
        assert!(!s.audio_initialized);
        assert!(s.sample_format_name.is_empty());
        assert!(!s.wake_lock_active);
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
        assert_eq!(s.scene, 1);
        assert_eq!(s.take, 1);
        assert_eq!(s.roll, "A001");
        assert!(s.auto_increment_take);
    }

    #[test]
    fn test_initial_logs_empty() {
        assert!(initial_state().logs.is_empty());
    }

    // ── Animation defaults ────────────────────────────────────────────────

    #[test]
    fn test_initial_animation() {
        let s = initial_state();
        assert_eq!(s.clap_flash_alpha, 0.0);
        assert!((s.clap_arm_angle - (-25.0f32).to_radians()).abs() < 1e-6);
    }

    #[test]
    fn test_initial_clap_arm_angle_negative() {
        let s = initial_state();
        assert!(s.clap_arm_angle < 0.0);
    }

    // ── Theme defaults ────────────────────────────────────────────────────

    #[test]
    fn test_initial_theme_light() {
        assert!(!initial_state().is_dark_theme);
    }

    // ── Status defaults ───────────────────────────────────────────────────

    #[test]
    fn test_initial_status() {
        assert_eq!(initial_state().status_message, "Ready");
    }

    // ── LTC decode defaults ───────────────────────────────────────────────

    #[test]
    fn test_initial_decode_state() {
        let s = initial_state();
        assert_eq!(s.decode_fps_index, 1);
        assert_eq!(s.decode_fps, 25.0);
        assert!(!s.decode_drop_frame);
        assert!(!s.use_libltc);
        assert!(s.ltc_decode_result.is_none());
        assert!(s.ltc_decode_error.is_none());
        assert!(!s.ltc_is_detecting);
        assert_eq!(s.ltc_decode_generation, 0);
    }

    // ── Generation field ──────────────────────────────────────────────────

    #[test]
    fn test_initial_generation_zero() {
        assert_eq!(initial_state().generation, 0);
    }

    // ── Clone produces independent copy ───────────────────────────────────

    #[test]
    fn test_state_clone_is_independent() {
        let mut a = AppStateSnapshot::initial();
        let mut b = a.clone();
        a.scene = 99;
        b.take = 88;
        assert_eq!(a.scene, 99);
        assert_eq!(b.scene, 1);
        assert_eq!(a.take, 1);
        assert_eq!(b.take, 88);
    }
}