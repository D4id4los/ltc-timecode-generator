//! Session default conversion integration tests (phase 4): synthetic
//! session folders (TASCAM `_S<n>.wav` set + camera clips generated with
//! ffmpeg, `converter_integration.rs` / `video_extraction.rs` precedent)
//! driven through the real preflight/conversion runners and the engine
//! command path.
//!
//! Real-ffmpeg tests loud-skip (same convention as `video_extraction.rs`).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use gui_engine::command::{GuiCommand, SessionCommand, SessionConversionCommand};
use gui_engine::engine::engine_main_with_probe;
use gui_engine::job::{JobFinal, JobKind, JobPhase};
use gui_engine::session_conversion::{
    collect_preflight_groups, preflight_decode_real, run_session_conversion_with,
    run_session_preflight_with, session_group_runner_real, ChannelSource, PreflightDecodeRequest,
    PreflightFailure, PreflightGroup, SessionConversionDefaults, SessionGroupJob,
    SessionPreflightReport,
};
use gui_engine::state::AppStateSnapshot;
use gui_engine::{FfmpegCapabilities, HwDeviceCapabilities, RecordingType};

const POLL_TIMEOUT: Duration = Duration::from_secs(60);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn ffmpeg_tooling_available() -> bool {
    let run = |bin: &str| {
        Command::new(bin)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    run("ffmpeg") && run("ffprobe")
}

// ── Config isolation (same shape as session_lifecycle.rs) ───────────────

static CONFIG_LOCK: Mutex<()> = Mutex::new(());

fn take_config_lock() -> std::sync::MutexGuard<'static, ()> {
    CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn init_test_config() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let dir = tempfile::TempDir::new().expect("tempdir for test config");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        std::env::set_var("LTC_CONFIG_HOME", dir.path());
        let _ = Box::leak(Box::new(dir));
    });
}

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

// ── Fixtures ────────────────────────────────────────────────────────────

/// Generate an LTC WAV (start `01:00:00:00`) via the engine's own
/// generator, at the given channel selection ("left"/"right"/"both").
fn generate_ltc_wav(dir: &Path, name: &str, duration: f64, channel: &str) -> PathBuf {
    let path = dir.join(name);
    let cli = gui_engine::cli::Cli {
        output_to_file: Some(path.to_string_lossy().to_string()),
        duration: Some(duration),
        fps: 25.0,
        start_timecode: "01:00:00:00".to_string(),
        channel: channel.to_string(),
        volume: Some(0.5),
        sample_rate: Some(48000),
        list_devices: false,
        headless: false,
        device: None,
        device_index: None,
        drop_frame: false,
        verbose: false,
        debug: false,
        decode: None,
        audio_stream: 0,
        audio_channel: 0,
        decoder: "builtin".to_string(),
        decode_fps: 25.0,
        decode_drop_frame: false,
        single_pass: false,
        context_frames: 3,
        list_timecodes: false,
        autostart: false,
        probe_caps: false,
    };
    gui_engine::cli::generate_wav(cli).expect("LTC WAV generation failed");
    path
}

/// A mono WAV without LTC (location-audio stand-in).
fn generate_tone_wav(dir: &Path, name: &str, duration: f64) -> PathBuf {
    let path = dir.join(name);
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 48000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&path, spec).unwrap();
    let n = (48000.0 * duration) as u32;
    for i in 0..n {
        let v = ((i as f32 / 48000.0 * 440.0).sin() * 8000.0) as i32;
        w.write_sample(v).unwrap();
    }
    w.finalize().unwrap();
    path
}

/// Mux a test video + one mono tone stream + the LTC stream (LTC second,
/// absolute stream 2) into an MP4 — mirrors `video_extraction.rs`.
fn mux_video_fixture(dir: &Path, name: &str, tone_wav: &Path, ltc_wav: &Path) -> PathBuf {
    let out = dir.join(name);
    let status = Command::new("ffmpeg")
        .args(["-y", "-v", "error"])
        .args([
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=2:size=128x72:rate=25",
        ])
        .arg("-i")
        .arg(tone_wav)
        .arg("-i")
        .arg(ltc_wav)
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a"])
        .args(["-c:v", "mpeg4", "-q:v", "8", "-pix_fmt", "yuv420p"])
        .args(["-c:a", "aac", "-b:a", "192k", "-t", "2"])
        .arg(out.to_string_lossy().to_string())
        .status()
        .expect("failed to spawn ffmpeg for fixture");
    assert!(status.success(), "ffmpeg fixture mux failed for {}", name);
    out
}

