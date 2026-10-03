use std::path::Path;

use crate::converter::capabilities::FfmpegCapabilities;
use crate::video_codecs;

pub fn supported_audio_encoders() -> Vec<(&'static str, &'static str)> {
    vec![
        ("pcm_s24le", "PCM 24-bit — uncompressed, Resolve-compatible"),
        ("pcm_s16le", "PCM 16-bit — uncompressed, smaller"),
        ("aac", "AAC — compressed, good for MP4"),
        ("libopus", "Opus — modern compressed, MKV/MOV only"),
    ]
}

pub fn supported_containers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("mov", "QuickTime MOV — ProRes native, Resolve-friendly"),
        ("mkv", "Matroska MKV — versatile, all codecs"),
        ("mp4", "MPEG-4 MP4 — universal compatibility"),
        ("mxf", "MXF (Material eXchange Format) — professional broadcast"),
    ]
}

pub fn container_supports_audio_encoder(container: &str, encoder: &str) -> bool {
    match container {
        "mkv" => matches!(encoder, "pcm_s24le" | "pcm_s16le" | "aac" | "libopus"),
        "mov" => matches!(encoder, "pcm_s24le" | "pcm_s16le" | "aac" | "libopus"),
        "mp4" => matches!(encoder, "pcm_s24le" | "pcm_s16le" | "aac"),
        "mxf" => matches!(encoder, "pcm_s24le" | "pcm_s16le" | "aac"),
        _ => false,
    }
}

pub fn available_audio_encoders_for_container<'a>(
    container: &str,
    caps: &FfmpegCapabilities,
) -> Vec<(&'a str, &'a str)> {
    supported_audio_encoders()
        .into_iter()
        .filter(|(key, _)| {
            container_supports_audio_encoder(container, key)
                && caps.available_encoders.contains(*key)
        })
        .collect()
}

pub fn available_containers<'a>(caps: &FfmpegCapabilities) -> Vec<(&'a str, &'a str)> {
    supported_containers()
        .into_iter()
        .filter(|(key, _)| {
            let ffmpeg_name = container_to_ffmpeg_format(key);
            caps.available_formats.contains(ffmpeg_name)
        })
        .collect()
}

pub fn select_best_combination(caps: &FfmpegCapabilities) -> (String, String, String) {
    let preferences: &[(&str, &str, &str)] = &[
        ("mov", "h265", "pcm_s24le"),
        ("mp4", "h265", "aac"),
        ("mov", "prores", "pcm_s24le"),
        ("mxf", "dnxhd", "pcm_s24le"),
        ("mov", "h264", "pcm_s24le"),
        ("mkv", "h264", "pcm_s24le"),
        ("mkv", "h265", "aac"),
        ("mp4", "h264", "aac"),
    ];

    let codec_available = |codec: &str| !video_codecs::resolve_encoder_chain(codec, caps).is_empty();

    for &(container, codec, audio) in preferences {
        let ffmpeg_name = container_to_ffmpeg_format(container);
        if caps.available_formats.contains(ffmpeg_name)
            && caps.available_encoders.contains(audio)
            && container_supports_audio_encoder(container, audio)
            && video_codecs::codec_supports_container(codec, container)
            && codec_available(codec)
        {
            return (container.to_string(), codec.to_string(), audio.to_string());
        }
    }

    for (container, _) in supported_containers() {
        let ffmpeg_name = container_to_ffmpeg_format(container);
        if !caps.available_formats.contains(ffmpeg_name) {
            continue;
        }
        for (codec, _) in video_codecs::supported_video_codecs() {
            if !video_codecs::codec_supports_container(codec, container) || !codec_available(codec)
            {
                continue;
            }
            for (audio, _) in supported_audio_encoders() {
                if caps.available_encoders.contains(audio)
                    && container_supports_audio_encoder(container, audio)
                {
                    return (
                        container.to_string(),
                        codec.to_string(),
                        audio.to_string(),
                    );
                }
            }
        }
    }

    (
        "mkv".to_string(),
        video_codecs::DEFAULT_VIDEO_CODEC.to_string(),
        "pcm_s24le".to_string(),
    )
}

