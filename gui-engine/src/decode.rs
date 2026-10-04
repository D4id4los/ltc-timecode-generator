//! Shared LTC video-decode pipeline.
//!
//! One implementation of the extract→decode pipeline used by both the GUI
//! engine ([`crate::engine`]) and the CLI (`crate::cli`): extract the
//! selected audio channel of a video file to a uniquely-named temp WAV,
//! chunk-decode it, and remove the temp file afterwards.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use audio_core::LtcDecodeError;
use audio_core::{DecodeConfig, DecodeProgress, LtcDetectionResult, WavChunkReader};
use log::info;

use crate::ffprobe;
use crate::job;

/// One video-clip LTC decode request: extract the selected audio channel to
/// a temp WAV and chunk-decode it.
pub struct VideoDecodeRequest<'a> {
    pub path: &'a Path,
    /// Absolute ffprobe stream index (the `index` field of the probed stream).
    pub stream_index: usize,
    pub channel_index: usize,
    pub use_libltc: bool,
    pub decode_fps: f64,
    pub decode_drop_frame: bool,
    /// Shared cancel flag checked during extraction and decode.
    pub cancel: Option<&'a Arc<AtomicBool>>,
}

/// Extract a single audio channel from a video file and decode LTC from it.
/// Returns the LTC detection result or an error string.
///
/// `on_extract_progress` — called during extraction with fraction 0.0..1.0.
/// `decode_unit` — if `Some`, a progress bridge thread is spawned during the
/// chunked decode phase to report "Chunk {done}/{total}" progress into this
/// unit. Pass `None` to skip bridging (e.g. when the caller has no unit, or
/// when the caller will bridge externally).
pub fn decode_video_channel<F: Fn(f32)>(
    req: VideoDecodeRequest<'_>,
    capture_gen: u64,
    on_extract_progress: &F,
    decode_unit: Option<job::UnitProgress>,
) -> Result<LtcDetectionResult, LtcDecodeError> {
    let VideoDecodeRequest {
        path,
        stream_index,
        channel_index,
        use_libltc,
        decode_fps,
        decode_drop_frame,
        cancel,
    } = req;
    let cancel: &Arc<AtomicBool> = cancel.unwrap_or_else(|| dummy_cancel());
    let pipeline_start = Instant::now();

    let tmp_wav = temp_extract_wav(capture_gen, stream_index, channel_index);

    // Phase 1: audio extraction via ffmpeg
    let duration = ffprobe::probe_stream_duration_secs(Path::new(path), stream_index);

    ffprobe::extract_audio_channel_with_progress(
        Path::new(path),
        stream_index,
        channel_index,
        &tmp_wav,
        duration,
        Some(cancel),
        on_extract_progress,
    )
    .map_err(|e| match e {
        // A cancel observed mid-extraction must classify as cancellation
        // (not a generic failure) so the engine's decode job phase stays
        // `Cancelled` — same classification as the post-extraction check
        // below.
        ffprobe::ExtractError::Cancelled => LtcDecodeError::Cancelled,
        other => LtcDecodeError::Failed(format!("Audio extraction failed: {}", other)),
    })?;

    // Check cancel after extraction, before decode
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&tmp_wav);
        return Err(LtcDecodeError::Cancelled);
    }

    let wav_path = tmp_wav.clone();

    // Phase 2: chunked LTC decode from extracted WAV
    let chunks_done = Arc::new(AtomicUsize::new(0));
    let result = match WavChunkReader::open(&wav_path) {
        Ok((reader, _start)) => {
            let total_mono = reader.total_mono_samples();
            let sr = reader.sample_rate();
            let ch = reader.channels() as u16;
            let bps = reader.spec().bits_per_sample;
            drop(reader);

            let config = DecodeConfig::default();
            let chunk_count = audio_core::count_chunks(total_mono, sr, ch, bps, &config);

            if chunk_count <= 1 || total_mono == 0 {
                let result = audio_core::decode_ltc_with_decoder(
                    &wav_path, use_libltc, decode_fps, decode_drop_frame, Some(cancel),
                );
                chunks_done.store(1, Ordering::Relaxed);
                result
            } else {
                let decode_progress = DecodeProgress {
                    chunks_total: chunk_count,
                    chunks_completed: chunks_done.clone(),
                    cancel_flag: cancel.clone(),
                };

                let bridge = decode_unit.as_ref().map(|unit| {
                    bridge_decode_progress(decode_progress.clone(), unit.clone(), None)
                });

                let result = audio_core::decode_ltc_chunked(
                    &wav_path, use_libltc, decode_fps, decode_drop_frame,
                    config, &decode_progress,
                );

                if let Some(h) = bridge {
                    let _ = h.join();
                }

                result
            }
        }
        Err(e) => Err(LtcDecodeError::Failed(format!("Failed to open extracted WAV: {}", e))),
    };

    // Stamp total pipeline time (extraction + decode) onto the result
    let result = result.map(|mut r| {
        r.processing_time_ms = pipeline_start.elapsed().as_secs_f64() * 1000.0;
        r
    });

    let _ = std::fs::remove_file(&tmp_wav);
    result
}

