//! Shared audio/LTC types: timecode, channel routing, audio events, device
//! info, and the chunked-decode configuration/progress types.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timecode {
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
    pub frames: u32,
}

/// Stereo channel routing for generated LTC/beep tones.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelSel {
    Left,
    Right,
    Both,
}

impl ChannelSel {
    /// Canonical lowercase name (used by the CLI and UI labels).
    pub fn as_str(self) -> &'static str {
        match self {
            ChannelSel::Left => "left",
            ChannelSel::Right => "right",
            ChannelSel::Both => "both",
        }
    }

    /// Parse a channel name; case-insensitive. `None` for unknown names.
    pub fn parse(s: &str) -> Option<ChannelSel> {
        match s.to_ascii_lowercase().as_str() {
            "left" => Some(ChannelSel::Left),
            "right" => Some(ChannelSel::Right),
            "both" => Some(ChannelSel::Both),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum AudioEvent {
    StreamError(String),
    StreamDied,
    StreamRecovering { attempt: u8 },
    StreamDead,
    RecoveryNeeded { reason: String },
    Underrun,
    FramesDropped { total: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
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

/// Why an LTC decode produced no result. `Cancelled` is a normal,
/// user-initiated outcome — callers must not surface it as an error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LtcDecodeError {
    /// The caller's cancel flag was observed; no result was produced.
    Cancelled,
    /// The decode could not run or failed (I/O, format, extraction, …).
    Failed(String),
    /// The decoder only supports 16-bit integer PCM but the WAV carries a
    /// different bit depth. Only produced for integer-PCM input; a
    /// non-integer sample format is reported as `Failed` instead.
    UnsupportedBitDepth { bits: u16 },
}

impl std::fmt::Display for LtcDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LtcDecodeError::Cancelled => f.write_str("Decode canceled by user"),
            LtcDecodeError::Failed(msg) => f.write_str(msg),
            // Byte-identical to the former Failed(...) prose payload.
            LtcDecodeError::UnsupportedBitDepth { bits } => {
                write!(
                    f,
                    "libltc decoder requires 16-bit integer PCM WAV (got {bits} bit Int)"
                )
            }
        }
    }
}

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
            chunk_size_bytes: 50_000_000, // 50 MB
            overlap_seconds: 2.0,
        }
    }
}

/// Fine-grained decode progress sink: receives the fraction (0.0..=1.0) of
/// the buffer/chunk being decoded. Shared across worker threads, hence the
/// `Send + Sync` bound.
pub type DecodeProgressCb<'a> = &'a (dyn Fn(f32) + Send + Sync);

/// Shared progress state for a chunked decode operation.
#[derive(Clone)]
pub struct DecodeProgress {
    pub chunks_total: usize,
    pub chunks_completed: Arc<AtomicUsize>,
    pub cancel_flag: Arc<AtomicBool>,
    /// Work-weighted progress in completed work units. The chunked runner
    /// sizes the job with [`DecodeProgress::set_work_total`] (the plan's
    /// total sample count); each parallel chunk worker converts its
    /// per-buffer fraction deltas into sample units via
    /// [`DecodeProgress::add_work_units`]. A work **sum** — not a leading
    /// stream position — is the only honest model for parallel chunks: the
    /// chunk that happens to finish first (e.g. a small trailing remainder
    /// chunk, or a silent region that fast-fails) must not pin the bar at
    /// 100 % while slower chunks are still working.
    work_units: Arc<AtomicU64>,
    work_total: Arc<AtomicU64>,
}

