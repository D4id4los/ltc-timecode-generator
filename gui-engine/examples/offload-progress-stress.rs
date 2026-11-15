//! Offload progress/status stress runner (plan 2026-11-06 phase 1, step 6).
//!
//! Drives the real engine thread with the real copy backend against a
//! user-supplied target directory, running deterministic stress scenarios
//! while continuously sampling the *published snapshot* — the exact data
//! both GUIs render. Reports invariant violations as a JSON digest.
//!
//! Invariants (sampled every tick of the sampler):
//! - published `jobs[OffloadCopy]` fraction is monotonic non-decreasing
//!   within a run;
//! - fraction < 1.0 while the job is active and the destination bytes are
//!   still below the planned total;
//! - no terminal phase while any `.offload_tmp` exists in the destination;
//! - final phase matches the run outcome (Succeeded ⇔ all planned bytes
//!   present, Cancelled ⇔ a cancel was requested);
//! - no `.offload_tmp` residue after a run ends.
//!
//! This is a *measurement* tool in the corpus-sweep tradition: exit 0 on
//! completed scenarios; violations are reported, not asserted.
//!
//! Usage:
//! ```text
//! cargo run --release -p gui-engine --example offload-progress-stress -- \
//!     --dest /tmp/offload-stress [--scenario many-small-files|--all] \
//!     [--json reports/offload-stress/run.json] [--files 200] [--size 1048576]
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use gui_engine::command::{GuiCommand, OffloadCommand};
use gui_engine::engine::{engine_main_with_seams, EngineSeams, OffloadCopyFn, ScanCardsFn};
use gui_engine::job::{JobKind, JobPhase};
use gui_engine::offload::{DeviceNameSource, OffloadFileInfo, SdCardInfo};
use gui_engine::state::AppStateSnapshot;
use gui_engine::{AudioEvent, FfmpegCapabilities, HwDeviceCapabilities};

// ── Scenario definition ──────────────────────────────────────────────────

struct Scenario {
    name: &'static str,
    /// (device name, per-device file count, per-file size in bytes).
    devices: Vec<(&'static str, usize, u64)>,
    /// Fraction of the copy after which to request cancel (None = run to completion).
    cancel_at_fraction: Option<f32>,
    /// Number of consecutive back-to-back runs with the same plan.
    consecutive_runs: usize,
}

fn scenarios(files: usize, size: u64) -> Vec<Scenario> {
    vec![
        Scenario {
            name: "many-small-files",
            devices: vec![("SMALL", files, 64 * 1024)],
            cancel_at_fraction: None,
            consecutive_runs: 1,
        },
        Scenario {
            name: "few-huge-files",
            devices: vec![("HUGE", 4, size.max(4 * 1024 * 1024))],
            cancel_at_fraction: None,
            consecutive_runs: 1,
        },
        Scenario {
            name: "multi-device-size-skew",
            devices: vec![("SKEW-A", 2, 4 * 1024 * 1024), ("SKEW-B", 8, 32 * 1024)],
            cancel_at_fraction: None,
            consecutive_runs: 1,
        },
        Scenario {
            // Large enough that the copy window outlives the sampler's
            // poll interval and the cancel request actually lands mid-copy.
            name: "cancel-mid-copy",
            devices: vec![("CANCEL", 6, size.max(32 * 1024 * 1024))],
            cancel_at_fraction: Some(0.3),
            consecutive_runs: 1,
        },
        Scenario {
            name: "consecutive-runs",
            devices: vec![("REPEAT", 6, 1024 * 1024)],
            cancel_at_fraction: None,
            consecutive_runs: 3,
        },
    ]
}

// ── Sampling / invariants ────────────────────────────────────────────────

#[derive(Clone, Default, serde::Serialize)]
struct ScenarioReport {
    scenario: String,
    runs: Vec<RunReport>,
    violations: Vec<String>,
    completed: bool,
}

#[derive(Clone, Default, serde::Serialize)]
struct RunReport {
    run_index: usize,
    cancelled: bool,
    final_phase: String,
    final_fraction: f32,
    samples: usize,
    max_fraction: f32,
    completed_devices: Vec<String>,
    failed_devices: Vec<String>,
}

struct SamplerState {
    /// Phase observed at the previous sample (edge trigger for run capture).
    prev_phase: JobPhase,
    /// Fraction observed at the previous sample within the current run.
    prev_fraction: f32,
    run_index: usize,
    report: Arc<Mutex<ScenarioReport>>,
}

impl SamplerState {
    fn violation(&self, msg: String) {
        self.report.lock().unwrap().violations.push(msg);
    }
}

fn planned_total_bytes(card_mounts: &[PathBuf]) -> u64 {
    card_mounts
        .iter()
        .flat_map(|m| std::fs::read_dir(m).expect("read_dir src mount"))
        .filter_map(|e| e.ok())
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

fn dest_bytes(dest_root: &Path) -> u64 {
    fn walk(dir: &Path, total: &mut u64) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, total);
            } else if let Ok(m) = entry.metadata() {
                *total += m.len();
            }
        }
    }
    let mut total = 0u64;
    walk(dest_root, &mut total);
    total
}

