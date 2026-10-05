use std::fmt;
use std::path::{Path, PathBuf};

use crate::converter::capabilities::FfmpegCapabilities;
use crate::converter::formats::{container_supports_audio_encoder, encoder_available_in_ffmpeg, format_available_in_ffmpeg};
use crate::naming;
use crate::video_codecs;

/// Typed sanity-check failure. One variant per distinct message site;
/// [`Display`](fmt::Display) renders the exact strings that were previously
/// returned as `Err(String)` (they surface in blockers and log lines shown
/// to users, so the rendering is a byte-identical contract).
#[derive(Debug, Clone, PartialEq)]
pub enum ConversionCheckError {
    /// ffmpeg binary missing. `None` when the capability probe produced no
    /// detail; `Some(detail)` carries the probe error message.
    FfmpegUnavailable(Option<String>),
    NoInputFiles,
    NoOutputName,
    UnsupportedContainer { container: String },
    InvalidAudioSuffixTemplate(String),
    InvalidVideoSuffixTemplate(String),
    InvalidPrefixTemplate(String),
    UnknownVideoCodec { codec: String, supported: String },
    NoEncoderAvailable { codec: String, candidates: String },
    UnsupportedAudioEncoder { encoder: String },
    IncompatibleVideoContainer { codec: String, container: String, hint: String },
    IncompatibleAudioContainer { encoder: String, container: String, hint: String },
    MissingInput(PathBuf),
    MissingOutputFolder(PathBuf),
}

impl fmt::Display for ConversionCheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConversionCheckError::FfmpegUnavailable(None) => {
                write!(f, "ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH.")
            }
            ConversionCheckError::FfmpegUnavailable(Some(detail)) => {
                write!(
                    f,
                    "ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH. ({})",
                    detail
                )
            }
            ConversionCheckError::NoInputFiles => {
                write!(f, "No input files selected.")
            }
            ConversionCheckError::NoOutputName => {
                write!(f, "No output filename prefix or suffix specified.")
            }
            ConversionCheckError::UnsupportedContainer { container } => write!(
                f,
                "Container format '{}' is not supported by your ffmpeg installation. \
                 Run `ffmpeg -formats` to see available formats.",
                container
            ),
            ConversionCheckError::InvalidAudioSuffixTemplate(e) => {
                write!(f, "Invalid audio suffix template: {}", e)
            }
            ConversionCheckError::InvalidVideoSuffixTemplate(e) => {
                write!(f, "Invalid video suffix template: {}", e)
            }
            ConversionCheckError::InvalidPrefixTemplate(e) => {
                write!(f, "Invalid filename prefix: {}", e)
            }
            ConversionCheckError::UnknownVideoCodec { codec, supported } => write!(
                f,
                "Unknown video codec '{}'. Supported codecs: {}.",
                codec, supported
            ),
            ConversionCheckError::NoEncoderAvailable { codec, candidates } => write!(
                f,
                "No {} encoder is available in your ffmpeg installation \
                 (needs one of: {}). Run `ffmpeg -encoders` to see available encoders.",
                codec, candidates
            ),
            ConversionCheckError::UnsupportedAudioEncoder { encoder } => write!(
                f,
                "Audio encoder '{}' is not supported by your ffmpeg installation. \
                 Run `ffmpeg -encoders` to see available encoders. \
                 Common alternatives: pcm_s24le (PCM 24-bit), pcm_s16le (PCM 16-bit), aac, libopus.",
                encoder
            ),
            ConversionCheckError::IncompatibleVideoContainer { codec, container, hint } => {
                write!(
                    f,
                    "Video codec '{}' is not compatible with container format '{}'. \
                     {}",
                    codec, container, hint
                )
            }
            ConversionCheckError::IncompatibleAudioContainer { encoder, container, hint } => {
                write!(
                    f,
                    "Audio encoder '{}' is not compatible with container format '{}'. \
                     {}",
                    encoder, container, hint
                )
            }
            ConversionCheckError::MissingInput(p) => {
                write!(f, "Input file does not exist: {}", p.display())
            }
            ConversionCheckError::MissingOutputFolder(p) => {
                write!(f, "Output directory does not exist: {}", p.display())
            }
        }
    }
}

impl std::error::Error for ConversionCheckError {}

