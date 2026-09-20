use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use gui_engine::command::GuiCommand;
use gui_engine::state::AppStateSnapshot;
use gui_engine::{decode_ltc_from_wav, LtcDecodeStatus};

// ── Helper: start engine, send command, poll for result, check state ──

const POLL_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn run_engine_with_command(cmd: GuiCommand, use_libltc: bool) -> AppStateSnapshot {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-test".into())
        .spawn(move || {
            gui_engine::engine::engine_main(rx, state_clone, use_libltc);
        })
        .expect("failed to spawn engine thread");

    // Send the command
    tx.send(cmd).unwrap();

    // Poll the state until the command has been processed (non-detecting)
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let snapshot: AppStateSnapshot = state.load().as_ref().clone();

        // For ParseLtcFile, wait until ltc_is_detecting becomes false
        // (the engine sets it true on receiving the command, then false when result arrives)
        // For non-decode commands, the generation counter has advanced.
        if snapshot.generation > 0 && !snapshot.ltc_is_detecting {
            break;
        }

        if Instant::now() > deadline {
            // Return what we have so tests can inspect the timeout
            break;
        }

        std::thread::sleep(POLL_INTERVAL);
    }

    let snapshot: AppStateSnapshot = state.load().as_ref().clone();

    // Signal shutdown and wait
    drop(tx);
    handle.join().expect("engine thread panicked");

    snapshot
}

// ── Integration tests ────────────────────────────────────────────────────

#[test]
fn test_cli_generate_wav_roundtrip() {
    let dir = tempfile::TempDir::new().unwrap();
    let wav_path = dir.path().join("test_cli.wav");

    // Use the CLI's WAV generator
    let cli = gui_engine::cli::Cli {
        output_to_file: Some(wav_path.to_string_lossy().to_string()),
        duration: Some(2.0),
        fps: 25.0,
        start_timecode: "01:00:00:00".to_string(),
        channel: "both".to_string(),
        volume: 0.5,
        sample_rate: Some(48000),
        list_devices: false,
        headless: false,
        device: None,
        device_index: None,
        drop_frame: false,
        verbose: false,
        debug: false,
        decode: None,
        decoder: "builtin".to_string(),
        decode_fps: 25.0,
        decode_drop_frame: false,
    };

    gui_engine::cli::generate_wav(cli).expect("WAV generation failed");

    // Decode the file
    let result = decode_ltc_from_wav(&wav_path, 25.0, false).expect("LTC decode failed");
    assert!(
        matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})",
        result.status,
        result.valid_frames
    );
    assert!(result.valid_frames >= 48, "expected ~50 frames, got {}", result.valid_frames);
}

#[test]
fn test_engine_mpsc_parse_ltc_command() {
    let dir = tempfile::TempDir::new().unwrap();
    let wav_path = dir.path().join("test_parse.wav");

    // Generate WAV via the public helper
    let cli = gui_engine::cli::Cli {
        output_to_file: Some(wav_path.to_string_lossy().to_string()),
        duration: Some(1.0),
        fps: 25.0,
        start_timecode: "10:00:00:00".to_string(),
        channel: "both".to_string(),
        volume: 0.5,
        sample_rate: Some(48000),
        list_devices: false,
        headless: false,
        device: None,
        device_index: None,
        drop_frame: false,
        verbose: false,
        debug: false,
        decode: None,
        decoder: "builtin".to_string(),
        decode_fps: 25.0,
        decode_drop_frame: false,
    };
    gui_engine::cli::generate_wav(cli).expect("WAV generation failed");

    // Send the ParseLtcFile command through the engine's MPSC channel
    let snapshot = run_engine_with_command(
        GuiCommand::ParseLtcFile(wav_path.to_string_lossy().to_string()),
        false,
    );

    // Verify the decode result was stored in the snapshot fields
    assert!(
        snapshot.ltc_decode_result.is_some(),
        "expected ltc_decode_result to be Some"
    );
    let result = snapshot.ltc_decode_result.as_ref().unwrap();
    assert!(
        matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?}",
        result.status
    );
    assert!(result.valid_frames > 0, "expected valid_frames > 0, got {}", result.valid_frames);
    assert!(!snapshot.ltc_is_detecting, "expected ltc_is_detecting to be false");
    assert!(snapshot.ltc_decode_error.is_none(), "expected no error");
    assert!(snapshot.ltc_decode_generation > 0, "expected generation > 0");
}

#[test]
fn test_engine_mpsc_parse_invalid_file() {
    // Send ParseLtcFile for a non-existent file
    let snapshot = run_engine_with_command(
        GuiCommand::ParseLtcFile("/tmp/nonexistent_ltc_test_file.wav".to_string()),
        false,
    );

    assert!(
        snapshot.status_message.contains("Parse failed"),
        "expected 'Parse failed' in status message, got: {}",
        snapshot.status_message
    );
    assert!(
        snapshot.ltc_decode_result.is_none(),
        "expected ltc_decode_result to be None, got: {:?}",
        snapshot.ltc_decode_result
    );
    assert!(
        snapshot.ltc_decode_error.is_some(),
        "expected ltc_decode_error to be Some"
    );
    assert!(!snapshot.ltc_is_detecting, "expected ltc_is_detecting to be false");
}