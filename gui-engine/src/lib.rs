pub mod bext_meta;
pub mod camera_meta;
pub mod cli;
pub mod clip_probe;
pub mod command;
pub mod config;
pub mod converter;
pub mod decode;
pub mod device_name;
pub mod duration;
pub mod edit_state;
pub mod engine;
pub mod ffprobe;
pub mod file_pattern;
pub mod hw_cache;
pub mod hw_device;
pub mod job;
pub mod log_buffer;
pub mod media_ext;
pub mod naming;
pub mod offload;
pub mod state;
pub mod subprocess;
pub mod tagger;
pub mod theme;
pub mod timecode;
pub mod video_codecs;

// Re-export commonly-used types so GUI crates don't need direct deps
pub use arc_swap::ArcSwap;
// Re-export camera_meta types
pub use camera_meta::{probe_camera_info, CameraInfo, CameraMetaSource};

pub use audio_core::{
    compute_ltc_quality, dbfs_to_ui_volume, decode_ltc_chunked, decode_ltc_from_wav,
    decode_ltc_from_wav_libltc, decode_ltc_with_decoder, ui_volume_to_dbfs, AudioDeviceInfo,
    AudioEvent, ChannelSel, DecodeConfig, DecodeProgress, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult, LtcQualityReport, Timecode, WavChunkReader, SAMPLE_RATE_OPTIONS,
};

// Re-export converter/file_pattern types for convenience
pub use converter::{
    apply_available_defaults, available_audio_encoders_for_container, available_containers,
    build_per_file_start_timecodes, build_per_file_trim_and_timecode, conversion_sanity_check,
    conversion_sanity_check_metadata_only, evaluate_readiness, find_timecode_at_offset,
    format_blockers, format_ffmpeg_timecode, output_collision_warning, plan_concat_outputs,
    plan_video_outputs, preview_output_files, query_ffmpeg_capabilities, run_conversion,
    select_best_combination, spawn_conversion_job, start_timecode_from_ltc,
    supported_audio_encoders, supported_containers, AudioKeep, ChannelMap, ConversionCheckError,
    ConversionPipeline, ConversionReport, ConvertBlocker, ConvertReadiness, ConverterSettings,
    FailureKind, FfmpegCapabilities, HwDeviceCapabilities, JobConversionReport, OutputKind,
    PreviewOutput, RecordingType, StepFailure, StepFailureRecord, StepOutcome, TimecodeMetadata,
    VideoOutputStep, DEFAULT_AUDIO_SUFFIX, DEFAULT_VIDEO_SUFFIX,
};
pub use file_pattern::{
    default_output_filename, group_display_key, group_key_prefix, match_files_all_patterns,
    match_files_to_groups, wrap_user_selected_files, FileNamingPattern, BUILTIN_PATTERNS,
    CAMERA_PATTERNS,
};
pub use video_codecs::{
    available_video_codecs, codec_supports_container, describe_chain, find_candidate,
    hw_frames_for, normalize_video_codec, resolve_encoder_chain, supported_video_codecs,
    EncoderClass, HwFramePath,
};

// Re-export ffprobe types
pub use ffprobe::{
    extract_audio_channel, extract_audio_channel_with_progress, parse_out_time_us, path_is_video,
    probe_stream_duration_secs, probe_video_audio, AudioStreamInfo, ExtractError, ProbeError,
    VideoAudioProbe,
};

// Re-export media-extension registry
pub use media_ext::{container_for_input, is_audio, is_video, AUDIO_EXTENSIONS, VIDEO_EXTENSIONS};

// Re-export duration helpers
pub use duration::{file_duration_secs, format_duration_secs, group_duration_secs};
pub use hw_cache::{
    cache_key_for, publish_stage1_caps, validate_hw_encoders_cached_with, CacheKey,
    HwValidationCache,
};

// Re-export job types
pub use job::{
    spawn_job, CancelToken, JobContext, JobError, JobEvent, JobFinal, JobId, JobItem, JobKind,
    JobOutcome, JobPhase, JobSpec, JobStatus, JobSupervisor, ProbeStatusLabel, ProgressSnapshot,
    ProgressTracker, SpeedMeter, UnitProgress, UnitSnapshot, UnitSpec, UnitState,
};

// Re-export offload types
pub use offload::{
    apply_selection, default_parent_name, default_selection, detect_cards_with_progress,
    is_media_file, plan_copies_for_card, plan_copies_for_files, plan_copies_for_files_with_sizes,
    run_offload_copy_job, run_offload_scan_job, verify_copy, DeviceNameSource, OffloadDeviceTotals,
    OffloadFileInfo, OffloadSnapshot, ScanProgress, SdCardInfo, VerifyMode,
};