/// Borrowed view of the converter settings fields the sanity checks
/// validate. Both `conversion_sanity_check*` entry points take one of these
/// instead of ten loose parameters; build it from the settings snapshot.
pub struct SanityCheckInput<'a> {
    pub container: &'a str,
    pub video_codec: &'a str,
    pub audio_encoder: &'a str,
    pub input_files: &'a [PathBuf],
    pub output_folder: &'a Path,
    pub filename_prefix: &'a str,
    pub caps: &'a FfmpegCapabilities,
    pub audio_suffix: Option<&'a str>,
    pub video_suffix: Option<&'a str>,
    pub copy_video: bool,
}

/// Pure validation: checks templates, caps, codec/container compatibility.
/// Does **not** access the filesystem (no `exists()` on files or folders).
/// Use for per-frame UI display; use `conversion_sanity_check` (which
/// additionally calls `validate_conversion_paths`) before spawning work.
pub fn conversion_sanity_check_pure(
    input: SanityCheckInput<'_>,
) -> Result<(), ConversionCheckError> {
    let SanityCheckInput {
        container,
        video_codec,
        audio_encoder,
        input_files,
        output_folder: _output_folder,
        filename_prefix,
        caps,
        audio_suffix,
        video_suffix,
        copy_video,
    } = input;
    if !caps.has_ffmpeg {
        return Err(ConversionCheckError::FfmpegUnavailable(None));
    }

    if input_files.is_empty() {
        return Err(ConversionCheckError::NoInputFiles);
    }

    if filename_prefix.is_empty() && audio_suffix.unwrap_or("").is_empty() && video_suffix.unwrap_or("").is_empty() {
        return Err(ConversionCheckError::NoOutputName);
    }

    if !format_available_in_ffmpeg(container, caps) {
        return Err(ConversionCheckError::UnsupportedContainer {
            container: container.to_string(),
        });
    }

    if let Some(t) = audio_suffix {
        if !t.is_empty() {
            naming::validate_template(t).map_err(|e| ConversionCheckError::InvalidAudioSuffixTemplate(e.to_string()))?;
        }
    }
    if let Some(t) = video_suffix {
        if !t.is_empty() {
            naming::validate_template(t).map_err(|e| ConversionCheckError::InvalidVideoSuffixTemplate(e.to_string()))?;
        }
    }
    if !filename_prefix.is_empty() {
        naming::validate_template(filename_prefix).map_err(|e| ConversionCheckError::InvalidPrefixTemplate(e.to_string()))?;
    }

    if !copy_video {
        let codec_id = video_codecs::normalize_video_codec(video_codec);
        if video_codecs::find_codec(codec_id).is_none() {
            let known: Vec<&str> = video_codecs::supported_video_codecs()
                .iter()
                .map(|(k, _)| *k)
                .collect();
            return Err(ConversionCheckError::UnknownVideoCodec {
                codec: video_codec.to_string(),
                supported: known.join(", "),
            });
        }

        if video_codecs::resolve_encoder_chain(codec_id, caps).is_empty() {
            return Err(ConversionCheckError::NoEncoderAvailable {
                codec: codec_id.to_string(),
                candidates: video_codecs::static_encoder_chain(codec_id).join(", "),
            });
        }
    }

    if !encoder_available_in_ffmpeg(audio_encoder, caps) {
        return Err(ConversionCheckError::UnsupportedAudioEncoder {
            encoder: audio_encoder.to_string(),
        });
    }

    if !copy_video && !video_codecs::codec_supports_container(
        video_codecs::normalize_video_codec(video_codec),
        container,
    ) {
        let codec_id = video_codecs::normalize_video_codec(video_codec);
        return Err(ConversionCheckError::IncompatibleVideoContainer {
            codec: codec_id.to_string(),
            container: container.to_string(),
            hint: match codec_id {
                "prores" => "ProRes typically requires MOV or MKV containers.",
                "av1" => "AV1 works in MKV, MP4, and MOV containers.",
                "dnxhd" => "DNxHD requires MXF, MOV, or MKV containers.",
                "h264" | "h265" => "H.264/HEVC work in all containers.",
                _ => "",
            }
            .to_string(),
        });
    }

    if !container_supports_audio_encoder(container, audio_encoder) {
        return Err(ConversionCheckError::IncompatibleAudioContainer {
            encoder: audio_encoder.to_string(),
            container: container.to_string(),
            hint: match audio_encoder {
                "libopus" => "Opus is only supported in MKV and MOV containers.",
                "pcm_s24le" | "pcm_s16le" => "Uncompressed PCM works in all containers.",
                "aac" => "AAC works in all containers.",
                _ => "",
            }
            .to_string(),
        });
    }

    Ok(())
}

