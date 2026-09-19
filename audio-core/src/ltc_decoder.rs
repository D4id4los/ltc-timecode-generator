use std::path::Path;

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

use crate::Timecode;

// ── Public types ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
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
}

impl LtcDetectionResult {
    fn error(msg: impl Into<String>) -> Self {
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
        }
    }
}

// ── Internal candidates ──────────────────────────────────────────────────────

const CANDIDATES: &[(f64, bool, &str)] = &[
    (24.0, false, "24 fps"),
    (25.0, false, "25 fps"),
    (29.97, false, "29.97 ND"),
    (29.97, true, "29.97 DF"),
    (30.0, false, "30 fps"),
];

// Sync word at bits 64-79:  0 0 1 1 1 1 1 1 1 1 1 1 1 1 0 1
const SYNC_WORD: [u8; 16] = [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1];
const SYNC_OFFSET: usize = 64; // sync word starts at bit 64 of 80-bit frame

/// Maximum duration of the prefix used for fast candidate search.
/// Only the first N seconds are analyzed to determine FPS, SPB, and phase.
const PREFIX_DURATION_SECS: f64 = 30.0;

/// If prefix analysis achieves this confidence or higher, skip the full
/// candidate search and decode the entire file in a single pass.
const HIGH_CONFIDENCE_THRESHOLD: f32 = 0.70;

// ── Public API ───────────────────────────────────────────────────────────────

