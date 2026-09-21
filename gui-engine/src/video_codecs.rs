//! Codec-level video encoder registry.
//!
//! The converter UI lets the user pick a *video codec* ("av1", "h265", …)
//! instead of a concrete ffmpeg encoder. At conversion time the codec is
//! resolved into an ordered chain of concrete ffmpeg encoders: hardware
//! accelerated candidates first (NVENC / QSV / AMF / MediaFoundation /
//! V4L2 mem2mem), software encoders last as fallbacks.
//!
//! Only encoders that accept ordinary software frame input are listed here
//! (they work with a plain `-c:v <name>` plus the static args below).
//! `*_vaapi` and `*_vulkan` encoders additionally require an initialized
//! hardware device (`-init_hw_device` / `-filter_hw_device`) plus a
//! `format=nv12,hwupload` filter stage, so they are deferred to a follow-up.

use crate::converter::FfmpegCapabilities;

// ── Registry types ───────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderClass {
    Hardware,
    Software,
}

/// A concrete ffmpeg encoder and its per-encoder argument requirements.
#[derive(Clone, Copy, Debug)]
pub struct EncoderCandidate {
    pub name: &'static str,
    pub class: EncoderClass,
    /// Extra ffmpeg args required by this encoder (e.g. `-pix_fmt yuv420p`),
    /// applied after `-c:v` and after the codec-level args.
    pub args: &'static [(&'static str, &'static str)],
}

/// A user-facing video codec: what the dropdown shows, which containers it
/// can be muxed into, and the priority-ordered encoder candidate chain.
#[derive(Clone, Copy, Debug)]
pub struct VideoCodecSpec {
    /// Stable id stored in settings/UI state ("av1", "h265", …).
    pub id: &'static str,
    /// Human-readable dropdown label.
    pub label: &'static str,
    /// Containers this codec may be muxed into.
    pub containers: &'static [&'static str],
    /// Codec-level args applied regardless of the chosen candidate
    /// (e.g. `-tag:v hvc1` for HEVC).
    pub codec_args: &'static [(&'static str, &'static str)],
    /// Priority-ordered candidates: hardware first, software last.
    pub candidates: &'static [EncoderCandidate],
}

const HW: EncoderClass = EncoderClass::Hardware;
const SW: EncoderClass = EncoderClass::Software;
const NO_ARGS: &[(&str, &str)] = &[];
const YUV420P: &[(&str, &str)] = &[("pix_fmt", "yuv420p")];

pub static VIDEO_CODECS: &[VideoCodecSpec] = &[
    VideoCodecSpec {
        id: "prores",
        label: "ProRes — ideal for Resolve, larger files",
        containers: &["mov", "mkv"],
        codec_args: NO_ARGS,
        candidates: &[
            EncoderCandidate { name: "prores_ks", class: SW, args: &[("profile:v", "0"), ("pix_fmt", "yuv422p10le")] },
            EncoderCandidate { name: "prores_aw", class: SW, args: &[("pix_fmt", "yuv422p10le")] },
        ],
    },
    VideoCodecSpec {
        id: "dnxhd",
        label: "DNxHD — broadcast codec, ideal for MXF",
        containers: &["mxf", "mov", "mkv"],
        codec_args: NO_ARGS,
        candidates: &[
            EncoderCandidate {
                name: "dnxhd",
                class: SW,
                args: &[("pix_fmt", "yuv422p"), ("profile:v", "dnxhd"), ("b:v", "36M")],
            },
        ],
    },
    VideoCodecSpec {
        id: "h264",
        label: "H.264 — maximum compatibility",
        containers: &["mkv", "mov", "mp4", "mxf"],
        codec_args: NO_ARGS,
        candidates: &[
            EncoderCandidate { name: "h264_nvenc", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "h264_qsv", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "h264_amf", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "h264_mf", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "h264_v4l2m2m", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "libx264", class: SW, args: YUV420P },
        ],
    },
    VideoCodecSpec {
        id: "h265",
        label: "H.265/HEVC — efficient, Resolve-compatible",
        containers: &["mkv", "mov", "mp4", "mxf"],
        codec_args: &[("tag:v", "hvc1")],
        candidates: &[
            EncoderCandidate { name: "hevc_nvenc", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "hevc_qsv", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "hevc_amf", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "hevc_mf", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "hevc_v4l2m2m", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "libx265", class: SW, args: YUV420P },
        ],
    },
    VideoCodecSpec {
        id: "av1",
        label: "AV1 — good compression, widely supported",
        containers: &["mkv", "mov", "mp4"],
        codec_args: NO_ARGS,
        candidates: &[
            EncoderCandidate { name: "av1_nvenc", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "av1_qsv", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "av1_amf", class: HW, args: NO_ARGS },
            EncoderCandidate { name: "libsvtav1", class: SW, args: YUV420P },
            EncoderCandidate { name: "libaom-av1", class: SW, args: YUV420P },
            EncoderCandidate { name: "librav1e", class: SW, args: YUV420P },
        ],
    },
];

