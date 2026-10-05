//! LTC corpus sweep harness (WP-RW / RW1).
//!
//! Decodes every recording in the local real-world corpus (default root from
//! `LTC_YT_TESTS_DIR`) and reports per-file results plus a per-day
//! cross-device timeline-consistency check (ground-truth-free oracle).
//! Optionally emits JSON for diffing across decoder versions.
//!
//! This is a *measurement* tool: exit 0 on a completed sweep regardless of
//! decode outcomes; non-zero only on operational errors (missing root, …).
//! Pass/fail assertions live in the committed fixture tests, not here.
//!
//! Usage:
//! ```text
//! cargo run --release -p gui-engine --example ltc-corpus-sweep -- <corpus-root> \
//!     [--json <out-path>] [--decoder builtin|libltc] [--limit N] [--day YYYY-MM-DD]
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use gui_engine::LtcDecodeStatus;
use gui_engine::decode::{decode_video_file, decode_wav_core, WavDecodeParams};

const FPS: f64 = 25.0;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum FileClass {
    /// Expected to carry LTC; the sweep asserts decode success + timeline.
    LtcExpected,
    /// Expected to have no LTC; the assertion is on the non-Success status.
    NoLtcExpected,
    /// Derived converter output or unclassifiable — report-only.
    Derived,
}

impl FileClass {
    fn label(self) -> &'static str {
        match self {
            FileClass::LtcExpected => "LTC",
            FileClass::NoLtcExpected => "NO-LTC",
            FileClass::Derived => "DERIVED",
        }
    }
}

struct SweepFile {
    path: PathBuf,
    class: FileClass,
}

/// One decoded file's outcome, as reported and serialised.
struct FileOutcome {
    path: PathBuf,
    class: FileClass,
    /// `Success` / `NoSyncWord` / `LowConfidence` / `Error` / `Empty` / `Failed`
    status: String,
    valid_frames: u32,
    total_possible_frames: u32,
    confidence: f32,
    first_tc: Option<String>,
    last_tc: Option<String>,
    first_tc_at_secs: f64,
    audio_duration_secs: f64,
    processing_ms: f64,
    /// Absolute frame numbers of first/last decoded timecode (25 fps grid).
    first_frame_no: Option<u64>,
    last_frame_no: Option<u64>,
}

