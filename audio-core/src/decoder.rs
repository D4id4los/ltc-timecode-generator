//! Backend-agnostic LTC decode interface. Selection happens once, in
//! [`decoder_for`], instead of at every call site.

use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use crate::LtcDecodeError;
use crate::ltc_decoder::{decode_ltc_from_wav, decode_ltc_samples, LtcDetectionResult};
use crate::ltc_decoder_libltc::{decode_ltc_from_wav_libltc, decode_ltc_samples_libltc};
use crate::wav_chunk_reader::WavChunkReader;

/// Backend-agnostic LTC decode interface.
pub trait LtcDecoder: Send + Sync {
    /// Short name for logs ("builtin" / "libltc").
    fn name(&self) -> &'static str;

    /// Decode a whole WAV file.
    fn decode_wav(
        &self,
        path: &Path,
        fps: f64,
        drop_frame: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<LtcDetectionResult, LtcDecodeError>;

    /// Decode the chunk `[start, start+len)` (mono-sample units) of `path`
    /// via `reader`. Each backend reads its own preferred sample format —
    /// f32 for builtin, i16 for libltc (whose "requires 16-bit int PCM"
    /// failure surfaces as the existing `Failed to read chunk N` error).
    /// `chunk_idx` only formats into that prose message; cancellation is
    /// carried by `LtcDecodeError::Cancelled`, not by this wrapper.
    // Cohesive decode request: path/reader/window/backend decode one chunk.
    #[allow(clippy::too_many_arguments)]
    fn decode_chunk(
        &self,
        path: &Path,
        reader: &mut WavChunkReader,
        chunk_idx: usize,
        start: usize,
        len: usize,
        sample_rate: u32,
        fps: f64,
        drop_frame: bool,
        start_time: Instant,
        cancel: &AtomicBool,
    ) -> Result<LtcDetectionResult, LtcDecodeError>;
}

/// Pure-Rust builtin decoder (f32 samples, chunked parallel decode support).
pub struct BuiltinDecoder;

/// Decoder backed by the system `libltc` C library (i16 samples).
pub struct LibltcDecoder;

impl LtcDecoder for BuiltinDecoder {
    fn name(&self) -> &'static str {
        "builtin"
    }

    fn decode_wav(
        &self,
        path: &Path,
        fps: f64,
        drop_frame: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<LtcDetectionResult, LtcDecodeError> {
        decode_ltc_from_wav(path, fps, drop_frame, cancel)
    }

    fn decode_chunk(
        &self,
        _path: &Path,
        reader: &mut WavChunkReader,
        chunk_idx: usize,
        start: usize,
        len: usize,
        sample_rate: u32,
        fps: f64,
        drop_frame: bool,
        start_time: Instant,
        cancel: &AtomicBool,
    ) -> Result<LtcDetectionResult, LtcDecodeError> {
        match reader.read_mono_samples_f32(start, len) {
            Ok(samples) => decode_ltc_samples(
                &samples, sample_rate, 1, fps, drop_frame, start_time, Some(cancel),
            ),
            Err(e) => Err(LtcDecodeError::Failed(format!("Failed to read chunk {}: {}", chunk_idx, e))),
        }
    }
}

impl LtcDecoder for LibltcDecoder {
    fn name(&self) -> &'static str {
        "libltc"
    }

    fn decode_wav(
        &self,
        path: &Path,
        fps: f64,
        drop_frame: bool,
        cancel: Option<&AtomicBool>,
    ) -> Result<LtcDetectionResult, LtcDecodeError> {
        decode_ltc_from_wav_libltc(path, fps, drop_frame, cancel)
    }

    fn decode_chunk(
        &self,
        _path: &Path,
        reader: &mut WavChunkReader,
        chunk_idx: usize,
        start: usize,
        len: usize,
        sample_rate: u32,
        fps: f64,
        drop_frame: bool,
        start_time: Instant,
        cancel: &AtomicBool,
    ) -> Result<LtcDetectionResult, LtcDecodeError> {
        match reader.read_mono_samples_i16(start, len) {
            Ok(samples) => decode_ltc_samples_libltc(
                &samples, 1, sample_rate, fps, drop_frame, start_time, Some(cancel),
            ),
            Err(e) => Err(LtcDecodeError::Failed(format!("Failed to read chunk {}: {}", chunk_idx, e))),
        }
    }
}

static BUILTIN: BuiltinDecoder = BuiltinDecoder;
static LIBLTC: LibltcDecoder = LibltcDecoder;

/// The single `if use_libltc` left in the crate.
pub fn decoder_for(use_libltc: bool) -> &'static dyn LtcDecoder {
    if use_libltc { &LIBLTC } else { &BUILTIN }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decoder_for_names() {
        assert_eq!(decoder_for(false).name(), "builtin");
        assert_eq!(decoder_for(true).name(), "libltc");
    }
}
