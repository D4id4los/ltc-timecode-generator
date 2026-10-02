//! Single source of truth for media file-extension classification.
//!
//! Video-extension lists previously drifted across five sites
//! (`ffprobe`, `device_name`, `offload`, `converter/formats`, `tagger`) —
//! e.g. an `.m4v` file was ingested by offload but `path_is_video()`
//! rejected it. All subsystems must classify extensions through this
//! module.

use std::path::Path;

/// Video container extensions recognised across all subsystems
/// (card offload, device naming, decode auto-detection, converter,
/// tagger). Lowercase, without the leading dot.
pub const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "mov", "mkv", "mts", "m2ts", "m2t", "ts", "mxf", "avi", "webm",
];

/// Audio extensions recognised as media (currently WAV recordings).
pub const AUDIO_EXTENSIONS: &[&str] = &["wav"];

fn path_ext_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
}

/// True if `path` has a video extension from [`VIDEO_EXTENSIONS`].
pub fn is_video(path: &Path) -> bool {
    path_ext_lower(path)
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
}

/// True if `path` has an audio extension from [`AUDIO_EXTENSIONS`].
pub fn is_audio(path: &Path) -> bool {
    path_ext_lower(path)
        .is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.as_str()))
}

/// Map an input container extension to the output container used when
/// stream-copy remuxing. Unknown extensions fall back to `"mkv"`.
pub fn container_for_input(ext: &str) -> &'static str {
    match ext.to_ascii_lowercase().as_str() {
        "mp4" | "m4v" => "mp4",
        "mov" => "mov",
        "mkv" => "mkv",
        "mxf" => "mxf",
        "mts" | "m2ts" | "m2t" | "ts" => "mp4",
        _ => "mkv",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn test_is_video_registry() {
        for ext in VIDEO_EXTENSIONS {
            assert!(is_video(&p(&format!("clip.{}", ext))), "clip.{}", ext);
            assert!(is_video(&p(&format!("CLIP.{}", ext.to_uppercase()))));
        }
        assert!(!is_video(&p("tone.wav")));
        assert!(!is_video(&p("noext")));
        assert!(!is_video(&p("notes.txt")));
    }

    #[test]
    fn test_is_audio() {
        assert!(is_audio(&p("take01.WAV")));
        assert!(!is_audio(&p("clip.mp4")));
    }

    #[test]
    fn test_container_for_input() {
        assert_eq!(container_for_input("mp4"), "mp4");
        assert_eq!(container_for_input("M4V"), "mp4");
        assert_eq!(container_for_input("mov"), "mov");
        assert_eq!(container_for_input("mkv"), "mkv");
        assert_eq!(container_for_input("mxf"), "mxf");
        assert_eq!(container_for_input("mts"), "mp4");
        assert_eq!(container_for_input("m2t"), "mp4");
        assert_eq!(container_for_input("avi"), "mkv");
        assert_eq!(container_for_input("unknown"), "mkv");
    }
}