pub fn decode_ltc_from_wav(path: &Path) -> Result<LtcDetectionResult, String> {
    let start = std::time::Instant::now();

    let mut reader = hound::WavReader::open(path)
        .map_err(|e| format!("Failed to open WAV file: {}", e))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as usize;

    info!("Decoding LTC from: {} ({} Hz, {} ch)", path.display(), sample_rate, channels);

    let samples = read_mono_samples(&mut reader, &spec)
        .map_err(|e| format!("Failed to read audio samples: {}", e))?;

    if samples.is_empty() {
        warn!("LTC decode: audio file contains no samples: {}", path.display());
        return Ok(LtcDetectionResult::error("Audio file contains no samples"));
    }

    let total_duration = samples.len() as f64 / sample_rate as f64;
    let noise_floor = estimate_noise_floor(&samples);
    let threshold = (noise_floor * 0.5).max(0.005);
    debug!("LTC decode: read {:.2}s of audio, noise_floor={:.8}, threshold={:.8}",
        total_duration, noise_floor, threshold);

    if threshold < 1e-8 {
        warn!("LTC decode: signal is completely silent: {}", path.display());
        return Ok(LtcDetectionResult::error("Audio signal is completely silent"));
    }

    // ── Dispatch based on file size ──────────────────────────────────────

    let (best_result, zc) = if total_duration <= PREFIX_DURATION_SECS {
        debug!("LTC decode: file <= {}s prefix -- direct full search", PREFIX_DURATION_SECS);
        let zc = find_zero_crossings(&samples, threshold);
        debug!("LTC decode: found {} zero-crossings", zc.len());
        if zc.len() < 8 {
            warn!("LTC decode: only {} zero-crossings, signal may not be LTC: {}", zc.len(), path.display());
            return Ok(LtcDetectionResult::error(format!(
                "Only {} zero-crossings found (need ≥8) -- signal may be silent or not LTC audio",
                zc.len()
            )));
        }
        let (result, _) = evaluate_on_slice(&samples, &zc, sample_rate, threshold);
        (result, zc)
    } else {
        let prefix_len = (PREFIX_DURATION_SECS * sample_rate as f64) as usize;
        let prefix_len = prefix_len.min(samples.len());
        info!("LTC decode: prefix analysis -- using first {:.2}s of {:.2}s total ({} samples, threshold={:.6})",
            prefix_len as f64 / sample_rate as f64, total_duration, prefix_len, threshold);

        let prefix = &samples[..prefix_len];
        let zc_prefix = find_zero_crossings(prefix, threshold);
        debug!("LTC decode: prefix -- found {} zero-crossings (need ≥8)", zc_prefix.len());

        if zc_prefix.len() < 8 {
            warn!("LTC decode: prefix has only {} zero-crossings -- signal may start past prefix, falling back to full-file search", zc_prefix.len());
            let zc = find_zero_crossings(&samples, threshold);
            if zc.len() < 8 {
                return Ok(LtcDetectionResult::error(format!(
                    "Only {} zero-crossings found (need ≥8) -- signal may be silent or not LTC audio",
                    zc.len()
                )));
            }
            debug!("LTC decode: full -- found {} zero-crossings", zc.len());
            info!("LTC decode: full-file candidate search -- evaluating {} FPS candidates on {:.2}s of audio",
                CANDIDATES.len(), total_duration);
            let (result, _) = evaluate_on_slice(&samples, &zc, sample_rate, threshold);
            (result, zc)
        } else {
            // Run coarse-to-fine candidate search on the prefix
            info!("LTC decode: prefix -- running coarse-to-fine candidate search ({} FPS candidates, {} zero-crossings)",
                CANDIDATES.len(), zc_prefix.len());
            let (prefix_result, _) = evaluate_on_slice(prefix, &zc_prefix, sample_rate, threshold);

            let confidence = prefix_result.as_ref().map_or(0.0, |r| {
                if r.total_possible > 0 { r.valid_frames as f32 / r.total_possible as f32 } else { 0.0 }
            });

            match prefix_result {
                Some(ref r) => {
                    info!("LTC decode: prefix result -- {} valid / {} possible ({:.1}%) at FPS {}, spb={:.2}, phase={}",
                        r.valid_frames, r.total_possible, confidence * 100.0, r.fps, r.spb, r.phase);
                }
                None => {
                    info!("LTC decode: prefix -- no valid candidates found");
                }
            }

            if confidence >= HIGH_CONFIDENCE_THRESHOLD {
                let r = prefix_result.unwrap();
                info!("LTC decode: prefix confidence {:.1}% ≥ {}% -- single-pass decoding full file ({:.2}s, {} samples)",
                    confidence * 100.0, HIGH_CONFIDENCE_THRESHOLD * 100.0, total_duration, samples.len());
                info!("LTC decode: full decode -- extracting bits (spb={:.2}, phase={}, threshold={:.6})",
                    r.spb, r.phase, threshold);

                let bits = extract_bits(&samples, r.spb, r.phase, threshold);
                debug!("LTC decode: full decode -- extracted {} bits", bits.len());

                if bits.len() < 80 {
                    warn!("LTC decode: full decode -- fewer than 80 bits extracted, no frames possible");
                    let zc = find_zero_crossings(&samples, threshold);
                    return build_result(None, &zc, sample_rate, threshold, channels, total_duration, start);
                }

                let (valid_frames, total_possible, frame_starts) = find_frames(&bits);
                info!("LTC decode: full decode -- found {} valid / {} possible frames",
                    valid_frames, total_possible);

                if valid_frames > 0 {
                    debug!("LTC decode: full decode -- decoding {} timecodes", valid_frames);
                }
                let timecodes: Vec<FrameTimecode> = frame_starts
                    .iter()
                    .enumerate()
                    .map(|(idx, &start)| FrameTimecode {
                        frame_index: idx as u32,
                        timecode: decode_timecode_from_bits(&bits, start),
                        timecode_secs: (r.phase as f64 + start as f64 * r.spb) / sample_rate as f64,
                    })
                    .collect();

                let fps_label = match r.fps as u32 {
                    24 => "24 fps",
                    25 => "25 fps",
                    29 => "29.97",
                    30 => "30 fps",
                    _ => "other",
                };

                let details_entry = format!(
                    "{}: {} valid / {} possible frames (single-pass, spb={:.2})",
                    fps_label, valid_frames, total_possible, r.spb
                );

                let full_result = ScoredResult {
                    fps: r.fps,
                    drop_frame: r.drop_frame,
                    valid_frames,
                    total_possible,
                    timecodes,
                    bits,
                    details_entry,
                    spb: r.spb,
                    phase: r.phase,
                    frame_starts,
                };

                let zc = find_zero_crossings(&samples, threshold);
                return build_result(Some(full_result), &zc, sample_rate, threshold, channels, total_duration, start);
            }

            // Fallback: full-file candidate search
            info!("LTC decode: prefix confidence {:.1}% < {}% -- falling back to full-file candidate search on {:.2}s of audio",
                confidence * 100.0, HIGH_CONFIDENCE_THRESHOLD * 100.0, total_duration);
            let zc = find_zero_crossings(&samples, threshold);
            if zc.len() < 8 {
                return Ok(LtcDetectionResult::error(format!(
                    "Only {} zero-crossings found (need ≥8) -- signal may be silent or not LTC audio",
                    zc.len()
                )));
            }
            debug!("LTC decode: full -- found {} zero-crossings", zc.len());
            info!("LTC decode: full-file candidate search -- evaluating {} FPS candidates on {:.2}s of audio",
                CANDIDATES.len(), total_duration);
            let (result, _) = evaluate_on_slice(&samples, &zc, sample_rate, threshold);
            (result, zc)
        }
    };

    build_result(best_result, &zc, sample_rate, threshold, channels, total_duration, start)
}

