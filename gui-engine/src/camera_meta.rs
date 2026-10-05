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

/// Detected camera make and model, plus extended metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    /// Manufacturer name (e.g. "Sony", "Canon").
    pub make: Option<String>,
    /// Camera model name (e.g. "NEX-FS100EK", "ILCE-6700").
    pub model: Option<String>,
    /// How the info was obtained.
    pub source: CameraMetaSource,
    /// Creation date of the media, normalized as `YYYY-MM-DD`.
    pub creation_date: Option<String>,
    /// Lens model (e.g. "E PZ 18-105mm F4 G OSS").
    pub lens: Option<String>,
    /// Camera body serial number (None if unset/0xFFFFFFFF sentinel).
    pub serial: Option<String>,
    /// Full creation timestamp with timezone, normalized to ISO-8601
    /// (e.g. "2026-09-25T20:45:22+01:00").
    pub creation_time: Option<String>,
    /// Capture gamma / colour equation (e.g. "rec709-xvycc", "ex-cine4").
    pub gamma: Option<String>,
    /// Native camera timecode at start-of-file, formatted as HH:MM:SS:FF
    /// (from XAVC LtcChangeTableLtcChangeValue or AVCHD H264:TimeCode).
    pub native_timecode: Option<String>,
    /// Human-readable summary of shooting parameters (aperture, shutter,
    /// gain, white balance, focus, exposure program, image stabilisation).
    pub exposure_summary: Option<String>,
}

/// Convenience wrapper — probes `path` for camera info using real subprocesses
/// with a 10-second timeout per subprocess.
pub fn probe_camera_info(path: &Path) -> Option<CameraInfo> {
    probe_camera_info_with(path, &mut run_probe_program)
}

/// Default probe runner: spawn `prog` with `args` under the probe timeout,
/// mapping the shared subprocess failure type onto `io::Error`.
pub(crate) fn run_probe_program(prog: &str, args: &[String]) -> io::Result<Output> {
    crate::subprocess::run_output_with_timeout(
        crate::subprocess::no_window_command(prog)
            .args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null()),
        crate::subprocess::PROBE_TIMEOUT,
    )
    .map_err(|e| match e {
        crate::subprocess::SubprocessFailure::Io(msg) => io::Error::other(msg),
        crate::subprocess::SubprocessFailure::TimedOut => {
            io::Error::new(io::ErrorKind::TimedOut, "probe timed out")
        }
        // run_output_with_timeout never produces these, but the match
        // must stay exhaustive; treat like a generic failure.
        crate::subprocess::SubprocessFailure::NonZeroExit { stderr_tail } => {
            io::Error::other(stderr_tail)
        }
        crate::subprocess::SubprocessFailure::Parse(msg) => io::Error::other(msg),
    })
}

/// Core probe function with injectable command `runner`.
///
/// `runner` receives `(program_name, &[arg_strings])` and must return
/// `io::Result<Output>` matching what the real program would produce.
///
/// Strategy:
/// 1. Try `exiftool -json -Make -Model ... <path>` — covers AVCHD SEI,
///    XAVC XML timed-metadata, MOV/MP4 tags, RAW sidecars, etc.  Tags:
///    `-Make -Model -CreateDate -DateTimeOriginal -DeviceManufacturer
///    -DeviceModelName -LensModel -LensModelName -SerialNumber
///    -DeviceSerialNo -CreationDateValue -CaptureGammaEquation
///    -LtcChangeTableLtcChangeValue -TimeCode -ExposureTime -FNumber
///    -ISO -Gain -WhiteBalance -Focus -ImageStabilization
///    -ExposureProgram -ApertureSetting`.
///    Returns early on success.
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
        "-CreateDate".into(),
        "-DateTimeOriginal".into(),
        "-DeviceManufacturer".into(),
        "-DeviceModelName".into(),
        "-LensModel".into(),
        "-LensModelName".into(),
        "-SerialNumber".into(),
        "-DeviceSerialNo".into(),
        "-CreationDateValue".into(),
        "-CaptureGammaEquation".into(),
        "-LtcChangeTableLtcChangeValue".into(),
        "-TimeCode".into(),
        "-ExposureTime".into(),
        "-FNumber".into(),
        "-ISO".into(),
        "-Gain".into(),
        "-WhiteBalance".into(),
        "-Focus".into(),
        "-ImageStabilization".into(),
        "-ExposureProgram".into(),
        "-ApertureSetting".into(),
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

