use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

use crate::LtcDecodeError;
use crate::Timecode;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum LtcDecodeStatus {
    Success,
    NoSyncWord,
    LowConfidence,
    Error { message: String },
}

/// Status thresholds shared by both decoders (builtin and libltc).
/// `avg_confidence` is a 0.0–1.0 fraction of expected frames decoded.
pub const CONFIDENCE_SUCCESS_THRESHOLD: f32 = 0.70;
pub const CONFIDENCE_LOW_THRESHOLD: f32 = 0.30;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameTimecode {
    pub frame_index: u32,
    pub timecode: Timecode,
    pub timecode_secs: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LtcDetectionResult {
    pub status: LtcDecodeStatus,
    pub detected_fps: f32,
    pub drop_frame: bool,
    pub total_possible_frames: u32,
    pub valid_frames: u32,
    pub timecodes: Vec<FrameTimecode>,
    /// Fraction of expected frames successfully decoded (0.0–1.0).
    /// Both decoders publish this on the same scale; multiply by 100 for
    /// display.
    pub avg_confidence: f32,
    pub details: Vec<String>,
    pub total_audio_duration_secs: f64,
    pub sample_rate: u32,
    pub processing_time_ms: f64,
    pub first_ltc_timecode_secs: f64,
    pub quality: Option<LtcQualityReport>,
    /// Structured per-chunk outcome of a chunked decode. Populated only by
    /// `decode_ltc_chunked`'s merge path; every other constructor leaves it
    /// empty. The human-rendered `details` lines carry the same information.
    pub chunk_summaries: Vec<ChunkSummary>,
}

/// Structured per-chunk outcome of a chunked LTC decode — the typed
/// counterpart of one `Chunk N: ...` details line.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChunkSummary {
    pub chunk_idx: usize,
    pub valid_frames: u32,
    pub total_possible_frames: u32,
    pub avg_confidence: f32,
    /// `Some` when the chunk failed (or was cancelled); the counts are then
    /// meaningless (zeroed).
    pub error: Option<LtcDecodeError>,
}

/// Qualitative grade bucket for a [`LtcQualityReport`], derived from the
/// overall score.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum QualityGrade {
    Excellent,
    Good,
    Fair,
    Poor,
    Bad,
}

impl std::fmt::Display for QualityGrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl QualityGrade {
    pub fn as_str(self) -> &'static str {
        match self {
            QualityGrade::Excellent => "Excellent",
            QualityGrade::Good => "Good",
            QualityGrade::Fair => "Fair",
            QualityGrade::Poor => "Poor",
            QualityGrade::Bad => "Bad",
        }
    }

    pub fn from_score(score: f64) -> QualityGrade {
        if score >= 0.95 {
            QualityGrade::Excellent
        } else if score >= 0.80 {
            QualityGrade::Good
        } else if score >= 0.60 {
            QualityGrade::Fair
        } else if score >= 0.30 {
            QualityGrade::Poor
        } else {
            QualityGrade::Bad
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LtcQualityReport {
    /// Overall quality score 0.0–1.0
    pub score: f64,
    /// Qualitative grade bucket derived from `score`
    pub grade: QualityGrade,
    /// Number of undetected frames within the span between the first and last
    /// decoded frame (silent lead-in/out and pre-LTC silence are not counted)
    pub missing_frames: u32,
    /// Number of gaps between contiguous frame blocks
    pub gap_count: u32,
    /// Number of isolated glitch frames (single-frame outliers)
    pub glitch_count: u32,
    /// Number of edit points (jumps where LTC shifts and continues consecutively)
    pub edit_count: u32,
    /// Maximum drift between LTC and audio position (seconds)
    pub max_drift_secs: f64,
    /// Drift rate (seconds of drift per second of audio)
    pub drift_rate: f64,
    /// Largest contiguous block of consecutive frames
    pub largest_block: u32,
    /// Fraction of decoded frames that lie in usable blocks (0.0–1.0).
    /// A block is usable when it is long enough to sync against (>= 2s,
    /// or >= 90% of the recording for very short recordings) and its
    /// internal clock drift accumulates at most 1 frame over its length.
    pub usable_coverage: f64,
    /// Number of contiguous blocks (segments) the decode is split into
    pub block_count: u32,
    /// Worst drift accumulation within a single block, in frames
    pub worst_block_drift_frames: f64,
    /// Number of block boundaries where the timecode jumps backwards
    /// (timecode reset — the same TC values can recur, which makes
    /// syncing by TC value ambiguous in editors)
    pub backward_jump_count: u32,
    /// Human-readable summary of issues found
    pub summary: String,
    /// Indices of gap boundaries: (prev_segment_last_idx, next_segment_first_idx)
    pub gap_edges: Vec<(usize, usize)>,
    /// Indices of individual glitch frames (single-frame outliers)
    pub glitch_indices: Vec<usize>,
}

impl LtcDetectionResult {
    pub fn error(msg: impl Into<String>) -> Self {
        LtcDetectionResult {
            status: LtcDecodeStatus::Error { message: msg.into() },
            detected_fps: 0.0,
            drop_frame: false,
            total_possible_frames: 0,
            valid_frames: 0,
            timecodes: Vec::new(),
            avg_confidence: 0.0,
            details: Vec::new(),
            total_audio_duration_secs: 0.0,
            sample_rate: 0,
            processing_time_ms: 0.0,
            first_ltc_timecode_secs: 0.0,
            quality: None,
            chunk_summaries: Vec::new(),
        }
    }
}

// Sync word at bits 64-79:  0 0 1 1 1 1 1 1 1 1 1 1 1 1 0 1
const SYNC_WORD: [u8; 16] = [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1];
const SYNC_OFFSET: usize = 64;

// ── Public API ───────────────────────────────────────────────────────────────

/// Decode LTC from a pre-loaded buffer of mono f32 samples.
/// This is the core decoding logic, extracted from `decode_ltc_from_wav`.
/// Shared substrate of the decode strategy ladder (coarse scan → refinement
/// → result assembly). Built once per decode pass.
struct DecodeCtx<'a> {
    samples: &'a [f32],
    zc: &'a [usize],
    sample_rate: u32,
    threshold: f32,
    fps: f64,
    drop_frame: bool,
    cancel: Option<&'a AtomicBool>,
}

fn decode_ltc_samples_inner(
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
    fps: f64,
    drop_frame: bool,
    start: std::time::Instant,
    cancel: Option<&AtomicBool>,
) -> Result<LtcDetectionResult, LtcDecodeError> {
    if samples.is_empty() {
        warn!("LTC decode: audio buffer contains no samples");
        return Ok(LtcDetectionResult::error("Audio buffer contains no samples"));
    }

    // WP-DR signal conditioning (DR3 DC blocker + high-pass, DR4 median
    // pre-filter): removes DC offsets, mains hum, and single-sample clicks
    // before any detection constant sees the data. Always-on — no
    // thresholds to mis-tune; the full suite + real-world anchor gate
    // regressions.
    let mut conditioned: Vec<f32> = samples.to_vec();
    condition_signal(&mut conditioned, sample_rate);
    let samples: &[f32] = &conditioned;

    let total_duration = samples.len() as f64 / sample_rate as f64;
    let noise_floor = estimate_noise_floor(samples);
    // WP-DR (DR5): the absolute 0.005 floor becomes peak-relative so quiet
    // recordings decode, capped so a loud peak cannot silence detection.
    // The peak is a histogram percentile (not raw max): even after the
    // median filter, clustered impulse pairs would inflate a raw max.
    let robust_peak = robust_peak_amplitude(samples);
    let threshold = (noise_floor * 0.5).max((robust_peak * PEAK_FLOOR_FRAC).min(ABS_FLOOR_CAP));
    debug!("LTC decode: read {:.2}s of audio, noise_floor={:.8}, threshold={:.8}",
        total_duration, noise_floor, threshold);

    if threshold < 1e-8 {
        warn!("LTC decode: signal is completely silent");
        return Ok(LtcDetectionResult::error("Audio signal is completely silent"));
    }

    info!("LTC decode (+{:.1}s): scanning {} samples for zero-crossings (threshold={:.6})...",
        start.elapsed().as_secs_f64(), samples.len(), threshold);
    let mut zc = find_zero_crossings(samples, threshold);
    debug!("LTC decode: found {} zero-crossings", zc.len());

    // Adaptive ZC: if far more ZCs than expected for clean LTC, noise is causing
    // micro-crossings. Re-run with a stricter threshold to filter them out.
    // Expected ZC upper bound: each LTC frame has ~120 ZC pairs on average at 25fps.
    let expected_max_zcs = ((samples.len() as f64 / sample_rate as f64)
        * fps * 240.0) as usize;
    if zc.len() > expected_max_zcs * 3 && zc.len() > 1000 {
        let stricter = (threshold * 4.0).min(0.5);
        info!("LTC decode: ZC count {} is > {}x expected ({}) -- re-running with stricter threshold {:.6}",
            zc.len(), 3, expected_max_zcs, stricter);
        zc = find_zero_crossings(samples, stricter);
        debug!("LTC decode: re-run with stricter threshold found {} zero-crossings", zc.len());
    }

    if cancelled(cancel) {
        return Err(LtcDecodeError::Cancelled);
    }

    if zc.len() < 8 {
        warn!("LTC decode: only {} zero-crossings, signal may not be LTC", zc.len());
        return Ok(LtcDetectionResult::error(format!(
            "Only {} zero-crossings found (need ≥8) -- signal may be silent or not LTC audio",
            zc.len()
        )));
    }

    // ── Try ZC-interval method (fast, works on synthetic/clean LTC) ─────────
    // The gate uses the grid-only count: chain re-lock recovers post-desync
    // frames but says nothing about how clean the dominant grid is, and a
    // chain-inflated confidence must not skip the detailed decode path (its
    // sample-locked extraction decodes fade/noise regions the ZC bitstream
    // mangles).
    let ctx = DecodeCtx {
        samples,
        zc: &zc,
        sample_rate,
        threshold,
        fps,
        drop_frame,
        cancel,
    };
    let zc_result = try_decode_via_zc_intervals(&zc, sample_rate, fps, drop_frame, samples.len());
    let zc_conf = zc_result.as_ref().map_or(0.0, |r| {
        if r.total_possible > 0 { r.grid_valid as f32 / r.total_possible as f32 } else { 0.0 }
    });

    match zc_result.as_ref() {
        Some(r) => info!("LTC decode: ZC-interval result -- {} valid / {} possible ({:.1}%)",
            r.valid_frames, r.total_possible, zc_conf * 100.0),
        None => debug!("LTC decode: ZC-interval returned no frames"),
    }

    if zc_conf >= 0.70 {
        info!("LTC decode: ZC-interval grid confidence {:.1}% >= 70% -- using directly", zc_conf * 100.0);
        return build_result(&ctx, zc_result, channels, total_duration, start);
    }

    // ── Sliding window search for SPB/phase ─────────────────────────────────
    // Evaluates 30s windows at 15s strides, using ZCs to skip silent regions.
    // First window with >=70% confidence -> single-pass extract_bits on full file.
    let scan = scan_windows(&ctx)?;

    match scan {
        WindowScan::HighConf { params, window_start } => {
            let decoded = decode_full_file(samples, &params, threshold, sample_rate, window_start, &zc, cancel);
            let final_r = prefer_zc_or_detailed(zc_result, Some(decoded), "detailed scan");
            return build_result(&ctx, final_r, channels, total_duration, start);
        }
        WindowScan::Best { params, window_start } => {
            let conf = params.valid_frames as f32 / params.total_possible.max(1) as f32;
            info!("LTC decode: best window eval {:.1}% ({} valid) -- single-pass on full file (spb={:.2}, phase={})",
                conf * 100.0, params.valid_frames, params.spb, params.phase);
            let decoded = decode_full_file(samples, &params, threshold, sample_rate, window_start, &zc, cancel);
            let final_r = prefer_zc_or_detailed(zc_result, Some(decoded), "detailed scan");
            return build_result(&ctx, final_r, channels, total_duration, start);
        }
        WindowScan::NoCandidate => {}
    }

    // ── Fallback: full-file evaluate_on_slice (rare) ────────────────────────
    warn!("LTC decode: sliding window found no valid LTC -- full-file eval fallback");
    let (fallback_result, _) = evaluate_on_slice(&ctx);
    let final_r = prefer_zc_or_detailed(zc_result, fallback_result, "fallback scan");
    build_result(&ctx, final_r, channels, total_duration, start)
}

/// Outcome of the sliding-window scan (:253-317 of the pre-refactor inner).
enum WindowScan {
    /// A window reached >= 70% confidence — decode the full file with its
    /// parameters immediately.
    HighConf { params: ScoredResult, window_start: usize },
    /// No window reached high confidence — use the best window's parameters.
    Best { params: ScoredResult, window_start: usize },
    /// No window produced any candidate.
    NoCandidate,
}

/// The window-scan loop, returning instead of early-returning a full decode.
/// `Err` propagates cancellation from the loop body.
fn scan_windows(ctx: &DecodeCtx) -> Result<WindowScan, LtcDecodeError> {
    let samples = ctx.samples;
    let zc = ctx.zc;
    let sample_rate = ctx.sample_rate;
    const WINDOW_SECS: f64 = 30.0;
    const STRIDE_SECS: f64 = 15.0;
    const HIGH_CONF_THRESHOLD: f32 = 0.70;

    let window_len = (WINDOW_SECS * sample_rate as f64) as usize;
    let stride = (STRIDE_SECS * sample_rate as f64) as usize;
    let max_windows = (samples.len() / stride.max(1)).max(1);

    let mut best_window_result: Option<ScoredResult> = None;
    let mut best_window_valid = 0u32;
    let mut best_window_start = 0usize;

    for window_idx in 0..max_windows {
        if cancelled(ctx.cancel) {
            return Err(LtcDecodeError::Cancelled);
        }
        let window_start = window_idx * stride;
        let window_end = (window_start + window_len).min(samples.len());

        let window_zc_abs = zc_in_range(zc, window_start, window_end);
        if window_zc_abs.len() < 8 {
            if window_end >= samples.len() { break; }
            continue;
        }
        let window_zc: Vec<usize> = window_zc_abs.iter().map(|p| p - window_start).collect();

        debug!("LTC eval: window {}/{} [+{:.0}s..{:.0}s] -- {} ZCs, best={} valid",
            window_idx + 1, max_windows,
            window_start as f64 / sample_rate as f64,
            window_end as f64 / sample_rate as f64,
            window_zc.len(), best_window_valid);

        let window_ctx = DecodeCtx {
            samples: &samples[window_start..window_end],
            zc: &window_zc,
            sample_rate: ctx.sample_rate,
            threshold: ctx.threshold,
            fps: ctx.fps,
            drop_frame: ctx.drop_frame,
            cancel: ctx.cancel,
        };
        let (result, valid) = evaluate_on_slice(&window_ctx);

        if valid > best_window_valid {
            best_window_valid = valid;
            best_window_result = result;
            best_window_start = window_start;

            let conf = best_window_result.as_ref().map_or(0.0, |r| {
                if r.total_possible > 0 { r.valid_frames as f32 / r.total_possible as f32 } else { 0.0 }
            });

            if conf >= HIGH_CONF_THRESHOLD {
                let r = best_window_result.as_ref().unwrap();
                info!("LTC decode: window eval {:.1}% >= 70% -- single-pass on full file (spb={:.2}, phase={})",
                    conf * 100.0, r.spb, r.phase);
                return Ok(WindowScan::HighConf {
                    params: best_window_result.expect("just checked"),
                    window_start: best_window_start,
                });
            }
        }

        if window_end >= samples.len() { break; }
    }

    Ok(match best_window_result {
        Some(params) => WindowScan::Best { params, window_start: best_window_start },
        None => WindowScan::NoCandidate,
    })
}

/// One helper for the epilogue previously repeated 3× in
/// `decode_ltc_samples_inner`. Picks the ZC-interval result iff it strictly
/// beats the detailed decode (or the detailed decode is absent); `context`
/// preserves the distinct log wordings ("detailed scan"/"fallback scan").
fn prefer_zc_or_detailed(
    zc_result: Option<ScoredResult>,
    detailed: Option<ScoredResult>,
    context: &str,
) -> Option<ScoredResult> {
    let zc_better = match (zc_result.as_ref(), detailed.as_ref()) {
        (Some(z), Some(d)) => z.valid_frames > d.valid_frames,
        (Some(_), None) => true,
        _ => false,
    };
    if zc_better {
        let zcr = zc_result.as_ref().unwrap();
        info!(
            "LTC decode: ZC-interval ({}/{}) beats {} ({}/{}) -- using ZC-interval",
            zcr.valid_frames, zcr.total_possible,
            detailed.as_ref().map_or(0, |d| d.valid_frames),
            detailed.as_ref().map_or(0, |d| d.total_possible),
            context
        );
        return zc_result;
    }
    detailed
}

/// Public entry point for LTC decode. Wraps the inner decoder.
pub(crate) fn decode_ltc_samples(
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
    fps: f64,
    drop_frame: bool,
    start: std::time::Instant,
    cancel: Option<&AtomicBool>,
) -> Result<LtcDetectionResult, LtcDecodeError> {
    decode_ltc_samples_inner(samples, sample_rate, channels, fps, drop_frame, start, cancel)
}

/// Decode LTC from a WAV file (builtin decoder, whole-file path).
/// If channel 0 is (near-)silent, the channel carrying signal is decoded
/// instead; a non-silent channel 0 is always kept.
pub fn decode_ltc_from_wav(path: &Path, fps: f64, drop_frame: bool, cancel: Option<&AtomicBool>) -> Result<LtcDetectionResult, LtcDecodeError> {
    let start = std::time::Instant::now();

    let mut reader = hound::WavReader::open(path)
        .map_err(|e| LtcDecodeError::Failed(format!("Failed to open WAV file: {}", e)))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as usize;

    info!("Decoding LTC from: {} ({} Hz, {} ch, {} fps)", path.display(), sample_rate, channels, fps);

    // Pass 1: per-channel peak scan — if channel 0 is (near-)silent, the
    // channel carrying signal is decoded instead (the generator supports
    // ChannelSel::Right output; WAV decode has no explicit channel override).
    let peaks = scan_channel_peaks_hound(&mut reader, &spec);
    let active_channel = pick_active_channel(&peaks);
    if active_channel != 0 {
        info!("LTC decode: channel 0 is silent, decoding channel {}", active_channel);
    }

    // Pass 2: read the selected channel (fresh reader — pass 1 exhausted it).
    drop(reader);
    let mut reader = hound::WavReader::open(path)
        .map_err(|e| LtcDecodeError::Failed(format!("Failed to open WAV file: {}", e)))?;
    let spec = reader.spec();

    info!("LTC decode (+{:.1}s): reading audio samples from disk...", start.elapsed().as_secs_f64());
    let samples = read_mono_samples(&mut reader, &spec, active_channel)
        .map_err(|e| LtcDecodeError::Failed(format!("Failed to read audio samples: {}", e)))?;

    drop(reader);

    if let Some(c) = cancel {
        if c.load(Ordering::Relaxed) {
            return Err(LtcDecodeError::Cancelled);
        }
    }

    decode_ltc_samples(&samples, sample_rate, channels, fps, drop_frame, start, cancel)
}

/// Peak amplitude below which a channel counts as silent (~1 LSB of 16-bit).
pub(crate) const SILENT_CHANNEL_PEAK: f32 = 2.0 / 32768.0;

/// Decode channel 0 unless it is (near-)silent; then the strongest other
/// channel (ties → lowest index; all-silent → 0). Back-compat: a non-silent
/// ch0 is always kept, even if another channel is louder.
pub(crate) fn pick_active_channel(peaks: &[f32]) -> usize {
    if peaks.is_empty() || peaks[0] > SILENT_CHANNEL_PEAK {
        return 0;
    }
    let mut best = 0usize;
    let mut best_peak = 0.0f32;
    for (i, &p) in peaks.iter().enumerate().skip(1) {
        if p > best_peak {
            best = i;
            best_peak = p;
        }
    }
    if best_peak > SILENT_CHANNEL_PEAK { best } else { 0 }
}

/// One streaming pass over the file computing the absolute peak per channel.
fn scan_channel_peaks_hound<R: std::io::Read>(
    reader: &mut hound::WavReader<R>,
    spec: &hound::WavSpec,
) -> Vec<f32> {
    let channels = (spec.channels as usize).max(1);
    let mut peaks = vec![0f32; channels];
    match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = (1i64 << (spec.bits_per_sample - 1)) as f32;
            for (i, s) in reader.samples::<i32>().filter_map(|s| s.ok()).enumerate() {
                let v = (s as f32 / max_val).abs();
                let ch = i % channels;
                if v > peaks[ch] {
                    peaks[ch] = v;
                }
            }
        }
        hound::SampleFormat::Float => {
            for (i, s) in reader.samples::<f32>().filter_map(|s| s.ok()).enumerate() {
                let v = s.abs();
                let ch = i % channels;
                if v > peaks[ch] {
                    peaks[ch] = v;
                }
            }
        }
    }
    peaks
}

/// Outcome of one (spb, phase) decode attempt.
#[derive(Debug)]
enum ScoredCandidate {
    /// Fewer than 80 bits extractable — not counted as an attempt.
    TooShort,
    /// Bits extracted but the candidate did not beat `best_valid`.
    NoBeat,
    /// The candidate is the new best.
    Beat(ScoredResult),
}

/// One (spb, phase) decode attempt; `Beat(new)` iff it beats `best_valid`.
/// `adaptive` selects extract_bits_adaptive (refinement loop) vs extract_bits.
fn score_candidate(
    ctx: &DecodeCtx,
    spb: f64,
    phase: usize,
    adaptive: bool,
    best_valid: u32,
) -> ScoredCandidate {
    let bits = if adaptive {
        extract_bits_adaptive(ctx.samples, spb, phase, ctx.threshold, ctx.zc, ctx.cancel)
    } else {
        extract_bits(ctx.samples, spb, phase, ctx.threshold, ctx.cancel)
    };
    if bits.len() < 80 {
        return ScoredCandidate::TooShort;
    }

    let scan = find_frames(&bits, ctx.fps, ctx.drop_frame);
    let (valid_frames, total_possible, frame_starts, grid_valid) =
        (scan.valid_frames, scan.total_possible, scan.frame_starts, scan.grid_valid);
    if valid_frames <= best_valid {
        return ScoredCandidate::NoBeat;
    }
    let fps_name = format!("{:.2} fps", ctx.fps);
    let details_entry = if adaptive {
        debug!("LTC refinement adaptive -- {} valid / {} possible (phase={})",
            valid_frames, total_possible, phase);
        format!(
            "{}: {} valid / {} possible frames (adaptive, phase={}, spb={:.2})",
            fps_name, valid_frames, total_possible, phase, spb
        )
    } else {
        debug!("LTC extract-bits: {} spb={:.2} phase={} -- {} valid / {} possible (new best)",
            fps_name, spb, phase, valid_frames, total_possible);
        format!(
            "{}: {} valid / {} possible frames (spb={:.2}, phase={})",
            fps_name, valid_frames, total_possible, spb, phase
        )
    };

    ScoredCandidate::Beat(ScoredResult::from_frame_starts(
        CandidateTiming {
            fps: ctx.fps,
            drop_frame: ctx.drop_frame,
            sample_rate: ctx.sample_rate,
            spb,
            phase,
        },
        total_possible,
        &bits,
        frame_starts,
        details_entry,
        adaptive,
        grid_valid,
    ))
}

/// Number of phase candidates to try for a given samples-per-bit: a quarter
/// of the spb, clamped to a 5..=12 working window.
fn phase_window(spb: f64) -> usize {
    ((spb / 4.0).round() as usize).clamp(5, 12)
}

/// SPB search lattice around the nominal samples-per-bit: ±0.4% in five
/// steps when the resolution is high enough for drift to matter, otherwise
/// just the nominal value.
fn spb_variants(spb_nominal: f64) -> Vec<f64> {
    if spb_nominal >= 8.0 {
        let half_range = (spb_nominal * 0.004).max(0.05);
        (0..5)
            .map(|i| { let t = i as f64 / 4.0; spb_nominal + (t - 0.5) * 2.0 * half_range })
            .collect::<Vec<_>>()
    } else {
        vec![spb_nominal]
    }
}

/// First search pass: try every (spb variant, zero-crossing phase) pair with
/// plain bit extraction and keep the best valid-frame count.
fn coarse_search(
    ctx: &DecodeCtx,
    variants: &[f64],
    eval_start: std::time::Instant,
) -> (Option<ScoredResult>, u32) {
    let mut best_valid = 0u32;
    let mut best_result: Option<ScoredResult> = None;

    for (spb_idx, &spb) in variants.iter().enumerate() {
        if cancelled(ctx.cancel) {
            return (None, 0);
        }
        let max_phases = phase_window(spb);
        let phases_to_try = ctx.zc.iter().take(max_phases).copied();

        let half_spb = (spb * 0.5) as usize;
        let mut attempts_this_spb = 0u32;
        debug!("LTC evaluate: SPB variant {}/{} -- spb={:.2} ({} phases)",
            spb_idx + 1, variants.len(), spb, max_phases * 2);

        for phase in phases_to_try {
            for &candidate_phase in &[phase, phase.saturating_sub(half_spb)] {
                match score_candidate(ctx, spb, candidate_phase, false, best_valid) {
                    ScoredCandidate::TooShort => {}
                    ScoredCandidate::NoBeat => attempts_this_spb += 1,
                    ScoredCandidate::Beat(new) => {
                        attempts_this_spb += 1;
                        best_valid = new.valid_frames;
                        best_result = Some(new);
                    }
                }
            }
        }
        debug!("LTC evaluate: SPB variant {}/{} done -- {} attempts in {:.1}s, best={} valid",
            spb_idx + 1, variants.len(),
            attempts_this_spb, eval_start.elapsed().as_secs_f64(), best_valid);
    }

    (best_result, best_valid)
}

/// Second pass: refine the best coarse candidate with adaptive bit
/// extraction over the remaining phases at the winner's spb.
fn refine_best(
    ctx: &DecodeCtx,
    best: ScoredResult,
    best_valid: u32,
    eval_start: std::time::Instant,
) -> (Option<ScoredResult>, u32) {
    debug!("LTC evaluate (+{:.1}s): refinement phase for best candidate ({} fps, spb={:.2}, best_valid={})",
        eval_start.elapsed().as_secs_f64(), best.fps, best.spb, best_valid);
    let best_spb = best.spb;
    let best_phase = best.phase;

    let max_phases = phase_window(best_spb);
    let phases_to_try = ctx.zc.iter().take(max_phases).copied();
    let mut last_heartbeat = std::time::Instant::now();
    let mut refine_idx = 0u32;
    let mut best_result = Some(best);
    let mut best_valid = best_valid;
    for phase in phases_to_try {
        if cancelled(ctx.cancel) {
            return (best_result, best_valid);
        }
        if phase == best_phase { continue; }
        refine_idx += 1;
        if last_heartbeat.elapsed().as_secs_f64() >= 10.0 {
            debug!("LTC refine (+{:.1}s): phase {}/{} (phase={}), best_valid={}",
                eval_start.elapsed().as_secs_f64(), refine_idx, max_phases - 1,
                phase, best_valid);
            last_heartbeat = std::time::Instant::now();
        }
        if let ScoredCandidate::Beat(new) = score_candidate(ctx, best_spb, phase, true, best_valid) {
            best_valid = new.valid_frames;
            best_result = Some(new);
        }
    }
    (best_result, best_valid)
}

/// Evaluate LTC on a slice using the given FPS.
///
/// Tries SPB variants to compensate for clock drift, with phases derived
/// from the first several zero-crossings.
fn evaluate_on_slice(ctx: &DecodeCtx) -> (Option<ScoredResult>, u32) {
    let eval_start = std::time::Instant::now();
    let fps = ctx.fps;
    let fps_name = format!("{:.2} fps", fps);
    let spb_nominal = ctx.sample_rate as f64 / (fps * 80.0);
    if spb_nominal < 0.5 {
        return (None, 0);
    }
    debug!("LTC evaluate: trying {} (spb_nominal={:.2})", fps_name, spb_nominal);

    let variants = spb_variants(spb_nominal);
    let (best_result, best_valid) = coarse_search(ctx, &variants, eval_start);

    let (best_result, best_valid) = match best_result {
        Some(best) => refine_best(ctx, best, best_valid, eval_start),
        None => (None, 0),
    };

    let confidence = best_result.as_ref().map_or(0.0, |r| {
        if r.total_possible > 0 { r.valid_frames as f32 / r.total_possible as f32 } else { 0.0 }
    });
    debug!("LTC evaluate final -- {} valid/{} possible ({:.1}%), FPS {}",
        best_valid,
        best_result.as_ref().map_or(0, |r| r.total_possible),
        confidence * 100.0,
        best_result.as_ref().map_or(0.0, |r| r.fps));

    (best_result, best_valid)
}

