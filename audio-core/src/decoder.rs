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

    /// Decode the chunk `[start, start+len)` (mono-sample units) via
    /// `req.reader`. Each backend reads its own preferred sample format —
    /// f32 for builtin, i16 for libltc (whose "requires 16-bit int PCM"
    /// failure surfaces as the existing `Failed to read chunk N` error).
    /// `req.chunk_idx` only formats into that prose message; cancellation is
    /// carried by `LtcDecodeError::Cancelled`, not by this wrapper.
    fn decode_chunk(&self, req: ChunkDecodeReq<'_>) -> Result<LtcDetectionResult, LtcDecodeError>;
}

/// One chunk decode request: the WAV window plus the decode settings shared
/// by every chunk of the pass.
pub struct ChunkDecodeReq<'a> {
    pub path: &'a Path,
    pub reader: &'a mut WavChunkReader,
    pub chunk_idx: usize,
    pub start: usize,
    pub len: usize,
    pub sample_rate: u32,
    pub fps: f64,
    pub drop_frame: bool,
    pub start_time: Instant,
    pub cancel: &'a AtomicBool,
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

    fn decode_chunk(&self, req: ChunkDecodeReq<'_>) -> Result<LtcDetectionResult, LtcDecodeError> {
        let ChunkDecodeReq {
            reader,
            chunk_idx,
            start,
            len,
            sample_rate,
            fps,
            drop_frame,
            start_time,
            cancel,
            ..
        } = req;
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

    fn decode_chunk(&self, req: ChunkDecodeReq<'_>) -> Result<LtcDetectionResult, LtcDecodeError> {
        let ChunkDecodeReq {
            reader,
            chunk_idx,
            start,
            len,
            sample_rate,
            fps,
            drop_frame,
            start_time,
            cancel,
            ..
        } = req;
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
