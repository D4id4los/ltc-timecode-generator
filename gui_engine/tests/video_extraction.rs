//! Regression tests for the video → audio-extraction path used by LTC decoding.
//!
//! These tests build real container fixtures at runtime with ffmpeg (lavfi
//! test video + a muxed LTC wav) and exercise `probe_video_audio` /
//! `extract_audio_channel` and the engine's `ProbeVideo`/`ParseLtcVideo`
//! commands end-to-end.
//!
//! The critical invariant under test: `AudioStreamInfo.stream_index` is the
//! **absolute** stream index inside the container (ffprobe's `index` field —
//! e.g. `1` for the only audio track of a typical video+audio MP4), and
//! `extract_audio_channel` must map it via `-map 0:{n}` (absolute), NOT
//! `-map 0:a:{n}` (n-th audio stream). A typical camera MP4 has its audio at
//! absolute index 1, so the relative form extracts nothing and ffmpeg fails
//! with "Stream map '0:a:1' matches no streams".

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use gui_engine::command::GuiCommand;
use gui_engine::state::AppStateSnapshot;
use gui_engine::{
    decode_ltc_from_wav, extract_audio_channel, path_is_video, probe_video_audio, LtcDecodeStatus,
};

// ── Helpers ──────────────────────────────────────────────────────────────

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

/// Generate a stereo/mono LTC wav (01:00:00:00 start) via the engine's own
/// WAV generator.
fn generate_ltc_wav(dir: &Path, name: &str, fps: f64, duration: f64, channels: &str) -> PathBuf {
    let path = dir.join(name);
    let cli = gui_engine::cli::Cli {
        output_to_file: Some(path.to_string_lossy().to_string()),
        duration: Some(duration),
        fps,
        start_timecode: "01:00:00:00".to_string(),
        channel: channels.to_string(),
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
        decode_fps: fps,
        decode_drop_frame: false,
        single_pass: false,
        context_frames: 3,
        list_timecodes: false,
    };
    gui_engine::cli::generate_wav(cli).expect("LTC WAV generation failed");
    path
}

/// Mux a video stream plus one or more audio streams into an MP4 fixture.
/// With `leading_silent_stream`, a silent mono audio track is inserted first,
/// so the LTC wav ends up as the *second* audio stream (absolute index 2).
///
/// Uses the core `mpeg4` encoder so the fixture does not depend on external
/// encoder libraries (libx264 etc.).
fn mux_video_fixture(dir: &Path, name: &str, ltc_wav: &Path, leading_silent_stream: bool) -> PathBuf {
    let out = dir.join(name);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-v", "error"]);
    cmd.args(["-f", "lavfi", "-i", "testsrc=duration=2:size=128x72:rate=25"]);
    if leading_silent_stream {
        cmd.args(["-f", "lavfi", "-i", "anullsrc=r=48000:cl=mono"]);
    }
    cmd.arg("-i").arg(ltc_wav);
    cmd.args(["-map", "0:v"]);
    if leading_silent_stream {
        cmd.args(["-map", "1:a", "-map", "2:a"]);
    } else {
        cmd.args(["-map", "1:a"]);
    }
    cmd.args(["-c:v", "mpeg4", "-q:v", "8", "-pix_fmt", "yuv420p"]);
    cmd.args(["-c:a", "aac", "-b:a", "192k", "-t", "2"]);
    cmd.arg(out.to_string_lossy().to_string());

    let status = cmd.status().expect("failed to spawn ffmpeg for fixture");
    assert!(status.success(), "ffmpeg fixture mux failed for {}", name);
    out
}

/// Engine harness (same pattern as integration.rs).
fn run_engine_with_commands(commands: Vec<GuiCommand>) -> AppStateSnapshot {
    let (tx, rx) = mpsc::channel();
    let state = Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())));
    let state_clone = Arc::clone(&state);

    let handle = std::thread::Builder::new()
        .name("gui-engine-vtest".into())
        .spawn(move || {
            gui_engine::engine::engine_main(rx, state_clone, false);
        })
        .expect("failed to spawn engine thread");

    for cmd in commands {
        tx.send(cmd).unwrap();
    }

    // Wait until a decode request has been processed (generation > 0) and the
    // async result has been published (no longer detecting).
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snapshot = state.load().as_ref().clone();
        if snapshot.generation > 0
            && snapshot.ltc_decode_generation > 0
            && !snapshot.ltc_is_detecting
        {
            break;
        }
        if Instant::now() > deadline {
            panic!("engine did not finish video decode within 60s");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let snapshot = state.load().as_ref().clone();
    drop(tx);
    handle.join().expect("engine thread panicked");
    snapshot
}

fn assert_decodes_ltc(wav: &Path, min_valid_frames: u32) {
    let result = decode_ltc_from_wav(wav, 25.0, false)
        .expect("LTC decode of extracted wav failed");
    assert!(
        matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})",
        result.status,
        result.valid_frames
    );
    assert!(
        result.valid_frames >= min_valid_frames,
        "expected >= {} valid frames, got {}",
        min_valid_frames,
        result.valid_frames
    );
}

// ── Probe contract ───────────────────────────────────────────────────────

#[test]
fn test_probe_reports_absolute_stream_indices() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_1audio.mp4", &ltc_wav, false);

    assert!(path_is_video(&mp4));
    let probe = probe_video_audio(&mp4).expect("probe failed");
    assert!(probe.is_video_file);
    assert_eq!(probe.streams.len(), 1);
    assert_eq!(
        probe.streams[0].stream_index, 1,
        "first audio stream of a video+audio MP4 must be reported at absolute index 1"
    );
    assert_eq!(probe.streams[0].channels, 2);
    assert_eq!(probe.streams[0].codec_name, "aac");
}

