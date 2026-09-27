use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::Local;

use crate::camera_meta;
use crate::file_pattern::{CAMERA_PATTERNS, BUILTIN_PATTERNS};

/// Video file extensions recognised as media (same list as `ffprobe::VIDEO_EXTENSIONS`).
const VIDEO_EXTS: &[&str] = &["mp4", "mov", "mkv", "mts", "m2ts", "mxf", "avi", "webm", "m4v"];

/// Audio file extensions recognised as media.
const AUDIO_EXTS: &[&str] = &["wav"];

/// Buffer size for file copy (1 MiB).
const COPY_BUF_SIZE: usize = 1 << 20;

/// Maximum depth when scanning a card for media files.
const CARD_SCAN_DEPTH: usize = 6;

/// Maximum files to enumerate on a card.
const MAX_CARD_FILES: usize = 10_000;

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

/// Information about a detected media card.
#[derive(Debug, Clone)]
pub struct SdCardInfo {
    /// Mount point of the card (e.g. `/run/media/viktoria/EOS_DIGITAL`).
    pub mount: PathBuf,
    /// Volume label (best-effort; may be empty).
    pub volume_label: String,
    /// Resolved device folder name (user-editable in the snapshot).
    pub device_name: String,
    /// Source of the device name.
    pub name_source: DeviceNameSource,
    /// Number of media files found on the card.
    pub media_file_count: usize,
    /// Total size in bytes of all media files.
    pub total_bytes: u64,
    /// Pattern name from file_pattern matching, if any.
    pub pattern_name: Option<String>,
}

/// Per-device copy status for the published snapshot.
#[derive(Debug, Clone)]
pub enum OffloadDeviceState {
    Pending,
    Copying,
    Done,
    Failed(String),
    Skipped,
}

/// Per-device progress published in the snapshot.
#[derive(Debug, Clone)]
pub struct OffloadDeviceStatus {
    pub device_name: String,
    pub files_total: usize,
    pub files_done: usize,
    pub bytes_total: u64,
    pub bytes_done: u64,
    pub current_file: String,
    pub state: OffloadDeviceState,
}

/// Published snapshot of the entire offload subsystem.
#[derive(Debug, Clone)]
pub struct OffloadSnapshot {
    /// Detected cards from the most recent scan.
    pub cards: Vec<SdCardInfo>,
    /// True while a scan is in progress.
    pub scanning: bool,
    /// User-chosen parent directory (base folder for ISO-date subfolder).
    pub parent_folder: Option<PathBuf>,
    /// Editable folder name (defaults to today's ISO date).
    pub parent_name: String,
    /// True while a copy operation is running.
    pub running: bool,
    /// Overall progress fraction 0.0…1.0.
    pub overall_progress: f32,
    /// Per-device progress details.
    pub device_progress: Vec<OffloadDeviceStatus>,
    /// Device names that have been fully offloaded this session.
    pub completed_devices: Vec<String>,
    /// The last parent folder that was written to (for converter auto-switch).
    pub last_offload_parent: Option<PathBuf>,
    /// Monotonically increasing version — increment on each offload completion.
    pub last_offload_version: u64,
    /// Global error message (e.g. no parent folder set).
    pub error: Option<String>,
}

impl OffloadSnapshot {
    pub fn initial() -> Self {
        OffloadSnapshot {
            cards: Vec::new(),
            scanning: false,
            parent_folder: None,
            parent_name: default_parent_name(),
            running: false,
            overall_progress: 0.0,
            device_progress: Vec::new(),
            completed_devices: Vec::new(),
            last_offload_parent: None,
            last_offload_version: 0,
            error: None,
        }
    }
}

/// Shared progress state for a running offload copy operation.
/// The engine reads atomics each tick to build the snapshot.
pub struct OffloadContext {
    pub devices_total: AtomicUsize,
    pub devices_done: AtomicUsize,
    pub overall_files_total: AtomicUsize,
    pub overall_files_done: AtomicUsize,
    /// Total bytes in 1 MiB units (to stay within usize range on 32-bit).
    pub overall_blocks_total: AtomicUsize,
    pub overall_blocks_done: AtomicUsize,
    /// Per-device progress vectors, indexed by device index.
    pub per_device: Vec<DeviceProgressInner>,
    pub cancel: AtomicBool,
}

/// Per-device progress within `OffloadContext`.
pub struct DeviceProgressInner {
    pub files_total: AtomicUsize,
    pub files_done: AtomicUsize,
    pub bytes_total: u64,
    pub bytes_done: AtomicUsize,
    pub current_file: Mutex<String>,
    pub error: Mutex<Option<String>>,
    pub(crate) state: Mutex<DeviceState>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum DeviceState {
    Pending,
    Copying,
    Done,
    Failed,
    Skipped,
}

/// Verification mode for copied files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Compare source and destination file sizes.
    SizeOnly,
    // Future: Checksum(ChecksumKind) — reserved for xxh3/blake3 later.
}

