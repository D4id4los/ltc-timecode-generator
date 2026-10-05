use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use libltc_rs::prelude::*;
use log::{debug, info, warn};

use crate::LtcDecodeError;
use crate::ltc_decoder::{apply_coherent_first_timecode, compute_ltc_quality, CONFIDENCE_LOW_THRESHOLD, CONFIDENCE_SUCCESS_THRESHOLD, FrameTimecode, LtcDecodeStatus, LtcDetectionResult};
use crate::Timecode;

/// Decode LTC from a WAV file via the libltc binding.
/// If channel 0 is (near-)silent, the channel carrying signal is decoded
/// instead; a non-silent channel 0 is always kept.
pub fn decode_ltc_from_wav_libltc(path: &Path, fps: f64, drop_frame: bool, cancel: Option<&AtomicBool>) -> Result<LtcDetectionResult, LtcDecodeError> {
    let start = Instant::now();

    let mut reader = hound::WavReader::open(path)
        .map_err(|e| LtcDecodeError::Failed(format!("Failed to open WAV file: {}", e)))?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;
    let channels = spec.channels as usize;

    info!(
        "Decoding LTC with libltc from: {} ({} Hz, {} ch)",
        path.display(),
        sample_rate,
        channels
    );

    if spec.bits_per_sample != 16 || spec.sample_format != hound::SampleFormat::Int {
        return if spec.sample_format == hound::SampleFormat::Int {
            // Typed payload; Display renders the former prose byte-identically.
            Err(LtcDecodeError::UnsupportedBitDepth { bits: spec.bits_per_sample })
        } else {
            Err(LtcDecodeError::Failed(format!(
                "libltc decoder requires 16-bit integer PCM WAV (got {} bit {:?})",
                spec.bits_per_sample, spec.sample_format
            )))
        };
    }

    if let Some(c) = cancel {
        if c.load(Ordering::Relaxed) {
            return Err(LtcDecodeError::Cancelled);
        }
    }

    let sample_data: Vec<i16> = reader.samples::<i16>().filter_map(|s| s.ok()).collect();
    let total_samples = sample_data.len() / channels;

    if total_samples == 0 {
        warn!("LTC decode (libltc): audio file contains no samples: {}", path.display());
        return Ok(LtcDetectionResult::error("Audio file contains no samples"));
    }

    decode_ltc_samples_libltc(&sample_data, channels, sample_rate, fps, drop_frame, start, cancel)
}

pub(crate) fn decode_ltc_samples_libltc(
    sample_data: &[i16],
    channels: usize,
    sample_rate: u32,
    fps: f64,
    drop_frame: bool,
    start: Instant,
    cancel: Option<&AtomicBool>,
) -> Result<LtcDetectionResult, LtcDecodeError> {
    let total_samples = sample_data.len() / channels;
    if total_samples == 0 {
        return Ok(LtcDetectionResult::error("Audio buffer contains no samples"));
    }

    // If channel 0 is (near-)silent, the channel carrying signal is decoded
    // instead (see `find_active_channel`); the file is already fully
    // interleaved in memory, so the peak scan is a cheap pass over it.
    let active_channel = find_active_channel(sample_data, channels);

    let initial_apv = (sample_rate as f64 / 25.0).ceil() as i32;
    let config = LTCDecoderConfig {
        initial_apv,
        queue_size: 64,
    };

    let mut decoder = LTCDecoder::try_new(&config)
        .map_err(|e| LtcDecodeError::Failed(format!("Failed to create libltc decoder: {:?}", e)))?;

    let timecodes = decode_all_chunks(
        &mut decoder, sample_data, channels, active_channel, sample_rate, cancel,
    )?;

    let total_duration = total_samples as f64 / sample_rate as f64;
    debug!(
        "libltc decode: read {:.2}s of audio ({} samples)",
        total_duration, total_samples
    );

    let result = assemble_result(
        timecodes,
        &AssembleStats {
            fps,
            drop_frame,
            total_duration,
            sample_rate,
            initial_apv,
            queue_length: decoder.queue_length(),
            processing_time: start.elapsed(),
        },
    );

    info!(
        "libltc decode complete: {} valid / {} possible ({:.1}%) in {:.1}ms, first_ltc_timecode_secs={:.3}s",
        result.valid_frames,
        result.total_possible_frames,
        result.avg_confidence * 100.0,
        result.processing_time_ms,
        result.first_ltc_timecode_secs,
    );

    Ok(result)
}