/// Never-set cancel flag used when a caller passes no cancellation.
static DUMMY_CANCEL: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();

fn dummy_cancel() -> &'static Arc<AtomicBool> {
    DUMMY_CANCEL.get_or_init(|| Arc::new(AtomicBool::new(false)))
}

/// engine job.
#[derive(Clone, Copy, Debug)]
pub struct WavDecodeParams {
    pub use_libltc: bool,
    /// Force the non-chunked decoder regardless of file size.
    pub single_pass: bool,
    pub decode_fps: f64,
    pub decode_drop_frame: bool,
}

#[derive(Debug)]
pub struct WavDecodeOutcome {
    pub result: LtcDetectionResult,
    /// The chunk count that drove the dispatch decision (1 for the
    /// single-pass path).
    pub chunk_count: usize,
}

/// The dispatch decision, extracted for lowest-layer unit testing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum WavDispatch {
    SinglePass,
    Chunked(usize),
}

/// Resolve the single-pass-vs-chunked dispatch for a WAV decode:
/// `single_pass` wins over any count; otherwise the known (or counted)
/// chunk count decides (`<= 1` → single-pass).
pub(crate) fn wav_dispatch_decision(single_pass: bool, known: Option<usize>, counted: usize) -> WavDispatch {
    if single_pass {
        return WavDispatch::SinglePass;
    }
    match known.unwrap_or(counted) {
        n if n <= 1 => WavDispatch::SinglePass,
        n => WavDispatch::Chunked(n),
    }
}

/// Decode LTC directly from a WAV file — the WAV-side shared dispatch core.
/// The single-pass-vs-chunked decision lives here exactly once: the CLI
/// wraps this with the stderr progress printer, the engine job wraps it
/// with `ProgressTracker` bridging.
///
/// `known_chunk_count` — `Some` skips the header re-open (the engine has
/// already counted for its eager status message; also the test lever to
/// force the chunked branch). `None` = count here (CLI path), preserving
/// the `unwrap_or(1)` fallback so a missing file surfaces as a typed error
/// from the decoder itself.
/// `cancel` / `progress` — optional cancellation flag and shared
/// `DecodeProgress` for the chunked branch; `None` progress gets a fresh
/// unused one.
pub fn decode_wav_core(
    path: &Path,
    params: WavDecodeParams,
    known_chunk_count: Option<usize>,
    cancel: Option<&Arc<AtomicBool>>,
    progress: Option<&DecodeProgress>,
) -> Result<WavDecodeOutcome, LtcDecodeError> {
    let config = DecodeConfig::default();
    let counted = known_chunk_count
        .unwrap_or_else(|| audio_core::count_chunks_in_wav(path, &config).unwrap_or(1));
    let chunk_count = counted;

    match wav_dispatch_decision(params.single_pass, known_chunk_count, counted) {
        WavDispatch::SinglePass => {
            let result = audio_core::decode_ltc_with_decoder(
                path, params.use_libltc, params.decode_fps, params.decode_drop_frame,
                cancel.map(|flag| flag.as_ref()),
            )?;
            Ok(WavDecodeOutcome { result, chunk_count })
        }
        WavDispatch::Chunked(count) => {
            let owned_progress;
            let progress = match progress {
                Some(dp) => dp,
                None => {
                    owned_progress = DecodeProgress {
                        chunks_total: count,
                        chunks_completed: Arc::new(AtomicUsize::new(0)),
                        cancel_flag: cancel.cloned().unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
                    };
                    &owned_progress
                }
            };
            let result = audio_core::decode_ltc_chunked(
                path, params.use_libltc, params.decode_fps, params.decode_drop_frame,
                config, progress,
            )?;
            Ok(WavDecodeOutcome { result, chunk_count: count })
        }
    }
}

