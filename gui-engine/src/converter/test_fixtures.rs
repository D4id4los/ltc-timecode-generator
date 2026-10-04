use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::converter::{
    ChannelMap, ConversionPipeline, ConverterSettings, HwDeviceCapabilities,
    FfmpegCapabilities, RecordingType, DEFAULT_AUDIO_SUFFIX, DEFAULT_VIDEO_SUFFIX,
};
use crate::ffprobe::{AudioStreamInfo, VideoAudioProbe};

pub fn make_settings_audio_only() -> ConverterSettings {
    ConverterSettings {
        pipeline: ConversionPipeline::AudioOnly { generate_synthetic_video: false },
        input_files: vec![
            PathBuf::from("/tmp/input1.wav"),
            PathBuf::from("/tmp/input2.wav"),
        ],
        recording_type: RecordingType::MultiTrackAudio,
        ltc_track_channel_index: 1,
        channel_map: ChannelMap::identity(2),
        split_tracks: false,
        drop_ltc_track: false,
        ltc_video_source: None,
        container: "mkv".to_string(),
        copy_video: false,
        video_encoder: "h264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: std::env::temp_dir(),
        filename_prefix: "output".to_string(),
        audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
        video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        set_start_from_ltc: false,
        embed_camera_metadata: true,
        trim_offsets_secs: vec![0.0; 2],
        timecode_meta_per_file: vec![None; 2],
        camera_meta_per_file: vec![None; 2],
        device_name: None,
        concat_audio: false,
        resolved_hw_device: None,
    }
}

pub fn make_settings_synthetic_video(trim: f64) -> ConverterSettings {
    let mut s = make_settings_audio_only();
    s.pipeline = ConversionPipeline::AudioOnly { generate_synthetic_video: true };
    s.trim_offsets_secs = vec![trim; 2];
    s
}

pub fn make_caps(
    has_ffmpeg: bool,
    encoders: BTreeSet<&str>,
    formats: BTreeSet<&str>,
) -> FfmpegCapabilities {
    FfmpegCapabilities {
        has_ffmpeg,
        available_encoders: encoders.into_iter().map(String::from).collect(),
        available_formats: formats.into_iter().map(String::from).collect(),
        error_message: None,
            ffmpeg_version: None,
        hw: HwDeviceCapabilities::default(),
    }
}

pub fn make_probe(stream_index: usize, channels: usize, sample_rate: u32) -> VideoAudioProbe {
    VideoAudioProbe {
        streams: vec![AudioStreamInfo {
            stream_index,
            channels,
            codec_name: "pcm_s24le".to_string(),
            sample_rate,
        }],
        total_audio_channels: channels,
        is_video_file: true,
    }
}

pub fn make_video_settings() -> ConverterSettings {
    ConverterSettings {
        pipeline: ConversionPipeline::VideoPassthrough,
        input_files: vec![PathBuf::from("/tmp/test.mp4")],
        recording_type: RecordingType::VideoClipSequence,
        ltc_track_channel_index: 0,
        channel_map: ChannelMap::identity(1),
        split_tracks: false,
        drop_ltc_track: false,
        ltc_video_source: None,
        container: "mkv".to_string(),
        copy_video: false,
        video_encoder: "h264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: std::env::temp_dir(),
        filename_prefix: "output".to_string(),
        audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
        video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        set_start_from_ltc: false,
        embed_camera_metadata: true,
        trim_offsets_secs: vec![0.0],
        timecode_meta_per_file: vec![None],
        camera_meta_per_file: vec![None],
        device_name: None,
        concat_audio: false,
        resolved_hw_device: None,
    }
}

pub fn make_stereo_probe() -> VideoAudioProbe {
    VideoAudioProbe {
        streams: vec![
            AudioStreamInfo {
                stream_index: 1,
                channels: 2,
                codec_name: "aac".to_string(),
                sample_rate: 48000,
            },
        ],
        total_audio_channels: 2,
        is_video_file: true,
    }
}

pub fn make_mono_probe() -> VideoAudioProbe {
    VideoAudioProbe {
        streams: vec![
            AudioStreamInfo {
                stream_index: 1,
                channels: 1,
                codec_name: "aac".to_string(),
                sample_rate: 48000,
            },
        ],
        total_audio_channels: 1,
        is_video_file: true,
    }
}

pub fn make_copy_settings() -> ConverterSettings {
    let mut s = make_video_settings();
    s.copy_video = true;
    s
}
/// Create a tiny test video with a sine tone using the cheapest encoders
/// (mpeg4 video + aac audio). Requires a real `ffmpeg` binary; callers in
/// ffmpeg-optional suites must apply the loud-skip rule before using it.
pub fn create_test_video_with_tone(path: &std::path::Path, duration_secs: f64) {
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y", "-v", "error",
            "-f", "lavfi", "-i", &format!("color=c=blue:s=320x240:r=25:duration={}", duration_secs),
            "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={}:sample_rate=48000", duration_secs),
            "-map", "0:v", "-map", "1:a",
            "-c:v", "mpeg4", "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-shortest",
            "-t", &format!("{}", duration_secs),
            &path.to_string_lossy(),
        ])
        .status()
        .expect("failed to spawn ffmpeg for test video");
    assert!(status.success(), "ffmpeg fixture creation failed for {}", path.display());
}
