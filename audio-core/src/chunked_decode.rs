//! Chunked LTC decode: boundary planning, per-chunk decode, and result merge.

use log::{debug, info, warn};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::LtcDecodeError;
use crate::decoder::LtcDecoder;
use crate::ltc_decoder::{
    apply_coherent_first_timecode, compute_ltc_quality,
    CONFIDENCE_LOW_THRESHOLD, CONFIDENCE_SUCCESS_THRESHOLD, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult,
};
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

/// Everything the chunked decode needs to know about the WAV being decoded:
/// the exact chunk boundaries plus the stream geometry the merge uses.
pub(crate) struct ChunkPlan {
    /// `[start, end)` mono-sample ranges, ascending.
    pub boundaries: Vec<(usize, usize)>,
    pub sample_rate: u32,
    pub total_mono: usize,
    pub total_duration: f64,
    pub overlap_seconds: f64,
    /// Channel the mono-sample readers extract; picked once per file so
    /// every chunk decodes the same channel.
    pub active_channel: usize,
}

/// Build the chunk plan from an open reader: chunk geometry + boundaries.
fn plan_chunks(reader: &WavChunkReader, config: &DecodeConfig) -> ChunkPlan {
    let sample_rate = reader.sample_rate();
    let total_mono = reader.total_mono_samples();
    let bytes_per_mono_sample =
        (reader.channels() as u64) * (reader.spec().bits_per_sample as u64 / 8);
    let (chunk_mono, overlap) = chunk_geometry(config, bytes_per_mono_sample, sample_rate);
    ChunkPlan {
        boundaries: plan_chunk_boundaries(total_mono, chunk_mono, overlap),
        sample_rate,
        total_mono,
        total_duration: total_mono as f64 / sample_rate as f64,
        overlap_seconds: config.overlap_seconds,
        active_channel: 0,
    }
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
/// If channel 0 is (near-)silent, the channel carrying signal is decoded
/// instead; a non-silent channel 0 is always kept.
pub fn decode_ltc_chunked(
    path: &Path,
    use_libltc: bool,
    fps: f64,
    drop_frame: bool,
    config: DecodeConfig,
    progress: &DecodeProgress,
) -> Result<LtcDetectionResult, LtcDecodeError> {
    let (mut chunk_reader, overall_start) = WavChunkReader::open(path).map_err(LtcDecodeError::Failed)?;
    let mut plan = plan_chunks(&chunk_reader, &config);

    debug!("decode_ltc_chunked: {} samples @ {} Hz, config chunk={} bytes, overlap={}s",
        plan.total_mono, plan.sample_rate, config.chunk_size_bytes, config.overlap_seconds);

    if plan.total_mono == 0 {
        warn!("decode_ltc_chunked: WAV file contains no samples");
        return Ok(LtcDetectionResult::error("Audio file contains no samples"));
    }

    // Pick the decode channel once per file, before chunk planning, so every
    // chunk decodes the same channel: if channel 0 is (near-)silent, the
    // channel carrying signal is decoded instead.
    let peaks = chunk_reader.scan_channel_peaks().map_err(LtcDecodeError::Failed)?;
    plan.active_channel = crate::ltc_decoder::pick_active_channel(&peaks);
    if plan.active_channel != 0 {
        info!("decode_ltc_chunked: channel 0 is silent, decoding channel {}", plan.active_channel);
    }

    let num_chunks = plan.boundaries.len();
    info!("decode_ltc_chunked: split into {} chunks, overlap={}s",
        num_chunks, plan.overlap_seconds);

    if num_chunks == 0 {
        return Ok(LtcDetectionResult::error("No audio data to decode"));
    }

    let decoder = crate::decoder::decoder_for(use_libltc);
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .min(num_chunks);

    let mut chunk_results = if num_workers <= 1 {
        run_sequential(path, &plan, decoder, fps, drop_frame, progress)
    } else {
        run_parallel(path, &plan, decoder, fps, drop_frame, progress, num_workers)
    };

    chunk_results.sort_by_key(|cr| cr.chunk_idx);
    if progress.cancel_flag.load(Ordering::Relaxed) {
        return Err(LtcDecodeError::Cancelled);
    }

    let merged = merge_results(&chunk_results, &plan, fps);

    let processing_time_ms = overall_start.elapsed().as_secs_f64() * 1000.0;
    let mut result = LtcDetectionResult {
        status: merged.status,
        detected_fps: fps as f32,
        drop_frame,
        total_possible_frames: merged.total_possible,
        valid_frames: merged.valid_frames,
        timecodes: merged.timecodes,
        avg_confidence: merged.avg_confidence,
        details: merged.details,
        total_audio_duration_secs: plan.total_duration,
        sample_rate: plan.sample_rate,
        processing_time_ms,
        first_ltc_timecode_secs: merged.first_ltc_timecode_secs,
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
    result: Result<LtcDetectionResult, LtcDecodeError>,
}

/// Decode chunks strictly in order, honoring cancellation between chunks.
fn run_sequential(
    path: &Path,
    plan: &ChunkPlan,
    decoder: &dyn LtcDecoder,
    fps: f64,
    drop_frame: bool,
    progress: &DecodeProgress,
) -> Vec<ChunkResult> {
    let cancel_flag = &progress.cancel_flag;
    let mut results = Vec::with_capacity(plan.boundaries.len());
    for (chunk_idx, &(start_sample, end_sample)) in plan.boundaries.iter().enumerate() {
        if cancel_flag.load(Ordering::Relaxed) {
            info!("decode_ltc_chunked: cancel requested, stopping at chunk {}", chunk_idx);
            break;
        }
        let r = decode_one_chunk(
            path, decoder, chunk_idx, start_sample, end_sample,
            plan.sample_rate, fps, drop_frame, cancel_flag, plan.active_channel,
        );
        progress.chunks_completed.fetch_add(1, Ordering::Relaxed);
        results.push(r);
    }
    results
}

/// Decode chunks across `num_workers` threads; every chunk slot is filled —
/// workers bail on cancellation, leaving `LtcDecodeError::Cancelled` placeholders.
fn run_parallel(
    path: &Path,
    plan: &ChunkPlan,
    decoder: &dyn LtcDecoder,
    fps: f64,
    drop_frame: bool,
    progress: &DecodeProgress,
    num_workers: usize,
) -> Vec<ChunkResult> {
    let num_chunks = plan.boundaries.len();
    let cancel_flag = progress.cancel_flag.clone();
    let progress_completed = progress.chunks_completed.clone();
    let chunks = Arc::new(plan.boundaries.clone());
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
                    path, decoder, idx, start_sample, end_sample,
                    plan.sample_rate, fps, drop_frame, &cancel_flag, plan.active_channel,
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
                result: Err(LtcDecodeError::Cancelled),
            }),
        }
    }
    collected
}

