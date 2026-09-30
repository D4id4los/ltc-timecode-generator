use std::collections::BTreeSet;
use std::sync::mpsc;
use std::sync::Arc;
use std::sync::Once;
use std::time::{Duration, Instant};
use std::path::Path;

use arc_swap::ArcSwap;
use gui_engine::command::{GuiCommand, OffloadCommand};
use gui_engine::engine::engine_main_with_probe;
use gui_engine::state::AppStateSnapshot;
use gui_engine::{decode_ltc_from_wav, JobKind, JobPhase, LtcDecodeStatus, FfmpegCapabilities, HwDeviceCapabilities};

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
            engine_main_with_probe(rx, state_clone, use_libltc, fake_probe);
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
                "Timeout waiting for predicate (generation={}, ltc_job={:?}, status={})",
                snapshot.generation, snapshot.job(JobKind::LtcDecode), snapshot.status_message
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

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
fn test_wav_roundtrip_24fps() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_24fps.wav");
    generate_wav(&path, 24.0, false, 2.0, 48000);
    let result = decode_ltc_from_wav(&path, 24.0, false, None).expect("LTC decode failed");
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
    assert!(result.valid_frames >= 46, "expected ~48 frames, got {}", result.valid_frames);
}

#[test]
fn test_wav_roundtrip_30fps() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_30fps.wav");
    generate_wav(&path, 30.0, false, 2.0, 48000);
    let result = decode_ltc_from_wav(&path, 30.0, false, None).expect("LTC decode failed");
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
    assert!(result.valid_frames >= 58, "expected ~60 frames, got {}", result.valid_frames);
}

#[test]
fn test_wav_roundtrip_2997_nd() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_2997nd.wav");
    generate_wav(&path, 29.97, false, 3.0, 48000);
    let result = decode_ltc_from_wav(&path, 29.97, false, None).expect("LTC decode failed");
    assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
        "expected no Error, got {:?} (valid={})", result.status, result.valid_frames);
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

#[test]
fn test_wav_roundtrip_44khz() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_44khz.wav");
    generate_wav(&path, 25.0, false, 2.0, 44100);
    let result = decode_ltc_from_wav(&path, 25.0, false, None).expect("LTC decode failed");
    assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
        "expected no Error at 44kHz, got {:?}", result.status);
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
        |s| s.job(JobKind::LtcDecode).phase != JobPhase::Running && s.ltc_decode_result.is_some(),
    );

    assert!(snapshot.ltc_decode_result.is_some(), "expected ltc_decode_result to be Some");
    let result = snapshot.ltc_decode_result.as_ref().unwrap();
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?}", result.status);
    assert!(result.valid_frames > 0, "expected valid_frames > 0");
    assert!(snapshot.job(JobKind::LtcDecode).phase != JobPhase::Running);
    assert!(snapshot.ltc_decode_error.is_none());
    assert!(snapshot.ltc_decode_generation > 0);
}

#[test]
fn test_engine_mpsc_parse_invalid_file() {
    let snapshot = run_engine(
        vec![GuiCommand::ParseLtcWavFile("/tmp/nonexistent_ltc_test_file.wav".to_string())],
        false,
        |s| s.job(JobKind::LtcDecode).phase != JobPhase::Running && s.ltc_decode_error.is_some(),
    );

    assert!(snapshot.status_message.contains("Parse failed"),
        "expected 'Parse failed', got: {}", snapshot.status_message);
    assert!(snapshot.ltc_decode_result.is_none());
    assert!(snapshot.ltc_decode_error.is_some());
    assert!(snapshot.job(JobKind::LtcDecode).phase != JobPhase::Running);
}

// ── Clap command integration ────────────────────────────────────────────

#[test]
fn test_engine_clap_creates_log_entry() {
    let snapshot = run_engine(vec![GuiCommand::Clap], false, |s| s.logs.len() == 1);

    assert_eq!(snapshot.logs.len(), 1, "expected 1 log entry after Clap");
    let log = &snapshot.logs[0];
    assert_eq!(log.note, "Scene 1");
    assert!(log.timecode.contains(':'), "expected timecode in log, got {}", log.timecode);
    assert_eq!(log.id, "1");
}

#[test]
fn test_engine_clap_auto_increments_take() {
    let snapshot = run_engine(vec![GuiCommand::Clap], false, |s| s.take == 2);

    // auto_increment_take defaults to true, take starts at 1
    assert_eq!(snapshot.take, 2, "take should auto-increment from 1 to 2");
}

