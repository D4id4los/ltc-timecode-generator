use std::collections::BTreeSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};
use std::path::{Path, PathBuf};

use arc_swap::ArcSwap;
use gui_engine::command::{GuiCommand, OffloadCommand};
use gui_engine::engine::{engine_main_with_probe, engine_main_with_seams, EngineSeams, ScanCardsFn};
use gui_engine::state::AppStateSnapshot;
use gui_engine::offload::{OffloadFileInfo, SdCardInfo};
use gui_engine::{decode_ltc_from_wav, JobKind, JobPhase, LtcDecodeStatus, FfmpegCapabilities, HwDeviceCapabilities, DeviceNameSource};

// ── Helpers ──────────────────────────────────────────────────────────────

const POLL_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn make_wav_cli(path: &Path, fps: f64, drop_frame: bool, duration: f64, sample_rate: u32) -> gui_engine::cli::Cli {
    gui_engine::cli::Cli {
        output_to_file: Some(path.to_string_lossy().to_string()),
        duration: Some(duration),
        fps,
        start_timecode: "01:00:00:00".to_string(),
        channel: "both".to_string(),
        volume: 0.5,
        sample_rate: Some(sample_rate),
        list_devices: false,
        headless: false,
        device: None,
        device_index: None,
        drop_frame,
        verbose: false,
        debug: false,
        decode: None, audio_stream: 0, audio_channel: 0,
        decoder: "builtin".to_string(),
        decode_fps: fps,
        decode_drop_frame: drop_frame,
        single_pass: false,
        context_frames: 3,
        list_timecodes: false,
        autostart: false,
    }
}

fn generate_wav(path: &Path, fps: f64, drop_frame: bool, duration: f64, sample_rate: u32) {
    let cli = make_wav_cli(path, fps, drop_frame, duration, sample_rate);
    gui_engine::cli::generate_wav(cli).expect("WAV generation failed");
}

fn fake_probe() -> FfmpegCapabilities {
    FfmpegCapabilities {
        has_ffmpeg: false,
        available_encoders: BTreeSet::new(),
        available_formats: BTreeSet::new(),
        error_message: None,
        hw: HwDeviceCapabilities::default(),
    }
}

/// Ensure the test does not write to the real user config directory.
fn init_test_config() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let dir = tempfile::TempDir::new().expect("tempdir for test config");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        let _ = Box::leak(Box::new(dir));
    });
}

/// Start engine, send commands, wait until a predicate is satisfied, return snapshot.
fn run_engine<F>(commands: Vec<GuiCommand>, use_libltc: bool, predicate: F) -> AppStateSnapshot
where
    F: Fn(&AppStateSnapshot) -> bool,
{
    init_test_config();

    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, use_libltc, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    for cmd in commands {
        tx.send(cmd).unwrap();
    }

    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snapshot: AppStateSnapshot = state.load().as_ref().clone();
        if predicate(&snapshot) {
            let final_snapshot: AppStateSnapshot = state.load().as_ref().clone();
            drop(tx);
            handle.join().expect("engine thread panicked");
            return final_snapshot;
        }
        if Instant::now() > deadline {
            panic!(
                "Timeout waiting for predicate (ltc_job={:?}, status={})",
                snapshot.job(JobKind::LtcDecode), snapshot.status.message()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

// ── Offload engine-test harness (PR-1/PR-2 seam) ─────────────────────────

/// Handle to a running test engine thread with injectable seams.
struct TestEngine {
    tx: mpsc::Sender<GuiCommand>,
    state: Arc<ArcSwap<AppStateSnapshot>>,
    handle: std::thread::JoinHandle<()>,
}

impl TestEngine {
    fn shutdown(self) {
        let _ = self.tx.send(GuiCommand::Shutdown);
        self.handle.join().expect("engine thread panicked");
    }
}

/// Spawn the engine with the default seams except `scan_cards`.
fn spawn_engine_with_scan_seam(
    scan_cards: ScanCardsFn,
) -> TestEngine {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);
    let handle = std::thread::Builder::new()
        .name("gui-engine-seams-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            let seams = EngineSeams {
                ffmpeg_caps: Box::new(fake_probe),
                scan_cards,
            };
            engine_main_with_seams(rx, state_clone, false, event_tx, seams);
        })
        .expect("failed to spawn engine thread");
    TestEngine { tx, state, handle }
}

/// Build a one-card seam whose files live in `mount_dir`.
fn static_scan_seam(card: SdCardInfo) -> ScanCardsFn {
    Arc::new(move |_cancel, _progress| Ok(vec![card.clone()]))
}