/// Probe → validate → extract → decode, the shared CLI/GUI front door.
///
/// `stream_pos` is the 0-based *position* among the audio streams (the CLI
/// `--audio-stream` semantics); the absolute ffprobe stream index is
/// resolved internally after validating both indices.
pub fn decode_video_file(
    path: &Path,
    stream_pos: usize,
    channel_idx: usize,
    use_libltc: bool,
    decode_fps: f64,
    decode_drop_frame: bool,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<LtcDetectionResult, LtcDecodeError> {
    // Probe the video file to validate it has audio streams
    let probe = ffprobe::probe_video_audio(path)
        .map_err(|e| LtcDecodeError::Failed(format!("Failed to probe video: {}", e)))?;

    if stream_pos >= probe.streams.len() {
        return Err(LtcDecodeError::Failed(format!(
            "Audio stream index {} out of range ({} streams available). Use --audio-stream to select.",
            stream_pos,
            probe.streams.len(),
            )));
    }
    let stream = &probe.streams[stream_pos];
    if channel_idx >= stream.channels {
        return Err(LtcDecodeError::Failed(format!(
            "Channel index {} out of range for stream {} ({} channels available). Use --audio-channel to select.",
            channel_idx, stream_pos, stream.channels,
            )));
    }
    let stream_idx = stream.stream_index;

    info!(
        "Probed video: {} streams, audio #{} = absolute stream {} ({} ch), decoding channel {} ({})",
        probe.streams.len(),
        stream_pos,
        stream_idx,
        stream.channels,
        channel_idx,
        stream.codec_name,
    );

    decode_video_channel(
        VideoDecodeRequest {
            path,
            stream_index: stream_idx,
            channel_index: channel_idx,
            use_libltc,
            decode_fps,
            decode_drop_frame,
            cancel,
        },
        /* capture_gen */ 0,
        &|_| {},
        None,
    )
}

/// Shared temp-WAV naming for extracted audio: collision-proof across
/// concurrent decodes in the same process (CLI and GUI share it) via a
/// process-wide counter.
fn temp_extract_wav(capture_gen: u64, stream_index: usize, channel_index: usize) -> PathBuf {
    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "ltc_extract_{}_{}_{}_{}_{}.wav",
        std::process::id(),
        capture_gen,
        stream_index,
        channel_index,
        counter,
    ))
}