#[test]
fn test_engine_clap_status_message() {
    let snapshot = run_engine(vec![GuiCommand::Clap], false, |s| s.status_message == "Clap!");

    assert_eq!(snapshot.status_message, "Clap!");
}

#[test]
fn test_engine_multiple_claps_accumulate_logs() {
    let snapshot = run_engine(
        vec![GuiCommand::Clap, GuiCommand::Clap, GuiCommand::Clap],
        false,
        |s| s.logs.len() == 3,
    );

    assert_eq!(snapshot.logs.len(), 3, "expected 3 log entries after 3 Claps");
    // take auto-increments 3 times from 1
    assert_eq!(snapshot.take, 4, "take should be 4 after 3 Claps starting from 1");
}

#[test]
fn test_engine_clap_without_auto_increment() {
    let snapshot = run_engine(
        vec![
            GuiCommand::SetAutoIncrement(false),
            GuiCommand::Clap,
            GuiCommand::Clap,
        ],
        false,
        |s| s.logs.len() == 2,
    );

    assert_eq!(snapshot.logs.len(), 2, "expected 2 log entries");
    assert_eq!(snapshot.take, 1, "take should remain 1 when auto-increment is off");
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
            engine_main_with_probe(rx, state_clone, false, fake_probe);
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
            engine_main_with_probe(rx, state_clone, false, fake_probe);
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

#[test]
fn test_engine_hour_up_down() {
    let snapshot = run_engine(
        vec![GuiCommand::HourUp, GuiCommand::HourUp, GuiCommand::HourDown],
        false,
        |s| s.start_timecode.hours == 2,
    );
    assert_eq!(snapshot.start_timecode.hours, 2, "expected hours=2 (start=1, up 2, down 1), got {}", snapshot.start_timecode.hours);
}

#[test]
fn test_engine_frame_up_wrap() {
    let snapshot = run_engine(
        vec![GuiCommand::SetFpsIndex(1), GuiCommand::FrameDown],
        false,
        |s| s.start_timecode.frames == 24,
    );
    // 25fps: frame 0 - 1 → wrap to 24
    assert_eq!(snapshot.start_timecode.frames, 24, "expected frames=24 (wrap around 25fps), got {}", snapshot.start_timecode.frames);
}

// ── FPS selection ──────────────────────────────────────────────────────

#[test]
fn test_engine_set_fps_24() {
    let snapshot = run_engine(vec![GuiCommand::SetFpsIndex(0)], false, |s| s.fps_index == 0);
    assert_eq!(snapshot.fps_index, 0);
    assert_eq!(snapshot.fps, 24.0);
    assert!(!snapshot.drop_frame);
}

#[test]
fn test_engine_set_fps_2997_df() {
    let snapshot = run_engine(vec![GuiCommand::SetFpsIndex(3)], false, |s| s.fps_index == 3);
    assert_eq!(snapshot.fps_index, 3);
    assert!((snapshot.fps - 29.97).abs() < 0.01);
    assert!(snapshot.drop_frame);
}

#[test]
fn test_engine_set_fps_30() {
    let snapshot = run_engine(vec![GuiCommand::SetFpsIndex(4)], false, |s| s.fps_index == 4);
    assert_eq!(snapshot.fps_index, 4);
    assert_eq!(snapshot.fps, 30.0);
    assert!(!snapshot.drop_frame);
}

// ── Theme commands ──────────────────────────────────────────────────────

#[test]
fn test_engine_toggle_theme() {
    let snapshot = run_engine(vec![GuiCommand::ToggleTheme], false, |s| s.is_dark_theme);
    assert!(snapshot.is_dark_theme, "expected dark theme after toggle");
}

#[test]
fn test_engine_set_theme() {
    let snapshot = run_engine(vec![GuiCommand::SetTheme(true)], false, |s| s.is_dark_theme);
    assert!(snapshot.is_dark_theme);

    let snapshot = run_engine(vec![GuiCommand::SetTheme(false)], false, |s| !s.is_dark_theme);
    assert!(!snapshot.is_dark_theme);
}

// ── Decode result generation tracking ───────────────────────────────────

#[test]
fn test_engine_decode_generation_increments() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_gen.wav");
    generate_wav(&path, 25.0, false, 1.0, 48000);

    let snapshot = run_engine(
        vec![GuiCommand::ParseLtcWavFile(path.to_string_lossy().to_string())],
        false,
        |s| s.job(JobKind::LtcDecode).phase != JobPhase::Running && s.ltc_decode_result.is_some(),
    );

    assert!(snapshot.ltc_decode_generation > 0, "generation should be > 0");
    assert!(snapshot.job(JobKind::LtcDecode).phase != JobPhase::Running);
    assert!(snapshot.ltc_decode_result.is_some());
}