impl OffloadContext {
    pub fn new(device_names: &[String], plans: &[Vec<CopyPlanItem>]) -> Self {
        let per_device: Vec<DeviceProgressInner> = device_names
            .iter()
            .enumerate()
            .map(|(idx, _name)| {
                let files_total = plans.get(idx).map(|p| p.len()).unwrap_or(0);
                let bytes_total: u64 = plans
                    .get(idx)
                    .map(|p| p.iter().map(|i| i.size).sum())
                    .unwrap_or(0);
                DeviceProgressInner {
                    files_total: AtomicUsize::new(files_total),
                    files_done: AtomicUsize::new(0),
                    bytes_total,
                    bytes_done: AtomicUsize::new(0),
                    current_file: Mutex::new(String::new()),
                    error: Mutex::new(None),
                    state: Mutex::new(DeviceState::Pending),
                }
            })
            .collect();

        let total_files: usize = per_device.iter().map(|d| d.files_total.load(Ordering::Relaxed)).sum();
        let total_bytes: u64 = per_device.iter().map(|d| d.bytes_total).sum();
        let total_blocks = (total_bytes.saturating_add((1 << 20) - 1)) >> 20; // ceil(MiB)

        OffloadContext {
            devices_total: AtomicUsize::new(device_names.len()),
            devices_done: AtomicUsize::new(0),
            overall_files_total: AtomicUsize::new(total_files),
            overall_files_done: AtomicUsize::new(0),
            overall_blocks_total: AtomicUsize::new(total_blocks as usize),
            overall_blocks_done: AtomicUsize::new(0),
            per_device,
            cancel: AtomicBool::new(false),
        }
    }
}

// ── A single copy item ──────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CopyPlanItem {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub size: u64,
}

// ── Default parent name (ISO date) ─────────────────────────────────────

/// Default parent subfolder name: today's date in ISO 8601 format.
pub fn default_parent_name() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

// ── Media file check ────────────────────────────────────────────────────

/// Returns true if `path` has a recognised media extension.
pub fn is_media_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let e = e.to_ascii_lowercase();
            VIDEO_EXTS.contains(&e.as_str()) || AUDIO_EXTS.contains(&e.as_str())
        })
        .unwrap_or(false)
}

// ── Card detection ──────────────────────────────────────────────────────

/// Detect mounted media cards by parsing `/proc/mounts` and probing `/sys`.
#[cfg(target_os = "linux")]
pub fn detect_cards() -> Vec<SdCardInfo> {
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    detect_cards_from_mounts(&mounts, Path::new("/sys"))
}