struct DayVerdict {
    day: String,
    ltc_files: usize,
    consistent: bool,
    violations: Vec<String>,
    /// Pairwise overlaps between LTC-bearing files' TC ranges, in minutes.
    overlaps: Vec<(String, String, f64)>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root: Option<String> = None;
    let mut json_out: Option<PathBuf> = None;
    let mut use_libltc = false;
    let mut limit: Option<usize> = None;
    let mut day_filter: Option<String> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => match it.next() {
                Some(p) => json_out = Some(PathBuf::from(p)),
                None => return usage_error("--json requires a path"),
            },
            "--decoder" => match it.next().map(|s| s.as_str()) {
                Some("builtin") => use_libltc = false,
                Some("libltc") => use_libltc = true,
                _ => return usage_error("--decoder expects builtin|libltc"),
            },
            "--limit" => match it.next().and_then(|v| v.parse().ok()) {
                Some(n) => limit = Some(n),
                None => return usage_error("--limit expects a number"),
            },
            "--day" => match it.next() {
                Some(d) => day_filter = Some(d.clone()),
                None => return usage_error("--day expects YYYY-MM-DD"),
            },
            other if root.is_none() && !other.starts_with("--") => root = Some(other.to_string()),
            other => return usage_error(&format!("unknown argument: {other}")),
        }
    }

    let root = match root.or_else(|| std::env::var("LTC_YT_TESTS_DIR").ok()) {
        Some(r) => PathBuf::from(r),
        None => {
            eprintln!(
                "error: no corpus root given.\n  pass <corpus-root> as the first argument, or\n  set the LTC_YT_TESTS_DIR environment variable"
            );
            return ExitCode::from(2);
        }
    };
    if !root.is_dir() {
        eprintln!("error: corpus root is not a directory: {}", root.display());
        return ExitCode::from(2);
    }

    let files = match collect_files(&root, day_filter.as_deref()) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: cannot walk corpus root: {e}");
            return ExitCode::from(2);
        }
    };
    let files: Vec<_> = match limit {
        Some(n) => files.into_iter().take(n).collect(),
        None => files,
    };
    println!(
        "corpus sweep: {} files under {} (decoder: {}, fps {})",
        files.len(),
        root.display(),
        if use_libltc { "libltc" } else { "builtin" },
        FPS
    );

    let mut outcomes = Vec::new();
    for f in &files {
        let start = Instant::now();
        print!("  {:<60} [{:?}] ", f.path.display(), f.class);
        let outcome = decode_one(&f.path, f.class, use_libltc, start);
        println!("{}", outcome.status);
        outcomes.push(outcome);
    }

    let verdicts = evaluate_days(&outcomes);

    println!("\n=== Per-day consistency oracle ===");
    for v in &verdicts {
        println!(
            "day {}: {} LTC file(s), consistent: {}",
            v.day, v.ltc_files, v.consistent
        );
        for viol in &v.violations {
            println!("  VIOLATION: {viol}");
        }
        for (a, b, mins) in &v.overlaps {
            println!("  overlap: {a} × {b} = {mins:.1} min");
        }
    }

    println!("\n=== Summary (class × status) ===");
    let mut counts: BTreeMap<(FileClass, String), usize> = BTreeMap::new();
    for o in &outcomes {
        *counts.entry((o.class, o.status.clone())).or_default() += 1;
    }
    for ((class, status), n) in &counts {
        println!("  {:<9} {:<14} {}", class.label(), status, n);
    }

    if let Some(json_path) = json_out {
        match write_json(&json_path, &outcomes, &verdicts, use_libltc) {
            Ok(()) => println!("\nJSON written to {}", json_path.display()),
            Err(e) => {
                eprintln!("error: cannot write JSON: {e}");
                return ExitCode::from(2);
            }
        }
    }

    ExitCode::SUCCESS
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!(
        "error: {msg}\n\nusage: ltc-corpus-sweep <corpus-root> [--json <out-path>] \
         [--decoder builtin|libltc] [--limit N] [--day YYYY-MM-DD]\n\
         corpus-root defaults to $LTC_YT_TESTS_DIR"
    );
    ExitCode::from(2)
}

/// Walk `<root>/YYYY-MM-DD/<device>/**` (files at depth ≤ 3 below the day
/// folder), classifying each file by *filename* patterns — never folder names
/// (device folder names vary across days).
fn collect_files(root: &Path, day_filter: Option<&str>) -> Result<Vec<SweepFile>, String> {
    let mut out = Vec::new();
    let mut days: Vec<_> = std::fs::read_dir(root)
        .map_err(|e| e.to_string())?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir())
        .collect();
    days.sort();
    for day in days {
        let day_name = day.file_name().unwrap_or_default().to_string_lossy().to_string();
        if !is_day_folder(&day_name) {
            continue;
        }
        if let Some(f) = day_filter {
            if day_name != f {
                continue;
            }
        }
        let mut day_files = Vec::new();
        walk_files(&day, 0, 3, &mut day_files);
        day_files.sort();
        for path in day_files {
            let class = classify(&path);
            out.push(SweepFile { path, class });
        }
    }
    Ok(out)
}

fn is_day_folder(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

fn walk_files(dir: &Path, depth: usize, max_depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.is_dir() {
            if depth < max_depth {
                walk_files(&p, depth + 1, max_depth, out);
            }
        } else {
            out.push(p);
        }
    }
}