/// Validate that all input files and the output folder exist on disk.
pub fn validate_conversion_paths(
    input_files: &[PathBuf],
    output_folder: &Path,
) -> Result<(), ConversionCheckError> {
    for f in input_files {
        if !f.exists() {
            return Err(ConversionCheckError::MissingInput(f.to_path_buf()));
        }
    }
    if !output_folder.as_os_str().is_empty() && !output_folder.exists() {
        return Err(ConversionCheckError::MissingOutputFolder(output_folder.to_path_buf()));
    }
    Ok(())
}

/// Full sanity check: pure validation + filesystem existence checks.
/// Use this before actually starting a conversion.
pub fn conversion_sanity_check(input: SanityCheckInput<'_>) -> Result<(), ConversionCheckError> {
    let SanityCheckInput {
        input_files,
        output_folder,
        ..
    } = input;
    conversion_sanity_check_pure(input)?;
    validate_conversion_paths(input_files, output_folder)
}

/// Pure validation for metadata-only mode — no filesystem access.
pub fn conversion_sanity_check_metadata_only_pure(
    input_files: &[PathBuf],
    _output_folder: &Path,
    filename_prefix: &str,
    caps: &FfmpegCapabilities,
    audio_suffix: Option<&str>,
    video_suffix: Option<&str>,
) -> Result<(), ConversionCheckError> {
    if !caps.has_ffmpeg {
        return Err(ConversionCheckError::FfmpegUnavailable(None));
    }

    if input_files.is_empty() {
        return Err(ConversionCheckError::NoInputFiles);
    }

    if filename_prefix.is_empty()
        && audio_suffix.unwrap_or("").is_empty()
        && video_suffix.unwrap_or("").is_empty()
    {
        return Err(ConversionCheckError::NoOutputName);
    }

    if let Some(t) = audio_suffix {
        if !t.is_empty() {
            naming::validate_template(t)
                .map_err(|e| ConversionCheckError::InvalidAudioSuffixTemplate(e.to_string()))?;
        }
    }
    if let Some(t) = video_suffix {
        if !t.is_empty() {
            naming::validate_template(t)
                .map_err(|e| ConversionCheckError::InvalidVideoSuffixTemplate(e.to_string()))?;
        }
    }
    if !filename_prefix.is_empty() {
        naming::validate_template(filename_prefix)
            .map_err(|e| ConversionCheckError::InvalidPrefixTemplate(e.to_string()))?;
    }

    Ok(())
}

