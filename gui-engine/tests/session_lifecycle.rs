//! Recording-session lifecycle integration tests (Phase 3, session
//! foundation). Covers: create → stop → engine restart → load (the
//! "two-day" continuation), the continue gap, the offload destination
//! link (parent-name default + destination capture), and per-file
//! convert selection command routing through the engine.
//!
//! Poll-with-deadline everywhere; the periodic last-emitted persist is
//! exercised in the engine unit tests via the tick counter (no
//! wall-clock assertions here).

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use gui_engine::command::{ConverterCommand, GuiCommand, OffloadCommand, SessionCommand};
use gui_engine::engine::{engine_main_with_seams, EngineSeams, OffloadCopyFn, ScanCardsFn};
use gui_engine::job::{JobError, JobFinal, JobKind, JobPhase};
use gui_engine::offload::{DeviceNameSource, OffloadFileInfo, SdCardInfo};
use gui_engine::state::AppStateSnapshot;
use gui_engine::{FfmpegCapabilities, HwDeviceCapabilities, Timecode};
use std::collections::BTreeSet;

const POLL_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

// ── Harness ──────────────────────────────────────────────────────────────

/// Serialize tests that touch the shared per-binary config home (session
/// files and offload-parent config both persist there; under nextest each
/// test runs in its own process, the lock only closes the `cargo test`
/// window — same pattern as integration.rs's offload config lock).
static CONFIG_LOCK: Mutex<()> = Mutex::new(());

fn take_config_lock() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ensure the test does not write to the real user config directory.
fn init_test_config() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = tempfile::TempDir::new().expect("tempdir for test config");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        // Windows ignores XDG_CONFIG_HOME (dirs uses SHGetKnownFolderPath),
        // so tests isolate config state there via this override instead.
        std::env::set_var("LTC_CONFIG_HOME", dir.path());
        let _ = Box::leak(Box::new(dir));
    });
}

fn fake_probe() -> FfmpegCapabilities {
    FfmpegCapabilities {
        has_ffmpeg: false,
        available_encoders: BTreeSet::new(),
        available_formats: BTreeSet::new(),
        error_message: None,
        ffmpeg_version: None,
        hw: HwDeviceCapabilities::default(),
    }
}

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

fn spawn_engine(scan_cards: ScanCardsFn, offload_copy: OffloadCopyFn) -> TestEngine {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);
    let (event_tx, _event_rx) = mpsc::channel();
    let handle = std::thread::Builder::new()
        .name("gui-engine-session-test".into())
        .spawn(move || {
            let seams = EngineSeams {
                ffmpeg_caps: Box::new(fake_probe),
                scan_cards,
                offload_copy,
                ..EngineSeams::default()
            };
            engine_main_with_seams(rx, state_clone, false, event_tx, seams);
        })
        .expect("failed to spawn engine thread");
    TestEngine { tx, state, handle }
}

