//! Camera model detection via external metadata tools.
//!
//! Provides a single probe function that tries exiftool first (handles AVCHD
//! MTS/M2TS, MP4, MOV, any format), then falls back to ffprobe format-tag
//! extraction (MP4/MOV with Apple/ISOM metadata).
//!
//! All probe functions accept an injectable command runner for testability
//! (see `probe_camera_info_with` / `test_encode_with` in `hw_device.rs`).

use std::io;
use std::path::Path;
use std::process::Output;

/// How the camera make/model was obtained.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CameraMetaSource {
    /// From exiftool `-json -Make -Model` output.
    ExifTool,
    /// From ffprobe `-show_entries format_tags` output.
    FfprobeTags,
}

/// Detected camera make and model.
#[derive(Debug, Clone)]
pub struct CameraInfo {
    /// Manufacturer name (e.g. "Sony", "Canon").
    pub make: Option<String>,
    /// Camera model name (e.g. "NEX-FS100EK", "ILCE-6700").
    pub model: Option<String>,
    /// How the info was obtained.
    pub source: CameraMetaSource,
}

/// Convenience wrapper — probes `path` for camera info using real subprocesses.
pub fn probe_camera_info(path: &Path) -> Option<CameraInfo> {
    probe_camera_info_with(path, &mut |prog, args| {
        crate::subprocess::no_window_command(prog)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
    })
}

/// Core probe function with injectable command `runner`.
///
/// `runner` receives `(program_name, &[arg_strings])` and must return
/// `io::Result<Output>` matching what the real program would produce.
///
/// Strategy:
/// 1. Try `exiftool -json -Make -Model <path>` — covers AVCHD SEI, MP4/MOV
///    tags, RAW sidecars, etc. Returns early on success.
/// 2. Fallback: `ffprobe -v quiet -print_format json -show_entries
///    format_tags <path>` — works for most MP4/MOV cameras without exiftool.
pub fn probe_camera_info_with(
    path: &Path,
    runner: &mut dyn FnMut(&str, &[String]) -> io::Result<Output>,
) -> Option<CameraInfo> {
    // ── 1. exiftool path ──────────────────────────────────────────────
    let exif_args: Vec<String> = vec![
        "-json".into(),
        "-Make".into(),
        "-Model".into(),
        path.to_string_lossy().into(),
    ];
    if let Ok(output) = runner("exiftool", &exif_args) {
        if output.status.success() {
            if let Some(info) = parse_exiftool_json(&String::from_utf8_lossy(&output.stdout)) {
                return Some(info);
            }
        }
    }

    // ── 2. ffprobe fallback ───────────────────────────────────────────
    let ff_args: Vec<String> = vec![
        "-v".into(),
        "quiet".into(),
        "-print_format".into(),
        "json".into(),
        "-show_entries".into(),
        "format_tags".into(),
        path.to_string_lossy().into(),
    ];
    if let Ok(output) = runner("ffprobe", &ff_args) {
        if output.status.success() {
            if let Some(info) = parse_ffprobe_tags(&String::from_utf8_lossy(&output.stdout)) {
                return Some(info);
            }
        }
    }

    None
}

/// Parse exiftool `-json -Make -Model` output.
///
/// Expected format:
/// ```json
/// [{"SourceFile":"/path","Make":"Sony","Model":"NEX-FS100EK"}]
/// ```
fn parse_exiftool_json(stdout: &str) -> Option<CameraInfo> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(stdout).ok()?;
    let entry = arr.first()?;
    let model = entry.get("Model")?.as_str()?.to_string();
    if model.is_empty() {
        return None;
    }
    let make = entry
        .get("Make")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    Some(CameraInfo {
        make,
        model: Some(model),
        source: CameraMetaSource::ExifTool,
    })
}