/// Bridge a `DecodeProgress` (from audio-core chunked decode) to a
/// `UnitProgress` by polling `chunks_completed` on a short-lived helper
/// thread.  Returns a `JoinHandle` the caller should join before the unit
/// finishes.
pub(crate) fn bridge_decode_progress(
    dp: DecodeProgress,
    unit: job::UnitProgress,
    msg_prefix: Option<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("dp-bridge".into())
        .spawn(move || {
            loop {
                let done = dp.chunks_completed.load(Ordering::Relaxed);
                let total = dp.chunks_total;
                if total > 0 {
                    unit.set_fraction(done as f32 / total as f32);
                    let msg = match &msg_prefix {
                        Some(prefix) => format!("{} — Chunk {}/{}", prefix, done.min(total), total),
                        None => format!("Chunk {}/{}", done.min(total), total),
                    };
                    unit.set_message(msg);
                }
                if done >= total || dp.cancel_flag.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
        })
        .expect("failed to spawn decode-progress bridge thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_extract_wav_names_are_unique() {
        let mut names = std::collections::HashSet::new();
        for i in 0..1000u64 {
            let p = temp_extract_wav(i, i as usize % 4, i as usize % 2);
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            assert!(name.starts_with("ltc_extract_"), "unexpected temp name: {}", name);
            assert_eq!(p.extension().and_then(|e| e.to_str()), Some("wav"));
            assert!(names.insert(p), "temp WAV path collision at iteration {}", i);
        }
    }

    // ── WAV decode dispatch core (WP-2.2) ────────────────────────────────

    fn test_params(single_pass: bool) -> WavDecodeParams {
        WavDecodeParams {
            use_libltc: false,
            single_pass,
            decode_fps: 25.0,
            decode_drop_frame: false,
        }
    }

    #[test]
    fn test_wav_dispatch_decision_table() {
        // Forced single-pass wins over any count.
        assert_eq!(wav_dispatch_decision(true, Some(9), 9), WavDispatch::SinglePass);
        assert_eq!(wav_dispatch_decision(true, None, 4), WavDispatch::SinglePass);
        // Known counts drive the decision.
        assert_eq!(wav_dispatch_decision(false, Some(1), 9), WavDispatch::SinglePass);
        assert_eq!(wav_dispatch_decision(false, Some(0), 9), WavDispatch::SinglePass);
        assert_eq!(wav_dispatch_decision(false, Some(3), 9), WavDispatch::Chunked(3));
        // No known count: fall back to the counted value.
        assert_eq!(wav_dispatch_decision(false, None, 1), WavDispatch::SinglePass);
        assert_eq!(wav_dispatch_decision(false, None, 4), WavDispatch::Chunked(4));
    }

    /// Write a small real LTC WAV via the CLI generator (in-module, so no
    /// engine/config test setup is needed).
    fn make_ltc_wav(path: &Path, seconds: f64) {
        let cli = crate::cli::Cli {
            output_to_file: Some(path.to_string_lossy().to_string()),
            duration: Some(seconds),
            fps: 25.0,
            start_timecode: "01:00:00:00".to_string(),
            channel: "left".to_string(),
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
        crate::cli::generate_wav(cli).expect("WAV generation failed");
    }

    #[test]
    fn test_wav_core_decodes_synthetic_ltc() {
        let dir = tempfile::TempDir::new().unwrap();
        let wav = dir.path().join("small.wav");
        make_ltc_wav(&wav, 0.5);

        let outcome = decode_wav_core(&wav, test_params(false), None, None, None)
            .expect("small single-chunk WAV must decode");
        assert_eq!(outcome.chunk_count, 1);
        assert!(
            outcome.result.valid_frames >= 1,
            "synthetic LTC must decode at least one frame (got {})",
            outcome.result.valid_frames
        );
    }

    #[test]
    fn test_wav_core_forced_chunked_path_succeeds_on_small_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let wav = dir.path().join("small.wav");
        make_ltc_wav(&wav, 0.5);

        // known_chunk_count = Some(3) forces the chunked branch on a file
        // that would normally dispatch single-pass; decode_ltc_chunked
        // re-plans internally from the real file, so the result must not be
        // corrupted by the forced count.
        let outcome = decode_wav_core(&wav, test_params(false), Some(3), None, None)
            .expect("forced-chunked decode of a small WAV must succeed");
        assert_eq!(outcome.chunk_count, 3);
        assert!(
            outcome.result.valid_frames >= 1,
            "forced-chunked decode must still find the LTC frames (got {})",
            outcome.result.valid_frames
        );
    }

    #[test]
    fn test_wav_core_cancelled_single_pass() {
        let dir = tempfile::TempDir::new().unwrap();
        let wav = dir.path().join("small.wav");
        make_ltc_wav(&wav, 0.5);

        let cancel = Arc::new(AtomicBool::new(true));
        let err = decode_wav_core(&wav, test_params(false), None, Some(&cancel), None)
            .expect_err("pre-set cancel must abort the single-pass decode");
        assert!(matches!(err, LtcDecodeError::Cancelled));
    }

    #[test]
    fn test_wav_core_cancelled_chunked() {
        let dir = tempfile::TempDir::new().unwrap();
        let wav = dir.path().join("small.wav");
        make_ltc_wav(&wav, 0.5);

        let cancel = Arc::new(AtomicBool::new(true));
        let err = decode_wav_core(&wav, test_params(false), Some(3), Some(&cancel), None)
            .expect_err("pre-set cancel must abort the chunked decode");
        assert!(matches!(err, LtcDecodeError::Cancelled));
    }

    #[test]
    fn test_wav_core_missing_file_is_typed_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let wav = dir.path().join("does-not-exist.wav");
        let err = decode_wav_core(&wav, test_params(false), None, None, None)
            .expect_err("a missing file must be a typed decode error");
        assert!(matches!(err, LtcDecodeError::Failed(_)));
    }

    #[test]
    fn temp_extract_wav_encodes_request_coordinates() {
        let p = temp_extract_wav(42, 3, 1);
        let s = p.to_string_lossy();
        assert!(s.contains("_42_3_1_"), "path should encode gen/stream/channel: {}", s);
    }
}
