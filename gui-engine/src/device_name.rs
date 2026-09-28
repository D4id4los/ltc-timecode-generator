// ── Device-name resolution ──────────────────────────────────────────────
//
// Shared resolution chain used by offload (card→folder naming) and
// converter ({device} naming template).  Priority:
//   1. XAVC binary sniff (modelName XML / ILCE‑ / ILME‑ / DSC‑ / HDR‑)
//   2. Camera metadata (exiftool / ffprobe tags), optionally pre‑computed
//   3. Filename pattern matching (Sony, Canon, Panasonic, GoPro, TASCAM)
//   4. Volume label (offload only; converter passes "")
//   5. "unknown"

use std::fs;
use std::io::{Read, Seek};
use std::path::{Path, PathBuf};

use crate::camera_meta;

/// Video file extensions recognised as media (same list as `ffprobe::VIDEO_EXTENSIONS`).
pub(crate) const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "mts", "m2ts", "mxf", "avi", "webm", "m4v"];

// ── Public types ────────────────────────────────────────────────────────

/// How a device name was determined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceNameSource {
    /// Parsed from embedded camera metadata — XAVC XML sniff, exiftool
    /// (AVCHD SEI, MP4/MOV tags), or ffprobe format tags.
    Metadata,
    /// Derived from the filename pattern (e.g. `Sony FS100` → `FS100`).
    Pattern,
    /// Volume label of the mounted card.
    VolumeLabel,
    /// Set manually by the user.
    Manual,
    /// Fallback generic name.
    Unknown,
}

// ── Public API ──────────────────────────────────────────────────────────

/// Resolve device name from a set of media files.
///
/// * `files` — media files (audio/video) from a single card or recording.
/// * `volume_label` — card volume label (pass `""` when not applicable).
/// * `cameras` — pre‑computed per‑file `CameraInfo` (from converter probe).
///   Pass `None` when no pre‑computed data is available (offload) — the
///   function will call `probe_camera_info` for video files.
///
/// Returns `(device_name, source, pattern_name)` where `pattern_name` is
/// `Some` only when the name came from a file‑pattern match.
pub fn resolve_device_name(
    files: &[PathBuf],
    volume_label: &str,
    cameras: Option<&[Option<camera_meta::CameraInfo>]>,
) -> (String, DeviceNameSource, Option<String>) {
    // 1) XAVC metadata sniff on a sample video file.
    if let Some((name, source)) = try_metadata_name(files) {
        return (name, source, None);
    }

    // 2) Camera metadata via exiftool (AVCHD SEI) or ffprobe tags,
    //    or pre‑computed CameraInfo from the converter clip probe.
    if let Some((name, source)) = try_camera_meta_name_with(files, cameras) {
        return (name, source, None);
    }

    // 3) Filename pattern matching.
    let pattern_names = match_files_to_pattern_names(files);
    if !pattern_names.is_empty() {
        let best = &pattern_names[0];
        let device = normalize_pattern_name(best);
        return (device, DeviceNameSource::Pattern, Some(best.clone()));
    }

    // 4) Volume label.
    let label = volume_label.trim();
    if !label.is_empty()
        && !label.eq_ignore_ascii_case("usb")
        && !label.eq_ignore_ascii_case("usb drive")
    {
        return (label.to_string(), DeviceNameSource::VolumeLabel, None);
    }

    // 5) Fallback.
    ("unknown".to_string(), DeviceNameSource::Unknown, None)
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Try to extract a model name from a video file's XAVC metadata.
fn try_metadata_name(files: &[PathBuf]) -> Option<(String, DeviceNameSource)> {
    for f in files {
        if !is_video_file(f) {
            continue;
        }
        if let Some(model) = extract_model_name(f) {
            let normalized = normalize_model_name(&model);
            return Some((normalized, DeviceNameSource::Metadata));
        }
    }
    None
}

/// Try to obtain a device name from camera metadata — either pre‑computed
/// `CameraInfo` or live exiftool/ffprobe subprocesses.
fn try_camera_meta_name_with(
    files: &[PathBuf],
    cameras: Option<&[Option<camera_meta::CameraInfo>]>,
) -> Option<(String, DeviceNameSource)> {
    if let Some(cam_list) = cameras {
        // Use pre‑computed values (converter path).
        for info in cam_list.iter().flatten() {
            if let Some(ref model) = info.model {
                return Some((model.clone(), DeviceNameSource::Metadata));
            }
        }
        None
    } else {
        // Live probe (offload path).
        for f in files {
            if !is_video_file(f) {
                continue;
            }
            if let Some(info) = camera_meta::probe_camera_info(f) {
                if let Some(model) = info.model {
                    return Some((model, DeviceNameSource::Metadata));
                }
            }
        }
        None
    }
}

/// Extract `modelName` or `ILCE‑####` string from head+tail of a video file.
fn extract_model_name(path: &Path) -> Option<String> {
    if path.metadata().map(|m| m.len()).unwrap_or(0) < 4096 {
        return None;
    }

    // Read first 4 MB and last 4 MB.
    let head = read_head_tail(path, 4 << 20, 4 << 20)?;
    let haystack = String::from_utf8_lossy(&head);

    // 1. Try XML-style: modelName="ILCE-6700"
    if let Some(start) = haystack.find("modelName=\"") {
        let rest = &haystack[start + 11..];
        if let Some(end) = rest.find('"') {
            let model = &rest[..end];
            if !model.is_empty() {
                return Some(model.to_string());
            }
        }
    }

    // 2. Try bare ILCE‑#### / ILME‑#### / DSC‑#### / HDR‑#### anywhere in the file.
    for model_prefix in &["ILCE-", "ILME-", "DSC-", "HDR-"] {
        if let Some(start) = haystack.find(model_prefix) {
            let rest = &haystack[start..];
            let model: String = rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '-')
                .collect();
            if model.len() > model_prefix.len() {
                return Some(model);
            }
        }
    }

    // 3. Try quoted segments (split by ") for model-style strings.
    for cap in haystack.split('"') {
        if cap.starts_with("ILCE-")
            || cap.starts_with("ILME-")
            || cap.starts_with("DSC-")
            || cap.starts_with("HDR-")
        {
            return Some(cap.to_string());
        }
    }

    None
}