/// Parse ffprobe `-show_entries format_tags` JSON output.
///
/// Expected structure (in `format.tags`):
/// ```json
/// {"format":{"tags":{"make":"Sony","model":"NEX-FS100EK"}}}
/// ```
/// Also handles Apple-style keys:
/// `com.apple.quicktime.make` / `com.apple.quicktime.model`.
/// The `encoder` tag is tried as a secondary make source.
fn parse_ffprobe_tags(stdout: &str) -> Option<CameraInfo> {
    let value: serde_json::Value = serde_json::from_str(stdout).ok()?;
    let tags = value.get("format")?.get("tags")?;

    let make = tags
        .get("make")
        .or_else(|| tags.get("com.apple.quicktime.make"))
        .or_else(|| tags.get("encoder"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let model = tags
        .get("model")
        .or_else(|| tags.get("com.apple.quicktime.model"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())?;

    Some(CameraInfo {
        make,
        model: Some(model),
        source: CameraMetaSource::FfprobeTags,
    })
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // ── Test helpers ──────────────────────────────────────────────────

    fn ok_with(stdout: &[u8]) -> Output {
        let status = Command::new("true").status().unwrap();
        Output {
            status,
            stdout: stdout.to_vec(),
            stderr: vec![],
        }
    }

    fn failure() -> Output {
        let status = Command::new("false").status().unwrap();
        Output {
            status,
            stdout: vec![],
            stderr: vec![],
        }
    }

    fn not_found() -> io::Result<Output> {
        Err(io::Error::new(io::ErrorKind::NotFound, "not found"))
    }

    // ── parse_exiftool_json ───────────────────────────────────────────

    #[test]
    fn test_exiftool_fs100() {
        let json = r#"[{"SourceFile":"/test.MTS","Make":"Sony","Model":"NEX-FS100EK"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("NEX-FS100EK".to_string()));
        assert_eq!(info.source, CameraMetaSource::ExifTool);
    }

    #[test]
    fn test_exiftool_no_make() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"","Model":"EOS R5"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.make, None);
        assert_eq!(info.model, Some("EOS R5".to_string()));
    }

    #[test]
    fn test_exiftool_empty_model_returns_none() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"Sony","Model":""}]"#;
        assert!(parse_exiftool_json(json).is_none());
    }

    #[test]
    fn test_exiftool_missing_model_key_returns_none() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"Sony"}]"#;
        assert!(parse_exiftool_json(json).is_none());
    }

    #[test]
    fn test_exiftool_invalid_json_returns_none() {
        assert!(parse_exiftool_json("not json").is_none());
        assert!(parse_exiftool_json("{}").is_none());
    }

    // ── parse_ffprobe_tags ────────────────────────────────────────────

    #[test]
    fn test_ffprobe_tags_standard() {
        let json = r#"{"format":{"tags":{"make":"Sony","model":"NEX-FS100EK"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("NEX-FS100EK".to_string()));
        assert_eq!(info.source, CameraMetaSource::FfprobeTags);
    }

    #[test]
    fn test_ffprobe_tags_apple_keys() {
        let json = r#"{"format":{"tags":{"com.apple.quicktime.make":"GoPro","com.apple.quicktime.model":"HERO9 Black"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, Some("GoPro".to_string()));
        assert_eq!(info.model, Some("HERO9 Black".to_string()));
    }

    #[test]
    fn test_ffprobe_tags_encoder_as_make() {
        let json = r#"{"format":{"tags":{"encoder":"GoPro HERO9 Black","model":"HERO9 Black"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, Some("GoPro HERO9 Black".to_string()));
        assert_eq!(info.model, Some("HERO9 Black".to_string()));
    }

    #[test]
    fn test_ffprobe_tags_no_make() {
        let json = r#"{"format":{"tags":{"model":"GH6"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, None);
        assert_eq!(info.model, Some("GH6".to_string()));
    }

    #[test]
    fn test_ffprobe_tags_no_model_returns_none() {
        let json = r#"{"format":{"tags":{"make":"Sony"}}}"#;
        assert!(parse_ffprobe_tags(json).is_none());
    }

    #[test]
    fn test_ffprobe_tags_empty_format_returns_none() {
        let json = r#"{"format":{}}"#;
        assert!(parse_ffprobe_tags(json).is_none());
    }

    #[test]
    fn test_ffprobe_tags_invalid_json_returns_none() {
        assert!(parse_ffprobe_tags("").is_none());
        assert!(parse_ffprobe_tags("{}").is_none());
    }

    // ── integration: probe_camera_info_with ───────────────────────────

    #[test]
    fn test_probe_with_exiftool_success() {
        let path = Path::new("/media/card/00001.MTS");
        let json = r#"[{"SourceFile":"/media/card/00001.MTS","Make":"Sony","Model":"NEX-FS100EK"}]"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("NEX-FS100EK".to_string()));
        assert_eq!(info.source, CameraMetaSource::ExifTool);
    }

    #[test]
    fn test_probe_exiftool_fails_ffprobe_success() {
        let path = Path::new("/media/card/00001.MP4");
        let json = r#"{"format":{"tags":{"make":"GoPro","model":"HERO9 Black"}}}"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => Ok(failure()),
            "ffprobe" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner).unwrap();
        assert_eq!(info.make, Some("GoPro".to_string()));
        assert_eq!(info.model, Some("HERO9 Black".to_string()));
        assert_eq!(info.source, CameraMetaSource::FfprobeTags);
    }

    #[test]
    fn test_probe_exiftool_not_found_ffprobe_success() {
        let path = Path::new("/media/card/00001.mp4");
        let json = r#"{"format":{"tags":{"model":"GH6"}}}"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => not_found(),
            "ffprobe" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner).unwrap();
        assert_eq!(info.make, None);
        assert_eq!(info.model, Some("GH6".to_string()));
    }

    #[test]
    fn test_probe_both_fail_returns_none() {
        let path = Path::new("/media/card/00001.bin");
        let mut runner = |_: &str, _: &[String]| Ok(failure());
        assert!(probe_camera_info_with(path, &mut runner).is_none());
    }

    #[test]
    fn test_probe_both_not_found_returns_none() {
        let path = Path::new("/media/card/no_file.txt");
        let mut runner = |_: &str, _: &[String]| not_found();
        assert!(probe_camera_info_with(path, &mut runner).is_none());
    }
}