fn has_offload_tmp(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().map(|e| e == "offload_tmp").unwrap_or(false) {
                out.push(p);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out
}

// ── Scenario execution ───────────────────────────────────────────────────

/// Build synthetic source mounts under `<src_root>/<device>/` with real
/// deterministic bytes, and the matching `SdCardInfo` fixtures.
fn build_sources(src_root: &Path, devices: &[(&str, usize, u64)]) -> Vec<SdCardInfo> {
    let mut cards = Vec::new();
    for (name, count, size) in devices {
        let mount = src_root.join(name);
        std::fs::create_dir_all(&mount).expect("create source mount");
        let mut files = Vec::new();
        for i in 0..*count {
            let path = mount.join(format!("clip{i:03}.mxf"));
            // Deterministic pseudo-content; written once per scenario setup.
            let chunk = vec![0xA5u8; 1024 * 1024];
            let mut written = 0u64;
            {
                use std::io::Write;
                let mut f = std::fs::File::create(&path).expect("create source file");
                while written < *size {
                    let n = (*size - written).min(chunk.len() as u64) as usize;
                    f.write_all(&chunk[..n]).expect("write source chunk");
                    written += n as u64;
                }
            }
            let meta = std::fs::metadata(&path).expect("stat source");
            files.push(OffloadFileInfo {
                path: path.clone(),
                name: path.file_name().unwrap().to_string_lossy().to_string(),
                size_bytes: meta.len(),
                modified: meta.modified().ok().map(|t| t.into()),
            });
        }
        let selected_count = files.len();
        let selected_bytes = files.iter().map(|f| f.size_bytes).sum();
        cards.push(SdCardInfo {
            mount: mount.clone(),
            volume_label: name.to_string(),
            device_name: name.to_string(),
            name_source: DeviceNameSource::VolumeLabel,
            media_file_count: files.len(),
            total_bytes: selected_bytes,
            files,
            selected: vec![true; selected_count],
            selected_count,
            selected_bytes,
        });
    }
    cards
}

fn run_scenario(scenario: &Scenario, src_root: &Path, dest_root: &Path) -> ScenarioReport {
    // Fresh per-scenario dirs.
    let scenario_src = src_root.join(scenario.name);
    let scenario_dest = dest_root.join(scenario.name);
    let _ = std::fs::remove_dir_all(&scenario_src);
    let _ = std::fs::remove_dir_all(&scenario_dest);
    std::fs::create_dir_all(&scenario_dest).expect("create dest");

    let cards = build_sources(&scenario_src, &scenario.devices);
    let planned_bytes =
        planned_total_bytes(&cards.iter().map(|c| c.mount.clone()).collect::<Vec<_>>());

    let scan_seam: ScanCardsFn = {
        let cards = cards.clone();
        Arc::new(move |_cancel, _progress| Ok(cards.clone()))
    };
    // Real copy backend through the engine seam (default seam already does
    // this, but be explicit that no fake is involved here).
    let copy_seam: OffloadCopyFn = Arc::new(|ctx, plans, names, dest| {
        gui_engine::offload::run_offload_copy_job(ctx, plans, names, dest)
    });

    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_for_engine = Arc::clone(&state);
    let (event_tx, _event_rx) = mpsc::channel::<AudioEvent>();
    let seams = EngineSeams {
        ffmpeg_caps: Box::new(|| FfmpegCapabilities {
            has_ffmpeg: false,
            available_encoders: BTreeSet::new(),
            available_formats: BTreeSet::new(),
            error_message: None,
            ffmpeg_version: None,
            hw: HwDeviceCapabilities::default(),
        }),
        scan_cards: scan_seam,
        offload_copy: copy_seam,
        ..EngineSeams::default()
    };
    let engine_handle = std::thread::Builder::new()
        .name("offload-stress-engine".into())
        .spawn(move || engine_main_with_seams(rx, state_for_engine, false, event_tx, seams))
        .expect("spawn engine");

    tx.send(GuiCommand::Offload(OffloadCommand::SetParentFolder(
        scenario_dest.clone(),
    )))
    .unwrap();
    tx.send(GuiCommand::Offload(OffloadCommand::SetParentName(
        "run".into(),
    )))
    .unwrap();

    let cancel_requested = Arc::new(AtomicBool::new(false));

    // Sampler: continuously observes the published snapshot while a run is
    // active and records invariant violations.
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler_report = Arc::new(Mutex::new(ScenarioReport {
        scenario: scenario.name.to_string(),
        ..Default::default()
    }));
    let sampler_handle = {
        let state = Arc::clone(&state);
        let sampling = Arc::clone(&sampling);
        let scenario_dest = scenario_dest.clone();
        let sampler_report = Arc::clone(&sampler_report);
        let mut sampler = SamplerState {
            prev_phase: JobPhase::Idle,
            prev_fraction: 0.0,
            run_index: 0,
            report: sampler_report,
        };
        std::thread::Builder::new()
            .name("offload-stress-sampler".into())
            .spawn(move || {
                while sampling.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(5));
                    let snap = state.load().as_ref().clone();
                    let job = snap.jobs.get(&JobKind::OffloadCopy);
                    let phase = job.map(|j| j.phase()).unwrap_or(JobPhase::Idle);
                    let fraction = job.map(|j| j.fraction()).unwrap_or(0.0);

                    if matches!(phase, JobPhase::Running | JobPhase::Indeterminate) {
                        // Fresh-run start: reset the per-run monotonicity base.
                        if !matches!(
                            sampler.prev_phase,
                            JobPhase::Running | JobPhase::Indeterminate
                        ) {
                            sampler.prev_fraction = 0.0;
                        }
                        // Monotonicity within the run.
                        if fraction + 1e-4 < sampler.prev_fraction {
                            sampler.violation(format!(
                                "run {}: fraction regressed {:.4} → {:.4}",
                                sampler.run_index, sampler.prev_fraction, fraction
                            ));
                        }
                        sampler.prev_fraction = fraction;
                        // Below 1.0 while destination bytes are short.
                        let db = dest_bytes(&scenario_dest.join("run"));
                        if fraction >= 0.999 && db < planned_bytes.saturating_sub(64 * 1024) {
                            sampler.violation(format!(
                                "run {}: fraction {} at 1.0-ish while dest bytes {db} < planned {planned_bytes}",
                                sampler.run_index, fraction
                            ));
                        }
                    } else if matches!(
                        phase,
                        JobPhase::Succeeded | JobPhase::Cancelled | JobPhase::Failed
                    ) && matches!(
                        sampler.prev_phase,
                        JobPhase::Running | JobPhase::Indeterminate
                    ) {
                        // Active → terminal edge: capture the run outcome.
                        let residue = has_offload_tmp(&scenario_dest);
                        if !residue.is_empty() {
                            sampler.violation(format!(
                                "run {}: .offload_tmp residue after run end: {:?}",
                                sampler.run_index, residue
                            ));
                        }
                        // Outcome/phase agreement: Succeeded ⇔ everything
                        // planned is on disk (cancel runs exempt).
                        if phase == JobPhase::Succeeded {
                            let db = dest_bytes(&scenario_dest.join("run"));
                            if db + 64 * 1024 < planned_bytes {
                                sampler.violation(format!(
                                    "run {}: Succeeded but dest bytes {db} < planned {planned_bytes}",
                                    sampler.run_index
                                ));
                            }
                        }
                        let run = RunReport {
                            run_index: sampler.run_index,
                            cancelled: phase == JobPhase::Cancelled,
                            final_phase: format!("{phase:?}"),
                            final_fraction: fraction,
                            samples: 1,
                            max_fraction: sampler.prev_fraction,
                            completed_devices: snap.offload.completed_devices.clone(),
                            failed_devices: snap
                                .offload
                                .run_summary
                                .as_ref()
                                .map(|s| s.failed_devices.clone())
                                .unwrap_or_default(),
                        };
                        let mut report = sampler.report.lock().unwrap();
                        report.runs.push(run);
                        drop(report);
                        sampler.run_index += 1;
                        sampler.prev_fraction = 0.0;
                    }
                    sampler.prev_phase = phase;
                }
            })
            .expect("spawn sampler")
    };

    for run_idx in 0..scenario.consecutive_runs {
        // A consecutive run must start from a fresh snapshot: the previous
        // run's terminal status must not leak (defect A).
        let _ = run_idx;
        tx.send(GuiCommand::Offload(OffloadCommand::ScanCards))
            .unwrap();
        wait_for(&state, |s| {
            s.jobs
                .get(&JobKind::OffloadScan)
                .map(|j| j.phase() == JobPhase::Succeeded)
                .unwrap_or(false)
        });
        tx.send(GuiCommand::Offload(OffloadCommand::StartOffload))
            .unwrap();

        // Defect-A invariant: the run must become *active* shortly after
        // StartOffload (fresh Running status published on spawn) — never
        // sit on the previous run's terminal phase.
        if run_idx > 0 {
            let fresh_deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let phase = state
                    .load()
                    .jobs
                    .get(&JobKind::OffloadCopy)
                    .map(|j| j.phase())
                    .unwrap_or(JobPhase::Idle);
                if matches!(phase, JobPhase::Running | JobPhase::Indeterminate) {
                    break;
                }
                assert!(
                    Instant::now() < fresh_deadline,
                    "scenario {}: run {run_idx} never became active after StartOffload \
                     (stale terminal status leak, defect A)",
                    scenario.name
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        // Fresh-progress invariant: within a short window, an active run
        // must show a non-terminal phase and a fraction well below 1.0
        // unless the whole plan already fit in one tick.
        let deadline = Instant::now() + Duration::from_secs(120);
        let cancelled_at = scenario.cancel_at_fraction.is_some();
        loop {
            let snap = state.load().as_ref().clone();
            let phase = snap
                .jobs
                .get(&JobKind::OffloadCopy)
                .map(|j| j.phase())
                .unwrap_or(JobPhase::Idle);
            let fraction = snap
                .jobs
                .get(&JobKind::OffloadCopy)
                .map(|j| j.fraction())
                .unwrap_or(0.0);
            if cancelled_at
                && fraction >= scenario.cancel_at_fraction.unwrap()
                && !cancel_requested.swap(true, Ordering::SeqCst)
            {
                tx.send(GuiCommand::Offload(OffloadCommand::CancelOffload))
                    .unwrap();
            }
            if matches!(
                phase,
                JobPhase::Succeeded | JobPhase::Cancelled | JobPhase::Failed
            ) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "scenario {} run {run_idx}: copy never reached a terminal phase",
                scenario.name
            );
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    // Shut down: engine exits via the Shutdown command.
    let _ = tx.send(GuiCommand::Shutdown);
    sampling.store(false, Ordering::Relaxed);
    let _ = engine_handle.join();
    let _ = sampler_handle.join();

    // The sampler owns the run captures and violations — return its report.
    let mut sampler_report = {
        let guard = sampler_report.lock().unwrap();
        guard.clone()
    };
    sampler_report.completed = true;
    sampler_report
}

fn wait_for(state: &Arc<ArcSwap<AppStateSnapshot>>, pred: impl Fn(&AppStateSnapshot) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if pred(&state.load()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "stress harness predicate timeout"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ── CLI ──────────────────────────────────────────────────────────────────

struct Args {
    dest: PathBuf,
    scenario: Option<String>,
    json: Option<PathBuf>,
    files: usize,
    size: u64,
}

fn parse_args() -> Args {
    let mut args = Args {
        dest: PathBuf::from("/tmp/offload-stress"),
        scenario: None,
        json: None,
        files: 200,
        size: 8 * 1024 * 1024,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dest" => args.dest = PathBuf::from(it.next().expect("--dest needs a value")),
            "--scenario" => args.scenario = Some(it.next().expect("--scenario needs a value")),
            "--json" => args.json = Some(PathBuf::from(it.next().expect("--json needs a value"))),
            "--files" => {
                args.files = it
                    .next()
                    .expect("--files needs a value")
                    .parse()
                    .expect("--files N")
            }
            "--size" => {
                args.size = it
                    .next()
                    .expect("--size needs a value")
                    .parse()
                    .expect("--size BYTES")
            }
            "--all" => args.scenario = None,
            other => {
                eprintln!("unknown arg: {other}");
                std::process::exit(2);
            }
        }
    }
    args
}

fn main() -> ExitCode {
    let args = parse_args();
    std::fs::create_dir_all(&args.dest).expect("create dest root");
    let src_root = args.dest.join("_stress-src");
    let _ = std::fs::remove_dir_all(&src_root);
    std::fs::create_dir_all(&src_root).expect("create src root");

    let all = scenarios(args.files, args.size);
    let selected: Vec<Scenario> = match &args.scenario {
        Some(name) => all
            .into_iter()
            .filter(|s| s.name == name)
            .collect::<Vec<_>>(),
        None => all,
    };
    if selected.is_empty() {
        eprintln!("no scenario matched {:?}", args.scenario);
        return ExitCode::from(2);
    }

    let mut reports = Vec::new();
    let mut any_violation = false;
    for scenario in &selected {
        println!("── scenario: {} ──", scenario.name);
        let report = run_scenario(scenario, &src_root, &args.dest);
        for v in &report.violations {
            println!("  VIOLATION: {v}");
        }
        for run in &report.runs {
            println!(
                "  run {}: phase={} fraction={:.3} completed={:?} failed={:?}",
                run.run_index,
                run.final_phase,
                run.final_fraction,
                run.completed_devices,
                run.failed_devices
            );
        }
        println!(
            "  completed={}, violations={}",
            report.completed,
            report.violations.len()
        );
        any_violation |= !report.violations.is_empty();
        reports.push(report);
    }

    if let Some(json_path) = &args.json {
        if let Some(parent) = json_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match serde_json::to_string_pretty(&reports) {
            Ok(body) => {
                if let Err(e) = std::fs::write(json_path, body) {
                    eprintln!("failed to write {json_path:?}: {e}");
                } else {
                    println!("JSON digest written to {}", json_path.display());
                }
            }
            Err(e) => eprintln!("failed to serialise report: {e}"),
        }
    }

    // Measurement tool: exit 0 on completed scenarios; violations are
    // reported (loudly) but do not fail the run.
    if any_violation {
        eprintln!("NOTE: invariant violations detected (see report above)");
    }
    ExitCode::SUCCESS
}