/// Full sanity check for metadata-only mode: pure + filesystem existence checks.
pub fn conversion_sanity_check_metadata_only(
    input_files: &[PathBuf],
    output_folder: &Path,
    filename_prefix: &str,
    caps: &FfmpegCapabilities,
    audio_suffix: Option<&str>,
    video_suffix: Option<&str>,
) -> Result<(), ConversionCheckError> {
    conversion_sanity_check_metadata_only_pure(
        input_files, output_folder, filename_prefix, caps, audio_suffix, video_suffix,
    )?;
    validate_conversion_paths(input_files, output_folder)
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConvertBlocker {
    NoRecording,
    NoPrefix,
    NoOutputFolder,
    FfmpegNotQueried,
    FfmpegMissing(Option<String>),
}

#[derive(Clone, Debug)]
pub struct ConvertReadiness {
    pub can_convert: bool,
    pub blockers: Vec<ConvertBlocker>,
}

pub fn evaluate_readiness(
    has_group: bool,
    prefix_empty: bool,
    output_folder_empty: bool,
    caps: Option<&FfmpegCapabilities>,
) -> ConvertReadiness {
    let mut blockers = Vec::new();
    if !has_group {
        blockers.push(ConvertBlocker::NoRecording);
    }
    if prefix_empty {
        blockers.push(ConvertBlocker::NoPrefix);
    }
    if output_folder_empty {
        blockers.push(ConvertBlocker::NoOutputFolder);
    }
    match caps {
        None => blockers.push(ConvertBlocker::FfmpegNotQueried),
        Some(c) if !c.has_ffmpeg => blockers.push(ConvertBlocker::FfmpegMissing(c.error_message.clone())),
        Some(_) => {}
    }
    ConvertReadiness {
        can_convert: blockers.is_empty(),
        blockers,
    }
}

pub fn format_blockers(blockers: &[ConvertBlocker]) -> String {
    let imperatives: Vec<&str> = blockers.iter().filter_map(|b| match b {
        ConvertBlocker::NoRecording => Some("select a recording"),
        ConvertBlocker::NoPrefix => Some("set a filename prefix"),
        ConvertBlocker::NoOutputFolder => Some("choose an output folder"),
        ConvertBlocker::FfmpegNotQueried | ConvertBlocker::FfmpegMissing(_) => None,
    }).collect();

    let ffmpeg_messages: Vec<String> = blockers.iter().filter_map(|b| match b {
        ConvertBlocker::NoRecording
        | ConvertBlocker::NoPrefix
        | ConvertBlocker::NoOutputFolder => None,
        ConvertBlocker::FfmpegNotQueried => {
            Some("ffmpeg availability is being checked…".to_string())
        }
        ConvertBlocker::FfmpegMissing(msg) => {
            let base = "ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH.";
            match msg {
                Some(detail) if !detail.is_empty() => Some(format!("{} ({})", base, detail)),
                _ => Some(base.to_string()),
            }
        }
    }).collect();

    let mut parts: Vec<String> = Vec::new();
    if !imperatives.is_empty() {
        parts.push(format!("To convert, please {}.", imperatives.join(", ")));
    }
    parts.extend(ffmpeg_messages);
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use crate::converter::test_fixtures::*;
    use std::path::PathBuf;

    #[test]
    fn test_pure_ok_with_nonexistent_paths() {
        let mut fmts = BTreeSet::new();
        fmts.insert("mov");
        let mut encs = BTreeSet::new();
        encs.insert("pcm_s24le");
        let caps = make_caps(true, encs, fmts);
        let files = vec![PathBuf::from("/nonexistent/file.wav")];
        let result = conversion_sanity_check_pure(SanityCheckInput {
            container: "mov",
            video_codec: "h264",
            audio_encoder: "pcm_s24le",
            input_files: &files,
            output_folder: Path::new("/nonexistent/out"),
            filename_prefix: "test",
            caps: &caps,
            audio_suffix: Some("_audio"),
            video_suffix: Some("_video"),
            copy_video: true,
        });
        assert!(result.is_ok(), "pure check should not fail on nonexistent paths: {:?}", result);
    }

    #[test]
    fn test_full_fails_on_nonexistent_paths() {
        let mut fmts = BTreeSet::new();
        fmts.insert("mov");
        let mut encs = BTreeSet::new();
        encs.insert("pcm_s24le");
        let caps = make_caps(true, encs, fmts);
        let files = vec![PathBuf::from("/nonexistent/file.wav")];
        let result = conversion_sanity_check(SanityCheckInput {
            container: "mov",
            video_codec: "h264",
            audio_encoder: "pcm_s24le",
            input_files: &files,
            output_folder: Path::new("/nonexistent/out"),
            filename_prefix: "test",
            caps: &caps,
            audio_suffix: Some("_audio"),
            video_suffix: Some("_video"),
            copy_video: true,
        });
        match result {
            Err(ConversionCheckError::MissingInput(p)) => {
                assert_eq!(p, PathBuf::from("/nonexistent/file.wav"));
            }
            other => panic!("expected MissingInput, got {:?}", other.err()),
        }
    }

    #[test]
    fn test_pure_fails_on_empty_input() {
        let caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        let result = conversion_sanity_check_pure(SanityCheckInput {
            container: "mov",
            video_codec: "h265",
            audio_encoder: "pcm_s24le",
            input_files: &[],
            output_folder: Path::new("/tmp"),
            filename_prefix: "test",
            caps: &caps,
            audio_suffix: Some("_audio"),
            video_suffix: Some("_video"),
            copy_video: false,
        });
        assert_eq!(result, Err(ConversionCheckError::NoInputFiles));
    }

    #[test]
    fn test_validate_paths_ok_when_existing() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, b"dummy").unwrap();
        let result = validate_conversion_paths(
            &[file_path],
            dir.path(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_paths_fails_on_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.txt");
        let result = validate_conversion_paths(std::slice::from_ref(&missing), dir.path());
        match result {
            Err(ConversionCheckError::MissingInput(p)) => assert_eq!(p, missing),
            other => panic!("expected MissingInput, got {:?}", other.err()),
        }
    }

    #[test]
    fn test_validate_paths_fails_on_missing_output_folder() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        std::fs::write(&file_path, b"dummy").unwrap();
        let result = validate_conversion_paths(
            &[file_path],
            Path::new("/nonexistent/output_dir"),
        );
        match result {
            Err(ConversionCheckError::MissingOutputFolder(p)) => {
                assert_eq!(p, PathBuf::from("/nonexistent/output_dir"));
            }
            other => panic!("expected MissingOutputFolder, got {:?}", other.err()),
        }
    }

    #[test]
    fn test_readiness_unqueried_caps_is_not_missing() {
        let r = evaluate_readiness(true, false, false, None);
        assert!(!r.can_convert);
        assert!(r.blockers.contains(&ConvertBlocker::FfmpegNotQueried));
        assert!(!r.blockers.contains(&ConvertBlocker::FfmpegMissing(None)));
    }

    #[test]
    fn test_readiness_ready_when_all_met() {
        let caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        let r = evaluate_readiness(true, false, false, Some(&caps));
        assert!(r.can_convert);
        assert!(r.blockers.is_empty());
    }

    #[test]
    fn test_readiness_missing_ffmpeg() {
        let caps = make_caps(false, BTreeSet::new(), BTreeSet::new());
        let r = evaluate_readiness(true, false, false, Some(&caps));
        assert!(!r.can_convert);
        assert!(r.blockers.contains(&ConvertBlocker::FfmpegMissing(None)));
    }

    #[test]
    fn test_readiness_missing_ffmpeg_with_error() {
        let mut caps = make_caps(false, BTreeSet::new(), BTreeSet::new());
        caps.error_message = Some("permission denied".into());
        let r = evaluate_readiness(true, false, false, Some(&caps));
        assert!(!r.can_convert);
        assert_eq!(
            r.blockers,
            vec![ConvertBlocker::FfmpegMissing(Some("permission denied".into()))]
        );
    }

    #[test]
    fn test_readiness_no_group() {
        let caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        let r = evaluate_readiness(false, false, false, Some(&caps));
        assert!(!r.can_convert);
        assert!(r.blockers.contains(&ConvertBlocker::NoRecording));
    }

    #[test]
    fn test_readiness_no_prefix() {
        let caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        let r = evaluate_readiness(true, true, false, Some(&caps));
        assert!(!r.can_convert);
        assert!(r.blockers.contains(&ConvertBlocker::NoPrefix));
    }

    #[test]
    fn test_readiness_no_output_folder() {
        let caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        let r = evaluate_readiness(true, false, true, Some(&caps));
        assert!(!r.can_convert);
        assert!(r.blockers.contains(&ConvertBlocker::NoOutputFolder));
    }

    #[test]
    fn test_readiness_multiple_blockers() {
        let caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        let r = evaluate_readiness(false, true, true, Some(&caps));
        assert!(!r.can_convert);
        // Order is deterministic in evaluate_readiness: recording, prefix,
        // output folder.
        assert_eq!(
            r.blockers,
            vec![
                ConvertBlocker::NoRecording,
                ConvertBlocker::NoPrefix,
                ConvertBlocker::NoOutputFolder,
            ]
        );
    }

    #[test]
    // test-lint: allow(text-pin): format_blockers is a human-facing formatter; the wording is its Display contract (WP-T5 plan: KEEP)
    fn test_format_blockers_imperative_only() {
        let blockers = vec![
            ConvertBlocker::NoRecording,
            ConvertBlocker::NoPrefix,
            ConvertBlocker::NoOutputFolder,
        ];
        let msg = format_blockers(&blockers);
        assert!(msg.contains("select a recording"));
        assert!(msg.contains("set a filename prefix"));
        assert!(msg.contains("choose an output folder"));
        assert!(!msg.contains("ffmpeg"));
    }

    #[test]
    // test-lint: allow(text-pin): format_blockers is a human-facing formatter; the wording is its Display contract (WP-T5 plan: KEEP)
    fn test_format_blockers_ffmpeg_not_queried() {
        let blockers = vec![ConvertBlocker::FfmpegNotQueried];
        let msg = format_blockers(&blockers);
        assert!(msg.contains("ffmpeg availability is being checked"));
    }

    #[test]
    // test-lint: allow(text-pin): format_blockers is a human-facing formatter; the wording is its Display contract (WP-T5 plan: KEEP)
    fn test_format_blockers_ffmpeg_missing() {
        let blockers = vec![ConvertBlocker::FfmpegMissing(None)];
        let msg = format_blockers(&blockers);
        assert!(msg.contains("ffmpeg is not available"));
        assert!(msg.contains("install ffmpeg"));
    }

    #[test]
    // test-lint: allow(text-pin): format_blockers is a human-facing formatter; the wording is its Display contract (WP-T5 plan: KEEP)
    fn test_format_blockers_mixed() {
        let blockers = vec![
            ConvertBlocker::NoRecording,
            ConvertBlocker::FfmpegMissing(Some("not found: No such file".into())),
        ];
        let msg = format_blockers(&blockers);
        assert!(msg.contains("select a recording"));
        assert!(msg.contains("ffmpeg is not available"));
        assert!(msg.contains("not found"));
    }
}