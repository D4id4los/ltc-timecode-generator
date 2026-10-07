//! Low-level WAV chunk reader that reads from a data section offset without
//! loading the entire file into memory.

use log::info;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

pub struct WavChunkReader {
    file: std::fs::File,
    spec: hound::WavSpec,
    data_start: u64,
    data_len: u64,
    total_mono_samples: usize,
    channels: usize,
    bytes_per_sample: u64,
    /// Channel extracted by the mono-sample readers (stride filter).
    /// Picked once per file via `scan_channel_peaks` + `pick_active_channel`
    /// before chunk planning, so every chunk decodes the same channel.
    active_channel: usize,
}

/// Sign-extended little-endian integer sample of `bps` bytes at `raw[offset..]`.
/// Single home of the per-bps decode arithmetic previously duplicated between
/// `read_mono_samples_f32` ((v<<8)>>8) and `read_mono_samples_i16` ((v>>8) as i16).
/// For `bps == 1` the value is already in -128..127 (WAV 8-bit is unsigned).
fn decode_le_int_sample(raw: &[u8], offset: usize, bps: usize) -> Result<i32, String> {
    match bps {
        1 => Ok((raw[offset] as i32) - 128),
        2 => Ok(i16::from_le_bytes([raw[offset], raw[offset + 1]]) as i32),
        3 => {
            let b = &raw[offset..offset + 3];
            let val = i32::from_le_bytes([b[0], b[1], b[2], 0]);
            Ok((val << 8) >> 8)
        }
        4 => Ok(i32::from_le_bytes([
            raw[offset],
            raw[offset + 1],
            raw[offset + 2],
            raw[offset + 3],
        ])),
        _ => Err(format!("Unsupported bytes per sample: {}", bps)),
    }
}

impl WavChunkReader {
    /// Open a WAV file, parse the header, and prepare for chunked reading.
    pub fn open(path: &Path) -> Result<(Self, std::time::Instant), String> {
        let start = std::time::Instant::now();
        let file =
            std::fs::File::open(path).map_err(|e| format!("Failed to open WAV file: {}", e))?;

        let (spec, data_offset, data_len) = {
            let tmp = file
                .try_clone()
                .map_err(|e| format!("Failed to clone file handle: {}", e))?;
            let reader = hound::WavReader::new(tmp)
                .map_err(|e| format!("Failed to read WAV header: {}", e))?;
            let spec = reader.spec();
            let mut inner = reader.into_inner();
            let offset = inner
                .stream_position()
                .map_err(|e| format!("Failed to get data offset: {}", e))?;
            let file_len = inner
                .seek(SeekFrom::End(0))
                .map_err(|e| format!("Failed to seek: {}", e))?;
            let data_len = file_len.saturating_sub(offset);
            (spec, offset, data_len)
        };

        let bytes_per_sample = (spec.bits_per_sample / 8) as u64;
        let channels = spec.channels as usize;
        let total_mono_samples = (data_len / bytes_per_sample / spec.channels as u64) as usize;

        info!(
            "WavChunkReader: {} ({} Hz, {} ch, {} bit, {:.2}s, data_offset={}, data_len={})",
            path.display(),
            spec.sample_rate,
            channels,
            spec.bits_per_sample,
            total_mono_samples as f64 / spec.sample_rate as f64,
            data_offset,
            data_len
        );

        Ok((
            Self {
                file,
                spec,
                data_start: data_offset,
                data_len,
                total_mono_samples,
                channels,
                bytes_per_sample,
                active_channel: 0,
            },
            start,
        ))
    }

    /// Like [`WavChunkReader::open`] but extracts `active_channel` in the
    /// mono-sample readers instead of channel 0.
    pub fn open_with_channel(
        path: &Path,
        active_channel: usize,
    ) -> Result<(Self, std::time::Instant), String> {
        let (mut reader, start) = Self::open(path)?;
        reader.active_channel = active_channel;
        Ok((reader, start))
    }

    pub fn spec(&self) -> &hound::WavSpec {
        &self.spec
    }
    pub fn sample_rate(&self) -> u32 {
        self.spec.sample_rate
    }
    pub fn channels(&self) -> usize {
        self.channels
    }
    pub fn total_mono_samples(&self) -> usize {
        self.total_mono_samples
    }

