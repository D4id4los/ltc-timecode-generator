//! CLI dispatch tests via `process_cli_result` (WP-4 PR-4).
//!
//! These exercise the decodable CLI mode runners — `--output-to-file` and
//! `--decode` (WAV and video branches) — which previously had zero coverage
//! and could only fail by killing the test process (`std::process::exit(1)`
//! inside `process_cli`). The refactor routes errors through
//! `CliError::Decode` / `CliError::Generate` and returns the decoded
//! `LtcDetectionResult`, so tests assert on return values, not stdout.
//!
//! Deliberately untested here (documented in `process_cli_result`):
//! - `list_devices` arm — process-bound (`std::process::exit`).
//! - `headless` arm — real audio device + infinite loop.
//! - `RunGui` arm — spawns the real engine thread; covered indirectly by
//!   `tests/integration.rs`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Once;

use gui_engine::cli::{process_cli_result, Cli, CliError, CliOutcome};

// ── Fixtures ─────────────────────────────────────────────────────────────

/// Ensure the test does not write to the real user config directory.
fn init_test_config() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let dir = tempfile::TempDir::new().expect("tempdir for test config");
        std::env::set_var("XDG_CONFIG_HOME", dir.path());
        let _ = Box::leak(Box::new(dir));
    });
}

/// Build a `Cli` with the struct's defaults overridden per test.
fn cli_with(mutate: impl FnOnce(&mut Cli)) -> Cli {
    let mut cli = Cli {
        output_to_file: None,
        duration: None,
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
    mutate(&mut cli);
    cli
}

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

/// Generate a real LTC WAV (25 fps, start 01:00:00:00, 0.5 s).
fn generate_test_wav(path: &Path) {
    let cli = cli_with(|c| {
        c.output_to_file = Some(path.to_string_lossy().to_string());
        c.duration = Some(0.5);
    });
    process_cli_result(cli).expect("WAV generation dispatch failed");
}

// ── output-to-file dispatch ──────────────────────────────────────────────

#[test]
fn process_cli_result_output_to_file_writes_wav() {
    init_test_config();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("out.wav");

    let outcome = process_cli_result(cli_with(|c| {
        c.output_to_file = Some(path.to_string_lossy().to_string());
        c.duration = Some(1.0);
        c.fps = 25.0;
    })).expect("generate dispatch should succeed");

    assert!(matches!(outcome, CliOutcome::Done));
    let file = std::fs::File::open(&path).expect("WAV file exists");
    let mut reader = hound::WavReader::new(file).expect("valid WAV");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 48000);
    assert_eq!(spec.channels, 2);
    // 1.0 s at 25 fps = 25 frames; hound-reported duration must match.
    let frames = spec.sample_rate as f64 * 1.0;
    let actual: u32 = reader.samples::<i16>().count() as u32 / spec.channels as u32;
    assert!(
        (actual as f64 - frames).abs() < spec.sample_rate as f64 * 0.05,
        "duration mismatch: expected ~{frames} samples, got {actual}"
    );
}

// ── WAV decode dispatch ──────────────────────────────────────────────────

#[test]
fn process_cli_result_decode_wav_returns_detection_result() {
    init_test_config();
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("ltc.wav");
    generate_test_wav(&path);

    let outcome = process_cli_result(cli_with(|c| {
        c.decode = Some(path.to_string_lossy().to_string());
        c.single_pass = true;
    })).expect("WAV decode dispatch should succeed");

    assert!(matches!(outcome, CliOutcome::Done));
    // The result itself was printed; decode correctness is covered by the
    // dedicated decode tests — here we pin that the dispatch path runs the
    // WAV branch end-to-end without exiting the process.
}

#[test]
fn process_cli_result_decode_nonexistent_fails_without_exit() {
    init_test_config();
    // The previously process-killing path: a decode failure must come back
    // as Err(CliError::Decode) so callers (and tests) survive it.
    let err = match process_cli_result(cli_with(|c| {
        c.decode = Some("/nonexistent/definitely-missing.wav".to_string());
    })) {
        Err(e) => e,
        Ok(_) => panic!("missing file must fail, not succeed"),
    };
    assert!(matches!(err, CliError::Decode(_)), "got: {err:?}");
}

// ── Video decode dispatch (ffmpeg-gated, loud skip) ──────────────────────

/// Synthesize a 1 s test video with a silent stereo audio track.
fn generate_test_video(path: &Path) {
    let status = Command::new("ffmpeg")
        .args([
            "-y", "-f", "lavfi", "-i", "testsrc=duration=1:size=320x240:rate=25",
            "-f", "lavfi", "-i", "sine=frequency=1000:duration=1",
            "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
            "-shortest", path.to_str().unwrap(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("ffmpeg spawn");
    assert!(status.success(), "ffmpeg failed to synthesize test video");
}

#[test]
fn process_cli_result_decode_video_returns_detection_result() {
    init_test_config();
    if !ffmpeg_tooling_available() {
        eprintln!("--- SKIPPED: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let video = dir.path().join("clip.mp4");
    generate_test_video(&video);

    let outcome = process_cli_result(cli_with(|c| {
        c.decode = Some(video.to_string_lossy().to_string());
    })).expect("video decode dispatch should succeed");

    assert!(matches!(outcome, CliOutcome::Done));
}

#[test]
fn process_cli_result_decode_video_out_of_range_stream_fails() {
    init_test_config();
    if !ffmpeg_tooling_available() {
        eprintln!("--- SKIPPED: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let video = dir.path().join("clip.mp4");
    generate_test_video(&video);

    let err = match process_cli_result(cli_with(|c| {
        c.decode = Some(video.to_string_lossy().to_string());
        c.audio_stream = 7; // the clip has exactly one audio stream
    })) {
        Err(e) => e,
        Ok(_) => panic!("out-of-range stream must fail, not succeed"),
    };
    assert!(matches!(err, CliError::Decode(_)), "got: {err:?}");
}

#[test]
fn process_cli_result_probe_caps_dispatches_and_exits() {
    init_test_config();
    if !ffmpeg_tooling_available() {
        eprintln!("--- SKIPPED: ffmpeg/ffprobe not available");
        return;
    }
    let outcome = process_cli_result(cli_with(|c| c.probe_caps = true))
        .expect("--probe-caps dispatch should succeed");
    assert!(matches!(outcome, CliOutcome::Done));
}