/// Parse exiftool JSON output.
///
/// Expected format (includes extended tags):
/// ```json
/// [{"SourceFile":"/path","Make":"Sony","Model":"NEX-FS100EK","CreateDate":"2024:01:01 12:00:00","LensModel":"E PZ 18-105mm F4 G OSS","DeviceSerialNo":4294967295,"CreationDateValue":"2026:09:25 20:45:22+01:00","CaptureGammaEquation":"ex-cine4","LtcChangeTableLtcChangeValue":"08095915","TimeCode":"15:32:14:21","ExposureTime":"1/50","FNumber":5.6,"Gain":"9 dB","WhiteBalance":"1-Push","Focus":"Auto (0.045)","ImageStabilization":"On (0x3f)","ExposureProgram":"Manual"}]
/// ```
/// Make prefers `Make`; falls back to `DeviceManufacturer` (XAVC XML).
/// Model prefers `Model`; falls back to `DeviceModelName` (XAVC XML).
/// Creation date prefers `CreateDate`; falls back to `DateTimeOriginal`.
/// Full creation timestamp prefers `CreationDateValue` (has TZ).
fn parse_exiftool_json(stdout: &str) -> Option<CameraInfo> {
    let arr: Vec<serde_json::Value> = serde_json::from_str(stdout).ok()?;
    let entry = arr.first()?;

    let model = entry
        .get("Model")
        .or_else(|| entry.get("DeviceModelName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())?;

    let make = entry
        .get("Make")
        .or_else(|| entry.get("DeviceManufacturer"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let raw_date = entry
        .get("CreateDate")
        .or_else(|| entry.get("DateTimeOriginal"))
        .and_then(|v| v.as_str());
    let creation_date = raw_date.and_then(normalize_date);

    let lens = entry
        .get("LensModel")
        .or_else(|| entry.get("LensModelName"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let serial = entry
        .get("SerialNumber")
        .or_else(|| entry.get("DeviceSerialNo"))
        .and_then(|v| match v {
            serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
            serde_json::Value::Number(n) => {
                let v = n.as_u64().unwrap_or(0);
                if v == 0xFFFFFFFF || v == 0 {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            _ => None,
        })
        .filter(|s| !s.is_empty())
        .filter(|s| s != "4294967295" && s != "0xFFFFFFFF" && s != "FFFFFFFF");

    let raw_ct = entry.get("CreationDateValue").and_then(|v| v.as_str());
    let creation_time = raw_ct
        .or(raw_date)
        .and_then(normalize_exiftool_timestamp)
        .or_else(|| raw_date.and_then(normalize_exiftool_timestamp));

    let gamma = entry
        .get("CaptureGammaEquation")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    let native_timecode = entry
        .get("LtcChangeTableLtcChangeValue")
        .and_then(|v| v.as_str())
        .filter(|s| s.len() >= 8)
        .map(|s| format!("{}:{}:{}:{}", &s[0..2], &s[2..4], &s[4..6], &s[6..8]))
        .or_else(|| {
            entry
                .get("TimeCode")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
        });

    let exposure_summary = build_exposure_summary(entry);

    Some(CameraInfo {
        make,
        model: Some(model),
        source: CameraMetaSource::ExifTool,
        creation_date,
        lens,
        serial,
        creation_time,
        gamma,
        native_timecode,
        exposure_summary,
    })
}

/// Build a human-readable exposure summary from exiftool's H264 shooting
/// parameters.
fn build_exposure_summary(entry: &serde_json::Value) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    if let Some(et) = entry
        .get("ExposureTime")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("{}s", et));
    }
    if let Some(fn_val) = entry.get("FNumber").and_then(|v| v.as_f64()) {
        parts.push(format!("F{}", fn_val));
    }
    if let Some(gain) = entry
        .get("Gain")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("{} gain", gain));
    }
    if let Some(iso) = entry.get("ISO").and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }) {
        parts.push(format!("ISO {}", iso));
    }
    if let Some(wb) = entry
        .get("WhiteBalance")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("WB {}", wb));
    }
    if let Some(focus) = entry
        .get("Focus")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Focus {}", focus));
    }
    if let Some(is_val) = entry
        .get("ImageStabilization")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("IS {}", is_val));
    }
    if let Some(prog) = entry
        .get("ExposureProgram")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("{} exp", prog));
    }
    if let Some(ap) = entry
        .get("ApertureSetting")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        parts.push(format!("Aperture {}", ap));
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(", "))
    }
}

