//! Phase 5 (G4a) publish-count measurement — manual perf run, NOT a test:
//! `cargo run --release -p gui-engine --example clap_publish_count`
//!
//! Spawns the real engine loop (fake ffmpeg-caps probe, isolated config
//! dir), sends exactly one Clap, and counts distinct published snapshots
//! (ArcSwap pointer changes) over a 3 s window. Run against a pre-Phase-5
//! worktree to record the "before" number (engine-side per-tick decay
//! churn); the current code records the "after" number.

use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gui_engine::converter::{FfmpegCapabilities, HwDeviceCapabilities};

fn fake_probe() -> FfmpegCapabilities {
    FfmpegCapabilities {
        has_ffmpeg: false,
        available_encoders: std::collections::BTreeSet::new(),
        available_formats: std::collections::BTreeSet::new(),
        error_message: None,
        ffmpeg_version: None,
        hw: HwDeviceCapabilities::default(),
    }
}
use gui_engine::engine::engine_main_with_probe;
use gui_engine::{command::GuiCommand, state::AppStateSnapshot, ArcSwap};

fn main() {
    // Testing rule 4: never touch the real user config.
    let tmp = tempfile::TempDir::new().unwrap();
    std::env::set_var("LTC_CONFIG_HOME", tmp.path());
    std::env::set_var("XDG_CONFIG_HOME", tmp.path());

    let (tx, rx) = mpsc::channel::<GuiCommand>();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let (event_tx, _event_rx) = mpsc::channel();

    let engine_state = state.clone();
    let handle = std::thread::spawn(move || {
        engine_main_with_probe(rx, engine_state, false, event_tx, fake_probe)
    });

    std::thread::sleep(Duration::from_millis(500)); // let the startup settle

    let mut last_ptr: *const AppStateSnapshot = Arc::as_ptr(&state.load_full());
    let mut publishes = 0usize;
    let t0 = Instant::now();
    tx.send(GuiCommand::Clap).unwrap();
    while t0.elapsed() < Duration::from_secs(3) {
        let snap = state.load_full();
        let ptr = Arc::as_ptr(&snap);
        if ptr != last_ptr {
            publishes += 1;
            last_ptr = ptr;
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    let _ = tx.send(GuiCommand::Shutdown);
    handle.join().unwrap();
    println!("publishes observed across one clap (3 s window): {}", publishes);
}
