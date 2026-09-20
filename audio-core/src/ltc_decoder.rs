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

// Sync word at bits 64-79:  0 0 1 1 1 1 1 1 1 1 1 1 1 1 0 1
const SYNC_WORD: [u8; 16] = [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1];
const SYNC_OFFSET: usize = 64;

// ── Public API ───────────────────────────────────────────────────────────────

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

    info!("LTC decode (+{:.1}s): scanning {} samples for zero-crossings (threshold={:.6})...",
        start.elapsed().as_secs_f64(), samples.len(), threshold);
    let zc = find_zero_crossings(&samples, threshold);
    debug!("LTC decode: found {} zero-crossings", zc.len());
    if zc.len() < 8 {
        warn!("LTC decode: only {} zero-crossings, signal may not be LTC: {}", zc.len(), path.display());
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
                let decoded = decode_full_file(&samples, r, threshold, sample_rate, best_window_start);
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
        let decoded = decode_full_file(&samples, r, threshold, sample_rate, best_window_start);
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
    let (fallback_result, _) = evaluate_on_slice(&samples, &zc, sample_rate, threshold, fps, drop_frame);
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
            }
        }
    };

    apply_coherent_first_timecode(&mut result);
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
) -> ScoredResult {
    let absolute_phase = params.phase + phase_offset;
    let bits = extract_bits(samples, params.spb, absolute_phase, threshold);
    let (valid_frames, total_possible, frame_starts) = find_frames(&bits);
    let timecodes: Vec<FrameTimecode> = frame_starts
        .iter()
        .enumerate()
        .map(|(idx, &start)| FrameTimecode {
            frame_index: idx as u32,
            timecode: decode_timecode_from_bits(&bits, start),
            timecode_secs: (absolute_phase as f64 + start as f64 * params.spb) / sample_rate as f64,
        })
        .collect();
    ScoredResult {
        fps: params.fps,
        drop_frame: params.drop_frame,
        valid_frames,
        total_possible,
        timecodes,
        details_entry: format!(
            "{:.2} fps: {} valid / {} possible frames (single-pass, spb={:.2}, phase={})",
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
}
