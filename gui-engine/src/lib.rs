pub mod cli;
pub mod command;
pub mod config;
pub mod converter;
pub mod engine;
pub mod ffprobe;
pub mod file_pattern;
pub mod hw_device;
pub mod log_buffer;
pub mod state;
pub mod theme;
pub mod video_codecs;
pub mod timecode;

// Re-export commonly-used types so GUI crates don't need direct deps
pub use arc_swap::ArcSwap;
pub use audio_core::{
    compute_ltc_quality, decode_ltc_chunked, decode_ltc_from_wav, decode_ltc_from_wav_libltc,
    decode_ltc_samples, decode_ltc_samples_libltc, decode_ltc_with_decoder, quick_check_ltc,
    AudioDeviceInfo, AudioEvent, DecodeConfig, DecodeProgress, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult, LtcQualityReport, Timecode, WavChunkReader, SAMPLE_RATE_OPTIONS,
};

// Re-export converter/file_pattern types for convenience
pub use converter::{
    apply_available_defaults, available_audio_encoders_for_container,
    available_containers, conversion_sanity_check,
    conversion_sanity_check_with_naming, evaluate_readiness,
    find_timecode_at_offset, format_blockers, format_ffmpeg_timecode,
    plan_concat_outputs, plan_video_outputs, query_ffmpeg_capabilities,
    select_best_combination,
    spawn_conversion, supported_audio_encoders, supported_containers,
    AudioKeep, ChannelMap, ConvertBlocker, ConvertReadiness, ConversionPipeline,
    ConversionState, ConversionStatus, ConverterSettings, OutputNamingMode, StepFailure,
    VideoOutputStep, DEFAULT_AUDIO_SUFFIX, DEFAULT_VIDEO_SUFFIX,
    FfmpegCapabilities, HwDeviceCapabilities, RecordingType, SharedConversionState, CancelFlag,
    TimecodeMetadata,
};
pub use file_pattern::{default_output_filename, match_files_to_groups, match_files_all_patterns, wrap_user_selected_files, FileNamingPattern, BUILTIN_PATTERNS, CAMERA_PATTERNS};
pub use video_codecs::{
    available_video_codecs, codec_supports_container, describe_chain,
    find_candidate, hw_frames_for, normalize_video_codec,
    resolve_encoder_chain, supported_video_codecs, EncoderClass, HwFramePath,
};

// Re-export ffprobe types
pub use ffprobe::{extract_audio_channel, path_is_video, probe_video_audio, AudioStreamInfo, VideoAudioProbe};