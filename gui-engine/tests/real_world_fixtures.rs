//! Real-world corpus fixture tests (WP-RW RW3): the committed
//! `test-data/ltc-rw-*` cuts decoded through the production video pipeline
//! (ffprobe → extract → decode) and the gui-engine WAV dispatch.
//!
//! These fixtures carry signal classes no synthetic test covers: 16-bit
//! big-endian LPCM in MP4 (Sony A6100), lossy AAC LTC (m4v), and 24-bit PCM
//! WAV (TASCAM). Measured expectations and floors (measured −10 %) are
//! recorded in the assertions; measurement date 2026-10-05, decoder post-RW2
//! (see `reports/2026-10-05-ltc-chunked-decode-anomalies-report.md`).
//!
//! Video fixtures need ffmpeg/ffprobe (loud-skip without, mirroring
//! `cli_decode.rs` / `video_extraction.rs`); the WAV dispatch test needs no
//! subprocess.

use std::path::{Path, PathBuf};
use std::process::Command;

use audio_core::{LtcDecodeError, LtcDecodeStatus, Timecode};
use gui_engine::decode::{decode_video_file, decode_wav_core, WavDecodeParams};

fn fixture(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR parent")
        .join("test-data")
        .join(name);
    assert!(
        path.exists(),
        "committed real-world fixture missing at: {}",
        path.display()
    );
    path
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// 16-bit big-endian LPCM in MP4 (A6100 clip C0027, cut at 60 s): lossless
/// LTC over a real camera's recording chain. Measured 499/499, first TC
/// `01:22:06:16` (start TC `01:21:06:17` + 60 s seek).
#[test]
fn test_real_world_mp4_lpcm_fixture() {
    if !ffmpeg_available() {
        eprintln!("--- SKIPPED: ffmpeg not available (test_real_world_mp4_lpcm_fixture)");
        return;
    }
    let path = fixture("ltc-rw-a6100-mp4-20s.mp4");

    let result =
        decode_video_file(&path, 0, 0, false, 25.0, false, None).expect("mp4 fixture decode");

    assert!(
        matches!(result.status, LtcDecodeStatus::Success),
        "expected Success for the LPCM fixture, got {:?} (valid={}/{})",
        result.status,
        result.valid_frames,
        result.total_possible_frames,
    );
    // Measured 499/499; floor = measured − 10 %.
    assert!(
        result.valid_frames >= 450,
        "expected ≥450 valid frames, got {}",
        result.valid_frames,
    );
    assert_eq!(
        result.timecodes.first().map(|t| t.timecode),
        Some(Timecode {
            hours: 1,
            minutes: 22,
            seconds: 6,
            frames: 16
        }),
        "first TC must match the measured corpus value",
    );
}

/// Lossy AAC LTC (A6100 clip C0028-1, cut at 60 s): LTC must survive
/// lossy coding + the extract→decode pipeline — the assertion the single
/// pre-WP-RW real-world fixture could never make. Measured 499/499, first
/// TC `01:06:49:09`.
#[test]
fn test_real_world_m4v_aac_fixture() {
    if !ffmpeg_available() {
        eprintln!("--- SKIPPED: ffmpeg not available (test_real_world_m4v_aac_fixture)");
        return;
    }
    let path = fixture("ltc-rw-a6100-m4v-aac-20s.m4v");

    let result =
        decode_video_file(&path, 0, 0, false, 25.0, false, None).expect("m4v fixture decode");

    assert!(
        matches!(result.status, LtcDecodeStatus::Success),
        "expected Success for the AAC fixture, got {:?} (valid={}/{})",
        result.status,
        result.valid_frames,
        result.total_possible_frames,
    );
    assert!(
        result.valid_frames >= 450,
        "expected ≥450 valid frames, got {}",
        result.valid_frames,
    );
    assert_eq!(
        result.timecodes.first().map(|t| t.timecode),
        Some(Timecode {
            hours: 1,
            minutes: 6,
            seconds: 49,
            frames: 9
        }),
        "first TC must match the measured corpus value",
    );
}

/// Pipeline backend contract: video extraction produces mono **24-bit**
/// PCM WAV (`pcm_s24le`, ffprobe.rs), which the libltc binding cannot read
/// — so `--decoder libltc` on a video file surfaces the typed
/// `UnsupportedBitDepth` error instead of a decode. This pins the
/// error's propagation through the engine pipeline (known libltc
/// limitation; cross-backend *agreement* is covered on the 16-bit fixture
/// in audio-core's `test_decoder_contract_confidence_scale`). If extraction
/// ever gains a 16-bit mode, this test fails and the cross-backend
/// agreement belongs here.
#[test]
fn test_real_world_video_pipeline_libltc_unsupported_bit_depth() {
    if !ffmpeg_available() {
        eprintln!("--- SKIPPED: ffmpeg not available (test_real_world_video_pipeline_libltc_unsupported_bit_depth)");
        return;
    }
    let path = fixture("ltc-rw-a6100-mp4-20s.mp4");

    let err = decode_video_file(&path, 0, 0, true, 25.0, false, None)
        .expect_err("libltc must reject the 24-bit extracted WAV");

    assert!(
        matches!(err, LtcDecodeError::UnsupportedBitDepth { bits: 24 }),
        "expected typed UnsupportedBitDepth(24), got {err:?}"
    );
}

/// Routing-only: the gui-engine WAV dispatch (`decode_wav_core`) must
/// return the same first TC as the audio-core decoder on the 24-bit corpus
/// fixture (one behavior, one layer — the decode itself is asserted in
/// audio-core).
#[test]
fn test_real_world_fixtures_wav_via_engine() {
    let path = fixture("ltc-rw-tascam-s2-clean-20s.wav");

    let outcome = decode_wav_core(
        &path,
        WavDecodeParams {
            use_libltc: false,
            single_pass: false,
            decode_fps: 25.0,
            decode_drop_frame: false,
        },
        None,
        None,
        None,
    )
    .expect("engine WAV dispatch");

    let direct =
        audio_core::decode_ltc_from_wav(&path, 25.0, false, None).expect("audio-core decode");
    assert_eq!(
        outcome.result.timecodes.first().map(|t| t.timecode),
        direct.timecodes.first().map(|t| t.timecode),
        "engine dispatch must return the audio-core result unchanged",
    );
    assert!(matches!(outcome.result.status, LtcDecodeStatus::Success));
}

/// The mic fixture must stay non-Success through the engine dispatch too
/// (typed non-Success, exact variant unpinned).
#[test]
fn test_real_world_mic_fixture_negative_via_engine() {
    let path = fixture("ltc-rw-tascam-s1-mic-15s.wav");

    let outcome = decode_wav_core(
        &path,
        WavDecodeParams {
            use_libltc: false,
            single_pass: false,
            decode_fps: 25.0,
            decode_drop_frame: false,
        },
        None,
        None,
        None,
    )
    .expect("engine WAV dispatch");

    assert!(
        !matches!(outcome.result.status, LtcDecodeStatus::Success),
        "mic track must not decode to Success, got {:?}",
        outcome.result.status
    );
}

/// libltc dispatch on the 24-bit WAV surfaces the typed bit-depth error
/// through the engine dispatch (display-stringification happens only at
/// the boundaries; the typed error crosses the layer).
#[test]
fn test_real_world_wav_libltc_unsupported_bit_depth_via_engine() {
    let path = fixture("ltc-rw-tascam-s2-clean-20s.wav");

    let err = decode_wav_core(
        &path,
        WavDecodeParams {
            use_libltc: true,
            single_pass: false,
            decode_fps: 25.0,
            decode_drop_frame: false,
        },
        None,
        None,
        None,
    )
    .expect_err("libltc must reject 24-bit PCM");

    assert!(
        matches!(err, LtcDecodeError::UnsupportedBitDepth { bits: 24 }),
        "expected typed UnsupportedBitDepth(24), got {err:?}"
    );
}
