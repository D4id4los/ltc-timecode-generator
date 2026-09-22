pub mod audio_output;

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

pub mod ltc_decoder;
pub mod ltc_decoder_libltc;
pub mod ltc_encoder;

pub use ltc_encoder::{get_ltc_bits, increment_timecode, generate_ltc_frame_stereo};

// ── Re-exports from audio_output ─────────────────────────────────────────

pub use audio_output::{
    AudioCore,
    list_audio_devices,
    is_transient_audio_error,
    is_permanent_device_error,
    suggest_sample_rate,
    SAMPLE_RATE_OPTIONS,
};

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timecode {
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
    pub frames: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AudioEvent {
    StreamError(String),
    StreamDied,
    StreamRecovering { attempt: u8 },
    StreamDead,
    RecoveryNeeded { reason: String },
    Underrun,
    FramesDropped { total: u64 },
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub formats: Vec<String>,
    pub channels_min: u16,
    pub channels_max: u16,
    pub sample_rate_min: u32,
    pub sample_rate_max: u32,
    pub buffer_min: u32,
    pub buffer_max: u32,
}

// ── Chunked LTC decode infrastructure ─────────────────────────────────────

/// Configuration for chunked WAV reading.
pub struct DecodeConfig {
    /// Target raw-audio chunk size in bytes (~50MB).
    pub chunk_size_bytes: u64,
    /// Overlap between adjacent chunks in seconds (~2 seconds).
    pub overlap_seconds: f64,
}

impl Default for DecodeConfig {
    fn default() -> Self {
        Self {
            chunk_size_bytes: 50_000_000,  // 50 MB
            overlap_seconds: 2.0,
        }
    }
}

/// Shared progress state for a chunked decode operation.
pub struct DecodeProgress {
    pub chunks_total: usize,
    pub chunks_completed: Arc<AtomicUsize>,
    pub cancel_flag: Arc<AtomicBool>,
}

impl DecodeProgress {
    pub fn new(chunks_total: usize) -> Self {
        Self {
            chunks_total,
            chunks_completed: Arc::new(AtomicUsize::new(0)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn percent(&self) -> f32 {
        if self.chunks_total == 0 {
            return 1.0;
        }
        self.chunks_completed.load(Ordering::Relaxed) as f32 / self.chunks_total as f32
    }

    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }
}

/// Low-level WAV chunk reader that reads from a data section offset without
/// loading the entire file into memory.
pub struct WavChunkReader {
    file: std::fs::File,
    spec: hound::WavSpec,
    data_start: u64,
    data_len: u64,
    total_mono_samples: usize,
    channels: usize,
    bytes_per_sample: u64,
}

impl WavChunkReader {
    /// Open a WAV file, parse the header, and prepare for chunked reading.
    pub fn open(path: &Path) -> Result<(Self, std::time::Instant), String> {
        let start = std::time::Instant::now();
        let file = std::fs::File::open(path)
            .map_err(|e| format!("Failed to open WAV file: {}", e))?;

        let (spec, data_offset, data_len) = {
            let tmp = file.try_clone()
                .map_err(|e| format!("Failed to clone file handle: {}", e))?;
            let reader = hound::WavReader::new(tmp)
                .map_err(|e| format!("Failed to read WAV header: {}", e))?;
            let spec = reader.spec();
            let mut inner = reader.into_inner();
            let offset = inner.stream_position()
                .map_err(|e| format!("Failed to get data offset: {}", e))?;
            let file_len = inner.seek(SeekFrom::End(0))
                .map_err(|e| format!("Failed to seek: {}", e))?;
            let data_len = file_len.saturating_sub(offset);
            (spec, offset, data_len)
        };

        let bytes_per_sample = (spec.bits_per_sample / 8) as u64;
        let channels = spec.channels as usize;
        let total_mono_samples = (data_len / bytes_per_sample / spec.channels as u64) as usize;

        info!("WavChunkReader: {} ({} Hz, {} ch, {} bit, {:.2}s, data_offset={}, data_len={})",
            path.display(), spec.sample_rate, channels, spec.bits_per_sample,
            total_mono_samples as f64 / spec.sample_rate as f64,
            data_offset, data_len);

        Ok((Self {
            file,
            spec,
            data_start: data_offset,
            data_len,
            total_mono_samples,
            channels,
            bytes_per_sample,
        }, start))
    }

    pub fn spec(&self) -> &hound::WavSpec { &self.spec }
    pub fn sample_rate(&self) -> u32 { self.spec.sample_rate }
    pub fn channels(&self) -> usize { self.channels }
    pub fn total_mono_samples(&self) -> usize { self.total_mono_samples }

    /// Read a range of mono samples (first channel) from the file.
    /// `start_sample` and `num_samples` are in mono (first-channel) sample units.
    /// Returns a `Vec<f32>` for the builtin decoder.
    pub fn read_mono_samples_f32(&mut self, start_sample: usize, num_samples: usize) -> Result<Vec<f32>, String> {
        let byte_offset = self.data_start + (start_sample * self.channels) as u64 * self.bytes_per_sample;
        let bytes_to_read = num_samples * self.channels * self.bytes_per_sample as usize;
        let max_bytes = self.data_len as usize - ((start_sample * self.channels) as u64 * self.bytes_per_sample).min(self.data_len) as usize;
        let bytes_to_read = bytes_to_read.min(max_bytes);

        self.file.seek(SeekFrom::Start(byte_offset))
            .map_err(|e| format!("Failed to seek: {}", e))?;

        let mut raw = vec![0u8; bytes_to_read];
        let mut pos = 0;
        while pos < bytes_to_read {
            let n = self.file.read(&mut raw[pos..])
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            if n == 0 { break; }
            pos += n;
        }
        raw.truncate(pos);

        match self.spec.sample_format {
            hound::SampleFormat::Int => {
                let max_val = (1i64 << (self.spec.bits_per_sample - 1)) as f32;
                let samples_per_channel = raw.len() / (self.channels * self.bytes_per_sample as usize);
                let mut result = Vec::with_capacity(samples_per_channel);
                for i in 0..samples_per_channel {
                    let sample_start = i * self.channels * self.bytes_per_sample as usize;
                    let byte_ofs = sample_start;
                    let sample = match self.bytes_per_sample {
                        1 => (raw[byte_ofs] as i32) - 128,
                        2 => i16::from_le_bytes([raw[byte_ofs], raw[byte_ofs + 1]]) as i32,
                        3 => {
                            let b = &raw[byte_ofs..byte_ofs + 3];
                            let val = i32::from_le_bytes([b[0], b[1], b[2], 0]);
                            (val << 8) >> 8
                        }
                        4 => i32::from_le_bytes([raw[byte_ofs], raw[byte_ofs + 1], raw[byte_ofs + 2], raw[byte_ofs + 3]]),
                        _ => return Err(format!("Unsupported bytes per sample: {}", self.bytes_per_sample)),
                    };
                    result.push(sample as f32 / max_val);
                }
                Ok(result)
            }
            hound::SampleFormat::Float => {
                let samples_per_channel = raw.len() / (self.channels * 4);
                let mut result = Vec::with_capacity(samples_per_channel);
                for i in 0..samples_per_channel {
                    let byte_ofs = i * self.channels * 4;
                    let sample = f32::from_le_bytes([
                        raw[byte_ofs], raw[byte_ofs + 1],
                        raw[byte_ofs + 2], raw[byte_ofs + 3],
                    ]);
                    result.push(sample);
                }
                Ok(result)
            }
        }
    }

    /// Read a range of mono samples as `Vec<i16>` for the libltc decoder.
    pub fn read_mono_samples_i16(&mut self, start_sample: usize, num_samples: usize) -> Result<Vec<i16>, String> {
        if self.spec.sample_format != hound::SampleFormat::Int {
            return Err("libltc chunk reader requires integer PCM".to_string());
        }

        let byte_offset = self.data_start + (start_sample * self.channels) as u64 * self.bytes_per_sample;
        let bytes_to_read = num_samples * self.channels * self.bytes_per_sample as usize;
        let max_bytes = self.data_len as usize - ((start_sample * self.channels) as u64 * self.bytes_per_sample).min(self.data_len) as usize;
        let bytes_to_read = bytes_to_read.min(max_bytes);

        self.file.seek(SeekFrom::Start(byte_offset))
            .map_err(|e| format!("Failed to seek: {}", e))?;

        let mut raw = vec![0u8; bytes_to_read];
        let mut pos = 0;
        while pos < bytes_to_read {
            let n = self.file.read(&mut raw[pos..])
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            if n == 0 { break; }
            pos += n;
        }
        raw.truncate(pos);

        let bps = self.bytes_per_sample as usize;
        let num_mono_samples = raw.len() / (self.channels * bps);
        let mut result = Vec::with_capacity(num_mono_samples);
        for i in 0..num_mono_samples {
            let byte_ofs = i * self.channels * bps;
            let sample = match bps {
                1 => ((raw[byte_ofs] as i32) - 128) as i16,
                2 => i16::from_le_bytes([raw[byte_ofs], raw[byte_ofs + 1]]),
                3 => {
                    let b = &raw[byte_ofs..byte_ofs + 3];
                    let val = i32::from_le_bytes([b[0], b[1], b[2], 0]);
                    (val >> 8) as i16
                }
                4 => {
                    let val = i32::from_le_bytes([raw[byte_ofs], raw[byte_ofs + 1], raw[byte_ofs + 2], raw[byte_ofs + 3]]);
                    (val >> 16) as i16
                }
                _ => return Err(format!("Unsupported bytes per sample: {}", bps)),
            };
            result.push(sample);
        }
        Ok(result)
    }
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

    let bytes_per_mono_sample = (channels as u64) * (chunk_reader.spec.bits_per_sample as u64 / 8);
    let chunk_mono_samples = (config.chunk_size_bytes.checked_div(bytes_per_mono_sample)
        .map(|v| v as usize)).unwrap_or(total_mono / 4);
    let overlap_samples = (config.overlap_seconds * sample_rate as f64) as usize;
    let chunk_mono_samples = chunk_mono_samples.max(overlap_samples * 2);

    let mut chunks: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0usize;
    while pos < total_mono {
        let end = (pos + chunk_mono_samples).min(total_mono);
        chunks.push((pos, end));
        if end >= total_mono { break; }
        let next_start = end.saturating_sub(overlap_samples);
        if next_start <= pos { break; }
        pos = next_start;
    }

    let num_chunks = chunks.len();
    info!("decode_ltc_chunked: split into {} chunks ({} mono samples each, overlap={} samples)",
        num_chunks, chunk_mono_samples, overlap_samples);

    if num_chunks == 0 {
        return Ok(LtcDetectionResult::error("No audio data to decode"));
    }

    struct ChunkResult {
        chunk_idx: usize,
        result: Result<LtcDetectionResult, String>,
    }

    let mut chunk_results: Vec<ChunkResult> = Vec::with_capacity(num_chunks);

    let progress_completed = progress.chunks_completed.clone();
    let cancel_flag = progress.cancel_flag.clone();

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_chunks);
        for (chunk_idx, &(start_sample, end_sample)) in chunks.iter().enumerate() {
            if cancel_flag.load(Ordering::Relaxed) {
                info!("decode_ltc_chunked: cancel requested, stopping dispatch at chunk {}", chunk_idx);
                break;
            }

            let num_samples = end_sample - start_sample;
            let cancel_flag = cancel_flag.clone();
            let progress_completed = progress_completed.clone();

            let handle = s.spawn(move || {
                if cancel_flag.load(Ordering::Relaxed) {
                    return ChunkResult {
                        chunk_idx,
                        result: Err("Canceled".to_string()),
                    };
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
                        Ok(samples) => crate::ltc_decoder_libltc::decode_ltc_samples_libltc(
                            &samples, 1, sample_rate, fps, drop_frame, chunk_start,
                        ),
                        Err(e) => Err(format!("Failed to read chunk {}: {}", chunk_idx, e)),
                    }
                } else {
                    match local_reader.read_mono_samples_f32(start_sample, num_samples) {
                        Ok(samples) => crate::ltc_decoder::decode_ltc_samples(
                            &samples, sample_rate, 1, fps, drop_frame, chunk_start,
                        ),
                        Err(e) => Err(format!("Failed to read chunk {}: {}", chunk_idx, e)),
                    }
                };
                let elapsed = chunk_start.elapsed();
                let decoder_name = if use_libltc { "libltc" } else { "builtin" };
                debug!("Chunk {}/{} decoded ({}): {:.1}ms", chunk_idx + 1, num_chunks, decoder_name, elapsed.as_secs_f64() * 1000.0);
                progress_completed.fetch_add(1, Ordering::Relaxed);
                ChunkResult { chunk_idx, result }
            });

            handles.push(handle);
        }

        for handle in handles {
            chunk_results.push(handle.join().expect("chunk decode thread panicked"));
        }
    });

    if cancel_flag.load(Ordering::Relaxed) {
        return Err("Decode canceled by user".to_string());
    }

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
        if avg_confidence >= 0.70 {
            LtcDecodeStatus::Success
        } else if avg_confidence >= 0.30 {
            LtcDecodeStatus::LowConfidence
        } else {
            LtcDecodeStatus::NoSyncWord
        }
    } else {
        LtcDecodeStatus::NoSyncWord
    };

    let processing_time_ms = overall_start.elapsed().as_secs_f64() * 1000.0;

    merged_details.push(format!(
        "Chunked decode: {} chunks, {} valid / {} possible after merge",
        num_chunks, valid_frames, true_total_possible,
    ));

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

    info!("decode_ltc_chunked complete: {} valid / {} possible ({:.1}%) in {:.1}ms",
        result.valid_frames, result.total_possible_frames, result.avg_confidence * 100.0, processing_time_ms);

    Ok(result)
}

