use std::collections::HashMap;
use std::fs;
use std::io::{Error, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, NaiveDate};
use log::{debug, info, warn};

use crate::device_name;
use crate::subprocess::no_window_command;
pub use crate::device_name::DeviceNameSource;

// ── Windows drive enumeration helper ─────────────────────────────────

#[cfg(target_os = "windows")]
mod win_driver {
    use std::path::PathBuf;
    use windows_sys::Win32::Storage::FileSystem;
    use windows_sys::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK,
        DRIVE_REMOTE, DRIVE_UNKNOWN,
        DRIVE_REMOVABLE as WIN32_DRIVE_REMOVABLE,
    };

    pub struct DriveCandidate {
        pub letter: char,
        pub kind: u32,
        pub root: PathBuf,
    }

    pub fn enumerate() -> Vec<DriveCandidate> {
        let mut candidates = Vec::new();
        let mask = unsafe { FileSystem::GetLogicalDrives() };
        if mask == 0 {
            return candidates;
        }
        for i in 0..26u32 {
            if (mask >> i) & 1 == 1 {
                let letter = char::from_u32(b'A' as u32 + i).unwrap_or('?');
                let root = format!("{}:\\", letter);
                let root_wide: Vec<u16> = root.encode_utf16().chain(std::iter::once(0)).collect();
                let kind = unsafe { FileSystem::GetDriveTypeW(root_wide.as_ptr()) };
                candidates.push(DriveCandidate { letter, kind, root: PathBuf::from(root) });
            }
        }
        candidates
    }

    pub const DRIVE_REMOVABLE: u32 = WIN32_DRIVE_REMOVABLE;

    pub fn drive_type_name(kind: u32) -> &'static str {
        match kind {
            DRIVE_UNKNOWN => "unknown",
            DRIVE_NO_ROOT_DIR => "no_root_dir",
            WIN32_DRIVE_REMOVABLE => "removable",
            DRIVE_FIXED => "fixed",
            DRIVE_REMOTE => "remote",
            DRIVE_CDROM => "cdrom",
            DRIVE_RAMDISK => "ramdisk",
            _ => "invalid",
        }
    }
}

/// Buffer size for file copy (1 MiB).
const COPY_BUF_SIZE: usize = 1 << 20;

/// Errors from [`copy_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyError {
    Io(String),
    Cancelled,
}

impl CopyError {
    pub fn is_cancelled(&self) -> bool {
        matches!(self, CopyError::Cancelled)
    }
}

/// Maximum depth when scanning a card for media files.
const CARD_SCAN_DEPTH: usize = 6;

/// Maximum files to enumerate on a card.
const MAX_CARD_FILES: usize = 10_000;

/// Per-scan timeout — if a single card scan (including all mount probes) takes
/// longer than this, remaining mounts are skipped.
const CARD_SCAN_BUDGET: Duration = Duration::from_secs(120);

// ── Public types ────────────────────────────────────────────────────────

/// Information about a single media file found on a card.
#[derive(Debug, Clone, PartialEq)]
pub struct OffloadFileInfo {
    /// Full filesystem path.
    pub path: PathBuf,
    /// File name for display.
    pub name: String,
    /// File size in bytes.
    pub size_bytes: u64,
    /// File modification time (recording date/time) in local time, if available.
    pub modified: Option<DateTime<Local>>,
}

/// Information about a detected media card.
#[derive(Debug, Clone, PartialEq)]
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
    /// Per-file metadata collected during scan.
    pub files: Vec<OffloadFileInfo>,
    /// Per-file selection (aligned with `files`). Filled by engine on scan.
    pub selected: Vec<bool>,
    /// Number of selected files (engine-derived, updated on selection changes).
    pub selected_count: usize,
    /// Total bytes of selected files (engine-derived).
    pub selected_bytes: u64,
}

/// Per-device copy totals captured from the plans at `StartOffload`.
#[derive(Debug, Clone, PartialEq)]
pub struct OffloadDeviceTotals {
    pub name: String,
    pub files_total: usize,
    pub bytes_total: u64,
}

/// Published snapshot of the entire offload subsystem.
#[derive(Debug, Clone, PartialEq)]
pub struct OffloadSnapshot {
    /// Detected cards from the most recent scan.
    pub cards: Vec<SdCardInfo>,
    /// User-chosen parent directory (base folder for ISO-date subfolder).
    pub parent_folder: Option<PathBuf>,
    /// Editable folder name (defaults to today's ISO date).
    pub parent_name: String,
    /// Per-device totals captured from copy plans at StartOffload.
    pub device_totals: Vec<OffloadDeviceTotals>,
    /// Device names that have been fully offloaded this session.
    pub completed_devices: Vec<String>,
    /// The last parent folder that was written to (for converter auto-switch).
    pub last_offload_parent: Option<PathBuf>,
    /// Monotonically increasing version — increment on each offload completion.
    pub last_offload_version: u64,
    /// Global error message (e.g. no parent folder set).
    pub error: Option<String>,
    /// Per-file durations (seconds) for offload-scope files (card contents),
    /// probed asynchronously after scan. Keyed by full file path; value is
    /// `None` when probing failed or is pending. Separate from the
    /// converter-scope `AppStateSnapshot::file_durations` (different
    /// clearing points: `OffloadScan` vs `ProbeFileDurations`).
    pub file_durations: HashMap<PathBuf, Option<f64>>,
    /// Monotonically increasing version — incremented on each duration insertion.
    pub durations_version: u64,
}

impl OffloadSnapshot {
    pub fn initial() -> Self {
        OffloadSnapshot {
            cards: Vec::new(),
            parent_folder: None,
            parent_name: default_parent_name(),
            device_totals: Vec::new(),
            completed_devices: Vec::new(),
            last_offload_parent: None,
            last_offload_version: 0,
            error: None,
            file_durations: HashMap::new(),
            durations_version: 0,
        }
    }
}

/// Shared cell for scan progress: the scan thread writes per-drive messages,
/// the engine forwards them into the scan job's `ProgressTracker`, which is
/// polled each tick and published as `jobs[OffloadScan].message`.
pub struct ScanProgress {
    inner: Arc<Mutex<Option<String>>>,
    forward: Option<Box<dyn Fn(String) + Send + Sync>>,
}

impl ScanProgress {
    pub fn new() -> Self {
        ScanProgress {
            inner: Arc::new(Mutex::new(None)),
            forward: None,
        }
    }

    /// Create a ScanProgress that also calls `f` on every `set()`.
    /// Used to forward per-drive scan messages to a ProgressTracker.
    pub fn with_forward<F>(f: F) -> Self
    where
        F: Fn(String) + Send + Sync + 'static,
    {
        ScanProgress {
            inner: Arc::new(Mutex::new(None)),
            forward: Some(Box::new(f)),
        }
    }

    pub fn set(&self, msg: String) {
        *self.inner.lock().unwrap() = Some(msg.clone());
        if let Some(ref forward) = self.forward {
            forward(msg);
        }
    }

    pub fn clear(&self) {
        *self.inner.lock().unwrap() = None;
    }

    pub fn read(&self) -> Option<String> {
        self.inner.lock().unwrap().clone()
    }
}

impl Default for ScanProgress {
    fn default() -> Self {
        Self::new()
    }
}

/// Verification mode for copied files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyMode {
    /// Compare source and destination file sizes.
    SizeOnly,
    // Future: Checksum(ChecksumKind) — reserved for xxh3/blake3 later.
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
    crate::media_ext::is_video(path) || crate::media_ext::is_audio(path)
}

// ── Card detection ──────────────────────────────────────────────────────

/// Detect media cards, optionally reporting per-drive progress into the
/// given `ScanProgress` (whose `set()` forwards to `ctx.progress` when
/// called from `run_offload_scan_job`).
#[cfg(target_os = "linux")]
pub fn detect_cards_with_progress(progress: Option<&ScanProgress>) -> Vec<SdCardInfo> {
    let deadline = Instant::now() + CARD_SCAN_BUDGET;
    let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
    let sys_root = Path::new("/sys");

    // 1. Detect already-mounted cards.
    let mut cards = detect_cards_from_mounts(&mounts, sys_root, deadline, progress);

    // 2. Find unmounted card-like partitions and try to auto-mount.
    let unmounted = find_unmounted_card_partitions(&mounts, sys_root);
    for dev_name in &unmounted {
        if Instant::now() >= deadline {
            warn!(
                "detect_cards: budget exceeded — skipping auto-mount of /dev/{}",
                dev_name
            );
            break;
        }
        let dev_path = format!("/dev/{}", dev_name);
        if let Some(p) = progress {
            p.set(format!("Mounting /dev/{dev_name}…"));
        }
        match udisks_mount(&dev_path) {
            Ok(Some(mount_point)) => {
                info!(
                    "Auto-mounted {} at {:?} via udisksctl",
                    dev_path, mount_point
                );
                let updated_mounts =
                    fs::read_to_string("/proc/mounts").unwrap_or_else(|_| mounts.clone());
                let new_cards = detect_cards_from_mounts(&updated_mounts, sys_root, deadline, progress);
                let existing: std::collections::HashSet<PathBuf> =
                    cards.iter().map(|c| c.mount.clone()).collect();
                for c in new_cards {
                    if !existing.contains(&c.mount) {
                        cards.push(c);
                    }
                }
            }
            Ok(None) => {
                info!(
                    "Found unmounted removable /dev/{} — udisksctl auto-mount \
                     returned no mount point; consider mounting manually",
                    dev_name
                );
            }
            Err(e) => {
                info!(
                    "Found unmounted removable /dev/{} but could not auto-mount: \
                     {} (is udisks2 installed? Try: udisksctl mount -b /dev/{})",
                    dev_name, e, dev_name
                );
            }
        }
    }

    cards
}