#[test]
fn test_engine_decode_error_on_nonexistent_file() {
    let snapshot = run_engine(
        vec![GuiCommand::ParseLtcWavFile("/tmp/definitely_not_a_real_ltc_file.wav".to_string())],
        false,
        |s| s.job(JobKind::LtcDecode).phase != JobPhase::Running && s.ltc_decode_error.is_some(),
    );

    assert!(snapshot.ltc_decode_error.is_some());
    assert!(snapshot.ltc_decode_result.is_none());
    assert!(snapshot.job(JobKind::LtcDecode).phase != JobPhase::Running);
}

// ── State mutation commands ─────────────────────────────────────────────

#[test]
fn test_engine_set_scene_take_roll() {
    let snapshot = run_engine(
        vec![
            GuiCommand::SetScene(42),
            GuiCommand::SetTake(7),
            GuiCommand::SetRoll("B002".into()),
        ],
        false,
        |s| s.scene == 42 && s.take == 7 && s.roll == "B002",
    );
    assert_eq!(snapshot.scene, 42);
    assert_eq!(snapshot.take, 7);
    assert_eq!(snapshot.roll, "B002");
}

#[test]
fn test_engine_clear_logs() {
    let snapshot = run_engine(
        vec![GuiCommand::Clap, GuiCommand::Clap, GuiCommand::ClearLogs],
        false,
        |s| s.logs.is_empty(),
    );
    assert!(snapshot.logs.is_empty(), "expected empty logs after ClearLogs");
}

#[test]
fn test_engine_set_sample_rate() {
    let snapshot = run_engine(vec![GuiCommand::SetSampleRate(48000)], false, |s| s.sample_rate == 48000);
    assert_eq!(snapshot.sample_rate, 48000);
}

#[test]
fn test_engine_set_ltc_and_beep_channels() {
    let snapshot = run_engine(
        vec![
            GuiCommand::SetLtcChannel("both".into()),
            GuiCommand::SetBeepChannel("left".into()),
        ],
        false,
        |s| s.ltc_channel == "both" && s.beep_channel == "left",
    );
    assert_eq!(snapshot.ltc_channel, "both");
    assert_eq!(snapshot.beep_channel, "left");
}

#[test]
fn test_engine_set_volumes_and_beep_params() {
    let snapshot = run_engine(
        vec![
            GuiCommand::SetLtcVolume(0.75),
            GuiCommand::SetBeepVolume(0.3),
            GuiCommand::SetBeepFrequency(440.0),
            GuiCommand::SetBeepDuration(1.0),
        ],
        false,
        |s| (s.beep_duration - 1.0).abs() < 1e-6,
    );
    assert!((snapshot.ltc_volume - 0.75).abs() < 1e-6);
    assert!((snapshot.beep_volume - 0.3).abs() < 1e-6);
    assert!((snapshot.beep_frequency - 440.0).abs() < 1e-6);
    assert!((snapshot.beep_duration - 1.0).abs() < 1e-6);
}

#[test]
fn test_engine_toggle_lock() {
    let snapshot = run_engine(vec![GuiCommand::ToggleLock], false, |s| s.is_locked);
    assert!(snapshot.is_locked, "expected locked after one toggle");

    let snapshot = run_engine(vec![GuiCommand::ToggleLock, GuiCommand::ToggleLock], false, |s| !s.is_locked);
    assert!(!snapshot.is_locked, "expected unlocked after two toggles");
}

#[test]
fn test_engine_reset_current_timecode() {
    let snapshot = run_engine(vec![GuiCommand::Reset], false, |s| s.status_message == "Reset");
    assert_eq!(snapshot.current_timecode, snapshot.start_timecode);
    assert_eq!(snapshot.status_message, "Reset");
}

// ── LTC decode stream/channel selection ──────────────────────────────────

#[test]
fn test_engine_set_ltc_decode_stream() {
    let snapshot = run_engine(vec![GuiCommand::SetLtcDecodeStream(2)], false, |s| s.ltc_selected_stream == 2);
    assert_eq!(snapshot.ltc_selected_stream, 2);
}

