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