/// Build a synthetic session folder: a TASCAM-per-track set (S1 tone,
/// S2 LTC carrier, S3/S4 tone) plus a camera MP4 with LTC on stream 1
/// channel 1 — exercising the scan path (configured default misses).
fn make_session(dest: &Path) {
    std::fs::create_dir_all(dest).unwrap();
    let ltc_mono = generate_ltc_wav(dest, "ltc-src.wav", 4.0, "left");
    generate_tone_wav(dest, "TASCAM_0099S1.wav", 4.0);
    std::fs::copy(&ltc_mono, dest.join("TASCAM_0099S2.wav")).unwrap();
    generate_tone_wav(dest, "TASCAM_0099S3.wav", 4.0);
    generate_tone_wav(dest, "TASCAM_0099S4.wav", 4.0);

    // Tone-only companion so the video mux has a non-LTC audio stream.
    let tone = generate_tone_wav(dest, "cam-tone.wav", 2.0);
    mux_video_fixture(dest, "C0001.MP4", &tone, &ltc_mono);
}

fn audio_group(dest: &Path, tag: &str) -> PreflightGroup {
    let f = |n: usize| dest.join(format!("TASCAM_{tag}S{n}.wav"));
    PreflightGroup {
        dest: dest.to_path_buf(),
        prefix: format!("TASCAM_{tag}"),
        kind: RecordingType::MultiTrackAudio,
        files: vec![f(1), f(2), f(3), f(4)],
        ltc_file_idx: 1,
    }
}

fn real_decode(
    req: &PreflightDecodeRequest<'_>,
) -> Result<audio_core::LtcDetectionResult, gui_engine::session_conversion::PreflightDecodeError> {
    preflight_decode_real(req)
}

fn device_of(files: &[PathBuf]) -> String {
    gui_engine::device_name::resolve_device_name(files, "", None).0
}

// ── Preflight (runner-level, real ffmpeg) ───────────────────────────────

