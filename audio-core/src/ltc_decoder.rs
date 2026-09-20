use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

static OVERRIDE_SYNC_TOLERANCE: AtomicU32 = AtomicU32::new(0);

fn effective_sync_tolerance() -> u32 {
    let ov = OVERRIDE_SYNC_TOLERANCE.load(Ordering::Relaxed);
    if ov > 0 { ov } else { SYNC_MATCH_TOLERANCE }
}

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrameTimecode {
    pub frame_index: u32,
    pub timecode: Timecode,
    pub timecode_secs: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LtcDetectionResult {
    pub status: LtcDecodeStatus,
    pub detected_fps: f32,
    pub drop_frame: bool,
    pub total_possible_frames: u32,
    pub valid_frames: u32,
    pub timecodes: Vec<FrameTimecode>,
    pub avg_confidence: f32,
    pub details: Vec<String>,
    pub total_audio_duration_secs: f64,
    pub sample_rate: u32,
    pub processing_time_ms: f64,
    pub first_ltc_timecode_secs: f64,
    pub quality: Option<LtcQualityReport>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LtcQualityReport {
    /// Overall quality score 0.0–1.0
    pub score: f64,
    /// Human-readable grade: Excellent / Good / Fair / Poor / Bad
    pub grade: String,
    /// Number of undetected frames (total_possible - valid)
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
    /// Human-readable summary of issues found
    pub summary: String,
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
        }
    }
}

// Sync word at bits 64-79:  0 0 1 1 1 1 1 1 1 1 1 1 1 1 0 1
const SYNC_WORD: [u8; 16] = [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1];
const SYNC_OFFSET: usize = 64;

// ── Public API ───────────────────────────────────────────────────────────────

/// Decode LTC from a pre-loaded buffer of mono f32 samples.
/// This is the core decoding logic, extracted from `decode_ltc_from_wav`.
fn decode_ltc_samples_inner(
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
    fps: f64,
    drop_frame: bool,
    start: std::time::Instant,
) -> Result<LtcDetectionResult, String> {
    if samples.is_empty() {
        warn!("LTC decode: audio buffer contains no samples");
        return Ok(LtcDetectionResult::error("Audio buffer contains no samples"));
    }

    let total_duration = samples.len() as f64 / sample_rate as f64;
    let noise_floor = estimate_noise_floor(samples);
    let threshold = (noise_floor * 0.5).max(0.005);
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

    if zc.len() < 8 {
        warn!("LTC decode: only {} zero-crossings, signal may not be LTC", zc.len());
        return Ok(LtcDetectionResult::error(format!(
            "Only {} zero-crossings found (need ≥8) -- signal may be silent or not LTC audio",
            zc.len()
        )));
    }

    // ── Try ZC-interval method (fast, works on synthetic/clean LTC) ─────────
    let zc_result = try_decode_via_zc_intervals(&zc, sample_rate, fps, drop_frame);
    let zc_conf = zc_result.as_ref().map_or(0.0, |r| {
        if r.total_possible > 0 { r.valid_frames as f32 / r.total_possible as f32 } else { 0.0 }
    });

    match zc_result.as_ref() {
        Some(r) => info!("LTC decode: ZC-interval result -- {} valid / {} possible ({:.1}%)",
            r.valid_frames, r.total_possible, zc_conf * 100.0),
        None => debug!("LTC decode: ZC-interval returned no frames"),
    }

    if zc_conf >= 0.70 {
        info!("LTC decode: ZC-interval confidence {:.1}% >= 70% -- using directly", zc_conf * 100.0);
        return build_result(zc_result, &zc, sample_rate, threshold, channels, total_duration, start);
    }

    // ── Sliding window search for SPB/phase ─────────────────────────────────
    // Evaluates 30s windows at 15s strides, using ZCs to skip silent regions.
    // First window with >=70% confidence -> single-pass extract_bits on full file.
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
        let window_start = window_idx * stride;
        let window_end = (window_start + window_len).min(samples.len());

        let window_zc_abs = zc_in_range(&zc, window_start, window_end);
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

        let (result, valid) = evaluate_on_slice(
            &samples[window_start..window_end], &window_zc,
            sample_rate, threshold, fps, drop_frame,
        );

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
                let decoded = decode_full_file(samples, r, threshold, sample_rate, best_window_start, &zc);
                let zc_better = zc_result.as_ref().is_some_and(|zcr| {
                    zcr.valid_frames > decoded.valid_frames
                });
                if zc_better {
                    let zcr = zc_result.as_ref().unwrap();
                    info!("LTC decode: ZC-interval ({}/{}) beats detailed scan ({}/{}) -- using ZC-interval",
                        zcr.valid_frames, zcr.total_possible, decoded.valid_frames, decoded.total_possible);
                }
                let final_r = if zc_better { zc_result } else { Some(decoded) };
                return build_result(final_r, &zc,
                    sample_rate, threshold, channels, total_duration, start);
            }
        }

        if window_end >= samples.len() { break; }
    }

    // ── Decode full file with best window parameters ────────────────────────
    if let Some(ref r) = best_window_result {
        let conf = r.valid_frames as f32 / r.total_possible.max(1) as f32;
        info!("LTC decode: best window eval {:.1}% ({} valid) -- single-pass on full file (spb={:.2}, phase={})",
            conf * 100.0, r.valid_frames, r.spb, r.phase);
        let decoded = decode_full_file(samples, r, threshold, sample_rate, best_window_start, &zc);
        let zc_better = zc_result.as_ref().is_some_and(|zcr| {
            zcr.valid_frames > decoded.valid_frames
        });
        if zc_better {
            let zcr = zc_result.as_ref().unwrap();
            info!("LTC decode: ZC-interval ({}/{}) beats detailed scan ({}/{}) -- using ZC-interval",
                zcr.valid_frames, zcr.total_possible, decoded.valid_frames, decoded.total_possible);
        }
        let final_r = if zc_better { zc_result } else { Some(decoded) };
        return build_result(final_r, &zc,
            sample_rate, threshold, channels, total_duration, start);
    }

    // ── Fallback: full-file evaluate_on_slice (rare) ────────────────────────
    warn!("LTC decode: sliding window found no valid LTC -- full-file eval fallback");
    let (fallback_result, _) = evaluate_on_slice(samples, &zc, sample_rate, threshold, fps, drop_frame);
    let zc_better = zc_result.as_ref().is_some_and(|zcr| {
        fallback_result.as_ref().map_or(true, |fr| zcr.valid_frames > fr.valid_frames)
    });
    if zc_better {
        let zcr = zc_result.as_ref().unwrap();
        info!("LTC decode: ZC-interval ({}/{}) beats fallback scan ({}/{}) -- using ZC-interval",
            zcr.valid_frames, zcr.total_possible,
            fallback_result.as_ref().map_or(0, |fr| fr.valid_frames),
            fallback_result.as_ref().map_or(0, |fr| fr.total_possible));
    }
    let final_r = if zc_better { zc_result } else { fallback_result };
    build_result(final_r, &zc, sample_rate, threshold, channels, total_duration, start)
}