/// Codec selected by `select_best_combination` when nothing else matches.
pub const DEFAULT_VIDEO_CODEC: &str = "av1";

// ── Lookups ──────────────────────────────────────────────────────────────

pub fn find_codec(codec_id: &str) -> Option<&'static VideoCodecSpec> {
    VIDEO_CODECS.iter().find(|c| c.id == codec_id)
}

pub fn supported_video_codecs() -> Vec<(&'static str, &'static str)> {
    VIDEO_CODECS.iter().map(|c| (c.id, c.label)).collect()
}

pub fn codec_supports_container(codec_id: &str, container: &str) -> bool {
    find_codec(codec_id)
        .map(|c| c.containers.contains(&container))
        .unwrap_or(false)
}

/// True when `name` is a concrete encoder listed in the registry.
pub fn is_known_encoder(name: &str) -> bool {
    VIDEO_CODECS
        .iter()
        .any(|c| c.candidates.iter().any(|cand| cand.name == name))
}

/// Per-encoder args for a concrete encoder; empty for unknown names.
pub fn candidate_args(encoder: &str) -> &'static [(&'static str, &'static str)] {
    VIDEO_CODECS
        .iter()
        .flat_map(|c| c.candidates.iter())
        .find(|cand| cand.name == encoder)
        .map(|cand| cand.args)
        .unwrap_or(NO_ARGS)
}

/// Codec-level args for a codec id; empty for unknown ids.
pub fn codec_args(codec_id: &str) -> &'static [(&'static str, &'static str)] {
    find_codec(codec_id).map(|c| c.codec_args).unwrap_or(NO_ARGS)
}

pub fn encoder_class(encoder: &str) -> Option<EncoderClass> {
    VIDEO_CODECS
        .iter()
        .flat_map(|c| c.candidates.iter())
        .find(|cand| cand.name == encoder)
        .map(|cand| cand.class)
}

// ── Resolution ───────────────────────────────────────────────────────────

/// Full static candidate chain for a codec, independent of ffmpeg
/// availability. Unknown ids resolve to a single-element chain containing
/// the id itself (passed through to ffmpeg as-is).
pub fn static_encoder_chain(codec_id: &str) -> Vec<String> {
    match find_codec(codec_id) {
        Some(spec) => spec.candidates.iter().map(|c| c.name.to_string()).collect(),
        None => vec![codec_id.to_string()],
    }
}

/// Ordered, ffmpeg-available encoder candidates for a codec: hardware
/// candidates first, software fallbacks last. Empty when the ffmpeg build
/// lists none of the codec's candidates.
pub fn resolve_encoder_chain(codec_id: &str, caps: &FfmpegCapabilities) -> Vec<String> {
    static_encoder_chain(codec_id)
        .into_iter()
        .filter(|name| caps.available_encoders.contains(name.as_str()))
        .collect()
}

/// Intersection of supported codecs with (a) container compatibility and
/// (b) at least one available ffmpeg encoder. This is the dropdown source.
pub fn available_video_codecs(
    container: &str,
    caps: &FfmpegCapabilities,
) -> Vec<(&'static str, &'static str)> {
    VIDEO_CODECS
        .iter()
        .filter(|c| c.containers.contains(&container))
        .filter(|c| !resolve_encoder_chain(c.id, caps).is_empty())
        .map(|c| (c.id, c.label))
        .collect()
}

