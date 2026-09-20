use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::path::Path;

use arc_swap::ArcSwap;
use gui_engine::command::GuiCommand;
use gui_engine::state::AppStateSnapshot;
use gui_engine::{decode_ltc_from_wav, LtcDecodeStatus};

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
        decode: None,
        decoder: "builtin".to_string(),
        decode_fps: fps,
        decode_drop_frame: drop_frame,
    }
}

fn generate_wav(path: &Path, fps: f64, drop_frame: bool, duration: f64, sample_rate: u32) {
    let cli = make_wav_cli(path, fps, drop_frame, duration, sample_rate);
    gui_engine::cli::generate_wav(cli).expect("WAV generation failed");
}

/// Start engine, send one or more commands, poll for result, return snapshot.
fn run_engine_with_commands(commands: Vec<GuiCommand>, use_libltc: bool) -> AppStateSnapshot {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-test".into())
        .spawn(move || {
            gui_engine::engine::engine_main(rx, state_clone, use_libltc);
        })
        .expect("failed to spawn engine thread");

    for cmd in commands {
        tx.send(cmd).unwrap();
    }

    // Poll the state until processing completes
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snapshot: AppStateSnapshot = state.load().as_ref().clone();
        if snapshot.generation > 0 && !snapshot.ltc_is_detecting {
            break;
        }
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(POLL_INTERVAL);
    }

    let snapshot: AppStateSnapshot = state.load().as_ref().clone();
    drop(tx);
    handle.join().expect("engine thread panicked");
    snapshot
}

fn run_engine_with_command(cmd: GuiCommand, use_libltc: bool) -> AppStateSnapshot {
    run_engine_with_commands(vec![cmd], use_libltc)
}

// ── WAV round-trip tests (various FPS) ──────────────────────────────────

#[test]
fn test_wav_roundtrip_25fps() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_25fps.wav");
    generate_wav(&path, 25.0, false, 2.0, 48000);
    let result = decode_ltc_from_wav(&path, 25.0, false).expect("LTC decode failed");
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
    assert!(result.valid_frames >= 48, "expected ~50 frames, got {}", result.valid_frames);
}

#[test]
fn test_wav_roundtrip_24fps() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_24fps.wav");
    generate_wav(&path, 24.0, false, 2.0, 48000);
    let result = decode_ltc_from_wav(&path, 24.0, false).expect("LTC decode failed");
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
    assert!(result.valid_frames >= 46, "expected ~48 frames, got {}", result.valid_frames);
}

#[test]
fn test_wav_roundtrip_30fps() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_30fps.wav");
    generate_wav(&path, 30.0, false, 2.0, 48000);
    let result = decode_ltc_from_wav(&path, 30.0, false).expect("LTC decode failed");
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
    assert!(result.valid_frames >= 58, "expected ~60 frames, got {}", result.valid_frames);
}

#[test]
fn test_wav_roundtrip_2997_nd() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_2997nd.wav");
    generate_wav(&path, 29.97, false, 3.0, 48000);
    let result = decode_ltc_from_wav(&path, 29.97, false).expect("LTC decode failed");
    assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
        "expected no Error, got {:?} (valid={})", result.status, result.valid_frames);
}

#[test]
fn test_wav_roundtrip_2997_df() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_2997df.wav");
    generate_wav(&path, 29.97, true, 3.0, 48000);
    let result = decode_ltc_from_wav(&path, 29.97, true).expect("LTC decode failed");
    assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
        "expected no Error, got {:?} (valid={})", result.status, result.valid_frames);
}

#[test]
fn test_wav_roundtrip_16khz() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_16khz.wav");
    generate_wav(&path, 25.0, false, 2.0, 16000);
    let result = decode_ltc_from_wav(&path, 25.0, false).expect("LTC decode failed");
    assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
        "expected no Error at 16kHz, got {:?}", result.status);
}

// ── Engine MPSC: ParseLtcFile ───────────────────────────────────────────

#[test]
fn test_engine_mpsc_parse_ltc_command() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_parse.wav");
    generate_wav(&path, 25.0, false, 1.0, 48000);

    let snapshot = run_engine_with_command(
        GuiCommand::ParseLtcFile(path.to_string_lossy().to_string()),
        false,
    );

    assert!(snapshot.ltc_decode_result.is_some(), "expected ltc_decode_result to be Some");
    let result = snapshot.ltc_decode_result.as_ref().unwrap();
    assert!(matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?}", result.status);
    assert!(result.valid_frames > 0, "expected valid_frames > 0");
    assert!(!snapshot.ltc_is_detecting);
    assert!(snapshot.ltc_decode_error.is_none());
    assert!(snapshot.ltc_decode_generation > 0);
}