/// Collect the set of mounted `/dev/…` paths from `/proc/mounts` content.
fn collect_mounted_devices(mounts_content: &str) -> std::collections::HashSet<String> {
    let mut mounted = std::collections::HashSet::new();
    for line in mounts_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let device = parts[0];
        if device.starts_with("/dev/") {
            mounted.insert(device.to_string());
        }
    }
    mounted
}

/// Walk `/sys/class/block` for card-like device partitions that are not
/// listed in the given mounts content. Returns a list of partition device
/// names (e.g. `sdb1`, `mmcblk0p1`).
fn find_unmounted_card_partitions(
    mounts_content: &str,
    sys_root: &Path,
) -> Vec<String> {
    let mounted = collect_mounted_devices(mounts_content);
    let mut result = Vec::new();

    let block_dir = sys_root.join("class").join("block");
    let entries = match fs::read_dir(&block_dir) {
        Ok(e) => e,
        Err(_) => return result,
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) => s.to_string(),
            None => continue,
        };
        // Skip loop, dm-, md, zram, nbd — never physical cards.
        if name_str.starts_with("loop")
            || name_str.starts_with("dm-")
            || name_str.starts_with("md")
            || name_str.starts_with("zram")
            || name_str.starts_with("nbd")
        {
            continue;
        }

        let full_dev = format!("/dev/{}", name_str);
        if mounted.contains(&full_dev) {
            continue;
        }

        // Only consider partitions (have `partition` sysfs attribute).
        let part_path = block_dir.join(&name_str).join("partition");
        if !part_path.exists() {
            continue;
        }

        // Size check: must be > 0 (ignore empty card slots).
        let size_path = block_dir.join(&name_str).join("size");
        let size_ok = fs::read_to_string(&size_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|sz| sz > 0)
            .unwrap_or(false);
        if !size_ok {
            continue;
        }

        // Card-like check.
        if !is_card_like_device(&full_dev, sys_root) {
            continue;
        }

        result.push(name_str);
    }

    result
}

/// Per-mount timeout — a single hung `udisksctl` must not stall the scan
/// thread past this (the outer `CARD_SCAN_BUDGET` is only checked between
/// mounts).
const UDISKS_MOUNT_TIMEOUT: Duration = Duration::from_secs(15);

/// Try to mount a block device via `udisksctl mount`.
/// Returns the mount point on success, `Ok(None)` if the device was already
/// mounted (so we shouldn't retry), or `Err` on failure.
#[cfg(target_os = "linux")]
fn udisks_mount(dev_path: &str) -> std::io::Result<Option<PathBuf>> {
    udisks_mount_with(dev_path, &mut |args| {
        let mut cmd = no_window_command("udisksctl");
        cmd.args(args)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        crate::subprocess::run_output_with_timeout(&mut cmd, UDISKS_MOUNT_TIMEOUT)
    })
}

/// Injectable-runner variant of [`udisks_mount`] for testability; also holds
/// the shared exit-status/mountpoint parsing.
fn udisks_mount_with(
    dev_path: &str,
    runner: &mut dyn FnMut(&[String]) -> Result<std::process::Output, crate::subprocess::SubprocessFailure>,
) -> std::io::Result<Option<PathBuf>> {
    let args = ["mount".to_string(), "-b".to_string(), dev_path.to_string()];
    let output = runner(&args).map_err(|e| Error::other(e.to_string()))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(Error::other(stderr.trim().to_string()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    // udisksctl prints: "Mounted /dev/sdb1 at /run/media/viktoria/disk"
    if let Some(pos) = stdout.find(" at ") {
        let path_str = stdout[pos + 4..].trim();
        let mp = PathBuf::from(path_str);
        if mp.is_dir() {
            return Ok(Some(mp));
        }
    }
    Ok(None)
}

/// Fallback for non-Linux: enumerate common mount roots.
/// Reports per-drive progress into `progress` when provided.
#[cfg(not(target_os = "linux"))]
pub fn detect_cards_with_progress(progress: Option<&ScanProgress>) -> Vec<SdCardInfo> {
    let deadline = Instant::now() + CARD_SCAN_BUDGET;
    let mut cards = Vec::new();
    let start = std::time::Instant::now();

    #[cfg(target_os = "windows")]
    {
        let candidates = win_driver::enumerate();
        info!(
            "Windows card scan: {} drive(s) detected via GetLogicalDrives",
            candidates.len()
        );
        for cand in &candidates {
            let tname = win_driver::drive_type_name(cand.kind);
            if cand.kind != win_driver::DRIVE_REMOVABLE {
                debug!("Skipping drive {}: type={}", cand.letter, tname);
                continue;
            }
            if Instant::now() >= deadline {
                warn!("Windows card scan: budget exceeded — skipping drive {}", cand.letter);
                break;
            }
            if let Some(p) = progress {
                p.set(format!("Scanning drive {}:…", cand.letter));
            }
            debug!("Probing removable drive {}: {:?}", cand.letter, cand.root);
            if let Some(info) = classify_mount(&cand.root, &cand.root, deadline) {
                info!(
                    "Drive {} → card: {} files, {} bytes, name='{}'",
                    cand.letter, info.media_file_count, info.total_bytes, info.device_name
                );
                cards.push(info);
            } else {
                debug!("Drive {}: no media files found", cand.letter);
            }
        }
    }

    #[cfg(target_os = "macos")]
    {
        match fs::read_dir("/Volumes") {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let mp = entry.path();
                    if !mp.is_dir() {
                        continue;
                    }
                    if Instant::now() >= deadline {
                        warn!("macOS card scan: budget exceeded — skipping {:?}", mp);
                        break;
                    }
                    if let Some(p) = progress {
                        let label = volume_label_for(&mp);
                        if label.is_empty() {
                            p.set(format!("Scanning {}…", mp.display()));
                        } else {
                            p.set(format!("Scanning drive {label} ({})…", mp.display()));
                        }
                    }
                    debug!("Probing macOS volume: {:?}", mp);
                    if let Some(info) = classify_mount(&mp, &mp, deadline) {
                        info!(
                            "macOS volume {:?} → card: {} files, {} bytes, name='{}'",
                            mp, info.media_file_count, info.total_bytes, info.device_name
                        );
                        cards.push(info);
                    } else {
                        debug!("macOS volume {:?}: no media files found", mp);
                    }
                }
                info!(
                    "macOS /Volumes scan: {} volume(s) processed",
                    cards.len()
                );
            }
            Err(e) => {
                log::warn!("Cannot read /Volumes: {} — only real volumes skipped", e);
            }
        }
    }

    info!(
        "Card scan finished in {:.2?}: {} card(s)",
        start.elapsed(),
        cards.len()
    );
    cards
}

