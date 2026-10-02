//! audio-core: raw audio engine for LTC generation, playback, and decoding.
//!
//! This crate is the front door: shared types live in [`types`], byte-level
//! WAV chunk reading in [`wav_chunk_reader`], the chunked LTC decode
//! pipeline in [`chunked_decode`], the audio device/stream lifecycle in
//! [`audio_output`], and the LTC codecs in [`ltc_encoder`], [`ltc_decoder`],
//! and [`ltc_decoder_libltc`].

pub mod audio_output;
pub mod chunked_decode;
pub mod decoder;
pub mod ltc_decoder;
pub mod ltc_decoder_libltc;
pub mod ltc_encoder;
pub mod types;
pub mod wav_chunk_reader;

use std::sync::atomic::AtomicBool;

// ── Types ──────────────────────────────────────────────────────────────────

pub use types::{
    AudioDeviceInfo, AudioEvent, ChannelSel, DecodeConfig, DecodeProgress, Timecode,
};

// ── Chunked LTC decode ────────────────────────────────────────────────────

pub use chunked_decode::{count_chunks, count_chunks_in_wav, decode_ltc_chunked};
pub use wav_chunk_reader::WavChunkReader;

// ── LTC encoder ───────────────────────────────────────────────────────────

pub use ltc_encoder::{generate_ltc_frame_stereo, get_ltc_bits, increment_timecode};

// ── Audio output ──────────────────────────────────────────────────────────

pub use audio_output::{
    is_permanent_device_error, list_audio_devices, suggest_sample_rate, AudioCore,
    SAMPLE_RATE_OPTIONS,
};

// ── LTC decoder re-exports ────────────────────────────────────────────────

pub use ltc_decoder::{
    compute_ltc_quality, decode_ltc_from_wav, find_first_coherent_index,
    CONFIDENCE_LOW_THRESHOLD, CONFIDENCE_SUCCESS_THRESHOLD, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult, LtcQualityReport,
};
pub use ltc_decoder_libltc::decode_ltc_from_wav_libltc;

/// Decode LTC from a WAV file, selecting the decoder implementation.
pub fn decode_ltc_with_decoder(
    path: &std::path::Path,
    use_libltc: bool,
    fps: f64,
    drop_frame: bool,
    cancel: Option<&AtomicBool>,
) -> Result<LtcDetectionResult, String> {
    decoder::decoder_for(use_libltc).decode_wav(path, fps, drop_frame, cancel)
}