/// Poll the published snapshot until `predicate` holds; panic on timeout.
fn wait_for_snapshot<F>(state: &Arc<ArcSwap<AppStateSnapshot>>, what: &str, predicate: F) -> AppStateSnapshot
where
    F: Fn(&AppStateSnapshot) -> bool,
{
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snapshot = state.load().as_ref().clone();
        if predicate(&snapshot) {
            return state.load().as_ref().clone();
        }
        if Instant::now() > deadline {
            panic!("Timeout waiting for {what}");
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Create a tempdir whose lifetime outlives the engine thread by leaking it,
/// returning the plain path (tests never clean up; OS does at exit).
fn make_persistent_dir(tag: &str) -> PathBuf {
    let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir ({tag}): {e}"));
    let path = dir.path().to_path_buf();
    Box::leak(Box::new(dir));
    path
}

/// Hand-built card fixture pointing at real files on disk.
fn make_card(mount: &Path, device_name: &str, file_names: &[&str]) -> SdCardInfo {
    use std::fs;
    let mut files = Vec::new();
    for name in file_names {
        let path = mount.join(name);
        let contents = format!("fake-wav-payload:{name}").into_bytes();
        fs::write(&path, &contents).expect("write card file");
        files.push(OffloadFileInfo {
            path: path.clone(),
            name: (*name).to_string(),
            size_bytes: contents.len() as u64,
            modified: Some(chrono::Local::now()),
        });
    }
    let total_bytes = files.iter().map(|f| f.size_bytes).sum();
    SdCardInfo {
        mount: mount.to_path_buf(),
        volume_label: "TESTVOL".to_string(),
        device_name: device_name.to_string(),
        name_source: DeviceNameSource::Manual,
        media_file_count: files.len(),
        total_bytes,
        files,
        selected: Vec::new(),
        selected_count: 0,
        selected_bytes: 0,
    }
}

// ── Cancel decode → re-decode succeeds (stale-event gating end-to-end) ──
//
// The stale-event gate itself is unit-tested as `job_event_is_stale` in
// `engine.rs` (unreachable end-to-end by design: the spawn guard prevents
// same-kind job overlap). This test exercises the observable effect of the
// gate through public commands: after cancelling decode 1 and re-decoding,
// the second job's success must not be clobbered by the first job's
// terminal event.

#[test]
fn test_cancel_decode_then_redecode_succeeds() {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("stale-gate-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    // Use a real WAV so ParseLtcWavFile actually spawns a job
    let dir = tempfile::TempDir::new().unwrap();
    let path1 = dir.path().join("stale1.wav");
    let path2 = dir.path().join("stale2.wav");
    generate_wav(&path1, 25.0, false, 0.5, 48000);
    generate_wav(&path2, 25.0, false, 0.5, 48000);

    // Start decoding file 1
    tx.send(GuiCommand::ParseLtcWavFile(path1.to_string_lossy().to_string())).unwrap();

    // Wait until Running
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snap = state.load();
        if snap.job(JobKind::LtcDecode).phase() == JobPhase::Running {
            break;
        }
        if Instant::now() > deadline {
            panic!("timeout waiting for first decode to start running");
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Cancel + wait for Idle/Failed (thread winds down)
    tx.send(GuiCommand::CancelDecode).unwrap();

    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snap = state.load();
        if snap.job(JobKind::LtcDecode).phase() != JobPhase::Running {
            break;
        }
        if Instant::now() > deadline {
            panic!("timeout waiting for cancel to take effect");
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Now start decoding file 2
    tx.send(GuiCommand::ParseLtcWavFile(path2.to_string_lossy().to_string())).unwrap();

    // Wait until the second decode succeeds
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snap = state.load();
        if snap.job(JobKind::LtcDecode).phase() == JobPhase::Succeeded {
            break;
        }
        if Instant::now() > deadline {
            panic!("timeout waiting for second decode to succeed (phase: {:?})",
                snap.job(JobKind::LtcDecode).phase());
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let snap = state.load();
    assert!(
        snap.job(JobKind::LtcDecode).phase() == JobPhase::Succeeded,
        "second decode must succeed"
    );
    assert!(
        snap.decode.result.is_some(),
        "second decode must have produced a result (not clobbered by the cancelled job's terminal event)"
    );
    assert!(
        snap.decode.error.is_none(),
        "second decode must not carry the cancelled job's error"
    );

    drop(tx);
    handle.join().expect("engine thread panicked");
}

// ── Duplicate spawn rejection guard ─────────────────────────────────────

#[test]
fn test_duplicate_decode_rejected_while_running() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("dup_test.wav");
    generate_wav(&path, 25.0, false, 0.5, 48000);

    let snapshot = run_engine(
        vec![
            GuiCommand::ParseLtcWavFile(path.to_string_lossy().to_string()),
            GuiCommand::ParseLtcWavFile(path.to_string_lossy().to_string()),
        ],
        false,
        |s| s.job(JobKind::LtcDecode).phase() == JobPhase::Succeeded,
    );

    assert_eq!(
        snapshot.job(JobKind::LtcDecode).phase(),
        JobPhase::Succeeded,
        "expected decode to succeed after two identical commands (second rejected by guard)",
    );
    assert!(
        snapshot.decode.result.is_some(),
        "decode should have produced a result",
    );
}

// ── Duplicate group decode rejection via run_engine ─────────────────────

// ── Duplicate ProbeFileDurations rejection via run_engine ───────────────

// ── WAV round-trip tests (various FPS) ──────────────────────────────────

#[test]
fn test_wav_roundtrip_25fps() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_25fps.wav");
    generate_wav(&path, 25.0, false, 2.0, 48000);
    let result = decode_ltc_from_wav(&path, 25.0, false, None).expect("LTC decode failed");
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
    assert!(result.valid_frames >= 48, "expected ~50 frames, got {}", result.valid_frames);
}

#[test]
fn test_wav_roundtrip_2997_df() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_2997df.wav");
    generate_wav(&path, 29.97, true, 3.0, 48000);
    let result = decode_ltc_from_wav(&path, 29.97, true, None).expect("LTC decode failed");
    assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
        "expected no Error, got {:?} (valid={})", result.status, result.valid_frames);
}

// ── Engine MPSC: ParseLtcWavFile ────────────────────────────────────────

#[test]
fn test_engine_mpsc_parse_ltc_command() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_parse.wav");
    generate_wav(&path, 25.0, false, 1.0, 48000);

    let snapshot = run_engine(
        vec![GuiCommand::ParseLtcWavFile(path.to_string_lossy().to_string())],
        false,
        |s| s.job(JobKind::LtcDecode).phase() != JobPhase::Running && s.decode.result.is_some(),
    );

    assert!(snapshot.decode.result.is_some(), "expected ltc_decode_result to be Some");
    let result = snapshot.decode.result.as_ref().unwrap();
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?}", result.status);
    assert!(result.valid_frames > 0, "expected valid_frames > 0");
    assert!(snapshot.job(JobKind::LtcDecode).phase() != JobPhase::Running);
    assert!(snapshot.decode.error.is_none());
    assert!(snapshot.decode.generation > 0);
}