/// Peak-scan the interleaved buffer and pick the channel carrying signal
/// (channel 0 when it is not near-silent). Mono is always channel 0.
fn find_active_channel(sample_data: &[i16], channels: usize) -> usize {
    if channels == 1 {
        return 0;
    }
    let mut peaks = vec![0f32; channels];
    for (i, &s) in sample_data.iter().enumerate() {
        let v = (s as f32 / 32768.0).abs();
        let ch = i % channels;
        if v > peaks[ch] {
            peaks[ch] = v;
        }
    }
    let picked = crate::ltc_decoder::pick_active_channel(&peaks);
    if picked != 0 {
        info!("libltc decode: channel 0 is silent, decoding channel {}", picked);
    }
    picked
}

/// Convert one libltc extended frame into our [`FrameTimecode`].
fn frame_timecode_from(frame_ext: &LTCFrameExt, frame_index: u32, sample_rate: u32) -> FrameTimecode {
    let ltc = frame_ext.ltc();
    let tc = ltc.to_timecode(LtcBgFlags::default());
    FrameTimecode {
        frame_index,
        timecode: Timecode {
            hours: tc.hours() as u32,
            minutes: tc.minutes() as u32,
            seconds: tc.seconds() as u32,
            frames: tc.frame() as u32,
        },
        timecode_secs: frame_ext.off_start() as f64 / sample_rate as f64,
    }
}

/// Feed the interleaved buffer into the decoder in fixed-size chunks
/// (deinterleaved at `active_channel`; for mono that is the samples
/// themselves) and collect every decoded frame. Honours `cancel`.
fn decode_all_chunks(
    decoder: &mut LTCDecoder,
    sample_data: &[i16],
    channels: usize,
    active_channel: usize,
    sample_rate: u32,
    cancel: Option<&AtomicBool>,
) -> Result<Vec<FrameTimecode>, LtcDecodeError> {
    const CHUNK_SIZE: usize = 8192;
    let mut timecodes: Vec<FrameTimecode> = Vec::new();
    let mut frame_index: u32 = 0;
    let mut sample_pos: i64 = 0;

    for chunk_start in (0..sample_data.len()).step_by(CHUNK_SIZE * channels) {
        if let Some(c) = cancel {
            if c.load(Ordering::Relaxed) {
                return Err(LtcDecodeError::Cancelled);
            }
        }
        let chunk_end = (chunk_start + CHUNK_SIZE * channels).min(sample_data.len());
        let raw_chunk = &sample_data[chunk_start..chunk_end];
        let mono: Vec<i16> = raw_chunk.chunks(channels).map(|ch| ch[active_channel]).collect();
        decoder.write_i16(&mono, sample_pos);
        sample_pos += mono.len() as i64;
        while let Some(frame_ext) = decoder.read() {
            timecodes.push(frame_timecode_from(&frame_ext, frame_index, sample_rate));
            frame_index += 1;
        }
    }

    // Drain any remaining frames
    while let Some(frame_ext) = decoder.read() {
        timecodes.push(frame_timecode_from(&frame_ext, frame_index, sample_rate));
        frame_index += 1;
    }

    Ok(timecodes)
}

/// Decoder pass statistics threaded into `assemble_result` alongside the
/// decoded timecodes.
struct AssembleStats {
    fps: f64,
    drop_frame: bool,
    total_duration: f64,
    sample_rate: u32,
    initial_apv: i32,
    queue_length: i32,
    processing_time: Duration,
}