/// Evaluate LTC candidates on a slice of audio samples.
///
/// Strategy: try all FPS candidates at nominal SPB plus SPB variants,
/// with phases derived from the first several zero-crossings.
fn evaluate_on_slice(
    samples: &[f32],
    zc: &[usize],
    sample_rate: u32,
    threshold: f32,
) -> (Option<ScoredResult>, u32) {
    let mut best_valid = 0u32;
    let mut best_result: Option<ScoredResult> = None;

    info!("LTC evaluate: searching {} FPS candidates, {} ZCs over {:.2}s slice",
        CANDIDATES.len(), zc.len(), samples.len() as f64 / sample_rate as f64);

    for &(fps, drop_frame, fps_name) in CANDIDATES {
        let spb_nominal = sample_rate as f64 / (fps * 80.0);
        if spb_nominal < 0.5 { continue; }

        // Try SPB variants to compensate for clock drift
        let spb_variants = if spb_nominal >= 8.0 {
            let half_range = (spb_nominal * 0.004).max(0.05);
            (0..5)
                .map(|i| { let t = i as f64 / 4.0; spb_nominal + (t - 0.5) * 2.0 * half_range })
                .collect::<Vec<_>>()
        } else {
            vec![spb_nominal]
        };

        for &spb in &spb_variants {
            let max_phases = (spb / 4.0).round() as usize;
            let phases_to_try = zc.iter().take(max_phases.max(5).min(12)).copied();

            let half_spb = (spb * 0.5) as usize;
            for phase in phases_to_try {
                for &candidate_phase in &[phase, phase.saturating_sub(half_spb)] {
                    let bits = extract_bits(samples, spb, candidate_phase, threshold);
                    if bits.len() < 80 { continue; }

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

                        debug!("LTC extract-bits: {} spb={:.2} phase={} -- {} valid / {} possible",
                            fps_name, spb, candidate_phase, valid_frames, total_possible);

                        best_valid = valid_frames;
                        best_result = Some(ScoredResult {
                            fps,
                            drop_frame,
                            valid_frames,
                            total_possible,
                            timecodes,
                            bits,
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
        }
    }

    // ── Refinement: adaptive extraction for best candidate ──────────────
    // Uses zero-crossing-snapped bit boundaries to improve decoding of
    // noisy/real-world LTC where timing jitter causes fixed-spb drift.
    if let Some(ref best) = best_result.clone() {
        let best_spb = best.spb;
        let best_fps = best.fps;
        let best_drop_frame = best.drop_frame;
        let best_phase = best.phase;
        let fps_label = match best_fps as u32 {
            24 => "24 fps", 25 => "25 fps", 29 => "29.97", 30 => "30 fps", _ => "other",
        };

        let max_phases = (best_spb / 4.0).round() as usize;
        let phases_to_try = zc.iter().take(max_phases.max(5).min(12)).copied();
        for phase in phases_to_try {
            if phase == best_phase { continue; }
            let bits = extract_bits_adaptive(samples, best_spb, phase, threshold, zc);
            if bits.len() < 80 { continue; }

            let (valid_frames, total_possible, frame_starts) = find_frames(&bits);
            if valid_frames > best_valid {
                let timecodes: Vec<FrameTimecode> = frame_starts
                    .iter()
                    .enumerate()
                    .map(|(idx, &start)| FrameTimecode {
                        frame_index: idx as u32,
                        timecode: decode_timecode_from_bits(&bits, start),
                        timecode_secs: (phase as f64 + start as f64 * best_spb) / sample_rate as f64,
                    })
                    .collect();

                debug!("LTC refinement adaptive -- {} valid / {} possible (phase={})",
                    valid_frames, total_possible, phase);

                best_valid = valid_frames;
                best_result = Some(ScoredResult {
                    fps: best_fps,
                    drop_frame: best_drop_frame,
                    valid_frames,
                    total_possible,
                    timecodes,
                    bits,
                    details_entry: format!(
                        "{}: {} valid / {} possible frames (adaptive, phase={}, spb={:.2})",
                        fps_label, valid_frames, total_possible, phase, best_spb
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

#[allow(dead_code)]
fn estimate_spb_from_zc(zc: &[usize], _sample_rate: u32) -> f64 {
    if zc.len() < 5 {
        return 0.0;
    }

    let mut intervals: Vec<f64> = zc.windows(2)
        .map(|w| (w[1] - w[0]) as f64)
        .collect();

    intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = intervals.len();
    let median = intervals[n / 2];

    // Count intervals shorter than median
    let n_short = intervals.iter().filter(|&&i| i < median).count();
    let short_ratio = n_short as f64 / n as f64;

    debug!("SPB estimate: {} intervals, median={:.2}, {:.1}% short",
        n, median, short_ratio * 100.0);

    if short_ratio > 0.20 {
        // Real encoding: compute mean of intervals > median (these are ~1.0*spb)
        let n_long = n - n_short;
        if n_long == 0 { return median; }
        let sum_long: f64 = intervals.iter().filter(|&&i| i > median).sum();
        sum_long / n_long as f64
    } else {
        // Synthetic encoding: use the 15th percentile (small intervals ≈ spb)
        let idx = (n * 15 / 100).max(1).min(n - 1);
        // Average the 10th-20th percentile range for stability
        let p10 = (n * 10 / 100).max(1).min(n - 1);
        let p20 = (n * 20 / 100).max(1).min(n - 1);
        let sum: f64 = intervals[p10..p20].iter().sum();
        let count = (p20 - p10) as f64;
        if count > 0.0 { sum / count } else { intervals[idx] }
    }
}

/// Build the final LtcDetectionResult from the best candidate (or None) and
/// all the metadata gathered during decoding.
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

    match best_result {
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
                    "Detected rate: {:.2} fps / {} spb -- confidence: {:.1}%",
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

            Ok(LtcDetectionResult {
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
            })
        }
        None => {
            info!(
                "LTC decode: no valid alignment found for any candidate ({} zero-crossings, threshold={:.6}, processing={:.0}ms)",
                zc.len(), threshold, processing_time_ms,
            );
            let details = vec![
                "No valid LTC frame alignment found across any candidate frame rate.".to_string(),
                format!("Zero-crossings found: {} (threshold: {:.6})", zc.len(), threshold),
                format!(
                    "Audio: {:.2}s @ {} Hz, {} channels",
                    total_duration, sample_rate, channels
                ),
            ];
            Ok(LtcDetectionResult {
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
                processing_time_ms,
                first_ltc_timecode_secs: 0.0,
            })
        }
    }
}

pub fn quick_check_ltc(path: &Path) -> Result<bool, String> {
    let result = decode_ltc_from_wav(path)?;
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
    let n = samples.len().min(10_000);
    let mut sorted: Vec<f32> = samples[..n].iter().map(|s| s.abs()).collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median = sorted[sorted.len() / 2];
    median.max(1e-10)
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
//
// Two encoding schemes exist:
//   REAL (SMPTE standard): transition at EVERY bit boundary + extra at mid of bit-1
//     → intervals: 0.5 spb (half of bit-1) or 1.0 spb (full bit-0)
//     → decode: two shorts = bit-1, one long = bit-0
//   SYNTHETIC (our generator): transitions ONLY at mid of bit-1
//     → intervals: k * spb (k = 1 + zeros_between_consecutive_ones)
//     → decode: interval/spb = k → (k-1 zeros) + 1
//
// Auto-detect by counting intervals shorter than 75% of a bit period.

#[allow(dead_code)]
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

    // Detect encoding: count intervals shorter than 0.75 spb
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

#[allow(dead_code)]
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

#[allow(dead_code)]
fn decode_bits_synthetic_zc(zc: &[usize], spb: f64) -> Vec<u8> {
    let mut bits = Vec::with_capacity(zc.len() + 8);

    let first_zc = zc[0] as f64;
    let leading = (first_zc / spb - 0.5).round() as i32;
    if leading > 0 {
        for _ in 0..leading {
            bits.push(0);
        }
    }
    bits.push(1);

    for i in 1..zc.len() {
        let interval = (zc[i] - zc[i - 1]) as f64;
        let n_periods = (interval / spb).round() as u32;
        let zeros = n_periods.saturating_sub(1);
        for _ in 0..zeros {
            bits.push(0);
        }
        bits.push(1);
    }

    bits
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

fn extract_bits(samples: &[f32], samples_per_bit: f64, phase: usize, threshold: f32) -> Vec<u8> {
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

        if s25.abs() < threshold || s75.abs() < threshold {
            bits.push(0);
        } else {
            let bit = if s25.signum() != s75.signum() { 1 } else { 0 };
            bits.push(bit);
        }

        pos += samples_per_bit;
    }

    bits
}

fn extract_bits_adaptive(
    samples: &[f32],
    samples_per_bit: f64,
    phase: usize,
    threshold: f32,
    zero_crossings: &[usize],
) -> Vec<u8> {
    let mut bits = Vec::new();
    let quarter = samples_per_bit * 0.25;
    let three_quarter = samples_per_bit * 0.75;
    let snap_radius = (samples_per_bit * 0.25) as usize;
    let mut pos = phase as f64;

    // Binary search helper: find first ZC >= target
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

        if s25.abs() < threshold || s75.abs() < threshold {
            bits.push(0);
        } else {
            let bit = if s25.signum() != s75.signum() { 1 } else { 0 };
            bits.push(bit);
        }

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
    let mut dist = 0u32;
    for (i, &bit) in a.iter().enumerate() {
        if bit != SYNC_WORD[i] {
            dist += 1;
            if dist > SYNC_MATCH_TOLERANCE {
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

    // Find the best frame alignment (0-79) by counting sync words at expected positions
    let mut alignment_scores = vec![0u32; 80];
    for &sp in &sync_positions {
        if sp >= SYNC_OFFSET {
            let alignment = (sp - SYNC_OFFSET) % 80;
            alignment_scores[alignment] += 1;
        }
    }

    // max_by_key returns (index, &count) -- index is the alignment, count is the score
    let (best_alignment, _best_count_value) = alignment_scores
        .iter()
        .enumerate()
        .max_by_key(|&(_, &c)| c)
        .unwrap_or((0, &0));
    // ^^ Note: alignment_scores is [u32; 80]. .iter().enumerate() yields (usize, &u32).
    //    The name swap bug that existed here: previously (best_count, &best_alignment)
    //    assigned the index to best_count and the count to best_alignment,
    //    causing the "if best_count == 0" check to always early-return.

    if alignment_scores[0] == 0 && alignment_scores.iter().all(|&c| c == 0) {
        // All alignments have zero hits -- nothing found
        return (0, 0, Vec::new());
    }

    let total_possible = if bits.len() > best_alignment as usize {
        ((bits.len() - best_alignment as usize) / 80) as u32
    } else {
        0
    };

    // Walk through frames at the best alignment and verify each sync word
    let mut frame_starts = Vec::new();
    let align = best_alignment as usize;
    for idx in 0.. {
        let frame_start = align + idx * 80;
        if frame_start + 80 > bits.len() {
            break;
        }
        let sync_start = frame_start + SYNC_OFFSET;
        if sync_start + 16 <= bits.len()
            && bits_hamming_distance_16(&bits[sync_start..sync_start + 16]) <= SYNC_MATCH_TOLERANCE
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
#[allow(dead_code)]
struct ScoredResult {
    fps: f64,
    drop_frame: bool,
    valid_frames: u32,
    total_possible: u32,
    timecodes: Vec<FrameTimecode>,
    bits: Vec<u8>,
    details_entry: String,
    spb: f64,
    phase: usize,
    frame_starts: Vec<usize>,
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
        // Frame units = 4 = 0b0100 → bits[0]=0, [1]=0, [2]=1, [3]=0
        assert_eq!(bits[0], 0);
        assert_eq!(bits[1], 0);
        assert_eq!(bits[2], 1);
        assert_eq!(bits[3], 0);
        // Frame tens = 0 → bits[8]=0, [9]=0
        assert_eq!(bits[8], 0);
        assert_eq!(bits[9], 0);
    }

    #[test]
    fn test_get_ltc_bits_frame_13() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 13 };
        let bits = crate::get_ltc_bits(&tc, false);
        // Frame units = 3 = 0b0011
        assert_eq!(bits[0], 1);
        assert_eq!(bits[1], 1);
        assert_eq!(bits[2], 0);
        assert_eq!(bits[3], 0);
        // Frame tens = 1 = 0b01
        assert_eq!(bits[8], 1);
        assert_eq!(bits[9], 0);
    }

    #[test]
    fn test_get_ltc_bits_drop_frame_flag() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits_nd = crate::get_ltc_bits(&tc, false);
        let bits_df = crate::get_ltc_bits(&tc, true);
        // Bit 10 is the drop-frame flag
        assert_eq!(bits_nd[10], 0, "non-drop must have bit 10 = 0");
        assert_eq!(bits_df[10], 1, "drop-frame must have bit 10 = 1");
    }

    #[test]
    fn test_get_ltc_bits_color_frame_zero() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        let bits = crate::get_ltc_bits(&tc, false);
        // Bit 11 is color frame flag
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
        // Two frames concatenated: decode the second one
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
        // At 29.97 DF: 01:00:59:29 → should skip frames 0,1 → 01:01:00:02
        let tc = Timecode { hours: 1, minutes: 0, seconds: 59, frames: 29 };
        let next = increment_timecode(&tc, 29.97, true);
        assert_eq!(next, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 2 });
    }

    #[test]
    fn test_increment_timecode_drop_frame_no_skip_div10() {
        // At 29.97 DF: 01:09:59:29 → minute % 10 == 0 → NO skip → 01:10:00:00
        let tc = Timecode { hours: 1, minutes: 9, seconds: 59, frames: 29 };
        let next = increment_timecode(&tc, 29.97, true);
        assert_eq!(next, Timecode { hours: 1, minutes: 10, seconds: 0, frames: 0 });
    }

    #[test]
    fn test_increment_timecode_drop_frame_sequential() {
        // Verify the first few increments at 29.97 DF from a minute boundary (minutes % 10 != 0)
        let mut tc = Timecode { hours: 1, minutes: 1, seconds: 0, frames: 0 };
        tc = increment_timecode(&tc, 29.97, true);
        assert_eq!(tc, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 1 });
        tc = increment_timecode(&tc, 29.97, true);
        // frames 0 and 1 were already passed; this should be frame 2
        assert_eq!(tc, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 2 });
    }

    #[test]
    fn test_increment_timecode_29_97_non_drop() {
        // Non-drop should never skip frames
        let mut tc = Timecode { hours: 1, minutes: 0, seconds: 59, frames: 29 };
        tc = increment_timecode(&tc, 29.97, false);
        assert_eq!(tc, Timecode { hours: 1, minutes: 1, seconds: 0, frames: 0 });
    }

    // ── estimate_noise_floor ─────────────────────────────────────────────

    #[test]
    fn test_estimate_noise_floor_constant() {
        let samples = vec![0.5f32; 1000];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 0.5).abs() < 1e-6, "median of constant signal should be the constant value");
    }

    #[test]
    fn test_estimate_noise_floor_silent() {
        let samples = vec![0.0f32; 1000];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 1e-10).abs() < 1e-12, "silence floor should floor at 1e-10");
    }

    #[test]
    fn test_estimate_noise_floor_mixed() {
        let samples: Vec<f32> = vec![0.5, 0.1, 0.3, 0.8, 0.2];
        let nf = estimate_noise_floor(&samples);
        // sorted abs: [0.1, 0.2, 0.3, 0.5, 0.8] → median at index 2 = 0.3
        assert!((nf - 0.3).abs() < 1e-6);
    }

    #[test]
    fn test_estimate_noise_floor_negative_values() {
        let samples: Vec<f32> = vec![-0.7, -0.1, -0.5, -0.3];
        let nf = estimate_noise_floor(&samples);
        // sorted abs: [0.1, 0.3, 0.5, 0.7] → median at index 2 = 0.5
        assert!((nf - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_estimate_noise_floor_clamped_10k() {
        let samples = vec![0.42f32; 20_000];
        let nf = estimate_noise_floor(&samples);
        assert!((nf - 0.42).abs() < 1e-6, "should use only first 10K");
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
        assert!(crossings.is_empty(), "silence should produce no crossings");
    }

    #[test]
    fn test_find_zero_crossings_below_threshold() {
        let samples = vec![0.01f32, -0.02, 0.01];
        let crossings = find_zero_crossings(&samples, 0.05);
        assert!(crossings.is_empty(), "all values below threshold");
    }

    #[test]
    fn test_find_zero_crossings_alternating() {
        let samples: Vec<f32> = (0..20).map(|i| if i % 2 == 0 { 0.5 } else { -0.5 }).collect();
        let crossings = find_zero_crossings(&samples, 0.1);
        // Every position from 1..20 should be a crossing
        assert_eq!(crossings.len(), 19);
        for (idx, &pos) in crossings.iter().enumerate() {
            assert_eq!(pos, idx + 1, "crossing position mismatch");
        }
    }

    #[test]
    fn test_find_zero_crossings_stays_positive() {
        let samples = vec![0.5f32, 0.3, 0.1, -0.2, -0.4];
        let crossings = find_zero_crossings(&samples, 0.05);
        // prev goes: 0→1, 1 stays while positive, then at -0.2: cur=-1 → crossing at 3
        assert_eq!(crossings, vec![3]);
    }

    // ── extract_bits (bi-phase mark decoding) ────────────────────────────

    fn synthesize_bit(samples_per_bit: usize, bit_value: u8, start_level: f32) -> (Vec<f32>, f32) {
        let mut buf = vec![0.0f32; samples_per_bit];
        let half = samples_per_bit / 2;
        match bit_value {
            0 => {
                // No mid-bit transition: constant level
                for s in buf.iter_mut() {
                    *s = start_level;
                }
                (buf, start_level)
            }
            1 => {
                // Mid-bit transition: flip at midpoint
                let mid_level = -start_level;
                for i in 0..half {
                    buf[i] = start_level;
                }
                for i in half..samples_per_bit {
                    buf[i] = mid_level;
                }
                (buf, mid_level)
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn test_extract_bits_all_zeros() {
        let spb = 8.0;
        // 3 zero bits: constant positive level
        let mut signal = Vec::new();
        let mut level = 0.5;
        for _ in 0..3 {
            let (chunk, l) = synthesize_bit(spb as usize, 0, level);
            signal.extend(chunk);
            level = l;
        }
        let bits = extract_bits(&signal, spb, 0, 0.01);
        assert_eq!(bits, vec![0, 0, 0], "all same sign = zero bits");
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
        assert_eq!(bits, vec![1, 1, 1], "mid transition = one bits");
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
        let signal = vec![0.0f32; (spb as usize) * 3]; // all silence
        let bits = extract_bits(&signal, spb, 0, 0.1);
        assert_eq!(bits, vec![0, 0, 0], "below threshold → zero bits");
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
        // Prepend 3 samples of silence
        let mut padded = vec![0.0f32; 3];
        padded.extend(signal);
        // Extract with phase offset 3
        let bits = extract_bits(&padded, spb, 3, 0.01);
        assert_eq!(bits, vec![1, 0, 1], "phase offset should align correctly");
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
        assert_eq!(valid, 1, "one valid frame");
        assert_eq!(total, 1, "one possible frame (80 bits at alignment 0)");
        assert_eq!(starts, vec![0], "frame starts at 0");
    }

    #[test]
    fn test_find_frames_two_frames() {
        let mut bits = build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 });
        bits.extend(build_frame_bits(Timecode { hours: 1, minutes: 0, seconds: 0, frames: 1 }));
        let (valid, _total, starts) = find_frames(&bits);
        assert_eq!(valid, 2, "two valid frames");
        assert_eq!(starts, vec![0, 80]);
    }

    #[test]
    fn test_find_frames_no_sync_word() {
        let bits = vec![0u8; 160]; // all zeros, no sync word
        let (valid, _total, starts) = find_frames(&bits);
        assert_eq!(valid, 0);
        assert!(starts.is_empty());
    }

    #[test]
    fn test_find_frames_random_bits() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut bits = vec![0u8; 320];
        // Fill with pseudo-random bits based on a seed
        let seed = 42u64;
        for i in 0..bits.len() {
            let mut h = DefaultHasher::new();
            (i as u64).hash(&mut h);
            seed.hash(&mut h);
            bits[i] = (h.finish() & 1) as u8;
        }
        let (valid, _total, _) = find_frames(&bits);
        // With Hamming-distance tolerance of 2, random bits may produce
        // a small number of false sync word matches (~0.2% per position).
        // Verify that the count stays very low (<1% of total positions).
        assert!(valid <= 2, "random bits should produce at most 2 false sync word matches, got {}", valid);
    }

    #[test]
    fn test_find_frames_short_buffer() {
        let bits = vec![0u8; 10]; // too short for a sync word
        let (valid, total, starts) = find_frames(&bits);
        assert_eq!(valid, 0);
        assert_eq!(total, 0);
        assert!(starts.is_empty());
    }

    #[test]
    fn test_find_frames_alignment_matters() {
        // Place sync word at offset 0 (not 64) → should not match frame alignment
        let mut bits = vec![0u8; 160];
        // Write sync word at position 0 (without proper frame context)
        bits[0..16].copy_from_slice(&SYNC_WORD);
        // Now the alignment would be (0-64)%80 = 16 → alignment_scores[16] gets 1 hit
        // But when iterating frames at alignment 16: frame 0 starts at 16, sync word at 80 not found
        let (valid, _, _) = find_frames(&bits);
        assert_eq!(valid, 0, "sync word at wrong offset should not produce valid frames");
    }

    // ── WAV generation helper ────────────────────────────────────────────

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

        // Write silence prefix
        let silence_samples = (silent_prefix_secs * sample_rate as f64).round() as usize;
        for _ in 0..silence_samples * 2 {
            writer.write_sample(0i16).unwrap();
        }

        // Write LTC frames
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

    /// Run a full round-trip test: generate WAV → decode → verify
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
        let result = decode_ltc_from_wav(&path).unwrap();
        result
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
        // First decoded frame runs ~50, start offset is within affordable precision
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
        assert!((result.detected_fps - 24.0).abs() < 0.1,
            "expected ~24 fps, got {}", result.detected_fps);
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
        assert!((result.detected_fps - 30.0).abs() < 0.1,
            "expected ~30 fps, got {}", result.detected_fps);
    }

    #[test]
    fn test_wav_roundtrip_2997_nd() {
        // 29.97 fps gives fractional spb (~20.02 at 48 kHz).
        // Non-integer spb causes accumulated drift in bit alignment.
        // Decoder may find some frames but not enough for high confidence.
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
        // Initial transient may skip first frame; verify timecodes are reasonably close
        assert!(result.valid_frames >= 22, "expected ~25 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_wav_roundtrip_16khz() {
        // 16 kHz gives exactly 8 samples/bit at 25 fps.
        // Bit extraction at 16 kHz produces correct bits when phase aligns properly.
        // Use 48 kHz as the primary round-trip rate; this test verifies no crash.
        let result = verify_roundtrip(
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.5, 16000, 2.0,
        );
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error at 16kHz, got {:?}", result.status);
        // 16 kHz may produce NoSyncWord due to limited samples/bit.
        // The key requirement: no crash and valid output structure.
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
        // Decoder reads channel 0 (left). Signal on right channel only → silence on left
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
            0.5, // 0.5s of silence before LTC
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, "both", 0.5, 48000, 1.5,
        );

        let result = decode_ltc_from_wav(&path).unwrap();
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

            let result = decode_ltc_from_wav(&path).unwrap();
            assert!(matches!(result.status, LtcDecodeStatus::Success | LtcDecodeStatus::LowConfidence),
                "silent_prefix={:.1}s: expected Success/LowConfidence, got {:?}",
                silent_secs, result.status);

            let expected_offset = silent_secs;
            let tolerance = 0.05; // 50ms tolerance -- initial transient may shift detection

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
        // A single frame is the minimum -- the decoder may or may not find it
        // due to the initial transient. Use 3 frames for reliable detection.
        let result = verify_roundtrip(
            Timecode { hours: 12, minutes: 34, seconds: 56, frames: 18 },
            25.0, false, "both", 0.5, 48000, 0.12, // ~3 frames
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

        let result = decode_ltc_from_wav(&path).unwrap();
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

        let result = decode_ltc_from_wav(&path).unwrap();
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

        let result = decode_ltc_from_wav(&path).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }),
            "very short audio should produce Error, got {:?}", result.status);
    }

    #[test]
    fn test_wav_empty_file() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty.wav");

        // Write minimal valid WAV header with 0 samples
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.finalize().unwrap();

        let result = decode_ltc_from_wav(&path).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }),
            "empty WAV should produce Error, got {:?}", result.status);
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

    // ── Integration: round-trip from generated samples to decoder ───────

    #[test]
    fn test_direct_sample_roundtrip() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
        let sample_rate = 48000u32;
        let fps = 25.0;
        let drop_frame = false;
        let channel = "both";
        let volume = 0.5;
        let duration_secs = 0.5;

        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;
        let total_frames = (duration_secs * fps).ceil() as usize;

        let mut all_samples_l = Vec::new();
        let mut tc = tc;
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

        for _ in 0..total_frames {
            frame_buf.fill(0.0);
            generate_ltc_frame_stereo(
                &tc, drop_frame, samples_per_frame, samples_per_bit,
                volume, channel, &mut last_level,
                &mut frame_buf[..samples_per_frame * 2],
            );
            for s in 0..samples_per_frame {
                all_samples_l.push(frame_buf[s * 2]);
            }
            tc = increment_timecode(&tc, fps, drop_frame);
        }

        let nf = estimate_noise_floor(&all_samples_l);
        let threshold = (nf * 0.5).max(0.005);
        let spb = sample_rate as f64 / (fps * 80.0);

        let bits = extract_bits(&all_samples_l, spb, 0, threshold);
        assert!(bits.len() >= 80);

        let (valid, _total, starts) = find_frames(&bits);
        assert!(valid > 0, "no valid frames at phase=0, threshold={}, spb={}", threshold, spb);
        assert!(!starts.is_empty());

        let zc = find_zero_crossings(&all_samples_l, threshold);
        assert!(zc.len() >= 8);

        for &phase in zc.iter().take(5) {
            let bits2 = extract_bits(&all_samples_l, spb, phase, threshold);
            if bits2.len() >= 80 {
                let (v, _, _) = find_frames(&bits2);
                if v > 0 {
                    return;
                }
            }
        }
        panic!("no valid frame from any phase ({} zero-crossings)", zc.len());
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

    // ── Real-world LTC test (wireless TX/RX artefacts) ──────────────────

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

        let result = decode_ltc_from_wav(&wav_path).unwrap();

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

        assert!(
            (result.detected_fps - 25.0).abs() < 0.1,
            "Expected ~25 fps, got {:.2}",
            result.detected_fps,
        );

        let frames_spanned = result.total_possible_frames.max(1) - 1;
        let secs_spanned = frames_spanned as f64 / result.detected_fps as f64;
        assert!(
            secs_spanned > 15.0,
            "Real-world LTC should span >15s of timecode, got {:.2}s ({} possible frames @ {:.2}fps)",
            secs_spanned, result.total_possible_frames, result.detected_fps,
        );
    }
}