fn wait_for_snapshot<F>(
    state: &Arc<ArcSwap<AppStateSnapshot>>,
    what: &str,
    f: F,
) -> AppStateSnapshot
where
    F: Fn(&AppStateSnapshot) -> bool,
{
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snap = state.load().as_ref().clone();
        if f(&snap) {
            return snap;
        }
        if Instant::now() > deadline {
            panic!(
                "Timeout waiting for {what}; session={:?} playing={} status={}",
                snap.session,
                snap.is_playing,
                snap.status.message()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn make_persistent_dir(tag: &str) -> PathBuf {
    let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir ({tag}): {e}"));
    let path = dir.path().to_path_buf();
    Box::leak(Box::new(dir));
    path
}

/// Field-wise tuple comparison — Timecode has no `PartialOrd` impl and a
/// short test never wraps midnight (same convention as integration.rs).
fn tc_ge(a: Timecode, b: Timecode) -> bool {
    (a.hours, a.minutes, a.seconds, a.frames) >= (b.hours, b.minutes, b.seconds, b.frames)
}

fn tc_gt(a: Timecode, b: Timecode) -> bool {
    (a.hours, a.minutes, a.seconds, a.frames) > (b.hours, b.minutes, b.seconds, b.frames)
}

fn audio_available() -> bool {
    audio_core::list_audio_devices()
        .map(|d| !d.is_empty())
        .unwrap_or(false)
}

fn make_card(mount: &Path, device_name: &str, file_names: &[&str]) -> SdCardInfo {
    let mut files = Vec::new();
    for name in file_names {
        let path = mount.join(name);
        let contents = format!("fake-wav-payload:{name}").into_bytes();
        std::fs::write(&path, &contents).expect("write card file");
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

/// Immediately-succeeding fake copy runner (no real files copied; the
/// destination capture only needs the completed-device payload).
fn fake_copy_runner(device: &str) -> OffloadCopyFn {
    let device = device.to_string();
    Arc::new(move |_ctx, _plans, _names, _dest| {
        Ok(JobFinal::OffloadCopy {
            completed_devices: vec![device.clone()],
        })
    })
}

// ── Session lifecycle ────────────────────────────────────────────────────

/// Two-"day" simulation (acceptance criterion): day 1 — create a session,
/// (on audio-equipped machines) actually emit some timecode and stop; the
/// engine restarts (day 2) and loading the session resumes generation at
/// the persisted last-emitted TC plus the continue gap, with the
/// session's fps/drop-frame applied.
#[test]
fn session_restart_resumes_at_last_emitted_plus_gap() {
    let _lock = take_config_lock();
    init_test_config();

    // ── Day 1: create the session, record some timecode, shut down. ──
    let eng1 = spawn_engine(
        Arc::new(|_c, _p| Ok(Vec::new())),
        Arc::new(|_c, _p, _n, _d| Err(JobError::Cancelled)),
    );
    eng1.tx
        .send(GuiCommand::Session(SessionCommand::Create {
            name: "Forest Shoot".into(),
        }))
        .unwrap();
    let day1 = wait_for_snapshot(&eng1.state, "session created", |s| s.session.is_some());
    let view = day1.session.clone().expect("active session");
    assert_eq!(
        day1.offload.parent_name, view.id,
        "a session-backed offload run defaults its parent name to the session id"
    );

    let mut day1_emitted: Option<Timecode> = None;
    if audio_available() {
        eng1.tx.send(GuiCommand::StartLtc).unwrap();
        let playing = wait_for_snapshot(&eng1.state, "playback to start", |s| s.is_playing);
        let start = playing.start_timecode;
        // Floor: at least one second of LTC emitted (no ceilings — the
        // emission keeps running while we wait).
        wait_for_snapshot(&eng1.state, "timecode to advance", |s| {
            tc_gt(s.current_timecode, start)
        });
        eng1.tx.send(GuiCommand::StopLtc).unwrap();
        let stopped = wait_for_snapshot(&eng1.state, "playback to stop", |s| !s.is_playing);
        day1_emitted = Some(stopped.current_timecode);
    } else {
        eprintln!(
            "--- SKIPPED: real emission on day 1 needs an audio output device; \
             exercising the create → restart → load continuation with no emitted progress"
        );
    }
    eng1.shutdown();

    // The day-1 stop must have persisted the emitted TC (floor: emitted
    // strictly past the start TC when playback actually ran).
    let sessions = gui_engine::config::load_sessions();
    let record = sessions
        .iter()
        .find(|r| r.id == view.id)
        .expect("session file persisted on day 1")
        .clone();
    if let Some(emitted) = day1_emitted {
        assert!(
            tc_gt(record.last_emitted_timecode, record.start_timecode),
            "day-1 stop must stamp emitted progress past the start TC"
        );
        assert!(
            tc_ge(record.last_emitted_timecode, emitted),
            "persisted emitted TC must not go backwards"
        );
    } else {
        assert_eq!(
            record.last_emitted_timecode, record.start_timecode,
            "without playback the emitted TC stays at the start TC"
        );
    }

    // ── Day 2: a fresh engine (restart) loads the session. ──
    let eng2 = spawn_engine(
        Arc::new(|_c, _p| Ok(Vec::new())),
        Arc::new(|_c, _p, _n, _d| Err(JobError::Cancelled)),
    );
    eng2.tx
        .send(GuiCommand::Session(SessionCommand::Load {
            id: view.id.clone(),
        }))
        .unwrap();
    let day2 = wait_for_snapshot(&eng2.state, "session loaded", |s| s.session.is_some());
    let expected_resume = gui_engine::timecode::advance_timecode_by_seconds(
        record.last_emitted_timecode,
        record.continue_gap_seconds,
        record.fps,
        record.drop_frame,
    );
    assert_eq!(
        day2.start_timecode, expected_resume,
        "loading must resume at last emitted + gap"
    );
    assert_eq!(
        day2.current_timecode, expected_resume,
        "the resume position follows the loaded start TC (Q4)"
    );
    // fps/drop-frame come from the session, not the day-2 defaults.
    assert_eq!(day2.fps(), record.fps);
    assert_eq!(day2.drop_frame(), record.drop_frame);
    assert_eq!(
        day2.session.as_ref().expect("active").id,
        record.id,
        "the loaded session must be the active one"
    );
    eng2.shutdown();
}

/// Continue-gap reconfiguration: the gap set on the active session is
/// persisted and used by a subsequent load.
#[test]
fn session_gap_reconfiguration_survives_restart() {
    let _lock = take_config_lock();
    init_test_config();

    let eng1 = spawn_engine(
        Arc::new(|_c, _p| Ok(Vec::new())),
        Arc::new(|_c, _p, _n, _d| Err(JobError::Cancelled)),
    );
    eng1.tx
        .send(GuiCommand::Session(SessionCommand::Create {
            name: "Gap Shoot".into(),
        }))
        .unwrap();
    let day1 = wait_for_snapshot(&eng1.state, "session created", |s| s.session.is_some());
    let view = day1.session.clone().expect("active");
    eng1.tx
        .send(GuiCommand::Session(SessionCommand::SetContinueGapSeconds(
            120,
        )))
        .unwrap();
    wait_for_snapshot(&eng1.state, "gap applied", |s| {
        s.session
            .as_ref()
            .map(|v| v.continue_gap_seconds == 120)
            .unwrap_or(false)
    });
    eng1.shutdown();

    let eng2 = spawn_engine(
        Arc::new(|_c, _p| Ok(Vec::new())),
        Arc::new(|_c, _p, _n, _d| Err(JobError::Cancelled)),
    );
    eng2.tx
        .send(GuiCommand::Session(SessionCommand::Load {
            id: view.id.clone(),
        }))
        .unwrap();
    let day2 = wait_for_snapshot(&eng2.state, "session loaded", |s| s.session.is_some());
    let record = gui_engine::config::load_sessions()
        .into_iter()
        .find(|r| r.id == view.id)
        .expect("session file");
    assert_eq!(
        record.continue_gap_seconds, 120,
        "gap must persist across restart"
    );
    let expected = gui_engine::timecode::advance_timecode_by_seconds(
        record.last_emitted_timecode,
        120,
        record.fps,
        record.drop_frame,
    );
    assert_eq!(
        day2.start_timecode, expected,
        "the 120 s gap drives the resume TC"
    );
    eng2.shutdown();
}

// ── Offload link ─────────────────────────────────────────────────────────

/// While a session is active: the offload parent name defaults to the
/// session id, and a completed offload (≥1 device) appends the
/// destination to the session's persisted `offload_destinations`.
#[test]
fn session_captures_offload_destination_on_completed_run() {
    let _lock = take_config_lock();
    init_test_config();

    let mount = make_persistent_dir("session-offload-mount");
    let dest = make_persistent_dir("session-offload-dest");
    let card = make_card(&mount, "TESTCAM", &["a.wav"]);

    let eng = spawn_engine(
        Arc::new(move |_c, _p| Ok(vec![card.clone()])),
        fake_copy_runner("TESTCAM"),
    );

    // Create the session first — the offload tab must default to it.
    eng.tx
        .send(GuiCommand::Session(SessionCommand::Create {
            name: "Dest Shoot".into(),
        }))
        .unwrap();
    let with_session = wait_for_snapshot(&eng.state, "session created", |s| s.session.is_some());
    let view = with_session.session.clone().expect("active");
    assert_eq!(with_session.offload.parent_name, view.id);

    // Run an offload: scan → parent folder → start (parent name stays at
    // the session default; no SetParentName override).
    eng.tx
        .send(GuiCommand::Offload(OffloadCommand::ScanCards))
        .unwrap();
    wait_for_snapshot(&eng.state, "scan success", |s| {
        s.jobs
            .get(&JobKind::OffloadScan)
            .map(|j| j.phase() == JobPhase::Succeeded)
            .unwrap_or(false)
    });
    eng.tx
        .send(GuiCommand::Offload(OffloadCommand::SetParentFolder(
            dest.clone(),
        )))
        .unwrap();
    eng.tx
        .send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(
            0, true,
        )))
        .unwrap();
    eng.tx
        .send(GuiCommand::Offload(OffloadCommand::StartOffload))
        .unwrap();
    wait_for_snapshot(&eng.state, "offload run succeeded", |s| {
        s.jobs
            .get(&JobKind::OffloadCopy)
            .map(|j| j.phase() == JobPhase::Succeeded)
            .unwrap_or(false)
            && s.offload.run_summary.is_some()
    });

    let expected_dest = dest.join(&view.id).to_string_lossy().to_string();
    let record = gui_engine::config::load_sessions()
        .into_iter()
        .find(|r| r.id == view.id)
        .expect("session file");
    assert!(
        record.offload_destinations.contains(&expected_dest),
        "a completed offload must append its destination to the session \
         (destinations: {:?})",
        record.offload_destinations
    );
    eng.shutdown();
}

// ── Per-file convert selection ───────────────────────────────────────────

/// Engine routing for per-file selection: a deselected file leaves the
/// snapshot's selection set, the empty selection engages the
/// NoFilesSelected blocker, and Select All restores the default.
#[test]
fn per_file_selection_routes_through_the_engine_and_blocks_empty() {
    let _lock = take_config_lock();
    init_test_config();

    let eng = spawn_engine(
        Arc::new(|_c, _p| Ok(Vec::new())),
        Arc::new(|_c, _p, _n, _d| Err(JobError::Cancelled)),
    );

    // Plant a folder with three files matching the TASCAM pattern.
    let folder = make_persistent_dir("session-sel-folder");
    let names = ["T_S01.wav", "T_S02.wav", "T_S03.wav"];
    for n in names {
        std::fs::write(folder.join(n), b"fake wav").unwrap();
    }
    eng.tx
        .send(GuiCommand::Converter(ConverterCommand::SelectFolder(
            folder.clone(),
        )))
        .unwrap();
    eng.tx
        .send(GuiCommand::Converter(ConverterCommand::SelectRecording(0)))
        .unwrap();
    let selected = wait_for_snapshot(&eng.state, "recording selected", |s| {
        s.converter.selected_group_idx.is_some() && !s.converter.groups.is_empty()
    });
    let group_files = selected.converter.groups[0].files.clone();
    assert_eq!(group_files.len(), 3, "fixture group must hold three files");

    // Deselect the second file.
    eng.tx
        .send(GuiCommand::Converter(ConverterCommand::SetFileSelected {
            group: 0,
            path: group_files[1].clone(),
            selected: false,
        }))
        .unwrap();
    let after_deselect = wait_for_snapshot(&eng.state, "file deselected", |s| {
        s.converter
            .group_file_selection
            .get(&0)
            .map(|sel| sel.contains(&group_files[1]))
            == Some(false)
    });
    let sel = after_deselect
        .converter
        .group_file_selection
        .get(&0)
        .expect("selection set present");
    assert!(sel.contains(&group_files[0]) && sel.contains(&group_files[2]));
    assert!(
        !after_deselect
            .converter
            .readiness
            .iter()
            .any(|b| matches!(b, gui_engine::ConvertBlocker::NoFilesSelected)),
        "a partial selection must not block"
    );

    // Deselect everything → NoFilesSelected blocker engages.
    eng.tx
        .send(GuiCommand::Converter(ConverterCommand::SetAllGroupFiles {
            group: 0,
            selected: false,
        }))
        .unwrap();
    let empty = wait_for_snapshot(&eng.state, "empty selection blocker", |s| {
        s.converter
            .readiness
            .iter()
            .any(|b| matches!(b, gui_engine::ConvertBlocker::NoFilesSelected))
    });
    assert!(
        empty
            .converter
            .group_file_selection
            .get(&0)
            .map(|s| s.is_empty())
            .unwrap_or(false),
        "deselect-all must produce an empty selection set"
    );

    // Select All → back to the default (no selection entry at all).
    eng.tx
        .send(GuiCommand::Converter(ConverterCommand::SetAllGroupFiles {
            group: 0,
            selected: true,
        }))
        .unwrap();
    let restored = wait_for_snapshot(&eng.state, "selection restored", |s| {
        !s.converter.group_file_selection.contains_key(&0)
            && !s
                .converter
                .readiness
                .iter()
                .any(|b| matches!(b, gui_engine::ConvertBlocker::NoFilesSelected))
    });
    assert!(
        !restored.converter.group_file_selection.contains_key(&0),
        "select-all normalizes back to the no-entry default"
    );
    eng.shutdown();
}
