pub mod cli;
pub mod command;
pub mod engine;
pub mod log_buffer;
pub mod state;
pub mod timecode;

// Re-export commonly-used types so GUI crates don't need direct deps
pub use arc_swap::ArcSwap;
pub use audio_core::{AudioDeviceInfo, AudioEvent, Timecode, SAMPLE_RATE_OPTIONS};