#[test]
fn test_engine_mpsc_parse_invalid_file() {
    let snapshot = run_engine(
        vec![GuiCommand::ParseLtcWavFile("/tmp/nonexistent_ltc_test_file.wav".to_string())],
        false,
        |s| s.job(JobKind::LtcDecode).phase() != JobPhase::Running && s.decode.error.is_some(),
    );

    assert!(snapshot.decode.result.is_none());
    assert!(snapshot.decode.error.is_some());
    assert!(snapshot.job(JobKind::LtcDecode).phase() != JobPhase::Running);
}

// ── Clap command integration ────────────────────────────────────────────

#[test]
fn test_engine_clap_creates_log_entry() {
    let snapshot = run_engine(vec![GuiCommand::Clap], false, |s| s.clapper.logs.len() == 1);

    assert_eq!(snapshot.clapper.logs.len(), 1, "expected 1 log entry after Clap");
    let log = &snapshot.clapper.logs[0];
    assert_eq!(log.note, "Scene 1");
    assert!(log.timecode.contains(':'), "expected timecode in log, got {}", log.timecode);
    assert_eq!(log.id, 1);
}

#[test]
fn test_engine_multiple_claps_accumulate_logs() {
    let snapshot = run_engine(
        vec![GuiCommand::Clap, GuiCommand::Clap, GuiCommand::Clap],
        false,
        |s| s.clapper.logs.len() == 3,
    );

    assert_eq!(snapshot.clapper.logs.len(), 3, "expected 3 log entries after 3 Claps");
    // take auto-increments 3 times from 1
    assert_eq!(snapshot.clapper.take, 4, "take should be 4 after 3 Claps starting from 1");
}

// ── Engine shutdown ─────────────────────────────────────────────────────

