use std::path::Path;
use std::time::Instant;

use libltc_rs::prelude::*;
use log::{debug, info, warn};

use crate::ltc_decoder::{apply_coherent_first_timecode, FrameTimecode, LtcDecodeStatus, LtcDetectionResult};
use crate::Timecode;

pub fn decode_ltc_from_wav_libltc(path: &Path, fps: f64, drop_frame: bool) -> Result<LtcDetectionResult, String> {
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

    let progress_interval: usize;
    if channels == 1 {
        info!("LTC decode (+{:.1}s): reading audio samples from disk...", start.elapsed().as_secs_f64());
        let all_samples: Vec<i16> = reader.samples::<i16>().filter_map(|s| s.ok()).collect();
        total_samples = all_samples.len();
        if total_samples == 0 {
            warn!("LTC decode (libltc): audio file contains no samples: {}", path.display());
            return Ok(error_result("Audio file contains no samples"));
        }
        let total_chunks = total_samples.div_ceil(chunk_size);
        progress_interval = (total_chunks / 10).max(1);
        info!("LTC decode (+{:.1}s): processing {} samples in {} chunks of {}...",
            start.elapsed().as_secs_f64(), total_samples, total_chunks, chunk_size);
        for (chunk_idx, chunk) in all_samples.chunks(chunk_size).enumerate() {
            if chunk_idx % progress_interval == 0 {
                debug!("LTC decode (+{:.1}s):  processed {:.0}% ({}/{})",
                    start.elapsed().as_secs_f64(),
                    (sample_pos as f64 / total_samples as f64) * 100.0,
                    sample_pos, total_samples);
            }
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
        info!("LTC decode (+{:.1}s): reading audio samples from disk...", start.elapsed().as_secs_f64());
        let sample_data: Vec<i16> = reader.samples::<i16>().filter_map(|s| s.ok()).collect();
        total_samples = sample_data.len() / channels;
        if total_samples == 0 {
            warn!("LTC decode (libltc): audio file contains no samples: {}", path.display());
            return Ok(error_result("Audio file contains no samples"));
        }
        let total_chunks = total_samples.div_ceil(chunk_size);
        progress_interval = (total_chunks / 10).max(1);
        info!("LTC decode (+{:.1}s): processing {} samples in {} chunks of {} ({} ch)...",
            start.elapsed().as_secs_f64(), total_samples, total_chunks, chunk_size, channels);
        for (chunk_idx, chunk_start) in (0..sample_data.len()).step_by(chunk_size * channels).enumerate() {
            if chunk_idx % progress_interval == 0 {
                debug!("LTC decode (+{:.1}s):  processed {:.0}% ({}/{})",
                    start.elapsed().as_secs_f64(),
                    (sample_pos as f64 / total_samples as f64) * 100.0,
                    sample_pos, total_samples);
            }
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
    debug!("LTC decode (+{:.1}s): draining {} remaining decoded frames...",
        start.elapsed().as_secs_f64(), decoder.queue_length());
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

    info!("LTC decode (+{:.1}s): using specified FPS {:.2} (drop_frame={})",
        start.elapsed().as_secs_f64(), fps, drop_frame);

    let total_possible_frames = (total_duration * fps).round() as u32;
    let avg_confidence = if total_possible_frames > 0 {
        (valid_count as f32 / total_possible_frames as f32 * 100.0).min(100.0)
    } else {
        0.0
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

    let details = vec![
        format!("libltc decoder: using {:.2} fps", fps),
        format!("initial_apv={}", initial_apv),
        format!("libltc queue length: {}", decoder.queue_length()),
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
    };

    apply_coherent_first_timecode(&mut result);

    info!(
        "libltc decode complete: {} valid / {} possible ({:.1}%) in {:.1}ms, first_ltc_timecode_secs={:.3}s",
        result.valid_frames,
        result.total_possible_frames,
        result.avg_confidence,
        result.processing_time_ms,
        result.first_ltc_timecode_secs,
    );

    Ok(result)
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