/// Public entry point for LTC decode. Wraps the inner decoder.
pub fn decode_ltc_samples(
    samples: &[f32],
    sample_rate: u32,
    channels: usize,
    fps: f64,
    drop_frame: bool,
    start: std::time::Instant,
) -> Result<LtcDetectionResult, String> {
    decode_ltc_samples_inner(samples, sample_rate, channels, fps, drop_frame, start)
}

pub fn decode_ltc_from_wav(path: &Path, fps: f64, drop_frame: bool) -> Result<LtcDetectionResult, String> {
    let start = std::time::Instant::now();

    let mut reader = hound::WavReader::open(path)
        .map_err(|e| format!("Failed to open WAV file: {}", e))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as usize;

    info!("Decoding LTC from: {} ({} Hz, {} ch, {} fps)", path.display(), sample_rate, channels, fps);

    info!("LTC decode (+{:.1}s): reading audio samples from disk...", start.elapsed().as_secs_f64());
    let samples = read_mono_samples(&mut reader, &spec)
        .map_err(|e| format!("Failed to read audio samples: {}", e))?;

    drop(reader);

    decode_ltc_samples(&samples, sample_rate, channels, fps, drop_frame, start)
}

/// Evaluate LTC on a slice using the given FPS.
///
/// Tries SPB variants to compensate for clock drift, with phases derived
/// from the first several zero-crossings.
fn evaluate_on_slice(
    samples: &[f32],
    zc: &[usize],
    sample_rate: u32,
    threshold: f32,
    fps: f64,
    drop_frame: bool,
) -> (Option<ScoredResult>, u32) {
    let eval_start = std::time::Instant::now();
    let mut best_valid = 0u32;
    let mut best_result: Option<ScoredResult> = None;

    let fps_name = format!("{:.2} fps", fps);
    let spb_nominal = sample_rate as f64 / (fps * 80.0);
    if spb_nominal < 0.5 {
        return (None, 0);
    }
    debug!("LTC evaluate: trying {} (spb_nominal={:.2})", fps_name, spb_nominal);

    let spb_variants = if spb_nominal >= 8.0 {
        let half_range = (spb_nominal * 0.004).max(0.05);
        (0..5)
            .map(|i| { let t = i as f64 / 4.0; spb_nominal + (t - 0.5) * 2.0 * half_range })
            .collect::<Vec<_>>()
    } else {
        vec![spb_nominal]
    };

    for (spb_idx, &spb) in spb_variants.iter().enumerate() {
        let max_phases = (spb / 4.0).round() as usize;
        let phases_to_try = zc.iter().take(max_phases.clamp(5, 12)).copied();

        let half_spb = (spb * 0.5) as usize;
        let mut attempts_this_spb = 0u32;
        debug!("LTC evaluate: SPB variant {}/{} -- spb={:.2} ({} phases)",
            spb_idx + 1, spb_variants.len(), spb, max_phases.clamp(5, 12) * 2);

        for phase in phases_to_try {
            for &candidate_phase in &[phase, phase.saturating_sub(half_spb)] {
                let bits = extract_bits(samples, spb, candidate_phase, threshold);
                if bits.len() < 80 { continue; }
                attempts_this_spb += 1;

                let (valid_frames, total_possible, frame_starts) = find_frames(&bits);
                if valid_frames > best_valid {
                    let timecodes: Vec<FrameTimecode> = frame_starts
                        .iter()
                        .enumerate()
                        .map(|(idx, &start)| FrameTimecode {
                            frame_index: idx as u32,
                            timecode: decode_timecode_from_bits(&bits, start),
                            timecode_secs: (candidate_phase as f64 + start as f64 * spb) / sample_rate as f64,
                        })
                        .collect();

                    debug!("LTC extract-bits: {} spb={:.2} phase={} -- {} valid / {} possible (new best)",
                        fps_name, spb, candidate_phase, valid_frames, total_possible);

                    best_valid = valid_frames;
                    best_result = Some(ScoredResult {
                        fps,
                        drop_frame,
                        valid_frames,
                        total_possible,
                        timecodes,
                        details_entry: format!(
                            "{}: {} valid / {} possible frames (spb={:.2}, phase={})",
                            fps_name, valid_frames, total_possible, spb, candidate_phase
                        ),
                        spb,
                        phase: candidate_phase,
                        frame_starts,
                    });
                }
            }
        }
        debug!("LTC evaluate: SPB variant {}/{} done -- {} attempts in {:.1}s, best={} valid",
            spb_idx + 1, spb_variants.len(),
            attempts_this_spb, eval_start.elapsed().as_secs_f64(), best_valid);
    }

    if let Some(ref best) = best_result.clone() {
        debug!("LTC evaluate (+{:.1}s): refinement phase for best candidate ({} fps, spb={:.2}, best_valid={})",
            eval_start.elapsed().as_secs_f64(), best.fps, best.spb, best_valid);
        let best_spb = best.spb;
        let best_fps = best.fps;
        let best_drop_frame = best.drop_frame;
        let best_phase = best.phase;

let max_phases = (best_spb / 4.0).round() as usize;
        let phases_to_try = zc.iter().take(max_phases.clamp(5, 12)).copied();
        let mut last_heartbeat = std::time::Instant::now();
        let mut refine_idx = 0u32;
        for phase in phases_to_try {
            if phase == best_phase { continue; }
            refine_idx += 1;
            if last_heartbeat.elapsed().as_secs_f64() >= 10.0 {
                debug!("LTC refine (+{:.1}s): phase {}/{} (phase={}), best_valid={}",
                    eval_start.elapsed().as_secs_f64(), refine_idx, max_phases.clamp(5, 12) - 1,
                    phase, best_valid);
                last_heartbeat = std::time::Instant::now();
            }
            let bits = extract_bits_adaptive(samples, best_spb, phase, threshold, zc);
            if bits.len() < 80 { continue; }

            let (valid_frames, total_possible, frame_starts) = find_frames(&bits);
            if valid_frames > best_valid {

                debug!("LTC refinement adaptive -- {} valid / {} possible (phase={})",
                    valid_frames, total_possible, phase);

                best_valid = valid_frames;
                best_result = Some(ScoredResult {
                    fps: best_fps,
                    drop_frame: best_drop_frame,
                    valid_frames,
                    total_possible,
                    timecodes: frame_starts
                        .iter()
                        .enumerate()
                        .map(|(idx, &start)| FrameTimecode {
                            frame_index: idx as u32,
                            timecode: decode_timecode_from_bits(&bits, start),
                            timecode_secs: (phase as f64 + start as f64 * best_spb) / sample_rate as f64,
                        })
                        .collect(),
                    details_entry: format!(
                        "{}: {} valid / {} possible frames (adaptive, phase={}, spb={:.2})",
                        fps_name, valid_frames, total_possible, phase, best_spb
                    ),
                    spb: best_spb,
                    phase,
                    frame_starts,
                });
            }
        }
    }

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
    best_result: Option<ScoredResult>,
    zc: &[usize],
    sample_rate: u32,
    threshold: f32,
    channels: usize,
    total_duration: f64,
    start: std::time::Instant,
) -> Result<LtcDetectionResult, String> {
    let elapsed = start.elapsed();
    let processing_time_ms = elapsed.as_secs_f64() * 1000.0;

let mut result = match best_result {
        Some(r) => {
            let confidence = if r.total_possible > 0 {
                r.valid_frames as f32 / r.total_possible as f32
            } else {
                0.0
            };

            let status = if confidence >= 0.70 {
                LtcDecodeStatus::Success
            } else if confidence >= 0.30 {
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

            let first_ltc_timecode_secs = if r.valid_frames > 0 && !r.frame_starts.is_empty() {
                (r.phase as f64 + r.frame_starts[0] as f64 * r.spb) / sample_rate as f64
            } else {
                0.0
            };

            let tc0_secs = timecodes.first().map(|t| t.timecode_secs).unwrap_or(-1.0);
            let diff_with_tc0 = (first_ltc_timecode_secs - tc0_secs).abs();
            if diff_with_tc0 > 0.001 && r.valid_frames > 0 {
                warn!(
                    "LTC decode: first_ltc_timecode_secs ({:.6}s) differs from timecodes[0].timecode_secs ({:.6}s) by {:.6}s",
                    first_ltc_timecode_secs, tc0_secs, diff_with_tc0
                );
            }

            info!(
                "LTC decode result: status={:?}, fps={:.2}, valid={}/{}, confidence={:.1}%, first_offset={:.3}s, tc[0]={:.3}s, processing={:.0}ms",
                status, r.fps, r.valid_frames, r.total_possible, confidence * 100.0,
                first_ltc_timecode_secs, tc0_secs, processing_time_ms,
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
            }
        }
    };

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
pub fn apply_coherent_first_timecode(result: &mut LtcDetectionResult) {
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

/// Compute a quality report for a decoded LTC sequence.
///
/// Returns `None` when there are no decoded timecodes to analyze.
/// Otherwise compares LTC timecode values against audio positions to detect
/// gaps, glitches, edit points, and clock drift.
pub fn compute_ltc_quality(result: &LtcDetectionResult) -> Option<LtcQualityReport> {
    let timecodes = &result.timecodes;
    let valid = result.valid_frames;
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

    // Find contiguous segments (frame_index increments by 1)
    let mut segments: Vec<std::ops::Range<usize>> = Vec::new();
    let mut seg_start = 0;
    for i in 1..n {
        if timecodes[i].frame_index != timecodes[i - 1].frame_index + 1 {
            segments.push(seg_start..i);
            seg_start = i;
        }
    }
    segments.push(seg_start..n);

    let _seg_count = segments.len();
    let total_possible = result.total_possible_frames.max(valid) as f64;
    let missing_frames = if total_possible > 0.0 {
        (total_possible - valid as f64).max(0.0) as u32
    } else {
        0
    };

    // Largest contiguous block
    let largest_block = segments.iter().map(|s| (s.end - s.start) as u32).max().unwrap_or(0);

    // Analyze gaps between segments and detect edits
    let mut gap_count: u32 = 0;
    let mut edit_count: u32 = 0;

    for w in segments.windows(2) {
        let prev = &w[0];
        let cur = &w[1];
        let i_prev = prev.end - 1;
        let i_cur = cur.start;

        gap_count += 1;

        let audio_elapsed = audio_secs[i_cur] - audio_secs[i_prev];
        let ltc_elapsed = ltc_secs[i_cur] - ltc_secs[i_prev];
        let diff = (audio_elapsed - ltc_elapsed).abs();
        let frame_threshold = 2.0 / fps;

        if diff > frame_threshold {
            edit_count += 1;
        }
    }

    // Detect glitch frames within contiguous segments
    let mut glitch_count: u32 = 0;
    let frame_2_threshold = 2.0 / fps;

    for seg in &segments {
        let seg_len = seg.end - seg.start;
        if seg_len < 3 {
            continue;
        }
        for i in (seg.start + 1)..(seg.end - 1) {
            let expected = (ltc_secs[i - 1] + ltc_secs[i + 1]) / 2.0;
            if (ltc_secs[i] - expected).abs() > frame_2_threshold {
                glitch_count += 1;
            }
        }
    }

    // Compute overall drift rate and max drift via linear fit
    let max_drift_secs = drift.iter().map(|d| d.abs()).fold(0.0f64, f64::max);

    let drift_rate = if n >= 2 && (audio_secs[n - 1] - audio_secs[0]).abs() > 1e-6 {
        (drift[n - 1] - drift[0]) / (audio_secs[n - 1] - audio_secs[0])
    } else {
        0.0
    };

    // Calculate score (0.0 - 1.0)
    let missing_ratio = if total_possible > 0.0 {
        missing_frames as f64 / total_possible
    } else {
        0.0
    };

    let mut score = 1.0;

    if missing_ratio > 0.05 {
        score -= 0.15 * (missing_ratio / 0.5).min(1.0);
    }

    score -= 0.03 * (gap_count as f64).min(5.0);
    score -= 0.03 * (glitch_count as f64).min(5.0);

    if edit_count > 0 {
        score -= 0.30;
        if edit_count > 1 {
            score -= 0.10 * (edit_count - 1) as f64;
        }
    }

    let drift_rate_fps = drift_rate.abs() * fps;
    if drift_rate_fps > 0.5 {
        score -= 0.05 * (drift_rate_fps / 5.0).min(1.0);
    }

    let max_drift_frames = max_drift_secs * fps;
    if max_drift_frames > 3.0 {
        score -= 0.10 * (max_drift_frames / 10.0).min(1.0);
    }

    score = score.clamp(0.0, 1.0);

    // Grade
    let grade = if score >= 0.95 {
        "Excellent".to_string()
    } else if score >= 0.80 {
        "Good".to_string()
    } else if score >= 0.60 {
        "Fair".to_string()
    } else if score >= 0.30 {
        "Poor".to_string()
    } else {
        "Bad".to_string()
    };

    // Build summary
    let mut parts: Vec<String> = Vec::new();
    if missing_frames > 0 {
        parts.push(format!("{} missing frame(s)", missing_frames));
    }
    if gap_count > 0 {
        parts.push(format!("{} gap(s)", gap_count));
    }
    if glitch_count > 0 {
        parts.push(format!("{} glitch(es)", glitch_count));
    }
    if edit_count > 0 {
        parts.push(format!("{} edit point(s)", edit_count));
    }
    if drift_rate_fps > 0.5 {
        parts.push(format!("drift {:.3} s/s", drift_rate));
    }
    if max_drift_frames > 1.0 {
        parts.push(format!("max drift {:.2}s", max_drift_secs));
    }

    let summary = if parts.is_empty() {
        "No issues detected — all frames contiguous and in sync".to_string()
    } else {
        parts.join(", ")
    };

    Some(LtcQualityReport {
        score,
        grade,
        missing_frames,
        gap_count,
        glitch_count,
        edit_count,
        max_drift_secs,
        drift_rate,
        largest_block,
        summary,
    })
}

pub fn quick_check_ltc(path: &Path) -> Result<bool, String> {
    let result = decode_ltc_from_wav(path, 25.0, false)?;
    Ok(matches!(result.status, LtcDecodeStatus::Success))
}

// ── Reading ──────────────────────────────────────────────────────────────────

fn read_mono_samples<R: std::io::Read>(
    reader: &mut hound::WavReader<R>,
    spec: &hound::WavSpec,
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
                .filter(|(i, _)| i % channels == 0)
                .map(|(_, s)| s as f32 / max_val)
                .collect();
            Ok(samples)
        }
        hound::SampleFormat::Float => {
            let samples: Vec<f32> = reader
                .samples::<f32>()
                .filter_map(|s| s.ok())
                .enumerate()
                .filter(|(i, _)| i % channels == 0)
                .map(|(_, s)| s)
                .collect();
            Ok(samples)
        }
    }
}

// ── Noise floor estimation ───────────────────────────────────────────────────

fn estimate_noise_floor(samples: &[f32]) -> f32 {
    let total = samples.len();
    if total == 0 {
        return 1e-10;
    }
    let window_count = 4usize;
    let window_size = (total / window_count).max(100).min(10_000);
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

    if short_ratio > 0.10 {
        decode_bits_real_zc(zc, spb)
    } else {
        decode_bits_synthetic_zc(zc, spb)
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
) -> Option<ScoredResult> {
    if zc.len() < 8 {
        return None;
    }

    let bits = decode_bits_from_zero_crossings(zc, sample_rate, fps);
    if bits.len() < 80 {
        return None;
    }

    let (valid_frames, total_possible, frame_starts) = find_frames(&bits);
    if valid_frames == 0 {
        return None;
    }

    let spb = sample_rate as f64 / (fps * 80.0);
    let fps_name = format!("{:.2} fps", fps);

    let timecodes: Vec<FrameTimecode> = frame_starts
        .iter()
        .enumerate()
        .map(|(idx, &start)| {
            let timecode = decode_timecode_from_bits(&bits, start);
            let timecode_secs = (zc[0] as f64 + start as f64 * spb) / sample_rate as f64;
            FrameTimecode {
                frame_index: idx as u32,
                timecode,
                timecode_secs,
            }
        })
        .collect();

    let details_entry = format!(
        "{}: {} valid / {} possible frames (ZC-interval)",
        fps_name, valid_frames, total_possible
    );

    Some(ScoredResult {
        fps,
        drop_frame,
        valid_frames,
        total_possible,
        timecodes,
        details_entry,
        spb,
        phase: zc[0],
        frame_starts,
    })
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

fn extract_bits(samples: &[f32], samples_per_bit: f64, phase: usize, _threshold: f32) -> Vec<u8> {
    let mut bits = Vec::new();
    let quarter = samples_per_bit * 0.25;
    let three_quarter = samples_per_bit * 0.75;
    let mut pos = phase as f64;

    while (pos + samples_per_bit) as usize <= samples.len() {
        let p25 = (pos + quarter) as usize;
        let p75 = (pos + three_quarter) as usize;

        if p75 >= samples.len() {
            break;
        }

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

    while (pos + samples_per_bit) as usize <= samples.len() {
        let p25 = (pos + quarter) as usize;
        let p75 = (pos + three_quarter) as usize;

        if p75 >= samples.len() {
            break;
        }

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
    let tolerance = effective_sync_tolerance();
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

fn find_frames(bits: &[u8]) -> (u32, u32, Vec<usize>) {
    let mut sync_positions = Vec::new();
    if bits.len() < 16 {
        return (0, 0, Vec::new());
    }
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

    if sync_positions.is_empty() {
        return (0, 0, Vec::new());
    }

    let mut alignment_scores = vec![0u32; 80];
    for &sp in &sync_positions {
        if sp >= SYNC_OFFSET {
            let alignment = (sp - SYNC_OFFSET) % 80;
            alignment_scores[alignment] += 1;
        }
    }

    let (best_alignment, _best_count_value) = alignment_scores
        .iter()
        .enumerate()
        .max_by_key(|&(_, &c)| c)
        .unwrap_or((0, &0));

    if alignment_scores[0] == 0 && alignment_scores.iter().all(|&c| c == 0) {
        return (0, 0, Vec::new());
    }

    let total_possible = if bits.len() > best_alignment {
        ((bits.len() - best_alignment) / 80) as u32
    } else {
        0
    };

    let mut frame_starts = Vec::new();
    let align = best_alignment;
    for idx in 0.. {
        let frame_start = align + idx * 80;
        if frame_start + 80 > bits.len() {
            break;
        }
        let sync_start = frame_start + SYNC_OFFSET;
        if sync_start + 16 <= bits.len()
            && bits_hamming_distance_16(&bits[sync_start..sync_start + 16]) <= effective_sync_tolerance()
        {
            frame_starts.push(frame_start);
        }
    }

    (frame_starts.len() as u32, total_possible, frame_starts)
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

#[derive(Clone)]
struct ScoredResult {
    fps: f64,
    drop_frame: bool,
    valid_frames: u32,
    total_possible: u32,
    timecodes: Vec<FrameTimecode>,
    details_entry: String,
    spb: f64,
    phase: usize,
    frame_starts: Vec<usize>,
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
) -> ScoredResult {
    let absolute_phase = params.phase + phase_offset;

    let bits_nominal = extract_bits(samples, params.spb, absolute_phase, threshold);
    let (valid_nominal, total_possible, frame_starts_nominal) = find_frames(&bits_nominal);

    let bits_adaptive = extract_bits_adaptive(samples, params.spb, absolute_phase, threshold, zero_crossings);
    let (valid_adaptive, total_possible_adaptive, frame_starts_adaptive) = find_frames(&bits_adaptive);

    let (use_adaptive, valid_frames, total_possible, frame_starts, bits) = if valid_adaptive > valid_nominal {
        (true, valid_adaptive, total_possible_adaptive, frame_starts_adaptive, bits_adaptive)
    } else {
        (false, valid_nominal, total_possible, frame_starts_nominal, bits_nominal)
    };

    let timecodes: Vec<FrameTimecode> = frame_starts
        .iter()
        .enumerate()
        .map(|(idx, &start)| FrameTimecode {
            frame_index: idx as u32,
            timecode: decode_timecode_from_bits(&bits, start),
            timecode_secs: (absolute_phase as f64 + start as f64 * params.spb) / sample_rate as f64,
        })
        .collect();
    let method = if use_adaptive { "adaptive" } else { "nominal" };
    ScoredResult {
        fps: params.fps,
        drop_frame: params.drop_frame,
        valid_frames,
        total_possible,
        timecodes,
        details_entry: format!(
            "{:.2} fps: {} valid / {} possible frames (single-pass {method}, spb={:.2}, phase={})",
            params.fps, valid_frames, total_possible, params.spb, absolute_phase
        ),
        spb: params.spb,
        phase: absolute_phase,
        frame_starts,
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::{generate_ltc_frame_stereo, increment_timecode, Timecode};

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

    #[test]
    fn test_get_ltc_bits_sync_word_various_tc() {
        for tc in [
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            Timecode { hours: 23, minutes: 59, seconds: 59, frames: 29 },
            Timecode { hours: 12, minutes: 34, seconds: 56, frames: 18 },
        ] {
            let bits = crate::get_ltc_bits(&tc, false);
            let sync = &bits[SYNC_OFFSET..SYNC_OFFSET + 16];
            assert_eq!(sync, SYNC_WORD, "sync word must be invariant for {:?}", tc);
        }
    }

    #[test]
    fn test_get_ltc_bits_sync_word_80_bits() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        assert_eq!(bits.len(), 80, "LTC frame must be exactly 80 bits");
    }

    // ── get_ltc_bits: known timecode values ──────────────────────────────

    #[test]
    fn test_get_ltc_bits_frame_04() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 4 };
        let bits = crate::get_ltc_bits(&tc, false);
        assert_eq!(bits[0], 0);
        assert_eq!(bits[1], 0);
        assert_eq!(bits[2], 1);
        assert_eq!(bits[3], 0);
        assert_eq!(bits[8], 0);
        assert_eq!(bits[9], 0);
    }

    #[test]
    fn test_get_ltc_bits_frame_13() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 13 };
        let bits = crate::get_ltc_bits(&tc, false);
        assert_eq!(bits[0], 1);
        assert_eq!(bits[1], 1);
        assert_eq!(bits[2], 0);
        assert_eq!(bits[3], 0);
        assert_eq!(bits[8], 1);
        assert_eq!(bits[9], 0);
    }

    #[test]
    fn test_get_ltc_bits_drop_frame_flag() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits_nd = crate::get_ltc_bits(&tc, false);
        let bits_df = crate::get_ltc_bits(&tc, true);
        assert_eq!(bits_nd[10], 0, "non-drop must have bit 10 = 0");
        assert_eq!(bits_df[10], 1, "drop-frame must have bit 10 = 1");
    }

    #[test]
    fn test_get_ltc_bits_color_frame_zero() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        assert_eq!(bits[11], 0, "color frame flag must be 0");
    }

    // ── decode_timecode_from_bits round-trip ─────────────────────────────

    #[test]
    fn test_decode_timecode_from_bits_roundtrip_zero() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        let decoded = decode_timecode_from_bits(&bits, 0);
        assert_eq!(decoded, tc);
    }

    #[test]
    fn test_decode_timecode_from_bits_roundtrip_typical() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let bits = crate::get_ltc_bits(&tc, false);
        let decoded = decode_timecode_from_bits(&bits, 0);
        assert_eq!(decoded, tc);
    }

    #[test]
    fn test_decode_timecode_from_bits_roundtrip_max() {
        let tc = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 29 };
        let bits = crate::get_ltc_bits(&tc, false);
        let decoded = decode_timecode_from_bits(&bits, 0);
        assert_eq!(decoded, tc);
    }

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
    fn test_increment_timecode_basic() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let next = increment_timecode(&tc, 25.0, false);
        assert_eq!(next, Timecode { hours: 0, minutes: 0, seconds: 0, frames: 1 });
    }

    #[test]
    fn test_increment_timecode_frame_rollover_25fps() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 24 };
        let next = increment_timecode(&tc, 25.0, false);
        assert_eq!(next, Timecode { hours: 0, minutes: 0, seconds: 1, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_frame_rollover_30fps() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 29 };
        let next = increment_timecode(&tc, 30.0, false);
        assert_eq!(next, Timecode { hours: 0, minutes: 0, seconds: 1, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_second_rollover() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 59, frames: 24 };
        let next = increment_timecode(&tc, 25.0, false);
        assert_eq!(next, Timecode { hours: 0, minutes: 1, seconds: 0, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_minute_rollover() {
        let tc = Timecode { hours: 0, minutes: 59, seconds: 59, frames: 24 };
        let next = increment_timecode(&tc, 25.0, false);
        assert_eq!(next, Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_hour_rollover() {
        let tc = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 24 };
        let next = increment_timecode(&tc, 25.0, false);
        assert_eq!(next, Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_24fps_rollover() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 23 };
        let next = increment_timecode(&tc, 24.0, false);
        assert_eq!(next, Timecode { hours: 0, minutes: 0, seconds: 1, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_drop_frame_skip() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 59, frames: 29 };
        let next = increment_timecode(&tc, 29.97, true);
        assert_eq!(next, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 2 });
    }

    #[test]
    fn test_increment_timecode_drop_frame_no_skip_div10() {
        let tc = Timecode { hours: 1, minutes: 9, seconds: 59, frames: 29 };
        let next = increment_timecode(&tc, 29.97, true);
        assert_eq!(next, Timecode { hours: 1, minutes: 10, seconds: 0, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_drop_frame_sequential() {
        let mut tc = Timecode { hours: 1, minutes: 1, seconds: 0, frames: 0 };
        tc = increment_timecode(&tc, 29.97, true);
        assert_eq!(tc, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 1 });
        tc = increment_timecode(&tc, 29.97, true);
        assert_eq!(tc, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 2 });
    }

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
        let bits = extract_bits(&signal, spb, 0, 0.01);
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
        let bits = extract_bits(&signal, spb, 0, 0.01);
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
        let bits = extract_bits(&signal, spb, 0, 0.01);
        assert_eq!(bits, vec![0, 1, 0, 1]);
    }

    #[test]
    fn test_extract_bits_below_threshold() {
        let spb = 8.0;
        let signal = vec![0.0f32; (spb as usize) * 3];
        let bits = extract_bits(&signal, spb, 0, 0.1);
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
        let bits = extract_bits(&padded, spb, 3, 0.01);
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
        let (valid, total, starts) = find_frames(&bits);
        assert_eq!(valid, 1);
        assert_eq!(total, 1);
        assert_eq!(starts, vec![0]);
    }

    #[test]
    fn test_find_frames_two_frames() {
        let mut bits = build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 });
        bits.extend(build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: 1 }));
        let (valid, _total, starts) = find_frames(&bits);
        assert_eq!(valid, 2);
        assert_eq!(starts, vec![0, 80]);
    }

    #[test]
    fn test_find_frames_no_sync_word() {
        let bits = vec![0u8; 160];
        let (valid, _total, starts) = find_frames(&bits);
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
        let (valid, _, _) = find_frames(&bits);
        assert!(valid <= 2, "random bits should produce at most 2 false sync word matches, got {}", valid);
    }

    #[test]
    fn test_find_frames_short_buffer() {
        let bits = vec![0u8; 10];
        let (valid, total, starts) = find_frames(&bits);
        assert_eq!(valid, 0);
        assert_eq!(total, 0);
        assert!(starts.is_empty());
    }

    #[test]
    fn test_find_frames_alignment_matters() {
        let mut bits = vec![0u8; 160];
        bits[0..16].copy_from_slice(&SYNC_WORD);
        let (valid, _, _) = find_frames(&bits);
        assert_eq!(valid, 0, "sync word at wrong offset should not produce valid frames");
    }

    // ── WAV generation helper ────────────────────────────────────────────

    #[allow(clippy::too_many_arguments)]
    fn generate_test_wav(
        path: &Path,
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        channel: &str,
        volume: f32,
        sample_rate: u32,
        duration_secs: f64,
    ) {
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
            generate_ltc_frame_stereo(
                &tc,
                drop_frame,
                samples_per_frame,
                samples_per_bit,
                volume,
                channel,
                &mut last_level,
                &mut frame_buf[..samples_per_frame * 2],
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

    #[allow(clippy::too_many_arguments)]
    fn generate_test_wav_with_prefix(
        path: &Path,
        silent_prefix_secs: f64,
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        channel: &str,
        volume: f32,
        sample_rate: u32,
        duration_secs: f64,
    ) {
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
            generate_ltc_frame_stereo(
                &tc,
                drop_frame,
                samples_per_frame,
                samples_per_bit,
                volume,
                channel,
                &mut last_level,
                &mut frame_buf[..samples_per_frame * 2],
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

    fn verify_roundtrip(
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        channel: &str,
        volume: f32,
        sample_rate: u32,
        duration_secs: f64,
    ) -> LtcDetectionResult {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test_ltc.wav");
        generate_test_wav(&path, start_tc, fps, drop_frame, channel, volume, sample_rate, duration_secs);
        decode_ltc_from_wav(&path, fps, drop_frame).unwrap()
    }

    // ── WAV round-trip tests ─────────────────────────────────────────────

    #[test]
    fn test_wav_roundtrip_25fps() {
        let result = verify_roundtrip(
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 48, "expected ~50 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_24fps() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            24.0, false, "both", 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 46, "expected ~48 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_30fps() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            30.0, false, "both", 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 58, "expected ~60 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_2997_nd() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            29.97, false, "both", 0.5, 48000, 3.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for 29.97 ND, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_2997_df() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            29.97, true, "both", 0.5, 48000, 3.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for 29.97 DF, got {:?} (valid={})",
            result.status, result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_different_start_tc() {
        let result = verify_roundtrip(
            Timecode { hours: 10, minutes: 15, seconds: 30, frames: 12 },
            25.0, false, "both", 0.5, 48000, 1.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(result.valid_frames >= 22, "expected ~25 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_44khz() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.5, 44100, 2.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error at 44kHz, got {:?}", result.status);
    }

    #[test]
    fn test_wav_roundtrip_48khz() {
        let result = verify_roundtrip(
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.5, 48000, 2.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success at 48kHz, got {:?}", result.status);
    }

    #[test]
    fn test_wav_roundtrip_left_channel() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "left", 0.5, 48000, 1.0,
        );
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success on left channel, got {:?}", result.status);
    }

    #[test]
    fn test_wav_roundtrip_right_channel() {
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "right", 0.5, 48000, 1.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Success),
            "right-only signal should not be decoded (left channel is silent), got {:?}", result.status);
    }

    // ── Edge case: mid-signal start ──────────────────────────────────────

    #[test]
    fn test_wav_mid_signal_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mid_signal.wav");

        generate_test_wav_with_prefix(
            &path,
            0.5,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.5, 48000, 1.5,
        );

        let result = decode_ltc_from_wav(&path, 25.0, false).unwrap();
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

            generate_test_wav_with_prefix(
                &path,
                silent_secs,
                Timecode { hours: 2, minutes: 0, seconds: 0, frames: 0 },
                25.0, false, "both", 0.5, 48000, 1.0,
            );

            let result = decode_ltc_from_wav(&path, 25.0, false).unwrap();
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
            25.0, false, "both", 0.5, 48000, 0.12,
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

        let result = decode_ltc_from_wav(&path, 25.0, false).unwrap();
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

        let result = decode_ltc_from_wav(&path, 25.0, false).unwrap();
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

        let result = decode_ltc_from_wav(&path, 25.0, false).unwrap();
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

        let result = decode_ltc_from_wav(&path, 25.0, false).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }),
            "empty WAV should produce Error, got {:?}", result.status);
    }

    // ── Low-amplitude LTC (below old 0.005 min threshold) ────────────────

    #[test]
    fn test_wav_low_amplitude_ltc() {
        let result = verify_roundtrip(
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.12, 48000, 2.0,
        );
        assert!(result.valid_frames > 0,
            "expected >0 valid frames with low-amplitude LTC (volume=0.12, amp≈0.014), got {}/{}",
            result.valid_frames, result.total_possible_frames);
    }

    // ── quick_check_ltc ──────────────────────────────────────────────────

    #[test]
    fn test_quick_check_ltc_valid() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("valid_check.wav");
        generate_test_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.5, 48000, 1.0,
        );
        let result = quick_check_ltc(&path).unwrap();
        assert!(result, "quick_check_ltc should return true for valid LTC");
    }

    #[test]
    fn test_quick_check_ltc_invalid() {
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
        let result = quick_check_ltc(&path).unwrap();
        assert!(!result, "quick_check_ltc should return false for silent file");
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

        let result = decode_ltc_from_wav(&wav_path, 25.0, false).unwrap();

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
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0);
        assert!(bits.is_empty());
    }

    #[test]
    fn test_decode_bits_from_zc_empty_returns_empty() {
        let zc = vec![];
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0);
        assert!(bits.is_empty());
    }

    #[test]
    fn test_decode_bits_from_zc_synthetic_path() {
        // Create ZCs with long intervals (few short intervals) → synthetic path
        // At 25fps, spb = 48000/(25*80) = 24
        // All intervals = spb → short_ratio = 0 → synthetic path
        let zc = vec![12, 36, 60, 84, 108];
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0);
        // synthetic: leading=0, then each interval=spb → n_periods=1 → zeros=0 + 1
        // So: [1,1,1,1,1]
        assert_eq!(bits, vec![1, 1, 1, 1, 1]);
    }

    #[test]
    fn test_decode_bits_from_zc_real_path() {
        // Create ZCs with many short intervals → real path
        // spb = 24, short_threshold = 18
        // Alternate short(10) and long(30) intervals: short_ratio ≈ 0.5 > 0.10
        let zc = vec![0, 10, 40, 50, 80, 90, 120, 130]; // short, long, short, long, short, long, short
        let bits = decode_bits_from_zero_crossings(&zc, 48000, 25.0);
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
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_try_decode_zc_intervals_too_few_bits() {
        // ZCs that produce fewer than 80 bits
        // Only generate a few ZCs
        let zc = vec![12, 36, 60, 84, 108, 132, 156]; // 7 ZCs → synthetic produces 7 bits
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false);
        assert!(result.is_none());
    }

    #[test]
    fn test_try_decode_zc_intervals_valid_synthetic() {
        // Build ZCs matching a valid LTC frame (use get_ltc_bits to know the pattern)
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        let spb = 24.0; // 48000/(25*80)
        let zc = bits_to_zc(&bits, spb);
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false);
        assert!(result.is_some(), "should decode a valid frame");
        if let Some(r) = result {
            assert!(r.valid_frames >= 1, "should find at least 1 valid frame, got {}", r.valid_frames);
        }
    }

    #[test]
    fn test_try_decode_zc_intervals_all_zero_bits() {
        // No '1' bits means no ZCs at all
        let zc = vec![];
        let result = try_decode_via_zc_intervals(&zc, 48000, 25.0, false);
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
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.01, &zc);
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
        let bits = extract_bits_adaptive(&signal, spb, 3, 0.01, &zc);
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
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.01, &drift_zc);
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
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.1, &zc);
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
        let bits = extract_bits_adaptive(&signal, spb, 0, 0.01, &zc);
        assert_eq!(bits, vec![1, 1]);
    }

    // ── build_result ──────────────────────────────────────────────────

    #[test]
    fn test_build_result_with_valid_result() {
        let r = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 50,
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
            frame_starts: vec![0],
        };
        let zc = vec![12, 36, 60];
        let result = build_result(Some(r), &zc, 48000, 0.01, 2, 2.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success));
        assert!((result.avg_confidence - 50.0 / 60.0).abs() < 0.001);
        assert_eq!(result.valid_frames, 50);
        assert_eq!(result.total_possible_frames, 60);
        assert!(!result.timecodes.is_empty());
    }

    #[test]
    fn test_build_result_with_none() {
        let zc = vec![];
        let result = build_result(None, &zc, 48000, 0.01, 2, 0.5, std::time::Instant::now()).unwrap();
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
            total_possible: 100,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            frame_starts: vec![],
        };
        let zc = vec![];
        let result = build_result(Some(low_conf), &zc, 48000, 0.01, 2, 1.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::NoSyncWord));

        let med_conf = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 40,
            total_possible: 100,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            frame_starts: vec![],
        };
        let result = build_result(Some(med_conf), &zc, 48000, 0.01, 2, 1.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::LowConfidence));

        let high_conf = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 90,
            total_possible: 100,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            frame_starts: vec![],
        };
        let result = build_result(Some(high_conf), &zc, 48000, 0.01, 2, 1.0, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success));
    }

    #[test]
    fn test_build_result_zero_possible() {
        let r = ScoredResult {
            fps: 25.0,
            drop_frame: false,
            valid_frames: 0,
            total_possible: 0,
            timecodes: vec![],
            details_entry: "".to_string(),
            spb: 24.0,
            phase: 0,
            frame_starts: vec![],
        };
        let zc = vec![];
        let result = build_result(Some(r), &zc, 48000, 0.01, 2, 1.0, std::time::Instant::now()).unwrap();
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
            total_possible: 3,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb,
            phase: 0, // phase=0 = start of bit boundary
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.001, 48000, 0, &[]);
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
            total_possible: 0,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb,
            phase: 0,
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.5, 48000, 0, &[]);
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
            total_possible: 3,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb,
            phase: 0,
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.001, 48000, 0, &zc);
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
            total_possible: 3,
            timecodes: vec![],
            details_entry: "test".to_string(),
            spb: spb_mismatch,
            phase: 0,
            frame_starts: vec![],
        };

        let result = decode_full_file(&signal, &params, 0.001, 48000, 0, &zc);
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

        let bits_adaptive = extract_bits_adaptive(&signal, spb, 0, 0.01, &zc);
        let bits_nominal = extract_bits(&signal, spb, 0, 0.01);

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

        let bits_adaptive = extract_bits_adaptive(&signal, nominal_spb, 0, 0.01, &zc);
        assert_eq!(bits_adaptive.len(), num_bits,
            "adaptive: expected {} bits, got {}", num_bits, bits_adaptive.len());
        for (i, (&got, &expected)) in bits_adaptive.iter().zip(bit_pattern.iter()).enumerate() {
            assert_eq!(got, expected,
                "adaptive bit {} mismatch: got {}, expected {}", i, got, expected);
        }

        let bits_nominal = extract_bits(&signal, nominal_spb, 0, 0.01);
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

    // ── decode_ltc_samples ────────────────────────────────────────────

    #[test]
    fn test_decode_ltc_samples_empty_buffer() {
        let samples = vec![];
        let result = decode_ltc_samples(&samples, 48000, 2, 25.0, false, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_silent_buffer() {
        let samples = vec![0.0f32; 48000 * 2]; // 1 sec silence
        let result = decode_ltc_samples(&samples, 48000, 2, 25.0, false, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_25fps_basic() {
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for 25fps LTC, got {:?} (valid={})", result.status, result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_samples_24fps() {
        let tcs: Vec<Timecode> = (0..24).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 24.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 24.0, false, std::time::Instant::now()).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_30fps() {
        let tcs: Vec<Timecode> = (0..30).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 30.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 30.0, false, std::time::Instant::now()).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_44100hz() {
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 44100, 0.5);
        let result = decode_ltc_samples(&signal, 44100, 1, 25.0, false, std::time::Instant::now()).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_different_start_timecode() {
        let tcs = vec![Timecode { hours: 10, minutes: 15, seconds: 30, frames: 12 }];
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
    }

    #[test]
    fn test_decode_ltc_samples_too_short_signal() {
        // Only a few samples — not enough for any zero crossings
        let samples = vec![0.5f32, -0.5, 0.5, -0.5];
        let result = decode_ltc_samples(&samples, 48000, 1, 25.0, false, std::time::Instant::now()).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_wrong_fps() {
        // Generate 25fps LTC but decode at 30fps
        let tcs: Vec<Timecode> = (0..50).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 30.0, false, std::time::Instant::now()).unwrap();
        // Should get something (maybe low confidence or error)
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }) || result.valid_frames > 0);
    }

    #[test]
    fn test_decode_ltc_samples_multiple_frames_timecodes() {
        // Generate a sequence of timecodes and verify they decode correctly
        let tcs: Vec<Timecode> = (0..10).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i * 3,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now()).unwrap();
        if matches!(result.status, LtcDecodeStatus::Success) {
            assert!(result.valid_frames >= 5,
                "should decode at least 5 of 10 frames, got {}", result.valid_frames);
            // Check that timecodes are roughly in the right ballpark
            if !result.timecodes.is_empty() {
                assert!(result.timecodes[0].timecode.minutes == 0
                    || result.timecodes[0].timecode.seconds == 0);
            }
        }
    }

    // ───ƒ─ decode_ltc_samples drop-frame ──────────────────────────────────

    #[test]
    fn test_decode_ltc_samples_drop_frame() {
        let tcs: Vec<Timecode> = (0..30).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let signal = synthesize_ltc_signal(&tcs, 29.97, true, 48000, 0.5);
        let result = decode_ltc_samples(&signal, 48000, 1, 29.97, true, std::time::Instant::now()).unwrap();
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
        let result = decode_ltc_samples(&signal, 48000, 1, 25.0, false, std::time::Instant::now()).unwrap();
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
        decode_ltc_samples(signal, 48000, 1, 25.0, false, std::time::Instant::now()).unwrap()
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

    // ── 9. Verify clean signal baseline (noise-free sanity check) ──────

    #[test]
    fn test_noise_baseline_clean() {
        let signal = base_signal();
        let result = base_decode(&signal);
        assert_ltc_ok(&result, 45);
    }
}
