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
use std::time::{Duration, Instant};
use std::sync::mpsc;

use log::{debug, info, warn};

use crate::camera_meta;

/// Maximum video files to probe during device-name resolution.
/// One clip is usually sufficient; the cap provides corrupt-file resilience.
pub(crate) const DEVICE_NAME_PROBE_SAMPLE: usize = 3;

/// Per-file I/O budget for [`read_head_tail`] (abandon-thread timeout).
pub(crate) const READ_BUDGET: Duration = Duration::from_secs(10);

/// Overall budget for device-name resolution before falling back to
/// pattern/label/"unknown" (skips all I/O-heavy probe steps).
pub(crate) const RESOLVE_BUDGET: Duration = Duration::from_secs(60);

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

/// Resolve device name from a set of media files, using the default budget.
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
    resolve_device_name_with_budget(files, volume_label, cameras, RESOLVE_BUDGET)
}

/// Resolve device name with an explicit overall time budget.
///
/// Falls through probe steps that take longer than the deadline and
/// degrades gracefully to pattern/label/"unknown".
pub fn resolve_device_name_with_budget(
    files: &[PathBuf],
    volume_label: &str,
    cameras: Option<&[Option<camera_meta::CameraInfo>]>,
    budget: Duration,
) -> (String, DeviceNameSource, Option<String>) {
    let deadline = Instant::now() + budget;
    let video_count = files.iter().filter(|f| is_video_file(f)).count();

    debug!(
        "resolve_device_name: {} file(s) ({} video), budget={:.2?}",
        files.len(),
        video_count,
        budget,
    );

    // 1) XAVC metadata sniff on a sample of video files.
    if let Some((name, source)) = try_metadata_name(files, DEVICE_NAME_PROBE_SAMPLE) {
        return (name, source, None);
    }
    if Instant::now() >= deadline {
        info!("resolve_device_name: budget exhausted after XAVC sniff — falling through");
    } else {
        // 2) Camera metadata via exiftool/ffprobe on a sample of files.
        if let Some((name, source)) = try_camera_meta_name_with(files, cameras, DEVICE_NAME_PROBE_SAMPLE) {
            return (name, source, None);
        }
    }

    // 3) Filename pattern matching.
    let pattern_names = match_files_to_pattern_names(files);
    if !pattern_names.is_empty() {
        let best = &pattern_names[0];
        let device = normalize_pattern_name(best);
        info!("resolve_device_name: pattern match → '{}'", best);
        return (device, DeviceNameSource::Pattern, Some(best.clone()));
    }

    // 4) Volume label.
    let label = volume_label.trim();
    if !label.is_empty()
        && !label.eq_ignore_ascii_case("usb")
        && !label.eq_ignore_ascii_case("usb drive")
    {
        info!("resolve_device_name: volume label → '{}'", label);
        return (label.to_string(), DeviceNameSource::VolumeLabel, None);
    }

    // 5) Fallback.
    info!("resolve_device_name: fallback to 'unknown'");
    ("unknown".to_string(), DeviceNameSource::Unknown, None)
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Try to extract a model name from a video file's XAVC metadata.
/// Only examines the first `max_files` video files that exist in `files`.
fn try_metadata_name(
    files: &[PathBuf],
    max_files: usize,
) -> Option<(String, DeviceNameSource)> {
    let mut sampled = 0u32;
    for f in files {
        if !is_video_file(f) {
            continue;
        }
        if sampled >= max_files as u32 {
            debug!("try_metadata_name: reached sample limit of {} files, stopping", max_files);
            break;
        }
        sampled += 1;
        let start = Instant::now();
        debug!("try_metadata_name: sniffing {:?} (head+tail, {:?} budget)", f, READ_BUDGET);
        if let Some(model) = extract_model_name(f) {
            let elapsed = start.elapsed();
            let normalized = normalize_model_name(&model);
            info!("try_metadata_name: {:?} → model '{}' → '{}' in {:.2?}", f, model, normalized, elapsed);
            return Some((normalized, DeviceNameSource::Metadata));
        }
        debug!("try_metadata_name: {:?} → no model found ({:.2?})", f, start.elapsed());
    }
    None
}

/// Try to obtain a device name from camera metadata — either pre‑computed
/// `CameraInfo` or live exiftool/ffprobe subprocesses.
/// Only examines the first `max_files` video files in `files`.
fn try_camera_meta_name_with(
    files: &[PathBuf],
    cameras: Option<&[Option<camera_meta::CameraInfo>]>,
    max_files: usize,
) -> Option<(String, DeviceNameSource)> {
    if let Some(cam_list) = cameras {
        // Use pre‑computed values (converter path) — cheap, no budget needed.
        for info in cam_list.iter().flatten() {
            if let Some(ref model) = info.model {
                return Some((model.clone(), DeviceNameSource::Metadata));
            }
        }
        None
    } else {
        // Live probe (offload path).
        let mut sampled = 0u32;
        for f in files {
            if !is_video_file(f) {
                continue;
            }
            if sampled >= max_files as u32 {
                debug!("try_camera_meta_name_with: reached sample limit of {} files", max_files);
                break;
            }
            sampled += 1;
            let start = Instant::now();
            debug!("try_camera_meta_name_with: probing {:?}", f);
            if let Some(info) = camera_meta::probe_camera_info(f) {
                let elapsed = start.elapsed();
                if let Some(model) = info.model {
                    info!("try_camera_meta_name_with: {:?} → model '{}' in {:.2?}", f, model, elapsed);
                    return Some((model, DeviceNameSource::Metadata));
                }
            }
            debug!("try_camera_meta_name_with: {:?} → no metadata ({:.2?})", f, start.elapsed());
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

/// Read first `head_bytes` and last `tail_bytes` of a file, with an overall
/// per‑call budget.  The actual I/O runs on a helper thread so a blocking
/// read (e.g. stalled USB card) never hangs the caller — when the budget
/// expires the thread is abandoned and `None` is returned.
fn read_head_tail(path: &Path, head_bytes: u64, tail_bytes: u64) -> Option<Vec<u8>> {
    let path_clone = path.to_path_buf();
    let display_path = path.to_string_lossy().to_string();
    let (tx, rx) = mpsc::channel();
    let _handle = std::thread::Builder::new()
        .name("head-tail-reader".into())
        .spawn(move || {
            let _ = tx.send(read_head_tail_inner(&path_clone, head_bytes, tail_bytes));
        })
        .ok()?;
    match rx.recv_timeout(READ_BUDGET) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => {
            warn!(
                "read_head_tail({}) timed out after {:?} — reader thread abandoned",
                display_path, READ_BUDGET,
            );
            None
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => None,
    }
}

/// Actual synchronous head+tail read (runs on a helper thread).
fn read_head_tail_inner(path: &Path, head_bytes: u64, tail_bytes: u64) -> Option<Vec<u8>> {
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
    crate::media_ext::is_video(path)
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

    // ── sample cap & budget ───────────────────────────────────────────

    #[test]
    fn test_try_metadata_sample_cap_skips_fourth_file() {
        let dir = TempDir::new().unwrap();
        // First 3 video files have no modelName; only the 4th does.
        // DEVICE_NAME_PROBE_SAMPLE = 3 → step 1 must NOT find it.
        // Use `clipNNNN.mp4` suffix — no camera pattern matches `clip*`.
        for i in 0..3 {
            let f = dir.path().join(format!("clip{:04}.mp4", i));
            let content = vec![0u8; 5000];
            fs::write(&f, &content).unwrap();
        }
        let f4 = dir.path().join("clip0003.mp4");
        let mut content4 = vec![0u8; 5000];
        let xml = br#"<Device manufacturer="Sony" modelName="ILCE-6700"/>"#;
        content4[4500..4500 + xml.len()].copy_from_slice(xml);
        fs::write(&f4, &content4).unwrap();

        let mut files: Vec<_> = dir.path().read_dir().unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        files.sort();

        // Step 1 only sees first 3 files (no model) → falls to unknown.
        let (name, source, _) = resolve_device_name(&files, "", None);
        assert_eq!(name, "unknown");
        assert_eq!(source, DeviceNameSource::Unknown);
    }

    #[test]
    fn test_try_metadata_finds_model_in_first_file() {
        let dir = TempDir::new().unwrap();
        // First file (sorted) has the model, rest without.
        let mut content0 = vec![0u8; 5000];
        let xml = br#"<Device manufacturer="Sony" modelName="ILCE-6700"/>"#;
        content0[4500..4500 + xml.len()].copy_from_slice(xml);
        fs::write(dir.path().join("clip0000.mp4"), &content0).unwrap();
        for i in 1..6 {
            fs::write(dir.path().join(format!("clip{:04}.mp4", i)), vec![0u8; 5000]).unwrap();
        }

        let mut files: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        files.sort();

        let (name, source, _) = resolve_device_name(&files, "", None);
        assert_eq!(name, "A6700");
        assert_eq!(source, DeviceNameSource::Metadata);
    }

    #[test]
    fn test_resolve_with_expired_budget_falls_to_volume_label() {
        let dir = TempDir::new().unwrap();
        // .wav files are not video → no probe steps will run.
        fs::write(dir.path().join("track01.wav"), b"data").unwrap();
        let mut files: Vec<_> = dir.path().read_dir().unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        files.sort();

        let (name, source, _) = resolve_device_name_with_budget(
            &files, "EOS_DIGITAL", None, Duration::from_nanos(1),
        );
        assert_eq!(name, "EOS_DIGITAL");
        assert_eq!(source, DeviceNameSource::VolumeLabel);
    }

    #[test]
    fn test_resolve_with_expired_budget_and_no_label_returns_unknown() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("track01.wav"), b"data").unwrap();
        let mut files: Vec<_> = dir.path().read_dir().unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        files.sort();

        let (name, source, _) = resolve_device_name_with_budget(
            &files, "", None, Duration::from_nanos(1),
        );
        assert_eq!(name, "unknown");
        assert_eq!(source, DeviceNameSource::Unknown);
    }
}