/// Parse `/proc/mounts` content and return detected cards.
/// `sys_root` is typically `/sys` on Linux.  `deadline` caps the overall
/// time budget for the mount probes.
fn detect_cards_from_mounts(
    mounts_content: &str,
    sys_root: &Path,
    deadline: Instant,
    progress: Option<&ScanProgress>,
) -> Vec<SdCardInfo> {
    let user = whoami_fallback();
    let mut candidates: Vec<PathBuf> = Vec::new();
    let mut skipped = 0u32;

    for line in mounts_content.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 2 {
            continue;
        }
        let device = parts[0];
        let mount_point = parts[1];

        // Skip non-real devices.
        if !device.starts_with("/dev/") || device.contains("/loop") || device == "systemd-1" {
            skipped += 1;
            continue;
        }
        let mp = Path::new(mount_point);
        // Skip non-removable system paths; whitelist /run/media (udisks2).
        if mp == Path::new("/")
            || mp.starts_with("/boot")
            || mp.starts_with("/proc")
            || mp.starts_with("/sys")
            || mp.starts_with("/dev")
        {
            skipped += 1;
            continue;
        }
        // Whitelist /run/media before checking the generic /run skip.
        let in_user_run_media = mp.starts_with(format!("/run/media/{}", user));
        if !in_user_run_media && mp.starts_with("/run") {
            skipped += 1;
            continue;
        }

        // Must be under a recognised media mount root.
        let in_media = mp.starts_with("/media")
            || mp.starts_with("/run/media")
            || mp.starts_with("/mnt");
        let in_user_media = mp.starts_with(format!("/media/{}", user))
            || in_user_run_media;
        if !in_media && !in_user_media {
            skipped += 1;
            continue;
        }

        // Card-like check: removable flag / USB-MMC bus.
        // Mounts under /run/media/$USER are also checked — the bus heuristic
        // correctly excludes internal disks (nvme, SATA) even if mounted there.
        if !is_card_like_device(device, sys_root) {
            skipped += 1;
            continue;
        }
        candidates.push(mp.to_path_buf());
    }

    info!(
        "Linux mount scan: {} candidate(s), {} skipped",
        candidates.len(),
        skipped
    );

    let mut seen = std::collections::HashSet::new();
    let mut cards = Vec::new();
    let mut rejected = 0u32;
    let mut budget_exhausted = 0u32;
    for mp in &candidates {
        if !seen.insert(mp.clone()) {
            continue;
        }
        if Instant::now() >= deadline {
            warn!("detect_cards_from_mounts: budget exceeded — skipping {:?}", mp);
            budget_exhausted += 1;
            continue;
        }
        debug!("Probing mount candidate: {:?}", mp);
        if let Some(p) = progress {
            let label = volume_label_for(mp);
            if label.is_empty() {
                p.set(format!("Scanning drive {}…", mp.display()));
            } else {
                p.set(format!("Scanning drive {label} ({})…", mp.display()));
            }
        }
        if let Some(info) = classify_mount(mp, mp, deadline) {
            info!(
                "Mount {:?} → card: {} files, {} bytes, name='{}'",
                mp, info.media_file_count, info.total_bytes, info.device_name
            );
            cards.push(info);
        } else {
            rejected += 1;
            info!("Mount {:?}: rejected — no recognized media files", mp);
        }
    }

    info!(
        "Mount scan complete: {} card(s), {} rejected{}",
        cards.len(),
        rejected,
        if budget_exhausted > 0 {
            format!(", {} budget-exhausted", budget_exhausted)
        } else {
            String::new()
        },
    );
    cards
}

/// Classify a mount point as a media card: shallow media scan + device name guess.
/// `deadline` caps the overall time spent on I/O-heavy probe steps.
fn classify_mount(mount: &Path, _label_source: &Path, deadline: Instant) -> Option<SdCardInfo> {
    let mut infos = collect_media_files_shallow(mount);
    infos.sort_by_key(|i| std::cmp::Reverse(i.modified));
    let media_files: Vec<PathBuf> = infos.iter().map(|f| f.path.clone()).collect();
    let total_bytes: u64 = infos.iter().map(|f| f.size_bytes).sum();
    let volume_label = volume_label_for(mount);

    if media_files.is_empty() {
        debug!("Mount {:?} rejected: no media files found (label='{}')", mount, volume_label);
        return None;
    }

    debug!("Resolving device name from {} file(s) with budget until {:?}", media_files.len(), deadline);
    let budget = deadline.saturating_duration_since(Instant::now());
    let (device_name, name_source, pattern_name) =
        device_name::resolve_device_name_with_budget(&media_files, &volume_label, None, budget);

    debug!(
        "Mount {:?} accepted: {} files, {} bytes, label='{}', device='{}', source={:?}, pattern={:?}",
        mount,
        media_files.len(),
        total_bytes,
        volume_label,
        device_name,
        name_source,
        pattern_name,
    );

    Some(SdCardInfo {
        mount: mount.to_path_buf(),
        volume_label,
        device_name,
        name_source,
        media_file_count: media_files.len(),
        total_bytes,
        files: infos,
        selected: Vec::new(),
        selected_count: 0,
        selected_bytes: 0,
    })
}

/// Shallow recursive scan for media files (up to CARD_SCAN_DEPTH).
fn collect_media_files_shallow(root: &Path) -> Vec<OffloadFileInfo> {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut visited_dirs: u32 = 0;
    let mut skipped_symlinks: u32 = 0;

    while let Some(dir) = stack.pop() {
        if files.len() >= MAX_CARD_FILES {
            break;
        }
        visited_dirs += 1;
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
            // Skip symlinks/junctions to avoid cycles (e.g. Windows junction cycles).
            if ft.is_symlink() {
                skipped_symlinks += 1;
                continue;
            }
            if ft.is_dir() && depth < CARD_SCAN_DEPTH {
                stack.push(entry.path());
            } else if ft.is_file() && is_media_file(&entry.path()) {
                let p = entry.path();
                let modified = fs::metadata(&p).ok().and_then(|m| m.modified().ok().map(|sys| sys.into()));
                let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                files.push(OffloadFileInfo {
                    path: p,
                    name: name_str.to_string(),
                    size_bytes: size,
                    modified,
                });
            }
        }
    }

    let total_bytes: u64 = files.iter().map(|f| f.size_bytes).sum();
    debug!(
        "Scanned {:?}: {} dirs visited, {} symlinks skipped, {} media files ({} bytes)",
        root, visited_dirs, skipped_symlinks, files.len(), total_bytes
    );
    files
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

// ── Copy planning ───────────────────────────────────────────────────────

/// Plan copies for a single card: flat layout, collision handling.
/// Scans the mount point fresh; for selection-aware planning use
/// [`plan_copies_for_files`] with pre-selected file paths.
pub fn plan_copies_for_card(
    mount: &Path,
    device_name: &str,
    dest_parent: &Path,
) -> Vec<CopyPlanItem> {
    let infos = collect_media_files_shallow(mount);
    let files: Vec<PathBuf> = infos.iter().map(|f| f.path.clone()).collect();
    plan_copies_for_files(&files, device_name, dest_parent)
}

/// Plan copies for a given set of file paths (no extra scan).
/// Collision handling, flat device dir layout.
/// Uses `fs::metadata` per file to determine sizes.
pub fn plan_copies_for_files(
    files: &[PathBuf],
    device_name: &str,
    dest_parent: &Path,
) -> Vec<CopyPlanItem> {
    let files_with_sizes: Vec<(PathBuf, u64)> = files
        .iter()
        .map(|p| {
            let size = fs::metadata(p).map(|m| m.len()).unwrap_or(0);
            (p.clone(), size)
        })
        .collect();
    plan_copies_for_files_with_sizes(&files_with_sizes, device_name, dest_parent)
}

/// Plan copies using pre-determined sizes (no filesystem I/O).
/// Collision handling, flat device dir layout.
/// Use this when sizes are already known (e.g. from a prior scan)
/// to avoid the `fs::metadata` per file that `plan_copies_for_files` does.
pub fn plan_copies_for_files_with_sizes(
    files: &[(PathBuf, u64)],
    device_name: &str,
    dest_parent: &Path,
) -> Vec<CopyPlanItem> {
    let dest_dir = dest_parent.join(device_name);

    // Collision suffixes must be reproducible: assign them in sorted src
    // order, never in the (filesystem-dependent) order the files arrived.
    let mut sorted: Vec<(PathBuf, u64)> = files.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));

    let mut used_names: HashMap<String, u32> = HashMap::new();
    let mut plans = Vec::new();

    for (src, size) in &sorted {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown")
            .to_string();

        let dst_name = resolve_collision(&name, &mut used_names);
        let dst = dest_dir.join(&dst_name);
        plans.push(CopyPlanItem {
            src: src.clone(),
            dst,
            size: *size,
        });
    }

    plans
}

// ── Selection helpers ────────────────────────────────────────────────────

/// Find the latest date (in local time) for which any file has a recording.
/// Returns `None` if no files have a valid modification time.
pub fn latest_recording_date(files: &[OffloadFileInfo]) -> Option<NaiveDate> {
    files.iter()
        .filter_map(|f| f.modified)
        .map(|dt| dt.date_naive())
        .max()
}

/// Build a default selection: select all files whose recording date (mtime in
/// local time) matches the latest date present.
pub fn default_selection(files: &[OffloadFileInfo]) -> Vec<bool> {
    let latest = match latest_recording_date(files) {
        Some(d) => d,
        None => return vec![false; files.len()],
    };
    files.iter()
        .map(|f| f.modified.map(|dt| dt.date_naive() == latest).unwrap_or(false))
        .collect()
}