#[test]
fn test_engine_mpsc_parse_invalid_file() {
    let snapshot = run_engine_with_command(
        GuiCommand::ParseLtcFile("/tmp/nonexistent_ltc_test_file.wav".to_string()),
        false,
    );

    assert!(snapshot.status_message.contains("Parse failed"),
        "expected 'Parse failed', got: {}", snapshot.status_message);
    assert!(snapshot.ltc_decode_result.is_none());
    assert!(snapshot.ltc_decode_error.is_some());
    assert!(!snapshot.ltc_is_detecting);
}

// ── Clap command integration ────────────────────────────────────────────

#[test]
fn test_engine_clap_creates_log_entry() {
    let snapshot = run_engine_with_command(GuiCommand::Clap, false);

    assert_eq!(snapshot.logs.len(), 1, "expected 1 log entry after Clap");
    let log = &snapshot.logs[0];
    assert_eq!(log.note, "Scene 1");
    assert!(log.timecode.contains(':'), "expected timecode in log, got {}", log.timecode);
    assert_eq!(log.id, "1");
}

#[test]
fn test_engine_clap_auto_increments_take() {
    let snapshot = run_engine_with_command(GuiCommand::Clap, false);

    // auto_increment_take defaults to true, take starts at 1
    assert_eq!(snapshot.take, 2, "take should auto-increment from 1 to 2");
}

#[test]
fn test_engine_clap_sets_flash_and_arm() {
    let snapshot = run_engine_with_command(GuiCommand::Clap, false);

    assert!((snapshot.clap_flash_alpha - 1.0).abs() < 0.01, "flash alpha should be ~1.0 on Clap, got {}", snapshot.clap_flash_alpha);
    assert!(snapshot.clap_arm_angle.abs() < 0.1, "arm angle should be ~0.0 on Clap, got {}", snapshot.clap_arm_angle);
}

#[test]
fn test_engine_clap_status_message() {
    let snapshot = run_engine_with_command(GuiCommand::Clap, false);

    assert_eq!(snapshot.status_message, "Clap!");
}

#[test]
fn test_engine_multiple_claps_accumulate_logs() {
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::Clap, GuiCommand::Clap, GuiCommand::Clap],
        false,
    );

    assert_eq!(snapshot.logs.len(), 3, "expected 3 log entries after 3 Claps");
    // take auto-increments 3 times from 1
    assert_eq!(snapshot.take, 4, "take should be 4 after 3 Claps starting from 1");
}

#[test]
fn test_engine_clap_without_auto_increment() {
    let snapshot = run_engine_with_commands(
        vec![
            GuiCommand::SetAutoIncrement(false),
            GuiCommand::Clap,
            GuiCommand::Clap,
        ],
        false,
    );

    assert_eq!(snapshot.logs.len(), 2, "expected 2 log entries");
    assert_eq!(snapshot.take, 1, "take should remain 1 when auto-increment is off");
}

// ── Engine shutdown ─────────────────────────────────────────────────────

#[test]
fn test_engine_shutdown_via_command() {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-shutdown-test".into())
        .spawn(move || {
            gui_engine::engine::engine_main(rx, state_clone, false);
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
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-drop-test".into())
        .spawn(move || {
            gui_engine::engine::engine_main(rx, state_clone, false);
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
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::HourUp, GuiCommand::HourUp, GuiCommand::HourDown],
        false,
    );
    assert_eq!(snapshot.start_timecode.hours, 2, "expected hours=2 (start=1, up 2, down 1), got {}", snapshot.start_timecode.hours);
}

#[test]
fn test_engine_frame_up_wrap() {
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::SetFpsIndex(1), GuiCommand::FrameDown],
        false,
    );
    // 25fps: frame 0 - 1 → wrap to 24
    assert_eq!(snapshot.start_timecode.frames, 24, "expected frames=24 (wrap around 25fps), got {}", snapshot.start_timecode.frames);
}

// ── FPS selection ──────────────────────────────────────────────────────

#[test]
fn test_engine_set_fps_24() {
    let snapshot = run_engine_with_command(GuiCommand::SetFpsIndex(0), false);
    assert_eq!(snapshot.fps_index, 0);
    assert_eq!(snapshot.fps, 24.0);
    assert!(!snapshot.drop_frame);
}

#[test]
fn test_engine_set_fps_2997_df() {
    let snapshot = run_engine_with_command(GuiCommand::SetFpsIndex(3), false);
    assert_eq!(snapshot.fps_index, 3);
    assert!((snapshot.fps - 29.97).abs() < 0.01);
    assert!(snapshot.drop_frame);
}