pub fn encoder_available_in_ffmpeg(encoder: &str, caps: &FfmpegCapabilities) -> bool {
    caps.available_encoders.contains(encoder)
}

pub fn container_to_ffmpeg_format(container: &str) -> &str {
    match container {
        "mkv" => "matroska",
        _ => container,
    }
}

pub fn format_available_in_ffmpeg(format: &str, caps: &FfmpegCapabilities) -> bool {
    let ffmpeg_name = container_to_ffmpeg_format(format);
    caps.available_formats.contains(ffmpeg_name)
}

pub fn copy_mode_container_for_input(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    crate::media_ext::container_for_input(&ext)
}

pub fn extension_for_container(container: &str) -> &str {
    match container {
        "mov" => "mov",
        "mkv" => "mkv",
        "mp4" => "mp4",
        "mxf" => "mxf",
        _ => container,
    }
}

pub fn audio_encoder_to_output_format(encoder: &str) -> (&str, &str) {
    match encoder {
        "pcm_s24le" | "pcm_s16le" => ("wav", "wav"),
        "aac" => ("adts", "aac"),
        "libopus" => ("opus", "opus"),
        _ => ("wav", "wav"),
    }
}

pub fn apply_available_defaults(
    container: &mut String,
    video_encoder: &mut String,
    audio_encoder: &mut String,
    caps: &FfmpegCapabilities,
) {
    let codec = video_codecs::normalize_video_codec(video_encoder);
    if codec != video_encoder.as_str() {
        *video_encoder = codec.to_string();
    }

    let containers: Vec<&str> = available_containers(caps).iter().map(|(k, _)| *k).collect();
    if !containers.contains(&container.as_str()) {
        let (c, v, a) = select_best_combination(caps);
        *container = c;
        *video_encoder = v;
        *audio_encoder = a;
        return;
    }
    let available = video_codecs::available_video_codecs(container, caps);
    let codecs: Vec<&str> = available.iter().map(|(k, _, _)| k.as_str()).collect();
    let auds: Vec<&str> =
        available_audio_encoders_for_container(container, caps).iter().map(|(k, _)| *k).collect();
    if !codecs.contains(&video_encoder.as_str()) || !auds.contains(&audio_encoder.as_str()) {
        let (c, v, a) = select_best_combination(caps);
        *container = c;
        *video_encoder = v;
        *audio_encoder = a;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use super::*;
    use crate::converter::test_fixtures::*;

    #[test]
    fn test_container_supports_audio_encoder_valid() {
        assert!(container_supports_audio_encoder("mkv", "pcm_s24le"));
        assert!(container_supports_audio_encoder("mov", "aac"));
        assert!(container_supports_audio_encoder("mp4", "aac"));
        assert!(container_supports_audio_encoder("mxf", "pcm_s16le"));
        assert!(container_supports_audio_encoder("mkv", "libopus"));
    }

    #[test]
    fn test_container_rejects_incompatible_audio_encoder() {
        assert!(!container_supports_audio_encoder("mp4", "libopus"));
        assert!(!container_supports_audio_encoder("mxf", "libopus"));
    }

    #[test]
    fn test_container_to_ffmpeg_format() {
        assert_eq!(container_to_ffmpeg_format("mkv"), "matroska");
        assert_eq!(container_to_ffmpeg_format("mov"), "mov");
        assert_eq!(container_to_ffmpeg_format("mp4"), "mp4");
        assert_eq!(container_to_ffmpeg_format("mxf"), "mxf");
    }

    #[test]
    fn test_copy_mode_container_for_input() {
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.mp4")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.m4v")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/CLIP.MOV")), "mov");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.mkv")), "mkv");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.MXF")), "mxf");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.mts")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.M2TS")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.ts")), "mp4");
        assert_eq!(copy_mode_container_for_input(Path::new("/x/clip.avi")), "mkv");
    }

    #[test]
    fn test_select_best_combination_prefers_prores() {
        let mut caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        caps.available_encoders = ["pcm_s24le", "libsvtav1", "libx264", "prores_ks"]
            .into_iter().map(String::from).collect();
        caps.available_formats = ["mov", "matroska", "mp4"].into_iter().map(String::from).collect();
        let (container, codec, audio) = select_best_combination(&caps);
        assert_eq!(container, "mov");
        assert_eq!(codec, "prores");
        assert_eq!(audio, "pcm_s24le");
    }

    #[test]
    fn test_select_best_combination_prefers_h265() {
        let mut caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        caps.available_encoders =
            ["pcm_s24le", "prores_ks", "libx264", "libx265"].into_iter().map(String::from).collect();
        caps.available_formats = ["mov", "matroska", "mp4"].into_iter().map(String::from).collect();
        let (container, codec, audio) = select_best_combination(&caps);
        assert_eq!(container, "mov");
        assert_eq!(codec, "h265");
        assert_eq!(audio, "pcm_s24le");
    }

    #[test]
    fn test_select_best_combination_h265_mp4_fallback() {
        let mut caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        caps.available_encoders = ["aac", "libx265"].into_iter().map(String::from).collect();
        caps.available_formats = ["mp4"].into_iter().map(String::from).collect();
        let (container, codec, audio) = select_best_combination(&caps);
        assert_eq!(container, "mp4");
        assert_eq!(codec, "h265");
        assert_eq!(audio, "aac");
    }

    #[test]
    fn test_apply_defaults_selects_h265_when_available() {
        let caps = make_caps(
            true,
            BTreeSet::from(["prores_ks", "libx264", "libx265", "pcm_s24le"]),
            BTreeSet::from(["mov", "matroska", "mp4"]),
        );
        let mut c = "mkv".to_string();
        let mut v = "av1".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mov");
        assert_eq!(v, "h265");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_select_best_combination_falls_back_to_dnxhd() {
        let mut caps = make_caps(true, BTreeSet::new(), BTreeSet::new());
        caps.available_encoders = ["pcm_s24le", "dnxhd", "libx264"]
            .into_iter().map(String::from).collect();
        caps.available_formats = ["mxf", "mov", "matroska"].into_iter().map(String::from).collect();
        let (container, codec, audio) = select_best_combination(&caps);
        assert_eq!(container, "mxf");
        assert_eq!(codec, "dnxhd");
        assert_eq!(audio, "pcm_s24le");
    }

    #[test]
    fn test_apply_defaults_replaces_invalid_container() {
        let caps = make_caps(true, BTreeSet::from(["prores_ks", "libx264", "pcm_s24le"]), BTreeSet::from(["mov", "matroska"]));
        let mut c = "mxf".to_string();
        let mut v = "h264".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mov");
        assert_eq!(v, "prores");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_apply_defaults_replaces_missing_encoder() {
        let caps = make_caps(true, BTreeSet::from(["libx264", "pcm_s24le"]), BTreeSet::from(["matroska"]));
        let mut c = "mkv".to_string();
        let mut v = "av1".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mkv");
        assert_eq!(v, "h264");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_apply_defaults_normalizes_legacy_encoder_names() {
        let caps = make_caps(true, BTreeSet::from(["libx264", "pcm_s24le"]), BTreeSet::from(["matroska"]));
        let mut c = "mkv".to_string();
        let mut v = "libx264".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mkv");
        assert_eq!(v, "h264");
        assert_eq!(a, "pcm_s24le");
    }

    #[test]
    fn test_apply_defaults_keeps_valid_selection() {
        let caps = make_caps(true, BTreeSet::from(["prores_ks", "pcm_s24le"]), BTreeSet::from(["mov"]));
        let mut c = "mov".to_string();
        let mut v = "prores".to_string();
        let mut a = "pcm_s24le".to_string();
        apply_available_defaults(&mut c, &mut v, &mut a, &caps);
        assert_eq!(c, "mov");
        assert_eq!(v, "prores");
        assert_eq!(a, "pcm_s24le");
    }
}