/// Build the final [`LtcDetectionResult`] from the decoded timecodes:
/// confidence on the 0.0–1.0 fraction scale (same as the builtin decoder),
/// thresholded status, detail lines, coherent-first-timecode alignment and
/// the quality report.
fn assemble_result(timecodes: Vec<FrameTimecode>, stats: &AssembleStats) -> LtcDetectionResult {
    let AssembleStats { fps, drop_frame, total_duration, sample_rate, initial_apv, queue_length, processing_time } = *stats;
    let processing_time_ms = processing_time.as_secs_f64() * 1000.0;

    info!("LTC decode (+{:.1}s): using specified FPS {:.2} (drop_frame={})",
        processing_time.as_secs_f64(), fps, drop_frame);

    let valid_count = timecodes.len() as u32;
    let total_possible_frames = (total_duration * fps).round() as u32;
    // Same 0.0–1.0 fraction scale as the builtin decoder (see
    // LtcDetectionResult::avg_confidence); consumers multiply by 100.
    let avg_confidence = if total_possible_frames > 0 {
        (valid_count as f32 / total_possible_frames as f32).min(1.0)
    } else {
        0.0
    };

    let status = if avg_confidence >= CONFIDENCE_SUCCESS_THRESHOLD {
        LtcDecodeStatus::Success
    } else if avg_confidence >= CONFIDENCE_LOW_THRESHOLD {
        LtcDecodeStatus::LowConfidence
    } else {
        LtcDecodeStatus::NoSyncWord
    };

    let first_secs = timecodes
        .first()
        .map(|ft| ft.timecode_secs)
        .unwrap_or(0.0);

    let details = vec![
        format!("libltc decoder: using {:.2} fps", fps),
        format!("initial_apv={}", initial_apv),
        format!("libltc queue length: {}", queue_length),
    ];

    let mut result = LtcDetectionResult {
        status,
        detected_fps: fps as f32,
        drop_frame,
        total_possible_frames,
        valid_frames: valid_count,
        timecodes,
        avg_confidence,
        details,
        total_audio_duration_secs: total_duration,
        sample_rate,
        processing_time_ms,
        first_ltc_timecode_secs: first_secs,
        quality: None,
        chunk_summaries: Vec::new(),
    };

    apply_coherent_first_timecode(&mut result);
    result.quality = compute_ltc_quality(&result);
    result
}

#[cfg(test)]
mod tests {
    use crate::ChannelSel;
    use super::*;

    // ── Helper: generate synthetic LTC samples for testing ──────────────

    /// Generate mono i16 LTC samples using the same encoding as the main codebase
    /// (delegates to `generate_ltc_frame_stereo`, then extracts the left channel).
    fn synthesize_ltc_samples_i16(
        timecodes: &[Timecode],
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        volume: f32,
    ) -> Vec<i16> {
        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;
        let mut samples = Vec::with_capacity(samples_per_frame * timecodes.len());
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

        for tc in timecodes {
            frame_buf.fill(0.0);
            crate::generate_ltc_frame_stereo(
                tc,
                drop_frame,
                samples_per_frame,
                samples_per_bit,
                volume,
                ChannelSel::Left,
                &mut last_level,
                &mut frame_buf,
            );
            // Extract left channel (even indices in stereo interleaved buffer)
            for ch in frame_buf.chunks(2) {
                let clamped = ch[0].clamp(-1.0, 1.0);
                samples.push((clamped * i16::MAX as f32) as i16);
            }
        }
        samples
    }

    // ── LtcDetectionResult::error tests ─────────────────────────────────

    #[test]
    fn test_error_result_contains_message() {
        let r = LtcDetectionResult::error("test error");
        assert_eq!(r.status, LtcDecodeStatus::Error { message: "test error".to_string() });
    }

    #[test]
    fn test_error_result_empty_message() {
        let r = LtcDetectionResult::error("");
        assert!(matches!(r.status, LtcDecodeStatus::Error { ref message } if message.is_empty()));
    }

    #[test]
    fn test_error_result_zeroed_fields() {
        let r = LtcDetectionResult::error("err");
        assert_eq!(r.detected_fps, 0.0);
        assert!(!r.drop_frame);
        assert_eq!(r.total_possible_frames, 0);
        assert_eq!(r.valid_frames, 0);
        assert!(r.timecodes.is_empty());
        assert_eq!(r.avg_confidence, 0.0);
        assert!(r.details.is_empty());
        assert_eq!(r.total_audio_duration_secs, 0.0);
        assert_eq!(r.sample_rate, 0);
        assert_eq!(r.processing_time_ms, 0.0);
        assert_eq!(r.first_ltc_timecode_secs, 0.0);
    }

    #[test]
    fn test_error_result_from_string() {
        let r = LtcDetectionResult::error("permission denied".to_string());
        assert_eq!(r.status, LtcDecodeStatus::Error { message: "permission denied".to_string() });
    }

    #[test]
    fn decode_ltc_from_wav_libltc_pre_cancelled_returns_cancelled_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("pre-cancelled.wav");
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let samples = synthesize_ltc_samples_i16(&tcs, 25.0, false, 48000, 0.5);
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for s in &samples {
            writer.write_sample(*s).unwrap();
        }
        writer.finalize().unwrap();

