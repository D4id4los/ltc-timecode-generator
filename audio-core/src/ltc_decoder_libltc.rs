use std::path::Path;
use std::time::Instant;

use libltc_rs::prelude::*;
use log::{debug, info, warn};

use crate::ltc_decoder::{FrameTimecode, LtcDecodeStatus, LtcDetectionResult};
use crate::Timecode;

pub fn decode_ltc_from_wav_libltc(path: &Path) -> Result<LtcDetectionResult, String> {
    let start = Instant::now();

    let mut reader = hound::WavReader::open(path)
        .map_err(|e| format!("Failed to open WAV file: {}", e))?;
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
        return Err(format!(
            "libltc decoder requires 16-bit integer PCM WAV (got {} bit {:?})",
            spec.bits_per_sample, spec.sample_format
        ));
    }

    let initial_apv = (sample_rate as f64 / 25.0).ceil() as i32;
    let config = LTCDecoderConfig {
        initial_apv,
        queue_size: 64,
    };

    let mut decoder = LTCDecoder::try_new(&config)
        .map_err(|e| format!("Failed to create libltc decoder: {:?}", e))?;

    let total_samples: usize;
    let chunk_size: usize = 8192;
    let mut sample_pos: i64 = 0;
    let mut timecodes: Vec<FrameTimecode> = Vec::new();
    let mut frame_index: u32 = 0;

    if channels == 1 {
        let all_samples: Vec<i16> = reader.samples::<i16>().filter_map(|s| s.ok()).collect();
        total_samples = all_samples.len();
        if total_samples == 0 {
            warn!("LTC decode (libltc): audio file contains no samples: {}", path.display());
            return Ok(error_result("Audio file contains no samples"));
        }
        for chunk in all_samples.chunks(chunk_size) {
            decoder.write_i16(chunk, sample_pos);
            sample_pos += chunk.len() as i64;
            while let Some(frame_ext) = decoder.read() {
                let ltc = frame_ext.ltc();
                let tc = ltc.to_timecode(LtcBgFlags::default());
                timecodes.push(FrameTimecode {
                    frame_index,
                    timecode: Timecode {
                        hours: tc.hours() as u32,
                        minutes: tc.minutes() as u32,
                        seconds: tc.seconds() as u32,
                        frames: tc.frame() as u32,
                    },
                    timecode_secs: frame_ext.off_start() as f64 / sample_rate as f64,
                });
                frame_index += 1;
            }
        }
    } else {
        let sample_data: Vec<i16> = reader.samples::<i16>().filter_map(|s| s.ok()).collect();
        total_samples = sample_data.len() / channels;
        if total_samples == 0 {
            warn!("LTC decode (libltc): audio file contains no samples: {}", path.display());
            return Ok(error_result("Audio file contains no samples"));
        }
        for chunk_start in (0..sample_data.len()).step_by(chunk_size * channels) {
            let chunk_end = (chunk_start + chunk_size * channels).min(sample_data.len());
            let raw_chunk = &sample_data[chunk_start..chunk_end];
            let left: Vec<i16> = raw_chunk.chunks(channels).map(|ch| ch[0]).collect();
            decoder.write_i16(&left, sample_pos);
            sample_pos += left.len() as i64;
            while let Some(frame_ext) = decoder.read() {
                let ltc = frame_ext.ltc();
                let tc = ltc.to_timecode(LtcBgFlags::default());
                timecodes.push(FrameTimecode {
                    frame_index,
                    timecode: Timecode {
                        hours: tc.hours() as u32,
                        minutes: tc.minutes() as u32,
                        seconds: tc.seconds() as u32,
                        frames: tc.frame() as u32,
                    },
                    timecode_secs: frame_ext.off_start() as f64 / sample_rate as f64,
                });
                frame_index += 1;
            }
        }
    }

    // Drain any remaining frames
    while let Some(frame_ext) = decoder.read() {
        let ltc = frame_ext.ltc();
        let tc = ltc.to_timecode(LtcBgFlags::default());
        timecodes.push(FrameTimecode {
            frame_index,
            timecode: Timecode {
                hours: tc.hours() as u32,
                minutes: tc.minutes() as u32,
                seconds: tc.seconds() as u32,
                frames: tc.frame() as u32,
            },
            timecode_secs: frame_ext.off_start() as f64 / sample_rate as f64,
        });
        frame_index += 1;
    }

    let total_duration = total_samples as f64 / sample_rate as f64;
    debug!(
        "libltc decode: read {:.2}s of audio ({} samples)",
        total_duration, total_samples
    );

    let valid_count = timecodes.len() as u32;
    let processing_time_ms = start.elapsed().as_secs_f64() * 1000.0;

    let (detected_fps, drop_frame, total_possible_frames, avg_confidence, details) =
        if valid_count >= 2 {
            let first_off = timecodes[0].timecode_secs;
            let last_off = timecodes[timecodes.len() - 1].timecode_secs;
            let span = last_off - first_off;
            let raw_inferred = if span > 0.0 {
                (valid_count - 1) as f64 / span
            } else {
                25.0
            };

            let (inferred_fps, df) = snap_to_known_fps(raw_inferred);

            let total_possible = (total_duration * inferred_fps as f64).round() as u32;
            let confidence = if total_possible > 0 {
                (valid_count as f32 / total_possible as f32 * 100.0).min(100.0)
            } else {
                0.0
            };

            (
                inferred_fps,
                df,
                total_possible,
                confidence,
                vec![
                    format!(
                        "libltc decoder: inferred {:.2} fps from {} frames over {:.2}s span",
                        raw_inferred, valid_count, span
                    ),
                    format!("initial_apv={}", initial_apv),
                    format!("libltc queue length: {}", decoder.queue_length()),
                ],
            )
        } else {
            let msg = if valid_count == 0 {
                "libltc did not detect any complete frames".to_string()
            } else {
                "libltc detected only 1 frame — cannot infer FPS".to_string()
            };
            (0.0, false, 0, 0.0, vec![msg])
        };

    let status = if valid_count > 0 {
        LtcDecodeStatus::Success
    } else {
        LtcDecodeStatus::NoSyncWord
    };

    let first_secs = timecodes
        .first()
        .map(|ft| ft.timecode_secs)
        .unwrap_or(0.0);

    let result = LtcDetectionResult {
        status,
        detected_fps,
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
    };

    info!(
        "libltc decode complete: {} valid / {} possible ({:.1}%) in {:.1}ms",
        result.valid_frames,
        result.total_possible_frames,
        result.avg_confidence,
        result.processing_time_ms
    );

    Ok(result)
}

fn snap_to_known_fps(raw: f64) -> (f32, bool) {
    if (raw - 24.0).abs() < 0.5 {
        (24.0, false)
    } else if (raw - 25.0).abs() < 0.5 {
        (25.0, false)
    } else if (raw - 30.0).abs() < 0.5 {
        (30.0, false)
    } else if (raw - 29.97).abs() < 1.0 {
        (29.97, true)
    } else {
        (raw as f32, false)
    }
}

fn error_result(msg: impl Into<String>) -> LtcDetectionResult {
    LtcDetectionResult {
        status: LtcDecodeStatus::Error {
            message: msg.into(),
        },
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