/// Happy path: the synthetic session's audio + video groups both resolve.
/// LTC is on channel 1 of the camera clip (scan finds it after the
/// configured default misses) and channel 0 of the mono TASCAM LTC file.
#[test]
fn preflight_happy_path_resolves_both_media_types() {
    let _cfg = take_config_lock();
    init_test_config();
    if !ffmpeg_tooling_available() {
        eprintln!("--- SKIPPED: ffmpeg/ffprobe not available (preflight_happy_path_resolves_both_media_types)");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    make_session(dir.path());
    let groups = collect_preflight_groups(&[dir.path().to_path_buf()], 1);
    assert_eq!(groups.len(), 2, "audio + video group discovered");

    let tracker = gui_engine::ProgressTracker::new(vec![
        gui_engine::UnitSpec {
            weight: 0.5,
            label: "audio".into(),
        },
        gui_engine::UnitSpec {
            weight: 0.5,
            label: "video".into(),
        },
    ]);
    let ctx = gui_engine::JobContext {
        progress: tracker,
        cancel: gui_engine::CancelToken::new(),
        emit: Box::new(|_| {}),
    };
    let defaults = SessionConversionDefaults::default();

    let out = run_session_preflight_with(
        &ctx,
        &groups,
        &defaults,
        25.0,
        false,
        &mut real_decode,
        &device_of,
    )
    .expect("preflight completes");
    let JobFinal::SessionPreflight { report } = out else {
        panic!("expected preflight final");
    };
    assert!(
        !report.has_failures(),
        "all groups must resolve: failures={:?}",
        report.failures
    );
    assert_eq!(report.groups.len(), 2);

    let audio = report
        .groups
        .iter()
        .find(|g| g.kind == RecordingType::MultiTrackAudio)
        .expect("audio group result");
    assert_eq!(audio.files.len(), 4);
    assert_eq!(
        audio.files[0].resolved.channel, 0,
        "mono LTC carrier on ch0"
    );
    assert_eq!(
        audio
            .start_timecode()
            .map(|t| (t.hours, t.minutes, t.seconds, t.frames)),
        Some((1, 0, 0, 0)),
        "start TC from the generated 01:00:00:00 signal"
    );

    let video = report
        .groups
        .iter()
        .find(|g| g.kind == RecordingType::VideoClipSequence)
        .expect("video group result");
    assert_eq!(
        video.files[0].resolved.stream, 2,
        "LTC is the second audio stream (absolute ffprobe index: video=0, tone=1, ltc=2)"
    );
    assert_eq!(video.files[0].resolved.channel, 0, "LTC muxed on channel 0");
    assert_eq!(
        video.files[0].source,
        ChannelSource::Scanned,
        "configured default (stream 0) misses; scan finds stream 1"
    );
}

/// A recording whose designated LTC carrier has no LTC aborts with the
/// typed `NoLtc` failure listing every tried candidate — the prominent
/// pre-run abort contract.
#[test]
fn preflight_aborts_on_ltc_less_recording() {
    let _cfg = take_config_lock();
    init_test_config();
    if !ffmpeg_tooling_available() {
        eprintln!(
            "--- SKIPPED: ffmpeg/ffprobe not available (preflight_aborts_on_ltc_less_recording)"
        );
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path()).unwrap();
    generate_tone_wav(dir.path(), "TASCAM_0100S1.wav", 2.0);
    generate_tone_wav(dir.path(), "TASCAM_0100S2.wav", 2.0);

    let groups = vec![audio_group(dir.path(), "0100")];
    let tracker = gui_engine::ProgressTracker::new(vec![gui_engine::UnitSpec {
        weight: 1.0,
        label: "audio".into(),
    }]);
    let ctx = gui_engine::JobContext {
        progress: tracker,
        cancel: gui_engine::CancelToken::new(),
        emit: Box::new(|_| {}),
    };
    let defaults = SessionConversionDefaults::default();

    let out = run_session_preflight_with(
        &ctx,
        &groups,
        &defaults,
        25.0,
        false,
        &mut real_decode,
        &device_of,
    )
    .expect("preflight completes");
    let JobFinal::SessionPreflight { report } = out else {
        panic!("expected preflight final");
    };
    assert!(report.has_failures(), "LTC-less recording must abort");
    assert!(report.groups.is_empty());
    match &report.failures[0] {
        PreflightFailure::NoLtc { tried, .. } => {
            assert!(
                !tried.is_empty(),
                "the scan must have attempted the carrier's channels"
            );
        }
        other => panic!("expected NoLtc, got {other:?}"),
    }
}

// ── Conversion run (runner-level, real ffmpeg) ──────────────────────────

/// End-to-end: preflight → conversion of both media types. The audio group
/// yields one deliverable per non-LTC track (the LTC file is TC source
/// only — it must produce no output); the video group yields a re-encoded
/// video file with timecode metadata.
#[test]
fn session_conversion_end_to_end_produces_q2_deliverables() {
    let _cfg = take_config_lock();
    init_test_config();
    if !ffmpeg_tooling_available() {
        eprintln!("--- SKIPPED: ffmpeg/ffprobe not available (session_conversion_end_to_end_produces_q2_deliverables)");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    make_session(dir.path());
    let inputs: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();

    let groups = collect_preflight_groups(&[dir.path().to_path_buf()], 1);
    let tracker = gui_engine::ProgressTracker::new(vec![
        gui_engine::UnitSpec {
            weight: 0.5,
            label: "audio".into(),
        },
        gui_engine::UnitSpec {
            weight: 0.5,
            label: "video".into(),
        },
    ]);
    let ctx = gui_engine::JobContext {
        progress: tracker,
        cancel: gui_engine::CancelToken::new(),
        emit: Box::new(|_| {}),
    };
    let defaults = SessionConversionDefaults::default();
    let pre = run_session_preflight_with(
        &ctx,
        &groups,
        &defaults,
        25.0,
        false,
        &mut real_decode,
        &device_of,
    )
    .expect("preflight completes");
    let JobFinal::SessionPreflight { report } = pre else {
        panic!("expected preflight final");
    };
    assert!(!report.has_failures());

    let caps = gui_engine::query_ffmpeg_capabilities();
    let resolved = (
        "mov".to_string(),
        "h265".to_string(),
        "pcm_s24le".to_string(),
    );
    let jobs: Vec<SessionGroupJob> = report
        .groups
        .iter()
        .map(|g| SessionGroupJob {
            settings: gui_engine::assemble_session_group_settings(g, &defaults, resolved.clone()),
            group: g.clone(),
        })
        .collect();

    let run_tracker = gui_engine::ProgressTracker::new(vec![
        gui_engine::UnitSpec {
            weight: 0.5,
            label: "audio".into(),
        },
        gui_engine::UnitSpec {
            weight: 0.5,
            label: "video".into(),
        },
    ]);
    let run_ctx = gui_engine::JobContext {
        progress: run_tracker,
        cancel: gui_engine::CancelToken::new(),
        emit: Box::new(|_| {}),
    };
    let mut runner = session_group_runner_real(&run_ctx);
    let out = run_session_conversion_with(&run_ctx, jobs, Some(caps), &mut runner)
        .expect("run completes");
    let JobFinal::SessionConversion { summary } = out else {
        panic!("expected session conversion final");
    };
    assert!(
        !summary.partial,
        "all groups must convert: {:?}",
        summary.outcomes
    );
    assert_eq!(summary.completed_count(), 2);

    // Structural deliverable assertions (file inventory, not text).
    let outputs: Vec<PathBuf> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| !inputs.contains(p))
        .collect();

    // Audio group: exactly 3 TASCAM per-track deliverables for the 4-input
    // set — the LTC carrier (S2) is the TC source only and produces none.
    let tascam_outs: Vec<&PathBuf> = outputs
        .iter()
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("TASCAM") && n.ends_with(".wav"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        tascam_outs.len(),
        3,
        "one deliverable per non-LTC track, LTC file excluded (got {tascam_outs:?}; all outputs {outputs:?})"
    );

    // Video group: one re-encoded deliverable (container mov) plus its
    // split audio track.
    let new_movs: Vec<&PathBuf> = outputs
        .iter()
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("mov"))
        .collect();
    assert_eq!(new_movs.len(), 1, "one video deliverable (got {outputs:?})");
}