/// Filename-pattern classification per the corpus inventory (WP-RW §2.3).
/// Keyed on file names only: device folder names differ between days
/// (`DR70D` vs `TASCAM`, `FS100` vs `NEX-FS100EK`).
fn classify(path: &Path) -> FileClass {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let lower = name.to_lowercase();
    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();

    // Derived converter outputs first (they also match the base patterns).
    if lower.contains("_clip") || lower.contains("_tr") || lower.contains("_audio_track") {
        return FileClass::Derived;
    }
    match ext.as_str() {
        // TASCAM Portacapture track files: S2 carries LTC, S1/S3/S4 are mics.
        "wav" => {
            if lower.starts_with("tascam_") && lower.ends_with("s2.wav") {
                FileClass::LtcExpected
            } else if lower.starts_with("tascam_")
                && (lower.ends_with("s1.wav")
                    || lower.ends_with("s3.wav")
                    || lower.ends_with("s4.wav"))
            {
                FileClass::NoLtcExpected
            } else {
                FileClass::Derived
            }
        }
        // A6100/A6700 clip originals: LPCM MP4 and the AAC m4v variants.
        "mp4" | "m4v" if lower.starts_with("c0") => FileClass::LtcExpected,
        // FS100 AVCHD originals: 0C0xxxx.MTS-style numeric names.
        "mts" | "m2ts" if lower.starts_with('0') => FileClass::LtcExpected,
        _ => FileClass::Derived,
    }
}

/// Decode one file (WAV direct, video via extract→decode) and package the
/// outcome. Empty-buffer errors (slicing past EOF) classify as `Empty`.
fn decode_one(path: &Path, class: FileClass, use_libltc: bool, start: Instant) -> FileOutcome {
    let is_wav = path.extension().unwrap_or_default().eq_ignore_ascii_case("wav");
    let params = WavDecodeParams {
        use_libltc,
        single_pass: false,
        decode_fps: FPS,
        decode_drop_frame: false,
    };
    let result = if is_wav {
        decode_wav_core(path, params, None, None, None).map(|o| o.result)
    } else {
        decode_video_file(path, 0, 0, use_libltc, FPS, false, None)
    };

    let mut outcome = FileOutcome {
        path: path.to_path_buf(),
        class,
        status: "Failed".to_string(),
        valid_frames: 0,
        total_possible_frames: 0,
        confidence: 0.0,
        first_tc: None,
        last_tc: None,
        first_tc_at_secs: 0.0,
        audio_duration_secs: 0.0,
        processing_ms: start.elapsed().as_secs_f64() * 1000.0,
        first_frame_no: None,
        last_frame_no: None,
    };

    match result {
        Err(e) => {
            let msg = e.to_string();
            if msg.to_lowercase().contains("no samples") {
                outcome.status = "Empty".to_string();
            } else {
                outcome.status = "Failed".to_string();
                eprintln!("    (error: {msg})");
            }
        }
        Ok(r) => {
            outcome.status = match &r.status {
                LtcDecodeStatus::Success => "Success".to_string(),
                LtcDecodeStatus::NoSyncWord => "NoSyncWord".to_string(),
                LtcDecodeStatus::LowConfidence => "LowConfidence".to_string(),
                LtcDecodeStatus::Error { message } => {
                    if message.to_lowercase().contains("no samples") {
                        "Empty".to_string()
                    } else {
                        "Error".to_string()
                    }
                }
            };
            outcome.valid_frames = r.valid_frames;
            outcome.total_possible_frames = r.total_possible_frames;
            outcome.confidence = r.avg_confidence;
            outcome.first_tc_at_secs = r.first_ltc_timecode_secs;
            outcome.audio_duration_secs = r.total_audio_duration_secs;
            if let (Some(first), Some(last)) = (r.timecodes.first(), r.timecodes.last()) {
                outcome.first_tc = Some(format_tc(&first.timecode));
                outcome.last_tc = Some(format_tc(&last.timecode));
                outcome.first_frame_no = Some(frame_number(&first.timecode, FPS));
                outcome.last_frame_no = Some(frame_number(&last.timecode, FPS));
            }
        }
    }
    outcome
}