fn build_result(
    ctx: &DecodeCtx,
    best_result: Option<ScoredResult>,
    channels: usize,
    total_duration: f64,
    start: std::time::Instant,
) -> Result<LtcDetectionResult, LtcDecodeError> {
    let zc = ctx.zc;
    let samples = ctx.samples;
    let sample_rate = ctx.sample_rate;
    let threshold = ctx.threshold;
    let elapsed = start.elapsed();
    let processing_time_ms = elapsed.as_secs_f64() * 1000.0;

let mut result = match best_result {
        Some(mut r) => {
            backfill_leading_frames(samples, &mut r, threshold, sample_rate);
            let confidence = if r.total_possible > 0 {
                r.valid_frames as f32 / r.total_possible as f32
            } else {
                0.0
            };

            let status = if confidence >= CONFIDENCE_SUCCESS_THRESHOLD {
                LtcDecodeStatus::Success
            } else if confidence >= CONFIDENCE_LOW_THRESHOLD {
                LtcDecodeStatus::LowConfidence
            } else {
                LtcDecodeStatus::NoSyncWord
            };

            let mut details = r.details_entry.split('\n').map(String::from).collect::<Vec<_>>();
            details.insert(
                0,
                format!(
                    "Rate: {:.2} fps / {} spb -- confidence: {:.1}%",
                    r.fps,
                    r.spb,
                    confidence * 100.0
                ),
            );
            details.push(format!(
                "Zero-crossings found: {} (threshold: {:.6})",
                zc.len(),
                threshold
            ));
            details.push(format!(
                "Audio: {:.2}s @ {} Hz, {} channels",
                total_duration, sample_rate, channels
            ));

            let timecodes = r.timecodes;

            let first_ltc_timecode_secs = timecodes.first().map_or(0.0, |t| t.timecode_secs);

            info!(
                "LTC decode result: status={:?}, fps={:.2}, valid={}/{}, confidence={:.1}%, first_offset={:.3}s, tc[0]={:.3}s, processing={:.0}ms",
                status, r.fps, r.valid_frames, r.total_possible, confidence * 100.0,
                first_ltc_timecode_secs, first_ltc_timecode_secs, processing_time_ms,
            );

            LtcDetectionResult {
                status,
                detected_fps: r.fps as f32,
                drop_frame: r.drop_frame,
                total_possible_frames: r.total_possible,
                valid_frames: r.valid_frames,
                timecodes,
                avg_confidence: confidence,
                details,
                total_audio_duration_secs: total_duration,
                sample_rate,
                processing_time_ms,
                first_ltc_timecode_secs,
                quality: None,
                chunk_summaries: Vec::new(),
            }
        }
        None => {
            info!(
                "LTC decode: no valid alignment found ({} zero-crossings, threshold={:.6}, processing={:.0}ms)",
                zc.len(), threshold, processing_time_ms,
            );
            let details = vec![
                "No valid LTC frame alignment found.".to_string(),
                format!("Zero-crossings found: {} (threshold: {:.6})", zc.len(), threshold),
                format!(
                    "Audio: {:.2}s @ {} Hz, {} channels",
                    total_duration, sample_rate, channels
                ),
            ];
            LtcDetectionResult {
                status: LtcDecodeStatus::NoSyncWord,
                detected_fps: 0.0,
                drop_frame: false,
                total_possible_frames: 0,
                valid_frames: 0,
                timecodes: Vec::new(),
                avg_confidence: 0.0,
                details,
                total_audio_duration_secs: total_duration,
                sample_rate,
                processing_time_ms: 0.0,
                first_ltc_timecode_secs: 0.0,
                quality: None,
                chunk_summaries: Vec::new(),
            }
        }
    };

    // WP-DR value integrity (DR1 BCD validation + DR2 continuity repair)
    // runs before the coherent-start trim so garbage values cannot become
    // the anchor.
    crate::ltc_integrity::apply_value_integrity(&mut result);
    apply_coherent_first_timecode(&mut result);
    result.quality = compute_ltc_quality(&result);
    Ok(result)
}

/// Scan decoded timecodes to find the index of the first frame that is part of
/// a coherent sequence (no jumps or gaps) continuing for at least 2 seconds.
///
/// Returns `Some(index)` if found, or `None` if (a) the timecodes already start
/// with a coherent run, (b) there are too few frames to form a 2-second run, or
/// (c) no sufficiently long coherent run exists anywhere.
pub fn find_first_coherent_index(
    timecodes: &[FrameTimecode],
    fps: f64,
    drop_frame: bool,
) -> Option<usize> {
    if timecodes.is_empty() || fps <= 0.0 {
        return None;
    }

    let frame_duration = 1.0 / fps;
    let max_frames = fps.ceil() as u32;
    let min_run = (2.0 * fps).ceil() as usize;

    if timecodes.len() < min_run {
        return None;
    }

    let is_valid_tc = |tc: &Timecode| -> bool {
        tc.hours < 24 && tc.minutes < 60 && tc.seconds < 60 && tc.frames < max_frames
    };

    let is_valid_pair = |prev: &FrameTimecode, curr: &FrameTimecode| -> bool {
        let expected = crate::increment_timecode(&prev.timecode, fps, drop_frame);
        if curr.timecode != expected {
            return false;
        }
        let dt = curr.timecode_secs - prev.timecode_secs;
        (dt - frame_duration).abs() < frame_duration * 0.5
    };

    for i in 0..=timecodes.len() - min_run {
        if !is_valid_tc(&timecodes[i].timecode) {
            continue;
        }

        let mut run_len = 1;
        for j in i + 1..timecodes.len() {
            if !is_valid_tc(&timecodes[j].timecode) {
                break;
            }
            if is_valid_pair(&timecodes[j - 1], &timecodes[j]) {
                run_len += 1;
                if run_len >= min_run {
                    return Some(i);
                }
            } else {
                break;
            }
        }
    }

    None
}

/// Post-process a `LtcDetectionResult` to find the first coherent timecode
/// and trim any non-coherent frames from the start of the `timecodes` vector.
/// Updates `first_ltc_timecode_secs` to match the first coherent frame.
pub(crate) fn apply_coherent_first_timecode(result: &mut LtcDetectionResult) {
    if result.timecodes.is_empty() || result.detected_fps <= 0.0 {
        return;
    }

    let fps = result.detected_fps as f64;

    if let Some(idx) = find_first_coherent_index(&result.timecodes, fps, result.drop_frame) {
        if idx > 0 {
            let first_coherent_secs = result.timecodes[idx].timecode_secs;
            let first_tc = result.timecodes[idx].timecode;

            result.timecodes = result.timecodes[idx..].to_vec();
            for (i, ftc) in result.timecodes.iter_mut().enumerate() {
                ftc.frame_index = i as u32;
            }

            result.first_ltc_timecode_secs = first_coherent_secs;

            result.details.push(format!(
                "Coherent start: trimmed {} non-coherent frame(s), first clean TC at {:.3}s = {:02}:{:02}:{:02}:{:02}",
                idx, first_coherent_secs,
                first_tc.hours, first_tc.minutes, first_tc.seconds, first_tc.frames,
            ));
        }
    }
}

/// Timecode immediately before `tc` on the locked frame grid (drop-frame
/// aware): the predecessor is the unique value whose successor is `tc`.
/// `None` if no candidate within a few naive steps maps back onto `tc`
/// (cannot happen for a well-formed frame number).
fn decrement_timecode(tc: &Timecode, fps: f64, drop_frame: bool) -> Option<Timecode> {
    let max_frames = fps.round() as u32;
    let naive_decrement = |t: Timecode| -> Timecode {
        if t.frames > 0 {
            return Timecode { frames: t.frames - 1, ..t };
        }
        if t.seconds > 0 {
            return Timecode { seconds: t.seconds - 1, frames: max_frames - 1, ..t };
        }
        if t.minutes > 0 {
            return Timecode { minutes: t.minutes - 1, seconds: 59, frames: max_frames - 1, ..t };
        }
        if t.hours > 0 {
            return Timecode { hours: t.hours - 1, minutes: 59, seconds: 59, frames: max_frames - 1 };
        }
        Timecode { hours: 23, minutes: 59, seconds: 59, frames: max_frames - 1 }
    };
    let mut cand = *tc;
    for _ in 0..4 {
        cand = naive_decrement(cand);
        if crate::increment_timecode(&cand, fps, drop_frame) == *tc {
            return Some(cand);
        }
    }
    None
}

/// Back-fill the leading frame(s) once the decoder has locked: walk backwards
/// one frame period from the earliest decoded start while the offset is ≥ 0,
/// decode each candidate frame at the locked phase/period with the regular
/// bit-extraction machinery, and accept it only if it yields a valid sync
/// word whose timecode is the predecessor of the previously accepted frame.
/// Stops at the first rejection — frames are never invented. This recovers
/// the start frame(s) that sync lock-in skips (ZC lock needs ~1 frame of
/// signal before the first sync word can be validated in-array).
fn backfill_leading_frames(
    samples: &[f32],
    r: &mut ScoredResult,
    threshold: f32,
    sample_rate: u32,
) {
    if r.frame_starts.is_empty() {
        return;
    }
    let mut accepted_tcs: Vec<FrameTimecode> = Vec::new();
    let mut accepted_starts: Vec<i64> = Vec::new();
    let mut prev_tc = r.timecodes[0].timecode;
    let mut start = r.frame_starts[0];
    loop {
        start -= 80;
        let offset_f = r.phase as f64 + start as f64 * r.spb;
        if offset_f < 0.0 {
            break;
        }
        let offset = offset_f as usize;
        if offset >= samples.len() {
            break;
        }
        let bits = extract_bits(&samples[offset..], r.spb, 0, threshold, None);
        if bits.len() < 80 {
            break;
        }
        if bits_hamming_distance_16(&bits[SYNC_OFFSET..SYNC_OFFSET + 16]) > SYNC_MATCH_TOLERANCE {
            break;
        }
        let tc = decode_timecode_from_bits(&bits, 0);
        match decrement_timecode(&prev_tc, r.fps, r.drop_frame) {
            Some(expected) if tc == expected => {}
            _ => break,
        }
        accepted_tcs.push(FrameTimecode {
            frame_index: 0,
            timecode: tc,
            timecode_secs: offset_f / sample_rate as f64,
        });
        accepted_starts.push(start);
        prev_tc = tc;
    }
    if accepted_tcs.is_empty() {
        return;
    }
    r.total_possible += accepted_tcs.len() as u32;
    accepted_tcs.append(&mut r.timecodes);
    for (i, ftc) in accepted_tcs.iter_mut().enumerate() {
        ftc.frame_index = i as u32;
    }
    r.timecodes = accepted_tcs;
    let mut starts = accepted_starts;
    starts.append(&mut r.frame_starts);
    r.frame_starts = starts;
    r.valid_frames = r.frame_starts.len() as u32;
}


/// contiguous frame range.
///
/// Returns `(slope, max_abs_residual)` where `slope` is seconds of drift
/// per second of audio within the range, and `max_abs_residual` is the
/// largest deviation of any sample from the fitted line (jitter).
/// Ranges with fewer than 2 points or zero time span yield `(0.0, 0.0)`.
fn fit_segment_drift(
    audio_secs: &[f64],
    drift: &[f64],
    range: std::ops::Range<usize>,
) -> (f64, f64) {
    let len = range.end.saturating_sub(range.start);
    if len < 2 {
        return (0.0, 0.0);
    }
    let xs = &audio_secs[range.start..range.end];
    let ys = &drift[range.start..range.end];
    let n = xs.len() as f64;
    let mx = xs.iter().sum::<f64>() / n;
    let my = ys.iter().sum::<f64>() / n;
    let mut sxx = 0.0f64;
    let mut sxy = 0.0f64;
    for (&x, &y) in xs.iter().zip(ys) {
        let dx = x - mx;
        sxx += dx * dx;
        sxy += dx * (y - my);
    }
    if sxx <= 1e-12 {
        return (0.0, 0.0);
    }
    let slope = sxy / sxx;
    let intercept = my - slope * mx;
    let max_residual = xs
        .iter()
        .zip(ys)
        .map(|(&x, &y)| (y - (slope * x + intercept)).abs())
        .fold(0.0f64, f64::max);
    (slope, max_residual)
}

// ── Quality analysis: pure sub-analyzers ────────────────────────────────────
// compute_ltc_quality orchestrates these; each stage is directly testable.
// It compares LTC timecode values against audio positions to detect gaps,
// glitches, edit points, and clock drift, returning None when there are no
// decoded timecodes to analyze.

struct QualityInputs {
    ltc_secs: Vec<f64>,
    audio_secs: Vec<f64>,
    drift: Vec<f64>,
    fps: f64,
}

fn quality_inputs(result: &LtcDetectionResult) -> Option<QualityInputs> {
    let timecodes = &result.timecodes;
    if timecodes.is_empty() || result.detected_fps <= 0.0 {
        return None;
    }
    let fps = result.detected_fps as f64;
    let n = timecodes.len();

    // Convert each LTC timecode to total seconds
    let ltc_secs: Vec<f64> = timecodes
        .iter()
        .map(|ft| {
            ft.timecode.hours as f64 * 3600.0
                + ft.timecode.minutes as f64 * 60.0
                + ft.timecode.seconds as f64
                + ft.timecode.frames as f64 / fps
        })
        .collect();

    let audio_secs: Vec<f64> = timecodes.iter().map(|ft| ft.timecode_secs).collect();
    let first_audio = audio_secs[0];
    let first_ltc = ltc_secs[0];

    // Normalized drift (audio position minus LTC value, zeroed at first frame)
    let drift: Vec<f64> = (0..n)
        .map(|i| (audio_secs[i] - first_audio) - (ltc_secs[i] - first_ltc))
        .collect();

    Some(QualityInputs { ltc_secs, audio_secs, drift, fps })
}

/// Find contiguous segments by comparing LTC timecode values,
/// NOT frame_index (which gets re-indexed by chunked merge).
pub(crate) fn split_segments(ltc_secs: &[f64], fps: f64) -> Vec<std::ops::Range<usize>> {
    let frame_duration = 1.0 / fps;
    let gap_threshold = frame_duration * 2.0;

    let mut segments: Vec<std::ops::Range<usize>> = Vec::new();
    let mut seg_start = 0;
    for i in 1..ltc_secs.len() {
        let expected = ltc_secs[i - 1] + frame_duration;
        if (ltc_secs[i] - expected).abs() > gap_threshold {
            segments.push(seg_start..i);
            seg_start = i;
        }
    }
    segments.push(seg_start..ltc_secs.len());
    segments
}

pub(crate) struct BlockDrift {
    slope: f64,
    accum_frames: f64,
    frames: usize,
    duration: f64,
}

struct DriftStats {
    blocks: Vec<BlockDrift>,
    usable_coverage: f64,
    worst_block_drift_frames: f64,
    worst_slope: f64,
    drift_penalty: f64,
}

/// Per-block clock drift (least-squares fit) plus block usability.
/// Drift is fitted per block so that TC jumps between blocks cannot
/// contaminate the measurement: each block gets its own linear model
/// and only its own clock error counts against it.
fn analyze_drift(
    audio_secs: &[f64],
    drift: &[f64],
    segments: &[std::ops::Range<usize>],
    fps: f64,
) -> DriftStats {
    let n = audio_secs.len();
    let blocks: Vec<BlockDrift> = segments
        .iter()
        .map(|seg| {
            let (slope, _residual) = fit_segment_drift(audio_secs, drift, seg.start..seg.end);
            let duration = audio_secs[seg.end - 1] - audio_secs[seg.start];
            let accum_frames = (slope * duration).abs() * fps;
            BlockDrift { slope, accum_frames, frames: seg.end - seg.start, duration }
        })
        .collect();

    // A block is usable for syncing when it is long enough to align against
    // (>= 2s; for very short recordings 90% of the span so a short clean
    // clip still counts) and its drift accumulates at most 1 frame over its
    // length. Everything else in the score is anchored on this.
    let total_span = audio_secs[n - 1] - audio_secs[0];
    let min_block_duration = if total_span < 2.2 { (total_span * 0.9).min(2.0) } else { 2.0 };
    let mut usable_frames = 0usize;
    let mut worst_block_drift_frames = 0.0f64;
    let mut worst_slope = 0.0f64;
    let mut drift_penalty = 0.0f64;
    for b in &blocks {
        if b.slope.abs() > worst_slope.abs() {
            worst_slope = b.slope;
        }
        if b.accum_frames > worst_block_drift_frames {
            worst_block_drift_frames = b.accum_frames;
        }
        // A single-frame block is never usable: with zero span it can
        // establish neither sync nor drift (degenerate decodes — e.g. an
        // fps mismatch reduced to one frame by the integrity pass — must
        // not score as usable).
        let usable = b.frames >= 2 && b.duration >= min_block_duration && b.accum_frames <= 1.0;
        if usable {
            usable_frames += b.frames;
            if b.accum_frames > 0.5 {
                // Usable but degraded: the sync error creeps towards a frame
                let severity = ((b.accum_frames - 0.5) / 0.5).min(1.0);
                drift_penalty += 0.10 * severity * (b.frames as f64 / n as f64);
            }
        }
    }
    DriftStats {
        usable_coverage: usable_frames as f64 / n as f64,
        blocks,
        worst_block_drift_frames,
        worst_slope,
        drift_penalty,
    }
}

struct GapStats {
    gap_count: u32,
    edit_count: u32,
    backward_jump_count: u32,
    gap_edges: Vec<(usize, usize)>,
    backward_affected_frames: usize,
}

/// Gaps between segments, edit detection (large LTC jump where audio elapsed
/// doesn't match LTC elapsed), and backward jumps (TC resets).
fn analyze_gaps(
    audio_secs: &[f64],
    ltc_secs: &[f64],
    segments: &[std::ops::Range<usize>],
    fps: f64,
) -> GapStats {
    let frame_duration = 1.0 / fps;
    let mut gap_count: u32 = 0;
    let mut edit_count: u32 = 0;
    let mut backward_jump_count: u32 = 0;
    let mut first_backward_block: Option<usize> = None;
    let mut gap_edges: Vec<(usize, usize)> = Vec::new();
    let edit_threshold = 0.1;  // seconds — audio-vs-LTC mismatch must exceed this
    let edit_ltc_jump_threshold = 10.0 / fps;  // LTC must jump by at least 10 frames

    for (block_idx, w) in segments.windows(2).enumerate() {
        let prev = &w[0];
        let cur = &w[1];
        let i_prev = prev.end - 1;
        let i_cur = cur.start;

        gap_count += 1;
        gap_edges.push((i_prev, i_cur));

        let audio_elapsed = audio_secs[i_cur] - audio_secs[i_prev];
        let ltc_elapsed = ltc_secs[i_cur] - ltc_secs[i_prev];
        let diff = (audio_elapsed - ltc_elapsed).abs();

        if ltc_elapsed.abs() > edit_ltc_jump_threshold && diff > edit_threshold {
            edit_count += 1;
        }

        // Backward jump: the TC value goes back (timecode reset). The TC
        // values after the jump can recur from earlier in the recording,
        // which makes syncing by TC value ambiguous in editors.
        if ltc_elapsed < -0.5 * frame_duration {
            backward_jump_count += 1;
            if first_backward_block.is_none() {
                first_backward_block = Some(block_idx + 1);
            }
        }
    }

    // Frames from the first backward jump onward are ambiguous
    let backward_affected_frames: usize = match first_backward_block {
        Some(k) => segments[k..].iter().map(|s| s.end - s.start).sum(),
        None => 0,
    };

    GapStats { gap_count, edit_count, backward_jump_count, gap_edges, backward_affected_frames }
}

struct GlitchStats {
    glitch_count: u32,
    glitch_indices: Vec<usize>,
}

/// Isolated glitch frames within contiguous segments.
/// Threshold 1.5 frames: a single frame deviating by 2 frames stays
/// inside its segment (segment splits need > 2 frames deviation) but is
/// caught here; deviations >= 3 frames split segments and count as gaps.
fn analyze_glitches(
    ltc_secs: &[f64],
    segments: &[std::ops::Range<usize>],
    fps: f64,
) -> GlitchStats {
    let glitch_threshold = 1.5 / fps;
    let mut glitch_count: u32 = 0;
    let mut glitch_indices: Vec<usize> = Vec::new();

    for seg in segments {
        let seg_len = seg.end - seg.start;
        if seg_len < 3 {
            continue;
        }
        for i in (seg.start + 1)..(seg.end - 1) {
            let expected = (ltc_secs[i - 1] + ltc_secs[i + 1]) / 2.0;
            if (ltc_secs[i] - expected).abs() > glitch_threshold {
                glitch_count += 1;
                glitch_indices.push(i);
            }
        }
    }
    GlitchStats { glitch_count, glitch_indices }
}

/// Missing frames within the decoded LTC span. Only frames between the first
/// and last decoded frame can be missing: silent lead-in/out and pre-LTC
/// silence at the edges of the recording are normal, not defects.
/// Returns (missing_frames, missing_ratio).
fn missing_in_span(audio_secs: &[f64], fps: f64) -> (u32, f64) {
    let n = audio_secs.len();
    let expected_in_span = ((audio_secs[n - 1] - audio_secs[0]) * fps).round() as i64 + 1;
    let missing_frames = (expected_in_span - n as i64).max(0) as u32;
    let missing_ratio = if expected_in_span > 0 {
        missing_frames as f64 / expected_in_span as f64
    } else {
        0.0
    };
    (missing_frames, missing_ratio)
}

/// THE scoring formula (0.0–1.0), anchored on usable coverage:
/// score = usable_coverage − 0.02·min(edits,10) − 0.30·backward_ratio
///         − glitch ramp (0.15 above 0.1%) − drift_penalty
///         − missing ramp (0.15 above 5%), clamped [0,1].
fn quality_score(
    usable_coverage: f64,
    edit_count: u32,
    backward_ratio: f64,
    glitch_ratio: f64,
    drift_penalty: f64,
    missing_ratio: f64,
) -> f64 {
    let mut score = usable_coverage;

    // Forward TC jumps are normal in the field (generator restarts, re-jams)
    // and do not invalidate the frames around them: small fixed penalty.
    score -= 0.02 * (edit_count.min(10)) as f64;

    // Backward jumps (TC reset) make TC values recur: penalize by the
    // fraction of the recording that becomes ambiguous.
    score -= 0.30 * backward_ratio;

    // Glitches only matter once they exceed 0.1% of frames, then scale up.
    if glitch_ratio > 0.001 {
        score -= 0.15 * ((glitch_ratio - 0.001) / 0.009).min(1.0);
    }

    // Usable blocks whose drift creeps towards a frame
    score -= drift_penalty;

    // Missing frames inside the LTC span (interior holes)
    if missing_ratio > 0.05 {
        score -= 0.15 * (missing_ratio / 0.5).min(1.0);
    }

    score.clamp(0.0, 1.0)
}

/// Issue counters and drift spans of one decode, feeding the
/// human-readable quality summary.
struct QualityIssueStats {
    usable_coverage: f64,
    block_count: usize,
    edit_count: u32,
    backward_jump_count: u32,
    glitch_count: u32,
    missing_frames: u32,
    worst_block_drift_frames: f64,
    worst_slope: f64,
}

/// Human-readable summary of issues found.
fn quality_summary(stats: QualityIssueStats, fps: f64) -> String {
    let QualityIssueStats {
        usable_coverage,
        block_count,
        edit_count,
        backward_jump_count,
        glitch_count,
        missing_frames,
        worst_block_drift_frames,
        worst_slope,
    } = stats;
    let mut parts: Vec<String> = Vec::new();
    if edit_count > 0 || backward_jump_count > 0 {
        parts.push(format!("{} TC jump(s) ({} backward)", edit_count, backward_jump_count));
    }
    if glitch_count > 0 {
        parts.push(format!("{} glitch(es)", glitch_count));
    }
    if missing_frames > 0 {
        parts.push(format!("{} missing frame(s)", missing_frames));
    }
    if worst_block_drift_frames > 0.5 {
        parts.push(format!("max drift {:.2} frame(s)", worst_block_drift_frames));
    }
    if worst_slope.abs() * fps > 0.5 {
        parts.push(format!("drift rate {:.3} s/s", worst_slope));
    }

    let coverage_part = format!(
        "{:.1}% usable ({} block(s))",
        usable_coverage * 100.0,
        block_count
    );
    if parts.is_empty() {
        format!("{} — all frames contiguous and in sync", coverage_part)
    } else {
        format!("{}; {}", coverage_part, parts.join(", "))
    }
}

pub fn compute_ltc_quality(result: &LtcDetectionResult) -> Option<LtcQualityReport> {
    let inputs = quality_inputs(result)?;
    let fps = inputs.fps;
    let n = inputs.audio_secs.len();

    let segments = split_segments(&inputs.ltc_secs, fps);

    // Largest contiguous block
    let largest_block = segments.iter().map(|s| (s.end - s.start) as u32).max().unwrap_or(0);

    let drift = analyze_drift(&inputs.audio_secs, &inputs.drift, &segments, fps);
    let gaps = analyze_gaps(&inputs.audio_secs, &inputs.ltc_secs, &segments, fps);
    let glitches = analyze_glitches(&inputs.ltc_secs, &segments, fps);
    let (missing_frames, missing_ratio) = missing_in_span(&inputs.audio_secs, fps);

    let backward_ratio = gaps.backward_affected_frames as f64 / n as f64;
    let glitch_ratio = glitches.glitch_count as f64 / n as f64;
    let score = quality_score(
        drift.usable_coverage,
        gaps.edit_count,
        backward_ratio,
        glitch_ratio,
        drift.drift_penalty,
        missing_ratio,
    );

    // Grade
    let grade = QualityGrade::from_score(score);

    let summary = quality_summary(
        QualityIssueStats {
            usable_coverage: drift.usable_coverage,
            block_count: drift.blocks.len(),
            edit_count: gaps.edit_count,
            backward_jump_count: gaps.backward_jump_count,
            glitch_count: glitches.glitch_count,
            missing_frames,
            worst_block_drift_frames: drift.worst_block_drift_frames,
            worst_slope: drift.worst_slope,
        },
        fps,
    );

    Some(LtcQualityReport {
        score,
        grade,
        missing_frames,
        gap_count: gaps.gap_count,
        glitch_count: glitches.glitch_count,
        edit_count: gaps.edit_count,
        max_drift_secs: drift.worst_block_drift_frames / fps,
        drift_rate: drift.worst_slope,
        largest_block,
        usable_coverage: drift.usable_coverage,
        block_count: drift.blocks.len() as u32,
        worst_block_drift_frames: drift.worst_block_drift_frames,
        backward_jump_count: gaps.backward_jump_count,
        summary,
        gap_edges: gaps.gap_edges,
        glitch_indices: glitches.glitch_indices,
    })
}

// ── Reading ──────────────────────────────────────────────────────────────────

fn read_mono_samples<R: std::io::Read>(
    reader: &mut hound::WavReader<R>,
    spec: &hound::WavSpec,
    channel: usize,
) -> Result<Vec<f32>, String> {
    let channels = spec.channels as usize;
    let bits = spec.bits_per_sample;

    match spec.sample_format {
        hound::SampleFormat::Int => {
            let max_val = (1i64 << (bits - 1)) as f32;
            let samples: Vec<f32> = reader
                .samples::<i32>()
                .filter_map(|s| s.ok())
                .enumerate()
                .filter(|(i, _)| i % channels == channel)
                .map(|(_, s)| s as f32 / max_val)
                .collect();
            Ok(samples)
        }
        hound::SampleFormat::Float => {
            let samples: Vec<f32> = reader
                .samples::<f32>()
                .filter_map(|s| s.ok())
                .enumerate()
                .filter(|(i, _)| i % channels == channel)
                .map(|(_, s)| s)
                .collect();
            Ok(samples)
        }
    }
}

// ── WP-DR signal conditioning (DR3 + DR4) ────────────────────────────────────

/// Floor fraction of the robust signal peak for the zero-crossing
/// threshold (DR5) and its absolute cap. The cap keeps a loud peak from
/// raising the floor above what quiet content needs; the fraction is what
/// lets quiet content decode at all (bit amplitude ≈ volume/2 must clear
/// the floor with margin).
const PEAK_FLOOR_FRAC: f32 = 0.02;
const ABS_FLOOR_CAP: f32 = 0.05;

/// Always-on input conditioning, applied before any detection constant:
/// 3-tap median (kills single-sample clicks and protects the filters
/// below) → one-pole DC blocker (removes offsets) → 50/60 Hz mains-notch
/// pair (attenuates hum by tens of dB while leaving the ~1–4 kHz LTC band
/// untouched). Three linear passes, a few percent of decode runtime.
///
/// The plan's originally prescribed ~500 Hz high-pass was replaced by the
/// notch pair during implementation (the WP-DR §9.7 fallback): a 400–500 Hz
/// high-pass droops the bi-phase levels by ~50 % within a half-bit period,
/// which collapses them under the amplitude-derived ZC threshold and broke
/// clean-signal decode. Notches have no droop above ~200 Hz.
pub(crate) fn condition_signal(samples: &mut [f32], sample_rate: u32) {
    median_filter(samples);
    dc_block(samples);
    mains_notch(samples, sample_rate);
}

/// In-place 5-tap median filter. LTC's minimum half-period is ≥ 12 samples
/// @ 48 kHz, so spike runs up to 2 samples are always removable without
/// touching legitimate transitions. (5 taps, not 7: wider windows erode
/// the shortest legitimate half-bit plateaus and measurably *hurt* decode.)
fn median_filter(samples: &mut [f32]) {
    if samples.len() < 5 {
        return;
    }
    let mut prev2 = samples[0];
    let mut prev1 = samples[1];
    for i in 2..samples.len() - 2 {
        let cur = samples[i];
        let next1 = samples[i + 1];
        let next2 = samples[i + 2];
        samples[i] = median5([prev2, prev1, cur, next1, next2]);
        prev2 = prev1;
        prev1 = cur;
    }
}

fn median5(mut v: [f32; 5]) -> f32 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    v[2]
}

/// DC removal by global mean subtraction (one O(n) pass). A one-pole DC
/// blocker was tried first and rejected: its multi-frame time constant let
/// each bi-phase transition bias the baseline for ~2000 samples (DC
/// wander), which measurably degraded fading and low-passed material.
/// Global mean subtraction removes any static offset exactly and is a
/// no-op for a symmetric square wave.
fn dc_block(samples: &mut [f32]) {
    if samples.is_empty() {
        return;
    }
    let mean = samples.iter().sum::<f32>() / samples.len() as f32;
    for s in samples.iter_mut() {
        *s -= mean;
    }
}

/// RBJ-cookbook biquad notch at `f0` (Q ≈ 25 → ~4 Hz notch width, wide
/// enough for mains drift, narrow enough to leave the LTC band alone).
fn notch_stage(samples: &mut [f32], f0: f32, sample_rate: u32) {
    let w0 = 2.0 * std::f32::consts::PI * f0 / sample_rate as f32;
    let (sin_w, cos_w) = w0.sin_cos();
    let alpha = sin_w / (2.0 * 25.0);
    let a0 = 1.0 + alpha;
    let b0 = 1.0 / a0;
    let b1 = -2.0 * cos_w / a0;
    let b2 = 1.0 / a0;
    let a1 = -2.0 * cos_w / a0;
    let a2 = (1.0 - alpha) / a0;
    let mut x1 = 0.0f32;
    let mut x2 = 0.0f32;
    let mut y1 = 0.0f32;
    let mut y2 = 0.0f32;
    for s in samples.iter_mut() {
        let x = *s;
        let y = b0 * x + b1 * x1 + b2 * x2 - a1 * y1 - a2 * y2;
        x2 = x1;
        x1 = x;
        y2 = y1;
        y1 = y;
        *s = y;
    }
}

/// Twin mains notches. Both run always-on: a 50 Hz tone passes the 60 Hz
/// notch untouched and vice versa, so one pass shape covers both mains
/// standards without conditioning switches.
fn mains_notch(samples: &mut [f32], sample_rate: u32) {
    notch_stage(samples, 50.0, sample_rate);
    notch_stage(samples, 60.0, sample_rate);
}