/// Parse ffprobe `-show_entries format_tags` JSON output.
///
/// Expected structure (in `format.tags`):
/// ```json
/// {"format":{"tags":{"make":"Sony","model":"NEX-FS100EK","creation_time":"2024-01-01T12:00:00.000000Z"}}}
/// ```
/// Also handles Apple-style keys:
/// `com.apple.quicktime.make` / `com.apple.quicktime.model`.
/// The `encoder` tag is tried as a secondary make source.
/// `creation_time` is read from format tags for date.
///
/// Note: extended fields (lens, serial, gamma, native TC, exposure) are not
/// available via ffprobe format tags — they are set to `None`.
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

    let raw_date = tags.get("creation_time").and_then(|v| v.as_str());
    let creation_date = raw_date.and_then(normalize_date);

    Some(CameraInfo {
        make,
        model: Some(model),
        source: CameraMetaSource::FfprobeTags,
        creation_date,
        lens: None,
        serial: None,
        creation_time: None,
        gamma: None,
        native_timecode: None,
        exposure_summary: None,
    })
}

/// Normalize a date string to `YYYY-MM-DD` format.
///
/// Handles exiftool format (`2024:01:01 12:00:00`) and ISO 8601
/// (`2024-01-01T12:00:00.000000Z`). Returns `None` for unparseable input.
fn normalize_date(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() < 10 {
        return None;
    }
    let sep = raw.as_bytes()[4];
    if sep == b':' {
        Some(format!("{}-{}-{}", &raw[0..4], &raw[5..7], &raw[8..10]))
    } else if sep == b'-' {
        Some(raw[0..10].to_string())
    } else {
        None
    }
}