// Re-export LTC decoder types for convenience
pub use ltc_decoder::{
    apply_coherent_first_timecode, compute_ltc_quality, decode_ltc_from_wav, decode_ltc_samples,
    find_first_coherent_index, quick_check_ltc, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult, LtcQualityReport,
};
pub use ltc_decoder_libltc::{decode_ltc_from_wav_libltc, decode_ltc_samples_libltc};

/// Decode LTC from a WAV file, selecting the decoder implementation.
pub fn decode_ltc_with_decoder(
    path: &std::path::Path,
    use_libltc: bool,
    fps: f64,
    drop_frame: bool,
) -> Result<LtcDetectionResult, String> {
    if use_libltc {
        decode_ltc_from_wav_libltc(path, fps, drop_frame)
    } else {
        decode_ltc_from_wav(path, fps, drop_frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Timecode ──────────────────────────────────────────────────────────

    #[test]
    fn test_timecode_clone_copy() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let copied = tc;
        assert_eq!(copied, tc);
    }

    #[test]
    fn test_timecode_debug() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let d = format!("{:?}", tc);
        assert!(d.contains("1") || d.contains("hours"));
    }

    #[test]
    fn test_timecode_serialize_deserialize() {
        let tc = Timecode { hours: 10, minutes: 20, seconds: 30, frames: 15 };
        let json = serde_json::to_string(&tc).unwrap();
        let deserialized: Timecode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, tc);
    }

    // ── WavChunkReader 24-bit sign extension ──────────────────────────────

    #[test]
    fn test_wav_chunk_reader_24bit_sign_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test_24bit.wav");

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };

        let test_samples: &[i32] = &[
            0,
            1,
            -1,
            8388607,
            -8388608,
            1234567,
            -1234567,
            48000,
            -48000,
        ];

        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            for &s in test_samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        assert_eq!(reader.sample_rate(), 48000);
        assert_eq!(reader.channels(), 1);
        assert_eq!(reader.total_mono_samples(), test_samples.len());

        let max_val = (1i64 << 23) as f32;
        let read = reader.read_mono_samples_f32(0, test_samples.len()).unwrap();

        assert_eq!(read.len(), test_samples.len(),
            "should read all {} samples", test_samples.len());

        let tolerance = 1.0 / max_val;
        for (i, (&expected_int, &actual_f32)) in test_samples.iter().zip(read.iter()).enumerate() {
            let expected_f32 = expected_int as f32 / max_val;
            let abs_diff = (actual_f32 - expected_f32).abs();
            assert!(abs_diff <= tolerance,
                "sample[{}]: expected {:.10} (from {}), got {:.10}, diff={:.10}",
                i, expected_f32, expected_int, actual_f32, abs_diff);
            if expected_int < 0 {
                assert!(actual_f32 < 0.0,
                    "sample[{}]: expected negative for int={}, got {:.10}",
                    i, expected_int, actual_f32);
            } else if expected_int > 0 {
                assert!(actual_f32 > 0.0,
                    "sample[{}]: expected positive for int={}, got {:.10}",
                    i, expected_int, actual_f32);
            } else {
                assert!((actual_f32).abs() <= tolerance,
                    "sample[{}]: expected zero for int=0, got {:.10}",
                    i, actual_f32);
            }
        }
    }

    // ── DecodeConfig default ───────────────────────────────────────────

    #[test]
    fn test_decode_config_default() {
        let config = DecodeConfig::default();
        assert_eq!(config.chunk_size_bytes, 50_000_000);
        assert!((config.overlap_seconds - 2.0).abs() < 1e-9);
    }

    // ── DecodeProgress ────────────────────────────────────────────────

    #[test]
    fn test_decode_progress_new() {
        let p = DecodeProgress::new(10);
        assert_eq!(p.chunks_total, 10);
        assert!((p.percent() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_partial() {
        let p = DecodeProgress::new(4);
        p.chunks_completed.store(2, Ordering::Relaxed);
        assert!((p.percent() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_complete() {
        let p = DecodeProgress::new(5);
        p.chunks_completed.store(5, Ordering::Relaxed);
        assert!((p.percent() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_zero_total() {
        let p = DecodeProgress::new(0);
        assert!((p.percent() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_cancel() {
        let p = DecodeProgress::new(5);
        assert!(!p.cancel_flag.load(Ordering::Relaxed));
        p.cancel();
        assert!(p.cancel_flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_decode_progress_double_cancel() {
        let p = DecodeProgress::new(5);
        p.cancel();
        p.cancel();
        assert!(p.cancel_flag.load(Ordering::Relaxed));
    }

    // ── WavChunkReader: open errors ───────────────────────────────────

    #[test]
    fn test_wav_chunk_reader_nonexistent_file() {
        let result = WavChunkReader::open(Path::new("/nonexistent/path.wav"));
        assert!(result.is_err());
    }

    // ── WavChunkReader: 16-bit mono reads ─────────────────────────────

    fn write_test_wav_int(
        dir: &tempfile::TempDir,
        name: &str,
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        samples: &[i32],
    ) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for &s in samples {
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn test_wav_chunk_reader_16bit_mono_f32() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![0, 1, -1, 32767, -32768, 12345, -12345];
        let path = write_test_wav_int(&dir, "16bit_mono.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        assert_eq!(reader.sample_rate(), 48000);
        assert_eq!(reader.channels(), 1);
        assert_eq!(reader.total_mono_samples(), test_samples.len());

        let max_val = 32768.0f32;
        let read = reader.read_mono_samples_f32(0, test_samples.len()).unwrap();
        assert_eq!(read.len(), test_samples.len());

        for (i, (&expected_int, &actual_f32)) in test_samples.iter().zip(read.iter()).enumerate() {
            let expected_f32 = expected_int as f32 / max_val;
            let diff = (actual_f32 - expected_f32).abs();
            assert!(diff < 1e-6,
                "sample[{}]: expected {:.10}, got {:.10}, diff={:.10}",
                i, expected_f32, actual_f32, diff);
        }
    }

    #[test]
    fn test_wav_chunk_reader_16bit_mono_i16() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![0, 1, -1, 32767, -32768, 100, -200];
        let path = write_test_wav_int(&dir, "16bit_mono_i16.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_i16(0, test_samples.len()).unwrap();
        assert_eq!(read.len(), test_samples.len());
        for (i, (&expected, &actual)) in test_samples.iter().zip(read.iter()).enumerate() {
            assert_eq!(actual, expected as i16, "sample[{}]: mismatch", i);
        }
    }

    #[test]
    fn test_wav_chunk_reader_16bit_stereo_f32() {
        let dir = tempfile::TempDir::new().unwrap();
        let stereo_samples: Vec<i32> = vec![100, 0, 200, 0, 300, 0, -100, 0, -200, 0];
        let path = write_test_wav_int(&dir, "16bit_stereo.wav", 2, 48000, 16, &stereo_samples);
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        assert_eq!(reader.channels(), 2);
        assert_eq!(reader.total_mono_samples(), 5);

        let read = reader.read_mono_samples_f32(0, 5).unwrap();
        assert_eq!(read.len(), 5, "stereo should extract 5 left-channel samples");
        let max_val = 32768.0;
        assert!((read[0] - 100.0 / max_val).abs() < 1e-6);
        assert!((read[2] - 300.0 / max_val).abs() < 1e-6);
        assert!((read[3] + 100.0 / max_val).abs() < 1e-6);
    }

    #[test]
    fn test_wav_chunk_reader_16bit_stereo_i16() {
        let dir = tempfile::TempDir::new().unwrap();
        let stereo_samples: Vec<i32> = vec![100, 999, 200, 888, 300, 777];
        let path = write_test_wav_int(&dir, "16bit_stereo_i16.wav", 2, 48000, 16, &stereo_samples);
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();

        let read = reader.read_mono_samples_i16(0, 3).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read[0], 100i16);
        assert_eq!(read[1], 200i16);
        assert_eq!(read[2], 300i16);
    }

    #[test]
    fn test_wav_chunk_reader_read_i16_24bit_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![0, 256, -256, 8388607, -8388608, 65536, -65536];
        let expected: Vec<i16> = vec![0, 1, -1, 32767, -32768, 256, -256];
        let path = write_test_wav_int(&dir, "24bit_i16_ok.wav", 1, 48000, 24, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let result = reader.read_mono_samples_i16(0, test_samples.len());
        assert!(result.is_ok(), "expected Ok for 24-bit read, got: {:?}", result);
        let read = result.unwrap();
        assert_eq!(read.len(), expected.len());
        for (i, (&e, &a)) in expected.iter().zip(read.iter()).enumerate() {
            assert_eq!(a, e, "sample[{}]: expected {}, got {}", i, e, a);
        }
    }

    #[test]
    fn test_wav_chunk_reader_read_i16_8bit_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let input_i8: Vec<i8> = vec![0i8, 1, -1, 127, -128];
        let expected: Vec<i16> = vec![0, 1, -1, 127, -128];
        let path = dir.path().join("8bit_i16_ok.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 8,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for &s in &input_i8 {
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let result = reader.read_mono_samples_i16(0, input_i8.len());
        assert!(result.is_ok(), "expected Ok for 8-bit read, got: {:?}", result);
        let read = result.unwrap();
        assert_eq!(read.len(), expected.len());
        for (i, (&e, &a)) in expected.iter().zip(read.iter()).enumerate() {
            assert_eq!(a, e, "sample[{}]: expected {}, got {}", i, e, a);
        }
    }

    #[test]
    fn test_wav_chunk_reader_read_i16_32bit_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![0, 65536, -65536, 2147483647, -2147483648, 16777216, -16777216];
        let expected: Vec<i16> = vec![0, 1, -1, 32767, -32768, 256, -256];
        let path = write_test_wav_int(&dir, "32bit_i16_ok.wav", 1, 48000, 32, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let result = reader.read_mono_samples_i16(0, test_samples.len());
        assert!(result.is_ok(), "expected Ok for 32-bit read, got: {:?}", result);
        let read = result.unwrap();
        assert_eq!(read.len(), expected.len());
        for (i, (&e, &a)) in expected.iter().zip(read.iter()).enumerate() {
            assert_eq!(a, e, "sample[{}]: expected {}, got {}", i, e, a);
        }
    }

    #[test]
    fn test_wav_chunk_reader_read_i16_rejects_float() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("float_i16_reject.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.write_sample(0.0f32).unwrap();
        writer.write_sample(0.5f32).unwrap();
        writer.write_sample(-0.5f32).unwrap();
        writer.finalize().unwrap();

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let result = reader.read_mono_samples_i16(0, 3);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("integer PCM"));
    }

    #[test]
    fn test_wav_chunk_reader_partial_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = (0..100).collect();
        let path = write_test_wav_int(&dir, "partial.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(10, 5).unwrap();
        assert_eq!(read.len(), 5);
        let max_val = 32768.0;
        assert!((read[0] - 10.0 / max_val).abs() < 1e-6);
        assert!((read[4] - 14.0 / max_val).abs() < 1e-6);
    }

    #[test]
    fn test_wav_chunk_reader_read_beyond_end() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![1, 2, 3];
        let path = write_test_wav_int(&dir, "beyond_end.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 100).unwrap();
        assert_eq!(read.len(), 3, "should return only available samples");
    }

    #[test]
    fn test_wav_chunk_reader_empty_range() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![1, 2, 3];
        let path = write_test_wav_int(&dir, "empty_range.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 0).unwrap();
        assert!(read.is_empty());
    }

    // ── WavChunkReader: 8-bit reads ───────────────────────────────────

    #[test]
    fn test_wav_chunk_reader_8bit() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("8bit.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 8,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in -128i8..=127 {
            writer.write_sample(i).unwrap();
        }
        writer.finalize().unwrap();

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 256).unwrap();
        assert_eq!(read.len(), 256);
        let max_val = 128.0f32;
        assert!((read[0] + 1.0).abs() < 0.01, "first sample (-128) should be -1.0, got {}", read[0]);
        assert!((read[128] - 0.0).abs() < 1e-4, "sample at zero should be 0.0, got {}", read[128]);
        assert!((read[255] - 127.0 / max_val).abs() < 1e-4, "last sample (127) should be ~0.992, got {}", read[255]);
    }

    // ── WavChunkReader: float format reads ────────────────────────────

    #[test]
    fn test_wav_chunk_reader_float32() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("float.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            writer.write_sample(0.5f32).unwrap();
            writer.write_sample(-0.25f32).unwrap();
            writer.write_sample(1.0f32).unwrap();
            writer.write_sample(-1.0f32).unwrap();
            writer.finalize().unwrap();
        }
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 4).unwrap();
        assert_eq!(read.len(), 4);
        assert!((read[0] - 0.5).abs() < 1e-6);
        assert!((read[1] + 0.25).abs() < 1e-6);
        assert!((read[2] - 1.0).abs() < 1e-6);
        assert!((read[3] + 1.0).abs() < 1e-6);
    }

    // ── WavChunkReader: unsupported bits_per_sample ───────────────────

    #[test]
    fn test_wav_chunk_reader_unsupported_bps() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("unsupported.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 17,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let result = hound::WavWriter::create(&path, spec);
            if let Ok(mut writer) = result {
                writer.write_sample(0i32).unwrap();
                writer.finalize().unwrap();
                let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
                let read = reader.read_mono_samples_f32(0, 1);
                assert!(read.is_err() || read.unwrap().len() <= 1);
            }
        }
    }

    // ── decode_ltc_chunked merge tests ──────────────────────────────

    fn chunk_count_for_config(
        total_mono: usize,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        config: &DecodeConfig,
    ) -> usize {
        let bytes_per_mono = (channels as u64) * (bits_per_sample as u64 / 8);
        let chunk_mono = (config.chunk_size_bytes / bytes_per_mono.max(1)) as usize;
        let overlap_samples = (config.overlap_seconds * sample_rate as f64) as usize;
        let chunk_mono = chunk_mono.max(overlap_samples * 2);
        if total_mono <= chunk_mono + overlap_samples {
            return 1;
        }
        let mut count = 0usize;
        let mut pos = 0usize;
        while pos < total_mono {
            count += 1;
            let end = (pos + chunk_mono).min(total_mono);
            if end >= total_mono { break; }
            let next = end.saturating_sub(overlap_samples);
            if next <= pos || next >= total_mono { break; }
            pos = next;
        }
        count
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
        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, fps, false).unwrap();

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
        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, fps, false).unwrap();

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
        let direct = crate::ltc_decoder_libltc::decode_ltc_from_wav_libltc(&path, fps, false).unwrap();

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
                "both",
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
                "both",
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

        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, 25.0, false).unwrap();

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