/// Read first `head_bytes` and last `tail_bytes` of a file.
fn read_head_tail(path: &Path, head_bytes: u64, tail_bytes: u64) -> Option<Vec<u8>> {
    let len = path.metadata().ok()?.len();
    let mut file = fs::File::open(path).ok()?;

    let head_len = head_bytes.min(len) as usize;
    let mut buf = Vec::with_capacity((head_bytes + tail_bytes) as usize);
    let mut head = vec![0u8; head_len];
    file.read_exact(&mut head).ok()?;
    buf.extend_from_slice(&head);

    if len > tail_bytes {
        let tail_start = len.saturating_sub(tail_bytes);
        let tail_len = (len - tail_start) as usize;
        file.seek(std::io::SeekFrom::Start(tail_start)).ok()?;
        let mut tail = vec![0u8; tail_len];
        file.read_exact(&mut tail).ok()?;
        buf.extend_from_slice(&tail);
    }

    Some(buf)
}

/// Match media files against known camera/recorder patterns and return pattern names.
fn match_files_to_pattern_names(files: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut used = std::collections::HashSet::new();

    // Try each pattern in order: TASCAM first, then cameras.
    let all_patterns: Vec<&crate::file_pattern::FileNamingPattern> =
        crate::file_pattern::BUILTIN_PATTERNS[..1]
            .iter()
            .chain(crate::file_pattern::CAMERA_PATTERNS.iter())
            .collect();

    for pattern in all_patterns {
        let re = match regex::Regex::new(pattern.regex) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for path in files {
            if !used.contains(path) {
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                let match_str = if pattern.name == "TASCAM" {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                } else {
                    name
                };
                if re.is_match(match_str) {
                    if !names.contains(&pattern.name.to_string()) {
                        names.push(pattern.name.to_string());
                    }
                    used.insert(path.clone());
                }
            }
        }
    }
    names
}

/// Normalise a pattern name to a short device folder name.
fn normalize_pattern_name(pattern_name: &str) -> String {
    match pattern_name {
        "Sony FS100" => "FS100".to_string(),
        "Sony Handycam" => "Handycam".to_string(),
        "Canon" => "Canon".to_string(),
        "Panasonic" => "Panasonic".to_string(),
        "GoPro" => "GoPro".to_string(),
        "TASCAM" => "TASCAM".to_string(),
        "* (any)" => "Camera".to_string(),
        other => other.to_string(),
    }
}

/// Normalise a raw model string to a user-friendly device folder name.
/// `ILCE‑6700` → `A6700`, `ILME‑6400` → `A6400`, etc.
fn normalize_model_name(raw: &str) -> String {
    // Sony Alpha (ILCE) and FX (ILME) mapping.
    if let Some(body) = raw
        .strip_prefix("ILCE-")
        .or_else(|| raw.strip_prefix("ILME-"))
    {
        format!("A{}", body)
    } else {
        raw.to_string()
    }
}

fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── file matcher helpers ──────────────────────────────────────────

    #[test]
    fn test_is_video_file_recognized() {
        assert!(is_video_file(Path::new("clip.mp4")));
        assert!(is_video_file(Path::new("clip.MOV")));
        assert!(is_video_file(Path::new("clip.mts")));
    }

    #[test]
    fn test_is_video_file_not_recognized() {
        assert!(!is_video_file(Path::new("sound.wav")));
        assert!(!is_video_file(Path::new("photo.jpg")));
        assert!(!is_video_file(Path::new("notes.txt")));
    }

    // ── pattern normalisation ─────────────────────────────────────────

    #[test]
    fn test_normalize_pattern_name_returns_short_name() {
        assert_eq!(normalize_pattern_name("Sony FS100"), "FS100");
        assert_eq!(normalize_pattern_name("Sony Handycam"), "Handycam");
        assert_eq!(normalize_pattern_name("Canon"), "Canon");
        assert_eq!(normalize_pattern_name("Panasonic"), "Panasonic");
        assert_eq!(normalize_pattern_name("GoPro"), "GoPro");
        assert_eq!(normalize_pattern_name("TASCAM"), "TASCAM");
        assert_eq!(normalize_pattern_name("* (any)"), "Camera");
    }

    #[test]
    fn test_normalize_pattern_name_passes_through_unknown() {
        assert_eq!(normalize_pattern_name("Custom Recorder"), "Custom Recorder");
    }

    // ── model name normalisation ──────────────────────────────────────

    #[test]
    fn test_normalize_model_name_ilce() {
        assert_eq!(normalize_model_name("ILCE-6700"), "A6700");
    }

    #[test]
    fn test_normalize_model_name_ilme() {
        assert_eq!(normalize_model_name("ILME-6400"), "A6400");
    }

    #[test]
    fn test_normalize_model_name_other() {
        assert_eq!(normalize_model_name("NEX-FS100EK"), "NEX-FS100EK");
    }

    // ── device name guessing via metadata ─────────────────────────────

    #[test]
    fn test_guess_from_ilce_metadata_via_read() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("C0001.MP4");
        let mut content = vec![0u8; 5000];
        let xml = br#"<Device manufacturer="Sony" modelName="ILCE-6700" serialNo="123"/> "#;
        content[4500..4500 + xml.len()].copy_from_slice(xml);
        fs::write(&f, &content).unwrap();

        let model = extract_model_name(&f);
        assert_eq!(model, Some("ILCE-6700".to_string()));
    }

    #[test]
    fn test_guess_from_ilce_bare_string_in_head() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("C0001.MP4");
        let mut content = vec![0u8; 5000];
        let s = br"ILCE-6700 some metadata";
        content[100..100 + s.len()].copy_from_slice(s);
        fs::write(&f, &content).unwrap();

        let model = extract_model_name(&f);
        assert_eq!(model, Some("ILCE-6700".to_string()));
    }

    #[test]
    fn test_guess_device_from_pattern_name() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("00001.MTS"), b"data").unwrap();
        fs::write(dir.path().join("00002.MTS"), b"data").unwrap();
        let files = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect::<Vec<_>>();

        let (name, source, pat) = resolve_device_name(&files, "", None);
        assert_eq!(name, "FS100");
        assert_eq!(source, DeviceNameSource::Pattern);
        assert_eq!(pat, Some("Sony FS100".to_string()));
    }

    #[test]
    fn test_guess_device_volume_label_fallback() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("sound.wav"), b"data").unwrap();
        let files = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect::<Vec<_>>();

        let (name, source, _) = resolve_device_name(&files, "EOS_DIGITAL", None);
        assert_eq!(name, "EOS_DIGITAL");
        assert_eq!(source, DeviceNameSource::VolumeLabel);
    }

    #[test]
    fn test_guess_device_unknown_fallback() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("sound.wav"), b"data").unwrap();
        let files = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect::<Vec<_>>();

        let (name, source, _) = resolve_device_name(&files, "", None);
        assert_eq!(name, "unknown");
        assert_eq!(source, DeviceNameSource::Unknown);
    }

    // ── pre‑computed cameras (converter path) ─────────────────────────

    #[test]
    fn test_resolve_with_precomputed_cameras() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("clip.wav"), b"data").unwrap();
        let files = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect::<Vec<_>>();

        let cameras = vec![Some(camera_meta::CameraInfo {
            make: Some("Sony".to_string()),
            model: Some("ILCE-6700".to_string()),
            source: camera_meta::CameraMetaSource::ExifTool,
            creation_date: None,
            lens: None,
            serial: None,
            creation_time: None,
            gamma: None,
            native_timecode: None,
            exposure_summary: None,
        })];

        // With pre-computed cameras and no XAVC data in the wav file,
        // step 2 should use the CameraInfo model.
        let (name, source, _) = resolve_device_name(&files, "", Some(&cameras));
        assert_eq!(name, "ILCE-6700");
        assert_eq!(source, DeviceNameSource::Metadata);
    }

    #[test]
    fn test_resolve_with_precomputed_cameras_empty() {
        let dir = TempDir::new().unwrap();
        // File matching TASCAM pattern
        fs::write(dir.path().join("nameS1.wav"), b"data").unwrap();
        let files = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect::<Vec<_>>();

        // All None → should fall through to pattern match
        let cameras = vec![None, None];
        let (name, source, pat) = resolve_device_name(&files, "", Some(&cameras));
        assert_eq!(name, "TASCAM");
        assert_eq!(source, DeviceNameSource::Pattern);
        assert_eq!(pat, Some("TASCAM".to_string()));
    }
}