    /// One sequential pass over the data section computing the absolute
    /// peak per channel (buffered reads; no storage cost). The caller feeds
    /// the result to `pick_active_channel` and re-opens with
    /// `open_with_channel` — the channel choice must be made once per file
    /// so every chunk decodes the same channel.
    pub fn scan_channel_peaks(&mut self) -> Result<Vec<f32>, String> {
        let bps = self.bytes_per_sample as usize;
        let frame_bytes = self.channels * bps;
        if frame_bytes == 0 {
            return Ok(vec![0.0]);
        }
        let mut peaks = vec![0f32; self.channels];
        let is_float = self.spec.sample_format == hound::SampleFormat::Float;
        let max_val = if is_float {
            1.0f32
        } else {
            (1i64 << (self.spec.bits_per_sample - 1)) as f32
        };

        self.file
            .seek(SeekFrom::Start(self.data_start))
            .map_err(|e| format!("Failed to seek: {}", e))?;

        let mut buf = vec![0u8; frame_bytes * 8192];
        loop {
            let n = self
                .file
                .read(&mut buf)
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            if n == 0 {
                break;
            }
            let frames = n / frame_bytes;
            for f in 0..frames {
                let base = f * frame_bytes;
                for (ch, peak) in peaks.iter_mut().enumerate() {
                    let v = if is_float {
                        let o = base + ch * 4;
                        f32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]).abs()
                    } else {
                        (decode_le_int_sample(&buf, base + ch * bps, bps)? as f32 / max_val).abs()
                    };
                    if v > *peak {
                        *peak = v;
                    }
                }
            }
        }
        Ok(peaks)
    }

    /// Read the raw interleaved bytes covering `start_sample..start_sample+num_samples`
    /// (mono-sample units), clamped to the data section. Single home of the
    /// seek/read/clamp loop previously duplicated in both sample readers.
    fn read_raw_bytes(
        &mut self,
        start_sample: usize,
        num_samples: usize,
    ) -> Result<Vec<u8>, String> {
        let byte_offset =
            self.data_start + (start_sample * self.channels) as u64 * self.bytes_per_sample;
        let bytes_to_read = num_samples * self.channels * self.bytes_per_sample as usize;
        let max_bytes = self.data_len as usize
            - ((start_sample * self.channels) as u64 * self.bytes_per_sample).min(self.data_len)
                as usize;
        let bytes_to_read = bytes_to_read.min(max_bytes);

        self.file
            .seek(SeekFrom::Start(byte_offset))
            .map_err(|e| format!("Failed to seek: {}", e))?;

        let mut raw = vec![0u8; bytes_to_read];
        let mut pos = 0;
        while pos < bytes_to_read {
            let n = self
                .file
                .read(&mut raw[pos..])
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            if n == 0 {
                break;
            }
            pos += n;
        }
        raw.truncate(pos);
        Ok(raw)
    }

    /// Read a range of mono samples (the reader's active channel, channel 0
    /// unless auto-selected otherwise) from the file.
    /// `start_sample` and `num_samples` are in mono (per-channel) sample units.
    /// Returns a `Vec<f32>` for the builtin decoder.
    pub fn read_mono_samples_f32(
        &mut self,
        start_sample: usize,
        num_samples: usize,
    ) -> Result<Vec<f32>, String> {
        let raw = self.read_raw_bytes(start_sample, num_samples)?;
        let channel = self.active_channel;

        match self.spec.sample_format {
            hound::SampleFormat::Int => {
                let max_val = (1i64 << (self.spec.bits_per_sample - 1)) as f32;
                let samples_per_channel =
                    raw.len() / (self.channels * self.bytes_per_sample as usize);
                let mut result = Vec::with_capacity(samples_per_channel);
                for i in 0..samples_per_channel {
                    let byte_ofs = (i * self.channels + channel) * self.bytes_per_sample as usize;
                    let sample =
                        decode_le_int_sample(&raw, byte_ofs, self.bytes_per_sample as usize)?;
                    result.push(sample as f32 / max_val);
                }
                Ok(result)
            }
            hound::SampleFormat::Float => {
                let samples_per_channel = raw.len() / (self.channels * 4);
                let mut result = Vec::with_capacity(samples_per_channel);
                for i in 0..samples_per_channel {
                    let byte_ofs = (i * self.channels + channel) * 4;
                    let sample = f32::from_le_bytes([
                        raw[byte_ofs],
                        raw[byte_ofs + 1],
                        raw[byte_ofs + 2],
                        raw[byte_ofs + 3],
                    ]);
                    result.push(sample);
                }
                Ok(result)
            }
        }
    }

    /// Read a range of mono samples as `Vec<i16>` for the libltc decoder.
    pub fn read_mono_samples_i16(
        &mut self,
        start_sample: usize,
        num_samples: usize,
    ) -> Result<Vec<i16>, String> {
        if self.spec.sample_format != hound::SampleFormat::Int {
            return Err("libltc chunk reader requires integer PCM".to_string());
        }

        let raw = self.read_raw_bytes(start_sample, num_samples)?;

        let bps = self.bytes_per_sample as usize;
        let bits = bps * 8;
        let num_mono_samples = raw.len() / (self.channels * bps);
        let mut result = Vec::with_capacity(num_mono_samples);
        for i in 0..num_mono_samples {
            let byte_ofs = (i * self.channels + self.active_channel) * bps;
            let sample = decode_le_int_sample(&raw, byte_ofs, bps)?;
            let s16 = if bits >= 16 {
                (sample >> (bits - 16)) as i16
            } else {
                sample as i16
            };
            result.push(s16);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── decode_le_int_sample ──────────────────────────────────────────

    #[test]
    fn test_decode_le_int_sample_24bit_sign_extension() {
        let raw = [0xFF, 0xFF, 0x7F]; // 0x7FFFFF
        assert_eq!(decode_le_int_sample(&raw, 0, 3).unwrap(), 8_388_607);
        let raw = [0x00, 0x00, 0x80]; // 0x800000
        assert_eq!(decode_le_int_sample(&raw, 0, 3).unwrap(), -8_388_608);
    }

    #[test]
    fn test_decode_le_int_sample_8bit() {
        assert_eq!(decode_le_int_sample(&[0], 0, 1).unwrap(), -128);
        assert_eq!(decode_le_int_sample(&[128], 0, 1).unwrap(), 0);
        assert_eq!(decode_le_int_sample(&[255], 0, 1).unwrap(), 127);
    }

    #[test]
    fn test_decode_le_int_sample_16bit() {
        let raw = [0x00, 0x80]; // -32768
        assert_eq!(decode_le_int_sample(&raw, 0, 2).unwrap(), -32768);
        let raw = [0xFF, 0x7F]; // 32767
        assert_eq!(decode_le_int_sample(&raw, 0, 2).unwrap(), 32767);
    }

    #[test]
    fn test_decode_le_int_sample_32bit() {
        let raw = [0x00, 0x00, 0x00, 0x80]; // -2147483648
        assert_eq!(decode_le_int_sample(&raw, 0, 4).unwrap(), -2_147_483_648);
        let raw = [0xFF, 0xFF, 0xFF, 0x7F]; // 2147483647
        assert_eq!(decode_le_int_sample(&raw, 0, 4).unwrap(), 2_147_483_647);
    }

    #[test]
    fn test_decode_le_int_sample_unsupported_bps() {
        let raw = [0u8; 8];
        assert!(decode_le_int_sample(&raw, 0, 5).is_err());
        assert!(decode_le_int_sample(&raw, 0, 0).is_err());
    }

    #[test]
    fn test_decode_le_int_sample_32bit_distinct_bytes() {
        // 0x12345678 has four distinct little-endian bytes, so a byte-offset
        // arithmetic mutant that re-reads an earlier byte cannot reproduce
        // the value (the all-zeros / all-FF fixtures cannot distinguish).
        assert_eq!(
            decode_le_int_sample(&[0x78, 0x56, 0x34, 0x12], 0, 4).unwrap(),
            0x12345678
        );
    }

    // ── scan_channel_peaks boundary arithmetic ────────────────────────────

    #[test]
    fn test_scan_channel_peaks_int_full_scale_negative_spike() {
        // Stereo 16-bit: ch0 quiet noise plus one full-scale negative sample
        // (|-32768/32768| must register as the channel peak 1.0); ch1
        // constant 12345 → 12345/32768.
        let dir = tempfile::TempDir::new().unwrap();
        let mut samples: Vec<i32> = Vec::new();
        for i in 0..64 {
            let ch0 = if i == 32 {
                -32768
            } else {
                ((i * 37) % 100) - 50
            };
            samples.push(ch0);
            samples.push(12345);
        }
        let path = write_test_wav_int(&dir, "peaks_int_spike.wav", 2, 48_000, 16, &samples);
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let peaks = reader.scan_channel_peaks().unwrap();
        assert!(
            (peaks[0] - 1.0).abs() < 1e-6,
            "full-scale negative spike must peak ch0 at 1.0, got {}",
            peaks[0]
        );
        assert!(
            (peaks[1] - 12345.0 / 32768.0).abs() < 1e-6,
            "ch1 peak must be 12345/32768, got {}",
            peaks[1]
        );
    }

    #[test]
    fn test_scan_channel_peaks_float_stereo_exact_peaks() {
        // Sample values chosen so every mantissa byte differs (0.82 =
        // 0x3F51EB85, −0.19 = 0xBEC28F5C little-endian): byte-index
        // arithmetic mutants that re-read a wrong byte cannot reproduce the
        // exact peaks. ch0: silence except one 0.82 sample; ch1: constant
        // −0.19.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("peaks_float_stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..64 {
            writer
                .write_sample(if i == 32 { 0.82f32 } else { 0.0f32 })
                .unwrap();
            writer.write_sample(-0.19f32).unwrap();
        }
        writer.finalize().unwrap();
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let peaks = reader.scan_channel_peaks().unwrap();
        assert!(
            (peaks[0] - 0.82).abs() < 1e-6,
            "ch0 peak must be 0.82, got {}",
            peaks[0]
        );
        assert!(
            (peaks[1] - 0.19).abs() < 1e-6,
            "ch1 peak must be 0.19, got {}",
            peaks[1]
        );
    }

    #[test]
    fn test_read_mono_samples_f32_stereo_extracts_exact_frames() {
        // Reading channel 1 of a stereo file must yield the exact ch1 frames
        // at their 4-byte stride — both the sample accounting and the values
        // (a nonzero channel also makes stride arithmetic under/mis-flow
        // observable).
        let dir = tempfile::TempDir::new().unwrap();
        let mut samples: Vec<i32> = Vec::new();
        let mut ch1_values = Vec::new();
        for i in 0..10 {
            let v = -5000 + i as i32;
            ch1_values.push(v);
            samples.push(1000 * (i as i32) + 7);
            samples.push(v);
        }
        let path = write_test_wav_int(&dir, "mono_stereo_frames.wav", 2, 48_000, 16, &samples);
        let (mut reader, _start) = WavChunkReader::open_with_channel(&path, 1).unwrap();
        let read = reader.read_mono_samples_f32(0, 10).unwrap();
        assert_eq!(read.len(), 10);
        let max_val = 32768.0f32;
        for (i, v) in ch1_values.iter().enumerate() {
            assert!(
                (read[i] - *v as f32 / max_val).abs() < 1e-6,
                "frame {i}: expected {}, got {}",
                *v as f32 / max_val,
                read[i]
            );
        }
    }

    #[test]
    fn test_read_mono_samples_f32_float_stereo_extracts_exact_frames() {
        // Float-format stereo, channel 1, with values whose four IEEE bytes
        // are pairwise distinct (0.82 = 0x3F51EB85, −0.19 = 0xBEC28F5C):
        // byte-offset arithmetic mutants that re-read an earlier byte cannot
        // reproduce the values.
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("mono_float_stereo.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        let mut ch1_values = Vec::new();
        for i in 0..10 {
            let v = if i % 2 == 0 { 0.82f32 } else { -0.19f32 };
            ch1_values.push(v);
            writer.write_sample(0.25f32).unwrap();
            writer.write_sample(v).unwrap();
        }
        writer.finalize().unwrap();
        let (mut reader, _start) = WavChunkReader::open_with_channel(&path, 1).unwrap();
        let read = reader.read_mono_samples_f32(0, 10).unwrap();
        assert_eq!(read.len(), 10);
        for (i, v) in ch1_values.iter().enumerate() {
            assert!(
                (read[i] - *v).abs() < 1e-6,
                "frame {i}: expected {v}, got {}",
                read[i]
            );
        }
    }

    #[test]
    fn test_read_mono_samples_i16_stereo_extracts_requested_channel() {
        // i16 stereo, channel 1: the stride filter must pick ch1 frames.
        let dir = tempfile::TempDir::new().unwrap();
        let mut samples: Vec<i32> = Vec::new();
        let mut ch1_values = Vec::new();
        for i in 0..10 {
            let v = -5000 + i as i32;
            ch1_values.push(v);
            samples.push(1000 * (i as i32) + 7);
            samples.push(v);
        }
        let path = write_test_wav_int(&dir, "i16_stereo_ch1.wav", 2, 48_000, 16, &samples);
        let (mut reader, _start) = WavChunkReader::open_with_channel(&path, 1).unwrap();
        let read = reader.read_mono_samples_i16(0, 10).unwrap();
        assert_eq!(read.len(), 10);
        for (i, v) in ch1_values.iter().enumerate() {
            assert_eq!(read[i], *v as i16, "frame {i}");
        }
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
            0, 1, -1, 8388607, -8388608, 1234567, -1234567, 48000, -48000,
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

        assert_eq!(
            read.len(),
            test_samples.len(),
            "should read all {} samples",
            test_samples.len()
        );

        let tolerance = 1.0 / max_val;
        for (i, (&expected_int, &actual_f32)) in test_samples.iter().zip(read.iter()).enumerate() {
            let expected_f32 = expected_int as f32 / max_val;
            let abs_diff = (actual_f32 - expected_f32).abs();
            assert!(
                abs_diff <= tolerance,
                "sample[{}]: expected {:.10} (from {}), got {:.10}, diff={:.10}",
                i,
                expected_f32,
                expected_int,
                actual_f32,
                abs_diff
            );
            if expected_int < 0 {
                assert!(
                    actual_f32 < 0.0,
                    "sample[{}]: expected negative for int={}, got {:.10}",
                    i,
                    expected_int,
                    actual_f32
                );
            } else if expected_int > 0 {
                assert!(
                    actual_f32 > 0.0,
                    "sample[{}]: expected positive for int={}, got {:.10}",
                    i,
                    expected_int,
                    actual_f32
                );
            } else {
                assert!(
                    (actual_f32).abs() <= tolerance,
                    "sample[{}]: expected zero for int=0, got {:.10}",
                    i,
                    actual_f32
                );
            }
        }
    }

    // ── WavChunkReader: open errors ───────────────────────────────────

    #[test]
    fn test_wav_chunk_reader_nonexistent_file() {
        let result = WavChunkReader::open(Path::new("/nonexistent/path.wav"));
        assert!(result.is_err());
    }

    // ── WavChunkReader: 16-bit mono reads ─────────────────────────────

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
            assert!(
                diff < 1e-6,
                "sample[{}]: expected {:.10}, got {:.10}, diff={:.10}",
                i,
                expected_f32,
                actual_f32,
                diff
            );
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
        assert_eq!(
            read.len(),
            5,
            "stereo should extract 5 left-channel samples"
        );
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
        assert!(
            result.is_ok(),
            "expected Ok for 24-bit read, got: {:?}",
            result
        );
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
        assert!(
            result.is_ok(),
            "expected Ok for 8-bit read, got: {:?}",
            result
        );
        let read = result.unwrap();
        assert_eq!(read.len(), expected.len());
        for (i, (&e, &a)) in expected.iter().zip(read.iter()).enumerate() {
            assert_eq!(a, e, "sample[{}]: expected {}, got {}", i, e, a);
        }
    }

    #[test]
    fn test_wav_chunk_reader_read_i16_32bit_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![
            0,
            65536,
            -65536,
            2147483647,
            -2147483648,
            16777216,
            -16777216,
        ];
        let expected: Vec<i16> = vec![0, 1, -1, 32767, -32768, 256, -256];
        let path = write_test_wav_int(&dir, "32bit_i16_ok.wav", 1, 48000, 32, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let result = reader.read_mono_samples_i16(0, test_samples.len());
        assert!(
            result.is_ok(),
            "expected Ok for 32-bit read, got: {:?}",
            result
        );
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
        assert!(
            (read[0] + 1.0).abs() < 0.01,
            "first sample (-128) should be -1.0, got {}",
            read[0]
        );
        assert!(
            (read[128] - 0.0).abs() < 1e-4,
            "sample at zero should be 0.0, got {}",
            read[128]
        );
        assert!(
            (read[255] - 127.0 / max_val).abs() < 1e-4,
            "last sample (127) should be ~0.992, got {}",
            read[255]
        );
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
}