// ── Extraction (the regression) ──────────────────────────────────────────

#[test]
fn test_extract_first_audio_stream_by_absolute_index() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_1audio.mp4", &ltc_wav, false);

    let out = dir.path().join("extract_c0.wav");
    extract_audio_channel(&mp4, 1, 0, &out)
        .expect("extraction of absolute stream 1 must succeed");
    assert!(out.exists(), "extracted wav must exist");

    let reader = hound::WavReader::open(&out).expect("extracted wav must be readable");
    let spec = reader.spec();
    assert_eq!(spec.channels, 1, "extraction must produce mono");
    assert_eq!(spec.sample_rate, 48000);
    assert!(
        reader.duration() > 48_000,
        "expected > 1s of audio, got {} frames",
        reader.duration()
    );
}

#[test]
fn test_probe_then_extract_ltc_roundtrip() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_1audio.mp4", &ltc_wav, false);

    let probe = probe_video_audio(&mp4).expect("probe failed");
    let stream_index = probe.streams[0].stream_index;

    let out = dir.path().join("roundtrip.wav");
    extract_audio_channel(&mp4, stream_index, 0, &out)
        .expect("extraction with probed stream index must succeed");
    assert_decodes_ltc(&out, 40);
}

#[test]
fn test_extract_second_audio_stream_selects_ltc_stream() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_2audio.mp4", &ltc_wav, true);

    let probe = probe_video_audio(&mp4).expect("probe failed");
    assert_eq!(probe.streams.len(), 2);
    assert_eq!(probe.streams[0].stream_index, 1);
    assert_eq!(probe.streams[1].stream_index, 2);

    // LTC stream (absolute index 2), right channel.
    let ltc_out = dir.path().join("extract_s2_c1.wav");
    extract_audio_channel(&mp4, 2, 1, &ltc_out)
        .expect("extraction of absolute stream 2 must succeed");
    assert_decodes_ltc(&ltc_out, 40);

    // Negative control: the silent stream (absolute index 1) must extract
    // fine but contain no decodable LTC.
    let silent_out = dir.path().join("extract_s1_c0.wav");
    extract_audio_channel(&mp4, 1, 0, &silent_out)
        .expect("extraction of the silent stream must succeed");
    match decode_ltc_from_wav(&silent_out, 25.0, false) {
        Err(_) => {} // no LTC found — acceptable
        Ok(r) => assert_eq!(
            r.valid_frames, 0,
            "silent stream must not yield valid LTC frames (got status {:?})",
            r.status
        ),
    }
}

#[test]
fn test_extract_stereo_channel_1() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_1audio.mp4", &ltc_wav, false);

    let out = dir.path().join("extract_c1.wav");
    extract_audio_channel(&mp4, 1, 1, &out)
        .expect("extraction of channel 1 must succeed");
    assert_decodes_ltc(&out, 40);
}

// ── Diagnostics: ffmpeg stderr must not be swallowed ────────────────────

#[test]
fn test_extract_invalid_stream_error_includes_ffmpeg_stderr() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_1audio.mp4", &ltc_wav, false);

    let out = dir.path().join("should_not_exist.wav");
    let err = extract_audio_channel(&mp4, 9, 0, &out)
        .expect_err("extraction of a nonexistent stream must fail");
    assert!(
        err.contains("matches no streams"),
        "error must include ffmpeg's stderr excerpt, got: {}",
        err
    );
    assert!(!out.exists(), "failed extraction must clean up its output file");
}

// ── Engine command path (what the GUIs drive) ────────────────────────────

#[test]
fn test_engine_mpsc_parse_ltc_video() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_1audio.mp4", &ltc_wav, false);
    let mp4_str = mp4.to_string_lossy().to_string();

    let snapshot = run_engine_with_commands(vec![
        GuiCommand::ProbeVideo(mp4_str.clone()),
        GuiCommand::ParseLtcVideo(mp4_str.clone(), 1, 0),
    ]);

    assert!(snapshot.ltc_probe.is_some(), "probe result must be stored");
    assert!(
        snapshot.ltc_decode_error.is_none(),
        "unexpected decode error: {:?}",
        snapshot.ltc_decode_error
    );
    let result = snapshot
        .ltc_decode_result
        .expect("expected a decode result");
    assert!(
        matches!(result.status, LtcDecodeStatus::Success),
        "expected Success, got {:?} (valid={})",
        result.status,
        result.valid_frames
    );
    assert!(result.valid_frames >= 40);
}

#[test]
fn test_engine_parse_ltc_video_rejects_out_of_range_stream() {
    if !ffmpeg_tooling_available() {
        eprintln!("Skipping: ffmpeg/ffprobe not available");
        return;
    }
    let dir = tempfile::TempDir::new().unwrap();
    let ltc_wav = generate_ltc_wav(dir.path(), "ltc.wav", 25.0, 2.0, "both");
    let mp4 = mux_video_fixture(dir.path(), "video_1audio.mp4", &ltc_wav, false);
    let mp4_str = mp4.to_string_lossy().to_string();

    let snapshot = run_engine_with_commands(vec![
        GuiCommand::ProbeVideo(mp4_str.clone()),
        GuiCommand::ParseLtcVideo(mp4_str.clone(), 7, 0),
    ]);

    assert!(
        snapshot.ltc_decode_error.is_some(),
        "out-of-range stream must produce an error instead of invoking ffmpeg"
    );
    assert!(snapshot.ltc_decode_result.is_none());
    assert!(!snapshot.ltc_is_detecting);
}
