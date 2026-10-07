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
//! - `list_devices` arm — only the device enumeration stays
//!   environment-bound; the output formatting is pure (`list_device_lines`)
//!   and covered by unit tests in `cli.rs`.
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
        // Windows ignores XDG_CONFIG_HOME (dirs uses SHGetKnownFolderPath),
        // so tests isolate config state there via this override instead.
        std::env::set_var("LTC_CONFIG_HOME", dir.path());
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

/// Mono samples the render's transmission lead-in contributes: one full
/// 80-bit frame worth, i.e. the base frame size (WP-EN item 3).
fn expected_lead_in_samples(base_samples: usize) -> usize {
    base_samples
}

/// Render + hound readback: total mono sample count.
fn render_mono_sample_count(cli: Cli) -> usize {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("drift.wav");
    let cli = {
        let mut c = cli;
        c.output_to_file = Some(path.to_string_lossy().to_string());
        c
    };
    process_cli_result(cli).expect("render dispatch should succeed");
    let mut reader = hound::WavReader::open(&path).expect("valid WAV");
    let ch = reader.spec().channels as usize;
    reader.samples::<i16>().count() / ch
}

fn render_decode_all_timecodes(
    path: &Path,
    fps: f64,
    drop_frame: bool,
) -> Vec<gui_engine::Timecode> {
    let result = gui_engine::decode_ltc_from_wav(path, fps, drop_frame, None)
        .expect("decode of generated render must succeed");
    result.timecodes.iter().map(|f| f.timecode).collect()
}

fn tc_frame_number(tc: &gui_engine::Timecode, fps: f64) -> u64 {
    let mpf = fps.ceil() as u64;
    (tc.hours as u64 * 3600 + tc.minutes as u64 * 60 + tc.seconds as u64) * mpf + tc.frames as u64
}

// ── render drift (WP-EN item 1) ──────────────────────────────────────────

#[test]
fn render_total_samples_drift_free_2997_48k() {
    init_test_config();
    let fps = 29.97f64;
    let rate = 48000u32;
    let frames = (10.0 * fps).ceil() as usize; // 300
    let exact_spf = rate as f64 / fps;
    let base = exact_spf.floor() as usize;

    let cli = cli_with(|c| {
        c.fps = fps;
        c.sample_rate = Some(rate);
        c.duration = Some(10.0);
    });
    let actual = render_mono_sample_count(cli);

    let expected =
        (frames as f64 * exact_spf).round() as i64 + expected_lead_in_samples(base) as i64;
    let drift = actual as i64 - expected;
    assert!(
        drift.abs() <= 2,
        "29.97 fps @ 48 kHz render must be drift-free: expected ≈{expected} samples, got {actual} (drift {drift})"
    );
}

#[test]
fn render_total_samples_drift_free_24fps_441k() {
    init_test_config();
    let fps = 24.0f64;
    let rate = 44100u32;
    let frames = (10.0 * fps).ceil() as usize; // 240
    let exact_spf = rate as f64 / fps; // 1837.5
    let base = exact_spf.floor() as usize;

    let cli = cli_with(|c| {
        c.fps = fps;
        c.sample_rate = Some(rate);
        c.duration = Some(10.0);
    });
    let actual = render_mono_sample_count(cli);

    let expected =
        (frames as f64 * exact_spf).round() as i64 + expected_lead_in_samples(base) as i64;
    let drift = actual as i64 - expected;
    assert!(
        drift.abs() <= 2,
        "24 fps @ 44.1 kHz render must be drift-free: expected ≈{expected} samples, got {actual} (drift {drift})"
    );
}

#[test]
fn render_2997_decodes_continuous() {
    init_test_config();
    let fps = 29.97f64;
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("cont2997.wav");

    let cli = cli_with(|c| {
        c.output_to_file = Some(path.to_string_lossy().to_string());
        c.fps = fps;
        c.sample_rate = Some(48000);
        c.duration = Some(10.0);
        c.start_timecode = "01:00:00:00".to_string();
    });
    process_cli_result(cli).expect("render dispatch should succeed");

    let decoded = render_decode_all_timecodes(&path, fps, false);
    assert_eq!(decoded.len(), 300, "every rendered frame must decode");
    assert_eq!(
        decoded[0],
        gui_engine::Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0
        },
        "first decoded frame must be the start TC"
    );
    for i in 1..decoded.len() {
        let prev = tc_frame_number(&decoded[i - 1], fps);
        let cur = tc_frame_number(&decoded[i], fps);
        assert_eq!(cur, prev + 1, "frame {i} must be the strict +1 successor");
    }
}

/// Render + hound readback: peak absolute sample as a fraction of full scale.
fn render_peak_level(cli: Cli) -> f32 {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("level.wav");
    let cli = {
        let mut c = cli;
        c.output_to_file = Some(path.to_string_lossy().to_string());
        c
    };
    process_cli_result(cli).expect("render dispatch should succeed");
    let mut reader = hound::WavReader::open(&path).expect("valid WAV");
    reader
        .samples::<i16>()
        .map(|s| (s.expect("sample read") as f32 / i16::MAX as f32).abs())
        .fold(0.0f32, f32::max)
}

// ── render level defaults (WP-EN item 2) ─────────────────────────────────

#[test]
fn render_default_volume_is_minus_12dbfs() {
    init_test_config();
    let peak = render_peak_level(cli_with(|c| {
        c.volume = None;
        c.duration = Some(0.5);
    }));
    // 0.5 UI default → quadratic gain 0.25 → −12 dBFS peak.
    assert!(
        (peak - 0.25).abs() <= 0.02,
        "default render peak must be ≈ −12 dBFS (0.25 linear), got {peak}"
    );
}

#[test]
fn render_explicit_volume_is_honored() {
    init_test_config();
    let peak = render_peak_level(cli_with(|c| {
        c.volume = Some(0.25);
        c.duration = Some(0.5);
    }));
    assert!(
        (peak - 0.0625).abs() <= 0.005,
        "explicit 0.25 UI must render at 0.0625 linear peak, got {peak}"
    );
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
    }))
    .expect("generate dispatch should succeed");

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
    }))
    .expect("WAV decode dispatch should succeed");

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
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc=duration=1:size=320x240:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=1",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            path.to_str().unwrap(),
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
    }))
    .expect("video decode dispatch should succeed");

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