fn format_tc(tc: &gui_engine::Timecode) -> String {
    format!("{:02}:{:02}:{:02}:{:02}", tc.hours, tc.minutes, tc.seconds, tc.frames)
}

/// Absolute frame number on a non-drop grid.
fn frame_number(tc: &gui_engine::Timecode, fps: f64) -> u64 {
    ((tc.hours as u64 * 3600 + tc.minutes as u64 * 60 + tc.seconds as u64) as f64 * fps).round()
        as u64
        + tc.frames as u64
}

/// Ground-truth-free oracle: within a day, every *device's* decoded TC range
/// (the envelope over its LTC-bearing clips) must pairwise overlap every other
/// device's range, and each file's TC span must match its audio duration
/// within ±2 s.
///
/// Why per-device envelopes rather than strict pairwise-per-file overlap: a
/// camera's clips are *sequential* segments (clip N ends where clip N+1
/// starts — measured 2026-09-25: each FS100 clip overlaps the TASCAM/A6100
/// multi-hour window but never its neighbours), so per-file pairwise overlap
/// structurally flags healthy recordings. Cross-device staggered-start overlap
/// is the invariant the multi-cam sync workflow depends on.
///
/// Violations are reported, never fatal — a violation is a finding to record,
/// not an error to fix by tuning the oracle.
fn evaluate_days(outcomes: &[FileOutcome]) -> Vec<DayVerdict> {
    let mut by_day: BTreeMap<String, Vec<&FileOutcome>> = BTreeMap::new();
    for o in outcomes {
        let day = day_of(&o.path);
        if o.class == FileClass::LtcExpected && o.status == "Success" {
            by_day.entry(day).or_default().push(o);
        }
    }

    by_day
        .into_iter()
        .map(|(day, files)| {
            let ltc_files = files.len();
            let mut violations = Vec::new();
            let mut overlaps = Vec::new();

            // Per-file span ≈ audio duration (±2 s at 25 fps).
            for f in &files {
                let (Some(f0), Some(f1)) = (f.first_frame_no, f.last_frame_no) else {
                    continue;
                };
                let span_secs = (f1.saturating_sub(f0)) as f64 / FPS;
                if (span_secs - f.audio_duration_secs).abs() > 2.0 {
                    violations.push(format!(
                        "{}: TC span {:.1}s vs audio duration {:.1}s",
                        file_name(&f.path),
                        span_secs,
                        f.audio_duration_secs
                    ));
                }
            }

            // Per-device envelopes, then pairwise cross-device overlap.
            let mut by_device: BTreeMap<String, (u64, u64)> = BTreeMap::new();
            for f in &files {
                let (Some(f0), Some(f1)) = (f.first_frame_no, f.last_frame_no) else {
                    continue;
                };
                let dev = device_of(&f.path);
                by_device
                    .entry(dev)
                    .and_modify(|(lo, hi)| {
                        *lo = (*lo).min(f0);
                        *hi = (*hi).max(f1);
                    })
                    .or_insert((f0, f1));
            }
            let devices: Vec<_> = by_device.into_iter().collect();
            for (i, (da, (a0, a1))) in devices.iter().enumerate() {
                for (db, (b0, b1)) in devices.iter().skip(i + 1) {
                    let overlap_frames = (*a1).min(*b1).saturating_sub((*a0).max(*b0));
                    overlaps.push((
                        da.clone(),
                        db.clone(),
                        overlap_frames as f64 / FPS / 60.0,
                    ));
                    if overlap_frames == 0 {
                        violations.push(format!(
                            "devices {da} and {db} TC ranges do not overlap"
                        ));
                    }
                }
            }

            let consistent = violations.is_empty();
            DayVerdict { day, ltc_files, consistent, violations, overlaps }
        })
        .collect()
}

