//! Chunked LTC decode: boundary planning, per-chunk decode, and result merge.

use log::{debug, info, warn};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::ltc_decoder::{
    apply_coherent_first_timecode, compute_ltc_quality, decode_ltc_samples,
    CONFIDENCE_LOW_THRESHOLD, CONFIDENCE_SUCCESS_THRESHOLD, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult,
};
use crate::ltc_decoder_libltc::decode_ltc_samples_libltc;
use crate::types::{DecodeConfig, DecodeProgress};
use crate::wav_chunk_reader::WavChunkReader;

/// Single home of the chunk-size math: returns `(chunk_mono_samples, overlap_samples)`.
fn chunk_geometry(config: &DecodeConfig, bytes_per_mono_sample: u64, sample_rate: u32) -> (usize, usize) {
    let overlap_samples = (config.overlap_seconds * sample_rate as f64) as usize;
    let chunk_mono = ((config.chunk_size_bytes / bytes_per_mono_sample.max(1)) as usize)
        .max(overlap_samples * 2);
    (chunk_mono, overlap_samples)
}

/// Compute the exact chunk boundaries `decode_ltc_chunked` decodes as
/// `[start, end)` mono-sample ranges. `count_chunks` is
/// `plan_chunk_boundaries(..).len()` — the prediction can no longer drift
/// from what the decode loop actually produces.
fn plan_chunk_boundaries(total_mono: usize, chunk_mono: usize, overlap: usize) -> Vec<(usize, usize)> {
    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0usize;
    while pos < total_mono {
        let end = (pos + chunk_mono).min(total_mono);
        chunks.push((pos, end));
        if end >= total_mono {
            break;
        }
        let next = end.saturating_sub(overlap);
        if next <= pos {
            break;
        }
        pos = next;
    }
    chunks
}

/// Count how many chunks `decode_ltc_chunked` would split a WAV into,
/// given its parameters and the [`DecodeConfig`]. Returns 0 for empty
/// files, 1 for files small enough to fit in a single chunk.
///
/// Exact by construction: the answer is `plan_chunk_boundaries(..).len()`
/// with the same chunk geometry the decode loop uses.
pub fn count_chunks(
    total_mono_samples: usize,
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    config: &DecodeConfig,
) -> usize {
    if total_mono_samples == 0 {
        return 0;
    }
    let bytes_per_mono = (channels as u64) * (bits_per_sample as u64 / 8);
    let (chunk_mono, overlap) = chunk_geometry(config, bytes_per_mono, sample_rate);
    plan_chunk_boundaries(total_mono_samples, chunk_mono, overlap).len()
}

/// Count chunks for a WAV file by opening it and calling [`count_chunks`].
pub fn count_chunks_in_wav(path: &Path, config: &DecodeConfig) -> Result<usize, String> {
    let (reader, _) = WavChunkReader::open(path)?;
    let total_mono = reader.total_mono_samples();
    Ok(count_chunks(
        total_mono,
        reader.sample_rate(),
        reader.channels() as u16,
        reader.spec().bits_per_sample,
        config,
    ))
}