#[test]
fn test_engine_set_ltc_decode_channel() {
    let snapshot = run_engine(vec![GuiCommand::SetLtcDecodeChannel(3)], false, |s| s.ltc_selected_channel == 3);
    assert_eq!(snapshot.ltc_selected_channel, 3);
}

#[test]
fn test_engine_set_ltc_decode_stream_and_channel() {
    let snapshot = run_engine(
        vec![
            GuiCommand::SetLtcDecodeStream(1),
            GuiCommand::SetLtcDecodeChannel(2),
        ],
        false,
        |s| s.ltc_selected_stream == 1 && s.ltc_selected_channel == 2,
    );
    assert_eq!(snapshot.ltc_selected_stream, 1);
    assert_eq!(snapshot.ltc_selected_channel, 2);
}

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
            engine_main_with_probe(rx, state_clone, false, fake_probe);
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
fn cancel_decode_clears_is_detecting_and_sets_status() {
    let snapshot = run_engine(
        vec![
            GuiCommand::ParseLtcWavFile("/nonexistent/bogus_file_for_test.wav".to_string()),
            GuiCommand::CancelDecode,
        ],
        false,
        |s| s.job(JobKind::LtcDecode).phase != JobPhase::Running && s.status_message == "Decode canceled by user",
    );
    assert!(snapshot.job(JobKind::LtcDecode).phase != JobPhase::Running,
        "CancelDecode should clear LtcDecode job phase");
    assert_eq!(snapshot.status_message, "Decode canceled by user",
        "CancelDecode should update status_message");
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
        |s| s.job(JobKind::LtcGroupDecode).phase != JobPhase::Running && s.status_message == "Decode canceled by user",
    );
    assert!(snapshot.job(JobKind::LtcGroupDecode).phase != JobPhase::Running,
        "CancelDecode should clear LtcGroupDecode job phase");
    assert!(snapshot.job(JobKind::LtcDecode).phase != JobPhase::Running,
        "CancelDecode should clear LtcDecode job phase");
    assert_eq!(snapshot.status_message, "Decode canceled by user");
}

#[test]
fn set_parent_folder_persists_in_snapshot() {
    let dir = tempfile::TempDir::new().unwrap();
    let snapshot = run_engine(
        vec![GuiCommand::Offload(OffloadCommand::SetParentFolder(dir.path().to_path_buf()))],
        false,
        |s| s.offload.parent_folder.is_some(),
    );
    assert_eq!(snapshot.offload.parent_folder, Some(dir.path().to_path_buf()),
        "SetParentFolder should update the snapshot parent_folder");
}

// ── Multi-chunk WAV decode progress ───────────────────────────────────────

#[test]
fn test_multi_chunk_decode_shows_intermediate_progress() {
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
            engine_main_with_probe(rx, state_clone, false, fake_probe);
        })
        .expect("failed to spawn engine thread");

    tx.send(GuiCommand::ParseLtcWavFile(path.to_string_lossy().to_string())).unwrap();

    let deadline = Instant::now() + Duration::from_secs(120);
    let mut observed_intermediate = false;

    loop {
        let snapshot: AppStateSnapshot = state.load().as_ref().clone();
        let job = snapshot.job(JobKind::LtcDecode);

        if job.phase == JobPhase::Running {
            let f = job.fraction;
            if f > 0.0 && f < 1.0 {
                observed_intermediate = true;
                break;
            }
        }

        if job.phase == JobPhase::Succeeded || job.phase == JobPhase::Failed {
            break;
        }

        if Instant::now() > deadline {
            panic!(
                "Timeout waiting for decode: phase={:?}, fraction={}, observed_intermediate={}",
                job.phase, job.fraction, observed_intermediate,
            );
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    // Verify final success
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snapshot: AppStateSnapshot = state.load().as_ref().clone();
        let job = snapshot.job(JobKind::LtcDecode);
        if job.phase != JobPhase::Running {
            if job.phase == JobPhase::Succeeded {
                assert!(
                    observed_intermediate,
                    "expected to observe intermediate fraction in (0,1) \
                     before succeeded, but never did (final fraction={})",
                    job.fraction,
                );
            }
            break;
        }
        if Instant::now() > deadline {
            panic!("Timeout waiting for decode to finish");
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    drop(tx);
    handle.join().expect("engine thread panicked");
}