#[test]
fn test_engine_set_fps_30() {
    let snapshot = run_engine_with_command(GuiCommand::SetFpsIndex(4), false);
    assert_eq!(snapshot.fps_index, 4);
    assert_eq!(snapshot.fps, 30.0);
    assert!(!snapshot.drop_frame);
}

// ── Theme commands ──────────────────────────────────────────────────────

#[test]
fn test_engine_toggle_theme() {
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::ToggleTheme],
        false,
    );
    assert!(snapshot.is_dark_theme, "expected dark theme after toggle");
}

#[test]
fn test_engine_set_theme() {
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::SetTheme(true)],
        false,
    );
    assert!(snapshot.is_dark_theme);

    let snapshot = run_engine_with_commands(
        vec![GuiCommand::SetTheme(false)],
        false,
    );
    assert!(!snapshot.is_dark_theme);
}

// ── Decode result generation tracking ───────────────────────────────────

#[test]
fn test_engine_decode_generation_increments() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("test_gen.wav");
    generate_wav(&path, 25.0, false, 1.0, 48000);

    let snapshot = run_engine_with_command(
        GuiCommand::ParseLtcFile(path.to_string_lossy().to_string()),
        false,
    );

    assert!(snapshot.ltc_decode_generation > 0, "generation should be > 0");
    assert!(!snapshot.ltc_is_detecting);
    assert!(snapshot.ltc_decode_result.is_some());
}

#[test]
fn test_engine_decode_error_on_nonexistent_file() {
    let snapshot = run_engine_with_command(
        GuiCommand::ParseLtcFile("/tmp/definitely_not_a_real_ltc_file.wav".to_string()),
        false,
    );

    assert!(snapshot.ltc_decode_error.is_some());
    assert!(snapshot.ltc_decode_result.is_none());
    assert!(!snapshot.ltc_is_detecting);
}

// ── State mutation commands ─────────────────────────────────────────────

#[test]
fn test_engine_set_scene_take_roll() {
    let snapshot = run_engine_with_commands(
        vec![
            GuiCommand::SetScene(42),
            GuiCommand::SetTake(7),
            GuiCommand::SetRoll("B002".into()),
        ],
        false,
    );
    assert_eq!(snapshot.scene, 42);
    assert_eq!(snapshot.take, 7);
    assert_eq!(snapshot.roll, "B002");
}

#[test]
fn test_engine_clear_logs() {
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::Clap, GuiCommand::Clap, GuiCommand::ClearLogs],
        false,
    );
    assert!(snapshot.logs.is_empty(), "expected empty logs after ClearLogs");
}

#[test]
fn test_engine_set_sample_rate() {
    let snapshot = run_engine_with_command(GuiCommand::SetSampleRate(48000), false);
    assert_eq!(snapshot.sample_rate, 48000);
}

#[test]
fn test_engine_set_ltc_and_beep_channels() {
    let snapshot = run_engine_with_commands(
        vec![
            GuiCommand::SetLtcChannel("both".into()),
            GuiCommand::SetBeepChannel("left".into()),
        ],
        false,
    );
    assert_eq!(snapshot.ltc_channel, "both");
    assert_eq!(snapshot.beep_channel, "left");
}

#[test]
fn test_engine_set_volumes_and_beep_params() {
    let snapshot = run_engine_with_commands(
        vec![
            GuiCommand::SetLtcVolume(0.75),
            GuiCommand::SetBeepVolume(0.3),
            GuiCommand::SetBeepFrequency(440.0),
            GuiCommand::SetBeepDuration(1.0),
        ],
        false,
    );
    assert!((snapshot.ltc_volume - 0.75).abs() < 1e-6);
    assert!((snapshot.beep_volume - 0.3).abs() < 1e-6);
    assert!((snapshot.beep_frequency - 440.0).abs() < 1e-6);
    assert!((snapshot.beep_duration - 1.0).abs() < 1e-6);
}

#[test]
fn test_engine_toggle_lock() {
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::ToggleLock],
        false,
    );
    assert!(snapshot.is_locked, "expected locked after one toggle");

    let snapshot = run_engine_with_commands(
        vec![GuiCommand::ToggleLock, GuiCommand::ToggleLock],
        false,
    );
    assert!(!snapshot.is_locked, "expected unlocked after two toggles");
}

#[test]
fn test_engine_reset_current_timecode() {
    let snapshot = run_engine_with_commands(
        vec![GuiCommand::Reset],
        false,
    );
    assert_eq!(snapshot.current_timecode, snapshot.start_timecode);
    assert_eq!(snapshot.status_message, "Reset");
}