impl DecodeProgress {
    pub fn new(chunks_total: usize) -> Self {
        Self {
            chunks_total,
            chunks_completed: Arc::new(AtomicUsize::new(0)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            work_units: Arc::new(AtomicU64::new(0)),
            work_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Like [`DecodeProgress::new`], but sharing externally owned chunk
    /// bookkeeping atomics (callers that count completions themselves).
    pub fn with_atomics(
        chunks_total: usize,
        chunks_completed: Arc<AtomicUsize>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Self {
        Self {
            chunks_total,
            chunks_completed,
            cancel_flag,
            work_units: Arc::new(AtomicU64::new(0)),
            work_total: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Size the job: `units` is the total amount of work the fraction is
    /// measured against (the chunk plan's total sample count, or 1000 for
    /// a single-buffer decode). Must be set before the first fraction is
    /// consumed; a total of 0 keeps pure chunk-count granularity.
    pub fn set_work_total(&self, units: u64) {
        self.work_total.store(units, Ordering::Relaxed);
    }

    /// Record completed work units. Callers add only positive deltas of
    /// their own progress, so the sum is monotonic by construction even
    /// with out-of-order parallel chunk completion.
    pub fn add_work_units(&self, units: u64) {
        self.work_units.fetch_add(units, Ordering::Relaxed);
    }

    /// Completed work units so far (observability accessor).
    pub fn work_units_snapshot(&self) -> u64 {
        self.work_units.load(Ordering::Relaxed)
    }

    /// Decode fraction (`0.0..=1.0`): the work-weighted sum when the job
    /// was sized, otherwise the chunk-completion fraction.
    pub fn percent(&self) -> f32 {
        let total = self.work_total.load(Ordering::Relaxed);
        if total > 0 {
            let units = self.work_units.load(Ordering::Relaxed);
            return (units as f32 / total as f32).min(1.0);
        }
        if self.chunks_total == 0 {
            return 1.0;
        }
        self.chunks_completed.load(Ordering::Relaxed) as f32 / self.chunks_total as f32
    }

    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LtcDecodeError Display ────────────────────────────────────────────

    // Display output is the contract here (R3/R4 of the error-handling
    // policy): the UnsupportedBitDepth prose is byte-identical to the
    // former Failed(...) payload that consumers matched on.
    #[test]
    fn test_ltc_decode_error_display_renders_each_variant() {
        assert_eq!(
            LtcDecodeError::Cancelled.to_string(),
            "Decode canceled by user"
        );
        assert_eq!(
            LtcDecodeError::Failed("disk on fire".to_string()).to_string(),
            "disk on fire"
        );
        assert_eq!(
            LtcDecodeError::UnsupportedBitDepth { bits: 24 }.to_string(),
            "libltc decoder requires 16-bit integer PCM WAV (got 24 bit Int)"
        );
    }

    // ── Timecode ──────────────────────────────────────────────────────────

    #[test]
    fn test_timecode_serialize_deserialize() {
        let tc = Timecode {
            hours: 10,
            minutes: 20,
            seconds: 30,
            frames: 15,
        };
        let json = serde_json::to_string(&tc).unwrap();
        let deserialized: Timecode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, tc);
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

    // ── ChannelSel ────────────────────────────────────────────────────────

    #[test]
    fn test_channel_sel_parse_valid() {
        assert_eq!(ChannelSel::parse("left"), Some(ChannelSel::Left));
        assert_eq!(ChannelSel::parse("right"), Some(ChannelSel::Right));
        assert_eq!(ChannelSel::parse("both"), Some(ChannelSel::Both));
        assert_eq!(ChannelSel::parse("LEFT"), Some(ChannelSel::Left));
        assert_eq!(ChannelSel::parse("Both"), Some(ChannelSel::Both));
    }

    #[test]
    fn test_channel_sel_parse_invalid() {
        assert_eq!(ChannelSel::parse(""), None);
        assert_eq!(ChannelSel::parse("centre"), None);
        assert_eq!(ChannelSel::parse("lefft"), None);
    }

    #[test]
    fn test_channel_sel_as_str_roundtrip() {
        for sel in [ChannelSel::Left, ChannelSel::Right, ChannelSel::Both] {
            assert_eq!(ChannelSel::parse(sel.as_str()), Some(sel));
        }
    }
}