/// Map legacy concrete encoder names (and hardware variants that are not
/// registry candidates, like `*_vaapi` / `*_vulkan`) to their codec id.
/// Codec ids pass through unchanged; unknown strings pass through unchanged.
pub fn normalize_video_codec(input: &str) -> &str {
    for spec in VIDEO_CODECS {
        if spec.id == input || spec.candidates.iter().any(|c| c.name == input) {
            return spec.id;
        }
    }
    if input.starts_with("h264_") {
        return "h264";
    }
    if input.starts_with("hevc_") {
        return "h265";
    }
    if input.starts_with("av1_") {
        return "av1";
    }
    if input.starts_with("prores") {
        return "prores";
    }
    input
}

/// Human-readable summary of the resolution chain, e.g.
/// `"av1_nvenc (hardware) → libsvtav1 → libaom-av1"`, or a hint that no
/// candidate is available.
pub fn describe_chain(codec_id: &str, caps: &FfmpegCapabilities) -> String {
    let chain = resolve_encoder_chain(codec_id, caps);
    if chain.is_empty() {
        return format!(
            "no available encoder (needs one of: {})",
            static_encoder_chain(codec_id).join(", ")
        );
    }
    chain
        .iter()
        .map(|name| match encoder_class(name) {
            Some(EncoderClass::Hardware) => format!("{} (hardware)", name),
            _ => name.clone(),
        })
        .collect::<Vec<_>>()
        .join(" → ")
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn caps<I: IntoIterator<Item = &'static str>>(encoders: I) -> FfmpegCapabilities {
        FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: encoders.into_iter().map(String::from).collect(),
            available_formats: BTreeSet::new(),
            error_message: None,
        }
    }

    /// Linux AV1 encoder list (from `ffmpeg -encoders` on Linux).
    const LINUX_AV1: &[&str] = &[
        "libaom-av1", "librav1e", "libsvtav1", "av1_nvenc", "av1_qsv", "av1_vaapi", "av1_vulkan",
    ];

    /// Windows AV1 encoder list.
    const WINDOWS_AV1: &[&str] = &["libaom-av1", "av1_nvenc", "av1_qsv", "av1_amf"];

    fn strs(v: &[String]) -> Vec<&str> {
        v.iter().map(String::as_str).collect()
    }

    #[test]
    fn test_static_chain_orders_hardware_first() {
        assert_eq!(
            strs(&static_encoder_chain("av1")),
            vec!["av1_nvenc", "av1_qsv", "av1_amf", "libsvtav1", "libaom-av1", "librav1e"]
        );
        assert_eq!(
            strs(&static_encoder_chain("h265")),
            vec![
                "hevc_nvenc", "hevc_qsv", "hevc_amf", "hevc_mf", "hevc_v4l2m2m", "libx265"
            ]
        );
        assert_eq!(strs(&static_encoder_chain("prores")), vec!["prores_ks", "prores_aw"]);
        assert_eq!(strs(&static_encoder_chain("dnxhd")), vec!["dnxhd"]);
    }

    #[test]
    fn test_resolve_chain_filters_availability_and_keeps_priority() {
        let c = caps(LINUX_AV1.iter().copied());
        // av1_vaapi / av1_vulkan exist in ffmpeg but are not registry
        // candidates (they need hw-frame plumbing), so they are filtered out.
        assert_eq!(
            strs(&resolve_encoder_chain("av1", &c)),
            vec!["av1_nvenc", "av1_qsv", "libsvtav1", "libaom-av1", "librav1e"]
        );
    }

    #[test]
    fn test_resolve_chain_windows_av1() {
        let c = caps(WINDOWS_AV1.iter().copied());
        assert_eq!(
            strs(&resolve_encoder_chain("av1", &c)),
            vec!["av1_nvenc", "av1_qsv", "av1_amf", "libaom-av1"]
        );
    }

    #[test]
    fn test_resolve_chain_software_only() {
        let c = caps(["libsvtav1"]);
        assert_eq!(strs(&resolve_encoder_chain("av1", &c)), vec!["libsvtav1"]);
    }

    #[test]
    fn test_resolve_chain_empty_when_none_available() {
        let c = caps(["libx264"]);
        assert!(resolve_encoder_chain("av1", &c).is_empty());
    }

    #[test]
    fn test_static_chain_unknown_codec_passthrough() {
        assert_eq!(strs(&static_encoder_chain("weird")), vec!["weird"]);
    }

    #[test]
    fn test_codec_supports_container_matrix() {
        assert!(codec_supports_container("av1", "mkv"));
        assert!(codec_supports_container("av1", "mp4"));
        assert!(!codec_supports_container("av1", "mxf"));
        assert!(codec_supports_container("prores", "mov"));
        assert!(codec_supports_container("prores", "mkv"));
        assert!(!codec_supports_container("prores", "mp4"));
        assert!(codec_supports_container("dnxhd", "mxf"));
        assert!(!codec_supports_container("dnxhd", "mp4"));
        assert!(codec_supports_container("h265", "mxf"));
        assert!(codec_supports_container("h264", "mxf"));
        assert!(!codec_supports_container("unknown", "mkv"));
    }

    #[test]
    fn test_available_video_codecs_for_container() {
        // mov: prores, dnxhd, h264, h265, av1 — only h264/h265 have candidates
        let c = caps(["libx264", "libx265", "pcm_s24le"]);
        let available = available_video_codecs("mov", &c);
        let ids: Vec<&str> = available.iter().map(|(k, _)| *k).collect();
        assert_eq!(ids, vec!["h264", "h265"]);
    }

    #[test]
    fn test_available_video_codecs_excludes_codec_without_candidates() {
        // av1 listed in mkv but no AV1 encoder installed
        let c = caps(["libx264"]);
        let ids: Vec<&str> = available_video_codecs("mkv", &c)
            .iter()
            .map(|(k, _)| *k)
            .collect();
        assert!(!ids.contains(&"av1"));
        assert!(ids.contains(&"h264"));
    }

    #[test]
    fn test_normalize_video_codec() {
        // Already codec ids
        assert_eq!(normalize_video_codec("av1"), "av1");
        assert_eq!(normalize_video_codec("h264"), "h264");
        // Legacy concrete encoder names
        assert_eq!(normalize_video_codec("libsvtav1"), "av1");
        assert_eq!(normalize_video_codec("libx264"), "h264");
        assert_eq!(normalize_video_codec("libx265"), "h265");
        assert_eq!(normalize_video_codec("prores_ks"), "prores");
        assert_eq!(normalize_video_codec("dnxhd"), "dnxhd");
        // Hardware variants not in the registry
        assert_eq!(normalize_video_codec("h264_vaapi"), "h264");
        assert_eq!(normalize_video_codec("hevc_vaapi"), "h265");
        assert_eq!(normalize_video_codec("av1_vulkan"), "av1");
        // Unknown passthrough
        assert_eq!(normalize_video_codec("mystery"), "mystery");
    }

    #[test]
    fn test_candidate_and_codec_args() {
        assert_eq!(candidate_args("libx264"), YUV420P);
        assert_eq!(candidate_args("av1_nvenc"), NO_ARGS);
        assert_eq!(candidate_args("nonexistent"), NO_ARGS);
        assert_eq!(codec_args("h265"), &[("tag:v", "hvc1")]);
        assert_eq!(codec_args("h264"), NO_ARGS);
    }

    #[test]
    fn test_encoder_class() {
        assert_eq!(encoder_class("av1_nvenc"), Some(EncoderClass::Hardware));
        assert_eq!(encoder_class("libsvtav1"), Some(EncoderClass::Software));
        assert_eq!(encoder_class("nope"), None);
    }

    #[test]
    fn test_describe_chain() {
        let c = caps(["av1_nvenc", "libsvtav1"]);
        assert_eq!(describe_chain("av1", &c), "av1_nvenc (hardware) → libsvtav1");
        let empty = caps(["libx264"]);
        assert!(describe_chain("av1", &empty).contains("no available encoder"));
    }

    #[test]
    fn test_supported_video_codecs_ids_unique() {
        let mut ids: Vec<&str> = supported_video_codecs().iter().map(|(k, _)| *k).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), supported_video_codecs().len());
    }
}