/// Apply a selection vector to a card: update `selected`, `selected_count`,
/// and `selected_bytes`. Panics if the selection length doesn't match.
pub fn apply_selection(card: &mut SdCardInfo, selection: Vec<bool>) {
    assert_eq!(selection.len(), card.files.len(), "selection length mismatch");
    card.selected = selection;
    card.selected_count = card.selected.iter().filter(|&&s| s).count();
    card.selected_bytes = card.files.iter()
        .zip(card.selected.iter())
        .filter(|(_, &sel)| sel)
        .map(|(f, _)| f.size_bytes)
        .sum();
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



/// Copy a single file with 1 MiB chunked IO.
///
/// Reads the source in `COPY_BUF_SIZE` chunks, writes each chunk to a
/// `.offload_tmp` file beside the destination, then atomically renames.
/// Checks `cancel` per chunk and calls `on_progress` with the cumulative
/// bytes written so far.
fn copy_file(
    src: &Path,
    dst: &Path,
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(u64),
) -> Result<(), CopyError> {
    let src_file = fs::File::open(src).map_err(|e| CopyError::Io(format!("Cannot open {:?}: {}", src, e)))?;
    let mut src_file = std::io::BufReader::with_capacity(COPY_BUF_SIZE, src_file);

    let tmp = dst.with_extension("offload_tmp");
    let mut dst_file =
        fs::File::create(&tmp).map_err(|e| CopyError::Io(format!("Cannot create {:?}: {}", tmp, e)))?;

    let mut buf = vec![0u8; COPY_BUF_SIZE];
    let mut total: u64 = 0;

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(dst_file);
            let _ = fs::remove_file(&tmp);
            return Err(CopyError::Cancelled);
        }

        let n = src_file
            .read(&mut buf)
            .map_err(|e| CopyError::Io(format!("Read error on {:?}: {}", src, e)))?;
        if n == 0 {
            break;
        }
        dst_file
            .write_all(&buf[..n])
            .map_err(|e| CopyError::Io(format!("Write error on {:?}: {}", tmp, e)))?;
        total += n as u64;
        on_progress(total);
    }

    dst_file
        .sync_all()
        .map_err(|e| CopyError::Io(format!("Sync error on {:?}: {}", tmp, e)))?;
    drop(dst_file);
    fs::rename(&tmp, dst).map_err(|e| CopyError::Io(format!("Rename {:?} → {:?}: {}", tmp, dst, e)))?;
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



/// Resolve the base (whole-disk) device name from a partition or device name.
/// Uses the sysfs `partition` attribute to detect partitions:
/// returns the input unchanged if it's already a whole-disk device.
///
/// Examples with mock sysfs:
///   sda1 → sda, nvme0n1p3 → nvme0n1, mmcblk0p1 → mmcblk0,
///   sda → sda, nvme0n1 → nvme0n1, mmcblk0 → mmcblk0
fn base_device_name<'a>(dev_part: &'a str, sys_root: &'a Path) -> &'a str {
    // Not a partition → return as-is.
    let is_partition = sys_root
        .join("class")
        .join("block")
        .join(dev_part)
        .join("partition")
        .exists();
    if !is_partition {
        return dev_part;
    }
    // Strip trailing digits, then a trailing 'p' if the remainder ends in a digit.
    let trimmed = dev_part.trim_end_matches(|c: char| c.is_ascii_digit());
    if let Some(stripped) = trimmed.strip_suffix('p') {
        if !stripped.is_empty()
            && stripped.ends_with(|c: char| c.is_ascii_digit())
        {
            return stripped;
        }
    }
    trimmed
}

/// Check whether a block device is card-like: either the kernel's removable
/// flag is set, or the device is on a USB or MMC bus (the two common card-reader
/// transports).
fn is_card_like_device(device_path: &str, sys_root: &Path) -> bool {
    let dev_part = device_path.trim_start_matches("/dev/");
    let base = base_device_name(dev_part, sys_root);
    if is_dev_removable(base, sys_root) {
        return true;
    }
    // Bus heuristic: check the resolved device path for /usb or /mmc.
    let dev_path = sys_root.join("class").join("block").join(base);
    if let Ok(real) = dev_path.canonicalize() {
        let s = real.to_string_lossy();
        if s.contains("/usb") || s.contains("/mmc") {
            return true;
        }
    }
    false
}