/// 99.5th percentile of |samples| via a linear histogram over [0, max]
/// (two O(n) passes, no sort). This is the DR5 "robust peak": immune to
/// sparse full-scale clicks (post-median-filter they are ≪ 0.5 % of
/// samples) yet coverage-proof for recordings that are mostly silent
/// lead-in — a block-median peak would read 0 there and zero the
/// threshold floor.
fn robust_peak_amplitude(samples: &[f32]) -> f32 {
    const BINS: usize = 4096;
    let mut max = 0.0f32;
    for &s in samples {
        let a = s.abs();
        if a > max {
            max = a;
        }
    }
    if max <= 0.0 {
        return 0.0;
    }
    let mut hist = [0u32; BINS];
    let scale = (BINS - 1) as f32 / max;
    for &s in samples {
        let bin = ((s.abs() * scale) as usize).min(BINS - 1);
        hist[bin] += 1;
    }
    let target = (samples.len() as f64 * 0.995).ceil() as u64;
    let mut seen = 0u64;
    for (bin, &count) in hist.iter().enumerate() {
        seen += count as u64;
        if seen >= target {
            // Linear interpolation inside the winning bin.
            let bin_lo = bin as f32 / scale;
            let bin_hi = (bin + 1) as f32 / scale;
            let frac = if count > 0 {
                1.0 - (seen - target) as f32 / count as f32
            } else {
                0.0
            };
            return bin_lo + (bin_hi - bin_lo) * frac;
        }
    }
    max
}

// ── Noise floor estimation ───────────────────────────────────────────────────

fn estimate_noise_floor(samples: &[f32]) -> f32 {
    let total = samples.len();
    if total == 0 {
        return 1e-10;
    }
    let window_count = 4usize;
    let window_size = (total / window_count).clamp(100, 10_000);
    let mut best_median = f32::MAX;
    for w in 0..window_count {
        let start = (total * w / window_count).min(total.saturating_sub(window_size));
        let end = (start + window_size).min(total);
        let mut vals: Vec<f32> = samples[start..end].iter().map(|s| s.abs()).collect();
        vals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let med = vals[vals.len() / 2];
        if med < best_median {
            best_median = med;
        }
    }
    best_median.max(1e-10)
}

// ── Zero-crossing detection ──────────────────────────────────────────────────

fn find_zero_crossings(samples: &[f32], threshold: f32) -> Vec<usize> {
    let mut crossings = Vec::new();
    let mut prev_sign = 0i8;
    let hysteresis_scale = 2.0;
    let mut effective_threshold = threshold;

    for (i, &s) in samples.iter().enumerate() {
        let cur_sign = if s.abs() >= effective_threshold {
            if s > 0.0 { 1 } else { -1 }
        } else {
            0
        };

        if prev_sign != 0 && cur_sign != 0 && prev_sign != cur_sign {
            crossings.push(i);
            effective_threshold = threshold * hysteresis_scale;
        }

        if cur_sign != 0 {
            if cur_sign != prev_sign {
                effective_threshold = threshold;
            }
            prev_sign = cur_sign;
        } else {
            effective_threshold = threshold;
        }
    }

    crossings
}

// ── ZC-interval bit reconstruction ──────────────────────────────────────────

fn decode_bits_from_zero_crossings(
    zc: &[usize],
    sample_rate: u32,
    fps: f64,
    total_samples: usize,
) -> Vec<u8> {
    if zc.len() < 2 {
        return Vec::new();
    }

    let spb = sample_rate as f64 / (fps * 80.0);
    let short_threshold = spb * 0.75;
    let min_interval = spb * 0.20;

    let total = zc.len() - 1;
    let mut short_count = 0usize;
    for i in 0..total {
        let interval = (zc[i + 1] - zc[i]) as f64;
        if interval < short_threshold && interval >= min_interval {
            short_count += 1;
        }
    }

    let short_ratio = short_count as f64 / total as f64;
    debug!("LTC ZC-interval: {} intervals, {:.1}% short (encoding: {})",
        total, short_ratio * 100.0,
        if short_ratio > 0.10 { "real SMPTE" } else { "synthetic" });

    // Close the final bit period: the interval after the last zero-crossing is
    // never observed, so the bits from there to the end of the buffer are
    // appended as zeros. Without this the final frame is one bit short of its
    // 80-bit span and `find_frames` drops it.
    let trailing_zeros = zc.last()
        .map_or(0, |&last| (((total_samples.saturating_sub(last)) as f64 / spb).ceil() as i64).max(0) as usize);

    if short_ratio > 0.10 {
        let mut bits = decode_bits_real_zc(zc, spb);
        bits.extend(std::iter::repeat(0u8).take(trailing_zeros));
        bits
    } else {
        let mut bits = decode_bits_synthetic_zc(zc, spb);
        bits.extend(std::iter::repeat(0u8).take(trailing_zeros));
        bits
    }
}

fn decode_bits_real_zc(zc: &[usize], spb: f64) -> Vec<u8> {
    let short_threshold = spb * 0.75;
    let mut bits = Vec::with_capacity(zc.len());

    let mut i = 0;
    while i < zc.len() - 1 {
        let interval = (zc[i + 1] - zc[i]) as f64;

        if interval >= short_threshold {
            bits.push(0);
            i += 1;
        } else {
            if i + 2 < zc.len() {
                let next = (zc[i + 2] - zc[i + 1]) as f64;
                if next < short_threshold {
                    bits.push(1);
                    i += 2;
                } else {
                    i += 1;
                }
            } else {
                break;
            }
        }
    }

    bits
}

fn decode_bits_synthetic_zc(zc: &[usize], spb: f64) -> Vec<u8> {
    let mut bits = Vec::with_capacity(zc.len() + 8);

    let first_zc = zc[0] as f64;
    let leading = (first_zc / spb - 0.5).round() as usize;
    if leading > 0 {
        bits.resize(bits.len() + leading, 0);
    }
    bits.push(1);

    for i in 1..zc.len() {
        let interval = (zc[i] - zc[i - 1]) as f64;
        let n_periods = (interval / spb).round() as u32;
        let zeros = n_periods.saturating_sub(1);
        if zeros > 0 {
            bits.resize(bits.len() + zeros as usize, 0);
        }
        bits.push(1);
    }

    bits
}

/// Try to decode LTC using the fast ZC-interval method with a single FPS.
fn try_decode_via_zc_intervals(
    zc: &[usize],
    sample_rate: u32,
    fps: f64,
    drop_frame: bool,
    samples_len: usize,
) -> Option<ScoredResult> {
    if zc.len() < 8 {
        return None;
    }

    let bits = decode_bits_from_zero_crossings(zc, sample_rate, fps, samples_len);
    if bits.len() < 80 {
        return None;
    }

    let scan = find_frames(&bits, fps, drop_frame);
    if scan.valid_frames == 0 {
        return None;
    }
    let (valid_frames, total_possible, frame_starts) =
        (scan.valid_frames, scan.total_possible, scan.frame_starts);
    let grid_valid = scan.grid_valid;

    let spb = sample_rate as f64 / (fps * 80.0);
    let fps_name = format!("{:.2} fps", fps);

    let details_entry = format!(
        "{}: {} valid / {} possible frames (ZC-interval)",
        fps_name, valid_frames, total_possible
    );

    Some(ScoredResult::from_frame_starts(
        CandidateTiming {
            fps,
            drop_frame,
            sample_rate,
            spb,
            phase: zc[0],
        },
        total_possible,
        &bits,
        frame_starts,
        details_entry,
        false,
        grid_valid,
    ))
}

/// Return the subslice of ZC positions falling within [range_start, range_end).
fn zc_in_range(zc: &[usize], range_start: usize, range_end: usize) -> &[usize] {
    let lo = zc.partition_point(|&p| p < range_start);
    let hi = zc.partition_point(|&p| p < range_end);
    &zc[lo..hi]
}

// ── Bit extraction (fallback for noisy LTC) ────────────────────────────────

fn median_sample(samples: &[f32], center: usize) -> f32 {
    let start = center.saturating_sub(1);
    let end = (center + 2).min(samples.len());
    if end - start == 1 {
        return samples[start];
    }
    let mut buf = [0.0f32; 3];
    let len = end - start;
    for (i, j) in (start..end).enumerate() {
        buf[i] = samples[j];
    }
    buf[..len].sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    buf[len / 2]
}

fn extract_bits(samples: &[f32], samples_per_bit: f64, phase: usize, _threshold: f32, cancel: Option<&AtomicBool>) -> Vec<u8> {
    let mut bits = Vec::new();
    let quarter = samples_per_bit * 0.25;
    let three_quarter = samples_per_bit * 0.75;
    let mut pos = phase as f64;

    let mut bit_count: usize = 0;
    while ((pos + three_quarter) as usize) < samples.len() {
        if bit_count & 0xFF == 0 {
            if let Some(c) = cancel {
                if c.load(Ordering::Relaxed) {
                    return bits;
                }
            }
        }
        bit_count += 1;
        let p25 = (pos + quarter) as usize;
        let p75 = (pos + three_quarter) as usize;

        let s25 = median_sample(samples, p25);
        let s75 = median_sample(samples, p75);

        let bit = if s25.signum() != s75.signum() { 1 } else { 0 };
        bits.push(bit);

        pos += samples_per_bit;
    }

    bits
}

fn extract_bits_adaptive(
    samples: &[f32],
    samples_per_bit: f64,
    phase: usize,
    _threshold: f32,
    zero_crossings: &[usize],
    cancel: Option<&AtomicBool>,
) -> Vec<u8> {
    let mut bits = Vec::new();
    let quarter = samples_per_bit * 0.25;
    let three_quarter = samples_per_bit * 0.75;
    let snap_radius = (samples_per_bit * 0.25) as usize;
    let mut pos = phase as f64;

    fn zc_ge(zc: &[usize], target: usize) -> Option<usize> {
        match zc.binary_search(&target) {
            Ok(i) | Err(i) => zc.get(i).copied(),
        }
    }

    let mut bit_count: usize = 0;
    while ((pos + three_quarter) as usize) < samples.len() {
        if bit_count & 0xFF == 0 {
            if let Some(c) = cancel {
                if c.load(Ordering::Relaxed) {
                    return bits;
                }
            }
        }
        bit_count += 1;
        let p25 = (pos + quarter) as usize;
        let p75 = (pos + three_quarter) as usize;

        let s25 = median_sample(samples, p25);
        let s75 = median_sample(samples, p75);

        let bit = if s25.signum() != s75.signum() { 1 } else { 0 };
            bits.push(bit);

        let next_boundary = (pos + samples_per_bit) as usize;
        let target_lo = next_boundary.saturating_sub(snap_radius);
        let target_hi = next_boundary.saturating_add(snap_radius);

        let snapped = zc_ge(zero_crossings, target_lo)
            .filter(|&zc| zc <= target_hi);
        if let Some(snapped) = snapped {
            pos = snapped as f64;
        } else {
            pos = next_boundary as f64;
        }
    }

    bits
}

// ── Frame detection (sync word search) ───────────────────────────────────────

const SYNC_MATCH_TOLERANCE: u32 = 2;

fn bits_hamming_distance_16(a: &[u8]) -> u32 {
    let tolerance = SYNC_MATCH_TOLERANCE;
    let mut dist = 0u32;
    for (i, &bit) in a.iter().enumerate() {
        if bit != SYNC_WORD[i] {
            dist += 1;
            if dist > tolerance {
                return dist;
            }
        }
    }
    dist
}

/// Frame-detection outcome. `valid_frames` counts every accepted frame
/// (dominant grid + chain re-locks — the decoded reality); `grid_valid`
/// counts only dominant-grid frames and feeds the fast-path confidence
/// gates: chain re-lock recovers post-desync frames but says nothing about
/// how clean the dominant grid is, and a chain-inflated count must not skip
/// the detailed decode path.
struct FrameScan {
    valid_frames: u32,
    grid_valid: u32,
    total_possible: u32,
    frame_starts: Vec<usize>,
}

fn find_frames(bits: &[u8], fps: f64, drop_frame: bool) -> FrameScan {
    if bits.len() < 16 {
        return FrameScan { valid_frames: 0, grid_valid: 0, total_possible: 0, frame_starts: Vec::new() };
    }
    let sync_positions = scan_sync_positions(bits);
    if sync_positions.is_empty() {
        return FrameScan { valid_frames: 0, grid_valid: 0, total_possible: 0, frame_starts: Vec::new() };
    }
    let Some(alignment) = dominant_alignment(&sync_positions) else {
        return FrameScan { valid_frames: 0, grid_valid: 0, total_possible: 0, frame_starts: Vec::new() };
    };
    let total_possible = if bits.len() > alignment {
        ((bits.len() - alignment) / 80) as u32
    } else {
        0
    };

    let grid_starts = grid_walk_starts(bits, alignment);
    let chain_starts = chain_relock_starts(bits, &sync_positions, fps, drop_frame);
    let chain_starts = dedupe_near_duplicates(chain_starts);
    let merged = merge_grid_and_chains(grid_starts.clone(), chain_starts);

    FrameScan {
        valid_frames: merged.len() as u32,
        grid_valid: grid_starts.len() as u32,
        total_possible,
        frame_starts: merged,
    }
}

/// Tolerance-based sync-word scan: every position whose 16-bit window is
/// within `SYNC_MATCH_TOLERANCE` of the sync pattern starts a candidate;
/// after a hit the scan jumps a full frame.
fn scan_sync_positions(bits: &[u8]) -> Vec<usize> {
    let mut sync_positions = Vec::new();
    let max_start = bits.len() - 16;
    let mut i = 0;
    while i <= max_start {
        let dist = bits_hamming_distance_16(&bits[i..i + 16]);
        if dist <= SYNC_MATCH_TOLERANCE {
            sync_positions.push(i);
            i += 80;
        } else {
            i += 1;
        }
    }
    sync_positions
}

/// Alignment histogram: keeps the legacy total-possible denominator (the
/// dominant-alignment grid estimate). Frame *detection* below no longer
/// decodes along that grid: a single real-world disturbance shifts the
/// extracted bitstream, and grid decoding lost everything after the shift
/// (the WP-RW A2/A3 long-file collapse — chunk 0 of a 49-min recording
/// decoded 62% while its 120 s slices decoded 94–100%). Instead each
/// detected sync word re-locks its own frame at `sp - SYNC_OFFSET`.
/// Returns `None` when no sync sits at or after `SYNC_OFFSET` (empty
/// histogram) — the all-zero case of the legacy guard.
fn dominant_alignment(sync_positions: &[usize]) -> Option<usize> {
    let mut alignment_scores = vec![0u32; 80];
    for &sp in sync_positions {
        if sp >= SYNC_OFFSET {
            let alignment = (sp - SYNC_OFFSET) % 80;
            alignment_scores[alignment] += 1;
        }
    }
    let (best, count) = alignment_scores
        .iter()
        .enumerate()
        .max_by_key(|&(_, &c)| c)
        .unwrap_or((0, &0));
    if *count == 0 {
        None
    } else {
        Some(best)
    }
}

/// Legacy dominant-grid walk (acceptance unchanged). Chains below extend
/// this; grid frames remain the noise-regime baseline.
fn grid_walk_starts(bits: &[u8], alignment: usize) -> Vec<usize> {
    let mut grid_starts: Vec<usize> = Vec::new();
    let mut idx = 0usize;
    loop {
        let frame_start = alignment + idx * 80;
        if frame_start + 80 > bits.len() {
            break;
        }
        let sync_start = frame_start + SYNC_OFFSET;
        if sync_start + 16 <= bits.len()
            && bits_hamming_distance_16(&bits[sync_start..sync_start + 16]) <= SYNC_MATCH_TOLERANCE
        {
            grid_starts.push(frame_start);
        }
        idx += 1;
    }
    grid_starts
}

/// Chain re-lock (WP-RW A2/A3): a real-world disturbance shifts the
/// extracted bitstream, so every frame after it sits off the dominant
/// grid and the legacy walk loses the rest of the stream. A shifted (or
/// dominant-but-outvoted) segment is a run of consecutive sync words
/// spaced exactly 80 bits. Accept a run when it carries a consecutive-TC
/// pair: a true chain decodes to incrementing timecodes, while
/// noise-induced false syncs chained 80 bits apart decode to garbage.
/// Grid frames are unaffected (the walk above stays the noise-regime
/// baseline); dedup removes the overlap where both accept a frame.
fn chain_relock_starts(
    bits: &[u8],
    sync_positions: &[usize],
    fps: f64,
    drop_frame: bool,
) -> Vec<usize> {
    let mut chain_starts: Vec<usize> = Vec::new();
    let mut run: Vec<usize> = Vec::new();
    for i in 0..=sync_positions.len() {
        if i < sync_positions.len() {
            if let Some(&last) = run.last() {
                if sync_positions[i] != last + 80 {
                    chain_starts.extend(flush_chain_run(&run, bits, fps, drop_frame));
                    run.clear();
                }
            }
            run.push(sync_positions[i]);
        } else {
            chain_starts.extend(flush_chain_run(&run, bits, fps, drop_frame));
        }
    }
    chain_starts
}

/// Accept a single 80-bit-spaced sync run iff it has ≥5 members and a
/// consecutive-timecode pair. ≥5 members: any real desync strands at least
/// a fraction of a second of frames (25/s), while noise-induced chance
/// chains never stack four exact 80-bit hops *and* a consecutive-TC pair.
fn flush_chain_run(run: &[usize], bits: &[u8], fps: f64, drop_frame: bool) -> Vec<usize> {
    if run.len() < 5 {
        return Vec::new();
    }
    let tcs: Vec<Option<Timecode>> = run
        .iter()
        .map(|&sp| {
            sp.checked_sub(SYNC_OFFSET)
                .filter(|&fs| fs + 80 <= bits.len())
                .map(|fs| decode_timecode_from_bits(bits, fs))
        })
        .collect();
    let sequential = tcs.windows(2).any(|w| {
        matches!(
            (&w[0], &w[1]),
            (Some(a), Some(b)) if crate::increment_timecode(a, fps, drop_frame) == *b
        )
    });
    if !sequential {
        return Vec::new();
    }
    run.iter()
        .filter_map(|&sp| sp.checked_sub(SYNC_OFFSET))
        .collect()
}

/// Near-duplicate chains: a noise-shifted sync match can spawn a second
/// chain 1–2 bits off a first one; both decode to the same frame values.
/// Cluster chain starts closer than half a frame (40 bits), keeping the
/// first of each cluster. (Distinct true frames sit exactly 80 bits
/// apart, so the cluster radius cannot merge neighbours.)
fn dedupe_near_duplicates(mut chain_starts: Vec<usize>) -> Vec<usize> {
    chain_starts.sort_unstable();
    let mut clustered: Vec<usize> = Vec::with_capacity(chain_starts.len());
    for &c in &chain_starts {
        let near_prev = clustered
            .last()
            .is_some_and(|&k| c - k < 40);
        if !near_prev {
            clustered.push(c);
        }
    }
    clustered
}

/// Duplicate resolution, grid-preferred: in fading or impulse noise a
/// noise-shifted sync match can spawn a chain copy of a frame the grid
/// already accepted — its payload crosses bit boundaries differently and
/// decodes wrong where the grid copy decoded right, and keeping both
/// yields duplicated timecode values. A chain start within half a frame
/// (40 bits) of a grid start is the same frame; the grid copy wins.
/// Chains matter exactly where the grid is dark — the shifted segment
/// after a desync — and those have no grid neighbour.
fn merge_grid_and_chains(mut grid: Vec<usize>, chains: Vec<usize>) -> Vec<usize> {
    let mut merged = std::mem::take(&mut grid);
    for c in chains {
        let superseded = merged.iter().any(|&g| c.abs_diff(g) < 40);
        if !superseded {
            merged.push(c);
        }
    }
    merged.sort_unstable();
    merged.dedup();
    merged
}

// ── Timecode decoding ────────────────────────────────────────────────────────

fn decode_timecode_from_bits(bits: &[u8], frame_start: usize) -> Timecode {
    let frame_units = bits_to_u8(&bits[frame_start..frame_start + 4]);
    let frame_tens = bits_to_u8(&bits[frame_start + 8..frame_start + 10]);
    let frames = frame_tens * 10 + frame_units;

    let sec_units = bits_to_u8(&bits[frame_start + 16..frame_start + 20]);
    let sec_tens = bits_to_u8(&bits[frame_start + 24..frame_start + 27]);
    let seconds = sec_tens * 10 + sec_units;

    let min_units = bits_to_u8(&bits[frame_start + 32..frame_start + 36]);
    let min_tens = bits_to_u8(&bits[frame_start + 40..frame_start + 43]);
    let minutes = min_tens * 10 + min_units;

    let hour_units = bits_to_u8(&bits[frame_start + 48..frame_start + 52]);
    let hour_tens = bits_to_u8(&bits[frame_start + 56..frame_start + 58]);
    let hours = hour_tens * 10 + hour_units;

    Timecode {
        hours: hours as u32,
        minutes: minutes as u32,
        seconds: seconds as u32,
        frames: frames as u32,
    }
}

fn bits_to_u8(slice: &[u8]) -> u8 {
    slice
        .iter()
        .enumerate()
        .fold(0u8, |acc, (i, &b)| acc | (b << i))
}

// ── Internal helper struct ───────────────────────────────────────────────────

#[derive(Clone, Debug)]
struct ScoredResult {
    fps: f64,
    drop_frame: bool,
    valid_frames: u32,
    total_possible: u32,
    timecodes: Vec<FrameTimecode>,
    details_entry: String,
    spb: f64,
    phase: usize,
    /// Whether this candidate came from the adaptive (refinement) extraction
    /// rather than the nominal `extract_bits` path.
    adaptive: bool,
    /// Frame-start bit indices in the decoded bit array. Back-filled frames
    /// before the array origin carry negative indices (still sample-valid via
    /// `phase + idx·spb`).
    frame_starts: Vec<i64>,
    /// Dominant-grid-only valid count (see `FrameScan`): feeds the
    /// ZC-interval fast-path gate, never the reported result.
    grid_valid: u32,
}

/// Decode settings plus the candidate alignment identity (spb/phase)
/// shared by every `ScoredResult::from_frame_starts` construction site.
struct CandidateTiming {
    fps: f64,
    drop_frame: bool,
    sample_rate: u32,
    spb: f64,
    phase: usize,
}

impl ScoredResult {
    /// Single home of the `(phase + start·spb)/sample_rate` timing and the
    /// frame_index renumbering — previously 4 near-copies.
    fn from_frame_starts(
        timing: CandidateTiming,
        total_possible: u32,
        bits: &[u8],
        frame_starts: Vec<usize>,
        details_entry: String,
        adaptive: bool,
        grid_valid: u32,
    ) -> ScoredResult {
        let CandidateTiming {
            fps,
            drop_frame,
            sample_rate,
            spb,
            phase,
        } = timing;
        let valid_frames = frame_starts.len() as u32;
        let timecodes: Vec<FrameTimecode> = frame_starts
            .iter()
            .enumerate()
            .map(|(idx, &start)| FrameTimecode {
                frame_index: idx as u32,
                timecode: decode_timecode_from_bits(bits, start),
                timecode_secs: (phase as f64 + start as f64 * spb) / sample_rate as f64,
            })
            .collect();
        ScoredResult {
            fps,
            drop_frame,
            valid_frames,
            total_possible,
            timecodes,
            details_entry,
            spb,
            phase,
            adaptive,
            frame_starts: frame_starts.into_iter().map(|s| s as i64).collect(),
            grid_valid,
        }
    }

    /// The zeroed "Canceled" stub shape.
    fn canceled(params: &ScoredResult, phase: usize) -> ScoredResult {
        ScoredResult {
            fps: params.fps,
            drop_frame: params.drop_frame,
            valid_frames: 0,
            grid_valid: 0,
            total_possible: 0,
            timecodes: Vec::new(),
            details_entry: "Canceled".to_string(),
            spb: params.spb,
            phase,
            adaptive: params.adaptive,
            frame_starts: Vec::new(),
        }
    }
}

/// Replaces the inline cancel checks previously repeated across the
/// inner/evaluate path.
fn cancelled(cancel: Option<&AtomicBool>) -> bool {
    cancel.is_some_and(|c| c.load(Ordering::Relaxed))
}

