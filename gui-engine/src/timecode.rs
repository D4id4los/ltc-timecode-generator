use audio_core::Timecode;

// ── FPS options ─────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct FpsOption {
    pub name: &'static str,
    pub fps: f64,
    pub drop_frame: bool,
    pub description: &'static str,
}

pub const FPS_OPTIONS: &[FpsOption] = &[
    FpsOption {
        name: "24 fps",
        fps: 24.0,
        drop_frame: false,
        description: "Standard cinema & film frame rate.",
    },
    FpsOption {
        name: "25 fps",
        fps: 25.0,
        drop_frame: false,
        description: "PAL standard (Europe, UK, Australia, Africa, Asia).",
    },
    FpsOption {
        name: "29.97 ND",
        fps: 29.97,
        drop_frame: false,
        description: "NTSC Non-Drop (broadcast video & web production).",
    },
    FpsOption {
        name: "29.97 DF",
        fps: 29.97,
        drop_frame: true,
        description: "NTSC Drop Frame (syncs clock drift to wall-time).",
    },
    FpsOption {
        name: "30 fps",
        fps: 30.0,
        drop_frame: false,
        description: "High-definition video rate / digital audio standard.",
    },
];

// ── Formatting ──────────────────────────────────────────────────────────

pub fn timecode_to_string(tc: Timecode, drop_frame: bool) -> String {
    let sep = if drop_frame { ';' } else { ':' };
    format!(
        "{:02}:{:02}:{:02}{}{:02}",
        tc.hours, tc.minutes, tc.seconds, sep, tc.frames
    )
}

pub fn timecode_to_ms_string(tc: Timecode, fps: f64) -> String {
    let ms = (tc.frames as f64 / fps * 1000.0).round() as u32;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        tc.hours, tc.minutes, tc.seconds, ms
    )
}

pub fn chrono_now_string() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── timecode_to_string ────────────────────────────────────────────────

    #[test]
    fn test_timecode_to_string_non_drop() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        assert_eq!(timecode_to_string(tc, false), "01:02:03:04");
    }

    #[test]
    fn test_timecode_to_string_drop_frame() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        assert_eq!(timecode_to_string(tc, true), "01:02:03;04");
    }

    #[test]
    fn test_timecode_to_string_zero() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        assert_eq!(timecode_to_string(tc, false), "00:00:00:00");
    }

    #[test]
    fn test_timecode_to_string_max() {
        let tc = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 29 };
        assert_eq!(timecode_to_string(tc, false), "23:59:59:29");
    }

    #[test]
    fn test_timecode_to_string_single_digit_padding() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let s = timecode_to_string(tc, false);
        assert_eq!(s.len(), 11);
    }

    // ── timecode_to_ms_string ─────────────────────────────────────────────

    #[test]
    fn test_timecode_to_ms_string_25fps() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 0 };
        let s = timecode_to_ms_string(tc, 25.0);
        assert_eq!(s, "01:02:03.000");
    }

    #[test]
    fn test_timecode_to_ms_string_mid_frame() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 12 };
        let s = timecode_to_ms_string(tc, 25.0);
        assert_eq!(s, "00:00:00.480");
    }

    #[test]
    fn test_timecode_to_ms_string_last_frame() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 24 };
        let s = timecode_to_ms_string(tc, 25.0);
        assert_eq!(s, "00:00:00.960");
    }

    #[test]
    fn test_timecode_to_ms_string_30fps() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 15 };
        let s = timecode_to_ms_string(tc, 30.0);
        assert_eq!(s, "00:00:00.500");
    }

    #[test]
    fn test_timecode_to_ms_string_24fps() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 6 };
        let s = timecode_to_ms_string(tc, 24.0);
        assert_eq!(s, "00:00:00.250");
    }

    #[test]
    fn test_timecode_to_ms_string_full() {
        let tc = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 29 };
        let s = timecode_to_ms_string(tc, 30.0);
        assert_eq!(s, "23:59:59.967");
    }

    #[test]
    fn test_timecode_to_ms_string_2997() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 15 };
        let s = timecode_to_ms_string(tc, 29.97);
        assert!(s.starts_with("00:00:00."));
    }

    // ── FPS_OPTIONS invariants ────────────────────────────────────────────

    #[test]
    fn test_fps_options_all_valid() {
        for (i, opt) in FPS_OPTIONS.iter().enumerate() {
            assert!(!opt.name.is_empty(), "FPS_OPTIONS[{}] name is empty", i);
            assert!(!opt.description.is_empty(), "FPS_OPTIONS[{}] description is empty", i);
            assert!(opt.fps > 0.0, "FPS_OPTIONS[{}] fps = {} <= 0", i, opt.fps);
        }
    }

    #[test]
    fn test_fps_options_drop_frame_only_2997() {
        for (i, opt) in FPS_OPTIONS.iter().enumerate() {
            if opt.drop_frame {
                assert!(
                    (opt.fps - 29.97).abs() < 0.01,
                    "FPS_OPTIONS[{}]: drop_frame=true but fps={}, not 29.97",
                    i, opt.fps,
                );
                assert_eq!(
                    opt.name, "29.97 DF",
                    "FPS_OPTIONS[{}]: only '29.97 DF' should be drop-frame, got '{}'",
                    i, opt.name,
                );
            }
        }
    }

    #[test]
    fn test_fps_options_count() {
        assert_eq!(FPS_OPTIONS.len(), 5);
    }

    #[test]
    fn test_fps_options_expected_order() {
        assert_eq!(FPS_OPTIONS[0].name, "24 fps");
        assert_eq!(FPS_OPTIONS[0].fps, 24.0);
        assert_eq!(FPS_OPTIONS[1].name, "25 fps");
        assert_eq!(FPS_OPTIONS[1].fps, 25.0);
        assert_eq!(FPS_OPTIONS[2].name, "29.97 ND");
        assert_eq!(FPS_OPTIONS[3].name, "29.97 DF");
        assert_eq!(FPS_OPTIONS[3].drop_frame, true);
        assert_eq!(FPS_OPTIONS[4].name, "30 fps");
        assert_eq!(FPS_OPTIONS[4].fps, 30.0);
    }

    // ── chrono_now_string ─────────────────────────────────────────────────

    #[test]
    fn test_chrono_now_string_format() {
        let s = chrono_now_string();
        assert_eq!(s.len(), 8, "expected HH:MM:SS format, got '{}'", s);
        assert_eq!(s.as_bytes()[2], b':', "expected colon at position 2, got '{}'", s);
        assert_eq!(s.as_bytes()[5], b':', "expected colon at position 5, got '{}'", s);
        for ch in s.chars().filter(|&c| c != ':') {
            assert!(ch.is_ascii_digit(), "expected digit, got '{}' in '{}'", ch, s);
        }
    }

    // ── Timecode struct ───────────────────────────────────────────────────

    #[test]
    fn test_timecode_clone() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let cloned = tc;
        assert_eq!(cloned.hours, 1);
        assert_eq!(cloned.minutes, 2);
        assert_eq!(cloned.seconds, 3);
        assert_eq!(cloned.frames, 4);
    }

    #[test]
    fn test_timecode_partial_eq() {
        let a = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let b = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let c = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}