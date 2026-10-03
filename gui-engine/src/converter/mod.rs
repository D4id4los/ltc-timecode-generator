pub use crate::naming::DEFAULT_AUDIO_SUFFIX;
pub use crate::naming::DEFAULT_VIDEO_SUFFIX;
pub use crate::naming::DEFAULT_PREFIX;

pub(crate) mod channel_map;
pub use channel_map::ChannelMap;

pub(crate) mod timecode;
pub use timecode::{
    build_per_file_start_timecodes, build_per_file_trim_and_timecode,
    find_timecode_at_offset, format_ffmpeg_timecode, read_wav_sample_rate,
    read_wav_sample_rate_from_file, start_timecode_from_ltc, time_reference_samples,
    TimecodeMetadata,
};

pub(crate) mod capabilities;
pub use capabilities::{query_ffmpeg_capabilities, FfmpegCapabilities, HwDeviceCapabilities};

pub(crate) mod formats;
pub use formats::{
    apply_available_defaults, available_audio_encoders_for_container,
    available_containers, select_best_combination, supported_audio_encoders,
    supported_containers,
};

pub(crate) mod settings;
pub use settings::{ConversionPipeline, ConverterSettings, RecordingType};

pub(crate) mod planning;
pub use planning::{
    duplicate_output_names, duplicate_output_warning, output_collision_warning,
    preview_output_files, plan_concat_outputs, plan_video_outputs, AudioKeep,
    OutputKind, PreviewOutput, VideoOutputStep,
};

pub(crate) mod checks;
pub use checks::{
    conversion_sanity_check, conversion_sanity_check_metadata_only,
    conversion_sanity_check_pure, conversion_sanity_check_metadata_only_pure,
    validate_conversion_paths,
    ConversionCheckError, ConvertBlocker, ConvertReadiness, evaluate_readiness,
    format_blockers,
};

pub(crate) mod args;

pub(crate) mod process;
pub use process::StepFailure;

pub(crate) mod runner;
pub use runner::{spawn_conversion_job, run_conversion, ConversionReport, FailureKind, JobConversionReport, StepFailureRecord, StepOutcome, TestReport};

#[cfg(test)]
pub(crate) mod test_fixtures;