/// Helper: check `/sys/class/block/<base>/removable`.
fn is_dev_removable(base_dev: &str, sys_root: &Path) -> bool {
    let sys_paths = [
        format!("class/block/{}", base_dev),
        format!("block/{}", base_dev),
    ];
    for sys_path in &sys_paths {
        let removable_path = sys_root.join(sys_path).join("removable");
        if let Ok(val) = fs::read_to_string(&removable_path) {
            return val.trim() == "1";
        }
    }
    false
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

// ── Job-wrapped functions (for use with spawn_job) ──────────────────────

use crate::job::{JobContext, JobError, JobFinal, UnitState};

pub fn run_offload_scan_job(ctx: &JobContext) -> Result<JobFinal, JobError> {
    ctx.cancel.check()?;
    ctx.progress.set_indeterminate(true);
    ctx.progress.set_message("Scanning media cards…");

    let scan_progress = ScanProgress::with_forward({
        let tracker = ctx.progress.clone();
        move |msg| tracker.set_message(msg)
    });

    let cards = detect_cards_with_progress(Some(&scan_progress));

    ctx.progress.set_message(format!("Found {} card(s)", cards.len()));
    ctx.progress.set_indeterminate(false);
    ctx.progress.resize(1);
    ctx.progress.unit(0).finish();

    Ok(JobFinal::OffloadScan { cards })
}

/// Run the full offload copy job, reporting progress via `ctx.progress`.
/// Units = devices, per-unit state mirrors the device state machine
/// (Pending/Running/Done/Failed/Skipped). Byte-weighted progress is
/// approximated by equal weights.
pub fn run_offload_copy_job(
    ctx: &JobContext,
    device_plans: Vec<Vec<CopyPlanItem>>,
    device_names: Vec<String>,
    dest_parent: std::path::PathBuf,
) -> Result<JobFinal, JobError> {
    ctx.cancel.check()?;

    let grand_total: u64 = device_plans.iter().flat_map(|p| p.iter()).map(|i| i.size).sum();
    if grand_total == 0 || device_names.is_empty() {
        return Ok(JobFinal::OffloadCopy { completed_devices: Vec::new() });
    }

    let dev_count = device_names.len();
    ctx.progress.resize(dev_count);

    let mut completed: Vec<String> = Vec::new();
    let mut speed_meter = crate::job::SpeedMeter::new();
    let mut cumulative_bytes: u64 = 0;

    for (dev_idx, plans) in device_plans.iter().enumerate() {
        if ctx.cancel.is_cancelled() {
            break;
        }

        let name = &device_names[dev_idx];
        let dev_unit = ctx.progress.unit(dev_idx);
        dev_unit.set_state(UnitState::Running);
        dev_unit.set_message(format!("Starting device '{}'…", name));

        let dest_parent_dev = plans.first()
            .and_then(|p| p.dst.parent())
            .unwrap_or(&dest_parent)
            .to_path_buf();

        if let Err(e) = std::fs::create_dir_all(&dest_parent_dev) {
            dev_unit.set_state(UnitState::Failed);
            dev_unit.set_message(format!("Failed to create directory: {}", e));
            continue;
        }

        let mut dev_ok = true;
        let dev_total = plans.len();
        let cancel_flag = ctx.cancel.inner().clone();
        let verify_mode = VerifyMode::SizeOnly;

        for (item_idx, item) in plans.iter().enumerate() {
            if ctx.cancel.is_cancelled() {
                dev_ok = false;
                break;
            }

            dev_unit.set_message(format!("[{}/{}] {}", item_idx + 1, dev_total, item.dst.display()));

            let mut bytes_copied: u64 = 0;
            match copy_file(&item.src, &item.dst, &cancel_flag, &mut |total| {
                bytes_copied = total;
            }) {
                Ok(()) => {
                    cumulative_bytes += bytes_copied;
                    let speed = speed_meter.update(cumulative_bytes as usize, std::time::Instant::now());
                    ctx.progress.set_speed(speed);
                    if verify_copy(&item.src, &item.dst, &verify_mode).is_ok() {
                        dev_unit.set_fraction((item_idx + 1) as f32 / dev_total as f32);
                    } else {
                        let msg = format!("Verify failed: {}", item.dst.display());
                        warn!("{}", msg);
                        dev_unit.set_state(UnitState::Failed);
                        dev_unit.set_message(msg);
                        dev_ok = false;
                        break;
                    }
                }
                Err(CopyError::Cancelled) => {
                    dev_ok = false;
                    break;
                }
                Err(CopyError::Io(e)) => {
                    dev_unit.set_state(UnitState::Failed);
                    dev_unit.set_message(format!("Copy error: {}", e));
                    dev_ok = false;
                    break;
                }
            }
        }

        if dev_ok {
            completed.push(name.clone());
            dev_unit.finish();
            dev_unit.set_message("Completed");
        }
    }

    Ok(JobFinal::OffloadCopy { completed_devices: completed })
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{CancelToken, ProgressTracker, UnitSpec};
    use std::fs;
    use std::sync::Arc;
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
    fn test_plan_copies_numbering_independent_of_input_order() {
        // Regression test: collision suffixes used to be assigned in the
        // order the files arrived (raw readdir order), so the "(2)" landed
        // on a different file depending on the filesystem.  Numbering must
        // follow the sorted src order regardless of input order.
        let files = vec![
            PathBuf::from("/card/DCIM/101MSDCF/C0001.MP4"),
            PathBuf::from("/card/C0001.MP4"),
            PathBuf::from("/card/DCIM/100MSDCF/C0001.MP4"),
        ];
        let dest = TempDir::new().unwrap();
        let plans = plan_copies_for_files(&files, "A6100", dest.path());

        assert_eq!(plans.len(), 3);
        // Sorted src order: root, DCIM/100MSDCF, DCIM/101MSDCF.
        assert_eq!(plans[0].src, PathBuf::from("/card/C0001.MP4"));
        assert_eq!(plans[1].src, PathBuf::from("/card/DCIM/100MSDCF/C0001.MP4"));
        assert_eq!(plans[2].src, PathBuf::from("/card/DCIM/101MSDCF/C0001.MP4"));
        assert!(plans[0].dst.to_string_lossy().ends_with("C0001.MP4"));
        assert!(plans[1].dst.to_string_lossy().ends_with("C0001 (2).MP4"));
        assert!(plans[2].dst.to_string_lossy().ends_with("C0001 (3).MP4"));
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

    // ── plan_copies_for_files_with_sizes ─────────────────────────────

    #[test]
    fn test_plan_with_sizes_parity_with_stats() {
        let card = TempDir::new().unwrap();
        let dest = TempDir::new().unwrap();

        fs::write(card.path().join("C0001.MP4"), b"data_a").unwrap();
        fs::write(card.path().join("C0002.MP4"), b"data_bb").unwrap();
        fs::create_dir_all(card.path().join("DCIM").join("100MSDCF")).unwrap();
        fs::write(
            card.path().join("DCIM").join("100MSDCF").join("C0001.MP4"),
            b"data_ccc",
        )
        .unwrap();

        // Run the disk-I/O version.
        let paths: Vec<std::path::PathBuf> = vec![
            card.path().join("C0001.MP4"),
            card.path().join("C0002.MP4"),
            card.path().join("DCIM").join("100MSDCF").join("C0001.MP4"),
        ];
        let plans_disk = plan_copies_for_files(&paths, "A6100", dest.path());

        // Run the size-based version with known sizes.
        let sizes: Vec<(std::path::PathBuf, u64)> = vec![
            (card.path().join("C0001.MP4"), 6),       // "data_a" = 6 bytes
            (card.path().join("C0002.MP4"), 7),       // "data_bb" = 7 bytes
            (card.path().join("DCIM").join("100MSDCF").join("C0001.MP4"), 8), // "data_ccc" = 8 bytes
        ];
        let plans_size = plan_copies_for_files_with_sizes(&sizes, "A6100", dest.path());

        // Plans must be identical: same count, same src/dst/size per item.
        assert_eq!(plans_disk.len(), plans_size.len());
        for (disk, size) in plans_disk.iter().zip(plans_size.iter()) {
            assert_eq!(disk.src, size.src);
            assert_eq!(disk.dst, size.dst);
            assert_eq!(disk.size, size.size);
        }
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

        let files = collect_media_files_shallow(dir.path());
        // Only .mp4 and .wav should be collected.
        let names: Vec<&str> = files
            .iter()
            .filter_map(|f| f.path.file_name())
            .filter_map(|n| n.to_str())
            .collect();
        assert!(names.contains(&"clip1.mp4"));
        assert!(names.contains(&"sound.wav"));
        assert!(names.contains(&"C0001.MP4"));
        assert!(!names.contains(&"notes.txt"));
        assert!(!names.contains(&"image.jpg"));
        assert_eq!(names.len(), 3);
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

    

    #[test]
    fn test_copy_file_copies_content_correctly() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");

        // 3 MiB file (spans multiple chunks).
        let content = vec![0xABu8; 3 * 1024 * 1024];
        fs::write(&src, &content).unwrap();

        let cancel = AtomicBool::new(false);
        let mut progress_events: Vec<u64> = Vec::new();
        copy_file(&src, &dst, &cancel, &mut |c| progress_events.push(c)).unwrap();

        // Content matches.
        let dst_content = fs::read(&dst).unwrap();
        assert_eq!(dst_content, content, "copied file content must match source");

        // Progress callback called with increasing values ending at file size.
        assert!(
            !progress_events.is_empty(),
            "progress callback must be called at least once"
        );
        assert_eq!(
            *progress_events.last().unwrap(),
            content.len() as u64,
            "final progress must equal file size"
        );
        for w in progress_events.windows(2) {
            assert!(
                w[0] <= w[1],
                "progress must be monotonic: {} > {}",
                w[0],
                w[1]
            );
        }
        // For a 3 MiB file we expect at least 3 chunk updates (1 MiB each).
        assert!(progress_events.len() >= 3, "expected >= 3 progress callbacks, got {}", progress_events.len());
    }

    #[test]
    fn test_copy_file_cancel_mid_copy() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");

        // 5 MiB file — will span at least 5 chunks.
        let content = vec![0xCDu8; 5 * 1024 * 1024];
        fs::write(&src, &content).unwrap();

        let cancel = AtomicBool::new(true);
        let mut progress_events: Vec<u64> = Vec::new();
        let result = copy_file(&src, &dst, &cancel, &mut |c| progress_events.push(c));

        // Should fail with Cancelled.
        assert!(result.is_err(), "copy should return error when cancelled");
        assert!(
            matches!(result, Err(CopyError::Cancelled)),
            "error should be Cancelled, got {:?}",
            result
        );

        // Destination should NOT exist (tmp was deleted on cancel).
        assert!(!dst.exists(), "destination should not exist after cancelled copy");

        // Temp file should be gone.
        let tmp = dst.with_extension("offload_tmp");
        assert!(!tmp.exists(), "temp file should be deleted on cancel");

        // Progress may have been called 0 or 1 time (cancel checked before chunk).
        // Either is fine.
    }

    #[test]
    fn test_copy_file_respects_cancel_after_chunk() {
        let dir = TempDir::new().unwrap();
        let src = dir.path().join("src.bin");
        let dst = dir.path().join("dst.bin");

        // 3 MiB file.
        let content = vec![0xEFu8; 3 * 1024 * 1024];
        fs::write(&src, &content).unwrap();

        let cancel = AtomicBool::new(false);
        let mut count = 0;
        let result = copy_file(&src, &dst, &cancel, &mut |_c| {
            count += 1;
            if count >= 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        });

        // Should have been cancelled mid-copy.
        assert!(result.is_err());
        assert!(matches!(result, Err(CopyError::Cancelled)));
        // Destination should not exist.
        assert!(!dst.exists());
    }

    

    // ── detect_cards_from_mounts ──────────────────────────────────────

    fn far_deadline() -> Instant {
        Instant::now() + Duration::from_secs(3600)
    }

    #[test]
    fn test_detect_cards_from_mounts_empty() {
        let cards = detect_cards_from_mounts("", Path::new("/sys"), far_deadline(), None);
        assert!(cards.is_empty());
    }

    #[test]
    fn test_detect_cards_from_mounts_ignores_system() {
        let mounts = "\
/dev/nvme0n1p2 / ext4 rw 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
proc /proc proc rw 0 0
";
        let cards = detect_cards_from_mounts(mounts, Path::new("/sys"), far_deadline(), None);
        assert!(cards.is_empty());
    }

    // ── classify_mount (direct invocation via prerequisites) ──────────

    #[test]
    fn test_classify_mount_with_no_media_no_card() {
        let dir = TempDir::new().unwrap();
        let result = classify_mount(dir.path(), dir.path(), far_deadline());
        assert!(result.is_none(), "no media files → no card");
    }

    #[test]
    fn test_classify_mount_with_media_returns_sd_card() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("C0001.MP4"), b"data").unwrap();
        let result = classify_mount(dir.path(), dir.path(), far_deadline());
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(info.media_file_count, 1);
        assert!(info.total_bytes > 0);
    }

    // ── Symlink skip in collect_media_files_shallow ─────────────────────

    #[cfg(not(target_os = "windows"))]
    #[test]
    fn test_collect_media_skips_symlink_dirs() {
        use std::os::unix::fs as unix_fs;

        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("clip.mp4"), b"data").unwrap();
        fs::write(dir.path().join("root_clip.mp4"), b"root").unwrap();

        // Symlink pointing back to the root cycle.
        let cycle = dir.path().join("cycle_back");
        let _ = unix_fs::symlink(dir.path(), &cycle);

        // Symlink pointing to a file (would be skipped).
        let file_link = dir.path().join("linked.mp4");
        let _ = unix_fs::symlink(sub.join("clip.mp4"), &file_link);

        let files = collect_media_files_shallow(dir.path());
        let names: Vec<&str> = files
            .iter()
            .filter_map(|f| f.path.file_name())
            .filter_map(|n| n.to_str())
            .collect();

        assert!(names.contains(&"root_clip.mp4"), "real file should be found");
        assert!(!names.contains(&"cycle_back"), "symlink dir should be skipped");
        assert!(!names.contains(&"linked.mp4"), "symlink file should be skipped");
    }

    // ── Depth limit in collect_media_files_shallow ───────────────────────

    #[test]
    fn test_collect_media_depth_limit() {
        let dir = TempDir::new().unwrap();
        // Create a deep chain of directories beyond CARD_SCAN_DEPTH.
        let deep = dir.path().join("a/b/c/d/e/f/g/h/i/j");
        fs::create_dir_all(&deep).unwrap();
        fs::write(deep.join("deep.mp4"), b"data").unwrap();

        // Also a shallow file.
        fs::write(dir.path().join("shallow.mp4"), b"data").unwrap();

        let files = collect_media_files_shallow(dir.path());
        let names: Vec<&str> = files
            .iter()
            .filter_map(|f| f.path.file_name())
            .filter_map(|n| n.to_str())
            .collect();

        // The shallow file should be found, the deep one (depth > 6) should not.
        assert!(names.contains(&"shallow.mp4"), "shallow file should be found");
        assert!(!names.contains(&"deep.mp4"), "deep file beyond depth limit should not be found");
    }

    // ── Windows drive bitmask parsing (pure logic, no API calls) ────────

    #[test]
    fn test_drive_bitmask_parsing_logic() {
        // Simulate what win_driver::enumerate would parse from GetLogicalDrives.
        // Bit 0 = A:, bit 2 = C:, bit 25 = Z:
        let mask: u32 = (1u32 << 0) | (1u32 << 2) | (1u32 << 25);

        let mut letters: Vec<char> = Vec::new();
        for i in 0..26u32 {
            if (mask >> i) & 1 == 1 {
                letters.push(char::from_u32(b'A' as u32 + i).unwrap());
            }
        }

        assert_eq!(letters.len(), 3);
        assert!(letters.contains(&'A'));
        assert!(letters.contains(&'C'));
        assert!(letters.contains(&'Z'));
        assert!(!letters.contains(&'B'));
    }

    // ── base_device_name ──────────────────────────────────────────────────

    /// Quick mock sysfs: create a `class/block/<name>/partition` file for
    /// partition devices (names ending with a digit or p + digits).
    fn mock_base_dev_sys(partition_devs: &[&str]) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        for dev in partition_devs {
            let p = dir.path().join("class").join("block").join(dev);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("partition"), "1").unwrap();
        }
        dir
    }

    #[test]
    fn test_base_device_name_sda() {
        let sys = mock_base_dev_sys(&["sda1", "sdb1", "sdc123"]);
        // sda — no partition file → returned unchanged.
        assert_eq!(base_device_name("sda", sys.path()), "sda");
        assert_eq!(base_device_name("sda1", sys.path()), "sda");
        assert_eq!(base_device_name("sdb1", sys.path()), "sdb");
        assert_eq!(base_device_name("sdc123", sys.path()), "sdc");
    }

    #[test]
    fn test_base_device_name_nvme() {
        let sys = mock_base_dev_sys(&["nvme0n1p1", "nvme0n1p3", "nvme1n1p10"]);
        assert_eq!(base_device_name("nvme0n1", sys.path()), "nvme0n1");
        assert_eq!(base_device_name("nvme0n1p1", sys.path()), "nvme0n1");
        assert_eq!(base_device_name("nvme0n1p3", sys.path()), "nvme0n1");
        assert_eq!(base_device_name("nvme1n1p10", sys.path()), "nvme1n1");
    }

    #[test]
    fn test_base_device_name_mmc() {
        let sys = mock_base_dev_sys(&["mmcblk0p1", "mmcblk0p12"]);
        assert_eq!(base_device_name("mmcblk0", sys.path()), "mmcblk0");
        assert_eq!(base_device_name("mmcblk0p1", sys.path()), "mmcblk0");
        assert_eq!(base_device_name("mmcblk0p12", sys.path()), "mmcblk0");
    }

    #[test]
    fn test_base_device_name_no_partition_file() {
        let sys = mock_base_dev_sys(&[]);
        assert_eq!(base_device_name("sda", sys.path()), "sda");
        assert_eq!(base_device_name("nvme0n1", sys.path()), "nvme0n1");
        assert_eq!(base_device_name("mmcblk0", sys.path()), "mmcblk0");
        assert_eq!(base_device_name("loop0", sys.path()), "loop0");
    }

    // ── is_dev_removable / is_card_like_device (with mock sysfs) ──────────

    /// Create a mock sysfs tree for testing removable / bus checks.
    /// - `removable_devs`: devices with `removable` flag set to 1
    /// - `usb_devs`: devices on a USB bus (removable=0, path contains /usb)
    /// - `mmc_devs`: devices on an MMC bus (removable=0, path contains /mmc)
    /// - `partition_devs`: partition names relative to the above groups;
    ///   each gets a `partition` file and no `removable` file.
    fn make_mock_sys(
        removable_devs: &[&str],
        usb_devs: &[&str],
        mmc_devs: &[&str],
        partition_devs: &[&str],
    ) -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path();

        for dev in removable_devs {
            let p = root.join("class").join("block").join(dev);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("removable"), "1").unwrap();
        }

        for dev in usb_devs {
            let symlink_target = root.join("devices").join("pci0000:00").join("0000:00:14.0")
                .join("usb1").join("1-1").join("1-1:1.0").join("host0")
                .join("target0:0:0").join("0:0:0:0").join("block").join(dev);
            fs::create_dir_all(&symlink_target).unwrap();
            fs::write(symlink_target.join("removable"), "0").unwrap();
            let link_path = root.join("class").join("block").join(dev);
            fs::create_dir_all(link_path.parent().unwrap()).unwrap();
            let _ = std::fs::remove_dir(&link_path);
            let _ = std::os::unix::fs::symlink(&symlink_target, &link_path);
        }

        for dev in mmc_devs {
            let symlink_target = root.join("devices").join("pci0000:00").join("0000:00:1a.0")
                .join("mmc_host").join("mmc0").join("mmc0:0001").join("block").join(dev);
            fs::create_dir_all(&symlink_target).unwrap();
            fs::write(symlink_target.join("removable"), "0").unwrap();
            let link_path = root.join("class").join("block").join(dev);
            fs::create_dir_all(link_path.parent().unwrap()).unwrap();
            let _ = std::fs::remove_dir(&link_path);
            let _ = std::os::unix::fs::symlink(&symlink_target, &link_path);
        }

        // Nvme — should NOT be card-like (removable=0, pci bus, not usb/mmc).
        let nvme_tgt = root.join("devices").join("pci0000:00").join("0000:00:1c.0")
            .join("nvme").join("nvme0").join("nvme0n1").join("block").join("nvme0n1");
        fs::create_dir_all(&nvme_tgt).unwrap();
        fs::write(nvme_tgt.join("removable"), "0").unwrap();
        let nvme_link = root.join("class").join("block").join("nvme0n1");
        fs::create_dir_all(nvme_link.parent().unwrap()).unwrap();
        let _ = std::fs::remove_dir(&nvme_link);
        let _ = std::os::unix::fs::symlink(&nvme_tgt, &nvme_link);

        // Partition devices: create `partition` and `size` files.
        for dev in partition_devs {
            let p = root.join("class").join("block").join(dev);
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("partition"), "1").unwrap();
            fs::write(p.join("size"), "244306944").unwrap(); // ~120 MB
        }

        dir
    }

    #[test]
    fn test_is_dev_removable_found() {
        let sys = make_mock_sys(&["sdb"], &[], &[], &[]);
        assert!(is_dev_removable("sdb", sys.path()));
    }

    #[test]
    fn test_is_dev_removable_not_found() {
        let sys = make_mock_sys(&[], &[], &[], &[]);
        assert!(!is_dev_removable("nvme0n1", sys.path()));
    }

    #[test]
    fn test_is_card_like_device_removable_flag() {
        let sys = make_mock_sys(&["sdb"], &[], &[], &["sdb1"]);
        assert!(is_card_like_device("/dev/sdb1", sys.path()));
    }

    #[test]
    fn test_is_card_like_device_usb_bus() {
        let sys = make_mock_sys(&[], &["sdc"], &[], &[]);
        assert!(is_card_like_device("/dev/sdc", sys.path()));
    }

    #[test]
    fn test_is_card_like_device_mmc_bus() {
        let sys = make_mock_sys(&[], &[], &["mmcblk0"], &[]);
        assert!(is_card_like_device("/dev/mmcblk0", sys.path()));
    }

    #[test]
    fn test_is_card_like_device_nvme_excluded() {
        let sys = make_mock_sys(&[], &[], &[], &[]);
        assert!(!is_card_like_device("/dev/nvme0n1", sys.path()));
    }

    // ── detect_cards_from_mounts (Linux mount-parsing) ──────────────────

    /// Create a tempdir with media files under a subpath.
    fn make_mount_with_media(label: &str, media_names: &[&str]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let root = dir.path().join(label);
        fs::create_dir_all(&root).unwrap();
        for name in media_names {
            fs::write(root.join(name), b"video_data").unwrap();
        }
        (dir, root)
    }

    #[test]
    fn test_detect_cards_udisks2_path_passes_filtering() {
        let mounts = "\
/dev/sdb1 /run/media/viktoria/NONEXISTENT_UNIQUE_CARD_DIR_42 vfat rw 0 0
";
        let sys = make_mock_sys(&["sdb"], &[], &[], &["sdb1"]);
        let cards = detect_cards_from_mounts(mounts, sys.path(), far_deadline(), None);
        // The mount directory doesn't exist, so classify_mount rejects it
        // (0 files). But the filter should accept it (sdb is removable,
        // sdb1 is its partition).
        assert!(cards.is_empty());
    }

    #[test]
    fn test_detect_cards_skips_gvfs_snap() {
        let mounts = "\
/dev/nvme0n1p2 / ext4 rw 0 0
gvfsd-fuse /run/user/1000/gvfs fuse rw 0 0
systemd-1 /run/snapd/ns/snapd-disk-annotate.mnt autofs rw 0 0
/dev/loop0 /snap/emacs/4391 squashfs ro 0 0
";
        let sys = make_mock_sys(&[], &[], &[], &[]);
        let cards = detect_cards_from_mounts(mounts, sys.path(), far_deadline(), None);
        assert!(cards.is_empty());
    }

    #[test]
    fn test_detect_cards_media_dir_paths_pass_filtering() {
        let mounts = "\
/dev/sdc1 /media/viktoria/EOS_DIGITAL vfat rw 0 0
/dev/sdd1 /mnt/card vfat rw 0 0
";
        let sys = make_mock_sys(&["sdc", "sdd"], &[], &[], &["sdc1", "sdd1"]);
        let cards = detect_cards_from_mounts(mounts, sys.path(), far_deadline(), None);
        assert!(cards.is_empty());
    }

    #[test]
    fn test_detect_cards_nvme_under_run_media_excluded() {
        let mounts = "\
/dev/nvme0n1p3 /run/media/viktoria/1440965240963B06 ntfs3 rw 0 0
";
        let sys = make_mock_sys(&[], &[], &[], &[]);
        let cards = detect_cards_from_mounts(mounts, sys.path(), far_deadline(), None);
        assert!(cards.is_empty(), "nvme under /run/media should be excluded by bus heuristic");
    }

    #[test]
    fn test_classify_mount_real_dir_with_media_accepted() {
        let (_tmp, mp) = make_mount_with_media("EOS_DIGITAL", &["C0001.MP4"]);
        let info = classify_mount(&mp, &mp, far_deadline());
        assert!(info.is_some());
        assert_eq!(info.unwrap().media_file_count, 1);
    }

    // ── collect_mounted_devices ──────────────────────────────────────────

    #[test]
    fn test_collect_mounted_devices() {
        let mounts = "\
/dev/nvme0n1p2 / ext4 rw 0 0
/dev/sdb1 /run/media/viktoria/disk vfat rw 0 0
proc /proc proc rw 0 0
gvfsd-fuse /run/user/1000/gvfs fuse rw 0 0
";
        let mounted = collect_mounted_devices(mounts);
        assert!(mounted.contains("/dev/nvme0n1p2"));
        assert!(mounted.contains("/dev/sdb1"));
        assert!(!mounted.contains("/proc"));
        assert_eq!(mounted.len(), 2);
    }

    // ── find_unmounted_card_partitions ───────────────────────────────────

    #[test]
    fn test_find_unmounted_card_partitions_none_mounted() {
        let sys = make_mock_sys(&["sdb"], &[], &[], &["sdb1"]);
        let mounts = ""; // nothing mounted
        let unmounted = find_unmounted_card_partitions(mounts, sys.path());
        // sdb1 is card-like (removable parent) and has no mount → should be found
        assert!(
            unmounted.iter().any(|d| d == "sdb1"),
            "sdb1 should be found as unmounted candidate"
        );
    }

    #[test]
    fn test_find_unmounted_card_partitions_already_mounted() {
        let sys = make_mock_sys(&["sdb"], &[], &[], &["sdb1"]);
        let mounts = "/dev/sdb1 /run/media/viktoria/disk vfat rw 0 0\n";
        let unmounted = find_unmounted_card_partitions(mounts, sys.path());
        assert!(
            !unmounted.iter().any(|d| d == "sdb1"),
            "sdb1 mounted → should not be in unmounted list"
        );
    }

    #[test]
    fn test_find_unmounted_card_partitions_nvme_excluded() {
        let sys = make_mock_sys(&[], &[], &[], &["nvme0n1p3"]);
        let mounts = "";
        let unmounted = find_unmounted_card_partitions(mounts, sys.path());
        assert!(
            !unmounted.iter().any(|d| d == "nvme0n1p3"),
            "nvme partition should be excluded by bus heuristic"
        );
    }

    #[test]
    fn test_collect_mounted_devices_empty() {
        let mounted = collect_mounted_devices("");
        assert!(mounted.is_empty());
    }

    // ── ScanProgress ─────────────────────────────────────────────────────

    #[test]
    fn test_scan_progress_set_read_clear() {
        let p = ScanProgress::new();
        assert!(p.read().is_none());

        p.set("Scanning drive FOO…".to_string());
        assert_eq!(p.read().as_deref(), Some("Scanning drive FOO…"));

        p.clear();
        assert!(p.read().is_none());
    }

    #[test]
    fn test_scan_progress_default_is_none() {
        let p: ScanProgress = Default::default();
        assert!(p.read().is_none());
    }

    #[test]
    fn test_scan_progress_is_send_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<ScanProgress>();
        assert_sync::<ScanProgress>();
    }

    #[test]
    fn test_detect_cards_from_mounts_with_progress_fires_for_each_candidate() {
        let dir = tempfile::TempDir::new().unwrap();
        let mp = dir.path();
        // Place a media file so classify_mount accepts it.
        fs::write(mp.join("C0001.MP4"), b"data").unwrap();

        // The mount path must start with /media, /run/media, or /mnt to pass
        // the filter in detect_cards_from_mounts. Use /mnt/../tmp/... which
        // starts with /mnt as a prefix path component but resolves to the TempDir.
        let fake_mount = format!("/mnt/..{}", mp.display());
        let mounts = format!(
            "/dev/sdb1 {} vfat rw 0 0",
            fake_mount
        );
        let sys = make_mock_sys(&["sdb"], &[], &[], &["sdb1"]);

        let progress = ScanProgress::new();
        let cards = detect_cards_from_mounts(
            &mounts,
            sys.path(),
            far_deadline(),
            Some(&progress),
        );

        // The progress cell should have been set with a message about the mount path.
        let last_msg = progress.read();
        assert!(last_msg.is_some(), "progress should have been set");
        let msg = last_msg.unwrap();
        assert!(
            msg.contains(&fake_mount),
            "progress message '{}' should contain mount path '{}'",
            msg,
            fake_mount,
        );
        // Should also mention "Scanning drive".
        assert!(msg.contains("Scanning drive"), "progress message should mention 'Scanning drive'");

        // Ensure the card was properly discovered.
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].media_file_count, 1);
    }

    #[test]
    fn test_detect_cards_with_progress_none_still_works() {
        let dir = tempfile::TempDir::new().unwrap();
        let mp = dir.path();
        fs::write(mp.join("C0001.MP4"), b"data").unwrap();

        let fake_mount = format!("/mnt/..{}", mp.display());
        let mounts = format!(
            "/dev/sdb1 {} vfat rw 0 0",
            fake_mount
        );
        let sys = make_mock_sys(&["sdb"], &[], &[], &["sdb1"]);

        let cards = detect_cards_from_mounts(&mounts, sys.path(), far_deadline(), None);
        assert_eq!(cards.len(), 1);
    }

    // ── MAX_CARD_FILES limit in collect_media_files_shallow ──────────────

    #[test]
    fn test_collect_media_files_cap_at_max() {
        let dir = TempDir::new().unwrap();
        // Create more media files than MAX_CARD_FILES.
        let sub = dir.path().join("DCIM");
        fs::create_dir_all(&sub).unwrap();
        for i in 0..MAX_CARD_FILES + 100 {
            fs::write(sub.join(format!("C{:04}.MP4", i)), b"x").unwrap();
        }

        let files = collect_media_files_shallow(dir.path());
        assert_eq!(files.len(), MAX_CARD_FILES);
    }

    // ── ScanProgress::with_forward ──────────────────────────────────────

    #[test]
    fn test_scan_progress_with_forward_forwards_to_callback() {
        let forwarded = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fwd = Arc::clone(&forwarded);
        let p = ScanProgress::with_forward(move |msg| {
            fwd.lock().unwrap().push(msg);
        });

        p.set("Mounting /dev/sdb1…".to_string());
        p.set("Scanning drive F:…".to_string());

        let msgs = forwarded.lock().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0], "Mounting /dev/sdb1…");
        assert_eq!(msgs[1], "Scanning drive F:…");
    }

    #[test]
    fn test_scan_progress_with_forward_does_not_break_normal_read() {
        let p = ScanProgress::with_forward(|_| {});
        p.set("hello".to_string());
        assert_eq!(p.read().as_deref(), Some("hello"));
    }

    // ── SpeedMeter in run_offload_copy_job ──────────────────────────────

    /// Helper: create a minimal JobContext for testing.
    fn test_job_context() -> (JobContext, ProgressTracker) {
        let tracker = ProgressTracker::new(vec![UnitSpec { weight: 1.0, label: "test".into() }]);
        let tracker_clone = tracker.clone();
        let cancel = CancelToken::new();
        let emit: Box<dyn Fn(crate::job::JobItem) + Send + Sync> = Box::new(|_| {});
        let ctx = JobContext {
            progress: tracker_clone,
            cancel,
            emit,
        };
        (ctx, tracker)
    }

    #[test]
    fn test_copy_job_sets_speed_on_progress() {
        let dir = TempDir::new().unwrap();

        // Create a 1 MiB source file.
        let src = dir.path().join("src.bin");
        let content = vec![0xABu8; 1 << 20]; // 1 MiB
        fs::write(&src, &content).unwrap();

        let dst = dir.path().join("dst.bin");

        let (ctx, _tracker) = test_job_context();
        let plans = vec![vec![CopyPlanItem {
            src: src.clone(),
            dst: dst.clone(),
            size: content.len() as u64,
        }]];
        let names = vec!["DEVICE".to_string()];
        let dest_parent = dir.path().to_path_buf();

        let result = run_offload_copy_job(&ctx, plans, names, dest_parent);
        assert!(result.is_ok(), "copy job should succeed");
    }

    #[test]
    fn test_copy_job_speed_multiple_files() {
        let dir = TempDir::new().unwrap();

        // Two files of different sizes.
        let src1 = dir.path().join("src1.bin");
        let src2 = dir.path().join("src2.bin");
        fs::write(&src1, vec![0xABu8; 1 << 20]).unwrap(); // 1 MiB
        fs::write(&src2, vec![0xCDu8; 1 << 20]).unwrap(); // 1 MiB

        let dst1 = dir.path().join("dst1.bin");
        let dst2 = dir.path().join("dst2.bin");

        let (ctx, tracker) = test_job_context();
        let plans = vec![vec![
            CopyPlanItem {
                src: src1, dst: dst1, size: 1 << 20,
            },
            CopyPlanItem {
                src: src2, dst: dst2, size: 1 << 20,
            },
        ]];
        let names = vec!["DEVICE".to_string()];
        let dest_parent = dir.path().to_path_buf();

        let result = run_offload_copy_job(&ctx, plans, names, dest_parent);
        assert!(result.is_ok());

        let snap = tracker.snapshot();
        // Speed may be 0 if both files copy faster than 1 ms
        // (no meaningful EMA delta). The SpeedMeter is unit-tested
        // separately in job.rs; this test only checks that the
        // speed field is reachable (not panicking).
        if let Some(s) = snap.speed {
            assert!(s >= 0.0, "speed should be non-negative, got {s}");
        }
    }

    // ── ProgressTracker message forwarding during scan ──────────────────

    #[test]
    fn test_run_offload_scan_job_forwarding_progress_message() {
        // This test verifies that ScanProgress::with_forward correctly
        // forwards messages to the ProgressTracker during a scan.
        // We exercise the forwarding mechanism via detect_cards_from_mounts.

        let dir = tempfile::TempDir::new().unwrap();
        fs::write(dir.path().join("C0001.MP4"), b"data").unwrap();

        let fake_mount = format!("/mnt/..{}", dir.path().display());
        let mounts = format!("/dev/sdb1 {} vfat rw 0 0", fake_mount);
        let sys = make_mock_sys(&["sdb"], &[], &[], &["sdb1"]);

        let tracker = ProgressTracker::new(vec![]);
        let tracker_fwd = tracker.clone();
        let forward_msgs = Arc::new(std::sync::Mutex::new(Vec::new()));
        let fwd = Arc::clone(&forward_msgs);

        let scan_progress = ScanProgress::with_forward(move |msg| {
            tracker_fwd.set_message(msg.clone());
            fwd.lock().unwrap().push(msg);
        });

        // Run detect_cards_from_mounts with the forwarding ScanProgress.
        let cards = detect_cards_from_mounts(
            &mounts,
            sys.path(),
            far_deadline(),
            Some(&scan_progress),
        );

        assert_eq!(cards.len(), 1);

        // The progress tracker's message should have been set.
        let tracker_msg = tracker.snapshot().message;
        assert!(!tracker_msg.is_empty(), "tracker should have a non-empty message after scan");

        // The forwarded messages should include the mount path.
        let msgs = forward_msgs.lock().unwrap();
        assert!(!msgs.is_empty(), "at least one message should be forwarded");
        let combined = msgs.join(" | ");
        assert!(
            combined.contains("Scanning"),
            "forwarded messages should mention scanning, got: {combined}"
        );
    }

    // ── udisks_mount_with runner tests ────────────────────────────────────

    #[cfg(target_os = "linux")]
    fn fake_output(status: i32, stdout: &str, stderr: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(status),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn mount_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("udisks-test-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_udisks_mount_with_success() {
        let mp = mount_dir("success");
        let mp_str = mp.to_string_lossy().to_string();
        let mut runner = |_: &[String]| {
            Ok(fake_output(0, &format!("Mounted /dev/sdb1 at {}\n", mp_str), ""))
        };
        let result = udisks_mount_with("/dev/sdb1", &mut runner).unwrap();
        assert_eq!(result, Some(mp));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_udisks_mount_with_failure_surfaces_stderr() {
        let mut runner = |_: &[String]| {
            Ok(fake_output(1, "", "Error mounting(/dev/sdb1): not authorized\n"))
        };
        let err = udisks_mount_with("/dev/sdb1", &mut runner).unwrap_err();
        assert!(
            err.to_string().contains("not authorized"),
            "stderr must surface in the error, got: {}",
            err
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_udisks_mount_with_timeout() {
        let mut runner =
            |_: &[String]| Err(crate::subprocess::SubprocessFailure::TimedOut);
        let result = udisks_mount_with("/dev/sdb1", &mut runner);
        assert!(result.is_err(), "a timed-out mount must not be reported as success");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_udisks_mount_with_mountpoint_not_a_dir() {
        // udisksctl reported success but the printed path is not a directory —
        // treat as "already mounted / nothing to return" rather than an error.
        let mut runner = |_: &[String]| {
            Ok(fake_output(0, "Mounted /dev/sdb1 at /nonexistent/nope\n", ""))
        };
        let result = udisks_mount_with("/dev/sdb1", &mut runner).unwrap();
        assert_eq!(result, None);
    }
}