/// Normalize an exiftool-style timestamp to ISO-8601 with timezone.
///
/// Accepts:
/// - `"2026:09:25 20:45:22+01:00"` — exiftool w/ TZ suffix
/// - `"2024:01:01 12:00:00"` — plain exiftool (no TZ → local/unknown)
/// - ISO-8601 variants already in the right format
///
/// Returns: `"2026-09-25T20:45:22+01:00"` or `"2024-01-01T12:00:00"`.
fn normalize_exiftool_timestamp(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.len() < 19 {
        return None;
    }
    let sep = raw.as_bytes()[4];
    if sep == b':' {
        // Exiftool format: YYYY:MM:DD HH:MM:SS[±HH:MM]
        let date_part = raw[0..10].replace(':', "-");
        let time_part = &raw[11..19];
        let tz_part = if raw.len() > 19 { &raw[19..] } else { "" };
        Some(format!("{}T{}{}", date_part, time_part, tz_part))
    } else if sep == b'-' {
        // Already ISO-ish
        if raw.len() >= 19 && raw.as_bytes()[10] == b'T' {
            Some(raw.to_string())
        } else if raw.len() >= 19 && raw.as_bytes()[10] == b' ' {
            let mut s = raw.to_string();
            s.replace_range(10..11, "T");
            Some(s)
        } else {
            None
        }
    } else {
        None
    }
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
        let json = r#"[{"SourceFile":"/test.MTS","Make":"Sony","Model":"NEX-FS100EK","CreateDate":"2024:01:01 12:00:00","ExposureTime":"1/50","FNumber":5.6,"WhiteBalance":"1-Push","ExposureProgram":"Manual"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("NEX-FS100EK".to_string()));
        assert_eq!(info.source, CameraMetaSource::ExifTool);
        assert_eq!(info.creation_date, Some("2024-01-01".to_string()));
        assert_eq!(info.lens, None);
        assert_eq!(info.serial, None);
        assert_eq!(info.creation_time, Some("2024-01-01T12:00:00".to_string()));
        assert_eq!(info.native_timecode, None);
        assert!(info.exposure_summary.unwrap().contains("1/50s"));
    }

    #[test]
    fn test_exiftool_xavc_device_tags() {
        let json = r#"[{"SourceFile":"/test.MP4","DeviceManufacturer":"Sony","DeviceModelName":"ILCE-6700","LensModelName":"E PZ 18-105mm F4 G OSS","DeviceSerialNo":4294967295,"CreationDateValue":"2026:09:25 20:45:22+01:00","CaptureGammaEquation":"ex-cine4","LtcChangeTableLtcChangeValue":"08095915"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("ILCE-6700".to_string()));
        assert_eq!(info.lens, Some("E PZ 18-105mm F4 G OSS".to_string()));
        assert_eq!(info.serial, None); // 0xFFFFFFFF sentinel
        assert_eq!(
            info.creation_time,
            Some("2026-09-25T20:45:22+01:00".to_string())
        );
        assert_eq!(info.gamma, Some("ex-cine4".to_string()));
        assert_eq!(info.native_timecode, Some("08:09:59:15".to_string()));
        assert_eq!(info.exposure_summary, None);
    }

    #[test]
    fn test_exiftool_xavc_device_make_model_fallback() {
        let json = r#"[{"SourceFile":"/test.MP4","DeviceModelName":"ILCE-6100","DeviceSerialNo":"0000000012345678","LtcChangeTableLtcChangeValue":"01223344"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.make, None);
        assert_eq!(info.model, Some("ILCE-6100".to_string()));
        assert_eq!(info.serial, Some("0000000012345678".to_string()));
        assert_eq!(info.native_timecode, Some("01:22:33:44".to_string()));
    }

    #[test]
    fn test_exiftool_no_make() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"","Model":"EOS R5"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.make, None);
        assert_eq!(info.model, Some("EOS R5".to_string()));
        assert_eq!(info.creation_date, None);
    }

    #[test]
    fn test_exiftool_empty_model_returns_none() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"Sony","Model":""}]"#;
        assert!(parse_exiftool_json(json).is_none());
    }

    #[test]
    fn test_exiftool_empty_device_model_name_returns_none() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"Sony","DeviceModelName":""}]"#;
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

    #[test]
    fn test_exiftool_datetime_original_fallback() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"Canon","Model":"EOS R5","DateTimeOriginal":"2023:06:15 08:30:00"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.creation_date, Some("2023-06-15".to_string()));
    }

    #[test]
    fn test_exiftool_create_date_wins_over_datetime_original() {
        let json = r#"[{"SourceFile":"/test.mp4","Make":"Canon","Model":"EOS R5","CreateDate":"2024:01:01 12:00:00","DateTimeOriginal":"2023:06:15 08:30:00"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.creation_date, Some("2024-01-01".to_string()));
    }

    #[test]
    fn test_exiftool_avchd_timecode_and_exposure() {
        let json = r#"[{"SourceFile":"/test.MTS","Make":"Sony","Model":"NEX-FS100EK","TimeCode":"15:32:14:21","ExposureTime":"1/50","FNumber":5.6,"Gain":"9 dB","WhiteBalance":"1-Push","Focus":"Auto (0.045)","ImageStabilization":"On (0x3f)","ExposureProgram":"Manual","ApertureSetting":"Auto"}]"#;
        let info = parse_exiftool_json(json).unwrap();
        assert_eq!(info.native_timecode, Some("15:32:14:21".to_string()));
        let summary = info.exposure_summary.unwrap();
        assert!(summary.contains("1/50s"));
        assert!(summary.contains("F5.6"));
        assert!(summary.contains("9 dB gain"));
        assert!(summary.contains("WB 1-Push"));
        assert!(summary.contains("Focus Auto"));
        assert!(summary.contains("IS On"));
        assert!(summary.contains("Manual exp"));
        assert!(summary.contains("Aperture Auto"));
    }

    // ── parse_ffprobe_tags ────────────────────────────────────────────

    fn check_ffprobe_extended_none(info: &CameraInfo) {
        assert_eq!(info.lens, None);
        assert_eq!(info.serial, None);
        assert_eq!(info.creation_time, None);
        assert_eq!(info.gamma, None);
        assert_eq!(info.native_timecode, None);
        assert_eq!(info.exposure_summary, None);
    }

    #[test]
    fn test_ffprobe_tags_standard() {
        let json = r#"{"format":{"tags":{"make":"Sony","model":"NEX-FS100EK","creation_time":"2024-01-01T12:00:00.000000Z"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("NEX-FS100EK".to_string()));
        assert_eq!(info.source, CameraMetaSource::FfprobeTags);
        assert_eq!(info.creation_date, Some("2024-01-01".to_string()));
        check_ffprobe_extended_none(&info);
    }

    #[test]
    fn test_ffprobe_tags_apple_keys() {
        let json = r#"{"format":{"tags":{"com.apple.quicktime.make":"GoPro","com.apple.quicktime.model":"HERO9 Black"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, Some("GoPro".to_string()));
        assert_eq!(info.model, Some("HERO9 Black".to_string()));
        assert_eq!(info.creation_date, None);
        check_ffprobe_extended_none(&info);
    }

    #[test]
    fn test_ffprobe_tags_encoder_as_make() {
        let json = r#"{"format":{"tags":{"encoder":"GoPro HERO9 Black","model":"HERO9 Black"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, Some("GoPro HERO9 Black".to_string()));
        assert_eq!(info.model, Some("HERO9 Black".to_string()));
        check_ffprobe_extended_none(&info);
    }

    #[test]
    fn test_ffprobe_tags_no_make() {
        let json = r#"{"format":{"tags":{"model":"GH6"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.make, None);
        assert_eq!(info.model, Some("GH6".to_string()));
        check_ffprobe_extended_none(&info);
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

    #[test]
    fn test_ffprobe_tags_with_creation_time() {
        let json =
            r#"{"format":{"tags":{"model":"GH6","creation_time":"2023-12-25T10:00:00.000000Z"}}}"#;
        let info = parse_ffprobe_tags(json).unwrap();
        assert_eq!(info.creation_date, Some("2023-12-25".to_string()));
        check_ffprobe_extended_none(&info);
    }

    // ── exiftool output includes extended tags ────────────────────────

    #[test]
    fn test_probe_exiftool_with_xavc_tags() {
        let path = Path::new("/media/card/C0369.MP4");
        let json = r#"[{"SourceFile":"/media/card/C0369.MP4","DeviceManufacturer":"Sony","DeviceModelName":"ILCE-6700","LensModelName":"E PZ 18-105mm F4 G OSS","CreationDateValue":"2026:09:25 20:45:22+01:00","CaptureGammaEquation":"ex-cine4","LtcChangeTableLtcChangeValue":"08095915","DeviceSerialNo":4294967295}]"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("ILCE-6700".to_string()));
        assert_eq!(info.lens, Some("E PZ 18-105mm F4 G OSS".to_string()));
        assert_eq!(info.serial, None);
        assert_eq!(
            info.creation_time,
            Some("2026-09-25T20:45:22+01:00".to_string())
        );
        assert_eq!(info.gamma, Some("ex-cine4".to_string()));
        assert_eq!(info.native_timecode, Some("08:09:59:15".to_string()));
    }

    #[test]
    fn test_probe_exiftool_with_avchd_tags() {
        let path = Path::new("/media/card/00027.MTS");
        let json = r#"[{"SourceFile":"/media/card/00027.MTS","Make":"Sony","Model":"NEX-FS100EK","TimeCode":"15:32:14:21","DateTimeOriginal":"2026:09:25 20:21:16+01:00","ExposureTime":"1/50","FNumber":5.6,"Gain":"9 dB","WhiteBalance":"1-Push","Focus":"Auto (0.045)","ImageStabilization":"On (0x3f)","ExposureProgram":"Manual","ApertureSetting":"Auto"}]"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("NEX-FS100EK".to_string()));
        assert_eq!(info.native_timecode, Some("15:32:14:21".to_string()));
        assert!(info.exposure_summary.unwrap().contains("1/50s"));
    }

    #[test]
    fn test_probe_exiftool_tascam_empty() {
        let path = Path::new("/media/card/TASCAM_0094S1.wav");
        let json = r#"[{"SourceFile":"/media/card/TASCAM_0094S1.wav"}]"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner);
        assert!(info.is_none(), "WAV with no tags should return None");
    }

    // ── integration: probe_camera_info_with ───────────────────────────

    #[test]
    fn test_probe_with_exiftool_success() {
        let path = Path::new("/media/card/00001.MTS");
        let json = r#"[{"SourceFile":"/media/card/00001.MTS","Make":"Sony","Model":"NEX-FS100EK","CreateDate":"2024:01:01 12:00:00"}]"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner).unwrap();
        assert_eq!(info.make, Some("Sony".to_string()));
        assert_eq!(info.model, Some("NEX-FS100EK".to_string()));
        assert_eq!(info.source, CameraMetaSource::ExifTool);
        assert_eq!(info.creation_date, Some("2024-01-01".to_string()));
    }

    #[test]
    fn test_probe_exiftool_fails_ffprobe_success() {
        let path = Path::new("/media/card/00001.MP4");
        let json = r#"{"format":{"tags":{"make":"GoPro","model":"HERO9 Black","creation_time":"2023-06-15T08:30:00.000000Z"}}}"#;
        let mut runner = |prog: &str, _args: &[String]| match prog {
            "exiftool" => Ok(failure()),
            "ffprobe" => Ok(ok_with(json.as_bytes())),
            _ => Ok(failure()),
        };
        let info = probe_camera_info_with(path, &mut runner).unwrap();
        assert_eq!(info.make, Some("GoPro".to_string()));
        assert_eq!(info.model, Some("HERO9 Black".to_string()));
        assert_eq!(info.source, CameraMetaSource::FfprobeTags);
        assert_eq!(info.creation_date, Some("2023-06-15".to_string()));
        check_ffprobe_extended_none(&info);
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
        assert_eq!(info.creation_date, None);
        check_ffprobe_extended_none(&info);
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

    // ── normalize_date ────────────────────────────────────────────────

    #[test]
    fn test_normalize_date_exiftool_format() {
        assert_eq!(
            normalize_date("2024:01:01 12:00:00"),
            Some("2024-01-01".to_string())
        );
    }

    #[test]
    fn test_normalize_date_iso_format() {
        assert_eq!(
            normalize_date("2024-01-01T12:00:00.000000Z"),
            Some("2024-01-01".to_string())
        );
    }

    #[test]
    fn test_normalize_date_iso_no_trailing() {
        assert_eq!(normalize_date("2024-01-01"), Some("2024-01-01".to_string()));
    }

    #[test]
    fn test_normalize_date_bad_separator() {
        assert_eq!(normalize_date("2024/01/01 12:00:00"), None);
    }

    #[test]
    fn test_normalize_date_too_short() {
        assert_eq!(normalize_date("2024"), None);
    }

    #[test]
    fn test_normalize_date_empty() {
        assert_eq!(normalize_date(""), None);
    }

    #[test]
    fn test_normalize_date_whitespace() {
        assert_eq!(normalize_date("  "), None);
    }

    // ── normalize_exiftool_timestamp ─────────────────────────────────

    #[test]
    fn test_normalize_timestamp_exiftool_w_tz() {
        assert_eq!(
            normalize_exiftool_timestamp("2026:09:25 20:45:22+01:00"),
            Some("2026-09-25T20:45:22+01:00".to_string())
        );
    }

    #[test]
    fn test_normalize_timestamp_exiftool_no_tz() {
        assert_eq!(
            normalize_exiftool_timestamp("2024:01:01 12:00:00"),
            Some("2024-01-01T12:00:00".to_string())
        );
    }

    #[test]
    fn test_normalize_timestamp_exiftool_neg_tz() {
        assert_eq!(
            normalize_exiftool_timestamp("2026:09:25 20:45:22-05:00"),
            Some("2026-09-25T20:45:22-05:00".to_string())
        );
    }

    #[test]
    fn test_normalize_timestamp_iso_already() {
        assert_eq!(
            normalize_exiftool_timestamp("2024-01-01T12:00:00.000000Z"),
            Some("2024-01-01T12:00:00.000000Z".to_string())
        );
    }

    #[test]
    fn test_normalize_timestamp_iso_no_z() {
        assert_eq!(
            normalize_exiftool_timestamp("2024-01-01T12:00:00"),
            Some("2024-01-01T12:00:00".to_string())
        );
    }

    #[test]
    fn test_normalize_timestamp_too_short() {
        assert_eq!(normalize_exiftool_timestamp("2024:01:01"), None);
        assert_eq!(normalize_exiftool_timestamp(""), None);
    }

    #[test]
    fn test_normalize_timestamp_bad_separator() {
        assert_eq!(normalize_exiftool_timestamp("2024/01/01 12:00:00"), None);
    }
}