/// Run a single-pass `extract_bits` + `find_frames` on the full sample buffer
/// using the SPB/phase discovered from a window eval. Returns a populated
/// `ScoredResult` with timecodes computed from the full decode.
fn decode_full_file(
    samples: &[f32],
    params: &ScoredResult,
    threshold: f32,
    sample_rate: u32,
    phase_offset: usize,
    zero_crossings: &[usize],
    cancel: Option<&AtomicBool>,
) -> ScoredResult {
    let absolute_phase = params.phase + phase_offset;

    let bits_nominal = extract_bits(samples, params.spb, absolute_phase, threshold, cancel);
    if cancelled(cancel) {
        return ScoredResult::canceled(params, absolute_phase);
    }
    let scan_nominal = find_frames(&bits_nominal, params.fps, params.drop_frame);
    let (valid_nominal, total_possible, frame_starts_nominal) =
        (scan_nominal.valid_frames, scan_nominal.total_possible, scan_nominal.frame_starts);

    if cancelled(cancel) {
        return ScoredResult::canceled(params, absolute_phase);
    }
    let bits_adaptive = extract_bits_adaptive(samples, params.spb, absolute_phase, threshold, zero_crossings, cancel);
    let scan_adaptive = find_frames(&bits_adaptive, params.fps, params.drop_frame);
    let (valid_adaptive, total_possible_adaptive, frame_starts_adaptive) =
        (scan_adaptive.valid_frames, scan_adaptive.total_possible, scan_adaptive.frame_starts);

    let (use_adaptive, valid_frames, total_possible, frame_starts, bits) = if valid_adaptive > valid_nominal {
        (true, valid_adaptive, total_possible_adaptive, frame_starts_adaptive, bits_adaptive)
    } else {
        (false, valid_nominal, total_possible, frame_starts_nominal, bits_nominal)
    };

    let method = if use_adaptive { "adaptive" } else { "nominal" };
    let details_entry = format!(
        "{:.2} fps: {} valid / {} possible frames (single-pass {method}, spb={:.2}, phase={})",
        params.fps, valid_frames, total_possible, params.spb, absolute_phase
    );
    let grid_valid = if use_adaptive { scan_adaptive.grid_valid } else { scan_nominal.grid_valid };
    ScoredResult::from_frame_starts(
        CandidateTiming {
            fps: params.fps,
            drop_frame: params.drop_frame,
            sample_rate,
            spb: params.spb,
            phase: absolute_phase,
        },
        total_possible,
        &bits,
        frame_starts,
        details_entry,
        use_adaptive,
        grid_valid,
    )
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{generate_ltc_frame_stereo, increment_timecode, ChannelSel, Timecode};

    // ── bits_to_u8 ───────────────────────────────────────────────────────

    #[test]
    fn test_bits_to_u8_zero() {
        assert_eq!(bits_to_u8(&[0, 0, 0, 0]), 0);
    }

    #[test]
    fn test_bits_to_u8_single_bit() {
        assert_eq!(bits_to_u8(&[1, 0, 0, 0]), 1);
    }

    #[test]
    fn test_bits_to_u8_multiple_bits() {
        assert_eq!(bits_to_u8(&[1, 0, 1, 0]), 5);
        assert_eq!(bits_to_u8(&[0, 1, 0, 1]), 0b1010);
    }

    #[test]
    fn test_bits_to_u8_max() {
        assert_eq!(bits_to_u8(&[1, 1, 1, 1]), 15);
    }

    #[test]
    fn test_bits_to_u8_truncated() {
        assert_eq!(bits_to_u8(&[1, 0]), 1);
        assert_eq!(bits_to_u8(&[1, 1, 0, 0, 0, 0, 0, 0]), 3);
    }

    // ── get_ltc_bits: sync word ──────────────────────────────────────────

    #[test]
    fn test_get_ltc_bits_sync_word() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        let sync = &bits[SYNC_OFFSET..SYNC_OFFSET + 16];
        assert_eq!(sync, SYNC_WORD, "sync word at bits 64-79 should be 0011111111111101");
    }

    // ── get_ltc_bits: known timecode values ──────────────────────────────

    // ── decode_timecode_from_bits round-trip ─────────────────────────────

    #[test]
    fn test_decode_timecode_from_bits_later_frame() {
        let tc0 = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let tc1 = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let bits0 = crate::get_ltc_bits(&tc0, false);
        let bits1 = crate::get_ltc_bits(&tc1, false);
        let mut both = [0u8; 160];
        both[..80].copy_from_slice(&bits0);
        both[80..].copy_from_slice(&bits1);
        let decoded = decode_timecode_from_bits(&both, 80);
        assert_eq!(decoded, tc1);
    }

    // ── increment_timecode ───────────────────────────────────────────────

    #[test]
    fn test_increment_timecode_29_97_non_drop() {
        let mut tc = Timecode { hours: 1, minutes: 0, seconds: 59, frames: 29 };
        tc = increment_timecode(&tc, 29.97, false);
        assert_eq!(tc, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 0 });
    }

    // ── estimate_noise_floor ─────────────────────────────────────────────

    #[test]
    fn test_estimate_noise_floor_constant() {
        let samples = vec![0.5f32; 1000];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_estimate_noise_floor_silent() {
        let samples = vec![0.0f32; 1000];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 1e-10).abs() < 1e-12);
    }

    #[test]
    fn test_estimate_noise_floor_mixed() {
        let samples: Vec<f32> = vec![0.5, 0.1, 0.3, 0.8, 0.2];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_estimate_noise_floor_negative_values() {
        let samples: Vec<f32> = vec![-0.7, -0.1, -0.5, -0.3];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_estimate_noise_floor_clamped_10k() {
        let samples = vec![0.42f32; 20_000];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 0.42).abs() < 1e-6);
    }

    // ── find_zero_crossings ──────────────────────────────────────────────

    #[test]
    fn test_find_zero_crossings_basic() {
        let samples = vec![0.1f32, -0.2, 0.3, -0.1];
        let crossings = find_zero_crossings(&samples, 0.05);
        assert_eq!(crossings, vec![1, 2, 3]);
    }

    #[test]
    fn test_find_zero_crossings_silent() {
        let samples = vec![0.0f32; 100];
        let crossings = find_zero_crossings(&samples, 0.01);
        assert!(crossings.is_empty());
    }

    #[test]
    fn test_find_zero_crossings_below_threshold() {
        let samples = vec![0.01f32, -0.02, 0.01];
        let crossings = find_zero_crossings(&samples, 0.05);
        assert!(crossings.is_empty());
    }

    #[test]
    fn test_find_zero_crossings_alternating() {
        let samples: Vec<f32> = (0..20).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        let crossings = find_zero_crossings(&samples, 0.1);
        assert_eq!(crossings.len(), 19);
        for (idx, &pos) in crossings.iter().enumerate() {
            assert_eq!(pos, idx + 1, "crossing position mismatch");
        }
    }

    #[test]
    fn test_find_zero_crossings_stays_positive() {
        let samples = vec![0.5f32, 0.3, 0.1, -0.2, -0.4];
        let crossings = find_zero_crossings(&samples, 0.05);
        assert_eq!(crossings, vec![3]);
    }

    // ── extract_bits (bi-phase mark decoding) ────────────────────────────

    fn synthesize_bit(samples_per_bit: usize, bit_value: u8, start_level: f32) -> (Vec<f32>, f32) {
        let mut buf = vec![0.0f32; samples_per_bit];
        let half = samples_per_bit / 2;
        match bit_value {
            0 => {
                for s in buf.iter_mut() { *s = start_level; }
                (buf, start_level)
            }
            1 => {
                let mid_level = -start_level;
                buf[..half].fill(start_level);
                buf[half..samples_per_bit].fill(mid_level);
                (buf, mid_level)
            }
            _ => unreachable!(),
        }
    }

    /// Generate a single bit of SMPTE-standard bi-phase mark: every bit starts
    /// with a transition (level flips), and bit=1 adds a second transition at
    /// the midpoint. This matches real-world LTC where every bit boundary
    /// produces a zero-crossing, giving adaptive extraction a signal to follow.
    fn synthesize_bit_smpte(samples_per_bit: usize, bit_value: u8, start_level: f32) -> (Vec<f32>, f32) {
        let mut buf = vec![0.0f32; samples_per_bit];
        let half = samples_per_bit / 2;
        let first_half_level = -start_level;
        match bit_value {
            0 => {
                buf.fill(first_half_level);
                (buf, first_half_level)
            }
            1 => {
                let second_half_level = start_level;
                buf[..half].fill(first_half_level);
                buf[half..].fill(second_half_level);
                (buf, second_half_level)
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_extract_bits_all_zeros() {
        let spb = 8.0;
        let mut signal = Vec::new();
        let mut level = 0.5;
        for _ in 0..3 {
            let (chunk, l) = synthesize_bit(spb as usize, 0, level);
            signal.extend(chunk);
            level = l;
        }
        let bits = extract_bits(&signal, spb, 0, 0.01, None);
        assert_eq!(bits, vec![0, 0, 0]);
    }

    #[test]
    fn test_extract_bits_all_ones() {
        let spb = 8.0;
        let mut signal = Vec::new();
        let mut level = 0.5;
        for _ in 0..3 {
            let (chunk, l) = synthesize_bit(spb as usize, 1, level);
            signal.extend(chunk);
            level = l;
        }
        let bits = extract_bits(&signal, spb, 0, 0.01, None);
        assert_eq!(bits, vec![1, 1, 1]);
    }

    #[test]
    fn test_extract_bits_alternating() {
        let spb = 8.0;
        let mut signal = Vec::new();
        let mut level = 0.5;
        for &bit in &[0u8, 1, 0, 1] {
            let (chunk, l) = synthesize_bit(spb as usize, bit, level);
            signal.extend(chunk);
            level = l;
        }
        let bits = extract_bits(&signal, spb, 0, 0.01, None);
        assert_eq!(bits, vec![0, 1, 0, 1]);
    }

    #[test]
    fn test_extract_bits_below_threshold() {
        let spb = 8.0;
        let signal = vec![0.0f32; (spb as usize) * 3];
        let bits = extract_bits(&signal, spb, 0, 0.1, None);
        assert_eq!(bits, vec![0, 0, 0]);
    }

    #[test]
    fn test_extract_bits_phase_offset() {
        let spb = 8.0;
        let mut signal = Vec::new();
        let mut level = 0.5;
        for &bit in &[1u8, 0, 1] {
            let (chunk, l) = synthesize_bit(spb as usize, bit, level);
            signal.extend(chunk);
            level = l;
        }
        let mut padded = vec![0.0f32; 3];
        padded.extend(signal);
        let bits = extract_bits(&padded, spb, 3, 0.01, None);
        assert_eq!(bits, vec![1, 0, 1]);
    }

    // ── find_frames (sync word detection) ────────────────────────────────

    fn build_frame_bits(tc: Timecode) -> Vec<u8> {
        let mut bits = crate::get_ltc_bits(&tc, false).to_vec();
        bits.resize(80, 0);
        bits
    }

    #[test]
    fn test_find_frames_single_frame() {
        let bits = build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 });
        let scan = find_frames(&bits, 25.0, false);
        let (valid, total, starts) = (scan.valid_frames, scan.total_possible, scan.frame_starts);
        assert_eq!(valid, 1);
        assert_eq!(total, 1);
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn test_find_frames_two_frames() {
        let mut bits = build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 });
        bits.extend(build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: 1 }));
        let scan = find_frames(&bits, 25.0, false);
        let (valid, starts) = (scan.valid_frames, scan.frame_starts);
        assert_eq!(valid, 2);
        assert_eq!(starts, vec![0, 80]);
    }

    #[test]
    fn test_find_frames_no_sync_word() {
        let bits = vec![0u8; 160];
        let scan = find_frames(&bits, 25.0, false);
        let (valid, starts) = (scan.valid_frames, scan.frame_starts);
        assert_eq!(valid, 0);
        assert!(starts.is_empty());
    }

    #[test]
    fn test_find_frames_random_bits() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut bits = vec![0u8; 320];
        let seed = 42u64;
        for (i, b) in bits.iter_mut().enumerate() {
            let mut h = DefaultHasher::new();
            (i as u64).hash(&mut h);
            seed.hash(&mut h);
            *b = (h.finish() & 1) as u8;
        }
        let valid = find_frames(&bits, 25.0, false).valid_frames;
        // False-positive *ceiling* on garbage input: legitimate per the Test
        // Quality Rules (noise must not decode). `valid <= 2` bounds the sync
        // word false-match rate over 320 random bits; a decoder that got
        // *better* at rejecting noise keeps passing, one that got worse fails
        // here.
        assert!(valid <= 2, "random bits should produce at most 2 false sync word matches, got {}", valid);
    }

    #[test]
    fn test_find_frames_short_buffer() {
        let bits = vec![0u8; 10];
        let scan = find_frames(&bits, 25.0, false);
        let (valid, total, starts) = (scan.valid_frames, scan.total_possible, scan.frame_starts);
        assert_eq!(valid, 0);
        assert_eq!(total, 0);
        assert!(starts.is_empty());
    }

    #[test]
    fn test_find_frames_alignment_matters() {
        let mut bits = vec![0u8; 160];
        bits[0..16].copy_from_slice(&SYNC_WORD);
        let valid = find_frames(&bits, 25.0, false).valid_frames;
        assert_eq!(valid, 0, "sync word at wrong offset should not produce valid frames");
    }

    #[test]
    fn test_find_frames_resyncs_after_bit_glitch() {
        // A real-world disturbance (drop-out, noise burst) inserts or removes
        // bits from the extracted bitstream, shifting every subsequent frame
        // off the 80-bit grid. Frame detection must re-lock at each detected
        // sync word — losing only the glitched frame — not decode from one
        // global alignment grid (which discards everything after the first
        // shift; the A2/A3 long-file collapse).
        let mut bits = Vec::new();
        for f in 0..6u32 {
            bits.extend(build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: f }));
        }
        bits.extend_from_slice(&[0, 1, 0]); // glitch: permanent 3-bit shift
        for f in 6..12u32 {
            bits.extend(build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: f }));
        }
        let scan = find_frames(&bits, 25.0, false);
        let (valid, starts) = (scan.valid_frames, scan.frame_starts);
        assert_eq!(valid, 12, "all frames must be found across the bit shift, starts={:?}", starts);
        assert_eq!(
            starts,
            vec![0, 80, 160, 240, 320, 400, 483, 563, 643, 723, 803, 883],
            "post-shift frames start 3 bits past the grid"
        );
    }

    /// Deterministic mono LTC synthesis for in-memory decode tests.
    fn synth_ltc_mono(
        start_tc: Timecode,
        fps: f64,
        sample_rate: u32,
        total_frames: usize,
        volume: f32,
    ) -> Vec<f32> {
        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;
        let mut tc = start_tc;
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];
        let mut out = Vec::with_capacity(total_frames * samples_per_frame);
        for _ in 0..total_frames {
            frame_buf.fill(0.0);
            crate::generate_ltc_frame_stereo(crate::ltc_encoder::LtcFrameParams { tc: &tc, drop_frame: false, total_samples: samples_per_frame, samples_per_bit, volume, channel: ChannelSel::Left },
        &mut last_level,
        &mut frame_buf
    );
            out.extend(frame_buf.iter().step_by(2).copied());
            tc = crate::increment_timecode(&tc, fps, false);
        }
        out
    }

    #[test]
    fn test_decode_recovers_after_mid_stream_glitch_burst() {
        // Real-world failure shape (WP-RW A2/A3): a short disturbance inside
        // a long clean LTC recording must cost only the glitched frames. The
        // extracted bitstream shifts across the burst; frame detection that
        // decodes from one global alignment grid loses *everything after the
        // burst* (measured: chunk 0 of TASCAM_0094S2.wav 62.4% where 120 s
        // slices decode 94–100%).
        let fps = 25.0;
        let sample_rate = 48000u32;
        let total_frames = 1500usize; // 60 s: 30 s clean · 1 s burst · 29 s clean
        let mut buf = synth_ltc_mono(
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps, sample_rate, total_frames, 0.5,
        );
        let burst_start = 30 * sample_rate as usize;
        let burst_end = 31 * sample_rate as usize;
        let mut lcg = 0x1234_5678_9abc_def0u64;
        for s in &mut buf[burst_start..burst_end] {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let noise = ((lcg >> 33) as f32 / 0x7fff_ffffu32 as f32 - 1.0) * 0.8;
            *s = (*s + noise).clamp(-1.0, 1.0);
        }

        let result = decode_ltc_samples(
            &buf, sample_rate, 1, fps, false, std::time::Instant::now(), None,
        )
        .unwrap();
        assert!(
            matches!(result.status, LtcDecodeStatus::Success),
            "a 1 s burst in 60 s of clean LTC must still decode Success, got {:?}",
            result.status
        );
        // Floor well above the broken behaviour (~50% = everything after the
        // burst lost) and below perfection (the burst's own frames are gone).
        let floor = (total_frames as f64 * 0.90) as u32;
        assert!(
            result.valid_frames >= floor,
            "expected ≥ {} valid frames after glitch recovery, got {}",
            floor, result.valid_frames
        );
    }

    #[test]
    fn decode_ltc_from_wav_pre_cancelled_returns_cancelled_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pre-cancelled.wav");
        let spec = TestSignalSpec { fps: 25.0, drop_frame: false, sample_rate: 48000, channel: ChannelSel::Both, volume: 0.5 };
        generate_test_wav(&path, &spec, Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, 1.0);
        let cancel = AtomicBool::new(true);
        let err = decode_ltc_from_wav(&path, 25.0, false, Some(&cancel))
            .expect_err("pre-cancelled decode must not produce a result");
        assert_eq!(err, LtcDecodeError::Cancelled);
    }

    // ── WAV generation helper ────────────────────────────────────────────

    /// Signal spec of a generated test WAV: rate, frame shape, routing and
    /// level, shared by both WAV generation helpers.
    struct TestSignalSpec {
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        channel: ChannelSel,
        volume: f32,
    }

    fn generate_test_wav(path: &Path, spec: &TestSignalSpec, start_tc: Timecode, duration_secs: f64) {
        let TestSignalSpec { fps, drop_frame, sample_rate, channel, volume } = *spec;
        let total_frames = (duration_secs * fps).ceil() as u64;
        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let mut tc = start_tc;
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

        for _ in 0..total_frames {
            frame_buf.fill(0.0);
            generate_ltc_frame_stereo(crate::ltc_encoder::LtcFrameParams { tc: &tc, drop_frame, total_samples: samples_per_frame, samples_per_bit, volume, channel },
        &mut last_level,
        &mut frame_buf[..samples_per_frame * 2]
    );

            for &sample in &frame_buf[..samples_per_frame * 2] {
                let clamped = sample.clamp(-1.0, 1.0);
                let int_sample = (clamped * i16::MAX as f32) as i16;
                writer.write_sample(int_sample).unwrap();
            }

            tc = increment_timecode(&tc, fps, drop_frame);
        }

        writer.finalize().unwrap();
    }

    // Prefixed variant: silent_prefix_secs travels separately, the rest is
    // the shared TestSignalSpec.
    fn generate_test_wav_with_prefix(
        path: &Path,
        spec: &TestSignalSpec,
        silent_prefix_secs: f64,
        start_tc: Timecode,
        duration_secs: f64,
    ) {
        let TestSignalSpec { fps, drop_frame, sample_rate, channel, volume } = *spec;
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let mut writer = hound::WavWriter::create(path, spec).unwrap();

        let silence_samples = (silent_prefix_secs * sample_rate as f64).round() as usize;
        for _ in 0..silence_samples * 2 {
            writer.write_sample(0i16).unwrap();
        }

        let total_frames = (duration_secs * fps).ceil() as u64;
        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;

        let mut tc = start_tc;
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

        for _ in 0..total_frames {
            frame_buf.fill(0.0);
            generate_ltc_frame_stereo(crate::ltc_encoder::LtcFrameParams { tc: &tc, drop_frame, total_samples: samples_per_frame, samples_per_bit, volume, channel },
        &mut last_level,
        &mut frame_buf[..samples_per_frame * 2]
    );

            for &sample in &frame_buf[..samples_per_frame * 2] {
                let clamped = sample.clamp(-1.0, 1.0);
                let int_sample = (clamped * i16::MAX as f32) as i16;
                writer.write_sample(int_sample).unwrap();
            }

            tc = increment_timecode(&tc, fps, drop_frame);
        }

        writer.finalize().unwrap();
    }

    // The `valid_frames` floors below tolerate nothing after the lock-in fix
    // except genuine edge physics (the roundtrip fixture's own frame-count
    // rounding). They stay floors, not equalities, so a future decoder
    // improvement never turns them red.
    fn verify_roundtrip(
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        channel: ChannelSel,
        volume: f32,
        sample_rate: u32,
        duration_secs: f64,
    ) -> LtcDetectionResult {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test_ltc.wav");
        let spec = TestSignalSpec { fps, drop_frame, sample_rate, channel, volume };
        generate_test_wav(&path, &spec, start_tc, duration_secs);
        decode_ltc_from_wav(&path, fps, drop_frame, None).unwrap()
    }

    // ── WAV round-trip tests ─────────────────────────────────────────────

    #[test]
    fn test_wav_roundtrip_25fps() {
        let result = verify_roundtrip(
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, ChannelSel::Both, 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 48, "expected ~50 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_24fps() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            24.0, false, ChannelSel::Both, 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 46, "expected ~48 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_30fps() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            30.0, false, ChannelSel::Both, 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 58, "expected ~60 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_2997_nd() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            29.97, false, ChannelSel::Both, 0.5, 48000, 3.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for 29.97 ND, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_2997_df() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            29.97, true, ChannelSel::Both, 0.5, 48000, 3.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for 29.97 DF, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    // ── Back-fill of leading frames (lock-in recovery) ───────────────────

    #[test]
    fn test_backfill_single_frame_signal_decodes_start_tc() {
        let tcs = vec![Timecode { hours: 5, minutes: 6, seconds: 7, frames: 8 }];
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert_eq!(result.timecodes.len(), 1, "the only frame must decode");
        assert_eq!(result.timecodes[0].timecode, tcs[0]);
    }

    #[test]
    fn test_backfill_two_frame_signal_decodes_both() {
        let tcs = vec![
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 1 },
        ];
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        let decoded: Vec<Timecode> = result.timecodes.iter().map(|f| f.timecode).collect();
        assert_eq!(decoded, tcs, "both frames must decode in order");
    }

    #[test]
    fn test_backfill_stops_at_corrupt_first_frame() {
        let tcs: Vec<Timecode> = (0..8).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 1, frames: i,
        }).collect();
        let mut signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        // Destroy frame 0 entirely (silence): back-fill must reject it and
        // never invent a frame.
        let frame_samples = 48000 / 25;
        for s in signal[..frame_samples].iter_mut() {
            *s = 0.0;
        }
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        let decoded: Vec<Timecode> = result.timecodes.iter().map(|f| f.timecode).collect();
        assert_eq!(decoded.first(), Some(&tcs[1]),
            "decode must start at the first intact frame (frame 0 corrupted)");
        assert_eq!(decoded, tcs[1..], "every intact frame must decode, none invented");
    }

    #[test]
    fn test_wav_roundtrip_different_start_tc() {
        let result = verify_roundtrip(
            Timecode { hours: 10, minutes: 15, seconds: 30, frames: 12 },
            25.0, false, ChannelSel::Both, 0.5, 48000, 1.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 22, "expected ~25 valid frames, got {}", result.valid_frames);
        assert!(!result.timecodes.is_empty());
        assert_eq!(result.timecodes[0].timecode,
            Timecode { hours: 10, minutes: 15, seconds: 30, frames: 12 },
            "first decoded frame must be the start TC");
    }

    #[test]
    fn test_wav_roundtrip_44khz() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, ChannelSel::Both, 0.5, 44100, 2.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error at 44kHz, got {:?}", result.status);
    }

    #[test]
    fn test_wav_roundtrip_48khz() {
        let result = verify_roundtrip(
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, ChannelSel::Both, 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success at 48kHz, got {:?}", result.status);
    }

    #[test]
    fn test_wav_roundtrip_left_channel() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, ChannelSel::Left, 0.5, 48000, 1.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success on left channel, got {:?}", result.status);
    }

    #[test]
    fn test_wav_roundtrip_right_channel_auto_selected() {
        // The generator supports ChannelSel::Right output; WAV decode has no
        // channel override (only video files get explicit channel selection
        // via ffmpeg extraction). If channel 0 is (near-)silent, the channel
        // carrying signal is decoded instead — the tool can read back its
        // own right-channel output.
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, ChannelSel::Right, 0.5, 48000, 1.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success on right channel, got {:?}", result.status);
        assert!(result.valid_frames >= 20,
            "expected ~25 valid frames on right channel, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_channel0_preferred_when_nonsilent() {        // Policy: files with independent program material on channel 0 keep
        // today's channel-0 semantics — a louder ch1 never hijacks them.
        // ch0 = low-level noise (peak ~0.05, well above SILENT_CHANNEL_PEAK
        // ~= 6e-5), ch1 = valid LTC → decode must NOT succeed.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ch0_program.wav");
        let fps = 25.0f64;
        let sample_rate = 48000u32;
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        let mut tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];
        for _ in 0..50 {
            frame_buf.fill(0.0);
            generate_ltc_frame_stereo(crate::ltc_encoder::LtcFrameParams { tc: &tc, drop_frame: false, total_samples: samples_per_frame, samples_per_bit, volume: 0.5, channel: ChannelSel::Right },
        &mut last_level,
        &mut frame_buf
    );
            for (i, &sample) in frame_buf.iter().enumerate() {
                let s = if i % 2 == 0 {
                    // ch0: low-level noise, deterministic, peak ~0.05
                    ((i * 2654435761) as f32).sin() * 0.05
                } else {
                    sample
                };
                let clamped = s.clamp(-1.0, 1.0);
                writer.write_sample((clamped * i16::MAX as f32) as i16).unwrap();
            }
            tc = increment_timecode(&tc, fps, false);
        }
        writer.finalize().unwrap();

        let result = decode_ltc_from_wav(&path, fps, false, None).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Success),
            "non-silent ch0 must be decoded; a louder ch1 must not hijack, got {:?}",
            result.status);
    }

    // ── pick_active_channel ──────────────────────────────────────────────

    #[test]
    fn test_pick_active_channel_loud_ch0_kept_even_if_ch1_louder() {
        // Back-compat: a non-silent ch0 is always kept, even when another
        // channel is louder — program material on ch0 must not be hijacked.
        assert_eq!(pick_active_channel(&[0.5, 0.9]), 0);
        assert_eq!(pick_active_channel(&[0.001, 0.9, 0.9]), 0);
    }

    #[test]
    fn test_pick_active_channel_silent_ch0_selects_loudest_other() {
        assert_eq!(pick_active_channel(&[0.0, 0.5]), 1);
        assert_eq!(pick_active_channel(&[0.0, 0.2, 0.8, 0.3]), 2);
    }

    #[test]
    fn test_pick_active_channel_ties_prefer_lowest_index() {
        assert_eq!(pick_active_channel(&[0.0, 0.5, 0.5]), 1);
    }

    #[test]
    fn test_pick_active_channel_all_silent_or_empty_selects_zero() {
        assert_eq!(pick_active_channel(&[0.0, 0.0]), 0);
        assert_eq!(pick_active_channel(&[]), 0, "degenerate, defensive");
        assert_eq!(pick_active_channel(&[1e-9]), 0);
    }

    // ── Edge case: mid-signal start ──────────────────────────────────────

    #[test]
    fn test_wav_mid_signal_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mid_signal.wav");

        let spec = TestSignalSpec { fps: 25.0, drop_frame: false, sample_rate: 48000, channel: ChannelSel::Both, volume: 0.5 };
        generate_test_wav_with_prefix(&path, &spec, 0.5, Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, 1.5);

        let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for mid-signal LTC, got {:?} (details: {:?})",
            result.status, result.details);
        assert!(result.valid_frames >= 35, "expected ~37 valid frames, got {}", result.valid_frames);
        if let Some(first) = result.timecodes.first() {
            assert_eq!(first.timecode.hours, 1);
            assert_eq!(first.timecode.minutes, 0);
            assert_eq!(first.timecode.seconds, 0);
            assert_eq!(first.timecode.frames, 0);
        }
    }

    // ── Edge case: first_ltc_timecode_secs with large silent prefix ──────

    #[test]
    fn test_wav_first_ltc_offset() {
        for &silent_secs in &[0.0, 1.0, 30.0, 120.0] {
            let dir = tempfile::TempDir::new().unwrap();
            let path = dir.path().join("offset_test.wav");

            let spec = TestSignalSpec { fps: 25.0, drop_frame: false, sample_rate: 48000, channel: ChannelSel::Both, volume: 0.5 };
            generate_test_wav_with_prefix(&path, &spec, silent_secs, Timecode { hours: 2, minutes: 0, seconds: 0, frames: 0 }, 1.0);

            let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
            assert!(matches!(result.status, LtcDecodeStatus::Success | LtcDecodeStatus::LowConfidence),
                "silent_prefix={:.1}s: expected Success/LowConfidence, got {:?}",
                silent_secs, result.status);

            let expected_offset = silent_secs;
            let tolerance = 0.05;

            let abs_diff = (result.first_ltc_timecode_secs - expected_offset).abs();
            assert!(
                abs_diff < tolerance,
                "silent_prefix={:.1}s: first_ltc_timecode_secs={:.6}s, expected ≈{:.3}s (diff={:.6}s > {:.3}s)",
                silent_secs, result.first_ltc_timecode_secs, expected_offset, abs_diff, tolerance,
            );

            if let Some(first_tc) = result.timecodes.first() {
                let tc_diff = (result.first_ltc_timecode_secs - first_tc.timecode_secs).abs();
                assert!(
                    tc_diff < 0.001,
                    "silent_prefix={:.1}s: first_ltc_timecode_secs ({:.6}s) != timecodes[0].timecode_secs ({:.6}s), diff={:.6}s",
                    silent_secs, result.first_ltc_timecode_secs, first_tc.timecode_secs, tc_diff,
                );
            }
        }
    }

    // ── Edge case: single frame ──────────────────────────────────────────

    #[test]
    fn test_wav_single_frame() {
        let result = verify_roundtrip(
            Timecode { hours: 12, minutes: 34, seconds: 56, frames: 18 },
            25.0, false, ChannelSel::Both, 0.5, 48000, 0.12,
        );
        assert!(result.valid_frames >= 1,
            "expected at least 1 valid frame with 3-frame signal, got {}", result.valid_frames);
    }

    // ── Edge case: silent audio ──────────────────────────────────────────

    #[test]
    fn test_wav_silent() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("silent.wav");

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..48000 * 2 {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();

        let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }),
            "silent audio should produce Error, got {:?}", result.status);
    }

    // ── Edge case: random noise ──────────────────────────────────────────

    #[test]
    fn test_wav_noise() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("noise.wav");

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0u64..96000u64 {
            let val = ((i.wrapping_mul(1103515245).wrapping_add(12345)) % 65536) as i16;
            writer.write_sample(val).unwrap();
        }
        writer.finalize().unwrap();

        let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
        assert!(
            matches!(result.status, LtcDecodeStatus::NoSyncWord | LtcDecodeStatus::Error { .. }),
            "random noise should not produce Success, got {:?}", result.status
        );
    }

    // ── Edge case: very short audio ──────────────────────────────────────

    #[test]
    fn test_wav_very_short() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("short.wav");

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..100 {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();

        let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }),
            "very short audio should produce Error, got {:?}", result.status);
    }

    #[test]
    fn test_wav_empty_file() {
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

        let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }),
            "empty WAV should produce Error, got {:?}", result.status);
    }

    // ── Low-amplitude LTC (below old 0.005 min threshold) ────────────────

    #[test]
    fn test_wav_low_amplitude_ltc() {
        let result = verify_roundtrip(
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, ChannelSel::Both, 0.12, 48000, 2.0,
        );
        assert!(result.valid_frames > 0,
            "expected >0 valid frames with low-amplitude LTC (volume=0.12, amp≈0.014), got {}/{}",
            result.valid_frames, result.total_possible_frames);
    }

    // ── decode_ltc_from_wav valid/invalid round-trip ─────────────────────
    // (Formerly pinned via the deleted quick_check_ltc wrapper.)

    #[test]
    fn test_decode_ltc_from_wav_valid_status() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("valid_check.wav");
        let spec = TestSignalSpec { fps: 25.0, drop_frame: false, sample_rate: 48000, channel: ChannelSel::Both, volume: 0.5 };
        generate_test_wav(&path, &spec, Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 }, 1.0);
        let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
        assert!(
            matches!(result.status, LtcDecodeStatus::Success),
            "valid LTC should decode to Success, got {:?}",
            result.status
        );
    }

    #[test]
    fn test_decode_ltc_from_wav_silent_file_not_success() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("invalid_check.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..48000 {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
        let result = decode_ltc_from_wav(&path, 25.0, false, None).unwrap();
        assert!(
            !matches!(result.status, LtcDecodeStatus::Success),
            "silent file must not decode to Success, got {:?}",
            result.status
        );
    }

    // ── Real-world LTC test ──────────────────────────────────────────────

    #[test]
    fn test_wav_real_world_ltc() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let wav_path = manifest_dir
            .parent()
            .expect("CARGO_MANIFEST_DIR parent")
            .join("test-data")
            .join("ltc-real-world-test-20sec.wav");

        if !wav_path.exists() {
            panic!("Real-world LTC test file not found at: {}", wav_path.display());
        }

        let result = decode_ltc_from_wav(&wav_path, 25.0, false, None).unwrap();

        assert!(
            matches!(result.status, LtcDecodeStatus::Success),
            "Expected Success for real-world LTC, got {:?} (valid={}/{}, conf={:.1}%)",
            result.status,
            result.valid_frames,
            result.total_possible_frames,
            result.avg_confidence * 100.0,
        );

        assert!(
            result.valid_frames >= 450,
            "Expected ≥450 valid frames from 20s real-world LTC, got {}",
            result.valid_frames,
        );

        let frames_spanned = result.total_possible_frames.max(1) - 1;
        let secs_spanned = frames_spanned as f64 / result.detected_fps as f64;
        assert!(
            secs_spanned > 15.0,
            "Real-world LTC should span >15s of timecode, got {:.2}s ({} possible frames @ {:.2}fps)",
            secs_spanned, result.total_possible_frames, result.detected_fps,
        );
    }

    // ── Real-world corpus fixtures (WP-RW) ───────────────────────────────
    // Committed cuts from the 5-day recording corpus; see
    // reports/2026-10-05-ltc-chunked-decode-anomalies-report.md. Floors are
    // measured −10 % (measurement date 2026-10-05, decoder post-RW2).

    fn real_world_fixture(name: &str) -> std::path::PathBuf {
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

    /// 24-bit PCM WAV (TASCAM S2 track, cut at 1650 s of the 49-min
    /// recording) — the 24-bit decode path no synthetic test covers
    /// (synthetic fixtures are 16-bit).
    #[test]
    fn test_wav_real_world_tascam_s2_clean() {
        let wav_path = real_world_fixture("ltc-rw-tascam-s2-clean-20s.wav");

        let result = decode_ltc_from_wav(&wav_path, 25.0, false, None).unwrap();

        assert!(
            matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for the clean TASCAM fixture, got {:?} (valid={}/{})",
            result.status, result.valid_frames, result.total_possible_frames,
        );
        // Measured 500/500 (2026-10-05); floor = measured − 10 %.
        assert!(
            result.valid_frames >= 450,
            "expected ≥450 valid frames, got {}",
            result.valid_frames,
        );
        // SMPTE values are the contract: measured first/last TC.
        assert_eq!(
            result.timecodes.first().map(|t| t.timecode),
            Some(Timecode { hours: 1, minutes: 48, seconds: 34, frames: 24 }),
            "first TC must match the measured corpus value",
        );
        assert_eq!(
            result.timecodes.last().map(|t| t.timecode),
            Some(Timecode { hours: 1, minutes: 48, seconds: 54, frames: 23 }),
            "last TC must match the measured corpus value",
        );
        let secs_spanned = result.timecodes.last().map(|t| t.timecode_secs).unwrap_or(0.0)
            - result.timecodes.first().map(|t| t.timecode_secs).unwrap_or(0.0);
        assert!(
            secs_spanned > 18.0,
            "20 s fixture must span >18 s of timecode, got {:.2}s",
            secs_spanned,
        );
    }

    /// Mic track (TASCAM S1, loudest 15 s window, max_volume −11.4 dB) —
    /// voice must not decode as LTC. The sweep's 49-minute version measures
    /// `NoSyncWord`; any Success here is a false-positive regression.
    #[test]
    fn test_wav_real_world_tascam_s1_mic_negative() {
        let wav_path = real_world_fixture("ltc-rw-tascam-s1-mic-15s.wav");

        let result = decode_ltc_from_wav(&wav_path, 25.0, false, None).unwrap();

        assert!(
            !matches!(result.status, LtcDecodeStatus::Success),
            "mic track must not decode to Success, got {:?} (valid={}/{})",
            result.status, result.valid_frames, result.total_possible_frames,
        );
    }

    /// Backend support contract on the 24-bit corpus fixture: the builtin
    /// decoder decodes it (see `test_wav_real_world_tascam_s2_clean`);
    /// libltc's binding is 16-bit-only and must reject it with the typed
    /// `UnsupportedBitDepth` error (pre-existing libltc limitation — corpus
    /// cross-backend agreement is asserted through the 16-bit mp4 fixture
    /// in `gui-engine/tests/real_world_fixtures.rs`, whose extraction path
    /// produces 16-bit PCM both backends read).
    #[test]
    fn test_decoder_contract_real_world_fixtures() {
        let wav_path = real_world_fixture("ltc-rw-tascam-s2-clean-20s.wav");

        let builtin = decode_ltc_from_wav(&wav_path, 25.0, false, None).expect("builtin decode");
        assert!(matches!(builtin.status, LtcDecodeStatus::Success));

        let libltc = crate::ltc_decoder_libltc::decode_ltc_from_wav_libltc(
            &wav_path, 25.0, false, None,
        )
        .expect_err("libltc must reject 24-bit PCM");
        assert_eq!(
            libltc,
            crate::LtcDecodeError::UnsupportedBitDepth { bits: 24 },
            "libltc binding must reject 24-bit PCM with the typed error",
        );
    }

    // ── Cross-decoder confidence contract ────────────────────────────────

    /// Both decoders must publish `avg_confidence` on the same 0.0–1.0
    /// fraction scale and classify status with the same thresholds — every
    /// consumer multiplies by 100 for display and gates on status.
    #[test]
    fn test_decoder_contract_confidence_scale() {
        let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let wav_path = manifest_dir
            .parent()
            .expect("CARGO_MANIFEST_DIR parent")
            .join("test-data")
            .join("ltc-real-world-test-20sec.wav");

        if !wav_path.exists() {
            eprintln!("--- SKIPPED: real-world fixture missing at {}", wav_path.display());
            return;
        }

        let builtin = decode_ltc_from_wav(&wav_path, 25.0, false, None).expect("builtin decode");
        let libltc = crate::ltc_decoder_libltc::decode_ltc_from_wav_libltc(
            &wav_path, 25.0, false, None,
        )
        .expect("libltc decode");

        for (name, r) in [("builtin", &builtin), ("libltc", &libltc)] {
            assert!(
                (0.0..=1.0).contains(&r.avg_confidence),
                "{} decoder avg_confidence must be a 0.0–1.0 fraction, got {}",
                name,
                r.avg_confidence
            );
            assert!(
                matches!(r.status, LtcDecodeStatus::Success),
                "{} decoder should classify the clean fixture as Success, got {:?}",
                name,
                r.status
            );
        }

        assert!(
            (builtin.avg_confidence - libltc.avg_confidence).abs() < 0.25,
            "decoders disagree on frame coverage: builtin {:.3} vs libltc {:.3}",
            builtin.avg_confidence,
            libltc.avg_confidence
        );
    }

    // ── find_first_coherent_index ───────────────────────────────────────────

    #[test]
    fn test_coherent_index_clean_from_start() {
        let fps = 25.0;
        let fd = 1.0 / fps;
        let tcs: Vec<FrameTimecode> = (0..100)
            .map(|i| {
                let t = (0..i).fold(
                    Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
                    |acc, _| crate::increment_timecode(&acc, fps, false),
                );
                FrameTimecode {
                    frame_index: i as u32,
                    timecode: t,
                    timecode_secs: i as f64 * fd,
                }
            })
            .collect();
        assert_eq!(find_first_coherent_index(&tcs, fps, false), Some(0));
    }

    #[test]
    fn test_coherent_index_noisy_start() {
        let fps = 25.0;
        let fd = 1.0 / fps;
        let mut tcs = Vec::new();
        // 3 noise frames (out-of-range timecodes)
        tcs.push(FrameTimecode {
            frame_index: 0,
            timecode: Timecode { hours: 45, minutes: 85, seconds: 85, frames: 45 },
            timecode_secs: 0.0,
        });
        tcs.push(FrameTimecode {
            frame_index: 1,
            timecode: Timecode { hours: 2, minutes: 4, seconds: 14, frames: 2 },
            timecode_secs: 113.0,
        });
        tcs.push(FrameTimecode {
            frame_index: 2,
            timecode: Timecode { hours: 2, minutes: 4, seconds: 14, frames: 15 },
            timecode_secs: 113.04,
        });
        // 70 clean frames (2.8 seconds at 25fps)
        let base = Timecode { hours: 2, minutes: 4, seconds: 20, frames: 0 };
        for i in 0..70 {
            let t = (0..i).fold(base, |acc, _| crate::increment_timecode(&acc, fps, false));
            tcs.push(FrameTimecode {
                frame_index: (3 + i) as u32,
                timecode: t,
                timecode_secs: 200.0 + i as f64 * fd,
            });
        }
        let idx = find_first_coherent_index(&tcs, fps, false);
        assert_eq!(idx, Some(3));
    }

    #[test]
    fn test_coherent_index_isolated_valid_frames() {
        let fps = 25.0;
        let fd = 1.0 / fps;
        let mut tcs = Vec::new();
        for i in 0..10 {
            let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: (i * 5) as u32 };
            tcs.push(FrameTimecode {
                frame_index: i as u32,
                timecode: tc,
                timecode_secs: i as f64 * fd,
            });
        }
        assert_eq!(find_first_coherent_index(&tcs, fps, false), None);
    }

    #[test]
    fn test_coherent_index_too_few_frames() {
        let fps = 25.0;
        let tcs = vec![
            FrameTimecode {
                frame_index: 0,
                timecode: Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
                timecode_secs: 0.0,
            },
            FrameTimecode {
                frame_index: 1,
                timecode: Timecode { hours: 0, minutes: 0, seconds: 0, frames: 1 },
                timecode_secs: 0.04,
            },
        ];
        assert_eq!(find_first_coherent_index(&tcs, fps, false), None);
    }

    #[test]
    fn test_coherent_index_drop_frame() {
        let fps = 29.97;
        let fd = 1.0 / fps;
        let mut tcs = Vec::new();
        let base = Timecode { hours: 0, minutes: 9, seconds: 59, frames: 29 };
        for i in 0..120 {
            let t = (0..i).fold(base, |acc, _| crate::increment_timecode(&acc, fps, true));
            tcs.push(FrameTimecode {
                frame_index: i as u32,
                timecode: t,
                timecode_secs: i as f64 * fd,
            });
        }
        // All frames form a valid drop-frame sequence → no adjustment needed
        assert_eq!(find_first_coherent_index(&tcs, fps, true), Some(0));
    }

    #[test]
    fn test_coherent_index_midnight_wrap() {
        let fps = 25.0;
        let fd = 1.0 / fps;
        let mut tcs = Vec::new();
        let base = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 24 };
        for i in 0..150 {
            let t = (0..i).fold(base, |acc, _| crate::increment_timecode(&acc, fps, false));
            tcs.push(FrameTimecode {
                frame_index: i as u32,
                timecode: t,
                timecode_secs: i as f64 * fd,
            });
        }
        assert_eq!(find_first_coherent_index(&tcs, fps, false), Some(0));
    }

    // ── bits_hamming_distance_16 ───────────────────────────────────────

    #[test]
    fn test_bits_hamming_distance_16_exact_match() {
        assert_eq!(bits_hamming_distance_16(&SYNC_WORD), 0);
    }

    #[test]
    fn test_bits_hamming_distance_16_one_bit_flip() {
        let mut bits = SYNC_WORD;
        bits[0] ^= 1;
        assert_eq!(bits_hamming_distance_16(&bits), 1);
    }

    #[test]
    fn test_bits_hamming_distance_16_two_bit_flips() {
        let mut bits = SYNC_WORD;
        bits[0] ^= 1;
        bits[5] ^= 1;
        assert_eq!(bits_hamming_distance_16(&bits), 2);
    }

    #[test]
    fn test_bits_hamming_distance_16_three_bit_flips_early_exit() {
        let mut bits = SYNC_WORD;
        bits[0] ^= 1;
        bits[1] ^= 1;
        bits[2] ^= 1;
        assert_eq!(bits_hamming_distance_16(&bits), 3);
    }

    #[test]
    fn test_bits_hamming_distance_16_all_wrong() {
        let bits = [1u8; 16];
        assert_eq!(bits_hamming_distance_16(&bits), 3);
    }

    #[test]
    fn test_bits_hamming_distance_16_all_zero() {
        let bits = [0u8; 16];
        // SYNC_WORD has 13 ones → distance to all-zero is 13, but early exit at 3
        assert_eq!(bits_hamming_distance_16(&bits), 3);
    }

    // ── median_sample ─────────────────────────────────────────────────

    #[test]
    fn test_median_sample_middle() {
        let samples = vec![0.1, 0.5, 0.3];
        assert!((median_sample(&samples, 1) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_median_sample_left_edge() {
        let samples = vec![0.7, 0.3, 0.5];
        assert!((median_sample(&samples, 0) - 0.7).abs() < 1e-6);
    }

    #[test]
    fn test_median_sample_right_edge() {
        let samples = vec![0.1, 0.3, 0.9];
        assert!((median_sample(&samples, 2) - 0.9).abs() < 1e-6);
    }

    #[test]
    fn test_median_sample_two_samples_only() {
        let samples = vec![0.8, 0.2];
        // len=2, buf sorted → [0.2, 0.8], buf[2/2] = buf[1] = 0.8 (upper median)
        assert!((median_sample(&samples, 1) - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_median_sample_constant_values() {
        let samples = vec![0.5f32; 5];
        assert!((median_sample(&samples, 2) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_median_sample_negative_values() {
        let samples = vec![-0.7, -0.5, -0.9];
        assert!((median_sample(&samples, 1) + 0.7).abs() < 1e-6);
    }

    // ── zc_in_range ───────────────────────────────────────────────────

    #[test]
    fn test_zc_in_range_all_included() {
        let zc = vec![10, 20, 30, 40, 50];
        let result = zc_in_range(&zc, 10, 51);
        assert_eq!(result, &[10, 20, 30, 40, 50]);
    }

    #[test]
    fn test_zc_in_range_partial() {
        let zc = vec![10, 20, 30, 40, 50];
        let result = zc_in_range(&zc, 25, 45);
        assert_eq!(result, &[30, 40]);
    }

    #[test]
    fn test_zc_in_range_empty() {
        let zc = vec![10, 20, 30, 40, 50];
        let result = zc_in_range(&zc, 0, 5);
        assert!(result.is_empty());
    }

    #[test]
    fn test_zc_in_range_start_equals_end() {
        let zc = vec![10, 20, 30];
        let result = zc_in_range(&zc, 20, 20);
        assert!(result.is_empty());
    }

    #[test]
    fn test_zc_in_range_single_element() {
        let zc = vec![42];
        let result = zc_in_range(&zc, 0, 100);
        assert_eq!(result, &[42]);
    }

    #[test]
    fn test_zc_in_range_exclusive_end() {
        let zc = vec![10, 20, 30, 40];
        let result = zc_in_range(&zc, 20, 30);
        assert_eq!(result, &[20]);
    }

    // ── decode_bits_synthetic_zc ──────────────────────────────────────

    /// Build a zero-crossing array for a sequence of LTC bits.
    /// Each '1' bit produces a ZC at `(bit_index * spb + spb/2)`.
    fn bits_to_zc(bits: &[u8], spb: f64) -> Vec<usize> {
        bits.iter()
            .enumerate()
            .filter(|(_, &b)| b == 1)
            .map(|(i, _)| (i as f64 * spb + spb * 0.5).round() as usize)
            .collect()
    }

    #[test]
    fn test_decode_bits_synthetic_zc_basic() {
        // Bits: 1 followed by three 1s with regular spacing
        let spb = 24.0;
        // Bit pattern: 1,0,1,0,1
        // ZC positions: bit0 midpoint, bit2 midpoint, bit4 midpoint
        let zc = vec![12, 60, 108]; // spb*0+12, spb*2+12, spb*4+12
        let bits = decode_bits_synthetic_zc(&zc, spb);
        // Leading: (12/24 - 0.5).round() = 0 leading zeros
        // ZC at 12 -> first bit is 1
        // Between first(12) and second(60): interval=48, n_periods=2, zeros=1, then 1
        // Between second(60) and third(108): interval=48, n_periods=2, zeros=1, then 1
        assert_eq!(bits, vec![1, 0, 1, 0, 1]);
    }

    #[test]
    fn test_decode_bits_synthetic_zc_with_leading_gap() {
        let spb = 24.0;
        // First ZC at bit position 2 (spb*2+12=60)
        let zc = vec![60, 108]; // bit positions 2 and 4
        let bits = decode_bits_synthetic_zc(&zc, spb);
        // Leading: (60/24 - 0.5).round() = (2.5 - 0.5).round() = 2 zeros
        // Then ZC = 1
        // Between 60 and 108: interval=48=2*spb → n_periods=2 → zeros=1, then 1
        // Expected: [0,0,1,0,1]
        assert_eq!(bits, vec![0, 0, 1, 0, 1]);
    }

    #[test]
    fn test_decode_bits_synthetic_zc_consecutive_ones() {
        let spb = 24.0;
        // Consecutive 1s at bits 0, 1, 2
        let zc = vec![12, 36, 60]; // spb*0+12, spb*1+12, spb*2+12
        let bits = decode_bits_synthetic_zc(&zc, spb);
        // Leading: (12/24 - 0.5).round() = (0.5-0.5).round() = 0
        // ZC at 12 -> 1
        // interval 12→36 = 24 = spb → n_periods=1 → zeros=0 → 1
        // interval 36→60 = 24 = spb → n_periods=1 → zeros=0 → 1
        assert_eq!(bits, vec![1, 1, 1]);
    }

    #[test]
    fn test_decode_bits_synthetic_zc_single_zc() {
        let spb = 24.0;
        let zc = vec![12]; // one ZC at bit 0
        let bits = decode_bits_synthetic_zc(&zc, spb);
        assert_eq!(bits, vec![1]);
    }

    #[test]
    fn test_decode_bits_synthetic_zc_three_gap() {
        let spb = 24.0;
        // bit 0 has ZC, then bit 4 has ZC (3 zero bits between)
        let zc = vec![12, 108]; // positions: spb*0+12, spb*4+12
        let bits = decode_bits_synthetic_zc(&zc, spb);
        // Leading: 0
        // ZC at 12 -> 1
        // interval 12→108 = 96 = 4*spb → n_periods=4 → zeros=3 → 1
        assert_eq!(bits, vec![1, 0, 0, 0, 1]);
    }

    // ── decode_bits_real_zc ───────────────────────────────────────────

    #[test]
    fn test_decode_bits_real_zc_alternating() {
        let spb = 24.0;
        // Real ZCs: short (bit=1) then long (bit=0) intervals
        // ZC at bit 0, then bit 1 (short interval = spb), then bit 3 (long -> wait, that's 2*spb = is long)
        // Actually: short < 0.75*spb = 18. So short_threshold = 18
        // interval 0→1 = spb = 24 >= 18 → long → bit 0
        let zc = vec![12, 36]; // one short interval = spb
        let bits = decode_bits_real_zc(&zc, spb);
        // interval = 24 >= 18 (short_threshold) → long → push 0
        assert_eq!(bits, vec![0], "interval=spb should decode as long→0");
    }

    #[test]
    fn test_decode_bits_real_zc_short_interval() {
        let spb = 24.0;
        // Generate a "short" interval: make ZCs close together
        // ZC at 10, then ZC at 10+10 = 20 (interval=10 < 18)
        // Then a third ZC at 20+10 = 30 (interval=10 < 18)
        // Short interval followed by short → decoded as 1
        let zc = vec![10, 20, 30];
        let bits = decode_bits_real_zc(&zc, spb);
        assert_eq!(bits, vec![1]);
    }

    #[test]
    fn test_decode_bits_real_zc_long_short_long() {
        let spb = 24.0;
        // ZC at 10, ZC at 40 (interval=30 >= 18 = long → 0)
        // ZC at 40, ZC at 48 (interval=8 < 18 = short)
        // ZC at 48, ZC at 52 (interval=4 < 18 = short)
        // Pair of shorts at [40,48,52] → short+short = 1
        // Actually we need to trace through: i=0: interval(10→40)=30 >=18 → push 0, i=1
        // i=1: interval(40→48)=8 < 18 → check next: interval(48→52)=4 < 18 → push 1, i=3
        // Result: [0, 1]
        let zc = vec![10, 40, 48, 52];
        let bits = decode_bits_real_zc(&zc, spb);
        assert_eq!(bits, vec![0, 1]);
    }

    #[test]
    fn test_decode_bits_real_zc_trailing_short() {
        let spb = 24.0;
        // Short interval at the end with no following ZC → should be skipped safely
        let zc = vec![10, 40, 48]; // 10→40=long(0), 40→48=short but no next → skip
        let bits = decode_bits_real_zc(&zc, spb);
        assert_eq!(bits, vec![0]);
    }

    #[test]
    fn test_decode_bits_real_zc_single_interval() {
        let spb = 24.0;
        let zc = vec![10, 40]; // single long interval
        let bits = decode_bits_real_zc(&zc, spb);
        assert_eq!(bits, vec![0]);
    }

    // ── decode_bits_from_zero_crossings ───────────────────────────────

    #[test]
    fn test_decode_bits_from_zc_fewer_than_2_returns_empty() {
        let zc = vec![10];
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0, 120);
        assert!(bits.is_empty());
    }

    #[test]
    fn test_decode_bits_from_zc_empty_returns_empty() {
        let zc = vec![];
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0, 120);
        assert!(bits.is_empty());
    }

    #[test]
    fn test_decode_bits_from_zc_synthetic_path() {
        // Create ZCs with long intervals (few short intervals) → synthetic path
        // At 25fps, spb = 48000/(25*80) = 24
        // All intervals = spb → short_ratio = 0 → synthetic path
        let zc = vec![12, 36, 60, 84, 108];
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0, 120);
        // synthetic: leading=0, then each interval=spb → n_periods=1 → zeros=0 + 1
        // The final partial bit period (zc last = 108, total = 120 → 0.5 bit)
        // is closed with one trailing zero bit.
        assert_eq!(bits, vec![1, 1, 1, 1, 1, 0]);
    }

    #[test]
    fn test_decode_bits_from_zc_real_path() {
        // Create ZCs with many short intervals → real path
        // spb = 24, short_threshold = 18
        // Alternate short(10) and long(30) intervals: short_ratio ≈ 0.5 > 0.10
        let zc = vec![0, 10, 40, 50, 80, 90, 120, 130]; // short, long, short, long, short, long, short
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0, 120);
        // real path: intervals: 10(S), 30(L), 10(S), 30(L), 10(S), 30(L), 10(S)
        // S at [0-10]: next(10-40)=30(L) → no pair → skip
        // 10→40=30(L)→0
        // 40→50=10(S): next 50→80=30(L) → no pair → skip
        // 50→80=30(L)→0
        // 80→90=10(S): next 90→120=30(L) → no pair → skip
        // 90→120=30(L)→0
        // 120→130=10(S): no next → skip
        // But wait, let me trace more carefully:
        //
        // i=0: interval(0→10)=10 < 18 : short
        //   i+2 < len? 0+2=2 < 8: yes
        //   next = interval(10→40)=30 >= 18: no pair → i+=1 → i=1
        // i=1: interval(10→40)=30 >= 18: push 0 → i+=1 → i=2
        // i=2: interval(40→50)=10 < 18 : short
        //   i+2 < len? 2+2=4 < 8: yes
        //   next = interval(50→80)=30 >= 18: no pair → i+=1 → i=3
        // i=3: interval(50→80)=30 >= 18: push 0 → i+=1 → i=4
        // i=4: interval(80→90)=10 < 18 : short
        //   i+2 < len? 4+2=6 < 8: yes
        //   next = interval(90→120)=30 >= 18: no pair → i+=1 → i=5
        // i=5: interval(90→120)=30 >= 18: push 0 → i+=1 → i=6
        // i=6: interval(120→130)=10 < 18 : short
        //   i+2 < len? 6+2=8 < 8: NO → break
        //
        // Result: [0,0,0]
        assert_eq!(bits, vec![0, 0, 0]);
    }

    // ── try_decode_via_zc_intervals ───────────────────────────────────

    #[test]
    fn test_try_decode_zc_intervals_few_zcs() {
        let zc = vec![0, 10, 20];
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false, 200);
        assert!(result.is_none());
    }

    #[test]
    fn test_try_decode_zc_intervals_too_few_bits() {
        // ZCs that produce fewer than 80 bits
        // Only generate a few ZCs
        let zc = vec![12, 36, 60, 84, 108, 132, 156]; // 7 ZCs → synthetic produces 7 bits
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false, 200);
        assert!(result.is_none());
    }

    #[test]
    fn test_try_decode_zc_intervals_valid_synthetic() {
        // Build ZCs matching a valid LTC frame (use get_ltc_bits to know the pattern)
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        let spb = 24.0; // 48000/(25*80)
        let zc = bits_to_zc(&bits, spb);
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false, 200);
        assert!(result.is_some(), "should decode a valid frame");
        if let Some(r) = result {
            assert!(r.valid_frames >= 1, "should find at least 1 valid frame, got {}", r.valid_frames);
        }
    }

    #[test]
    fn test_try_decode_zc_intervals_all_zero_bits() {
        // No '1' bits means no ZCs at all
        let zc = vec![];
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false, 200);
        assert!(result.is_none());
    }

    // ── extract_bits_adaptive ─────────────────────────────────────────

    #[test]
    fn test_extract_bits_adaptive_basic() {
        let spb = 8.0;
        let mut signal = Vec::new();
        let mut level = 0.5;
        for &bit in &[1u8, 0, 1, 0] {
            let (chunk, l) = synthesize_bit(spb as usize, bit, level);
            signal.extend(chunk);
            level = l;
        }
        // ZCs: bit0 at mid=4, bit1=no ZC, bit2 at mid=16+4=20, bit3=no ZC
        let zc = vec![4, 20];
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.01, &zc, None);
        assert_eq!(bits, vec![1, 0, 1, 0]);
    }

    #[test]
    fn test_extract_bits_adaptive_phase_offset() {
        let spb = 8.0;
        let mut signal = vec![0.0f32; 3]; // phase offset
        let mut level = 0.5;
        for &bit in &[1u8, 0, 1] {
            let (chunk, l) = synthesize_bit(spb as usize, bit, level);
            signal.extend(chunk);
            level = l;
        }
        let zc = vec![7, 23]; // phase=3 → bit0 mid at 3+4=7, bit2 mid at 3+16+4=23
        let bits = extract_bits_adaptive(&signal, spb, 3, 0.01, &zc, None);
        assert_eq!(bits, vec![1, 0, 1]);
    }

    #[test]
    fn test_extract_bits_adaptive_snap_to_zc() {
        let spb = 8.0;
        // Create signal with slight clock drift in the ZCs
        let mut signal = Vec::new();
        let mut level = 0.5;
        for &bit in &[1u8, 1, 0] {
            let (chunk, l) = synthesize_bit(spb as usize, bit, level);
            signal.extend(chunk);
            level = l;
        }
        // ZC positions slightly offset from ideal (simulating drift)
        let ideal_first = 4;
        let ideal_second = 12;
        let drift_zc = vec![ideal_first + 1, ideal_second - 1]; // 5 and 11
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.01, &drift_zc, None);
        // First bit: should snap to ZC at 5 (within snap_radius=2 from ideal 4)
        // Second bit: next_boundary = 8, snap_radius=2 → target_lo=6, target_hi=10
        //   ZC at 11 is NOT in [6,10], so no snap → pos = 8
        // Third bit: at pos=8+8=16, p25=16+2=18...
        assert_eq!(bits, vec![1, 1, 0]);
    }

    #[test]
    fn test_extract_bits_adaptive_below_threshold() {
        let spb = 8.0;
        let signal = vec![0.0f32; (spb as usize) * 4];
        let zc = vec![];
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.1, &zc, None);
        assert_eq!(bits, vec![0, 0, 0, 0]);
    }

    #[test]
    fn test_extract_bits_adaptive_no_zc_snap() {
        let spb = 8.0;
        let mut signal = Vec::new();
        let mut level = 0.5;
        for &bit in &[1u8, 1] {
            let (chunk, l) = synthesize_bit(spb as usize, bit, level);
            signal.extend(chunk);
            level = l;
        }
        // No ZCs in the provided list → should use regular boundary
        let zc = vec![];
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.01, &zc, None);
        assert_eq!(bits, vec![1, 1]);
    }

    // ── build_result ──────────────────────────────────────────────────

    /// Test substrate for the ladder functions (real values filled per test).
    fn mk_ctx<'a>(samples: &'a [f32], zc: &'a [usize]) -> DecodeCtx<'a> {
        DecodeCtx {
            samples,
            zc,
            sample_rate: 48000,
            threshold: 0.01,
            fps: 25.0,
            drop_frame: false,
            cancel: None,
        }
    }

    #[test]
    fn test_build_result_with_valid_result() {
        let r = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 50,
            grid_valid: 50,
            total_possible: 60,
            timecodes: vec![
                FrameTimecode {
                    frame_index: 0,
                    timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
                    timecode_secs: 0.0,
                },
            ],
            details_entry: "test details".to_string(),
            spb: 24.0,
            phase: 12,
            adaptive: false,
            frame_starts: vec![0],
        };
        let zc = vec![12, 36, 60];
        let result = build_result(&mk_ctx(&[], &zc), Some(r), 2, 2.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success));
        assert!((result.avg_confidence - 50.0 / 60.0).abs() < 0.001);
        assert_eq!(result.valid_frames, 50);
        assert_eq!(result.total_possible_frames, 60);
        assert!(!result.timecodes.is_empty());
    }

    #[test]
    fn test_build_result_with_none() {
        let zc = vec![];
        let result = build_result(&mk_ctx(&[], &zc), None, 2, 0.5, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::NoSyncWord));
        assert_eq!(result.valid_frames, 0);
        assert!(result.timecodes.is_empty());
    }

    #[test]
    fn test_build_result_confidence_thresholds() {
        let low_conf = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 10,
            grid_valid: 10,
            total_possible: 100,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            adaptive: false,
            frame_starts: vec![],
        };
        let zc = vec![];
        let result = build_result(&mk_ctx(&[], &zc), Some(low_conf), 2, 1.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::NoSyncWord));

        let med_conf = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 40,
            grid_valid: 40,
            total_possible: 100,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            adaptive: false,
            frame_starts: vec![],
        };
        let result = build_result(&mk_ctx(&[], &zc), Some(med_conf), 2, 1.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::LowConfidence));

        let high_conf = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 90,
            grid_valid: 90,
            total_possible: 100,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            adaptive: false,
            frame_starts: vec![],
        };
        let result = build_result(&mk_ctx(&[], &zc), Some(high_conf), 2, 1.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success));
    }

    #[test]
    fn test_build_result_zero_possible() {
        let r = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 0,
            grid_valid: 0,
            total_possible: 0,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            adaptive: false,
            frame_starts: vec![],
        };
        let zc = vec![];
        let result = build_result(&mk_ctx(&[], &zc), Some(r), 2, 1.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::NoSyncWord));
        assert_eq!(result.avg_confidence, 0.0);
    }

    // ── decode_full_file ──────────────────────────────────────────────

    #[test]
    fn test_decode_full_file_clean_signal() {
        let spb = 24.0;
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        // Generate full signal for 3 identical frames
        let mut signal = Vec::new();
        let mut level = 0.5;
        for _ in 0..3 {
            for &bit in &bits {
                let (chunk, l) = synthesize_bit(spb as usize, bit, level);
                signal.extend(chunk);
                level = l;
            }
        }

        let params = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 3,
            grid_valid: 3,
            total_possible: 3,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb,
            phase: 0, // phase=0 = start of bit boundary
            adaptive: false,
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.001, 48000, 0, &[], None);
        assert_eq!(result.valid_frames, 3);
        assert_eq!(result.total_possible, 3);
        assert_eq!(result.timecodes.len(), 3);
        for ftc in &result.timecodes {
            assert_eq!(ftc.timecode, tc);
        }
    }

    #[test]
    fn test_decode_full_file_silent_signal() {
        let spb = 24.0;
        let signal = vec![0.0f32; 4800]; // silence
        let params = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 0,
            grid_valid: 0,
            total_possible: 0,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb,
            phase: 0,
            adaptive: false,
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.5, 48000, 0, &[], None);
        // With high threshold, signal is below threshold → extract_bits returns zeros
        // No sync word in zeros → valid_frames = 0
        assert_eq!(result.valid_frames, 0);
    }

    // ── decode_full_file adaptive tests ───────────────────────────────

    #[test]
    fn test_decode_full_file_no_drift_adaptive_regression() {
        let spb = 24.0;
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        let mut signal = Vec::new();
        let mut level = 0.5;
        for _ in 0..3 {
            for &bit in &bits {
                let (chunk, l) = synthesize_bit(spb as usize, bit, level);
                signal.extend(chunk);
                level = l;
            }
        }
        let zc = find_zero_crossings(&signal, 0.001);

        let params = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 3,
            grid_valid: 3,
            total_possible: 3,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb,
            phase: 0,
            adaptive: false,
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.001, 48000, 0, &zc, None);
        assert_eq!(result.valid_frames, 3);
        assert_eq!(result.total_possible, 3);
        assert_eq!(result.timecodes.len(), 3);
        for ftc in &result.timecodes {
            assert_eq!(ftc.timecode, tc);
        }
    }

    #[test]
    fn test_decode_full_file_adaptive_with_spb_mismatch() {
        let spb_true = 24.0;
        let spb_mismatch = 24.1;
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        let mut signal = Vec::new();
        let mut level = 0.5;
        for _ in 0..3 {
            for &bit in &bits {
                let (chunk, l) = synthesize_bit(spb_true as usize, bit, level);
                signal.extend(chunk);
                level = l;
            }
        }
        let zc = find_zero_crossings(&signal, 0.001);

        let params = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 0,
            grid_valid: 0,
            total_possible: 3,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb: spb_mismatch,
            phase: 0,
            adaptive: false,
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.001, 48000, 0, &zc, None);
        assert!(result.valid_frames >= 2,
            "expected >=2 valid frames with spb mismatch (true={}, used={}), got {}",
            spb_true, spb_mismatch, result.valid_frames);
    }

    // ── extract_bits_adaptive basic correctness ─────────────────────

    #[test]
    fn test_extract_bits_adaptive_consistent_with_nominal() {
        let spb = 24.0;
        let bit_pattern: Vec<u8> = (0..10).map(|i| if i % 2 == 0 { 1 } else { 0 }).collect();
        let mut signal = Vec::new();
        let mut level = 0.5;
        for &bit in &bit_pattern {
            let (chunk, l) = synthesize_bit(spb as usize, bit, level);
            signal.extend(chunk);
            level = l;
        }
        let zc = find_zero_crossings(&signal, 0.001);

        let bits_adaptive = extract_bits_adaptive(&signal, spb, 0, 0.01, &zc, None);
        let bits_nominal = extract_bits(&signal, spb, 0, 0.01, None);

        assert_eq!(bits_adaptive.len(), bit_pattern.len(),
            "adaptive: expected {} bits, got {}", bit_pattern.len(), bits_adaptive.len());
        assert_eq!(bits_nominal.len(), bit_pattern.len(),
            "nominal: expected {} bits, got {}", bit_pattern.len(), bits_nominal.len());
        for (i, (&got, &expected)) in bits_adaptive.iter().zip(bit_pattern.iter()).enumerate() {
            assert_eq!(got, expected,
                "adaptive bit {} mismatch: got {}, expected {}", i, got, expected);
        }
        // On clean synthetic signal, both extractors should produce identical results
        assert_eq!(bits_adaptive, bits_nominal,
            "adaptive and nominal should produce identical bits on clean signal");
    }

    // ── extract_bits_adaptive accumulating drift (SMPTE encoder) ─────

    #[test]
    fn test_extract_bits_adaptive_accumulating_drift() {
        let nominal_spb = 24.0;
        let actual_spb: usize = 25;
        let num_bits = 200;
        let bit_pattern: Vec<u8> = (0..num_bits).map(|i| if i % 2 == 0 { 1 } else { 0 }).collect();

        let mut signal = Vec::new();
        let mut last_level = 0.5f32;
        for &bit in &bit_pattern {
            let (chunk, l) = synthesize_bit_smpte(actual_spb, bit, last_level);
            signal.extend(chunk);
            last_level = l;
        }

        let zc = find_zero_crossings(&signal, 0.001);
        assert!(zc.len() > num_bits,
            "SMPTE signal should have at least {} ZCs, got {}",
            num_bits, zc.len());

        let bits_adaptive = extract_bits_adaptive(&signal, nominal_spb, 0, 0.01, &zc, None);
        assert_eq!(bits_adaptive.len(), num_bits,
            "adaptive: expected {} bits, got {}", num_bits, bits_adaptive.len());
        for (i, (&got, &expected)) in bits_adaptive.iter().zip(bit_pattern.iter()).enumerate() {
            assert_eq!(got, expected,
                "adaptive bit {} mismatch: got {}, expected {}", i, got, expected);
        }

        let bits_nominal = extract_bits(&signal, nominal_spb, 0, 0.01, None);
        assert_ne!(bits_nominal, bit_pattern,
            "non-adaptive extraction should fail with SPB mismatch (nominal={}, actual={})",
            nominal_spb, actual_spb);
    }

    // ── Helper: synthesize LTC signal with clock drift ────────────────

    /// Generate mono f32 LTC where each bit is slightly longer than nominal,
    /// simulating a sample-rate mismatch between recording and source.
    fn synthesize_ltc_signal_with_drift(
        timecodes: &[Timecode],
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        volume: f32,
        drift_ppm: f64,
    ) -> Vec<f32> {
        let nominal_spb = sample_rate as f64 / (fps * 80.0);
        let drift_spb = nominal_spb * (1.0 + drift_ppm / 1_000_000.0);
        let mut signal = Vec::new();
        let mut last_level = 1.0f32;
        let mut bit_acc = 0.0f64;

        for tc in timecodes {
            let bits = crate::get_ltc_bits(tc, drop_frame);
            for &bit in bits.iter() {
                bit_acc += drift_spb;
                let samples_this_bit = bit_acc.floor() as usize;
                bit_acc -= samples_this_bit as f64;
                let (chunk, l) = synthesize_bit(samples_this_bit.max(1), bit, last_level * volume.signum());
                signal.extend(chunk.iter().map(|s| s * volume));
                last_level = l;
            }
        }
        signal
    }

    // ── Helper: synthesize LTC signal for decode_ltc_samples ──────────

    /// Generate a mono f32 LTC signal from a list of timecodes.
    fn synthesize_ltc_signal(
        timecodes: &[Timecode],
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        volume: f32,
    ) -> Vec<f32> {
        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;
        let mut signal = Vec::with_capacity(samples_per_frame * timecodes.len());
        let mut last_level = 1.0f32;

        for tc in timecodes {
            let bits = crate::get_ltc_bits(tc, drop_frame);
            for &bit in &bits {
                let (chunk, l) = synthesize_bit(samples_per_bit as usize, bit, last_level * volume.signum());
                signal.extend(chunk.iter().map(|s| s * volume));
                last_level = l;
            }
        }
        signal
    }

    // ── Noise injection helpers (deterministic LCG) ───────────────────

    /// LCG in [0, 1). Deterministic, reproduces same sequence for same seed.
    fn lcg_next(state: &mut u64) -> f32 {
        *state = state.wrapping_mul(1103515245).wrapping_add(12345);
        ((*state >> 16) & 0x7FFF) as f32 / 32768.0
    }

    /// Box-Muller Gaussian sample using LCG as source of uniform randomness.
    fn gaussian_lcg(state: &mut u64) -> f32 {
        let u1 = lcg_next(state);
        let u2 = lcg_next(state);
        let r = (-2.0 * u1.ln()).sqrt();
        r * (2.0 * std::f32::consts::PI * u2).cos()
    }

    fn add_gaussian_noise(signal: &[f32], std_dev: f32, seed: u64) -> Vec<f32> {
        let mut state = seed;
        signal.iter().map(|&s| s + gaussian_lcg(&mut state) * std_dev).collect()
    }

    fn add_impulse_noise(signal: &[f32], probability: f32, amplitude: f32, seed: u64) -> Vec<f32> {
        let mut state = seed;
        signal.iter().map(|&s| {
            if lcg_next(&mut state) < probability { amplitude } else { s }
        }).collect()
    }

    fn add_dc_offset(signal: &[f32], offset: f32) -> Vec<f32> {
        signal.iter().map(|&s| s + offset).collect()
    }

    fn add_hum(signal: &[f32], sample_rate: u32, amplitude: f32, freq: f32) -> Vec<f32> {
        let phase_inc = 2.0 * std::f32::consts::PI * freq / sample_rate as f32;
        signal.iter().enumerate().map(|(i, &s)| {
            s + amplitude * (phase_inc * i as f32).sin()
        }).collect()
    }

    fn apply_fading(signal: &[f32], sample_rate: u32, mod_freq: f32, depth: f32) -> Vec<f32> {
        let phase_inc = 2.0 * std::f32::consts::PI * mod_freq / sample_rate as f32;
        signal.iter().enumerate().map(|(i, &s)| {
            let envelope = 1.0 - depth * 0.5 * (1.0 + (phase_inc * i as f32).sin());
            s * envelope
        }).collect()
    }

    fn add_dropouts(signal: &[f32], sample_rate: u32, dropout_secs: f32, num_dropouts: usize, seed: u64) -> Vec<f32> {
        let mut state = seed;
        let dropout_samples = (dropout_secs * sample_rate as f32) as usize;
        let mut result = signal.to_vec();
        let max_start = result.len().saturating_sub(dropout_samples);
        for _ in 0..num_dropouts {
            if max_start == 0 { break; }
            let start = ((lcg_next(&mut state) as f64) * max_start as f64) as usize;
            let end = (start + dropout_samples).min(result.len());
            result[start..end].fill(0.0);
        }
        result
    }

    /// Simple one-pole low-pass filter to simulate bandwidth-limited wireless link.
    fn apply_lowpass(signal: &[f32], factor: f32) -> Vec<f32> {
        let mut result = Vec::with_capacity(signal.len());
        let mut prev = 0.0f32;
        for &s in signal {
            let filtered = prev + factor * (s - prev);
            result.push(filtered);
            prev = filtered;
        }
        result
    }

    fn make_timecodes(count: u32) -> Vec<Timecode> {
        (0..count).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i,
        }).collect()
    }

    /// Assert that decode succeeded with at least `min_valid` valid frames.
    fn assert_ltc_ok(result: &LtcDetectionResult, min_valid: u32) {
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "unexpected Error: {:?} (valid={}/{})", result.status, result.valid_frames, result.total_possible_frames);
        assert!(result.valid_frames >= min_valid,
            "expected >= {} valid frames, got {} (status={:?})",
            min_valid, result.valid_frames, result.status);
    }

    /// Properly wrapped sequential timecodes (00:00:00:00, 00:00:00:01, …).
    /// `make_timecodes` yields invalid values for frames >= fps, which breaks
    /// timecode-value comparison.
    fn sequential_timecodes(count: u32) -> Vec<Timecode> {
        (0..count)
            .map(|i| Timecode {
                hours: 0,
                minutes: 0,
                seconds: (i / 25),
                frames: (i % 25),
            })
            .collect()
    }

    fn sequential_signal() -> Vec<f32> {
        let tcs = sequential_timecodes(50);
        synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5)
    }

    /// The strict robustness bar used by the *_max_decodable tests:
    /// Success, at least `min_valid` frames, and every decoded timecode is
    /// an in-order subsequence of `expected` (no garbage values). Values
    /// were measured with `sweep_robustness_limits_manual`.
    fn assert_ltc_fully_decoded(result: &LtcDetectionResult, min_valid: u32, expected: &[Timecode]) {
        assert_ltc_ok(result, min_valid);
        let mut exp_idx = 0usize;
        for ft in &result.timecodes {
            // A frame re-decoded at a corruption boundary may repeat the
            // just-matched timecode; that is a reporting artifact, not a
            // wrong value, so it does not break the check.
            if exp_idx > 0 && expected[exp_idx - 1] == ft.timecode {
                continue;
            }
            while exp_idx < expected.len() && expected[exp_idx] != ft.timecode {
                exp_idx += 1;
            }
            assert!(
                exp_idx < expected.len(),
                "decoded out-of-sequence timecode {:?} (status={:?}, valid={}/{})",
                ft.timecode, result.status, result.valid_frames, result.total_possible_frames
            );
            exp_idx += 1;
        }
    }

    // ── decode_ltc_samples ────────────────────────────────────────────

    #[test]
    fn test_decode_ltc_samples_empty_buffer() {
        let samples = vec![];
        let result = decode_ltc_samples(&samples, 48000, 2, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_silent_buffer() {
        let samples = vec![0.0f32; 48000 * 2]; // 1 sec silence
        let result = decode_ltc_samples(&samples, 48000, 2, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_25fps_basic() {
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for 25fps LTC, got {:?} (valid={})", result.status, result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_samples_24fps() {
        let tcs: Vec<Timecode> = (0..24).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 24.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 24.0, false, std::time::Instant::now(), None).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_30fps() {
        let tcs: Vec<Timecode> = (0..30).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 30.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 30.0, false, std::time::Instant::now(), None).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_44100hz() {
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 44100, 0.5);
        let result = decode_ltc_samples(&signal, 44100, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_different_start_timecode() {
        let tcs = vec![Timecode { hours: 10, minutes: 15, seconds: 30, frames: 12 }];
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
    }

    #[test]
    fn test_decode_ltc_samples_too_short_signal() {
        // Only a few samples — not enough for any zero crossings
        let samples = vec![0.5f32, -0.5, 0.5, -0.5];
        let result = decode_ltc_samples(&samples, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_wrong_fps() {
        // Decoding 25 fps content as 30 fps: the ZC-adaptive decoder locks
        // onto the real bit rate and returns "valid" frames whose values are
        // garbage. The contract is the quality report: an fps mismatch must
        // never score as a usable decode.
        let tcs: Vec<Timecode> = (0..50).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 30.0, false, std::time::Instant::now(), None).unwrap();
        let q = result.quality.as_ref()
            .expect("quality report must be present on a decoded signal");
        assert!(q.score < 0.5, "fps mismatch must score badly, got {}", q.score);
    }

    // ───ƒ─ decode_ltc_samples drop-frame ──────────────────────────────────

    #[test]
    fn test_decode_ltc_samples_drop_frame() {
        let tcs: Vec<Timecode> = (0..30).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 29.97, true, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 29.97, true, std::time::Instant::now(), None).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for 29.97 DF, got {:?}", result.status);
    }

    // ── Clock drift end-to-end test ─────────────────────────────────────

    #[test]
    fn test_decode_ltc_samples_clock_drift_long() {
        let tcs: Vec<Timecode> = (0..1500)
            .map(|i| Timecode {
                hours: (i / (25 * 60)) as u32,
                minutes: ((i / 25) % 60) as u32,
                seconds: (i % 25) as u32,
                frames: 0,
            })
            .collect();
        let signal = synthesize_ltc_signal_with_drift(&tcs, 25.0, false, 48000, 0.5, 100.0);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for 60s LTC with 100ppm drift, got {:?} (valid={}/{})",
            result.status, result.valid_frames, result.total_possible_frames);
        let total_expected = tcs.len() as u32;
        assert!(result.valid_frames as f32 / total_expected as f32 >= 0.70,
            "expected >=70% valid frames with 100ppm drift, got {}/{} ({:.1}%)",
            result.valid_frames, total_expected,
            result.valid_frames as f32 / total_expected as f32 * 100.0);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Noise robustness tests
    // ═══════════════════════════════════════════════════════════════════

    // ── Helper: generate clean base signal ────────────────────────────────

    fn base_signal() -> Vec<f32> {
        let tcs = make_timecodes(50);
        synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5)
    }

    fn base_decode(signal: &[f32]) -> LtcDetectionResult {
        decode_ltc_samples(signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap()
    }

    // ── 1. Additive Gaussian noise ──────────────────────────────────────

    #[test]
    fn test_noise_gaussian_20db() {
        let signal = add_gaussian_noise(&base_signal(), 0.05, 42);
        let result = base_decode(&signal);
        // 20dB SNR: should decode cleanly
        assert_ltc_ok(&result, 20);
    }

    #[test]
    fn test_noise_gaussian_15db() {
        let signal = add_gaussian_noise(&base_signal(), 0.09, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 15);
    }

    #[test]
    fn test_noise_gaussian_10db() {
        let signal = add_gaussian_noise(&base_signal(), 0.16, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 10);
    }

    #[test]
    fn test_noise_gaussian_6db() {
        let signal = add_gaussian_noise(&base_signal(), 0.25, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 3);
    }

    #[test]
    fn test_noise_gaussian_3db() {
        let signal = add_gaussian_noise(&base_signal(), 0.35, 42);
        let result = base_decode(&signal);
        // At 3dB SNR the decoder may struggle — verify it doesn't crash
        // and that at least some frames are detected
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "3dB SNR should not error, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    // ── 2. Impulse / click noise ────────────────────────────────────────

    #[test]
    fn test_noise_impulse_light() {
        let signal = add_impulse_noise(&base_signal(), 0.001, 1.0, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 20);
    }

    #[test]
    fn test_noise_impulse_medium() {
        let signal = add_impulse_noise(&base_signal(), 0.005, 1.0, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 10);
    }

    #[test]
    fn test_noise_impulse_heavy() {
        let signal = add_impulse_noise(&base_signal(), 0.05, 1.0, 42);
        let result = base_decode(&signal);
        // 5% impulse rate is extreme — just don't crash, may produce no frames
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "5% impulse noise should not error, got {:?}",
            result.status);
    }

    #[test]
    fn test_decode_impulse_noise_extreme_does_not_panic() {
        // Regression: a false sync-word match late in a heavily corrupted
        // signal made backfill_leading_frames compute a sample offset past
        // the end of the buffer and slice out of range. Decoding garbage
        // must degrade gracefully, never panic. The value-integrity pass
        // (WP-DR) may drop garbage frames from `timecodes`, so the length
        // can fall below the sync-matched `valid_frames`; the result must
        // stay well-formed either way.
        let signal = add_impulse_noise(&base_signal(), 0.4, 1.0, 42);
        let result = base_decode(&signal);
        // No frame-count floor — heavily corrupted input may decode
        // nothing. The contract is: no panic, and a well-formed result.
        assert!(result.timecodes.len() <= result.valid_frames as usize,
            "timecodes ({}) cannot exceed sync-matched valid_frames ({})",
            result.timecodes.len(), result.valid_frames);
        for (i, ftc) in result.timecodes.iter().enumerate() {
            assert_eq!(ftc.frame_index, i as u32, "frame indices stay contiguous");
        }
    }

    // ── 3. DC offset ─────────────────────────────────────────────────────

    #[test]
    fn test_noise_dc_offset_small() {
        let signal = add_dc_offset(&base_signal(), 0.01);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 20);
    }

    #[test]
    fn test_noise_dc_offset_medium() {
        let signal = add_dc_offset(&base_signal(), 0.05);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 10);
    }

    #[test]
    fn test_noise_dc_offset_large() {
        let signal = add_dc_offset(&base_signal(), 0.1);
        let result = base_decode(&signal);
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "DC offset 0.1 should not error, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    // ── 4. Hum interference (50/60 Hz) ──────────────────────────────────

    #[test]
    fn test_noise_hum_50hz_low() {
        let signal = add_hum(&base_signal(), 48000, 0.05, 50.0);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 20);
    }

    #[test]
    fn test_noise_hum_50hz_moderate() {
        let signal = add_hum(&base_signal(), 48000, 0.15, 50.0);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 10);
    }

    #[test]
    fn test_noise_hum_60hz_moderate() {
        let signal = add_hum(&base_signal(), 48000, 0.15, 60.0);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 10);
    }

    // ── 5. Amplitude modulation / fading ────────────────────────────────

    #[test]
    fn test_noise_fading_slow() {
        let signal = apply_fading(&base_signal(), 48000, 2.0, 0.5);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 20);
    }

    #[test]
    fn test_noise_fading_deep() {
        let signal = apply_fading(&base_signal(), 48000, 1.0, 0.9);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 10);
    }

    // ── 6. Dropouts (simulated wireless signal loss) ────────────────────

    #[test]
    fn test_noise_dropouts_short() {
        // Two 50ms gaps in a 2s signal — decoder should resync after each
        let signal = add_dropouts(&base_signal(), 48000, 0.05, 2, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 30);
    }

    #[test]
    fn test_noise_dropouts_medium() {
        // One 200ms gap — 10% of signal lost
        let signal = add_dropouts(&base_signal(), 48000, 0.2, 1, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 30);
    }

    #[test]
    fn test_noise_dropouts_long() {
        // One 500ms gap — 25% of signal lost, decoder should resync
        let signal = add_dropouts(&base_signal(), 48000, 0.5, 1, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 15);
    }

    // ── 7. Combined stress ──────────────────────────────────────────────

    #[test]
    fn test_noise_combined_light() {
        // Gaussian (+20dB) + DC offset (0.01) + hum (50Hz, low) + light fading
        let signal = base_signal();
        let signal = add_gaussian_noise(&signal, 0.05, 42);
        let signal = add_dc_offset(&signal, 0.01);
        let signal = add_hum(&signal, 48000, 0.05, 50.0);
        let signal = apply_fading(&signal, 48000, 2.0, 0.3);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 15);
    }

    #[test]
    fn test_noise_combined_moderate() {
        // Gaussian (+15dB) + DC offset (0.03) + hum (50Hz, moderate) + fading (50%)
        let signal = base_signal();
        let signal = add_gaussian_noise(&signal, 0.09, 42);
        let signal = add_dc_offset(&signal, 0.03);
        let signal = add_hum(&signal, 48000, 0.1, 50.0);
        let signal = apply_fading(&signal, 48000, 2.0, 0.5);
        let result = base_decode(&signal);
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "moderate combined noise should not error, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    #[test]
    fn test_noise_combined_heavy() {
        // Gaussian (+10dB) + DC offset (0.05) + hum (50Hz, moderate) + heavy fading
        let signal = base_signal();
        let signal = add_gaussian_noise(&signal, 0.16, 42);
        let signal = add_dc_offset(&signal, 0.05);
        let signal = add_hum(&signal, 48000, 0.15, 50.0);
        let signal = apply_fading(&signal, 48000, 1.5, 0.7);
        let result = base_decode(&signal);
        // Just don't crash — some frames may survive
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "heavy combined noise should not error, got {:?}",
            result.status);
    }

    // ── 8. Frequency roll-off (bandwidth-limited wireless link) ────────

    #[test]
    fn test_noise_lowpass_mild() {
        let signal = apply_lowpass(&base_signal(), 0.3);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 20);
    }

    #[test]
    fn test_noise_lowpass_moderate() {
        let signal = apply_lowpass(&base_signal(), 0.1);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 10);
    }

    #[test]
    fn test_noise_lowpass_heavy() {
        let signal = apply_lowpass(&base_signal(), 0.03);
        let result = base_decode(&signal);
        // Heavy low-pass erases bi-phase transitions — may not decode
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "heavy lowpass should not error, got {:?}",
            result.status);
    }

    // ── 9. Measured maximum-tolerable corruption ("*_max_decodable") ────
    //
    // Values below were measured by `sweep_robustness_limits_manual`
    // (bisecting each corruption parameter until the strict bar breaks:
    // Success + all decoded timecode values correct). Pins carry ~15–20 %
    // margin below the worst-seed cliff. Run the sweep again whenever the
    // decoder's detection constants change.

    #[test]
    fn test_noise_gaussian_max_decodable() {
        // Post-WP-DR worst-seed strict cliff: std 0.314 (SNR ≈ +4 dB; was
        // 0.180 pre-integrity-pass). Beyond it the decoder still reports
        // Success — but with wrong timecode values.
        let tcs = sequential_timecodes(50);
        let signal = add_gaussian_noise(&sequential_signal(), 0.25, 42);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_noise_impulse_max_decodable() {
        // Post-WP-DR worst-seed strict cliff: p = 0.056 (was 0.0041 — the
        // median pre-filter removes click runs the 2-point bit sampler
        // used to trip over); the loose bar holds to ≈ 0.27.
        let tcs = sequential_timecodes(50);
        let signal = add_impulse_noise(&sequential_signal(), 0.045, 1.0, 42);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_noise_dc_offset_max_decodable() {
        // Pre-WP-DR strict cliff: 0.492 ≈ the signal amplitude. The DC
        // blocker made the dimension frontend-immune — the strict bar now
        // holds to the sweep harness cap of 8.0 (12× the old cliff); the
        // pin sits at ~6 with margin below the cap.
        let tcs = sequential_timecodes(50);
        let signal = add_dc_offset(&sequential_signal(), 6.0);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_noise_dc_offset_beyond_cliff_not_success() {
        // False-positive ceiling: pure DC with *no signal* must not decode
        // (post-conditioning there is nothing left to lock onto). Pre-WP-DR
        // this test pinned the old DC-offset decode cliff on a *signalled*
        // input — with the DR3 DC blocker that limitation is gone by
        // design (offsets many times the signal amplitude decode
        // correctly now), so the garbage-input contract is pinned instead.
        let mut signal = vec![0.0f32; 96000];
        for s in signal.iter_mut() {
            *s = 0.6;
        }
        let result = base_decode(&signal);
        assert!(!matches!(result.status, LtcDecodeStatus::Success),
            "pure DC must not decode, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    #[test]
    fn test_noise_hum_50hz_max_decodable() {
        // Pre-WP-DR strict cliff: hum amplitude ≈ 0.50 = signal amplitude.
        // The mains notch made the dimension frontend-immune — the strict
        // bar now holds to the sweep harness cap of 8.0; pinned at ~6.
        let tcs = sequential_timecodes(50);
        let signal = add_hum(&sequential_signal(), 48000, 6.0, 50.0);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 35, &tcs);
    }

    #[test]
    fn test_noise_hum_60hz_max_decodable() {
        // Same post-notch immunity as the 50 Hz dimension.
        let tcs = sequential_timecodes(50);
        let signal = add_hum(&sequential_signal(), 48000, 6.0, 60.0);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_noise_fading_max_decodable() {
        // Strict cliff at depth 0.909 (envelope dips to ~5 % amplitude);
        // the loose bar holds even at depth 1.0 (momentary full silence).
        let tcs = sequential_timecodes(50);
        let signal = apply_fading(&sequential_signal(), 48000, 1.0, 0.8);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_noise_dropouts_max_loss() {
        // Loose-bar cliff (plain Success): a single zero-filled span of
        // ~0.60 s (30 % of this 2 s signal) across all tested placements;
        // pinned at 0.50 s. The strict bar has no meaningful single-number
        // cliff for dropouts — placement is a lottery: even a 6 ms gap over
        // one frame's BCD digits can flip that frame's value (:04 read as
        // :00) while all neighbours stay correct.
        let signal = add_dropouts(&sequential_signal(), 48000, 0.50, 1, 42);
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 25);
    }

    #[test]
    fn test_noise_lowpass_max_decodable() {
        // Measured cliff: below factor ≈ 0.04 the one-pole low-pass erases
        // the bi-phase transitions (0.03 → LowConfidence, 0.02 → no sync
        // word); strict-bar behaviour is non-monotonic just above it
        // (bisection measures ≈ 0.061). 0.08 pinned; values must stay
        // correct.
        let tcs = sequential_timecodes(50);
        let signal = apply_lowpass(&sequential_signal(), 0.08);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_noise_min_volume_max_attenuation() {
        // Post-WP-DR cliff: volume 0.0041 (was 0.0104, set by the 0.005
        // absolute zero-crossing threshold floor). The peak-relative floor
        // follows the signal down; pinned with ~15 % margin.
        let tcs = sequential_timecodes(50);
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.0035);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_decode_ltc_samples_clock_drift_max() {
        // Measured limit (24 s / 600 frames): the decoder locks up to
        // ≈ +11 400 ppm / −10 200 ppm speed error (≈ ±1 %) — far beyond the
        // ±0.4 % spb search lattice, carried by adaptive zero-crossing
        // snapping. Pinned at 8 000 ppm with ~20 % margin. The 100 ppm test
        // above is the comfortable anchor.
        let tcs: Vec<Timecode> = (0..600)
            .map(|i| Timecode {
                hours: (i / (25 * 60)) as u32,
                minutes: ((i / 25) % 60) as u32,
                seconds: (i % 25) as u32,
                frames: 0,
            })
            .collect();
        let signal = synthesize_ltc_signal_with_drift(&tcs, 25.0, false, 48000, 0.5, 8000.0);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for 24s LTC with 8000ppm drift, got {:?} (valid={}/{})",
            result.status, result.valid_frames, result.total_possible_frames);
        assert!(result.valid_frames * 2 >= result.total_possible_frames,
            "expected >=50% valid frames with 8000ppm drift, got {}/{}",
            result.valid_frames, result.total_possible_frames);
    }

    // ── 9b. WP-DR value-integrity integration (strict bar beyond the old
    //        cliffs — the decoder must report Success *and* correct values
    //        where the loose bar already held) ────────────────────────────

    /// All five harness seeds, shared with `sweep_robustness_limits_manual`.
    const DR_SEEDS: [u64; 5] = [42, 7, 123, 2024, 999];

    #[test]
    fn test_gaussian_beyond_old_strict_cliff_values_correct() {
        // Old worst-seed strict cliff: std 0.18. At std 0.30 the decoder
        // already reports Success (loose cliff ≈ 0.51) but with corrupt
        // values; the BCD + continuity integrity pass must make the values
        // correct on every harness seed.
        for &seed in &DR_SEEDS {
            let tcs = sequential_timecodes(50);
            let signal = add_gaussian_noise(&sequential_signal(), 0.30, seed);
            let result = base_decode(&signal);
            assert_ltc_fully_decoded(&result, 25, &tcs);
        }
    }

    #[test]
    fn test_dropout_over_bcd_digit_strict_passes() {
        // A 6 ms zero-span over one frame's BCD digits flips that frame's
        // value while its neighbours stay correct (the placement lottery
        // that broke the strict bar pre-WP-DR). The continuity repair must
        // recover it on at least 4 of 5 seeds.
        let mut passed = 0u32;
        for &seed in &DR_SEEDS {
            let signal = add_dropouts(&sequential_signal(), 48000, 0.006, 1, seed);
            let result = base_decode(&signal);
            let expected = sequential_timecodes(50);
            let strict = matches!(result.status, LtcDecodeStatus::Success) && {
                let mut ok = true;
                let mut exp_idx = 0usize;
                for ft in &result.timecodes {
                    if exp_idx > 0 && expected[exp_idx - 1] == ft.timecode { continue; }
                    while exp_idx < expected.len() && expected[exp_idx] != ft.timecode {
                        exp_idx += 1;
                    }
                    if exp_idx == expected.len() { ok = false; break; }
                    exp_idx += 1;
                }
                ok
            };
            if strict {
                passed += 1;
            } else {
                eprintln!("dropout-over-BCD seed {seed}: not strict, status={:?} valid={}/{}",
                    result.status, result.valid_frames, result.total_possible_frames);
            }
        }
        assert!(passed >= 4, "expected ≥4/5 seeds strict at 6ms dropout, got {passed}");
    }

    // ── 9c. WP-DR signal conditioning (DR3/DR4/DR5) ─────────────────────

    #[test]
    fn test_dc_blocker_removes_offset_four_times_signal() {
        // Old strict cliff: offset 0.492 ≈ signal amplitude. Four times
        // the signal must now decode with correct values.
        for offset in [2.0f32, 8.0] {
            let tcs = sequential_timecodes(50);
            let signal = add_dc_offset(&sequential_signal(), offset);
            let result = base_decode(&signal);
            assert_ltc_fully_decoded(&result, 40, &tcs);
        }
    }

    #[test]
    fn test_highpass_survives_hum_two_times_signal() {
        // Hum at twice the signal amplitude (old strict cliff ≈ 0.50 =
        // 1× signal) on both mains frequencies.
        for freq in [50.0f32, 60.0] {
            let tcs = sequential_timecodes(50);
            let signal = add_hum(&sequential_signal(), 48000, 1.0, freq);
            let result = base_decode(&signal);
            assert_ltc_fully_decoded(&result, 40, &tcs);
        }
    }

    #[test]
    fn test_conditioning_preserves_clean_decode() {
        // The always-on filters must be a no-op for clean material: full
        // frame yield and correct values.
        let tcs = sequential_timecodes(50);
        let result = base_decode(&sequential_signal());
        assert_ltc_fully_decoded(&result, 50, &tcs);
    }

    #[test]
    fn test_median_filter_removes_single_sample_spikes() {
        let mut signal = vec![0.0f32; 64];
        signal[10] = 1.0;
        signal[32] = -1.0;
        condition_signal(&mut signal, 48000);
        assert!(signal[10].abs() < 0.2, "single positive spike removed, got {}", signal[10]);
        assert!(signal[32].abs() < 0.2, "single negative spike removed, got {}", signal[32]);
        // A mid-band sinusoid (legitimate LTC-like content) passes the
        // whole conditioning chain essentially unchanged.
        let before: Vec<f32> = (0..4800)
            .map(|i| (2.0 * std::f32::consts::PI * 2000.0 * i as f32 / 48000.0).sin() * 0.5)
            .collect();
        let mut filtered = before.clone();
        condition_signal(&mut filtered, 48000);
        let max_diff = before.iter().zip(&filtered)
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max_diff < 0.05, "2 kHz carrier distorted by conditioning: max diff {max_diff}");
    }

    #[test]
    fn test_impulse_loose_cliff_beyond_quarter() {
        // Old loose cliff: p ≈ 0.21–0.24. A quarter of all samples
        // replaced by full-scale clicks must still decode (worst of the
        // five harness seeds).
        for &seed in &DR_SEEDS {
            let signal = add_impulse_noise(&sequential_signal(), 0.25, 1.0, seed);
            let result = base_decode(&signal);
            assert!(
                matches!(result.status, LtcDecodeStatus::Success),
                "impulse p=0.25 seed {seed}: expected Success, got {:?} (valid={}/{})",
                result.status, result.valid_frames, result.total_possible_frames
            );
        }
    }

    #[test]
    fn test_impulse_strict_beyond_point_zero_five() {
        // Old strict cliff: p ≈ 0.004 (seed lottery). At p = 0.05 the
        // values must stay correct on at least 4 of 5 seeds.
        let tcs = sequential_timecodes(50);
        let mut passed = 0u32;
        for &seed in &DR_SEEDS {
            let signal = add_impulse_noise(&sequential_signal(), 0.05, 1.0, seed);
            let result = base_decode(&signal);
            let check = std::panic::catch_unwind(|| {
                assert_ltc_fully_decoded(&result, 25, &tcs);
            });
            if check.is_ok() {
                passed += 1;
            } else {
                eprintln!("impulse p=0.05 seed {seed}: not strict, status={:?} valid={}/{}",
                    result.status, result.valid_frames, result.total_possible_frames);
            }
        }
        assert!(passed >= 4, "expected ≥4/5 seeds strict at impulse p=0.05, got {passed}");
    }

    #[test]
    fn test_min_volume_three_times_lower() {
        // Old strict cliff: volume 0.0104 (the 0.005 absolute ZC floor).
        // A third of that must now decode with correct values.
        let tcs = sequential_timecodes(50);
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.004);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    #[test]
    fn test_floor_survives_loud_clicks_on_quiet_signal() {
        // The peak-relative floor must not be inflated by full-scale
        // clicks riding a quiet signal (a raw-max peak would push the
        // floor above the 0.03 bit amplitude and kill the decode).
        let tcs = sequential_timecodes(50);
        let quiet = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.06);
        let signal = add_impulse_noise(&quiet, 0.01, 1.0, 42);
        let result = base_decode(&signal);
        assert_ltc_fully_decoded(&result, 40, &tcs);
    }

    // ── 10. Verify clean signal baseline (noise-free sanity check) ──────

    #[test]
    fn test_noise_baseline_clean() {
        let signal = base_signal();
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 45);
    }

    // ── compute_ltc_quality: real-world usability model ──────────────────
    //
    // The score must reflect what matters when syncing takes in an editor:
    // how much of the recording is covered by long, internally linear
    // blocks — not the raw count of discontinuities between them.

    const QFPS: f64 = 25.0;

    fn qtc_from_secs(secs: f64, fps: f64) -> Timecode {
        let fps_i = fps as i64;
        let total = (secs * fps).round() as i64;
        let total_secs = total.div_euclid(fps_i);
        Timecode {
            hours: (total_secs.div_euclid(3600).rem_euclid(24)) as u32,
            minutes: (total_secs.div_euclid(60).rem_euclid(60)) as u32,
            seconds: (total_secs.rem_euclid(60)) as u32,
            frames: (total.rem_euclid(fps_i)) as u32,
        }
    }

    /// One contiguous block of decoded frames.
    struct QBlock {
        frames: usize,
        /// TC value jump applied at this block's boundary (blocks > 0)
        tc_jump_secs: f64,
        /// Silence in the audio before this block starts (blocks > 0)
        #[allow(dead_code)]
        audio_gap_secs: f64,
        /// Audio advances at (1 + drift_rate) × the TC rate in this block
        drift_rate: f64,
    }

    fn build_quality_result(
        blocks: &[QBlock],
        glitch_indices: &[usize],
        lead_in_secs: f64,
        total_possible: u32,
        audio_duration: f64,
    ) -> LtcDetectionResult {
        let fd = 1.0 / QFPS;
        let mut tcs: Vec<f64> = Vec::new();
        let mut auds: Vec<f64> = Vec::new();
        let mut audio = lead_in_secs;
        let mut tc = 3600.0; // 01:00:00:00
        for (bi, b) in blocks.iter().enumerate() {
            if bi > 0 {
                audio += b.audio_gap_secs;
                tc += b.tc_jump_secs;
            }
            for _ in 0..b.frames {
                tcs.push(tc);
                auds.push(audio);
                audio += fd * (1.0 + b.drift_rate);
                tc += fd;
            }
        }
        // Glitch = single frame whose TC is 2 frames ahead of its neighbours
        // (stays within the segment but trips the glitch detector)
        for &g in glitch_indices {
            tcs[g] += 2.0 * fd;
        }
        let timecodes: Vec<FrameTimecode> = tcs
            .iter()
            .zip(&auds)
            .map(|(&t, &a)| FrameTimecode {
                frame_index: 0,
                timecode: qtc_from_secs(t, QFPS),
                timecode_secs: a,
            })
            .collect();
        LtcDetectionResult {
            status: LtcDecodeStatus::Success,
            detected_fps: QFPS as f32,
            drop_frame: false,
            total_possible_frames: total_possible,
            valid_frames: timecodes.len() as u32,
            avg_confidence: 1.0,
            timecodes,
            details: vec![],
            total_audio_duration_secs: audio_duration,
            sample_rate: 48000,
            processing_time_ms: 0.0,
            first_ltc_timecode_secs: 0.0,
            quality: None,
            chunk_summaries: Vec::new(),
        }
    }

    #[test]
    fn test_quality_fit_segment_drift_linear() {
        let audio: Vec<f64> = (0..100).map(|i| i as f64 * 0.04).collect();
        let drift: Vec<f64> = audio.iter().map(|&a| 0.002 * a).collect();
        let (slope, residual) = fit_segment_drift(&audio, &drift, 0..100);
        assert!((slope - 0.002).abs() < 1e-9, "slope {}", slope);
        assert!(residual < 1e-9, "residual {}", residual);
    }

    #[test]
    fn test_quality_fit_segment_drift_flat() {
        let audio: Vec<f64> = (0..50).map(|i| i as f64 * 0.04).collect();
        let drift = vec![0.5f64; 50];
        let (slope, residual) = fit_segment_drift(&audio, &drift, 0..50);
        assert!(slope.abs() < 1e-12);
        assert!(residual < 1e-12);
    }

    #[test]
    fn test_quality_fit_segment_drift_tiny_range() {
        let audio = vec![1.0f64];
        let drift = vec![0.5f64];
        let (slope, residual) = fit_segment_drift(&audio, &drift, 0..1);
        assert_eq!(slope, 0.0);
        assert_eq!(residual, 0.0);
    }

    /// The user-reported scenario: ~24000 frames, 11 forward TC jumps
    /// (generator restarts), sub-frame drift. Must stay highly usable —
    /// the old unbounded edit penalty drove this to 0% "Bad".
    #[test]
    fn test_quality_many_forward_edits_stay_usable() {
        let blocks: Vec<QBlock> = (0..12)
            .map(|_| QBlock { frames: 2000, tc_jump_secs: 5.0, audio_gap_secs: 0.0, drift_rate: 0.0 })
            .collect();
        let result = build_quality_result(&blocks, &[], 0.0, 24000, 960.0);
        let q = compute_ltc_quality(&result).unwrap();

        assert_eq!(q.block_count, 12, "12 blocks expected: {}", q.summary);
        assert_eq!(q.backward_jump_count, 0, "all jumps forward: {}", q.summary);
        assert!((q.usable_coverage - 1.0).abs() < 1e-9, "all blocks usable: {}", q.summary);
        assert!(q.score >= 0.75, "11 benign edits must not destroy the score, got {:.2} ({})",
            q.score, q.grade);
    }

    /// A backward TC jump (timecode reset) must cost more than a forward
    /// jump of the same size: the reset TC values can recur, making
    /// sync-by-TC ambiguous in editors.
    #[test]
    fn test_quality_backward_jump_penalized_harder_than_forward() {
        let mk = |jump: f64| {
            let blocks = vec![
                QBlock { frames: 1500, tc_jump_secs: 0.0, audio_gap_secs: 0.0, drift_rate: 0.0 },
                QBlock { frames: 1500, tc_jump_secs: jump, audio_gap_secs: 0.0, drift_rate: 0.0 },
                QBlock { frames: 1500, tc_jump_secs: 0.0, audio_gap_secs: 0.0, drift_rate: 0.0 },
            ];
            build_quality_result(&blocks, &[], 0.0, 4500, 180.0)
        };
        let fwd = compute_ltc_quality(&mk(60.0)).unwrap();
        let bwd = compute_ltc_quality(&mk(-60.0)).unwrap();

        assert_eq!(bwd.backward_jump_count, 1);
        assert!(bwd.score < fwd.score - 0.05,
            "backward jump ({:.2}) must score below forward jump ({:.2})",
            bwd.score, fwd.score);
        assert!(fwd.score >= 0.90, "single forward jump: {:.2} ({})", fwd.score, fwd.grade);
    }

    /// A block whose clock drifts (2 frames accumulated over a minute) is
    /// not frame-accurate and must reduce coverage; the damage must be
    /// proportional to the affected frame fraction.
    #[test]
    fn test_quality_drifting_block_reduces_score_proportionally() {
        // 1 of 3 blocks drifts
        let blocks3 = vec![
            QBlock { frames: 1500, tc_jump_secs: 0.0, audio_gap_secs: 0.0, drift_rate: 0.0 },
            QBlock { frames: 1500, tc_jump_secs: 5.0, audio_gap_secs: 0.0, drift_rate: 1.33e-3 },
            QBlock { frames: 1500, tc_jump_secs: 5.0, audio_gap_secs: 0.0, drift_rate: 0.0 },
        ];
        let r3 = build_quality_result(&blocks3, &[], 0.0, 4500, 180.0);
        let q3 = compute_ltc_quality(&r3).unwrap();

        // 1 of 10 blocks drifts (same drifting block size)
        let mut blocks10 = vec![
            QBlock { frames: 1500, tc_jump_secs: 0.0, audio_gap_secs: 0.0, drift_rate: 0.0 },
            QBlock { frames: 1500, tc_jump_secs: 5.0, audio_gap_secs: 0.0, drift_rate: 1.33e-3 },
        ];
        for _ in 0..8 {
            blocks10.push(QBlock { frames: 1500, tc_jump_secs: 5.0, audio_gap_secs: 0.0, drift_rate: 0.0 });
        }
        let r10 = build_quality_result(&blocks10, &[], 0.0, 15000, 600.0);
        let q10 = compute_ltc_quality(&r10).unwrap();

        assert!(q3.worst_block_drift_frames > 1.5 && q3.worst_block_drift_frames < 2.5,
            "worst block drift should be ~2 frames, got {:.2}", q3.worst_block_drift_frames);
        assert!(q3.score < 0.80, "drifting block must degrade the score, got {:.2}", q3.score);
        assert!(q10.score > q3.score,
            "same drift affecting 1/10 of frames ({:.2}) must score above 1/3 ({:.2})",
            q10.score, q3.score);
    }

    /// A few glitches in a long recording are statistically irrelevant and
    /// must not drag the score down (old code: fixed -0.03 per glitch).
    #[test]
    fn test_quality_few_glitches_in_long_recording_minor() {
        let blocks = vec![QBlock { frames: 20000, tc_jump_secs: 0.0, audio_gap_secs: 0.0, drift_rate: 0.0 }];
        let result = build_quality_result(&blocks, &[100, 200, 300, 400, 500, 600, 700, 800, 900, 1000,
                                                    1100, 1200, 1300, 1400, 1500, 1600, 1700, 1800, 1900, 2000,
                                                    2100, 2200, 2300, 2400, 2500, 2600, 2700, 2800, 2900, 3000],
                                          0.0, 20000, 800.0);
        let q = compute_ltc_quality(&result).unwrap();
        assert!(q.glitch_count >= 25, "glitches must be detected, got {}", q.glitch_count);
        assert!(q.score >= 0.95, "30 glitches in 20000 frames are minor, got {:.2} ({})",
            q.score, q.grade);
    }

    /// Silent lead-in before the LTC starts must not count as missing
    /// frames (real camera recordings routinely start recording before
    /// LTC is fed).
    #[test]
    fn test_quality_silent_prefix_not_missing_frames() {
        let blocks = vec![QBlock { frames: 1500, tc_jump_secs: 0.0, audio_gap_secs: 0.0, drift_rate: 0.0 }];
        let result = build_quality_result(&blocks, &[], 30.0, 2250, 90.0);
        let q = compute_ltc_quality(&result).unwrap();

        assert_eq!(q.missing_frames, 0, "30s silent lead-in is not missing LTC: {}", q.summary);
        assert!(q.score >= 0.99, "perfect LTC after a lead-in must score ~1.0, got {:.2} ({})",
            q.score, q.grade);
    }

    // ── ScoredResult constructors / prefer_zc_or_detailed / score_candidate ──

    fn mk_scored(valid_frames: u32) -> ScoredResult {
        ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames,
            grid_valid: valid_frames,
            total_possible: 10,
            timecodes: Vec::new(),
            details_entry: String::new(),
            spb: 24.0,
            phase: 0,
            adaptive: false,
            frame_starts: Vec::new(),
        }
    }

    #[test]
    fn test_scored_result_from_frame_starts() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let frame_bits = crate::get_ltc_bits(&tc, false);
        let mut bits = frame_bits.to_vec();
        bits.extend_from_slice(&frame_bits); // two frames worth

        let r = ScoredResult::from_frame_starts(
            CandidateTiming {
                fps: 25.0,
                drop_frame: false,
                sample_rate: 48000,
                spb: 24.0,
                phase: 480,
            },
            5,
            &bits,
            vec![0, 80],
            "details".to_string(),
            false,
            2,
        );

        assert_eq!(r.valid_frames, 2, "valid_frames == frame_starts.len()");
        assert_eq!(r.total_possible, 5, "total_possible passed through");
        assert_eq!(r.timecodes[0].frame_index, 0);
        assert_eq!(r.timecodes[1].frame_index, 1);
        // (phase + start*spb)/sample_rate: (480+0)/48000 and (480+80*24)/48000
        assert!((r.timecodes[0].timecode_secs - 0.01).abs() < 1e-12, "got {}", r.timecodes[0].timecode_secs);
        assert!((r.timecodes[1].timecode_secs - 0.05).abs() < 1e-12, "got {}", r.timecodes[1].timecode_secs);
        assert_eq!(r.timecodes[0].timecode, tc);
        assert_eq!(r.timecodes[1].timecode, tc);
        assert_eq!(r.phase, 480);
        assert_eq!(r.spb, 24.0);
    }

    #[test]
    fn test_scored_result_canceled_shape() {
        let params = mk_scored(7);
        let c = ScoredResult::canceled(&params, 999);
        assert_eq!(c.valid_frames, 0);
        assert_eq!(c.total_possible, 0);
        assert!(c.timecodes.is_empty());
        assert!(c.frame_starts.is_empty());
        assert_eq!(c.phase, 999);
        assert_eq!(c.spb, params.spb);
        assert_eq!(c.fps, params.fps);
        assert_eq!(c.drop_frame, params.drop_frame);
    }

    #[test]
    fn test_prefer_zc_or_detailed_strict_beat() {
        // ZC strictly beats → ZC wins.
        let zc = Some(mk_scored(5));
        let detailed = Some(mk_scored(3));
        let winner = prefer_zc_or_detailed(zc, detailed, "detailed scan").unwrap();
        assert_eq!(winner.valid_frames, 5);

        // Tie → detailed wins.
        let zc = Some(mk_scored(3));
        let detailed = Some(mk_scored(3));
        let winner = prefer_zc_or_detailed(zc, detailed, "detailed scan").unwrap();
        assert_eq!(winner.valid_frames, 3);

        // Detailed strictly better → detailed wins.
        let zc = Some(mk_scored(3));
        let detailed = Some(mk_scored(8));
        let winner = prefer_zc_or_detailed(zc, detailed, "detailed scan").unwrap();
        assert_eq!(winner.valid_frames, 8);
    }

    #[test]
    fn test_prefer_zc_or_detailed_none_handling() {
        // No ZC result → detailed passes through.
        let detailed = Some(mk_scored(2));
        let winner = prefer_zc_or_detailed(None, detailed, "detailed scan").unwrap();
        assert_eq!(winner.valid_frames, 2);

        // Detailed absent → ZC wins (fallback-scan semantics: no detailed
        // result to beat).
        let zc = Some(mk_scored(4));
        let winner = prefer_zc_or_detailed(zc, None, "fallback scan").unwrap();
        assert_eq!(winner.valid_frames, 4);

        // Both absent → None.
        assert!(prefer_zc_or_detailed(None, None, "fallback scan").is_none());
    }

    #[test]
    fn test_score_candidate_beats_and_branches() {
        let tcs: Vec<Timecode> = (0..10)
            .map(|i| Timecode { hours: 0, minutes: 0, seconds: 0, frames: i })
            .collect();
        let samples = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        // Nominal branch, beats best_valid=0.
        match score_candidate(&mk_ctx(&samples, &[]), 24.0, 0, false, 0) {
            ScoredCandidate::Beat(r) => {
                assert!(r.valid_frames >= 5, "expected several frames, got {}", r.valid_frames);
                assert_eq!(r.spb, 24.0);
                assert_eq!(r.phase, 0);
                assert!(!r.adaptive, "nominal branch must be flagged non-adaptive");
            }
            other => panic!("expected Beat, got {:?}", other),
        }

        // Adaptive branch: flagged adaptive, still beats 0.
        match score_candidate(&mk_ctx(&samples, &[]), 24.0, 0, true, 0) {
            ScoredCandidate::Beat(r) => {
                assert!(r.adaptive, "refinement branch must be flagged adaptive");
                assert_eq!(r.spb, 24.0);
                assert_eq!(r.phase, 0);
            }
            other => panic!("expected Beat, got {:?}", other),
        }

        // No beat: a best_valid no candidate can beat → NoBeat.
        match score_candidate(&mk_ctx(&samples, &[]), 12.0, 0, false, u32::MAX) {
            ScoredCandidate::NoBeat => {}
            other => panic!("expected NoBeat, got {:?}", other),
        }

        // Too short: fewer than 80 bits extractable → TooShort.
        let tiny = vec![0.0f32; 100];
        match score_candidate(&mk_ctx(&tiny, &[]), 12.0, 0, false, 0) {
            ScoredCandidate::TooShort => {}
            other => panic!("expected TooShort, got {:?}", other),
        }
    }

    // ── find_frames phase helpers ─────────────────────────────────────

    fn chain_frame_bits(tcs: &[Timecode]) -> Vec<u8> {
        let mut bits = Vec::new();
        for tc in tcs {
            bits.extend(crate::get_ltc_bits(tc, false));
        }
        bits
    }

    #[test]
    fn chain_relock_accepts_run_with_consecutive_timecode_pair() {
        let tcs: Vec<Timecode> = (0..6)
            .map(|i| Timecode { hours: 1, minutes: 0, seconds: 0, frames: i })
            .collect();
        let bits = chain_frame_bits(&tcs);
        let sync_positions = scan_sync_positions(&bits);
        assert_eq!(sync_positions.len(), 6);

        let starts = chain_relock_starts(&bits, &sync_positions, 25.0, false);
        assert_eq!(starts.len(), 6, "every real frame must re-lock");
        assert!(starts.windows(2).all(|w| w[1] == w[0] + 80), "{:?}", starts);
    }

    #[test]
    fn chain_relock_rejects_noise_chain_without_consecutive_timecodes() {
        // Five sync words spaced exactly 80 bits over a zero payload: the
        // spacing alone is not enough — the run must carry a consecutive-TC
        // pair, and an all-zero payload decodes to identical timecodes.
        let mut bits = vec![0u8; 6 * 80];
        for k in 0..6u8 {
            bits[(k as usize) * 80 + SYNC_OFFSET..(k as usize) * 80 + SYNC_OFFSET + 16]
                .copy_from_slice(&SYNC_WORD);
        }
        let sync_positions: Vec<usize> = (0..6).map(|k| k * 80 + SYNC_OFFSET).collect();

        let starts = chain_relock_starts(&bits, &sync_positions, 25.0, false);
        assert!(starts.is_empty(), "garbage-payload chain must be rejected: {:?}", starts);
    }

    #[test]
    fn chain_relock_rejects_runs_shorter_than_five_members() {
        let tcs: Vec<Timecode> = (0..3)
            .map(|i| Timecode { hours: 1, minutes: 0, seconds: 0, frames: i })
            .collect();
        let bits = chain_frame_bits(&tcs);
        let sync_positions = scan_sync_positions(&bits);
        assert_eq!(sync_positions.len(), 3);

        let starts = chain_relock_starts(&bits, &sync_positions, 25.0, false);
        assert!(starts.is_empty(), "sub-threshold run must be rejected: {:?}", starts);
    }

    #[test]
    fn dedupe_near_duplicates_clusters_within_40_bits_keeping_first() {
        // Clustering is relative to the last *kept* start: 139/141-style
        // 1-2-bit strays of a kept start collapse into it; anything ≥40
        // bits past the last kept start opens a new cluster.
        let starts = vec![220, 100, 142, 139];
        assert_eq!(dedupe_near_duplicates(starts), vec![100, 142, 220]);
    }

    #[test]
    fn dedupe_near_duplicates_keeps_distinct_80_bit_frames() {
        let starts: Vec<usize> = (0..5).map(|k| k * 80).collect();
        assert_eq!(dedupe_near_duplicates(starts), (0..5).map(|k| k * 80).collect::<Vec<_>>());
    }

    #[test]
    fn merge_grid_and_chains_prefers_grid_copy_within_40_bits() {
        // Chain start 10 is the same frame as grid start 0 — the grid copy
        // wins; chain start 200 has no grid neighbour and is kept.
        let merged = merge_grid_and_chains(vec![0, 80], vec![10, 200]);
        assert_eq!(merged, vec![0, 80, 200]);
    }

    // ── compute_ltc_quality sub-analyzers ─────────────────────────────

    #[test]
    fn test_quality_score_penalty_table() {
        // Clean input: score == usable_coverage.
        assert!((quality_score(1.0, 0, 0.0, 0.0, 0.0, 0.0) - 1.0).abs() < 1e-12);
        assert!((quality_score(0.8, 0, 0.0, 0.0, 0.0, 0.0) - 0.8).abs() < 1e-12);

        // Edit term: 0.02 per edit, capped at 10 edits.
        assert!((quality_score(1.0, 1, 0.0, 0.0, 0.0, 0.0) - 0.98).abs() < 1e-12);
        assert!((quality_score(1.0, 10, 0.0, 0.0, 0.0, 0.0) - 0.80).abs() < 1e-12);
        assert!((quality_score(1.0, 25, 0.0, 0.0, 0.0, 0.0) - 0.80).abs() < 1e-12, "edits cap at 10");

        // Backward term: 0.30 × affected ratio.
        assert!((quality_score(1.0, 0, 0.5, 0.0, 0.0, 0.0) - 0.85).abs() < 1e-12);

        // Glitch ramp: none below 0.1%; full 0.15 by 1%.
        assert!((quality_score(1.0, 0, 0.0, 0.001, 0.0, 0.0) - 1.0).abs() < 1e-12);
        assert!((quality_score(1.0, 0, 0.0, 0.01, 0.0, 0.0) - 0.85).abs() < 1e-12);

        // Drift penalty passes through.
        assert!((quality_score(1.0, 0, 0.0, 0.0, 0.05, 0.0) - 0.95).abs() < 1e-12);

        // Missing ramp: none below 5%; 0.15 at 50%.
        assert!((quality_score(1.0, 0, 0.0, 0.0, 0.0, 0.05) - 1.0).abs() < 1e-12);
        assert!((quality_score(1.0, 0, 0.0, 0.0, 0.0, 0.5) - 0.85).abs() < 1e-12);

        // Clamped at both ends.
        assert_eq!(quality_score(0.1, 10, 1.0, 0.01, 0.1, 0.5), 0.0);
        assert_eq!(quality_score(1.0, 0, 0.0, 0.0, 0.0, 0.0), 1.0);

        // Grade boundaries via QualityGrade::from_score.
        assert_eq!(QualityGrade::from_score(0.95), QualityGrade::Excellent);
        assert_eq!(QualityGrade::from_score(0.80), QualityGrade::Good);
        assert_eq!(QualityGrade::from_score(0.60), QualityGrade::Fair);
        assert_eq!(QualityGrade::from_score(0.30), QualityGrade::Poor);
        assert_eq!(QualityGrade::from_score(0.29), QualityGrade::Bad);
    }

    #[test]
    fn test_split_segments_contiguous_and_gaps() {
        let fd = 1.0 / 25.0;
        // Contiguous frames → one segment.
        let contig: Vec<f64> = (0..10).map(|i| i as f64 * fd).collect();
        assert_eq!(split_segments(&contig, 25.0), vec![0..10]);

        // A jump of 3.5 frames (deviation 2.5 frames > the 2-frame gap
        // threshold) → split. Exact-threshold values are float-fragile and
        // deliberately avoided.
        let mut with_jump = contig.clone();
        with_jump.push(contig[9] + fd * 4.5);
        let segs = split_segments(&with_jump, 25.0);
        assert_eq!(segs, vec![0..10, 10..11]);

        // 1.5-frame jump (deviation 0.5 frames, within threshold) → no split.
        let mut jitter = contig.clone();
        jitter.push(contig[9] + fd * 2.5);
        assert_eq!(split_segments(&jitter, 25.0), vec![0..11]);
    }

    #[test]
    fn test_analyze_glitches() {
        let fd = 1.0 / 25.0;
        let mut ltc: Vec<f64> = (0..10).map(|i| i as f64 * fd).collect();
        // Isolated glitch at index 5: deviates 1.8 frames from the midpoint
        // of its neighbours (above the 1.5-frame glitch threshold) while the
        // segment split deviations stay at 1.8 frames, under the 2-frame bar.
        ltc[5] = ltc[4] + fd * 2.8;
        let segments = split_segments(&ltc, 25.0);
        let stats = analyze_glitches(&ltc, &segments, 25.0);
        assert_eq!(stats.glitch_count, 1);
        assert_eq!(stats.glitch_indices, vec![5]);

        // Segments shorter than 3 frames are skipped entirely.
        let short: Vec<f64> = (0..2).map(|i| i as f64 * fd).collect();
        let segs = split_segments(&short, 25.0);
        let stats = analyze_glitches(&short, &segs, 25.0);
        assert_eq!(stats.glitch_count, 0);
        assert!(stats.glitch_indices.is_empty());
    }

    #[test]
    fn test_analyze_gaps_edit_and_backward() {
        let fd = 1.0 / 25.0;
        // Two blocks: audio continues, LTC jumps forward by 20 frames →
        // edit (jump >= 10 frames AND audio/ltc mismatch > 0.1 s).
        let audio: Vec<f64> = (0..20).map(|i| i as f64 * fd).collect();

        // Forward jump of 21 frames (> the 10-frame edit threshold) with an
        // audio/LTC mismatch → gap + edit.
        let mut ltc: Vec<f64> = (0..10).map(|i| i as f64 * fd).collect();
        ltc.extend((10..20).map(|i| i as f64 * fd + 20.0 * fd));
        let segments = split_segments(&ltc, 25.0);
        let stats = analyze_gaps(&audio, &ltc, &segments, 25.0);
        assert_eq!(stats.gap_count, 1);
        assert_eq!(stats.edit_count, 1);
        assert_eq!(stats.backward_jump_count, 0);
        assert_eq!(stats.gap_edges, vec![(9, 10)]);

        // Small forward jump (6 frames < 10-frame threshold) is a gap but
        // not an edit, even though the audio/LTC mismatch exceeds 0.1 s.
        let mut ltc2: Vec<f64> = (0..10).map(|i| i as f64 * fd).collect();
        ltc2.extend((10..20).map(|i| i as f64 * fd + 5.0 * fd));
        let segs2 = split_segments(&ltc2, 25.0);
        let stats2 = analyze_gaps(&audio, &ltc2, &segs2, 25.0);
        assert_eq!(stats2.gap_count, 1);
        assert_eq!(stats2.edit_count, 0);

        // Backward jump (TC reset): LTC value goes back by 3 frames
        // (> the 0.5-frame backward threshold).
        let mut ltc3: Vec<f64> = (0..10).map(|i| i as f64 * fd).collect();
        ltc3.extend((10..20).map(|i| i as f64 * fd - 4.0 * fd));
        let segs3 = split_segments(&ltc3, 25.0);
        let stats3 = analyze_gaps(&audio, &ltc3, &segs3, 25.0);
        assert_eq!(stats3.backward_jump_count, 1);
        assert_eq!(stats3.backward_affected_frames, 10, "frames from the backward block onward are ambiguous");
    }

    #[test]
    fn test_missing_in_span_excludes_silent_edges() {
        let fd = 1.0 / 25.0;
        // Full contiguous span → nothing missing.
        let full: Vec<f64> = (0..50).map(|i| i as f64 * fd).collect();
        assert_eq!(missing_in_span(&full, 25.0), (0, 0.0));

        // A leading silent edge is not an interior hole: the span is
        // measured from the first decoded frame.
        let lead_in: Vec<f64> = (0..50).map(|i| 10.0 + i as f64 * fd).collect();
        assert_eq!(missing_in_span(&lead_in, 25.0), (0, 0.0));

        // An interior hole: 50 frames span, one frame absent in the middle.
        let mut hole: Vec<f64> = (0..25).map(|i| i as f64 * fd).collect();
        hole.extend((26..50).map(|i| i as f64 * fd));
        let (missing, ratio) = missing_in_span(&hole, 25.0);
        assert_eq!(missing, 1);
        assert!(ratio > 0.0 && ratio < 0.05, "1 missing of ~50 is {:.3}", ratio);
    }

    // ═══════════════════════════════════════════════════════════════════
    //  Robustness-limit measurement harness (manual calibration sweep)
    //
    //  Not part of the regular suite (`#[ignore]`): bisects each corruption
    //  parameter to find the decoder's actual breaking point. Run with:
    //    cargo test -p audio-core --lib sweep_robustness -- --ignored --nocapture
    //  Re-run whenever the decoder's detection constants change.
    // ═══════════════════════════════════════════════════════════════════

    struct GateOutcome {
        /// Success + decoded TCs are an in-order subsequence of the
        /// synthesized source sequence (no garbage values). Success already
        /// enforces >= 70% valid frames via the confidence gate.
        strict: bool,
        /// The decoder's own Success verdict (confidence >= 0.70 gate).
        loose: bool,
        valid: u32,
        total: u32,
        confidence: f32,
        quality: f64,
        status_desc: String,
    }

    /// Decode a (possibly corrupted) signal and evaluate both gates.
    fn evaluate_gate(signal: &[f32], expected_tcs: &[Timecode]) -> GateOutcome {
        let result = base_decode(signal);
        let loose = matches!(result.status, LtcDecodeStatus::Success);
        // Two-pointer subsequence check: every decoded timecode must appear
        // in the expected sequence, in order, with no out-of-sequence values.
        let mut exp_idx = 0usize;
        let mut tc_ok = true;
        for ft in &result.timecodes {
            // A frame re-decoded at a corruption boundary may repeat the
            // just-matched timecode; that is a reporting artifact, not a
            // wrong value, so it does not break the check.
            if exp_idx > 0 && expected_tcs[exp_idx - 1] == ft.timecode {
                continue;
            }
            while exp_idx < expected_tcs.len() && expected_tcs[exp_idx] != ft.timecode {
                exp_idx += 1;
            }
            if exp_idx == expected_tcs.len() {
                tc_ok = false;
                break;
            }
            exp_idx += 1;
        }
        GateOutcome {
            strict: loose && tc_ok,
            loose,
            valid: result.valid_frames,
            total: result.total_possible_frames,
            confidence: result.avg_confidence,
            quality: result.quality.as_ref().map(|q| q.score).unwrap_or(0.0),
            status_desc: format!("{:?}", result.status),
        }
    }

    fn gate_passes(g: &GateOutcome, strict: bool) -> bool {
        if strict { g.strict } else { g.loose }
    }

    /// Bisect for the maximum parameter value that still passes. Both bounds
    /// auto-widen until they bracket the cliff: `lo` halves toward `lo_floor`
    /// until it passes, `hi` doubles toward `hi_cap` until it fails.
    /// Returns `(limit, probe_count)`.
    fn bisect_max_pass(
        mut probe: impl FnMut(f64) -> GateOutcome,
        lo0: f64,
        lo_floor: f64,
        hi0: f64,
        hi_cap: f64,
        tol: f64,
        strict: bool,
    ) -> (f64, usize) {
        let mut lo = lo0;
        let mut probes = 0;
        let mut first = probe(lo);
        probes += 1;
        while !gate_passes(&first, strict) && lo > lo_floor {
            lo = (lo / 2.0).max(lo_floor);
            first = probe(lo);
            probes += 1;
        }
        assert!(
            gate_passes(&first, strict),
            "no passing lo bound between {} and {}: {}",
            lo0,
            lo_floor,
            describe_failure(&first)
        );
        let mut hi = hi0;
        let mut out = probe(hi);
        probes += 1;
        while gate_passes(&out, strict) && hi < hi_cap {
            lo = hi;
            hi = (hi * 2.0).min(hi_cap);
            out = probe(hi);
            probes += 1;
        }
        if gate_passes(&out, strict) {
            return (hi_cap, probes);
        }
        while hi - lo > tol.max(lo * 1e-3) {
            let mid = (lo + hi) / 2.0;
            probes += 1;
            if gate_passes(&probe(mid), strict) {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        (lo, probes)
    }

    /// Bisect for the minimum parameter value that still passes (for
    /// dimensions where smaller = worse). Bounds auto-widen: `hi` doubles
    /// toward `hi_cap` until it passes, `lo` halves toward `lo_floor` until
    /// it fails.
    fn bisect_min_pass(
        mut probe: impl FnMut(f64) -> GateOutcome,
        hi0: f64,
        hi_cap: f64,
        lo0: f64,
        lo_floor: f64,
        tol: f64,
        strict: bool,
    ) -> (f64, usize) {
        let mut hi = hi0;
        let mut probes = 0;
        let mut first = probe(hi);
        probes += 1;
        while !gate_passes(&first, strict) && hi < hi_cap {
            hi = (hi * 2.0).min(hi_cap);
            first = probe(hi);
            probes += 1;
        }
        assert!(
            gate_passes(&first, strict),
            "no passing hi bound between {} and {}: {}",
            hi0,
            hi_cap,
            describe_failure(&first)
        );
        let mut lo = lo0;
        let mut out = probe(lo);
        probes += 1;
        while !gate_passes(&out, strict) && lo > lo_floor {
            hi = lo;
            lo = (lo / 2.0).max(lo_floor);
            out = probe(lo);
            probes += 1;
        }
        if !gate_passes(&out, strict) {
            return (lo_floor, probes);
        }
        while hi - lo > tol.max(hi * 1e-3) {
            let mid = (lo + hi) / 2.0;
            probes += 1;
            if gate_passes(&probe(mid), strict) {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        (hi, probes)
    }

    /// Re-probe a value, keeping the outcome for failure-mode reporting.
    fn snr_db(signal_power: f64, std_dev: f64) -> f64 {
        10.0 * (signal_power / (std_dev * std_dev)).log10()
    }

    fn describe_failure(g: &GateOutcome) -> String {
        format!(
            "just past cliff: {} valid={}/{} conf={:.2} quality={:.2}",
            g.status_desc, g.valid, g.total, g.confidence, g.quality
        )
    }

    #[test]
    #[ignore = "manual calibration harness — run with: cargo test -p audio-core --lib sweep_robustness -- --ignored --nocapture"]
    fn sweep_robustness_limits_manual() {
        let seeds: [u64; 5] = [42, 7, 123, 2024, 999];
        // Proper sequential timecodes (make_timecodes() yields invalid
        // frames ≥ fps, which breaks TC-value comparison in the gate).
        let tcs: Vec<Timecode> = (0..50)
            .map(|i| Timecode {
                hours: 0,
                minutes: 0,
                seconds: (i / 25),
                frames: (i % 25),
            })
            .collect();
        let clean = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let sig_power = 0.25f64; // square wave amplitude 0.5 → mean square 0.25

        println!("\n=== Robustness limit sweep ===");
        println!("base signal: 50 frames @ 25 fps, 48 kHz, volume 0.5 (2.0 s)");
        println!("strict bar = Success + all decoded TCs correct;  loose bar = plain Success\n");

        let clean_gate = evaluate_gate(&clean, &tcs);
        println!(
            "clean baseline gate: strict={} loose={} valid={}/{} conf={:.2}",
            clean_gate.strict, clean_gate.loose, clean_gate.valid, clean_gate.total, clean_gate.confidence
        );
        let near_clean = evaluate_gate(&add_gaussian_noise(&clean, 0.001, 42), &tcs);
        println!(
            "gaussian std=0.001 gate: strict={} valid={}/{} conf={:.2} status={}",
            near_clean.strict, near_clean.valid, near_clean.total, near_clean.confidence, near_clean.status_desc
        );

        // ── 1. Additive Gaussian noise ────────────────────────────────
        for (bar, strict) in [("strict", true), ("loose", false)] {
            let mut cliffs = Vec::new();
            for &seed in &seeds {
                let probe = |std: f64| {
                    let sig = add_gaussian_noise(&clean, std as f32, seed);
                    evaluate_gate(&sig, &tcs)
                };
                let (limit, _) = bisect_max_pass(probe, 0.05, 0.001, 0.5, 4.0, 0.005, strict);
                cliffs.push(limit);
            }
            // failure mode at worst-seed cliff
            let worst = cliffs
                .iter()
                .cloned()
                .filter(|c| c.is_finite())
                .fold(f64::INFINITY, f64::min);
            let worst_seed = seeds
                [cliffs.iter().position(|c| *c == worst).unwrap_or(0)];
            let fail_mode = describe_failure(&evaluate_gate(
                &add_gaussian_noise(&clean, ((worst * 1.1).min(4.0)) as f32, worst_seed),
                &tcs,
            ));
            let snrs: Vec<String> = cliffs
                .iter()
                .map(|c| format!("{:.1}dB", snr_db(sig_power, *c)))
                .collect();
            println!(
                "gaussian noise  [{bar}] std per seed: {:?}  (SNR: {:?})  worst std={:.3} @seed {}  {}",
                cliffs, snrs, worst, worst_seed, fail_mode
            );
        }

        // ── 2. Impulse (click) noise, amplitude 1.0 ───────────────────
        for (bar, strict) in [("strict", true), ("loose", false)] {
            let mut cliffs = Vec::new();
            for &seed in &seeds {
                let probe = |p: f64| {
                    let sig = add_impulse_noise(&clean, p as f32, 1.0, seed);
                    evaluate_gate(&sig, &tcs)
                };
                let (limit, _) = bisect_max_pass(probe, 0.001, 0.00001, 0.05, 0.95, 0.001, strict);
                cliffs.push(limit);
            }
            let worst = cliffs
                .iter()
                .cloned()
                .filter(|c| c.is_finite())
                .fold(f64::INFINITY, f64::min);
            let worst_seed = seeds
                [cliffs.iter().position(|c| *c == worst).unwrap_or(0)];
            let fail_mode = describe_failure(&evaluate_gate(
                &add_impulse_noise(&clean, (worst * 1.1).min(0.95) as f32, 1.0, worst_seed),
                &tcs,
            ));
            println!(
                "impulse noise   [{bar}] p per seed: {:?}  worst p={:.4} @seed {}  {}",
                cliffs, worst, worst_seed, fail_mode
            );
        }

        // ── 3. DC offset (deterministic) ──────────────────────────────
        for (bar, strict) in [("strict", true), ("loose", false)] {
            let probe = |off: f64| {
                let sig = add_dc_offset(&clean, off as f32);
                evaluate_gate(&sig, &tcs)
            };
            let (limit, _) = bisect_max_pass(probe, 0.01, 0.0001, 0.5, 8.0, 0.005, strict);
            let fail_mode = describe_failure(&evaluate_gate(
                &add_dc_offset(&clean, (limit * 1.1).min(8.0) as f32),
                &tcs,
            ));
            println!("dc offset       [{bar}] max={:.3}  {}", limit, fail_mode);
        }

        // ── 4. Mains hum (deterministic) ──────────────────────────────
        for freq in [50.0f32, 60.0] {
            for (bar, strict) in [("strict", true), ("loose", false)] {
                let probe = |amp: f64| {
                    let sig = add_hum(&clean, 48000, amp as f32, freq);
                    evaluate_gate(&sig, &tcs)
                };
                let (limit, _) = bisect_max_pass(probe, 0.05, 0.0001, 0.5, 8.0, 0.005, strict);
                let fail_mode = describe_failure(&evaluate_gate(
                    &add_hum(&clean, 48000, (limit * 1.1).min(8.0) as f32, freq),
                    &tcs,
                ));
                println!("hum {:.0} Hz     [{bar}] max amp={:.3}  {}", freq, limit, fail_mode);
            }
        }

        // ── 5. Amplitude fading (deterministic, 1 Hz modulation) ──────
        for (bar, strict) in [("strict", true), ("loose", false)] {
            let probe = |depth: f64| {
                let sig = apply_fading(&clean, 48000, 1.0, depth as f32);
                evaluate_gate(&sig, &tcs)
            };
            let (limit, _) = bisect_max_pass(probe, 0.3, 0.05, 0.6, 1.0, 0.005, strict);
            let fail_mode = describe_failure(&evaluate_gate(
                &apply_fading(&clean, 48000, 1.0, (limit * 1.05).min(1.0) as f32),
                &tcs,
            ));
            println!("fading 1Hz      [{bar}] max depth={:.3}  {}", limit, fail_mode);
        }

        // ── 6. Dropouts: one zero-filled span, bisect duration ────────
        for (bar, strict) in [("strict", true), ("loose", false)] {
            let mut cliffs = Vec::new();
            for &seed in &seeds {
                let probe = |secs: f64| {
                    let sig = add_dropouts(&clean, 48000, secs as f32, 1, seed);
                    evaluate_gate(&sig, &tcs)
                };
                let (limit, _) = bisect_max_pass(probe, 0.05, 0.005, 0.4, 1.9, 0.005, strict);
                cliffs.push(limit);
            }
            let worst = cliffs
                .iter()
                .cloned()
                .filter(|c| c.is_finite())
                .fold(f64::INFINITY, f64::min);
            let worst_seed = seeds
                [cliffs.iter().position(|c| *c == worst).unwrap_or(0)];
            let fail_mode = describe_failure(&evaluate_gate(
                &add_dropouts(&clean, 48000, (worst * 1.1).min(1.9) as f32, 1, worst_seed),
                &tcs,
            ));
            let pct: Vec<String> = cliffs.iter().map(|c| format!("{:.0}%", c * 50.0)).collect();
            println!(
                "dropout span    [{bar}] secs per seed: {:?} (of 2.0s = {:?} loss)  worst={:.3}s @seed {}  {}",
                cliffs, pct, worst, worst_seed, fail_mode
            );
        }

        // ── 7. Low-pass (bandwidth-limited link; smaller = worse) ─────
        for (bar, strict) in [("strict", true), ("loose", false)] {
            let probe = |factor: f64| {
                let sig = apply_lowpass(&clean, factor as f32);
                evaluate_gate(&sig, &tcs)
            };
            let (limit, _) = bisect_min_pass(probe, 0.3, 0.9, 0.06, 0.002, 0.001, strict);
            let fail_mode = describe_failure(&evaluate_gate(
                &apply_lowpass(&clean, (limit / 1.5).max(0.0005) as f32),
                &tcs,
            ));
            println!("lowpass factor  [{bar}] min={:.4}  {}", limit, fail_mode);
        }

        // ── 8. Minimum volume (ZC-threshold floor; smaller = worse) ───
        for (bar, strict) in [("strict", true), ("loose", false)] {
            let probe = |vol: f64| {
                let sig = synthesize_ltc_signal(&tcs, 25.0, false, 48000, vol as f32);
                evaluate_gate(&sig, &tcs)
            };
            let (limit, _) = bisect_min_pass(probe, 0.12, 1.0, 0.004, 0.0002, 0.0002, strict);
            let fail_mode = describe_failure(&evaluate_gate(
                &synthesize_ltc_signal(&tcs, 25.0, false, 48000, (limit / 1.5).max(0.0002) as f32),
                &tcs,
            ));
            println!("min volume      [{bar}] min={:.4}  {}", limit, fail_mode);
        }

        // ── 9. Clock drift on a 24 s / 600-frame signal (both signs) ──
        let tcs_long: Vec<Timecode> = (0..600)
            .map(|i| Timecode {
                hours: (i / (25 * 60)) as u32,
                minutes: ((i / 25) % 60) as u32,
                seconds: (i % 25) as u32,
                frames: 0,
            })
            .collect();
        for sign in [1.0f64, -1.0] {
            for (bar, strict) in [("strict", true), ("loose", false)] {
                let probe = |ppm: f64| {
                    let sig = synthesize_ltc_signal_with_drift(
                        &tcs_long, 25.0, false, 48000, 0.5, sign * ppm,
                    );
                    evaluate_gate(&sig, &tcs_long)
                };
                let (limit, _) = bisect_max_pass(probe, 100.0, 1.0, 1000.0, 200_000.0, 100.0, strict);
                let fail_mode = describe_failure(&evaluate_gate(
                    &synthesize_ltc_signal_with_drift(
                        &tcs_long, 25.0, false, 48000, 0.5, sign * limit * 1.2,
                    ),
                    &tcs_long,
                ));
                println!(
                    "drift {}      [{bar}] max={:.0}ppm  {}",
                    if sign > 0.0 { "+ppm" } else { "-ppm" },
                    limit,
                    fail_mode
                );
            }
        }

        println!("\n=== sweep complete ===");
    }


    // ── evaluate_on_slice decomposition (pure variant math) ─────────────────

    #[test]
    fn spb_variants_low_resolution_is_nominal_only() {
        // Below the 8.0 spb threshold there is no drift window — only nominal.
        assert_eq!(spb_variants(4.0), vec![4.0]);
        assert_eq!(spb_variants(7.999), vec![7.999]);
    }

    #[test]
    fn spb_variants_high_resolution_spans_half_percent_in_five_steps() {
        let v = spb_variants(100.0);
        assert_eq!(v.len(), 5);
        // half_range = 100 * 0.004 = 0.4 → ±0.4 around nominal, symmetric.
        assert!((v[0] - 99.6).abs() < 1e-9, "first variant {}", v[0]);
        assert!((v[2] - 100.0).abs() < 1e-9, "middle variant is nominal");
        assert!((v[4] - 100.4).abs() < 1e-9, "last variant {}", v[4]);
        assert!(v.windows(2).all(|w| w[0] < w[1]), "variants must ascend");
    }

    #[test]
    fn spb_variants_small_spb_keeps_minimum_half_range() {
        // half_range floors at 0.05 so tiny-but-high-resolution spb still
        // gets a usable search window.
        let v = spb_variants(8.0);
        assert_eq!(v.len(), 5);
        assert!((v[0] - (8.0 - 0.05)).abs() < 1e-9, "first variant {}", v[0]);
        assert!((v[4] - (8.0 + 0.05)).abs() < 1e-9, "last variant {}", v[4]);
    }

    #[test]
    fn phase_window_clamps_quarter_spb_into_5_to_12() {
        assert_eq!(phase_window(8.0), 5, "8/4 = 2 → clamped up to 5");
        assert_eq!(phase_window(16.0), 5, "16/4 = 4 → clamped up to 5");
        assert_eq!(phase_window(24.0), 6, "24/4 = 6 → exact");
        assert_eq!(phase_window(100.0), 12, "100/4 = 25 → clamped down to 12");
        assert_eq!(phase_window(1000.0), 12);
    }

    #[test]
    fn phase_window_rounds_quarter_spb_before_clamping() {
        // 30/4 = 7.5 → rounds to 8 (banker's-unaware round-half-away).
        assert_eq!(phase_window(30.0), 8);
    }

    // ── Property tests (proptest): generative decode degradation ───────
    //
    // See plans/2026-10-05-proptest-targeted-adoption-plan.md. All randomness
    // flows from generated LCG seeds — fully deterministic per case, no
    // wall-clock, no IO. Floors are floors, not pins: calibrated from observed
    // margins minus tolerance, never equalities (no quality ceilings).
    mod prop {
        use super::*;
        use proptest::prelude::*;

        /// Canonical (fps, drop-frame) pairs — invalid pairings never generate.
        const FPS_PAIRS: &[(f64, bool)] = &[
            (24.0, false),
            (25.0, false),
            (29.97, false),
            (29.97, true),
            (30.0, false),
        ];

        fn arb_fps_pair() -> impl Strategy<Value = (f64, bool)> {
            proptest::sample::select(FPS_PAIRS.to_vec())
        }

        /// Frames 0 and 1 do not exist in DF minutes whose number is not a
        /// multiple of 10 (seconds == 0). Rejection rate ≈ 0.1 %.
        fn df_frame_valid(m: u32, s: u32, f: u32) -> bool {
            !(s == 0 && m % 10 != 0 && f < 2)
        }

        fn arb_start_tc(drop_frame: bool, biased_minutes: bool) -> impl Strategy<Value = Timecode> {
            let minutes = if biased_minutes {
                // DF-critical minute boundaries: 0 (no skip), 1/9 (skip on
                // entry), 10 (no skip), 59 (skip into next hour).
                proptest::sample::select(vec![0u32, 1, 9, 10, 59]).boxed()
            } else {
                (0u32..60).boxed()
            };
            (0u32..24, minutes, 0u32..60, 0u32..30)
                .prop_filter("drop-frame-invalid frame numbers rejected", move |&(_, m, s, f)| {
                    !drop_frame || df_frame_valid(m, s, f)
                })
                .prop_map(|(h, m, s, f)| Timecode { hours: h, minutes: m, seconds: s, frames: f })
        }

        #[derive(Clone, Debug)]
        struct PropSignal {
            start_tc: Timecode,
            fps: f64,
            drop_frame: bool,
            sample_rate: u32, // 44100 | 48000
            frames: usize,    // 8..=48
            volume: f32,
            noise_std: f32,  // 0.0 = none
            noise_seed: u64, // LCG seed — part of the generated input, so it shrinks & persists
            dc_offset: f32,
            impulse_prob: f32,
            impulse_amp: f32,
            impulse_seed: u64,
            drift_ppm: f64, // 0.0 = nominal
            hum_amp: f32,   // 0.0 = none
            hum_freq: f32,
        }

        fn arb_prop_signal(hostile: bool, df_biased: bool) -> BoxedStrategy<PropSignal> {
            // Calibrated by deterministic random-seed sweeps (800+ cases per
            // point), NOT by the seed-42 single-factor example margins: at
            // random seeds, value corruption (1-bit BCD slips) was observed at
            // noise ≈ 0.030 alone, ≈ 0.018 combined with impulses, and near
            // drift ≈ 300 ppm. The supported envelope below sits at observed
            // margins minus tolerance; the gap to the seed-42 example numbers
            // (0.09 noise, 500 ppm) is decoder-headroom signal fed back to the
            // WP-DR direction (see plans/2026-10-05-proptest-targeted-adoption
            // -plan.md, "Matters for further analysis").
            let (noise_max, drift_max, imp_prob_max, hum_amp_max): (f32, f64, f32, f32) =
                if hostile {
                    (0.35, 5000.0, 0.05, 0.15)
                } else {
                    (0.015, 150.0, 0.002, 0.0)
                };
            let fps_df = if df_biased {
                Just((29.97, true)).boxed()
            } else {
                arb_fps_pair().boxed()
            };
            let hum_amp = if hum_amp_max > 0.0 {
                (0.0f32..hum_amp_max).boxed()
            } else {
                Just(0.0f32).boxed()
            };
            (
                (fps_df, arb_start_tc(!df_biased, df_biased)),
                (proptest::sample::select(vec![44100u32, 48000]), 8usize..=48usize),
                (0.2f32..0.9f32, 0.0f32..noise_max, any::<u64>()),
                (-50i32..=50, 0.0f32..imp_prob_max, any::<u64>()),
                (-drift_max..drift_max),
                (hum_amp, proptest::sample::select(vec![50.0f32, 60.0])),
            )
                .prop_map(
                    move |(
                        ((fps, drop_frame), start_tc),
                        (sample_rate, frames),
                        (volume, noise_std, noise_seed),
                        (dc_milli, impulse_prob, impulse_seed),
                        drift_ppm,
                        (hum_amp, hum_freq),
                    )| {
                        PropSignal {
                            start_tc,
                            fps,
                            drop_frame,
                            sample_rate,
                            frames,
                            volume,
                            noise_std,
                            noise_seed,
                            dc_offset: dc_milli as f32 / 1000.0,
                            impulse_prob,
                            impulse_amp: 1.0,
                            impulse_seed,
                            drift_ppm,
                            hum_amp,
                            hum_freq,
                        }
                    },
                )
                .boxed()
        }

        impl PropSignal {
            /// Encoder-side differential oracle: the start TC advanced
            /// `frames − 1` times via the production increment (two
            /// independent implementations cross-checked, same style as the
            /// existing roundtrip fixtures).
            fn expected_sequence(&self) -> Vec<Timecode> {
                let mut tc = self.start_tc;
                let mut seq = vec![tc];
                for _ in 1..self.frames {
                    tc = crate::increment_timecode(&tc, self.fps, self.drop_frame);
                    seq.push(tc);
                }
                seq
            }

            /// Fixed composition order: drift synthesis → DC offset →
            /// Gaussian noise → impulses → hum.
            fn build(&self) -> Vec<f32> {
                let seq = self.expected_sequence();
                // Always the fractional-accurate synthesizer: the plain
                // helper truncates samples-per-bit to integers, which at
                // e.g. 44100 Hz / 24 fps injects ~44 000 ppm systematic
                // rate error — a harness artifact, not a clean signal.
                // drift_ppm == 0.0 yields the exact nominal rate.
                let mut signal = synthesize_ltc_signal_with_drift(
                    &seq, self.fps, self.drop_frame, self.sample_rate, self.volume, self.drift_ppm,
                );
                if self.dc_offset != 0.0 {
                    signal = add_dc_offset(&signal, self.dc_offset);
                }
                if self.noise_std > 0.0 {
                    signal = add_gaussian_noise(&signal, self.noise_std, self.noise_seed);
                }
                if self.impulse_prob > 0.0 {
                    signal = add_impulse_noise(&signal, self.impulse_prob, self.impulse_amp, self.impulse_seed);
                }
                if self.hum_amp > 0.0 {
                    signal = add_hum(&signal, self.sample_rate, self.hum_amp, self.hum_freq);
                }
                signal
            }

            fn decode(&self) -> LtcDetectionResult {
                decode_ltc_samples(
                    &self.build(),
                    self.sample_rate,
                    1,
                    self.fps,
                    self.drop_frame,
                    std::time::Instant::now(),
                    None,
                )
                .expect("decode_ltc_samples must not error on an in-process signal")
            }
        }

        /// Every decoded timecode must be an in-order subsequence of
        /// `expected` (no garbage values). A frame re-decoded at a corruption
        /// boundary may repeat the just-matched timecode; that is a reporting
        /// artifact, not a wrong value.
        fn assert_in_order_subsequence(result: &LtcDetectionResult, expected: &[Timecode]) {
            let mut exp_idx = 0usize;
            for ft in &result.timecodes {
                if exp_idx > 0 && expected[exp_idx - 1] == ft.timecode {
                    continue;
                }
                while exp_idx < expected.len() && expected[exp_idx] != ft.timecode {
                    exp_idx += 1;
                }
                assert!(
                    exp_idx < expected.len(),
                    "decoded out-of-sequence timecode {:?} (status={:?}, valid={}/{})",
                    ft.timecode, result.status, result.valid_frames, result.total_possible_frames
                );
                exp_idx += 1;
            }
        }

        /// Positional oracle: every decoded frame must carry the expected
        /// sequence value at its absolute time. Two tolerated reporting
        /// artifacts, both documented on `assert_ltc_fully_decoded` and
        /// compensated downstream:
        ///
        /// 1. A missing head frame — zero-crossing anchoring needs a
        ///    transition before the first bit boundary when the signal starts
        ///    at sample 0; `start_timecode_from_ltc` re-anchors via
        ///    first_ltc_timecode_secs + shift_timecode_back.
        /// 2. A frame re-decoded at a corruption boundary may repeat the
        ///    just-matched timecode (value of position idx−1 at position idx).
        ///
        /// Any other deviation — in particular a *wrong value* at a position
        /// (1-bit BCD slip) — fails the property.
        fn assert_positional(result: &LtcDetectionResult, expected: &[Timecode], fps: f64) {
            for ft in &result.timecodes {
                let idx = (ft.timecode_secs * fps).round() as isize;
                assert!(
                    idx >= 0 && (idx as usize) < expected.len(),
                    "decoded frame at {:.4}s outside expected sequence (value {:?})",
                    ft.timecode_secs, ft.timecode
                );
                let idx = idx as usize;
                let repeat_artifact = idx >= 1 && ft.timecode == expected[idx - 1];
                assert!(
                    repeat_artifact || ft.timecode == expected[idx],
                    "decoded {:?} at {:.4}s (position {}), expected {:?}",
                    ft.timecode, ft.timecode_secs, idx, expected[idx]
                );
            }
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(64))]

            // P5 — clean signal: full-fidelity decode contract.
            #[test]
            fn p5_clean_signal_decodes_consecutive_sequence(sig in arb_prop_signal(false, false)) {
                let expected = sig.expected_sequence();
                let result = sig.decode();
                assert!(matches!(result.status, LtcDecodeStatus::Success),
                    "expected Success, got {:?} (valid={}/{})",
                    result.status, result.valid_frames, result.total_possible_frames);
                // Floor calibrated: on a signal with no lead-in/lead-out the
                // zero-crossing sync scan can drop the first frame (no
                // transition before the first bit boundary) and/or the last
                // frame (no transition after the last one) — 2 frames worst
                // case. Interior frames of a clean signal are never lost.
                assert!(result.valid_frames as usize >= sig.frames.saturating_sub(2),
                    "valid {} < frames−2 ({} frames)", result.valid_frames, sig.frames);
                assert!(!result.timecodes.is_empty(), "no timecodes decoded");
                assert_positional(&result, &expected, sig.fps);
                assert_in_order_subsequence(&result, &expected);
            }

            // P6 — degraded envelope (noise ≤ 0.015, |dc| ≤ 0.05, impulses
            // ≤ 0.2 %, |drift| ≤ 150 ppm): same value-correctness contract.
            // Floor calibrated: worst observed frame loss across 800 random
            // cases was 7 (impulse bursts at low volume); pinned at observed
            // minus margin — a floor, not a pin, and a candidate to tighten as
            // decoder headroom improves.
            #[test]
            fn p6_degraded_envelope_still_decodes(sig in arb_prop_signal(false, false)) {
                let expected = sig.expected_sequence();
                let result = sig.decode();
                assert_ltc_ok(&result, sig.frames.saturating_sub(8) as u32);
                assert!(!result.timecodes.is_empty(), "no timecodes decoded");
                assert_positional(&result, &expected, sig.fps);
                assert_in_order_subsequence(&result, &expected);
                assert_in_order_subsequence(&result, &expected);
            }

            // P7 — hostile envelope (noise ≤ 0.35, |drift| ≤ 5000 ppm, hum,
            // impulses): crash-safety. Decode returns without panicking.
            // Value oracles are deliberately NOT enforced here: at impulse
            // probabilities near the envelope maximum, frames can misdecode
            // (bit errors inside frames that still pass sync-word detection),
            // so garbage values are legitimate garbage-in-garbage-out. Value
            // correctness within the supported envelope is enforced by
            // P5/P6/P8.
            #[test]
            fn p7_hostile_envelope_never_panics(sig in arb_prop_signal(true, false)) {
                let _result = sig.decode(); // Err → expect panics → property fails
            }

            // P8 — drop-frame minute-skip logic under the P6 degradation
            // envelope; start minutes biased to DF-critical values. Floor
            // calibrated: worst observed frame loss across the DF sweep was 1,
            // plus 2 for the head/tail zero-crossing boundary condition.
            #[test]
            fn p8_drop_frame_minute_skips_under_degradation(sig in arb_prop_signal(false, true)) {
                let expected = sig.expected_sequence();
                let result = sig.decode();
                assert_ltc_ok(&result, sig.frames.saturating_sub(3) as u32);
                assert!(!result.timecodes.is_empty(), "no timecodes decoded");
                assert_positional(&result, &expected, sig.fps);
                assert_in_order_subsequence(&result, &expected);
                assert_in_order_subsequence(&result, &expected);
            }
        }
    }
}