// ── Engine command path ─────────────────────────────────────────────────

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

fn spawn_engine() -> TestEngine {
    init_test_config();
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);
    let handle = std::thread::Builder::new()
        .name("engine-session-conv".into())
        .spawn(move || {
            let (event_tx, _event_rx) = mpsc::channel();
            engine_main_with_probe(rx, state_clone, false, event_tx, fake_probe);
        })
        .expect("engine thread");
    TestEngine { tx, state, handle }
}

fn poll_snapshot(
    state: &Arc<ArcSwap<AppStateSnapshot>>,
    pred: impl Fn(&AppStateSnapshot) -> bool,
) -> AppStateSnapshot {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snap = state.load().as_ref().clone();
        if pred(&snap) {
            return snap;
        }
        if Instant::now() > deadline {
            panic!(
                "engine did not reach expected state in time; last error={:?}",
                snap.session_conversion.error
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Engine path: create a session whose offload destination is the
/// synthetic folder, send `RunPreflight`, and observe the report arriving
/// in the snapshot with per-file resolved channels; then `StartRun` and
/// observe the run summary. Real ffmpeg (loud skip).
#[test]
fn engine_session_conversion_command_path_preflight_then_run() {
    let _cfg = take_config_lock();
    init_test_config();
    if !ffmpeg_tooling_available() {
        eprintln!("--- SKIPPED: ffmpeg/ffprobe not available (engine_session_conversion_command_path_preflight_then_run)");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    make_session(dir.path());

    // Seed a session record with the destination (offload-append is phase
    // 3 machinery; the phase-4 consumption path reads the record).
    let rec = gui_engine::config::SessionRecord::new(
        "2026-11-15-test-session".to_string(),
        "Test Session".to_string(),
        "2026-11-15".to_string(),
        25.0,
        false,
        gui_engine::Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
        },
    );
    let mut rec = rec;
    rec.offload_destinations = vec![dir.path().to_string_lossy().to_string()];
    gui_engine::config::save_session(&rec).expect("seed session");

    let engine = spawn_engine();
    engine
        .tx
        .send(GuiCommand::Session(SessionCommand::Load {
            id: rec.id.clone(),
        }))
        .unwrap();
    engine
        .tx
        .send(GuiCommand::SessionConversion(
            SessionConversionCommand::RunPreflight,
        ))
        .unwrap();

    let state = Arc::clone(&engine.state);
    let snap = poll_snapshot(&state, |s| {
        s.session_conversion.preflight.is_some()
            && s.job(JobKind::SessionPreflight).phase() != JobPhase::Running
    });
    let report: &SessionPreflightReport = snap.session_conversion.preflight.as_ref().unwrap();
    assert!(
        !report.has_failures(),
        "preflight must resolve the synthetic session: {:?}",
        report.failures
    );
    assert_eq!(report.groups.len(), 2);

    engine
        .tx
        .send(GuiCommand::SessionConversion(
            SessionConversionCommand::StartRun,
        ))
        .unwrap();
    let snap = poll_snapshot(&state, |s| {
        s.session_conversion.run.is_some()
            && s.job(JobKind::SessionConversion).phase() != JobPhase::Running
    });
    let summary = snap.session_conversion.run.as_ref().unwrap();
    assert!(
        !summary.partial,
        "synthetic session must convert fully: {:?}",
        summary.outcomes
    );
    assert_eq!(summary.completed_count(), 2);

    engine.shutdown();
}

/// Without an active session the preflight is refused with a lifecycle
/// error (engine-side gating; no ffmpeg needed).
#[test]
fn engine_preflight_without_session_is_refused() {
    let _cfg = take_config_lock();
    init_test_config();
    let engine = spawn_engine();
    engine
        .tx
        .send(GuiCommand::SessionConversion(
            SessionConversionCommand::RunPreflight,
        ))
        .unwrap();
    let state = Arc::clone(&engine.state);
    let snap = poll_snapshot(&state, |s| s.session_conversion.error.is_some());
    assert!(snap.session_conversion.preflight.is_none());
    assert!(
        snap.job(JobKind::SessionPreflight).phase() != JobPhase::Running,
        "no preflight job may have started"
    );
    engine.shutdown();
}

/// A cancelled flag crossing the preflight runner classifies as
/// cancellation (typed), not failure.
#[test]
fn preflight_honours_cancellation() {
    let _cfg = take_config_lock();
    init_test_config();
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir_all(dir.path()).unwrap();
    generate_tone_wav(dir.path(), "TASCAM_0101S1.wav", 2.0);
    generate_tone_wav(dir.path(), "TASCAM_0101S2.wav", 2.0);

    let groups = vec![audio_group(dir.path(), "0099")];
    let tracker = gui_engine::ProgressTracker::new(vec![gui_engine::UnitSpec {
        weight: 1.0,
        label: "audio".into(),
    }]);
    let cancel = gui_engine::CancelToken::new();
    cancel.cancel();
    let ctx = gui_engine::JobContext {
        progress: tracker,
        cancel,
        emit: Box::new(|_| {}),
    };
    let defaults = SessionConversionDefaults::default();
    let flags = Arc::new(AtomicBool::new(false));
    let mut decode = |req: &PreflightDecodeRequest<'_>| {
        let _ = req;
        Err(gui_engine::session_conversion::PreflightDecodeError::Cancelled)
    };
    let _ = flags;
    let result = run_session_preflight_with(
        &ctx,
        &groups,
        &defaults,
        25.0,
        false,
        &mut decode,
        &device_of,
    );
    assert!(
        matches!(result, Err(gui_engine::JobError::Cancelled)),
        "cancelled preflight must surface the typed cancellation, got {result:?}"
    );
}
