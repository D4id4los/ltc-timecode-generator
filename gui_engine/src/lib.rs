pub mod cli;
pub mod command;
pub mod converter;
pub mod engine;
pub mod file_pattern;
pub mod log_buffer;
pub mod state;
pub mod theme;
pub mod timecode;

// Re-export commonly-used types so GUI crates don't need direct deps
pub use arc_swap::ArcSwap;
pub use audio_core::{AudioDeviceInfo, AudioEvent, Timecode, SAMPLE_RATE_OPTIONS};

// Re-export converter/file_pattern types for convenience
pub use converter::{
    conversion_sanity_check, query_ffmpeg_capabilities, spawn_conversion,
    supported_audio_encoders, supported_containers, supported_video_encoders,
    ChannelMap, ConversionState, ConversionStatus, ConverterSettings,
    FfmpegCapabilities, SharedConversionState, CancelFlag,
};
pub use file_pattern::{default_output_filename, match_files_to_groups, FileNamingPattern, BUILTIN_PATTERNS};