        let cancel = AtomicBool::new(true);
        let err = decode_ltc_from_wav_libltc(&path, 25.0, false, Some(&cancel))
            .expect_err("pre-cancelled libltc decode must not produce a result");
        assert_eq!(err, LtcDecodeError::Cancelled);
    }

    // ── decode_ltc_samples_libltc ──────────────────────────────────────

    #[test]
    fn test_decode_ltc_samples_libltc_empty_buffer() {
        let samples = vec![];
        let result = decode_ltc_samples_libltc(&samples, 1, 48000, 25.0, false, Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_samples_libltc_mono_25fps() {
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let samples = synthesize_ltc_samples_i16(&tcs, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples_libltc(&samples, 1, 48000, 25.0, false, Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for 25fps mono, got {:?} (valid={})", result.status, result.valid_frames);
        // 25 synthesized frames; libltc loses only the final frame (tail
        // validation) → 24 is the measured steady state. Deliberately a
        // floor, not an equality: a decoder improvement (decoding the tail)
        // must keep this green.
        assert!(result.valid_frames >= 24,
            "expected at least 24 valid frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_samples_libltc_stereo_extracts_left() {
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        // Generate mono LTC samples
        let mono = synthesize_ltc_samples_i16(&tcs, 25.0, false, 48000, 0.5);
        // Interleave with silence on right channel
        let mut stereo = Vec::with_capacity(mono.len() * 2);
        for &s in &mono {
            stereo.push(s);
            stereo.push(0i16); // right channel silence
        }
        let result = decode_ltc_samples_libltc(&stereo, 2, 48000, 25.0, false, Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for stereo LTC, got {:?}", result.status);
        // 25 synthesized frames; libltc loses only the final frame (tail
        // validation) → 24 is the measured steady state. Deliberately a
        // floor, not an equality: a decoder improvement (decoding the tail)
        // must keep this green.
        assert!(result.valid_frames >= 24,
            "expected at least 24 valid frames from stereo, got {}", result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_samples_libltc_stereo_extracts_right() {
        // Mirror of ..._extracts_left: mono LTC on the *right*, silence on
        // the left — the silent ch0 must not prevent decoding.
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let mono = synthesize_ltc_samples_i16(&tcs, 25.0, false, 48000, 0.5);
        let mut stereo = Vec::with_capacity(mono.len() * 2);
        for &s in &mono {
            stereo.push(0i16); // left channel silence
            stereo.push(s);
        }
        let result = decode_ltc_samples_libltc(&stereo, 2, 48000, 25.0, false, Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success for right-channel LTC, got {:?}", result.status);
        // 25 synthesized frames; libltc loses only the final frame (tail
        // validation) → 24 is the measured steady state. Deliberately a
        // floor, not an equality: a decoder improvement (decoding the tail)
        // must keep this green.
        assert!(result.valid_frames >= 24,
            "expected at least 24 valid frames from right channel, got {}", result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_samples_libltc_too_short() {
        // Only 100 samples — not enough to form a full frame
        let samples: Vec<i16> = vec![1000, -1000, 500, -500, 200, -200];
        let result = decode_ltc_samples_libltc(&samples, 1, 48000, 25.0, false, Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::NoSyncWord),
            "expected NoSyncWord for too-short buffer, got {:?}", result.status);
        assert_eq!(result.valid_frames, 0);
    }

    #[test]
    fn test_decode_ltc_samples_libltc_24fps() {
        let tcs: Vec<Timecode> = (0..24).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let samples = synthesize_ltc_samples_i16(&tcs, 24.0, false, 48000, 0.5);
        let result = decode_ltc_samples_libltc(&samples, 1, 48000, 24.0, false, Instant::now(), None).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for 24fps, got {:?}", result.status);
    }

    #[test]
    fn test_decode_ltc_samples_libltc_44100hz() {
        let tcs: Vec<Timecode> = (0..25).map(|i| Timecode {
            hours: 0, minutes: 0, seconds: 0, frames: i as u32,
        }).collect();
        let samples = synthesize_ltc_samples_i16(&tcs, 25.0, false, 44100, 0.5);
        let result = decode_ltc_samples_libltc(&samples, 1, 44100, 25.0, false, Instant::now(), None).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error at 44.1kHz, got {:?}", result.status);
    }

    #[test]
    fn test_decode_ltc_samples_libltc_different_start_tc() {
        let tcs = [Timecode { hours: 10, minutes: 15, seconds: 30, frames: 12 }];
        // Need enough samples for libltc to detect (just 1 frame may not be enough)
        // Repeat the same timecode a few times
        let tcs_rep: Vec<Timecode> = std::iter::repeat(tcs[0]).take(10).collect();
        let samples = synthesize_ltc_samples_i16(&tcs_rep, 25.0, false, 48000, 0.5);
        let result = decode_ltc_samples_libltc(&samples, 1, 48000, 25.0, false, Instant::now(), None).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?}", result.status);
        assert!(!result.timecodes.is_empty(), "frames must be decoded");
        // The fixture repeats one TC ×10; every decoded frame must carry it.
        for ftc in &result.timecodes {
            assert_eq!(ftc.timecode, tcs[0],
                "decoded frame {} must equal the synthesized start TC", ftc.frame_index);
        }
    }

    // ── find_active_channel / assemble_result (pure helpers) ───────────

    #[test]
    fn find_active_channel_mono_is_always_zero() {
        assert_eq!(find_active_channel(&[], 1), 0);
        assert_eq!(find_active_channel(&[0, 0, 0], 1), 0);
        assert_eq!(find_active_channel(&[100, -100], 1), 0);
    }

    #[test]
    fn find_active_channel_picks_signal_side() {
        // Loud signal on the right, silence on the left.
        let stereo: Vec<i16> = (0..100).flat_map(|_| [0i16, 10_000]).collect();
        assert_eq!(find_active_channel(&stereo, 2), 1);
        // Loud signal on the left, silence on the right → channel 0 kept.
        let stereo_left: Vec<i16> = (0..100).flat_map(|_| [10_000i16, 0]).collect();
        assert_eq!(find_active_channel(&stereo_left, 2), 0);
    }

    #[test]
    fn find_active_channel_both_silent_stays_on_zero() {
        let silent: Vec<i16> = vec![0; 200];
        assert_eq!(find_active_channel(&silent, 2), 0);
    }

    #[test]
    fn assemble_result_thresholds_confidence_into_status() {
        // 24 valid of 25 possible = 96% ≥ success threshold.
        let tcs: Vec<FrameTimecode> = (0..24u32).map(|i| FrameTimecode {
            frame_index: i,
            timecode: Timecode { hours: 0, minutes: 0, seconds: 0, frames: i },
            timecode_secs: i as f64 / 25.0,
        }).collect();
        let stats = AssembleStats {
            fps: 25.0,
            drop_frame: false,
            total_duration: 1.0,
            sample_rate: 48_000,
            initial_apv: 1920,
            queue_length: 0,
            processing_time: Duration::from_millis(5),
        };
        let r = assemble_result(tcs, &stats);
        assert!(matches!(r.status, LtcDecodeStatus::Success), "got {:?}", r.status);
        assert_eq!(r.valid_frames, 24);
        assert_eq!(r.total_possible_frames, 25);
        assert!(r.avg_confidence > 0.9);
        assert_eq!(r.first_ltc_timecode_secs, 0.0);
        assert!(r.quality.is_some(), "quality report must be computed");
        assert!(!r.details.is_empty());
        assert_eq!(r.sample_rate, 48_000);
    }

    #[test]
    fn assemble_result_empty_timecodes_is_nosyncword_with_quality() {
        let stats = AssembleStats {
            fps: 25.0,
            drop_frame: true,
            total_duration: 1.0,
            sample_rate: 48_000,
            initial_apv: 1920,
            queue_length: 3,
            processing_time: Duration::from_millis(1),
        };
        let r = assemble_result(Vec::new(), &stats);
        assert!(matches!(r.status, LtcDecodeStatus::NoSyncWord), "got {:?}", r.status);
        assert_eq!(r.valid_frames, 0);
        assert_eq!(r.first_ltc_timecode_secs, 0.0);
        assert!(r.drop_frame);
        // details still carry the three libltc lines
        assert_eq!(r.details.len(), 3);
    }

    #[test]
    fn test_decode_ltc_from_wav_libltc_rejects_non_16bit() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test_32bit.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0i32).unwrap();
        writer.finalize().unwrap();

        let result = decode_ltc_from_wav_libltc(&path, 25.0, false, None);
        assert!(result.is_err(), "expected error for non-16-bit WAV");
        let err = result.unwrap_err();
        assert_eq!(err, LtcDecodeError::UnsupportedBitDepth { bits: 32 });
    }
}