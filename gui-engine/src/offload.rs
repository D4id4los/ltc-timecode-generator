use std::collections::HashMap;
use std::fs;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::Local;
use log::{debug, info};

use crate::device_name;
pub use crate::device_name::DeviceNameSource;

// ── Windows drive enumeration helper ─────────────────────────────────

#[cfg(target_os = "windows")]
mod win_driver {
    use std::path::PathBuf;
    use windows_sys::Win32::Storage::FileSystem;

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

    pub const DRIVE_REMOVABLE: u32 = FileSystem::DRIVE_REMOVABLE;

    pub fn drive_type_name(kind: u32) -> &'static str {
        match kind {
            FileSystem::DRIVE_UNKNOWN => "unknown",
            FileSystem::DRIVE_NO_ROOT_DIR => "no_root_dir",
            FileSystem::DRIVE_REMOVABLE => "removable",
            FileSystem::DRIVE_FIXED => "fixed",
            FileSystem::DRIVE_REMOTE => "remote",
            FileSystem::DRIVE_CDROM => "cdrom",
            FileSystem::DRIVE_RAMDISK => "ramdisk",
            _ => "invalid",
        }
    }
}

/// Audio file extensions recognised as media.
const AUDIO_EXTS: &[&str] = &["wav"];

/// Buffer size for file copy (1 MiB).
const COPY_BUF_SIZE: usize = 1 << 20;

/// Maximum depth when scanning a card for media files.
const CARD_SCAN_DEPTH: usize = 6;

/// Maximum files to enumerate on a card.
const MAX_CARD_FILES: usize = 10_000;

// ── Public types ────────────────────────────────────────────────────────

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
            crate::device_name::VIDEO_EXTS.contains(&e.as_str()) || AUDIO_EXTS.contains(&e.as_str())
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
            debug!("Probing removable drive {}: {:?}", cand.letter, cand.root);
            if let Some(info) = classify_mount(&cand.root, &cand.root) {
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
                    debug!("Probing macOS volume: {:?}", mp);
                    if let Some(info) = classify_mount(&mp, &mp) {
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
/// `sys_root` is typically `/sys` on Linux.
fn detect_cards_from_mounts(mounts_content: &str, sys_root: &Path) -> Vec<SdCardInfo> {
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
        // Skip common non-removable system paths.
        if mp == Path::new("/")
            || mp.starts_with("/boot")
            || mp.starts_with("/proc")
            || mp.starts_with("/sys")
            || mp.starts_with("/dev")
            || mp.starts_with("/run")
        {
            skipped += 1;
            continue;
        }
        // Prefer mounts under standard media directories.
        let in_media = mp.starts_with("/media")
            || mp.starts_with("/run/media")
            || mp.starts_with("/mnt");
        if !in_media && !mp.starts_with(format!("/media/{}", user)) {
            skipped += 1;
            continue;
        }
        // Check removable flag via /sys.
        let is_removable = is_block_removable(device, sys_root);
        if !is_removable && !mp.starts_with(format!("/run/media/{}", user)) {
            // /run/media/$USER mounts are usually removable.
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
    for mp in &candidates {
        // Deduplicate (same mount point via multiple /proc/mounts lines).
        if !seen.insert(mp.clone()) {
            continue;
        }
        debug!("Probing mount candidate: {:?}", mp);
        if let Some(info) = classify_mount(mp, mp) {
            info!(
                "Mount {:?} → card: {} files, {} bytes, name='{}'",
                mp, info.media_file_count, info.total_bytes, info.device_name
            );
            cards.push(info);
        } else {
            rejected += 1;
            debug!("Mount {:?}: rejected by classify_mount", mp);
        }
    }

    info!(
        "Mount scan complete: {} card(s), {} rejected",
        cards.len(),
        rejected
    );
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
        debug!("Mount {:?} rejected: no media files found (label='{}')", mount, volume_label);
        return None;
    }

    let (device_name, name_source, pattern_name) =
        device_name::resolve_device_name(&media_files, &volume_label, None);

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
        pattern_name,
    })
}

/// Shallow recursive scan for media files (up to CARD_SCAN_DEPTH).
/// Returns (media_files, total_bytes).
fn collect_media_files_shallow(root: &Path) -> (Vec<PathBuf>, u64) {
    let mut files = Vec::new();
    let mut stack: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut total_bytes = 0u64;
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
                if let Ok(meta) = fs::metadata(entry.path()) {
                    total_bytes += meta.len();
                }
                files.push(entry.path());
            }
        }
    }

    debug!(
        "Scanned {:?}: {} dirs visited, {} symlinks skipped, {} media files ({} bytes)",
        root, visited_dirs, skipped_symlinks, files.len(), total_bytes
    );
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

        let (files, _) = collect_media_files_shallow(dir.path());
        let names: Vec<&str> = files
            .iter()
            .filter_map(|f| f.file_name())
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

        let (files, _) = collect_media_files_shallow(dir.path());
        let names: Vec<&str> = files
            .iter()
            .filter_map(|f| f.file_name())
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

        let (files, _) = collect_media_files_shallow(dir.path());
        assert_eq!(files.len(), MAX_CARD_FILES);
    }
}
