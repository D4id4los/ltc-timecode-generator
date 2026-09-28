use std::path::PathBuf;

use serde;

use crate::camera_meta::CameraInfo;
use crate::converter::channel_map::ChannelMap;
use crate::converter::timecode::TimecodeMetadata;
use crate::naming::{self, NameTemplate};
use crate::video_codecs;

use super::capabilities::ResolvedHwDevice;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ConversionPipeline {
    AudioOnly {
        generate_synthetic_video: bool,
    },
    VideoPassthrough,
    MetadataOnly,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum RecordingType {
    MultiTrackAudio,
    VideoClipSequence,
}

#[derive(Clone, Debug)]
pub struct ConverterSettings {
    pub pipeline: ConversionPipeline,
    pub input_files: Vec<PathBuf>,
    pub recording_type: RecordingType,
    pub ltc_track_channel_index: usize,
    pub channel_map: ChannelMap,
    pub split_tracks: bool,
    pub drop_ltc_track: bool,
    pub ltc_video_source: Option<(usize, usize)>,
    pub container: String,
    pub copy_video: bool,
    pub video_encoder: String,
    pub audio_encoder: String,
    pub resolved_video_encoder: String,
    pub resolved_hw_device: Option<ResolvedHwDevice>,
    pub output_folder: PathBuf,
    pub filename_prefix: String,
    pub audio_suffix_template: String,
    pub video_suffix_template: String,
    pub set_start_from_ltc: bool,
    pub embed_camera_metadata: bool,
    pub trim_offsets_secs: Vec<f64>,
    pub timecode_meta_per_file: Vec<Option<TimecodeMetadata>>,
    pub camera_meta_per_file: Vec<Option<CameraInfo>>,
    pub device_name: Option<String>,
    pub concat_audio: bool,
}

impl ConverterSettings {
    pub fn effective_video_encoder(&self) -> String {
        if !self.resolved_video_encoder.is_empty() {
            return self.resolved_video_encoder.clone();
        }
        let input = self.video_encoder.as_str();
        if video_codecs::is_known_encoder(input) {
            return input.to_string();
        }
        let codec = video_codecs::normalize_video_codec(input);
        video_codecs::static_encoder_chain(codec)
            .into_iter()
            .next()
            .unwrap_or_else(|| codec.to_string())
    }

    pub fn output_path_for_index(&self, kind: &str, index: usize, extension: &str) -> PathBuf {
        self.output_path_for_file(kind, 0, index, extension)
    }

    pub fn output_path_for_file_checked(&self, kind: &str, file_idx: usize, index: usize, extension: &str) -> (PathBuf, PathBuf) {
        let naming_ctx = naming::NamingContext {
            filename: self.input_files.get(file_idx)
                .and_then(|p| p.file_stem())
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string(),
            device: self.device_name.clone().unwrap_or_else(|| "unknown".into()),
            clip: file_idx + 1,
            track: index.max(1),
        };

        let prefix_expanded = NameTemplate::parse(&self.filename_prefix)
            .unwrap_or_else(|_| NameTemplate::parse("{filename}").unwrap())
            .expand(&naming_ctx);

        let suffix_template_str = match kind {
            "audio" => &self.audio_suffix_template,
            "video" => &self.video_suffix_template,
            _ => "",
        };
        let suffix_expanded = if suffix_template_str.is_empty() {
            String::new()
        } else {
            NameTemplate::parse(suffix_template_str)
                .unwrap_or_else(|_| NameTemplate::parse("").unwrap())
                .expand(&naming_ctx)
        };

        let filename = format!("{}{}.{}", prefix_expanded, suffix_expanded, extension);
        let unguarded = self.output_folder.join(&filename);
        if self.input_files.iter().any(|input| input == &unguarded) {
            let alt = format!("{}{}_conv.{}", prefix_expanded, suffix_expanded, extension);
            (self.output_folder.join(&alt), unguarded)
        } else {
            (unguarded.clone(), unguarded)
        }
    }

    pub fn output_path_for_file(&self, kind: &str, file_idx: usize, index: usize, extension: &str) -> PathBuf {
        self.output_path_for_file_checked(kind, file_idx, index, extension).0
    }

    pub fn merged_audio_output_path(&self, extension: &str) -> PathBuf {
        self.output_path_for_file("audio", 0, 0, extension)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use crate::converter::test_fixtures::*;
    use crate::converter::DEFAULT_AUDIO_SUFFIX;
    use crate::converter::DEFAULT_VIDEO_SUFFIX;
    use super::*;

    #[test]
    fn test_output_path_for_index_audio_01d() {
        let s = ConverterSettings {
            filename_prefix: "output".to_string(),
            audio_suffix_template: "_audio_track{track:01d}".to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
            ..make_settings_audio_only()
        };
        let path = s.output_path_for_index("audio", 1, "wav");
        assert_eq!(path.to_string_lossy(), "/tmp/output_audio_track1.wav");
    }

    #[test]
    fn test_output_path_for_index_audio_02d() {
        let s = ConverterSettings {
            filename_prefix: "output".to_string(),
            audio_suffix_template: "_audio_track{track:01d}".to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
            ..make_settings_audio_only()
        };
        let path = s.output_path_for_index("audio", 10, "wav");
        assert_eq!(path.to_string_lossy(), "/tmp/output_audio_track10.wav");
    }

    #[test]
    fn test_output_path_for_index_video_02d() {
        let s = ConverterSettings {
            filename_prefix: "output".to_string(),
            video_suffix_template: "_video_clip{clip:02d}".to_string(),
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            ..make_settings_audio_only()
        };
        let path = s.output_path_for_index("video", 2, "mov");
        assert_eq!(path.to_string_lossy(), "/tmp/output_video_clip01.mov");
    }

    #[test]
    fn test_conversion_pipeline_audio_only_default() {
        let p = ConversionPipeline::AudioOnly { generate_synthetic_video: false };
        assert_eq!(p, ConversionPipeline::AudioOnly { generate_synthetic_video: false });
        assert_ne!(p, ConversionPipeline::AudioOnly { generate_synthetic_video: true });
    }

    #[test]
    fn test_recording_type_debug() {
        let rt = RecordingType::MultiTrackAudio;
        assert_eq!(format!("{:?}", rt), "MultiTrackAudio");
        let rt = RecordingType::VideoClipSequence;
        assert_eq!(format!("{:?}", rt), "VideoClipSequence");
    }

    #[test]
    fn test_validate_suffix_template() {
        assert!(crate::naming::validate_template("_audio_track{track:01d}").is_ok());
        assert!(crate::naming::validate_template("_video_{clip}").is_ok());
    }

    #[test]
    fn test_output_path_for_file_prefix_mode_basename() {
        let s = ConverterSettings {
            filename_prefix: "prefix".to_string(),
            audio_suffix_template: "_audio".to_string(),
            video_suffix_template: "_video".to_string(),
            ..make_settings_audio_only()
        };
        let path = s.output_path_for_file("audio", 0, 1, "wav");
        assert!(path.to_string_lossy().contains("prefix"));
    }

    #[test]
    fn test_output_path_for_file_source_stems_basename() {
        let s = ConverterSettings {
            filename_prefix: "{filename}".to_string(),
            audio_suffix_template: "_audio{track:02d}".to_string(),
            video_suffix_template: "_video{clip:02d}".to_string(),
            ..make_settings_audio_only()
        };
        let path = s.output_path_for_file("audio", 0, 1, "wav");
        assert!(path.to_string_lossy().contains("input1"));

        let path_v = s.output_path_for_file("video", 0, 1, "mkv");
        assert!(path_v.to_string_lossy().contains("input1"));
    }

    #[test]
    fn test_output_path_for_file_prefix_mode_unchanged() {
        let s = ConverterSettings {
            filename_prefix: "my_prefix".to_string(),
            audio_suffix_template: "_track{track:01d}".to_string(),
            video_suffix_template: "_clip".to_string(),
            ..make_settings_audio_only()
        };
        let path = s.output_path_for_file("audio", 0, 1, "wav");
        assert!(path.to_string_lossy().contains("my_prefix_track1.wav"));
    }

    #[test]
    fn test_output_path_collision_fallback() {
        let path = std::path::Path::new("/tmp/input1.wav");
        let s = ConverterSettings {
            input_files: vec![path.to_path_buf()],
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "{filename}".to_string(),
            audio_suffix_template: String::new(),
            video_suffix_template: String::new(),
            ..make_settings_audio_only()
        };
        let guarded = s.output_path_for_file("audio", 0, 0, "wav");
        assert_eq!(guarded.file_name().unwrap(), "input1_conv.wav",
            "collision with input file should append _conv");
    }
}