fn day_of(path: &Path) -> String {
    for comp in path.components() {
        let s = comp.as_os_str().to_string_lossy();
        if is_day_folder(&s) {
            return s.to_string();
        }
    }
    "unknown".to_string()
}

fn file_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().to_string()
}

/// The device folder: the path component directly below the day folder.
/// Folder names vary across days (`DR70D` vs `TASCAM`, …) but are stable
/// within a day, which is all the oracle needs.
fn device_of(path: &Path) -> String {
    let comps: Vec<_> = path.components().map(|c| c.as_os_str().to_string_lossy().to_string()).collect();
    for (i, c) in comps.iter().enumerate() {
        if is_day_folder(c) {
            return comps.get(i + 1).cloned().unwrap_or_else(|| "unknown".to_string());
        }
    }
    "unknown".to_string()
}

// --- JSON output (hand-rolled: stdlib-only, controlled escaping) ---

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

fn json_opt(s: &Option<String>) -> String {
    match s {
        Some(v) => json_str(v),
        None => "null".to_string(),
    }
}

fn write_json(
    path: &Path,
    outcomes: &[FileOutcome],
    verdicts: &[DayVerdict],
    use_libltc: bool,
) -> std::io::Result<()> {
    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut j = String::with_capacity(64 * 1024);
    j.push_str("{\n");
    j.push_str("  \"schema\": \"ltc-corpus-sweep/v1\",\n");
    j.push_str(&format!(
        "  \"binary_version\": {},\n",
        json_str(env!("CARGO_PKG_VERSION"))
    ));
    j.push_str(&format!(
        "  \"decoder\": {},\n",
        json_str(if use_libltc { "libltc" } else { "builtin" })
    ));
    j.push_str(&format!("  \"generated_at\": {generated_at},\n"));

    j.push_str("  \"files\": [\n");
    for (i, o) in outcomes.iter().enumerate() {
        let pct = if o.total_possible_frames > 0 {
            o.valid_frames as f64 / o.total_possible_frames as f64 * 100.0
        } else {
            0.0
        };
        j.push_str(&format!(
            "    {{\"path\": {}, \"class\": {}, \"status\": {}, \"valid_frames\": {}, \
             \"total_possible_frames\": {}, \"percent\": {pct:.1}, \"confidence\": {:.3}, \
             \"first_tc\": {}, \"last_tc\": {}, \"first_tc_at_secs\": {:.3}, \
             \"audio_duration_secs\": {:.3}, \"processing_ms\": {:.0}}}{}\n",
            json_str(&o.path.display().to_string()),
            json_str(o.class.label()),
            json_str(&o.status),
            o.valid_frames,
            o.total_possible_frames,
            o.confidence,
            json_opt(&o.first_tc),
            json_opt(&o.last_tc),
            o.first_tc_at_secs,
            o.audio_duration_secs,
            o.processing_ms,
            if i + 1 < outcomes.len() { "," } else { "" },
        ));
    }
    j.push_str("  ],\n");

    j.push_str("  \"days\": [\n");
    for (i, v) in verdicts.iter().enumerate() {
        j.push_str(&format!(
            "    {{\"day\": {}, \"ltc_files\": {}, \"consistent\": {}, \
             \"violations\": [{}], \"overlaps\": [{}]}}{}\n",
            json_str(&v.day),
            v.ltc_files,
            v.consistent,
            v.violations
                .iter()
                .map(|s| json_str(s))
                .collect::<Vec<_>>()
                .join(", "),
            v.overlaps
                .iter()
                .map(|(a, b, m)| {
                    format!("[{}, {}, {m:.2}]", json_str(a), json_str(b))
                })
                .collect::<Vec<_>>()
                .join(", "),
            if i + 1 < verdicts.len() { "," } else { "" },
        ));
    }
    j.push_str("  ]\n}\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, j)
}