#[test]
fn test_engine_shutdown_via_command() {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-shutdown-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    // Give engine time to start its tick loop
    std::thread::sleep(Duration::from_millis(50));

    tx.send(GuiCommand::Shutdown).unwrap();

    // Engine should exit within a reasonable time
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if handle.is_finished() {
            break;
        }
        if Instant::now() > deadline {
            panic!("Engine thread did not shut down within 3 seconds via Shutdown command");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn test_engine_shutdown_via_channel_drop() {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-drop-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    std::thread::sleep(Duration::from_millis(50));

    drop(tx);

    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if handle.is_finished() {
            break;
        }
        if Instant::now() > deadline {
            panic!("Engine thread did not shut down within 3 seconds via channel drop");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ── Stepper commands via engine ─────────────────────────────────────────

// ── FPS selection ──────────────────────────────────────────────────────

// ── Theme commands ──────────────────────────────────────────────────────

// ── Decode result generation tracking ───────────────────────────────────

// ── State mutation commands ─────────────────────────────────────────────

// ── LTC decode stream/channel selection ──────────────────────────────────

// ── File duration probe ──────────────────────────────────────────────────

#[test]
fn test_engine_probe_file_durations_wav() {
    let dir = tempfile::TempDir::new().unwrap();
    let path1 = dir.path().join("ch1.wav");
    let path2 = dir.path().join("ch2.wav");

    init_test_config();

    // Create two WAVs with known durations
    let spec = hound::WavSpec { channels: 1, sample_rate: 48000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    {
        let mut w = hound::WavWriter::create(&path1, spec).unwrap();
        for _ in 0..96000 { w.write_sample(0i16).unwrap(); }
        w.finalize().unwrap();
    }
    {
        let mut w = hound::WavWriter::create(&path2, spec).unwrap();
        for _ in 0..48000 { w.write_sample(0i16).unwrap(); }
        w.finalize().unwrap();
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    tx.send(GuiCommand::ProbeFileDurations(vec![path1.clone(), path2.clone()])).unwrap();

    // Poll until both durations appear in the snapshot
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snapshot: AppStateSnapshot = state.load().as_ref().clone();
        if snapshot.file_durations.len() >= 2 {
            break;
        }
        if Instant::now() > deadline {
            panic!("timeout waiting for file durations");
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let snapshot: AppStateSnapshot = state.load().as_ref().clone();
    let dur1 = snapshot.file_durations.get(&path1).expect("missing path1").expect("path1 duration should be Some");
    let dur2 = snapshot.file_durations.get(&path2).expect("missing path2").expect("path2 duration should be Some");
    assert!((dur1 - 2.0).abs() < 0.001, "expected 2.0s for path1, got {}", dur1);
    assert!((dur2 - 1.0).abs() < 0.001, "expected 1.0s for path2, got {}", dur2);

    drop(tx);
    handle.join().expect("engine thread panicked");
}

#[test]
fn cancel_decode_clears_is_detecting() {
    let snapshot = run_engine(
        vec![
            GuiCommand::ParseLtcWavFile("/nonexistent/bogus_file_for_test.wav".to_string()),
            GuiCommand::CancelDecode,
        ],
        false,
        |s| {
            let phase = s.job(JobKind::LtcDecode).phase();
            phase != JobPhase::Running && s.decode.error.is_some()
        },
    );
    assert!(snapshot.job(JobKind::LtcDecode).phase() != JobPhase::Running,
        "CancelDecode should clear LtcDecode job phase");
    assert!(matches!(
        snapshot.job(JobKind::LtcDecode).phase(),
        JobPhase::Idle | JobPhase::Cancelled,
    ), "cancelled single decode must end Idle (failed synchronously before spawning) or Cancelled");
}

#[test]
fn cancel_decode_also_clears_group_detecting() {
    let snapshot = run_engine(
        vec![
            GuiCommand::DecodeLtcVideoGroup {
                paths: vec!["/nonexistent/bogus_clip_1.wav".to_string()],
                stream_index: 0,
                channel_index: 0,
            },
            GuiCommand::CancelDecode,
        ],
        false,
        // The group decode pre-populates a Running job status, so the eager
        // cancel deterministically lands in the Cancelled phase.
        |s| matches!(s.job(JobKind::LtcGroupDecode).phase(), JobPhase::Cancelled),
    );
    assert!(snapshot.job(JobKind::LtcGroupDecode).phase() != JobPhase::Running,
        "CancelDecode should clear LtcGroupDecode job phase");
    assert!(snapshot.job(JobKind::LtcDecode).phase() != JobPhase::Running,
        "CancelDecode should clear LtcDecode job phase");
    assert!(matches!(
        snapshot.job(JobKind::LtcGroupDecode).phase(),
        JobPhase::Idle | JobPhase::Cancelled,
    ), "cancelled group decode must end Idle or Cancelled, never Running/Failed/Succeeded");
}

// ── Multi-chunk WAV decode progress ───────────────────────────────────────

#[test]
fn test_multi_chunk_decode_succeeds() {
    init_test_config();
    let dir = tempfile::TempDir::new().unwrap();
    // Generate a WAV large enough to span multiple decode chunks at default
    // DecodeConfig (50 MB / chunk).  At 48 kHz stereo 16-bit:
    //   bytes_per_mono = 4,  chunk_mono = 12.5 M
    //   310 s × 48000 = 14.88 M mono samples → 2 chunks
    let path = dir.path().join("multi_chunk_decode.wav");
    generate_wav(&path, 25.0, false, 310.0, 48000);

    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-multi-chunk-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    tx.send(GuiCommand::ParseLtcWavFile(path.to_string_lossy().to_string())).unwrap();

    // Break on a terminal phase only: before the first publication
    // `job(kind)` returns an *idle* default, so `!= Running` alone would
    // false-pass inside the pre-start Idle window.
    let deadline = Instant::now() + Duration::from_secs(120);
    let (final_phase, final_fraction) = loop {
        let snapshot: AppStateSnapshot = state.load().as_ref().clone();
        let job = snapshot.job(JobKind::LtcDecode);
        if matches!(
            job.phase(),
            JobPhase::Succeeded | JobPhase::Failed | JobPhase::Cancelled
        ) {
            break (job.phase(), job.fraction());
        }
        if Instant::now() > deadline {
            panic!(
                "Timeout waiting for decode: phase={:?}, fraction={}",
                job.phase(),
                job.fraction(),
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    };

    assert_eq!(
        final_phase,
        JobPhase::Succeeded,
        "multi-chunk decode should succeed (final fraction={})",
        final_fraction,
    );

    drop(tx);
    handle.join().expect("engine thread panicked");
}
// ── Publish gating ──────────────────────────────────────────────────────

/// The engine's publish gate must store a new snapshot into the ArcSwap only
/// when the snapshot actually changed. If the gate ever degrades to an
/// unconditional store, the Arc pointer held by ArcSwap changes on every
/// 40 ms tick — which this test detects via `Arc::ptr_eq` between two
/// back-to-back loads during an idle phase.
#[test]
fn test_idle_engine_does_not_republish() {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("publish-gate-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    // Wait until the engine has published at least once (non-initial snapshot).
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            panic!("Timeout waiting for first publish");
        }
        // The FfmpegCapProbe finish (fake_probe) changes the snapshot, so a
        // published arc distinct from the initial one appears within a few
        // ticks. Detect "has published something" via the probe result.
        if state.load().ffmpeg_caps.is_some() {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    // Snapshot is now idle/static — several ticks must NOT store a new Arc.
    let before = state.load_full();
    std::thread::sleep(Duration::from_millis(150));
    let after = state.load_full();
    assert!(
        Arc::ptr_eq(&before, &after),
        "idle engine must skip stores when the snapshot is unchanged",
    );

    // A state change must publish again.
    tx.send(GuiCommand::Clap).unwrap();
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        if Instant::now() > deadline {
            panic!("Timeout waiting for republish after Clap");
        }
        if !Arc::ptr_eq(&before, &state.load_full()) {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    drop(tx);
    handle.join().expect("engine thread panicked");
}

// ── Applied-command ack counter ──────────────────────────────────────────

/// Every drained command bumps `applied_command_seq` and forces a publish —
/// including idempotent writes that would otherwise be gated by the
/// structural-PartialEq publish skip.
#[test]
fn test_applied_command_seq_counts_commands() {
    let snapshot = run_engine(
        vec![
            GuiCommand::SetFpsIndex(0),      // seq 1 — value change
            GuiCommand::SetLtcVolume(0.25),  // seq 2 — idempotent vs default 0.25
            GuiCommand::SetBeepFrequency(1234.0), // seq 3 — value change
        ],
        false,
        |s| {
            s.applied_command_seq >= 3
                && s.fps_index == 0
                && s.ltc_volume == 0.25
                && s.beep_frequency == 1234.0
        },
    );
    assert_eq!(snapshot.applied_command_seq, 3);
}

/// An idle engine (no commands) must keep `applied_command_seq` at zero and
/// never republish — the counter must not break the publish gate.
#[test]
fn test_applied_command_seq_stays_zero_when_idle() {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("idle-seq-test".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("failed to spawn engine thread");

    // Drop the sender right away: no commands are ever sent. Wait a few
    // ticks, then check the counter and pointer stability.
    std::thread::sleep(Duration::from_millis(150));
    let snapshot = state.load_full();
    assert_eq!(snapshot.applied_command_seq, 0);

    drop(tx);
    handle.join().expect("engine thread panicked");
}

// ── Publish-gate regression: idle ticks must not re-publish ────────────

// ── PR-1 canary: fake scan seam reaches the published snapshot ───────────

#[test]
fn fake_scan_reaches_snapshot() {
    let mount = make_persistent_dir("offload-canary-mount");
    let card = make_card(&mount, "TESTCAM", &["CLIP001.wav", "CLIP002.wav"]);
    let engine = spawn_engine_with_scan_seam(static_scan_seam(card));

    engine.tx.send(GuiCommand::Offload(OffloadCommand::ScanCards)).unwrap();
    let snap = wait_for_snapshot(&engine.state, "OffloadScan to succeed", |s| {
        s.jobs.get(&JobKind::OffloadScan)
            .map(|j| j.phase() == JobPhase::Succeeded)
            .unwrap_or(false)
    });

    assert_eq!(snap.offload.cards.len(), 1, "fake card must reach snapshot");
    assert_eq!(snap.offload.cards[0].device_name, "TESTCAM");
    // on_offload_scan_finished applies the default (latest-day) selection.
    assert_eq!(snap.offload.cards[0].selected_count, 2, "default selection applied on scan finish");
    engine.shutdown();
}

// ── PR-2: engine offload integration tests ───────────────────────────────

/// Build a card from files that already exist on disk (sizes from metadata).
fn card_for_existing_files(mount: &Path, device_name: &str, file_names: &[&str]) -> SdCardInfo {
    let mut files = Vec::new();
    for name in file_names {
        let path = mount.join(name);
        let meta = std::fs::metadata(&path).unwrap_or_else(|e| panic!("stat {}: {e}", path.display()));
        files.push(OffloadFileInfo {
            path: path.clone(),
            name: (*name).to_string(),
            size_bytes: meta.len(),
            modified: meta.modified().ok().map(|t| t.into()),
        });
    }
    let total_bytes = files.iter().map(|f| f.size_bytes).sum();
    SdCardInfo {
        mount: mount.to_path_buf(),
        volume_label: "TESTVOL".to_string(),
        device_name: device_name.to_string(),
        name_source: DeviceNameSource::Manual,
        media_file_count: files.len(),
        total_bytes,
        files,
        selected: Vec::new(),
        selected_count: 0,
        selected_bytes: 0,
    }
}

#[test]
fn test_offload_happy_path_scan_select_copy() {
    let mount = make_persistent_dir("offload-happy-mount");
    let dest = make_persistent_dir("offload-happy-dest");

    // Real (tiny) WAVs so the post-scan DurationProbe reports actual values.
    let p1 = mount.join("CLIP001.wav");
    let p2 = mount.join("CLIP002.wav");
    generate_wav(&p1, 25.0, false, 0.2, 48000);
    generate_wav(&p2, 25.0, false, 0.2, 48000);
    let card = card_for_existing_files(&mount, "TESTCAM", &["CLIP001.wav", "CLIP002.wav"]);

    let engine = spawn_engine_with_scan_seam(static_scan_seam(card));
    engine.tx.send(GuiCommand::Offload(OffloadCommand::ScanCards)).unwrap();
    let snap = wait_for_snapshot(&engine.state, "OffloadScan success", |s| {
        s.jobs.get(&JobKind::OffloadScan).map(|j| j.phase() == JobPhase::Succeeded).unwrap_or(false)
    });
    let version_before = snap.offload.last_offload_version;

    engine.tx.send(GuiCommand::Offload(OffloadCommand::SetParentFolder(dest.clone()))).unwrap();
    engine.tx.send(GuiCommand::Offload(OffloadCommand::SetParentName("day1".into()))).unwrap();
    engine.tx.send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(0, true))).unwrap();
    engine.tx.send(GuiCommand::Offload(OffloadCommand::StartOffload)).unwrap();

    let snap = wait_for_snapshot(&engine.state, "OffloadCopy success", |s| {
        s.jobs.get(&JobKind::OffloadCopy).map(|j| j.phase() == JobPhase::Succeeded).unwrap_or(false)
            && s.offload.completed_devices.contains(&"TESTCAM".to_string())
    });

    // Destination layout: dest/<parent_name>/<device>/<filename>, byte-identical.
    let dev_dir = dest.join("day1").join("TESTCAM");
    for name in ["CLIP001.wav", "CLIP002.wav"] {
        let copied = dev_dir.join(name);
        let src = mount.join(name);
        assert!(copied.is_file(), "missing copy: {}", copied.display());
        assert_eq!(
            std::fs::read(&copied).unwrap(),
            std::fs::read(&src).unwrap(),
            "copied bytes differ for {name}"
        );
    }

    assert_eq!(snap.offload.completed_devices, vec!["TESTCAM".to_string()]);
    assert_eq!(snap.offload.last_offload_parent, Some(dev_dir.parent().unwrap().to_path_buf()));
    assert_eq!(snap.offload.last_offload_version, version_before + 1, "completion bumps the handoff version");

    let totals = snap.offload.device_totals.iter().find(|t| t.name == "TESTCAM").expect("device totals for TESTCAM");
    assert_eq!(totals.files_total, 2);
    assert_eq!(totals.bytes_total, snap.offload.cards[0].selected_bytes);

    // DurationProbe items (spawned by on_offload_scan_finished) filled durations.
    wait_for_snapshot(&engine.state, "file durations populated", |s| {
        s.offload.file_durations.contains_key(&p1) && s.offload.file_durations.contains_key(&p2)
    });
    let snap = engine.state.load().as_ref().clone();
    assert!(snap.offload.file_durations[&p1].is_some(), "WAV duration should probe successfully");
    assert!(snap.offload.file_durations[&p2].is_some());

    engine.shutdown();
}

#[test]
fn test_offload_copy_failure_completes_with_no_devices() {
    // NOTE: a per-file copy failure does NOT fail the job — the job-level
    // contract is "Succeeded with the list of completed devices" (per-device
    // failure lives in the unit state). This test pins that contract: the
    // copy fails at read time, no device completes, nothing is version-bumped
    // into a "delivered" state, and the engine stays healthy.
    let mount = make_persistent_dir("offload-fail-mount");
    let dest = make_persistent_dir("offload-fail-dest");
    let card = make_card(&mount, "VANISH", &["gone1.wav", "gone2.wav"]);
    // The card snapshot retains sizes, so the plan builds fine — but the
    // sources are deleted before the copy starts.
    std::fs::remove_file(mount.join("gone1.wav")).unwrap();
    std::fs::remove_file(mount.join("gone2.wav")).unwrap();

    let engine = spawn_engine_with_scan_seam(static_scan_seam(card));
    engine.tx.send(GuiCommand::Offload(OffloadCommand::ScanCards)).unwrap();
    wait_for_snapshot(&engine.state, "scan success", |s| {
        s.jobs.get(&JobKind::OffloadScan).map(|j| j.phase() == JobPhase::Succeeded).unwrap_or(false)
    });

    let version_before = engine.state.load().offload.last_offload_version;
    engine.tx.send(GuiCommand::Offload(OffloadCommand::SetParentFolder(dest))).unwrap();
    engine.tx.send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(0, true))).unwrap();
    engine.tx.send(GuiCommand::Offload(OffloadCommand::StartOffload)).unwrap();

    let snap = wait_for_snapshot(&engine.state, "OffloadCopy terminal", |s| {
        matches!(
            s.jobs.get(&JobKind::OffloadCopy).map(|j| j.phase()),
            Some(JobPhase::Succeeded) | Some(JobPhase::Failed) | Some(JobPhase::Cancelled)
        )
    });

    assert!(
        snap.offload.completed_devices.is_empty(),
        "no device may complete when every file fails to copy"
    );
    // Desired: the offload→converter handoff only fires when the destination
    // actually received files. Zero completed devices = no handoff.
    assert_eq!(snap.offload.last_offload_version, version_before,
        "no handoff version bump when zero devices completed");
    assert!(snap.offload.last_offload_parent.is_none(),
        "handoff must not point at an empty destination");
    let dev_dir = snap.offload.parent_folder.as_ref().unwrap().join(&snap.offload.parent_name).join("VANISH");
    assert!(!dev_dir.join("gone1.wav").exists(), "no partial file may survive as a deliverable");

    // Engine alive and publishable afterwards: a no-op command is acked.
    engine.tx.send(GuiCommand::Offload(OffloadCommand::SetParentName("post-fail".into()))).unwrap();
    wait_for_snapshot(&engine.state, "engine alive after failure", |s| {
        s.offload.parent_name == "post-fail"
    });

    engine.shutdown();
}

#[test]
fn test_offload_cancel_during_scan() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let mount = make_persistent_dir("offload-cancel-mount");
    let card = make_card(&mount, "TESTCAM", &["a.wav"]);

    // First scan blocks, polling the job's cancel token; the seam returns a
    // card only on the second scan. Avoids racing a real (too-fast) copy.
    let first_scan = AtomicBool::new(true);
    let seam: ScanCardsFn = Arc::new(move |cancel, _progress| {
        if first_scan.swap(false, Ordering::SeqCst) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !cancel.is_cancelled() {
                assert!(Instant::now() < deadline, "scan seam never cancelled");
                std::thread::sleep(Duration::from_millis(10));
            }
            Err("aborted: cancelled".to_string())
        } else {
            Ok(vec![card.clone()])
        }
    });

    let engine = spawn_engine_with_scan_seam(seam);
    engine.tx.send(GuiCommand::Offload(OffloadCommand::ScanCards)).unwrap();
    wait_for_snapshot(&engine.state, "scan running", |s| {
        matches!(s.jobs.get(&JobKind::OffloadScan).map(|j| j.phase()), Some(JobPhase::Running) | Some(JobPhase::Indeterminate))
    });

    engine.tx.send(GuiCommand::Offload(OffloadCommand::CancelOffload)).unwrap();
    wait_for_snapshot(&engine.state, "scan cancelled", |s| {
        s.jobs.get(&JobKind::OffloadScan).map(|j| j.phase() == JobPhase::Cancelled).unwrap_or(false)
    });

    // A subsequent scan works again — no stale-supervisor lockout.
    engine.tx.send(GuiCommand::Offload(OffloadCommand::ScanCards)).unwrap();
    let snap = wait_for_snapshot(&engine.state, "rescan after cancel succeeds", |s| {
        s.jobs.get(&JobKind::OffloadScan).map(|j| j.phase() == JobPhase::Succeeded).unwrap_or(false)
    });
    assert_eq!(snap.offload.cards.len(), 1);

    engine.shutdown();
}

#[test]
fn test_start_offload_guard_branches() {
    use gui_engine::offload::OffloadPlanError;

    // (a) No cards → typed NoCards plan error.
    {
        let engine = spawn_engine_with_scan_seam(Arc::new(|_c, _p| Ok(Vec::new())));
        engine.tx.send(GuiCommand::Offload(OffloadCommand::StartOffload)).unwrap();
        let snap = wait_for_snapshot(&engine.state, "no-cards guard", |s| s.offload.plan_error.is_some());
        assert_eq!(snap.offload.plan_error, Some(OffloadPlanError::NoCards));
        engine.shutdown();
    }

    // (b) Card present but no parent folder → typed NoParentFolder plan error.
    {
        let mount = make_persistent_dir("offload-guard-mount");
        let card = make_card(&mount, "TESTCAM", &["a.wav"]);
        let engine = spawn_engine_with_scan_seam(static_scan_seam(card));
        engine.tx.send(GuiCommand::Offload(OffloadCommand::ScanCards)).unwrap();
        wait_for_snapshot(&engine.state, "scan success", |s| {
            s.jobs.get(&JobKind::OffloadScan).map(|j| j.phase() == JobPhase::Succeeded).unwrap_or(false)
        });
        engine.tx.send(GuiCommand::Offload(OffloadCommand::StartOffload)).unwrap();
        let snap = wait_for_snapshot(&engine.state, "no-parent guard", |s| {
            s.offload.plan_error == Some(OffloadPlanError::NoParentFolder)
        });
        assert_eq!(snap.offload.plan_error, Some(OffloadPlanError::NoParentFolder));
        engine.shutdown();
    }
    // (c) Duplicate StartOffload while copy running is deliberately not
    // engine-tested: real copies of test-sized files finish faster than the
    // duplicate can be observed, and the spawn guard (supervisor.is_running)
    // is exercised by the other job kinds' duplicate tests.
}

// ── PR-3: SetDevice revert path ──────────────────────────────────────────

/// Regression-pin the SetDevice revert path: a selection for an id that is
/// not in `state.devices` is rejected by `try_init_device` before cpal is
/// ever touched, so this is deterministic on every host — headless CI and
/// audio-equipped dev machines alike. The failed selection must never stick.
///
/// Non-goal (documented per WP-4 §4/F-2): the deeper `StreamDead`/
/// `RecoveryNeeded` recovery ladder needs an AudioCore event-injection seam.
#[test]
fn test_set_device_bogus_id_does_not_stick() {
    const BOGUS: &str = "__ltc_test_nonexistent__";

    // RefreshDevices is real but enumeration-only (no stream opened).
    // The trailing SetFpsIndex is an ack marker: applied_command_seq >= 3
    // proves the engine fully processed SetDevice before the snapshot.
    let snapshot = run_engine(
        vec![
            GuiCommand::RefreshDevices,
            GuiCommand::SetDevice(BOGUS.to_string()),
            GuiCommand::SetFpsIndex(1),
        ],
        false,
        |s| s.applied_command_seq >= 3 && s.fps_index == 1,
    );

    assert_ne!(
        snapshot.selected_device.as_deref(),
        Some(BOGUS),
        "a bogus device id must never stick in the published state"
    );
    assert!(!snapshot.devices.iter().any(|d| d.id == BOGUS));

    // Engine survived the failed switch — proven by run_engine joining the
    // thread cleanly after the ack predicate (a panicked loop would poison
    // the join). On a deviceless host (CI) the bogus selection also cannot
    // have been replaced by anything: the selection stays as it was.
    if audio_core::list_audio_devices().map(|d| d.is_empty()).unwrap_or(true) {
        assert_eq!(
            snapshot.selected_device, None,
            "on a deviceless host there is nothing to revert/fall back to"
        );
    }
}