/// Decode LTC from a WAV file in parallel chunks with progress reporting and cancelation.
pub fn decode_ltc_chunked(
    path: &Path,
    use_libltc: bool,
    fps: f64,
    drop_frame: bool,
    config: DecodeConfig,
    progress: &DecodeProgress,
) -> Result<LtcDetectionResult, String> {
    let (chunk_reader, overall_start) = WavChunkReader::open(path)?;
    let sample_rate = chunk_reader.sample_rate();
    let channels = chunk_reader.channels();
    let total_mono = chunk_reader.total_mono_samples();
    let total_duration = total_mono as f64 / sample_rate as f64;

    debug!("decode_ltc_chunked: {} samples @ {} Hz, {} ch, config chunk={} bytes, overlap={}s",
        total_mono, sample_rate, channels, config.chunk_size_bytes, config.overlap_seconds);

    if total_mono == 0 {
        warn!("decode_ltc_chunked: WAV file contains no samples");
        return Ok(LtcDetectionResult::error("Audio file contains no samples"));
    }

    let bytes_per_mono_sample = (channels as u64) * (chunk_reader.spec().bits_per_sample as u64 / 8);
    let (chunk_mono_samples, overlap_samples) = chunk_geometry(&config, bytes_per_mono_sample, sample_rate);
    let chunks = plan_chunk_boundaries(total_mono, chunk_mono_samples, overlap_samples);

    let num_chunks = chunks.len();
    info!("decode_ltc_chunked: split into {} chunks ({} mono samples each, overlap={} samples)",
        num_chunks, chunk_mono_samples, overlap_samples);

    if num_chunks == 0 {
        return Ok(LtcDetectionResult::error("No audio data to decode"));
    }

    let cancel_flag = progress.cancel_flag.clone();
    let progress_completed = progress.chunks_completed.clone();
    let chunks = Arc::new(chunks);

    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(num_chunks);

    let mut chunk_results: Vec<ChunkResult> = if num_workers <= 1 {
        let mut results = Vec::with_capacity(num_chunks);
        for (chunk_idx, &(start_sample, end_sample)) in chunks.iter().enumerate() {
            if cancel_flag.load(Ordering::Relaxed) {
                info!("decode_ltc_chunked: cancel requested, stopping at chunk {}", chunk_idx);
                break;
            }
            let r = decode_one_chunk(path, chunk_idx, start_sample, end_sample, sample_rate, fps, drop_frame, use_libltc, &cancel_flag);
            progress_completed.fetch_add(1, Ordering::Relaxed);
            results.push(r);
        }
        if cancel_flag.load(Ordering::Relaxed) {
            return Err("Decode canceled by user".to_string());
        }
        results
    } else {
        let results: Arc<[Mutex<Option<ChunkResult>>]> = (0..num_chunks)
            .map(|_| Mutex::new(None))
            .collect::<Vec<_>>()
            .into();
        let next_chunk = Arc::new(AtomicUsize::new(0));

        std::thread::scope(|s| {
            for _ in 0..num_workers {
                let results = Arc::clone(&results);
                let next_chunk = Arc::clone(&next_chunk);
                let chunks = Arc::clone(&chunks);
                let cancel_flag = cancel_flag.clone();
                let progress_completed = progress_completed.clone();
                s.spawn(move || loop {
                    let idx = next_chunk.fetch_add(1, Ordering::Relaxed);
                    if idx >= num_chunks { break; }
                    if cancel_flag.load(Ordering::Relaxed) { break; }
                    let (start_sample, end_sample) = chunks[idx];
                    let result = decode_one_chunk(
                        path, idx, start_sample, end_sample,
                        sample_rate, fps, drop_frame, use_libltc, &cancel_flag,
                    );
                    if cancel_flag.load(Ordering::Relaxed) { break; }
                    progress_completed.fetch_add(1, Ordering::Relaxed);
                    *results[idx].lock().unwrap() = Some(result);
                });
            }
        });

        let mut collected: Vec<ChunkResult> = Vec::with_capacity(num_chunks);
        for (idx, mutex) in results.iter().enumerate() {
            match mutex.lock().unwrap().take() {
                Some(r) => collected.push(r),
                None => collected.push(ChunkResult {
                    chunk_idx: idx,
                    result: Err("Canceled".to_string()),
                }),
            }
        }
        if cancel_flag.load(Ordering::Relaxed) {
            return Err("Decode canceled by user".to_string());
        }
        collected
    };

    chunk_results.sort_by_key(|cr| cr.chunk_idx);

    let mut all_timecodes: Vec<(usize, FrameTimecode)> = Vec::new();
    let mut merged_details: Vec<String> = Vec::new();
    let mut first_tc_secs: f64 = f64::MAX;
    let last_sample_rate: u32 = sample_rate;
    let mut max_conf: f32 = 0.0;

    for cr in &chunk_results {
        match &cr.result {
            Ok(r) => {
                merged_details.push(format!("Chunk {}: {} valid / {} possible (conf {:.1}%)",
                    cr.chunk_idx, r.valid_frames, r.total_possible_frames, r.avg_confidence * 100.0));
                max_conf = max_conf.max(r.avg_confidence);
                let chunk_start_sample = chunks.get(cr.chunk_idx).map(|&(s, _)| s).unwrap_or(0);
                let chunk_start_secs = chunk_start_sample as f64 / sample_rate as f64;
                let chunk_first_secs = if r.first_ltc_timecode_secs > 0.0 {
                    r.first_ltc_timecode_secs + chunk_start_secs
                } else {
                    0.0
                };
                if chunk_first_secs > 0.0 && chunk_first_secs < first_tc_secs {
                    first_tc_secs = chunk_first_secs;
                }
                for ftc in &r.timecodes {
                    let mut adjusted = ftc.clone();
                    adjusted.timecode_secs += chunk_start_secs;
                    all_timecodes.push((cr.chunk_idx, adjusted));
                }
            }
            Err(e) => {
                merged_details.push(format!("Chunk {}: error - {}", cr.chunk_idx, e));
            }
        }
    }

    all_timecodes.sort_by(|a, b| {
        a.1.timecode_secs
            .partial_cmp(&b.1.timecode_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    let frame_duration = 1.0 / fps;
    let dedup_threshold = (frame_duration * 0.5).min(config.overlap_seconds * 0.5);
    let mut deduped: Vec<FrameTimecode> = Vec::with_capacity(all_timecodes.len());
    let mut last_secs: f64 = -dedup_threshold;
    for (_, ftc) in all_timecodes {
        if ftc.timecode_secs - last_secs > dedup_threshold {
            last_secs = ftc.timecode_secs;
            deduped.push(ftc);
        }
    }

    for (i, ftc) in deduped.iter_mut().enumerate() {
        ftc.frame_index = i as u32;
    }

    let valid_frames = deduped.len() as u32;
    let true_total_possible = (total_duration * fps).round() as u32;
    let avg_confidence = if true_total_possible > 0 {
        valid_frames as f32 / true_total_possible as f32
    } else {
        0.0
    };

    let status = if valid_frames > 0 {
        if avg_confidence >= CONFIDENCE_SUCCESS_THRESHOLD {
            LtcDecodeStatus::Success
        } else if avg_confidence >= CONFIDENCE_LOW_THRESHOLD {
            LtcDecodeStatus::LowConfidence
        } else {
            LtcDecodeStatus::NoSyncWord
        }
    } else {
        LtcDecodeStatus::NoSyncWord
    };

    merged_details.push(format!(
        "Chunked decode: {} chunks, {} valid / {} possible after merge",
        num_chunks, valid_frames, true_total_possible,
    ));

    let processing_time_ms = overall_start.elapsed().as_secs_f64() * 1000.0;
    let mut result = LtcDetectionResult {
        status,
        detected_fps: fps as f32,
        drop_frame,
        total_possible_frames: true_total_possible,
        valid_frames,
        timecodes: deduped,
        avg_confidence,
        details: merged_details,
        total_audio_duration_secs: total_duration,
        sample_rate: last_sample_rate,
        processing_time_ms,
        first_ltc_timecode_secs: if first_tc_secs < f64::MAX { first_tc_secs } else { 0.0 },
        quality: None,
    };

    apply_coherent_first_timecode(&mut result);
    result.quality = compute_ltc_quality(&result);
    let processing_time_ms = overall_start.elapsed().as_secs_f64() * 1000.0;
    result.processing_time_ms = processing_time_ms;

    info!("decode_ltc_chunked complete: {} valid / {} possible ({:.1}%) in {:.1}ms",
        result.valid_frames, result.total_possible_frames, result.avg_confidence * 100.0, processing_time_ms);

    Ok(result)
}

/// Result produced by decoding one chunk.
struct ChunkResult {
    chunk_idx: usize,
    result: Result<LtcDetectionResult, String>,
}

/// Decode a single chunk of a WAV file in a worker thread.
fn decode_one_chunk(
    path: &Path,
    chunk_idx: usize,
    start_sample: usize,
    end_sample: usize,
    sample_rate: u32,
    fps: f64,
    drop_frame: bool,
    use_libltc: bool,
    cancel_flag: &AtomicBool,
) -> ChunkResult {
    let num_samples = end_sample - start_sample;

    if cancel_flag.load(Ordering::Relaxed) {
        return ChunkResult { chunk_idx, result: Err("Canceled".to_string()) };
    }

    let mut local_reader = match WavChunkReader::open(path) {
        Ok((r, _)) => r,
        Err(e) => return ChunkResult {
            chunk_idx,
            result: Err(format!("Failed to open file for chunk {}: {}", chunk_idx, e)),
        },
    };

    let chunk_start = Instant::now();

    let result = if use_libltc {
        match local_reader.read_mono_samples_i16(start_sample, num_samples) {
            Ok(samples) => decode_ltc_samples_libltc(
                &samples, 1, sample_rate, fps, drop_frame, chunk_start, Some(cancel_flag),
            ),
            Err(e) => Err(format!("Failed to read chunk {}: {}", chunk_idx, e)),
        }
    } else {
        match local_reader.read_mono_samples_f32(start_sample, num_samples) {
            Ok(samples) => decode_ltc_samples(
                &samples, sample_rate, 1, fps, drop_frame, chunk_start, Some(cancel_flag),
            ),
            Err(e) => Err(format!("Failed to read chunk {}: {}", chunk_idx, e)),
        }
    };
    let elapsed = chunk_start.elapsed();
    let decoder_name = if use_libltc { "libltc" } else { "builtin" };
    debug!("Chunk {} decoded ({}): {:.1}ms", chunk_idx + 1, decoder_name, elapsed.as_secs_f64() * 1000.0);
    ChunkResult { chunk_idx, result }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ChannelSel, Timecode};

    // ── plan_chunk_boundaries ─────────────────────────────────────────

    #[test]
    fn test_plan_chunk_boundaries_exact_table() {
        let plan = plan_chunk_boundaries(1000, 400, 100);
        assert_eq!(plan, vec![(0, 400), (300, 700), (600, 1000)]);
    }

    #[test]
    fn test_plan_chunk_boundaries_single_chunk() {
        assert_eq!(plan_chunk_boundaries(400, 400, 100), vec![(0, 400)]);
        assert_eq!(plan_chunk_boundaries(300, 400, 100), vec![(0, 300)]);
    }

    #[test]
    fn test_plan_chunk_boundaries_zero_total() {
        assert!(plan_chunk_boundaries(0, 400, 100).is_empty());
    }

    #[test]
    fn test_plan_chunk_boundaries_terminates() {
        // Degenerate overlap >= chunk would stall without the next <= pos guard.
        let plan = plan_chunk_boundaries(1000, 10, 10);
        assert!(!plan.is_empty());
        for w in plan.windows(2) {
            assert!(w[1].0 > w[0].0, "positions must strictly advance: {:?}", w);
        }
    }

    // ── count_chunks ↔ plan_chunk_boundaries equivalence ──────────────

    #[test]
    fn test_count_chunks_equals_plan_len_across_sizes() {
        // 48 kHz stereo 16-bit → bytes_per_mono=4, chunk_size 200_000 →
        // chunk_mono=50_000, overlap=14_400. Includes the former
        // count_chunks shortcut window (50_000, 64_400] where the old
        // prediction (1) disagreed with the decode loop (2).
        let config = DecodeConfig { chunk_size_bytes: 200_000, overlap_seconds: 0.3 };
        let (chunk_mono, overlap) = chunk_geometry(&config, 4, 48000);
        assert_eq!(chunk_mono, 50_000);
        assert_eq!(overlap, 14_400);
        for &total in &[
            0usize, 1, 49_999, 50_000, 50_001, 60_000, 64_399, 64_400, 64_401,
            100_000, 150_000, 200_000, 203_600, 250_000, 300_000,
        ] {
            let planned = plan_chunk_boundaries(total, chunk_mono, overlap).len();
            let counted = count_chunks(total, 48000, 2, 16, &config);
            assert_eq!(counted, planned, "total={} mono samples", total);
        }
    }

    #[test]
    fn test_count_chunks_window_predicts_two_chunks() {
        // Regression pin for the removed shortcut: chunk_mono < total <=
        // chunk_mono + overlap must predict 2 chunks (what decode does).
        let config = DecodeConfig { chunk_size_bytes: 200_000, overlap_seconds: 0.3 };
        assert_eq!(count_chunks(60_000, 48000, 2, 16, &config), 2);
    }

    // ── count_chunks ──────────────────────────────────────────────────────

    #[test]
    fn test_count_chunks_single_chunk_for_small_file() {
        let config = DecodeConfig::default();
        let n = count_chunks(100_000, 48000, 2, 16, &config);
        assert_eq!(n, 1, "small file should be 1 chunk");
    }

    #[test]
    fn test_count_chunks_multiple_chunks_for_large_file() {
        let config = DecodeConfig { chunk_size_bytes: 200_000, overlap_seconds: 0.3 };
        let total_mono = 500_000; // mono samples
        let n = count_chunks(total_mono, 48000, 2, 16, &config);
        assert!(n >= 2, "large file should produce >= 2 chunks, got {}", n);
    }

    #[test]
    fn test_count_chunks_zero_samples() {
        let config = DecodeConfig { chunk_size_bytes: 1_000, overlap_seconds: 0.1 };
        let n = count_chunks(0, 48000, 1, 16, &config);
        assert_eq!(n, 0, "zero samples -> zero chunks");
    }

    #[test]
    fn test_count_chunks_matches_decode_ltc_chunked_output_count() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("count_matches.wav");
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 200,
        );

        let config = DecodeConfig { chunk_size_bytes: 200_000, overlap_seconds: 0.3 };
        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let predicted = count_chunks(total_mono, reader.sample_rate(), reader.channels() as u16,
            reader.spec().bits_per_sample, &config);

        let progress = DecodeProgress::new(1);
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();
        let actual_chunks: usize = result.details.iter()
            .filter(|d| d.starts_with("Chunk ") && d.contains("valid"))
            .count();
        assert_eq!(predicted, actual_chunks,
            "count_chunks predicted {} actual decode produced {}", predicted, actual_chunks);
    }

    #[test]
    fn test_count_chunks_zero_chunk_size_uses_fallback() {
        let config = DecodeConfig { chunk_size_bytes: 0, overlap_seconds: 0.1 };
        let n = count_chunks(100, 48000, 1, 16, &config);
        assert_eq!(n, 1,
            "should handle zero chunk_size_bytes gracefully");
    }

    // ── count_chunks_in_wav wrapper ────────────────────────────────

    #[test]
    fn test_count_chunks_in_wav_missing_file() {
        let config = DecodeConfig::default();
        let result = count_chunks_in_wav(Path::new("/nonexistent/path.wav"), &config);
        assert!(result.is_err(), "missing file should return Err");
    }

    #[test]
    fn test_count_chunks_in_wav_empty_wav() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.finalize().unwrap();
        let config = DecodeConfig::default();
        let result = count_chunks_in_wav(&path, &config).unwrap();
        assert_eq!(result, 0, "empty WAV -> 0 chunks");
    }

    #[test]
    fn test_count_chunks_in_wav_matches_count_chunks() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("wrapper_matches.wav");
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 200,
        );
        let config = DecodeConfig { chunk_size_bytes: 200_000, overlap_seconds: 0.3 };
        let predicted = count_chunks_in_wav(&path, &config).unwrap();
        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let expected = count_chunks(
            reader.total_mono_samples(),
            reader.sample_rate(),
            reader.channels() as u16,
            reader.spec().bits_per_sample,
            &config,
        );
        assert_eq!(predicted, expected,
            "count_chunks_in_wav({}) should equal count_chunks({})",
            predicted, expected);
    }

    // ── chunked merge tests ──────────────────────────────────────────

    fn chunk_count_for_config(
        total_mono: usize,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        config: &DecodeConfig,
    ) -> usize {
        count_chunks(total_mono, sample_rate, channels, bits_per_sample, config)
    }

    #[test]
    fn test_chunked_merge_multi_chunk_preserves_all_frames() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_preserve.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        let sample_rate = 48000;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, sample_rate, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let nchunks = chunk_count_for_config(total_mono, sample_rate, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();
        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, fps, false, None).unwrap();

        assert_eq!(chunked.valid_frames, direct.valid_frames,
            "chunked merge lost frames: chunked={} vs direct={}",
            chunked.valid_frames, direct.valid_frames);
        assert_eq!(chunked.status, direct.status);
    }

    #[test]
    fn test_chunked_total_possible_not_inflated() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_total.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        let sample_rate = 48000;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, sample_rate, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let total_duration = total_mono as f64 / sample_rate as f64;
        let expected_possible = (total_duration * fps).round() as u32;
        let nchunks = chunk_count_for_config(total_mono, sample_rate, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();

        assert_eq!(chunked.total_possible_frames, expected_possible,
            "total_possible_frames should be {} (stream total), got {}",
            expected_possible, chunked.total_possible_frames);
        assert!(chunked.total_possible_frames <= num_frames + 5,
            "total_possible should not be significantly larger than num_frames={}, got {}",
            num_frames, chunked.total_possible_frames);
    }

    #[test]
    fn test_chunked_single_chunk_matches_nonchunked_exactly() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("single_chunk.wav");

        let num_frames = 50u32;
        let fps = 25.0;
        generate_ltc_wav(
            &path,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps, false, 48000, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };

        let progress = DecodeProgress::new(1);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();
        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, fps, false, None).unwrap();

        let diff = chunked.valid_frames.abs_diff(direct.valid_frames);
        assert!(diff <= 2,
            "single chunk: chunked={} != direct={} (diff={})",
            chunked.valid_frames, direct.valid_frames, diff);
        assert!(chunked.total_possible_frames >= chunked.valid_frames,
            "total_possible ({}) < valid_frames ({})",
            chunked.total_possible_frames, chunked.valid_frames);
    }

    #[test]
    fn test_chunked_merge_no_unnecessary_dedup() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_dedup.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        let sample_rate = 48000;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, sample_rate, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let overlap_secs = config.overlap_seconds;

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let nchunks = chunk_count_for_config(total_mono, sample_rate, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();

        let total_from_chunks: u32 = chunked.details.iter()
            .filter(|d| d.starts_with("Chunk ") && d.contains("valid"))
            .filter_map(|d| {
                let s = d.split_whitespace().nth(2)?;
                s.parse::<u32>().ok()
            })
            .sum();

        let loss = total_from_chunks.saturating_sub(chunked.valid_frames);
        let frame_duration = 1.0 / fps;
        let max_expected_loss = ((nchunks.saturating_sub(1)) as f64
            * (overlap_secs / frame_duration).ceil()) as u32;
        assert!(loss <= max_expected_loss,
            "unnecessary dedup: lost {} frames (max expected loss from overlap: {}). \
             total_from_chunks={}, valid_after_merge={}",
            loss, max_expected_loss, total_from_chunks, chunked.valid_frames);
    }

    #[test]
    fn test_chunked_merge_libltc_preserves_frames() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_libltc.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, 48000, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let nchunks = chunk_count_for_config(total_mono, 48000, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, true, fps, false, config, &progress).unwrap();
        let direct = crate::ltc_decoder_libltc::decode_ltc_from_wav_libltc(&path, fps, false, None).unwrap();

        let diff = chunked.valid_frames.abs_diff(direct.valid_frames);
        assert!(diff <= 2,
            "chunked libltc merge lost frames: chunked={} vs direct={} (diff={})",
            chunked.valid_frames, direct.valid_frames, diff);
        assert!(chunked.total_possible_frames >= chunked.valid_frames);
    }

    // ── decode_ltc_chunked (existing tests) ───────────────────────────

    fn generate_ltc_wav(
        path: &Path,
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        num_frames: u32,
    ) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;

        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let mut tc = start_tc;
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

        for _ in 0..num_frames {
            frame_buf.fill(0.0);
            crate::generate_ltc_frame_stereo(
                &tc,
                drop_frame,
                samples_per_frame,
                samples_per_bit,
                0.5,
                ChannelSel::Both,
                &mut last_level,
                &mut frame_buf[..samples_per_frame * 2],
            );

            for &sample in &frame_buf[..samples_per_frame * 2] {
                let clamped = sample.clamp(-1.0, 1.0);
                let int_sample = (clamped * i16::MAX as f32) as i16;
                writer.write_sample(int_sample).unwrap();
            }

            tc = crate::increment_timecode(&tc, fps, drop_frame);
        }

        writer.finalize().unwrap();
    }

    fn generate_ltc_wav_with_depth(
        path: &Path,
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        num_frames: u32,
        bits_per_sample: u16,
    ) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample,
            sample_format: hound::SampleFormat::Int,
        };

        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;

        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let mut tc = start_tc;
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

        for _ in 0..num_frames {
            frame_buf.fill(0.0);
            crate::generate_ltc_frame_stereo(
                &tc,
                drop_frame,
                samples_per_frame,
                samples_per_bit,
                0.5,
                ChannelSel::Both,
                &mut last_level,
                &mut frame_buf[..samples_per_frame * 2],
            );

            for &sample in &frame_buf[..samples_per_frame * 2] {
                let clamped = sample.clamp(-1.0, 1.0);
                let max_val = (1i64 << (bits_per_sample - 1)) as f32;
                let int_sample = (clamped * max_val) as i32;
                writer.write_sample(int_sample).unwrap();
            }

            tc = crate::increment_timecode(&tc, fps, drop_frame);
        }

        writer.finalize().unwrap();
    }

    #[test]
    fn test_decode_ltc_chunked_compare_samples() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("compare_samples.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 50,
        );

        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        let hound_samples: Vec<f32> = reader
            .samples::<i32>()
            .filter_map(|s| s.ok())
            .enumerate()
            .filter(|(i, _)| i % spec.channels as usize == 0)
            .map(|(_, s)| s as f32 / (1i64 << (spec.bits_per_sample - 1)) as f32)
            .collect();

        let (mut cr, _) = WavChunkReader::open(&path).unwrap();
        let cr_samples = cr.read_mono_samples_f32(0, hound_samples.len()).unwrap();

        assert_eq!(hound_samples.len(), cr_samples.len(),
            "sample count mismatch: hound={}, WavChunkReader={}",
            hound_samples.len(), cr_samples.len());

        let max_diff: f32 = hound_samples.iter().zip(cr_samples.iter())
            .map(|(a, b)| (*a - *b).abs())
            .fold(0.0f32, f32::max);
        let num_diff = hound_samples.iter().zip(cr_samples.iter())
            .filter(|(a, b)| (*a - *b).abs() > 1e-6)
            .count();

        assert!(max_diff < 1e-4,
            "max sample diff is {:.10} ({} samples differ > 1e-6)",
            max_diff, num_diff);
    }

    #[test]
    fn test_decode_ltc_chunked_compare_with_direct() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("compare.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 50,
        );

        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, 25.0, false, None).unwrap();

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };
        let progress = DecodeProgress::new(1);
        let chunked = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();

        assert_eq!(direct.valid_frames, chunked.valid_frames,
            "direct decode got {} valid, chunked got {} valid (both should match)",
            direct.valid_frames, chunked.valid_frames);
        assert_eq!(direct.status, chunked.status);
        assert!(chunked.processing_time_ms > 0.0,
            "chunked decode processing_time should be positive, got {}", chunked.processing_time_ms);
    }

    #[test]
    fn test_decode_ltc_chunked_single_chunk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ltc_chunked_single.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 50,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };
        let progress = DecodeProgress::new(1);
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
        assert!(result.valid_frames >= 40,
            "should decode at least 40 frames, got {}", result.valid_frames);
        assert!(result.processing_time_ms > 0.0,
            "processing_time should be positive, got {}", result.processing_time_ms);
    }

    #[test]
    fn test_decode_ltc_chunked_empty_wav() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty_ltc.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.finalize().unwrap();

        let config = DecodeConfig::default();
        let progress = DecodeProgress::new(0);
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_chunked_cancel() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("cancel_ltc.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 75,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 1000,
            overlap_seconds: 0.1,
        };
        let progress = DecodeProgress::new(100);
        progress.cancel();
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress);
        assert!(result.is_err(), "canceled decode should return Err");
        let err = result.unwrap_err();
        assert!(err.contains("Canceled") || err.contains("canceled"),
            "error should mention cancel: {}", err);
    }

    #[test]
    fn test_decode_ltc_chunked_libltc() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ltc_chunked_libltc.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 25,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };
        let progress = DecodeProgress::new(1);
        let result = decode_ltc_chunked(&path, true, 25.0, false, config, &progress).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for libltc chunked, got {:?}", result.status);
        assert!(result.valid_frames >= 20,
            "should decode at least 20 frames with libltc, got {}", result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_chunked_libltc_24bit() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ltc_chunked_libltc_24bit.wav");

        generate_ltc_wav_with_depth(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 25, 24,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };
        let progress = DecodeProgress::new(1);
        let result = decode_ltc_chunked(&path, true, 25.0, false, config, &progress).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for libltc chunked 24-bit, got {:?}", result.status);
        assert!(result.valid_frames >= 20,
            "should decode at least 20 frames with libltc 24-bit, got {}", result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_chunked_progress_on_libltc_read_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("float_error.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..2500 {
            writer.write_sample(0.0f32).unwrap();
        }
        writer.finalize().unwrap();

        let config = DecodeConfig {
            chunk_size_bytes: 1000,
            overlap_seconds: 0.1,
        };
        let progress = DecodeProgress::new(100);
        let result = decode_ltc_chunked(&path, true, 25.0, false, config, &progress).unwrap();
        assert_eq!(result.valid_frames, 0,
            "float WAV should decode 0 valid frames, got {}", result.valid_frames);
        let total = progress.chunks_completed.load(std::sync::atomic::Ordering::Relaxed);
        assert!(total > 0, "progress should have completed at least 1 chunk");
        let has_read_error = result.details.iter().any(|d| d.contains("Failed to read"));
        assert!(has_read_error, "expected detail mentioning 'Failed to read', got: {:?}", result.details);
    }
}