/// Stream-level outcome of merging all per-chunk results.
struct MergedDecode {
    timecodes: Vec<FrameTimecode>,
    details: Vec<String>,
    first_ltc_timecode_secs: f64,
    avg_confidence: f32,
    total_possible: u32,
    valid_frames: u32,
    status: LtcDecodeStatus,
}

/// Merge per-chunk results into one stream-level decode: offset each chunk's
/// timecodes by its start sample, sort, dedup within
/// `min(frame_dur·0.5, overlap·0.5)`, reindex, aggregate confidence on the
/// 0.0–1.0 fraction scale, classify status via the shared CONFIDENCE_* thresholds.
fn merge_results(chunk_results: &[ChunkResult], plan: &ChunkPlan, fps: f64) -> MergedDecode {
    let sample_rate = plan.sample_rate;
    let mut all_timecodes: Vec<(usize, FrameTimecode)> = Vec::new();
    let mut merged_details: Vec<String> = Vec::new();
    let mut first_tc_secs: f64 = f64::MAX;

    for cr in chunk_results {
        match &cr.result {
            Ok(r) => {
                merged_details.push(format!("Chunk {}: {} valid / {} possible (conf {:.1}%)",
                    cr.chunk_idx, r.valid_frames, r.total_possible_frames, r.avg_confidence * 100.0));
                let chunk_start_sample = plan.boundaries.get(cr.chunk_idx).map(|&(s, _)| s).unwrap_or(0);
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
    let dedup_threshold = (frame_duration * 0.5).min(plan.overlap_seconds * 0.5);
    let mut deduped: Vec<FrameTimecode> = Vec::with_capacity(all_timecodes.len());
    // NEG_INFINITY (not `-dedup_threshold`): a first frame reported at or
    // slightly below 0 s (libltc emits frame 0 with a small negative
    // off_start during warm-up) must survive the dedup.
    let mut last_secs: f64 = f64::NEG_INFINITY;
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
    let true_total_possible = (plan.total_duration * fps).round() as u32;
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
        plan.boundaries.len(), valid_frames, true_total_possible,
    ));

    MergedDecode {
        timecodes: deduped,
        details: merged_details,
        first_ltc_timecode_secs: if first_tc_secs < f64::MAX { first_tc_secs } else { 0.0 },
        avg_confidence,
        total_possible: true_total_possible,
        valid_frames,
        status,
    }
}

/// Decode a single chunk of a WAV file in a worker thread.
#[allow(clippy::too_many_arguments)]
fn decode_one_chunk(
    path: &Path,
    decoder: &dyn LtcDecoder,
    chunk_idx: usize,
    start_sample: usize,
    end_sample: usize,
    sample_rate: u32,
    fps: f64,
    drop_frame: bool,
    cancel_flag: &AtomicBool,
    active_channel: usize,
) -> ChunkResult {
    let num_samples = end_sample - start_sample;

    if cancel_flag.load(Ordering::Relaxed) {
        return ChunkResult { chunk_idx, result: Err(LtcDecodeError::Cancelled) };
    }

    let mut local_reader = match WavChunkReader::open_with_channel(path, active_channel) {
        Ok((r, _)) => r,
        Err(e) => return ChunkResult {
            chunk_idx,
            result: Err(LtcDecodeError::Failed(format!("Failed to open file for chunk {}: {}", chunk_idx, e))),
        },
    };

    let chunk_start = Instant::now();
    let result = decoder.decode_chunk(
        path, &mut local_reader, chunk_idx, start_sample, num_samples,
        sample_rate, fps, drop_frame, chunk_start, cancel_flag,
    );
    let elapsed = chunk_start.elapsed();
    debug!("Chunk {} decoded ({}): {:.1}ms", chunk_idx + 1, decoder.name(), elapsed.as_secs_f64() * 1000.0);
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

    // ── merge_results unit tests (synthetic, no WAV files) ────────────

    fn test_plan(boundaries: Vec<(usize, usize)>, total_duration: f64, overlap: f64) -> ChunkPlan {
        ChunkPlan {
            boundaries,
            sample_rate: 48000,
            total_mono: (total_duration * 48000.0) as usize,
            total_duration,
            overlap_seconds: overlap,
            active_channel: 0,
        }
    }

    fn ftc(index: u32, secs: f64) -> FrameTimecode {
        FrameTimecode {
            frame_index: index,
            timecode: Timecode { hours: 0, minutes: 0, seconds: 0, frames: index },
            timecode_secs: secs,
        }
    }

    fn chunk_ok(first_secs: f64, frame_count: u32) -> LtcDetectionResult {
        LtcDetectionResult {
            status: LtcDecodeStatus::Success,
            detected_fps: 25.0,
            drop_frame: false,
            total_possible_frames: frame_count,
            valid_frames: frame_count,
            timecodes: (0..frame_count)
                .map(|i| ftc(i, first_secs + i as f64 / 25.0))
                .collect(),
            avg_confidence: 1.0,
            details: vec![],
            total_audio_duration_secs: 0.0,
            sample_rate: 48000,
            processing_time_ms: 0.0,
            first_ltc_timecode_secs: first_secs,
            quality: None,
        }
    }

    #[test]
    fn test_merge_offsets_timecodes_by_chunk_start() {
        // Chunk starting at 5.0 s (240 000 mono samples @48 kHz); a frame
        // decoded at chunk-local 1.0 s must land at 6.0 s.
        let plan = test_plan(vec![(240_000, 480_000)], 10.0, 2.0);
        let results = vec![ChunkResult { chunk_idx: 0, result: Ok(chunk_ok(1.0, 2)) }];
        let merged = merge_results(&results, &plan, 25.0);
        assert_eq!(merged.timecodes.len(), 2);
        assert!((merged.timecodes[0].timecode_secs - 6.0).abs() < 1e-9,
            "expected 6.0, got {}", merged.timecodes[0].timecode_secs);
        assert!((merged.timecodes[1].timecode_secs - 6.04).abs() < 1e-9);
    }

    #[test]
    fn test_merge_dedups_overlap_and_renumbers() {
        // fps 25 → frame duration 0.04 s; overlap 2.0 s → dedup threshold
        // min(0.02, 1.0) = 0.02 s. Chunk 1 starts at 0.5 s: its frames are
        // given in chunk-local seconds and must be offset by +0.5 before
        // dedup (0.045 → 0.545, 0.53 → 1.03).
        let plan = test_plan(vec![(0, 24_000), (24_000, 48_000)], 2.0, 2.0);
        let mut chunk0 = chunk_ok(0.0, 1);
        chunk0.timecodes = vec![ftc(0, 0.001), ftc(1, 0.541)];
        let mut chunk1 = chunk_ok(0.0, 1);
        chunk1.timecodes = vec![ftc(0, 0.046), ftc(1, 0.531)];
        let results = vec![
            ChunkResult { chunk_idx: 0, result: Ok(chunk0) },
            ChunkResult { chunk_idx: 1, result: Ok(chunk1) },
        ];
        let merged = merge_results(&results, &plan, 25.0);
        let secs: Vec<f64> = merged.timecodes.iter().map(|t| t.timecode_secs).collect();
        // 0.001 kept (0.0 would sit exactly on the initial -threshold gate);
        // 0.546 dropped (diff 0.005 <= 0.02 vs 0.541); 1.031 kept.
        assert_eq!(secs.len(), 3, "got {:?}", secs);
        for (got, want) in secs.iter().zip([0.001, 0.541, 1.031]) {
            assert!((got - want).abs() < 1e-9, "got {} want {}", got, want);
        }
        for (i, t) in merged.timecodes.iter().enumerate() {
            assert_eq!(t.frame_index, i as u32, "frame_index must be renumbered 0..n");
        }
    }

    #[test]
    fn test_merge_keeps_frame_just_beyond_dedup_threshold() {
        // fps 25 → frame duration 0.04 s; overlap 2.0 s → dedup threshold
        // min(0.02, 1.0) = 0.02 s. The keep condition is a strict `>` on
        // the gap from the last kept frame.
        let plan = test_plan(vec![(0, 24_000)], 0.5, 2.0);
        let results = vec![
            ChunkResult { chunk_idx: 0, result: Ok(two_frames_at(0.50, 0.5201)) },
        ];
        let merged = merge_results(&results, &plan, 25.0);
        assert_eq!(merged.timecodes.len(), 2, "frame just beyond threshold must be kept");
        assert!((merged.timecodes[1].timecode_secs - 0.5201).abs() < 1e-9);

        // Gap below the threshold → dropped (strict >; an "exactly equal"
        // case is not stable under float rounding, so pin just-below).
        let results = vec![
            ChunkResult { chunk_idx: 0, result: Ok(two_frames_at(0.50, 0.519)) },
        ];
        let merged = merge_results(&results, &plan, 25.0);
        assert_eq!(merged.timecodes.len(), 1, "frame below the threshold must be dropped");
    }

    fn two_frames_at(secs0: f64, secs1: f64) -> LtcDetectionResult {
        let mut r = chunk_ok(secs0, 2);
        r.timecodes = vec![ftc(0, secs0), ftc(1, secs1)];
        r
    }

    #[test]
    fn test_merge_first_tc_is_min_over_chunks() {
        let plan = test_plan(vec![(0, 240_000), (240_000, 480_000)], 20.0, 2.0);
        let results = vec![
            ChunkResult { chunk_idx: 0, result: Ok(chunk_ok(1.0, 1)) },   // 1.0 + 0.0
            ChunkResult { chunk_idx: 1, result: Ok(chunk_ok(0.5, 1)) },   // 0.5 + 5.0 = 5.5
        ];
        let merged = merge_results(&results, &plan, 25.0);
        assert!((merged.first_ltc_timecode_secs - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_merge_all_error_chunks_zeroed() {
        let plan = test_plan(vec![(0, 24_000), (24_000, 48_000)], 2.0, 2.0);
        let results = vec![
            ChunkResult { chunk_idx: 0, result: Err(LtcDecodeError::Failed("boom 0".into())) },
            ChunkResult { chunk_idx: 1, result: Err(LtcDecodeError::Failed("boom 1".into())) },
        ];
        let merged = merge_results(&results, &plan, 25.0);
        assert_eq!(merged.valid_frames, 0);
        assert_eq!(merged.first_ltc_timecode_secs, 0.0);
        assert!(matches!(merged.status, LtcDecodeStatus::NoSyncWord));
        assert!(merged.details.iter().any(|d| d.contains("Chunk 0: error - boom 0")));
        assert!(merged.details.iter().any(|d| d.contains("Chunk 1: error - boom 1")));
    }

    #[test]
    fn test_merge_error_chunks_do_not_abort_merge() {
        let plan = test_plan(vec![(0, 24_000), (24_000, 48_000)], 2.0, 2.0);
        let results = vec![
            ChunkResult { chunk_idx: 0, result: Err(LtcDecodeError::Failed("read failure".into())) },
            ChunkResult { chunk_idx: 1, result: Ok(chunk_ok(0.0, 1)) },
        ];
        let merged = merge_results(&results, &plan, 25.0);
        assert_eq!(merged.valid_frames, 1);
        assert!(merged.details.iter().any(|d| d.contains("Chunk 0: error - read failure")));
    }

    #[test]
    fn test_merge_confidence_status_boundaries() {
        // 10 s * 25 fps = 250 possible frames.
        let plan_for = || test_plan(vec![(0, 480_000)], 10.0, 2.0);

        // 175/250 = 0.70 → Success (>= CONFIDENCE_SUCCESS_THRESHOLD).
        // First frame at 0.001 s: 0.0 would sit exactly on the initial
        // -threshold gate of the dedup loop and be dropped.
        let results = vec![ChunkResult { chunk_idx: 0, result: Ok(chunk_ok(0.001, 175)) }];
        let merged = merge_results(&results, &plan_for(), 25.0);
        assert_eq!(merged.total_possible, 250);
        assert!((merged.avg_confidence - 0.70).abs() < 1e-6, "got {}", merged.avg_confidence);
        assert!(matches!(merged.status, LtcDecodeStatus::Success));

        // 75/250 = 0.30 → LowConfidence (>= CONFIDENCE_LOW_THRESHOLD, < success)
        let results = vec![ChunkResult { chunk_idx: 0, result: Ok(chunk_ok(0.001, 75)) }];
        let merged = merge_results(&results, &plan_for(), 25.0);
        assert!((merged.avg_confidence - 0.30).abs() < 1e-6, "got {}", merged.avg_confidence);
        assert!(matches!(merged.status, LtcDecodeStatus::LowConfidence));

        // 74/250 = 0.296 → NoSyncWord
        let results = vec![ChunkResult { chunk_idx: 0, result: Ok(chunk_ok(0.001, 74)) }];
        let merged = merge_results(&results, &plan_for(), 25.0);
        assert!(matches!(merged.status, LtcDecodeStatus::NoSyncWord));
    }

    // ── run_sequential / run_parallel with MockDecoder ────────────────

    /// Canned per-chunk decoder: `results[chunk_idx]` is returned verbatim
    /// (index-addressed, so parallel and sequential runs are deterministic).
    struct MockDecoder {
        results: Vec<Result<LtcDetectionResult, LtcDecodeError>>,
        cancel_flag: Option<Arc<AtomicBool>>,
        cancel_after_calls: Option<usize>,
        calls: AtomicUsize,
    }

    impl MockDecoder {
        fn new(results: Vec<Result<LtcDetectionResult, LtcDecodeError>>) -> Self {
            Self { results, cancel_flag: None, cancel_after_calls: None, calls: AtomicUsize::new(0) }
        }

        fn bailing(results: Vec<Result<LtcDetectionResult, LtcDecodeError>>,
                   cancel_flag: Arc<AtomicBool>, after: usize) -> Self {
            Self { results, cancel_flag: Some(cancel_flag), cancel_after_calls: Some(after), calls: AtomicUsize::new(0) }
        }
    }

    impl LtcDecoder for MockDecoder {
        fn name(&self) -> &'static str { "mock" }

        fn decode_wav(&self, _path: &Path, _fps: f64, _drop: bool,
                      _cancel: Option<&AtomicBool>) -> Result<LtcDetectionResult, LtcDecodeError> {
            Err(LtcDecodeError::Failed("mock: decode_wav not supported".to_string()))
        }

        fn decode_chunk(&self, _path: &Path, _reader: &mut WavChunkReader, chunk_idx: usize,
                        _start: usize, _len: usize, _sample_rate: u32, _fps: f64, _drop: bool,
                        _start_time: Instant, _cancel: &AtomicBool) -> Result<LtcDetectionResult, LtcDecodeError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if let (Some(flag), Some(after)) = (&self.cancel_flag, self.cancel_after_calls) {
                if n + 1 >= after {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            self.results[chunk_idx].clone()
        }
    }

    fn five_chunk_plan() -> ChunkPlan {
        test_plan(
            vec![(0, 48_000), (43_200, 91_200), (86_400, 134_400), (129_600, 177_600), (172_800, 220_800)],
            4.6, 1.0,
        )
    }

    fn five_ok_results() -> Vec<Result<LtcDetectionResult, LtcDecodeError>> {
        (0..5)
            .map(|i| Ok(chunk_ok(i as f64 * 0.9, 2)))
            .collect()
    }

    /// `decode_one_chunk` opens a real WavChunkReader before reaching the
    /// decoder, so the run_* tests need a real (tiny) WAV on disk.
    fn write_mock_wav() -> tempfile::TempDir {
        let dir = tempfile::TempDir::new().unwrap();
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(dir.path().join("mock.wav"), spec).unwrap();
        for i in 0..100i16 {
            writer.write_sample(i).unwrap();
        }
        writer.finalize().unwrap();
        dir
    }

    #[test]
    fn test_run_parallel_matches_run_sequential() {
        let plan = five_chunk_plan();
        let dir = write_mock_wav();
        let path = dir.path().join("mock.wav");

        let seq_results = run_sequential(
            &path, &plan, &MockDecoder::new(five_ok_results()), 25.0, false, &DecodeProgress::new(5),
        );
        let par_results = run_parallel(
            &path, &plan, &MockDecoder::new(five_ok_results()), 25.0, false, &DecodeProgress::new(5), 3,
        );

        let mut seq = seq_results;
        seq.sort_by_key(|cr| cr.chunk_idx);
        let mut par = par_results;
        par.sort_by_key(|cr| cr.chunk_idx);

        assert_eq!(seq.len(), 5);
        assert_eq!(par.len(), 5);
        for (s, p) in seq.iter().zip(par.iter()) {
            assert_eq!(s.chunk_idx, p.chunk_idx);
            let s_res = s.result.as_ref().unwrap();
            let p_res = p.result.as_ref().unwrap();
            let s_secs: Vec<f64> = s_res.timecodes.iter().map(|t| t.timecode_secs).collect();
            let p_secs: Vec<f64> = p_res.timecodes.iter().map(|t| t.timecode_secs).collect();
            assert_eq!(s_secs, p_secs, "chunk {} mismatch", s.chunk_idx);
        }

        let merged_seq = merge_results(&seq, &plan, 25.0);
        let merged_par = merge_results(&par, &plan, 25.0);
        let seq_secs: Vec<f64> = merged_seq.timecodes.iter().map(|t| t.timecode_secs).collect();
        let par_secs: Vec<f64> = merged_par.timecodes.iter().map(|t| t.timecode_secs).collect();
        assert_eq!(seq_secs, par_secs);
        assert_eq!(merged_seq.valid_frames, merged_par.valid_frames);
    }

    #[test]
    fn test_run_parallel_pre_cancelled_fills_all_slots_canceled() {
        let plan = five_chunk_plan();
        let dir = write_mock_wav();
        let path = dir.path().join("mock.wav");
        let progress = DecodeProgress::new(5);
        progress.cancel();

        let results = run_parallel(
            &path, &plan, &MockDecoder::new(five_ok_results()), 25.0, false, &progress, 3,
        );
        assert_eq!(results.len(), 5);
        for cr in &results {
            assert_eq!(cr.result.as_ref().unwrap_err(), &LtcDecodeError::Cancelled);
        }
        assert_eq!(progress.chunks_completed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_run_parallel_worker_bail_leaves_canceled_slots() {
        let plan = five_chunk_plan();
        let dir = write_mock_wav();
        let path = dir.path().join("mock.wav");
        let progress = DecodeProgress::new(5);

        // Mock bails (sets the cancel flag) on its 2nd call; that caller
        // itself breaks out without storing, and the remaining workers
        // leave their slots as LtcDecodeError::Cancelled placeholders. Whether the
        // 1st caller still stores depends on scheduling, so assert the
        // invariant: at most the first call's result is stored, every slot
        // is filled, and progress counts exactly the stored results.
        let mock = MockDecoder::bailing(five_ok_results(), progress.cancel_flag.clone(), 2);
        let results = run_parallel(&path, &plan, &mock, 25.0, false, &progress, 3);

        assert_eq!(results.len(), 5);
        let completed = results.iter().filter(|cr| cr.result.is_ok()).count();
        let canceled = results.iter().filter(|cr| cr.result.is_err()).count();
        assert!(completed <= 1, "at most the first call completes, got {}", completed);
        assert_eq!(completed + canceled, 5, "every slot is filled");
        assert_eq!(
            progress.chunks_completed.load(Ordering::Relaxed),
            completed,
            "progress counts exactly the stored results"
        );
    }

    #[test]
    fn test_run_sequential_pre_cancelled_returns_empty() {
        let plan = five_chunk_plan();
        let dir = write_mock_wav();
        let path = dir.path().join("mock.wav");
        let progress = DecodeProgress::new(5);
        progress.cancel();

        let results = run_sequential(
            &path, &plan, &MockDecoder::new(five_ok_results()), 25.0, false, &progress,
        );
        assert!(results.is_empty());
        assert_eq!(progress.chunks_completed.load(Ordering::Relaxed), 0);
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

        assert_eq!(chunked.valid_frames, direct.valid_frames,
            "single-chunk merge is identity: chunked={} != direct={}",
            chunked.valid_frames, direct.valid_frames);
        assert_eq!(chunked.total_possible_frames, direct.total_possible_frames,
            "total_possible must match direct decode");
        assert_eq!(chunked.timecodes.first().map(|t| t.timecode),
            direct.timecodes.first().map(|t| t.timecode),
            "first decoded TC must match direct decode");
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

        assert_eq!(chunked.valid_frames, direct.valid_frames,
            "chunked libltc must equal direct libltc exactly: chunked={} vs direct={}",
            chunked.valid_frames, direct.valid_frames);
        assert_eq!(chunked.timecodes.first().map(|t| t.timecode),
            direct.timecodes.first().map(|t| t.timecode),
            "first decoded TC must match direct libltc");
        assert_eq!(chunked.timecodes.last().map(|t| t.timecode),
            direct.timecodes.last().map(|t| t.timecode),
            "last decoded TC must match direct libltc");
        assert!(chunked.total_possible_frames >= chunked.valid_frames);
    }

    #[test]
    fn test_merge_keeps_first_frame_at_or_below_zero_secs() {
        // libltc reports frame 0's off_start slightly negative (decoder
        // warm-up). The dedup sentinel must not eat a first frame whose
        // offset lands at or below 0 — reproduces the chunked-libltc
        // off-by-one at the merge level.
        let plan = test_plan(vec![(0, 480_000)], 10.0, 0.3);
        let mut chunk0 = chunk_ok(0.0, 3);
        chunk0.timecodes = vec![ftc(0, -0.0005), ftc(1, 0.0400), ftc(2, 0.0800)];
        let results = vec![ChunkResult { chunk_idx: 0, result: Ok(chunk0) }];
        let merged = merge_results(&results, &plan, 25.0);
        assert_eq!(merged.timecodes.len(), 3, "all three frames must survive the merge");
        assert!((merged.timecodes[0].timecode_secs - (-0.0005)).abs() < 1e-9,
            "first frame must be the negative-offset frame 0, got {}",
            merged.timecodes[0].timecode_secs);
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
        generate_ltc_wav_sel(path, start_tc, fps, drop_frame, sample_rate, num_frames, ChannelSel::Both);
    }

    fn generate_ltc_wav_sel(
        path: &Path,
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        num_frames: u32,
        channel: ChannelSel,
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
                channel,
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
    fn test_chunked_right_channel_wav_decodes() {
        // The chunked path must pick the decode channel once per file,
        // before chunk planning: a right-only WAV (silent ch0) decodes via
        // the auto-selected right channel, exercising the WavChunkReader
        // plumbing.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("right_only.wav");
        generate_ltc_wav_sel(
            &path,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 50,
            ChannelSel::Right,
        );

        let config = DecodeConfig { chunk_size_bytes: 10_000_000, overlap_seconds: 2.0 };
        let progress = DecodeProgress::new(1);
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for right-only WAV, got {:?}", result.status);
        assert!(result.valid_frames >= 40,
            "should decode at least 40 frames, got {}", result.valid_frames);
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
        assert_eq!(err, LtcDecodeError::Cancelled);
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

