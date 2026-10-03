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
) -> Result<LtcDetectionResult, String> {
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
    ).map_err(|e| format!("Audio extraction failed: {}", e))?;

    // Check cancel after extraction, before decode
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&tmp_wav);
        return Err("Decode canceled by user".to_string());
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
        Err(e) => Err(format!("Failed to open extracted WAV: {}", e)),
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

/// Decode LTC directly from a WAV file — the WAV-side shared front door
/// (mirrors [`decode_video_file`]'s shape). `single_pass` forces the
/// non-chunked decoder for small files; otherwise the chunked decoder runs
/// with a stderr progress indicator.
pub fn decode_wav_file(
    path: &Path,
    use_libltc: bool,
    single_pass: bool,
    decode_fps: f64,
    decode_drop_frame: bool,
) -> Result<LtcDetectionResult, String> {
    if single_pass {
        return audio_core::decode_ltc_with_decoder(path, use_libltc, decode_fps, decode_drop_frame, None);
    }

    let config = DecodeConfig::default();
    let chunk_count = audio_core::count_chunks_in_wav(path, &config).unwrap_or(1);

    if chunk_count <= 1 {
        audio_core::decode_ltc_with_decoder(path, use_libltc, decode_fps, decode_drop_frame, None)
    } else {
        let progress = DecodeProgress::new(chunk_count);
        let completed_ref = progress.chunks_completed.clone();
        let total_chunks = chunk_count;

        let progress_handle = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                let done = completed_ref.load(Ordering::Relaxed);
                let pct = (done.checked_mul(100))
                    .and_then(|v| v.checked_div(total_chunks))
                    .unwrap_or(100);
                eprint!("\rDecoding: {:3}%  (chunk {}/{})", pct.min(100), done.min(total_chunks), total_chunks);
                if done >= total_chunks || total_chunks == 0 || std::time::Instant::now() >= deadline { break; }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        });

        let result = audio_core::decode_ltc_chunked(path, use_libltc, decode_fps, decode_drop_frame, config, &progress)?;
        let _ = progress_handle.join();
        eprintln!("\rDecoding: 100%  (chunk {}/{})", total_chunks, total_chunks);
        Ok(result)
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
) -> Result<LtcDetectionResult, String> {
    // Probe the video file to validate it has audio streams
    let probe = ffprobe::probe_video_audio(path)
        .map_err(|e| format!("Failed to probe video: {}", e))?;

    if stream_pos >= probe.streams.len() {
        return Err(format!(
            "Audio stream index {} out of range ({} streams available). Use --audio-stream to select.",
            stream_pos,
            probe.streams.len(),
        ));
    }
    let stream = &probe.streams[stream_pos];
    if channel_idx >= stream.channels {
        return Err(format!(
            "Channel index {} out of range for stream {} ({} channels available). Use --audio-channel to select.",
            channel_idx, stream_pos, stream.channels,
        ));
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

    #[test]
    fn temp_extract_wav_encodes_request_coordinates() {
        let p = temp_extract_wav(42, 3, 1);
        let s = p.to_string_lossy();
        assert!(s.contains("_42_3_1_"), "path should encode gen/stream/channel: {}", s);
    }
}
