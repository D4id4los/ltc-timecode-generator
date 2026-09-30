pub mod camera_meta;
pub mod cli;
pub mod command;
pub mod config;
pub mod converter;
pub mod device_name;
pub mod duration;
pub mod engine;
pub mod ffprobe;
pub mod file_pattern;
pub mod job;
pub mod naming;
pub mod hw_device;
pub mod log_buffer;
pub mod offload;
pub mod state;
pub mod subprocess;
pub mod tagger;
pub mod theme;
pub mod video_codecs;
pub mod timecode;

// Re-export commonly-used types so GUI crates don't need direct deps
pub use arc_swap::ArcSwap;
// Re-export camera_meta types
pub use camera_meta::{probe_camera_info, CameraInfo, CameraMetaSource};

pub use audio_core::{
    compute_ltc_quality, decode_ltc_chunked, decode_ltc_from_wav, decode_ltc_from_wav_libltc,
    decode_ltc_samples, decode_ltc_samples_libltc, decode_ltc_with_decoder, quick_check_ltc,
    AudioDeviceInfo, AudioEvent, DecodeConfig, DecodeProgress, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult, LtcQualityReport, Timecode, WavChunkReader, SAMPLE_RATE_OPTIONS,
};

// Re-export converter/file_pattern types for convenience
pub use converter::{
    apply_available_defaults, available_audio_encoders_for_container,
    available_containers, build_per_file_start_timecodes,
    build_per_file_trim_and_timecode, conversion_sanity_check,
    conversion_sanity_check_metadata_only,
    evaluate_readiness,
    find_timecode_at_offset, format_blockers, format_ffmpeg_timecode,
    plan_concat_outputs, plan_video_outputs, query_ffmpeg_capabilities,
    select_best_combination,
    spawn_conversion, spawn_conversion_job, start_timecode_from_ltc,
    supported_audio_encoders, supported_containers,
    AudioKeep, ChannelMap, ConvertBlocker, ConvertReadiness, ConversionPipeline,
    ConversionState, ConversionStatus, ConverterSettings, output_collision_warning,
    OutputKind, PreviewOutput, preview_output_files, StepFailure,
    VideoOutputStep, DEFAULT_AUDIO_SUFFIX, DEFAULT_VIDEO_SUFFIX,
    FfmpegCapabilities, HwDeviceCapabilities, RecordingType, SharedConversionState, CancelFlag,
    TimecodeMetadata,
};
pub use file_pattern::{default_output_filename, group_display_key, group_key_prefix, match_files_to_groups, match_files_all_patterns, wrap_user_selected_files, FileNamingPattern, BUILTIN_PATTERNS, CAMERA_PATTERNS};
pub use video_codecs::{
    available_video_codecs, codec_supports_container, describe_chain,
    find_candidate, hw_frames_for, normalize_video_codec,
    resolve_encoder_chain, supported_video_codecs, EncoderClass, HwFramePath,
};

// Re-export ffprobe types
pub use ffprobe::{extract_audio_channel, extract_audio_channel_with_progress, parse_out_time_us, path_is_video, probe_stream_duration_secs, probe_video_audio, AudioStreamInfo, VideoAudioProbe};

// Re-export duration helpers
pub use duration::{file_duration_secs, format_duration_secs, group_duration_secs};

// Re-export job types
pub use job::{
    CancelToken, JobContext, JobError, JobEvent, JobFinal, JobId, JobItem, JobKind, JobOutcome,
    JobPhase, JobSpec, JobSupervisor, ProgressSnapshot, ProgressTracker, SpeedMeter, UnitProgress,
    UnitSnapshot, UnitSpec, UnitState, spawn_job,
};

// Re-export offload types
pub use offload::{
    apply_selection, default_parent_name, default_selection, detect_cards,
    detect_cards_with_progress, is_media_file, latest_recording_date, plan_copies_for_card,
    plan_copies_for_files, plan_copies_for_files_with_sizes, run_offload,
    run_offload_copy_job, run_offload_scan_job, verify_copy,
    DeviceNameSource, OffloadContext, OffloadDeviceState, OffloadDeviceStatus, OffloadFileInfo,
    OffloadSnapshot, ScanProgress, SdCardInfo, VerifyMode,
};