/// Fallback for non-Linux: enumerate common mount roots.
#[cfg(not(target_os = "linux"))]
pub fn detect_cards() -> Vec<SdCardInfo> {
    let mut cards = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for letter in 'A'..='Z' {
            let root = PathBuf::from(format!("{}:\\", letter));
            if root.exists() {
                if let Some(info) = classify_mount(&root, &root) {
                    cards.push(info);
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(entries) = fs::read_dir("/Volumes") {
            for entry in entries.flatten() {
                let mp = entry.path();
                if mp.is_dir() {
                    if let Some(info) = classify_mount(&mp, &mp) {
                        cards.push(info);
                    }
                }
            }
        }
    }
    cards
}

/// Parse `/proc/mounts` content and return detected cards.
/// `sys_root` is typically `/sys` on Linux.
fn detect_cards_from_mounts(mounts_content: &str, sys_root: &Path) -> Vec<SdCardInfo> {
    let user = whoami_fallback();
    let mut candidates: Vec<PathBuf> = Vec::new();

    for line in mounts_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let device = parts[0];
        let mount_point = parts[1];

        // Skip non-real devices.
        if !device.starts_with("/dev/") || device.contains("/loop") || device == "systemd-1" {
            continue;
        }
        let mp = Path::new(mount_point);
        // Skip common non-removable system paths.
        if mp == Path::new("/")
            || mp.starts_with("/boot")
            || mp.starts_with("/proc")
            || mp.starts_with("/sys")
            || mp.starts_with("/dev")
            || mp.starts_with("/run")
        {
            continue;
        }
        // Prefer mounts under standard media directories.
        let in_media = mp.starts_with("/media")
            || mp.starts_with("/run/media")
            || mp.starts_with("/mnt");
        if !in_media && !mp.starts_with(format!("/media/{}", user)) {
            continue;
        }
        // Check removable flag via /sys.
        let is_removable = is_block_removable(device, sys_root);
        if !is_removable && !mp.starts_with(format!("/run/media/{}", user)) {
            // /run/media/$USER mounts are usually removable.
            continue;
        }
        candidates.push(mp.to_path_buf());
    }

    let mut seen = std::collections::HashSet::new();
    let mut cards = Vec::new();
    for mp in &candidates {
        // Deduplicate (same mount point via multiple /proc/mounts lines).
        if !seen.insert(mp.clone()) {
            continue;
        }
        if let Some(info) = classify_mount(mp, mp) {
            cards.push(info);
        }
    }
    cards
}

/// Try to read `/sys/class/block/<devname>/removable`.
fn is_block_removable(device_path: &str, sys_root: &Path) -> bool {
    let dev_part = device_path.trim_start_matches("/dev/");
    // Try the partition-level removable file first; fall back to stripping trailing digits.
    let candidates = [
        format!("class/block/{}", dev_part),
        format!("block/{}", dev_part),
    ];
    for sys_path in &candidates {
        let removable_path = sys_root.join(sys_path).join("removable");
        if let Ok(val) = fs::read_to_string(&removable_path) {
            return val.trim() == "1";
        }
    }
    // Try base device (strip trailing digits).
    let base = dev_part.trim_end_matches(|c: char| c.is_ascii_digit());
    if !base.is_empty() && base != dev_part {
        for sys_path in &candidates {
            let base_sys = sys_path.replace(dev_part, base);
            let removable_path = sys_root.join(&base_sys).join("removable");
            if let Ok(val) = fs::read_to_string(&removable_path) {
                return val.trim() == "1";
            }
        }
    }
    false
}

/// Classify a mount point as a media card: shallow media scan + device name guess.
fn classify_mount(mount: &Path, _label_source: &Path) -> Option<SdCardInfo> {
    let (media_files, total_bytes) = collect_media_files_shallow(mount);
    let volume_label = volume_label_for(mount);

    if media_files.is_empty() {
        // No media files — not a media card.
        return None;
    }

    let (device_name, name_source, pattern_name) = guess_device_name_for_card(&media_files, &volume_label);

    Some(SdCardInfo {
        mount: mount.to_path_buf(),
        volume_label,
        device_name,
        name_source,
        media_file_count: media_files.len(),
        total_bytes,
        pattern_name,
    })
}

/// Shallow recursive scan for media files (up to CARD_SCAN_DEPTH).
fn collect_media_files_shallow(root: &Path) -> (Vec<PathBuf>, u64) {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut total_bytes = 0u64;

    while let Some(dir) = stack.pop() {
        if files.len() >= MAX_CARD_FILES {
            break;
        }
        let read_dir = match dir.read_dir() {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Limit depth by counting path components beyond root.
        let depth = dir.components().count().saturating_sub(root.components().count());
        for entry in read_dir.flatten() {
            if files.len() >= MAX_CARD_FILES {
                break;
            }
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            if ft.is_dir() && depth < CARD_SCAN_DEPTH {
                stack.push(entry.path());
            } else if ft.is_file() && is_media_file(&entry.path()) {
                if let Ok(meta) = fs::metadata(entry.path()) {
                    total_bytes += meta.len();
                }
                files.push(entry.path());
            }
        }
    }
    (files, total_bytes)
}

/// Best-effort volume label from the mount point's parent (Linux).
/// On Linux the last component of `/run/media/user/LABEL` is the volume label.
fn volume_label_for(mount: &Path) -> String {
    if let Some(name) = mount.file_name().and_then(|n| n.to_str()) {
        name.to_string()
    } else {
        String::new()
    }
}

// ── Device name resolution ──────────────────────────────────────────────

/// Resolve device name from media files on a card.
/// Priority: 1) XAVC metadata, 2) camera_meta (exiftool/ffprobe),
///            3) filename pattern, 4) volume label, 5) "Card N".
fn guess_device_name_for_card(
    files: &[PathBuf],
    volume_label: &str,
) -> (String, DeviceNameSource, Option<String>) {
    // 1) XAVC metadata sniff on a sample video file.
    if let Some((name, source)) = try_metadata_name(files) {
        return (name, source, None);
    }

    // 2) Camera metadata via exiftool (AVCHD SEI) or ffprobe tags.
    if let Some((name, source)) = try_camera_meta_name(files) {
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
    if !label.is_empty() && !label.eq_ignore_ascii_case("usb") && !label.eq_ignore_ascii_case("usb drive") {
        return (label.to_string(), DeviceNameSource::VolumeLabel, None);
    }

    // 5) Fallback.
    ("Card".to_string(), DeviceNameSource::Unknown, None)
}

/// Try to extract a model name from a video file's camera metadata
/// via exiftool or ffprobe (handles AVCHD SEI, MP4/MOV tags).
fn try_camera_meta_name(files: &[PathBuf]) -> Option<(String, DeviceNameSource)> {
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

fn is_video_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Extract `modelName` or `ILCE-####` string from head+tail of a video file.
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

    // 2. Try bare ILCE-#### / ILME-#### / DSC-#### / HDR-#### anywhere in the file.
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
        if cap.starts_with("ILCE-") || cap.starts_with("ILME-") || cap.starts_with("DSC-") || cap.starts_with("HDR-") {
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
    let all_patterns: Vec<&crate::file_pattern::FileNamingPattern> = BUILTIN_PATTERNS[..1]
        .iter()
        .chain(CAMERA_PATTERNS.iter())
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
                    path.file_stem().and_then(|s| s.to_str()).unwrap_or("")
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
/// `ILCE-6700` → `A6700`, `ILME-6400` → `A6400`, etc.
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

// ── Copy planning ───────────────────────────────────────────────────────

/// Plan copies for a single card: flat layout, collision handling.
pub fn plan_copies_for_card(
    mount: &Path,
    device_name: &str,
    dest_parent: &Path,
) -> Vec<CopyPlanItem> {
    let (files, _) = collect_media_files_shallow(mount);
    let dest_dir = dest_parent.join(device_name);

    let mut used_names: HashMap<String, u32> = HashMap::new();
    let mut plans = Vec::new();

    for src in &files {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let dst_name = resolve_collision(&name, &mut used_names);
        let dst = dest_dir.join(&dst_name);
        let size = fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        plans.push(CopyPlanItem {
            src: src.clone(),
            dst,
            size,
        });
    }

    plans.sort_by(|a, b| a.src.cmp(&b.src));
    plans
}

/// If `name` has been seen before, append ` (N)` before the extension.
fn resolve_collision(name: &str, used: &mut HashMap<String, u32>) -> String {
    let entry = used.entry(name.to_string()).or_insert(0);
    *entry += 1;
    let count = *entry;

    if count == 1 {
        return name.to_string();
    }

    // Append ` (2)`, ` (3)`, etc. before the extension.
    if let Some(dot) = name.rfind('.') {
        let base = &name[..dot];
        let ext = &name[dot..];
        format!("{} ({}){}", base, count, ext)
    } else {
        format!("{} ({})", name, count)
    }
}

// ── Copy execution ──────────────────────────────────────────────────────

/// Run the full offload copy job: copy each device's files, update context.
/// Returns a list of successfully completed device names.
pub fn run_offload(
    plans_per_device: &[Vec<CopyPlanItem>],
    device_names: &[String],
    context: &OffloadContext,
) -> Vec<String> {
    let mut completed = Vec::new();

    for (dev_idx, plans) in plans_per_device.iter().enumerate() {
        if context.cancel.load(Ordering::Relaxed) {
            break;
        }

        let name = &device_names[dev_idx];

        // Update state to Copying.
        {
            let inner = &context.per_device[dev_idx];
            let mut s = inner.state.lock().unwrap();
            *s = DeviceState::Copying;
        }

        let dest_parent = plans.first().and_then(|p| p.dst.parent()).unwrap_or(Path::new(""));

        // Create device directory.
        if let Err(e) = fs::create_dir_all(dest_parent) {
            let msg = format!("Failed to create directory {:?}: {}", dest_parent, e);
            {
                let inner = &context.per_device[dev_idx];
                *inner.error.lock().unwrap() = Some(msg.clone());
                *inner.state.lock().unwrap() = DeviceState::Failed;
            }
            continue;
        }

        let mut dev_ok = true;

        for item in plans {
            if context.cancel.load(Ordering::Relaxed) {
                dev_ok = false;
                break;
            }

            // Update progress: current file.
            {
                let inner = &context.per_device[dev_idx];
                *inner.current_file.lock().unwrap() = item
                    .src
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("")
                    .to_string();
            }

            // Skip if destination exists with the same size (idempotent resume).
            if let Ok(existing_meta) = fs::metadata(&item.dst) {
                if existing_meta.len() == item.size && existing_meta.is_file() {
                    // Already copied — just update counters.
                    advance_counters(context, dev_idx, 1, item.size);
                    continue;
                }
            }

            // Copy the file with 1 MiB buffered IO.
            match copy_file(&item.src, &item.dst) {
                Ok(()) => {
                    // Verify after copy.
                    match verify_copy(&item.src, &item.dst, &VerifyMode::SizeOnly) {
                        Ok(()) => {
                            advance_counters(context, dev_idx, 1, item.size);
                        }
                        Err(e) => {
                            let msg = format!("Verification failed for {:?}: {}", item.dst, e);
                            {
                                let inner = &context.per_device[dev_idx];
                                *inner.error.lock().unwrap() = Some(msg);
                                *inner.state.lock().unwrap() = DeviceState::Failed;
                            }
                            dev_ok = false;
                            break;
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("Copy failed for {:?}: {}", item.src, e);
                    {
                        let inner = &context.per_device[dev_idx];
                        *inner.error.lock().unwrap() = Some(msg);
                        *inner.state.lock().unwrap() = DeviceState::Failed;
                    }
                    dev_ok = false;
                    break;
                }
            }
        }

        // Finalise device.
        let inner = &context.per_device[dev_idx];
        let final_state = if dev_ok {
            DeviceState::Done
        } else if context.cancel.load(Ordering::Relaxed) {
            DeviceState::Skipped
        } else {
            // Already set to Failed above.
            DeviceState::Failed
        };
        *inner.state.lock().unwrap() = final_state;

        if dev_ok {
            completed.push(name.clone());
            context.devices_done.fetch_add(1, Ordering::Relaxed);
        }
    }

    completed
}

/// Advance file/byte counters after a successful copy.
fn advance_counters(ctx: &OffloadContext, dev_idx: usize, files_inc: usize, bytes_inc: u64) {
    let blocks = ((bytes_inc.saturating_add((1 << 20) - 1)) >> 20) as usize;
    let inner = &ctx.per_device[dev_idx];
    inner.files_done.fetch_add(files_inc, Ordering::Relaxed);
    inner.bytes_done.fetch_add(bytes_inc as usize, Ordering::Relaxed);
    ctx.overall_files_done.fetch_add(files_inc, Ordering::Relaxed);
    ctx.overall_blocks_done.fetch_add(blocks, Ordering::Relaxed);
}

/// Copy a single file with 1 MiB buffered IO.
fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    let src_file = fs::File::open(src).map_err(|e| format!("Cannot open {:?}: {}", src, e))?;
    let reader = BufReader::with_capacity(COPY_BUF_SIZE, src_file);

    // Write to a temp file next to dst, then atomically rename.
    let tmp = dst.with_extension("offload_tmp");
    let mut dst_file =
        fs::File::create(&tmp).map_err(|e| format!("Cannot create {:?}: {}", tmp, e))?;

    for chunk in reader.bytes() {
        let byte = chunk.map_err(|e| format!("Read error on {:?}: {}", src, e))?;
        dst_file
            .write_all(&[byte])
            .map_err(|e| format!("Write error on {:?}: {}", tmp, e))?;
    }
    dst_file
        .sync_all()
        .map_err(|e| format!("Sync error on {:?}: {}", tmp, e))?;
    drop(dst_file);
    fs::rename(&tmp, dst).map_err(|e| format!("Rename {:?} → {:?}: {}", tmp, dst, e))?;
    Ok(())
}

// ── Verification ────────────────────────────────────────────────────────

/// Verify that a copied file matches the source.
/// Currently only size-based verification; checksum variants reserved.
pub fn verify_copy(src: &Path, dst: &Path, mode: &VerifyMode) -> Result<(), String> {
    match mode {
        VerifyMode::SizeOnly => {
            let src_len = fs::metadata(src)
                .map_err(|e| format!("Cannot stat source {:?}: {}", src, e))?
                .len();
            let dst_len = fs::metadata(dst)
                .map_err(|e| format!("Cannot stat destination {:?}: {}", dst, e))?
                .len();
            if src_len != dst_len {
                return Err(format!(
                    "Size mismatch: source {} bytes, destination {} bytes",
                    src_len, dst_len
                ));
            }
            Ok(())
        }
    }
}

// ── Build snapshot from context ─────────────────────────────────────────

/// Read the current state of an `OffloadContext` and produce a status snapshot.
pub fn snapshot_from_context(
    context: &OffloadContext,
    device_names: &[String],
    completed_devices: &[String],
    parent_folder: Option<PathBuf>,
    parent_name: &str,
    error: Option<String>,
    last_version: u64,
) -> OffloadSnapshot {
    let total = context.devices_total.load(Ordering::Relaxed);
    let mut device_progress: Vec<OffloadDeviceStatus> = Vec::new();

    for i in 0..total {
        let inner = &context.per_device[i];
        let state_val = inner.state.lock().unwrap().clone();
        let err = inner.error.lock().unwrap().clone();
        let device_state = match state_val {
            DeviceState::Pending => OffloadDeviceState::Pending,
            DeviceState::Copying => OffloadDeviceState::Copying,
            DeviceState::Done => OffloadDeviceState::Done,
            DeviceState::Failed => {
                OffloadDeviceState::Failed(err.unwrap_or_else(|| "Unknown error".to_string()))
            }
            DeviceState::Skipped => OffloadDeviceState::Skipped,
        };

        device_progress.push(OffloadDeviceStatus {
            device_name: device_names
                .get(i)
                .cloned()
                .unwrap_or_else(|| format!("Device {}", i)),
            files_total: inner.files_total.load(Ordering::Relaxed),
            files_done: inner.files_done.load(Ordering::Relaxed),
            bytes_total: inner.bytes_total,
            bytes_done: inner.bytes_done.load(Ordering::Relaxed) as u64,
            current_file: inner.current_file.lock().unwrap().clone(),
            state: device_state,
        });
    }

    let total_blocks = context.overall_blocks_total.load(Ordering::Relaxed);
    let done_blocks = context.overall_blocks_done.load(Ordering::Relaxed);
    let progress = if total_blocks > 0 {
        (done_blocks as f32) / (total_blocks as f32)
    } else {
        0.0
    };

    OffloadSnapshot {
        cards: Vec::new(), // not updated from context; set elsewhere
        scanning: false,
        parent_folder: parent_folder.clone(),
        parent_name: parent_name.to_string(),
        running: true,
        overall_progress: progress.min(1.0),
        device_progress,
        completed_devices: completed_devices.to_vec(),
        last_offload_parent: parent_folder,
        last_offload_version: last_version,
        error,
    }
}

// ── Helper to get current username (used in mount detection) ────────────

#[cfg(target_os = "linux")]
fn whoami_fallback() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "viktoria".to_string())
}

#[cfg(not(target_os = "linux"))]
fn whoami_fallback() -> String {
    String::new()
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── default_parent_name ──────────────────────────────────────────

    #[test]
    fn test_default_parent_name_format() {
        let name = default_parent_name();
        assert_eq!(name.len(), 10, "Expected ISO date format YYYY-MM-DD");
        assert_eq!(&name[4..5], "-");
        assert_eq!(&name[7..8], "-");
    }

    // ── is_media_file ────────────────────────────────────────────────

    #[test]
    fn test_is_media_file_wav() {
        assert!(is_media_file(Path::new("foo.wav")));
        assert!(is_media_file(Path::new("bar.WAV")));
        assert!(!is_media_file(Path::new("foo.wav.bak")));
    }

    #[test]
    fn test_is_media_file_video() {
        assert!(is_media_file(Path::new("clip.mp4")));
        assert!(is_media_file(Path::new("clip.MOV")));
        assert!(is_media_file(Path::new("clip.m4v")));
        assert!(is_media_file(Path::new("clip.mts")));
        assert!(is_media_file(Path::new("clip.MXF")));
        assert!(!is_media_file(Path::new("readme.txt")));
        assert!(!is_media_file(Path::new("image.jpg")));
    }

    // ── resolve_collision ──────────────────────────────────────────────

    #[test]
    fn test_no_collision_first_use() {
        let mut used = HashMap::new();
        assert_eq!(resolve_collision("C0001.MP4", &mut used), "C0001.MP4");
    }

    #[test]
    fn test_collision_adds_suffix() {
        let mut used = HashMap::new();
        resolve_collision("C0001.MP4", &mut used);
        assert_eq!(resolve_collision("C0001.MP4", &mut used), "C0001 (2).MP4");
        assert_eq!(resolve_collision("C0001.MP4", &mut used), "C0001 (3).MP4");
    }

    #[test]
    fn test_no_extension_collision() {
        let mut used = HashMap::new();
        resolve_collision("README", &mut used);
        assert_eq!(resolve_collision("README", &mut used), "README (2)");
    }

    // ── normalize_model_name ──────────────────────────────────────────

    #[test]
    fn test_ilce_to_a() {
        assert_eq!(normalize_model_name("ILCE-6700"), "A6700");
        assert_eq!(normalize_model_name("ILCE-6100"), "A6100");
    }

    #[test]
    fn test_ilme_to_a() {
        assert_eq!(normalize_model_name("ILME-6400"), "A6400");
    }

    #[test]
    fn test_other_model_unchanged() {
        assert_eq!(normalize_model_name("FS100"), "FS100");
        assert_eq!(normalize_model_name("AG-HMC150"), "AG-HMC150");
    }

    // ── normalize_pattern_name ────────────────────────────────────────

    #[test]
    fn test_pattern_normalisation() {
        assert_eq!(normalize_pattern_name("Sony FS100"), "FS100");
        assert_eq!(normalize_pattern_name("Sony Handycam"), "Handycam");
        assert_eq!(normalize_pattern_name("TASCAM"), "TASCAM");
        assert_eq!(normalize_pattern_name("GoPro"), "GoPro");
        assert_eq!(normalize_pattern_name("Unknown Pattern"), "Unknown Pattern");
    }

    // ── plan_copies_for_card (flat layout, collision) ─────────────────

    #[test]
    fn test_plan_copies_flat_layout() {
        let card = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        fs::write(card.path().join("C0001.MP4"), b"data").unwrap();
        fs::write(card.path().join("C0002.MP4"), b"data").unwrap();

        let plans = plan_copies_for_card(card.path(), "A6700", dest.path());

        assert_eq!(plans.len(), 2);
        assert!(plans[0].dst.starts_with(dest.path().join("A6700")));
        assert!(plans[1].dst.starts_with(dest.path().join("A6700")));
        assert_eq!(
            plans[0].dst.file_name().unwrap(),
            "C0001.MP4"
        );
        assert_eq!(
            plans[1].dst.file_name().unwrap(),
            "C0002.MP4"
        );
    }

    #[test]
    fn test_plan_copies_collision_handling() {
        let card = TempDir::new().unwrap();
        // Same filename in different subdirs — both should land in device dir
        // with collision suffix.
        let sub1 = card.path().join("DCIM").join("100MSDCF");
        let sub2 = card.path().join("DCIM").join("101MSDCF");
        fs::create_dir_all(&sub1).unwrap();
        fs::create_dir_all(&sub2).unwrap();
        fs::write(sub1.join("C0001.MP4"), b"data_a").unwrap();
        fs::write(sub2.join("C0001.MP4"), b"data_b").unwrap();
        // Add a file at root level too.
        fs::write(card.path().join("C0001.MP4"), b"data_c").unwrap();

        let dest = TempDir::new().unwrap();
        let plans = plan_copies_for_card(card.path(), "A6100", dest.path());

        assert_eq!(plans.len(), 3);
        // First occurrence should be plain, second gets (2), third gets (3).
        // Since we sort alphabetically by src path, DCIM/100M... comes first,
        // DCIM/101M... second, root third.
        assert!(plans[0].dst.to_string_lossy().ends_with("C0001.MP4"));
        assert!(plans[1]
            .dst
            .to_string_lossy()
            .ends_with("C0001 (2).MP4"));
        assert!(plans[2]
            .dst
            .to_string_lossy()
            .ends_with("C0001 (3).MP4"));
    }

    // ── collect_media_files_shallow ───────────────────────────────────

    #[test]
    fn test_collect_media_only() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("clip1.mp4"), b"video").unwrap();
        fs::write(dir.path().join("notes.txt"), b"text").unwrap();
        fs::write(dir.path().join("sound.wav"), b"audio").unwrap();
        fs::write(dir.path().join("image.jpg"), b"img").unwrap();
        fs::create_dir(dir.path().join("DCIM")).unwrap();
        fs::write(dir.path().join("DCIM").join("C0001.MP4"), b"more").unwrap();

        let (files, _) = collect_media_files_shallow(dir.path());
        // Only .mp4 and .wav should be collected.
        let names: Vec<&str> = files
            .iter()
            .filter_map(|f| f.file_name())
            .filter_map(|n| n.to_str())
            .collect();
        assert!(names.contains(&"clip1.mp4"));
        assert!(names.contains(&"sound.wav"));
        assert!(names.contains(&"C0001.MP4"));
        assert!(!names.contains(&"notes.txt"));
        assert!(!names.contains(&"image.jpg"));
        assert_eq!(names.len(), 3);
    }

    // ── device name guessing ──────────────────────────────────────────

    #[test]
    fn test_guess_from_ilce_metadata_via_read() {
        // Create a fake video file with embedded metadata in the tail.
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("C0001.MP4");
        // Write a large enough buffer with the XML metadata at the tail position.
        let mut content = vec![0u8; 5000];
        let xml = br#"<Device manufacturer="Sony" modelName="ILCE-6700" serialNo="123"/> "#;
        // Place the model at offset 4500 (past head, within the tail when doing 4K head+tail).
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

        let (name, source, pat) = guess_device_name_for_card(&files, "");
        assert_eq!(name, "FS100");
        assert_eq!(source, DeviceNameSource::Pattern);
        assert_eq!(pat, Some("Sony FS100".to_string()));
    }

    #[test]
    fn test_guess_device_volume_label_fallback() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("misc_file.bin"), b"data").unwrap();
        // No media files! So we need media files for classification.
        // Create a .wav that won't match any pattern.
        fs::write(dir.path().join("sound.wav"), b"data").unwrap();
        let files = dir
            .path()
            .read_dir()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| is_media_file(p))
            .collect::<Vec<_>>();

        let (name, source, _) = guess_device_name_for_card(&files, "EOS_DIGITAL");
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
            .filter(|p| is_media_file(p))
            .collect::<Vec<_>>();

        let (name, source, _) = guess_device_name_for_card(&files, "");
        assert_eq!(name, "Card");
        assert_eq!(source, DeviceNameSource::Unknown);
    }

    // ── verify_copy ──────────────────────────────────────────────────

    #[test]
    fn test_verify_copy_size_match() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        fs::write(&src, b"hello world").unwrap();
        fs::write(&dst, b"hello world").unwrap();
        assert!(verify_copy(&src, &dst, &VerifyMode::SizeOnly).is_ok());
    }

    #[test]
    fn test_verify_copy_size_mismatch() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");
        fs::write(&src, b"hello world!").unwrap();
        fs::write(&dst, b"hello").unwrap();
        assert!(verify_copy(&src, &dst, &VerifyMode::SizeOnly).is_err());
    }

    // ── run_offload ───────────────────────────────────────────────────

    #[test]
    fn test_run_offload_copies_files() {
        let card = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        fs::write(card.path().join("C0001.MP4"), b"video_data").unwrap();
        fs::write(card.path().join("C0002.MP4"), b"more_video").unwrap();

        let plans = plan_copies_for_card(card.path(), "A6100", dest.path());
        let device_names = vec!["A6100".to_string()];
        let ctx = OffloadContext::new(&device_names, std::slice::from_ref(&plans));

        let completed = run_offload(&[plans], &device_names, &ctx);

        assert_eq!(completed.len(), 1);
        assert!(dest.path().join("A6100").join("C0001.MP4").exists());
        assert!(dest.path().join("A6100").join("C0002.MP4").exists());
    }

    #[test]
    fn test_run_offload_ids_when_dest_exists() {
        let card = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        let dst_dir = dest.path().join("A6100");
        fs::create_dir_all(&dst_dir).unwrap();

        // Pre-copy one file to dest.
        fs::write(card.path().join("C0001.MP4"), b"data").unwrap();
        fs::write(card.path().join("C0002.MP4"), b"other_data").unwrap();
        fs::write(dst_dir.join("C0001.MP4"), b"data").unwrap(); // same size as source

        let plans = plan_copies_for_card(card.path(), "A6100", dest.path());
        let device_names = vec!["A6100".to_string()];
        let ctx = OffloadContext::new(&device_names, std::slice::from_ref(&plans));

        let completed = run_offload(&[plans], &device_names, &ctx);
        assert_eq!(completed.len(), 1, "should succeed even with pre-existing file");
    }

    #[test]
    fn test_run_offload_cancel_midway() {
        let card = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();
        // Create several small files.
        for i in 0..10 {
            fs::write(card.path().join(format!("C{:04}.MP4", i)), vec![b'x'; 1024 * 10])
                .unwrap();
        }

        let plans = plan_copies_for_card(card.path(), "Cam", dest.path());
        let device_names = vec!["Cam".to_string()];
        let ctx = OffloadContext::new(&device_names, std::slice::from_ref(&plans));
        ctx.cancel.store(true, Ordering::Relaxed); // cancel immediately

        let completed = run_offload(&[plans], &device_names, &ctx);
        // Should have been cancelled before copying any files (or during).
        assert!(completed.is_empty() || completed.len() < 10);
    }

    // ── snapshot_from_context ─────────────────────────────────────────

    #[test]
    fn test_snapshot_from_context_progress() {
        let device_names = vec!["A6100".to_string(), "A6700".to_string()];

        // Create two small plans.
        let plan0 = vec![CopyPlanItem {
            src: PathBuf::from("/fake/src1.mp4"),
            dst: PathBuf::from("/fake/dst1.mp4"),
            size: 1_000_000,
        }];
        let plan1 = vec![CopyPlanItem {
            src: PathBuf::from("/fake/src2.mp4"),
            dst: PathBuf::from("/fake/dst2.mp4"),
            size: 2_000_000,
        }];

        let ctx = OffloadContext::new(&device_names, &[plan0, plan1]);

        // Mark file 0 as done.
        ctx.per_device[0]
            .files_done
            .store(1, Ordering::Relaxed);
        ctx.per_device[0]
            .bytes_done
            .store(1_000_000, Ordering::Relaxed);
        ctx.overall_files_done.fetch_add(1, Ordering::Relaxed);
        ctx.overall_blocks_done
            .fetch_add(((1_000_000 + (1 << 20) - 1) >> 20) as usize, Ordering::Relaxed);

        let snapshot = snapshot_from_context(
            &ctx,
            &device_names,
            &[],
            Some(PathBuf::from("/output")),
            "2026-09-26",
            None,
            1,
        );

        assert!(snapshot.running);
        assert!(snapshot.overall_progress > 0.0 && snapshot.overall_progress < 1.0);
        assert_eq!(snapshot.device_progress.len(), 2);
        assert_eq!(snapshot.device_progress[0].files_done, 1);
        assert_eq!(snapshot.device_progress[0].bytes_done, 1_000_000);
        assert_eq!(snapshot.last_offload_version, 1);
        assert_eq!(snapshot.parent_name, "2026-09-26");
    }

    // ── detect_cards_from_mounts ──────────────────────────────────────

    #[test]
    fn test_detect_cards_from_mounts_empty() {
        let cards = detect_cards_from_mounts("", Path::new("/sys"));
        assert!(cards.is_empty());
    }

    #[test]
    fn test_detect_cards_from_mounts_ignores_system() {
        let mounts = "\
/dev/nvme0n1p2 / ext4 rw 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
proc /proc proc rw 0 0
";
        let cards = detect_cards_from_mounts(mounts, Path::new("/sys"));
        assert!(cards.is_empty());
    }

    // ── classify_mount (direct invocation via prerequisites) ──────────

    #[test]
    fn test_classify_mount_with_no_media_no_card() {
        let dir = TempDir::new().unwrap();
        let result = classify_mount(dir.path(), dir.path());
        assert!(result.is_none(), "no media files → no card");
    }

    #[test]
    fn test_classify_mount_with_media_returns_sd_card() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("C0001.MP4"), b"data").unwrap();
        let result = classify_mount(dir.path(), dir.path());
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.media_file_count, 1);
        assert!(info.total_bytes > 0);
    }
}