//! WP-5.5 (G4b) snapshot-scalability measurement harness — manual perf run,
//! NOT a test: `cargo run --release -p gui-engine --example snapshot_scalability`.
//!
//! Builds a worst-case snapshot (MAX_CLAP_LOGS clap-log entries, a 10 000-file
//! offload card list, an active conversion job status) and times the two paths
//! the engine's publish gate takes per tick:
//!   1. compare  — `last != current` structural PartialEq (idle tick cost)
//!   2. clone    — `current.clone()` + Arc (changed-tick cost, while a job's
//!      progress mutates every tick)
//!
//! Reports p50/p99 per path over N iterations. Mirrors
//! `publish_if_changed` (gui-engine/src/engine.rs) without needing the
//! private EngineLoopState.

use std::path::PathBuf;
use std::time::Instant;

use gui_engine::job::{JobKind, JobPhase, JobStatus, ProgressSnapshot};
use gui_engine::offload::{DeviceNameSource, OffloadFileInfo, SdCardInfo};
use gui_engine::state::{AppStateSnapshot, ClapLogItem};

const CLAP_LOG_ENTRIES: usize = 1000; // = MAX_CLAP_LOGS
const CARD_FILES: usize = 10_000; // offload scan cap
const TICKS: usize = 2000; // ≈ 80 s of engine ticks

fn percentiles(mut samples: Vec<f64>) -> (f64, f64) {
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p = |f: f64| samples[((samples.len() as f64 - 1.0) * f).round() as usize];
    (p(0.50), p(0.99))
}

fn heavy_snapshot() -> AppStateSnapshot {
    let mut s = AppStateSnapshot::initial();
    for i in 0..CLAP_LOG_ENTRIES {
        s.clapper.logs.push(ClapLogItem {
            id: i as u64,
            timestamp: "2026-10-04T12:00:00Z".to_string(),
            timecode: "01:00:00:00".to_string(),
            milliseconds: "3600000.000".to_string(),
            note: format!("Scene {}/Take {}", i % 100, i % 50),
        });
    }
    let mut card = SdCardInfo {
        mount: PathBuf::from("/mnt/card"),
        volume_label: "BENCH".to_string(),
        device_name: "CAM".to_string(),
        name_source: DeviceNameSource::Manual,
        media_file_count: CARD_FILES,
        total_bytes: 0,
        files: Vec::with_capacity(CARD_FILES),
        selected: vec![true; CARD_FILES],
        selected_count: CARD_FILES,
        selected_bytes: 0,
    };
    for i in 0..CARD_FILES {
        card.files.push(OffloadFileInfo {
            path: PathBuf::from(format!("/mnt/card/CLIP{:04}.MXF", i)),
            name: format!("CLIP{:04}.MXF", i),
            size_bytes: 4_000_000_000,
            modified: None,
        });
        card.total_bytes += 4_000_000_000;
    }
    s.offload.cards.push(card);
    s.jobs.insert(
        JobKind::Conversion,
        JobStatus {
            progress: ProgressSnapshot {
                phase: JobPhase::Running,
                fraction: 0.5,
                message: "converting".to_string(),
                speed: Some(42.0),
                units: Vec::new(),
                log: String::new(),
            },
            error: None,
        },
    );
    s
}

fn main() {
    let last = Arc::new(heavy_snapshot());
    let mut current = (*last).clone();

    // 1. Idle-tick path: compare only (must be equal → no clone).
    let mut cmp_samples = Vec::with_capacity(TICKS);
    for _ in 0..TICKS {
        let t = Instant::now();
        let changed = last.as_ref() != &current;
        debug_assert!(!changed);
        cmp_samples.push(t.elapsed().as_secs_f64() * 1e3);
    }

    // 2. Changed-tick path: per-tick progress mutation + compare + clone.
    let mut changed_samples = Vec::with_capacity(TICKS);
    for i in 0..TICKS {
        current.jobs.get_mut(&JobKind::Conversion).unwrap().progress.fraction = (i % 1000) as f32 / 1000.0;
        let t = Instant::now();
        let changed = last.as_ref() != &current;
        if changed {
            let _next = std::sync::Arc::new(current.clone());
        }
        changed_samples.push(t.elapsed().as_secs_f64() * 1e3);
    }

    let (c50, c99) = percentiles(cmp_samples);
    let (d50, d99) = percentiles(changed_samples);
    println!("snapshot scalability ({} clap logs, {} card files, {} ticks)", CLAP_LOG_ENTRIES, CARD_FILES, TICKS);
    println!("idle tick   (PartialEq compare only): p50 {:.3} ms, p99 {:.3} ms", c50, c99);
    println!("active tick (compare + deep clone):   p50 {:.3} ms, p99 {:.3} ms", d50, d99);
    println!("tick budget = 40 ms; trigger threshold ≈ 1 ms compare cost (2.5 % of budget)");
}

use std::sync::Arc;
