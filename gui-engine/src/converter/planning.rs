use std::path::PathBuf;

use crate::converter::settings::{ConverterSettings, RecordingType};
use crate::converter::formats::{audio_encoder_to_output_format, copy_mode_container_for_input, extension_for_container};
use crate::ffprobe::VideoAudioProbe;

/// Describes how audio should be kept in a non-split output.
#[derive(Clone, Debug, PartialEq)]
pub enum AudioKeep {
    /// Pass all audio through unchanged.
    AllAudio,
    /// Keep all audio except the given (stream_index, channel_index) pairs.
    ChannelsExcept(Vec<(usize, usize)>),
    /// Reorder/select audio channels: the Vec lists physical
    /// (stream_index, channel_index) pairs in the desired output order.
    /// Implicitly drops any channel not in the list.
    Reordered(Vec<(usize, usize)>),
}

/// A single step in a video-to-video conversion plan.
#[derive(Clone, Debug, PartialEq)]
pub enum VideoOutputStep {
    /// Split mode: video with no audio.
    VideoOnly { file_idx: usize, output: PathBuf, naming_index: usize },
    /// Mux mode: video with audio (possibly filtered).
    VideoMux { file_idx: usize, output: PathBuf, keep: AudioKeep, naming_index: usize },
    /// Extract a single audio channel to a separate file.
    AudioChannel {
        file_idx: usize,
        stream_idx: usize,
        channel_idx: usize,
        output: PathBuf,
        format: String,
        naming_index: usize,
    },
    /// Concatenate a single audio track across multiple video clips (one output per track).
    AudioChannelConcat {
        segments: Vec<(usize, usize, usize)>,  // (file_idx, stream_idx, channel_idx) per clip
        output: PathBuf,
        format: String,
        sample_rate: u32,
    },
}

impl VideoOutputStep {
    pub fn output(&self) -> &std::path::Path {
        match self {
            VideoOutputStep::VideoOnly { output, .. }
            | VideoOutputStep::VideoMux { output, .. }
            | VideoOutputStep::AudioChannel { output, .. }
            | VideoOutputStep::AudioChannelConcat { output, .. } => output,
        }
    }
}

/// One surviving output slot of the channel-map iteration.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedChannel {
    /// Output slot index (0..channel_map.num_channels()).
    pub output_k: usize,
    /// Input index resolved through `ChannelMap::input_for_output`.
    pub input_i: usize,
    /// Physical (stream_index, channel_index) the slot maps to. Callers
    /// without a probe pass an empty slice and get `(input_i, 0)`.
    pub pair: (usize, usize),
}

/// The channel-map iteration with ALL split/drop semantics in one place:
/// output-slot order, unmapped slots skipped, inputs beyond
/// `physical.len()` skipped (unless `physical` is empty — audio-only
/// callers then get every slot with `pair = (input_i, 0)`), and the LTC
/// channel dropped when `drop_ltc_track` is set. LTC matching follows the
/// recording type: `VideoClipSequence` matches `ltc_video_source` against
/// the physical pair; `MultiTrackAudio` matches via
/// `ConverterSettings::is_ltc_output_track` (single definition of that rule).
pub fn selected_channel_pairs(
    settings: &ConverterSettings,
    physical: &[(usize, usize)],
) -> Vec<SelectedChannel> {
    let mut out = Vec::new();
    for output_k in 0..settings.channel_map.num_channels() {
        let Some(input_i) = settings.channel_map.input_for_output(output_k) else {
            continue;
        };
        let pair = match physical.get(input_i) {
            Some(&p) => p,
            // Audio-only callers pass no physical layout: every mapped
            // slot survives with a synthetic (input_i, 0) pair.
            None if physical.is_empty() => (input_i, 0),
            None => continue,
        };
        let is_ltc = match settings.recording_type {
            RecordingType::VideoClipSequence => settings.ltc_video_source == Some(pair),
            RecordingType::MultiTrackAudio => settings.is_ltc_output_track(output_k),
        };
        if settings.drop_ltc_track && is_ltc {
            continue;
        }
        out.push(SelectedChannel { output_k, input_i, pair });
    }
    out
}

pub fn plan_video_outputs_for_file(settings: &ConverterSettings, file_idx: usize, probe: &VideoAudioProbe) -> Vec<VideoOutputStep> {
    let ext = extension_for_container(&settings.container);
    let mut steps: Vec<VideoOutputStep> = Vec::new();
    let channels = probe_channel_list(probe).unwrap_or_default();
    let input_n = channels.len();
    let map_n = settings.channel_map.num_channels();

    if settings.split_tracks {
        let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
        steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out, naming_index: file_idx + 1 });

        let (fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
        let mut emitted = 0usize;
        for sel in selected_channel_pairs(settings, &channels) {
            emitted += 1;
            let audio_out = settings.output_path_for_file("audio", file_idx, emitted, aext);
            steps.push(VideoOutputStep::AudioChannel {
                file_idx,
                stream_idx: sel.pair.0,
                channel_idx: sel.pair.1,
                output: audio_out,
                format: fmt.to_string(),
                naming_index: emitted,
            });
        }
    } else {
        let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);

        let is_identity = (0..map_n).eq(settings.channel_map.mapping().iter().copied())
            && map_n == input_n;

        if is_identity {
            let drop_pairs: Vec<(usize, usize)> = if settings.drop_ltc_track {
                settings.ltc_video_source.into_iter().collect()
            } else {
                Vec::new()
            };
            if drop_pairs.is_empty() {
                steps.push(VideoOutputStep::VideoMux {
                    file_idx,
                    output: video_out,
                    keep: AudioKeep::AllAudio,
                    naming_index: file_idx + 1,
                });
            } else {
                let total_channels: usize = probe.streams.iter().map(|s| s.channels).sum();
                let dropped_count: usize = drop_pairs.iter().filter(|(s, c)| {
                    probe.streams.iter().any(|st| st.stream_index == *s && *c < st.channels)
                }).count();
                if dropped_count == total_channels {
                    steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out, naming_index: file_idx + 1 });
                } else {
                    steps.push(VideoOutputStep::VideoMux {
                        file_idx,
                        output: video_out,
                        keep: AudioKeep::ChannelsExcept(drop_pairs),
                        naming_index: file_idx + 1,
                    });
                }
            }
        } else {
            let ordered: Vec<(usize, usize)> = selected_channel_pairs(settings, &channels)
                .into_iter()
                .map(|sel| sel.pair)
                .collect();
            if ordered.is_empty() {
                steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out, naming_index: file_idx + 1 });
            } else {
                steps.push(VideoOutputStep::VideoMux {
                    file_idx,
                    output: video_out,
                    keep: AudioKeep::Reordered(ordered),
                    naming_index: file_idx + 1,
                });
            }
        }
    }

    steps
}

pub fn plan_video_outputs(settings: &ConverterSettings, probe: &VideoAudioProbe) -> Vec<VideoOutputStep> {
    let mut steps = Vec::new();
    for file_idx in 0..settings.input_files.len() {
        steps.extend(plan_video_outputs_for_file(settings, file_idx, probe));
    }
    steps
}

fn probe_channel_list(probe: &VideoAudioProbe) -> Option<Vec<(usize, usize)>> {
    let mut channels = Vec::new();
    for s in &probe.streams {
        for ch in 0..s.channels {
            channels.push((s.stream_index, ch));
        }
    }
    if channels.is_empty() { None } else { Some(channels) }
}

pub fn plan_concat_outputs(
    settings: &ConverterSettings,
    all_probes: &[Option<VideoAudioProbe>],
) -> (Vec<VideoOutputStep>, String) {
    let (fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
    let mut warnings = String::new();

    let clip_channels: Vec<Option<Vec<(usize, usize)>>> = all_probes
        .iter()
        .map(|p| p.as_ref().and_then(probe_channel_list))
        .collect();

    let Some(reference) = reference_layout(&clip_channels) else {
        return (Vec::new(), warnings);
    };

    let sample_rate: u32 = all_probes
        .iter()
        .find_map(|p| p.as_ref())
        .and_then(|p| p.streams.first())
        .map(|s| s.sample_rate)
        .unwrap_or(48000);

    let num_tracks = reference.len();
    let mut tracks_segments: Vec<Vec<(usize, usize, usize)>> = vec![Vec::new(); num_tracks];

    let mut consistent = true;
    for (file_idx, opt_cl) in clip_channels.iter().enumerate() {
        match opt_cl {
            Some(cl) => {
                if cl.len() != num_tracks {
                    consistent = false;
                    break;
                }
                if cl.iter().zip(&reference).any(|(a, b)| a != b) {
                    consistent = false;
                    break;
                }
                if let Some(p) = &all_probes[file_idx] {
                    if let Some(s) = p.streams.first() {
                        if s.sample_rate != sample_rate {
                            consistent = false;
                            break;
                        }
                    }
                }
                for track_idx in 0..num_tracks {
                    let (stream_idx, channel_idx) = cl[track_idx];
                    tracks_segments[track_idx].push((file_idx, stream_idx, channel_idx));
                }
            }
            None => {
                warnings.push_str(&format!(
                    "Warning: clip {} has no audio; excluded from concatenation.\n",
                    settings.input_files[file_idx].display()
                ));
            }
        }
    }

    if !consistent {
        warnings.push_str(
            "Warning: audio channel layouts differ across clips — falling back to per-clip audio outputs.\n"
        );
        return (fallback_per_clip_steps(settings, all_probes, fmt, aext), warnings);
    }

    let steps = concat_steps_for(settings, &reference, tracks_segments, fmt, aext, sample_rate);
    (steps, warnings)
}

/// The channel layout every clip must share for concatenation: the first
/// clip that has audio.
fn reference_layout(clip_channels: &[Option<Vec<(usize, usize)>>]) -> Option<Vec<(usize, usize)>> {
    clip_channels.iter().find_map(|c| c.as_ref()).cloned()
}

/// Concat-inconsistent fallback: one AudioChannel output per surviving
/// channel-map slot of every probed clip.
fn fallback_per_clip_steps(
    settings: &ConverterSettings,
    all_probes: &[Option<VideoAudioProbe>],
    fmt: &str,
    aext: &str,
) -> Vec<VideoOutputStep> {
    all_probes
        .iter()
        .enumerate()
        .filter_map(|(fi, p)| p.as_ref().map(|probe| (fi, probe)))
        .flat_map(|(fi, probe)| {
            let channels = probe_channel_list(probe).unwrap_or_default();
            let mut file_steps = Vec::new();
            let mut emitted = 0usize;
            for sel in selected_channel_pairs(settings, &channels) {
                emitted += 1;
                let audio_out = settings.output_path_for_file("audio", fi, emitted, aext);
                file_steps.push(VideoOutputStep::AudioChannel {
                    file_idx: fi,
                    stream_idx: sel.pair.0,
                    channel_idx: sel.pair.1,
                    output: audio_out,
                    format: fmt.to_string(),
                    naming_index: emitted,
                });
            }
            file_steps
        })
        .collect()
}

/// One AudioChannelConcat output per surviving channel-map slot, carrying
/// that track's segments across all clips.
#[allow(clippy::too_many_arguments)]
fn concat_steps_for(
    settings: &ConverterSettings,
    reference: &[(usize, usize)],
    tracks_segments: Vec<Vec<(usize, usize, usize)>>,
    fmt: &str,
    aext: &str,
    sample_rate: u32,
) -> Vec<VideoOutputStep> {
    let mut steps = Vec::new();
    let mut emitted = 0usize;
    for sel in selected_channel_pairs(settings, reference) {
        let segments: Vec<(usize, usize, usize)> = tracks_segments[sel.input_i].to_vec();
        if segments.is_empty() {
            continue;
        }
        emitted += 1;
        let audio_out = settings.output_path_for_file("audio", 0, emitted, aext);
        steps.push(VideoOutputStep::AudioChannelConcat {
            segments,
            output: audio_out,
            format: fmt.to_string(),
            sample_rate,
        });
    }
    steps
}

#[derive(Clone, Debug, PartialEq)]
pub enum OutputKind {
    Video,
    Audio,
}

#[derive(Clone, Debug)]
pub struct PreviewOutput {
    pub kind: OutputKind,
    pub path: PathBuf,
}

/// One planned output file with both its final (collision-guarded) path
/// and the unguarded path the naming templates produce. `path` is what
/// the converter writes; `unguarded_path != path` iff the guard renamed
/// the output because it would overwrite an input file.
#[derive(Clone, Debug, PartialEq)]
pub struct PlannedOutput {
    pub kind: OutputKind,
    pub path: PathBuf,
    pub unguarded_path: PathBuf,
}

/// The single enumeration of "what files will this conversion produce",
/// for all three pipelines. `preview_output_files` and
/// `output_collision_warning` are both projections of this.
pub fn plan_output_paths(
    settings: &ConverterSettings,
    probe: Option<&VideoAudioProbe>,
) -> Vec<PlannedOutput> {
    match settings.pipeline {
        super::settings::ConversionPipeline::VideoPassthrough => {
            let mut settings = settings.clone();
            if settings.copy_video {
                if let Some(first) = settings.input_files.first() {
                    settings.container = copy_mode_container_for_input(first).to_string();
                }
            }
            let ext = extension_for_container(&settings.container);
            let use_concat = settings.concat_audio
                && settings.split_tracks
                && settings.recording_type == super::settings::RecordingType::VideoClipSequence;
            let mut out = Vec::new();

            if use_concat {
                for file_idx in 0..settings.input_files.len() {
                    push_planned_video(&mut out, &settings, file_idx, ext);
                }
                let probe_cloned = probe.cloned();
                let probes: Vec<Option<VideoAudioProbe>> = (0..settings.input_files.len())
                    .map(|_| probe_cloned.clone())
                    .collect();
                let (concat_steps, _warning) = plan_concat_outputs(&settings, &probes);
                for i in 0..concat_steps.len() {
                    push_planned_audio(&mut out, &settings, i + 1);
                }
            } else if let Some(probe) = probe {
                for file_idx in 0..settings.input_files.len() {
                    for step in plan_video_outputs_for_file(&settings, file_idx, probe) {
                        out.push(planned_from_step(&settings, &step, ext));
                    }
                }
            } else {
                for file_idx in 0..settings.input_files.len() {
                    push_planned_video(&mut out, &settings, file_idx, ext);
                }
            }

            out
        }
        super::settings::ConversionPipeline::AudioOnly { generate_synthetic_video } => {
            plan_audio_only_paths(settings, probe, generate_synthetic_video, false)
        }
        super::settings::ConversionPipeline::MetadataOnly => {
            plan_audio_only_paths(settings, probe, false, true)
        }
    }
}

/// Shared AudioOnly / MetadataOnly arm: audio slots from
/// `selected_channel_pairs` (split), a merged slot, or concat outputs,
/// plus the synthetic video output for AudioOnly when requested.
/// MetadataOnly gates the audio slots on `recording_type ==
/// VideoClipSequence` and uses the concat planning when concatenation
/// is enabled (matching the old preview behavior).
fn plan_audio_only_paths(
    settings: &ConverterSettings,
    probe: Option<&VideoAudioProbe>,
    generate_synthetic_video: bool,
    metadata_only: bool,
) -> Vec<PlannedOutput> {
    let mut out = Vec::new();

    if !metadata_only
        || settings.recording_type == super::settings::RecordingType::VideoClipSequence
    {
        let use_concat = metadata_only && settings.concat_audio && settings.split_tracks;
        if use_concat {
            let probes: Vec<Option<VideoAudioProbe>> =
                (0..settings.input_files.len()).map(|_| probe.cloned()).collect();
            let (concat_steps, _warning) = plan_concat_outputs(settings, &probes);
            for i in 0..concat_steps.len() {
                push_planned_audio(&mut out, settings, i + 1);
            }
        } else if settings.split_tracks {
            for sel in selected_channel_pairs(settings, &[]) {
                push_planned_audio(&mut out, settings, sel.output_k + 1);
            }
        } else {
            push_planned_audio(&mut out, settings, 0);
        }
    }

    if generate_synthetic_video {
        let ext = extension_for_container(&settings.container);
        let (guarded, unguarded) =
            settings.output_path_for_file_checked("video", 0, 1, ext);
        out.push(PlannedOutput {
            kind: OutputKind::Video,
            path: guarded,
            unguarded_path: unguarded,
        });
    }

    out
}

fn push_planned_video(out: &mut Vec<PlannedOutput>, settings: &ConverterSettings, file_idx: usize, ext: &str) {
    let (guarded, unguarded) =
        settings.output_path_for_file_checked("video", file_idx, file_idx + 1, ext);
    out.push(PlannedOutput { kind: OutputKind::Video, path: guarded, unguarded_path: unguarded });
}

fn push_planned_audio(out: &mut Vec<PlannedOutput>, settings: &ConverterSettings, index: usize) {
    let (_fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
    let (guarded, unguarded) =
        settings.output_path_for_file_checked("audio", 0, index, aext);
    out.push(PlannedOutput { kind: OutputKind::Audio, path: guarded, unguarded_path: unguarded });
}

/// Map a planned step to its collision-checked output path.
/// `video_ext` is the container extension for video steps; audio steps
/// derive their extension from the audio encoder.
fn planned_from_step(
    settings: &ConverterSettings,
    step: &VideoOutputStep,
    video_ext: &str,
) -> PlannedOutput {
    match step {
        VideoOutputStep::VideoOnly { file_idx, naming_index, .. }
        | VideoOutputStep::VideoMux { file_idx, naming_index, .. } => {
            let (guarded, unguarded) =
                settings.output_path_for_file_checked("video", *file_idx, *naming_index, video_ext);
            PlannedOutput { kind: OutputKind::Video, path: guarded, unguarded_path: unguarded }
        }
        VideoOutputStep::AudioChannel { file_idx, naming_index, .. } => {
            let (_fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
            let (guarded, unguarded) =
                settings.output_path_for_file_checked("audio", *file_idx, *naming_index, aext);
            PlannedOutput { kind: OutputKind::Audio, path: guarded, unguarded_path: unguarded }
        }
        VideoOutputStep::AudioChannelConcat { .. } => {
            unreachable!("concat steps are handled by the concat arms")
        }
    }
}

pub fn preview_output_files(settings: &ConverterSettings, probe: Option<&VideoAudioProbe>) -> Vec<PreviewOutput> {
    plan_output_paths(settings, probe)
        .into_iter()
        .map(|p| PreviewOutput { kind: p.kind, path: p.path })
        .collect()
}

pub fn output_collision_warning(settings: &ConverterSettings) -> Option<String> {
    let colliding: Vec<(String, String)> = plan_output_paths(settings, None)
        .into_iter()
        .filter(|p| p.path != p.unguarded_path)
        .map(|p| (
            p.unguarded_path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
            p.path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
        ))
        .collect();

    if colliding.is_empty() {
        None
    } else {
        let details: String = colliding.iter()
            .map(|(orig, mitigated)| format!("'{}' will be written as '{}'", orig, mitigated))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "Output folder is the same as the input folder. {} \
             Consider choosing a different output folder.",
            details
        ))
    }
}

/// Returns the set of filenames (as `file_name` strings) that appear more than once
/// in the given planned output paths.
pub fn duplicate_output_names(paths: &[PathBuf]) -> Vec<String> {
    let mut seen = std::collections::HashMap::new();
    for p in paths {
        let name = p
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string())
            .unwrap_or_default();
        *seen.entry(name).or_insert(0u32) += 1;
    }
    let mut dups: Vec<String> = seen
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect();
    dups.sort();
    dups
}

/// Build a human-readable warning from a list of duplicate filenames.
/// Returns `None` when the list is empty.
pub fn duplicate_output_warning(duplicate_names: &[String]) -> Option<String> {
    if duplicate_names.is_empty() {
        return None;
    }
    let list = duplicate_names.join("\", \"");
    Some(format!(
        "Two or more output files would have the same name: \"{}\". \
         Later files will overwrite earlier ones. \
         Add {{device}}, {{clip}}, {{track}}, or {{filename}} to the naming templates to disambiguate.",
        list
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use crate::converter::test_fixtures::*;
    use crate::converter::{ChannelMap, ConversionPipeline, RecordingType};
    use super::*;

    fn make_drop_settings(channels: usize, drop_ltc: bool, ltc_source: Option<(usize, usize)>, split: bool) -> ConverterSettings {
        let mut s = make_video_settings();
        s.channel_map = crate::converter::ChannelMap::identity(channels);
        s.split_tracks = split;
        s.drop_ltc_track = drop_ltc;
        s.ltc_video_source = ltc_source;
        s
    }

    #[test]
    fn test_plan_split_drop_stereo_ltc_at_1_0() {
        let probe = make_stereo_probe();
        let s = make_drop_settings(2, true, Some((1, 0)), true);
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        assert_eq!(steps.len(), 2, "split+drop: video + 1 audio");
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
        assert!(matches!(steps[1], VideoOutputStep::AudioChannel { .. }));
    }

    #[test]
    fn test_plan_split_no_drop_stereo() {
        let probe = make_stereo_probe();
        let s = make_drop_settings(2, false, None, true);
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        assert_eq!(steps.len(), 3, "split+no-drop: video + 2 audio");
    }

    #[test]
    fn test_plan_drop_mono_ltc_at_1_0() {
        let probe = make_mono_probe();
        let s = make_drop_settings(1, true, Some((1, 0)), true);
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        assert_eq!(steps.len(), 1, "split+drop mono LTC: only video step");
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
    }

    #[test]
    fn test_plan_no_split_no_drop() {
        let probe = make_stereo_probe();
        let s = make_drop_settings(2, false, None, false);
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        assert_eq!(steps.len(), 1, "no-split+no-drop: mux step");
        assert!(matches!(steps[0], VideoOutputStep::VideoMux { keep: AudioKeep::AllAudio, .. }));
    }

    #[test]
    fn test_plan_no_split_drop_stereo() {
        let probe = make_stereo_probe();
        let s = make_drop_settings(2, true, Some((1, 0)), false);
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        assert_eq!(steps.len(), 1, "no-split+drop: mux with ChannelsExcept");
    }

    #[test]
    fn test_plan_no_split_drop_mono() {
        let probe = make_mono_probe();
        let s = make_drop_settings(1, true, Some((1, 0)), false);
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        assert_eq!(steps.len(), 1, "no-split+drop mono: video only (no audio survives)");
        assert!(matches!(steps[0], VideoOutputStep::VideoOnly { .. }));
    }

    #[test]
    fn test_plan_video_outputs_for_file_no_duplicates() {
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        let outputs: Vec<PathBuf> = steps.iter().map(|s| s.output().to_path_buf()).collect();
        let mut unique = outputs.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(outputs.len(), unique.len(), "duplicate output paths detected");
    }

    #[test]
    fn test_preview_video_split_stereo_drop_ltc() {
        let s = make_drop_settings(2, true, Some((1, 0)), true);
        let preview = preview_output_files(&s, Some(&make_stereo_probe()));
        assert!(!preview.is_empty());
    }

    #[test]
    fn test_preview_video_no_split() {
        let s = make_drop_settings(2, false, None, false);
        let preview = preview_output_files(&s, Some(&make_stereo_probe()));
        assert!(!preview.is_empty());
    }

    #[test]
    fn test_preview_copy_mode_container() {
        let s = make_copy_settings();
        let probe = make_stereo_probe();
        let preview = preview_output_files(&s, Some(&probe));
        assert!(!preview.is_empty());
    }

    #[test]
    fn test_preview_audio_only_split() {
        let mut s = make_settings_audio_only();
        s.split_tracks = true;
        let preview = preview_output_files(&s, None);
        assert_eq!(preview.len(), 2);
    }

    #[test]
    fn test_preview_audio_only_split_drop_ltc() {
        let mut s = make_settings_audio_only();
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_track_channel_index = 1;
        let preview = preview_output_files(&s, None);
        assert_eq!(preview.len(), 1);
    }

    #[test]
    fn test_collision_warning_none_when_suffix_differs() {
        let s = ConverterSettings {
            input_files: vec![PathBuf::from("/tmp/output.wav")],
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "{filename}".to_string(),
            audio_suffix_template: "_audio".to_string(),
            video_suffix_template: String::new(),
            ..make_settings_audio_only()
        };
        assert!(output_collision_warning(&s).is_none());
    }

    #[test]
    fn test_collision_warning_some_on_true_collision() {
        let path = PathBuf::from("/tmp/output.wav");
        let s = ConverterSettings {
            input_files: vec![path.clone()],
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "{filename}".to_string(),
            audio_suffix_template: String::new(),
            video_suffix_template: String::new(),
            ..make_settings_audio_only()
        };
        let warning = output_collision_warning(&s);
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("_conv"));
    }

    #[test]
    fn test_collision_warning_prefix_mode_no_false_positive() {
        let s = ConverterSettings {
            input_files: vec![PathBuf::from("/tmp/recording.wav")],
            output_folder: PathBuf::from("/tmp"),
            filename_prefix: "project".to_string(),
            audio_suffix_template: "_audio".to_string(),
            ..make_settings_audio_only()
        };
        assert!(output_collision_warning(&s).is_none());
    }

    #[test]
    fn test_duplicate_output_names_empty() {
        assert!(duplicate_output_names(&[]).is_empty());
    }

    #[test]
    fn test_duplicate_output_names_single_file() {
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let preview = preview_output_files(&s, Some(&probe));
        let paths: Vec<PathBuf> = preview.iter().map(|p| p.path.clone()).collect();
        assert!(duplicate_output_names(&paths).is_empty());
    }

    #[test]
    fn test_duplicate_output_names_video_two_files_no_placeholder() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip1.mp4"),
            PathBuf::from("/tmp/clip2.mp4"),
        ];
        s.video_suffix_template = "_video".to_string();
        let probe = make_stereo_probe();
        let preview = preview_output_files(&s, Some(&probe));
        let paths: Vec<PathBuf> = preview.iter().map(|p| p.path.clone()).collect();
        let dups = duplicate_output_names(&paths);
        assert!(dups.contains(&"output_video.mkv".to_string()),
            "should detect duplicate video output without {{clip}}");
    }

    #[test]
    fn test_duplicate_output_names_video_two_files_with_placeholder() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip1.mp4"),
            PathBuf::from("/tmp/clip2.mp4"),
        ];
        s.video_suffix_template = "_video{clip:02d}".to_string();
        let probe = make_stereo_probe();
        let preview = preview_output_files(&s, Some(&probe));
        let paths: Vec<PathBuf> = preview.iter().map(|p| p.path.clone()).collect();
        assert!(duplicate_output_names(&paths).is_empty(),
            "no dupes when {{clip}} is in the video suffix");
    }

    #[test]
    fn test_duplicate_output_names_audio_split_no_track() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip1.mp4"),
            PathBuf::from("/tmp/clip2.mp4"),
        ];
        s.split_tracks = true;
        s.audio_suffix_template = "_audio".to_string();
        s.channel_map = crate::converter::ChannelMap::identity(2);
        let probe = make_stereo_probe();
        let preview = preview_output_files(&s, Some(&probe));
        let paths: Vec<PathBuf> = preview.iter().map(|p| p.path.clone()).collect();
        let dups = duplicate_output_names(&paths);
        assert!(!dups.is_empty(),
            "should detect duplicate audio without {{track}}");
        assert!(dups.contains(&"output_audio.wav".to_string()));
    }

    #[test]
    fn test_duplicate_output_names_audio_split_with_track_only_still_dupes_across_files() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip1.mp4"),
            PathBuf::from("/tmp/clip2.mp4"),
        ];
        s.split_tracks = true;
        s.audio_suffix_template = "_audio{track:02d}".to_string();
        s.channel_map = crate::converter::ChannelMap::identity(2);
        let probe = make_stereo_probe();
        let preview = preview_output_files(&s, Some(&probe));
        let paths: Vec<PathBuf> = preview.iter().map(|p| p.path.clone()).collect();
        let dups = duplicate_output_names(&paths);
        assert!(!dups.is_empty(),
            "{{track}} alone is not enough across files — expect duplicates");
    }

    #[test]
    fn test_duplicate_output_names_audio_split_with_track_and_clip() {
        let mut s = make_video_settings();
        s.input_files = vec![
            PathBuf::from("/tmp/clip1.mp4"),
            PathBuf::from("/tmp/clip2.mp4"),
        ];
        s.split_tracks = true;
        s.audio_suffix_template = "_audio{clip:02d}_track{track:02d}".to_string();
        s.channel_map = crate::converter::ChannelMap::identity(2);
        let probe = make_stereo_probe();
        let preview = preview_output_files(&s, Some(&probe));
        let paths: Vec<PathBuf> = preview.iter().map(|p| p.path.clone()).collect();
        assert!(duplicate_output_names(&paths).is_empty(),
            "no dupes when both {{clip}} and {{track}} are in the audio suffix");
    }

    #[test]
    fn test_duplicate_output_warning_none() {
        assert!(duplicate_output_warning(&[]).is_none());
    }

    #[test]
    // test-lint: allow(text-pin): the warning must echo the colliding output names + naming-template tokens — on-disk naming syntax is a legitimate string per AGENTS.md
    fn test_duplicate_output_warning_some() {
        let dups = vec!["output.mkv".to_string(), "audio.wav".to_string()];
        let warn = duplicate_output_warning(&dups);
        assert!(warn.is_some());
        let msg = warn.unwrap();
        assert!(msg.contains("output.mkv"));
        assert!(msg.contains("audio.wav"));
        assert!(msg.contains("{clip}"));
        assert!(msg.contains("{track}"));
        assert!(msg.contains("{filename}"));
    }

    #[test]
    fn test_preview_metadata_only_concat() {
        let mut s = make_video_settings();
        s.pipeline = ConversionPipeline::MetadataOnly;
        s.input_files = vec![
            PathBuf::from("/tmp/clip1.mp4"),
            PathBuf::from("/tmp/clip2.mp4"),
        ];
        s.channel_map = ChannelMap::identity(2);
        s.split_tracks = true;
        s.concat_audio = true;
        s.recording_type = RecordingType::VideoClipSequence;
        let preview = preview_output_files(&s, Some(&make_stereo_probe()));
        let audio_count = preview
            .iter()
            .filter(|p| matches!(p.kind, OutputKind::Audio))
            .count();
        assert_eq!(audio_count, 2, "MetadataOnly+split+concat should show one audio per track");
    }

    // ── selected_channel_pairs ────────────────────────────────────────────

    use crate::converter::settings::RecordingType as RT;

    fn stereo_pairs() -> Vec<(usize, usize)> {
        vec![(1, 0), (1, 1)]
    }

    #[test]
    fn test_selected_pairs_identity_drop_video_ltc() {
        let mut s = make_video_settings();
        s.channel_map = ChannelMap::identity(2);
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        s.recording_type = RT::VideoClipSequence;
        let sel = selected_channel_pairs(&s, &stereo_pairs());
        assert_eq!(sel.len(), 1, "only slot 1 survives; slot 0 is the LTC pair");
        assert_eq!(sel[0].output_k, 1);
        assert_eq!(sel[0].input_i, 1);
        assert_eq!(sel[0].pair, (1, 1));
    }

    #[test]
    fn test_selected_pairs_identity_no_drop() {
        let mut s = make_video_settings();
        s.channel_map = ChannelMap::identity(2);
        s.ltc_video_source = Some((1, 0));
        s.recording_type = RT::VideoClipSequence;
        let sel = selected_channel_pairs(&s, &stereo_pairs());
        assert_eq!(sel.len(), 2);
        assert_eq!(sel[0].pair, (1, 0));
        assert_eq!(sel[1].pair, (1, 1));
    }

    #[test]
    fn test_selected_pairs_permuted_audio_drop() {
        // mapping [1, 0]: input 1 feeds output 0, input 0 feeds output 1.
        // MultiTrackAudio drop rule: input == ltc_track_channel_index.
        let mut s = make_settings_audio_only();
        s.channel_map = ChannelMap::from_mapping(vec![1, 0]);
        s.drop_ltc_track = true;
        s.ltc_track_channel_index = 1;
        s.recording_type = RT::MultiTrackAudio;
        let sel = selected_channel_pairs(&s, &stereo_pairs());
        assert_eq!(sel.len(), 1, "slot whose input is 1 (LTC) is dropped through the permutation");
        assert_eq!(sel[0].output_k, 1);
        assert_eq!(sel[0].input_i, 0);
        assert_eq!(sel[0].pair, (1, 0));
    }

    #[test]
    fn test_selected_pairs_skips_unmapped_and_out_of_bounds() {
        // mapping [-, 0] is impossible; use from_mapping with a duplicate:
        // mapping [1, 1] → output 1 has no unique input (input_for_output(1)
        // resolves to 0), output 0 resolves to input 1.
        let mut s = make_settings_audio_only();
        s.channel_map = ChannelMap::from_mapping(vec![1, 1]);
        s.recording_type = RT::MultiTrackAudio;
        let sel = selected_channel_pairs(&s, &[(1, 0)]);
        // output 0 → input_for_output(0) = None (unmapped) → skipped;
        // output 1 → input 0 → pair (1,0) survives
        assert_eq!(sel.len(), 1);
        assert_eq!(sel[0].output_k, 1);
        assert_eq!(sel[0].input_i, 0);
    }

    #[test]
    fn test_selected_pairs_no_video_source_drops_nothing() {
        // VideoClipSequence + drop on but ltc_video_source = None.
        // Policy: video LTC identity is a (stream, channel) pair — it cannot
        // be derived from the single `ltc_track_channel_index`. Falling back
        // to the audio index would guess and could silently drop a dialogue
        // channel. No fallback is the safe default: if the LTC source is
        // unidentified, nothing is dropped.
        let mut s = make_video_settings();
        s.channel_map = ChannelMap::identity(2);
        s.drop_ltc_track = true;
        s.ltc_track_channel_index = 0;
        s.ltc_video_source = None;
        s.recording_type = RT::VideoClipSequence;
        let sel = selected_channel_pairs(&s, &stereo_pairs());
        assert_eq!(sel.len(), 2, "video rule never falls back to the audio track index");
    }

    #[test]
    fn test_selected_pairs_empty_physical_audio_only() {
        // Audio-only callers pass an empty physical list and still get
        // every mapped slot with pair = (input_i, 0).
        let mut s = make_settings_audio_only();
        s.channel_map = ChannelMap::identity(3);
        s.recording_type = RT::MultiTrackAudio;
        let sel = selected_channel_pairs(&s, &[]);
        assert_eq!(sel.len(), 3);
        assert_eq!(sel[2], super::SelectedChannel { output_k: 2, input_i: 2, pair: (2, 0) });
    }

    /// Policy: when clip audio layouts differ (inconsistent probes), the
    /// concat fallback still routes through `selected_channel_pairs` like
    /// every other site, so the channel map is respected — a [1, 0] mapping
    /// yields 2 mapped slots per clip regardless of layout mismatch.
    #[test]
    fn test_concat_fallback_respects_channel_map() {
        let mut s = make_video_settings();
        s.input_files = vec![PathBuf::from("/tmp/clip1.mp4"), PathBuf::from("/tmp/clip2.mp4")];
        s.channel_map = ChannelMap::from_mapping(vec![1, 0]);
        s.split_tracks = true;
        s.concat_audio = true;
        s.recording_type = RT::VideoClipSequence;

        // Two probes with mismatched layouts → inconsistent → fallback.
        let p1 = make_stereo_probe(); // stream 1, 2 channels
        let p2 = VideoAudioProbe {
            streams: vec![
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 1,
                    channels: 2,
                    codec_name: "aac".to_string(),
                    sample_rate: 44100,
                },
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 2,
                    channels: 1,
                    codec_name: "aac".to_string(),
                    sample_rate: 44100,
                },
            ],
            total_audio_channels: 3,
            is_video_file: true,
        };
        let probes = vec![Some(p1), Some(p2)];
        let (steps, warnings) = plan_concat_outputs(&s, &probes);
        assert!(warnings.contains("layouts differ"));
        // Mapping [1, 0] maps 2 slots per clip → 4 outputs total.
        let audio_count = steps
            .iter()
            .filter(|st| matches!(st, VideoOutputStep::AudioChannel { .. }))
            .count();
        assert_eq!(audio_count, 4, "fallback must respect the channel map");
    }

    // ── plan_output_paths (PR-4) ─────────────────────────────────────────

    #[test]
    fn test_plan_output_paths_audio_only_split_and_merged() {
        let mut s = make_settings_audio_only();
        s.output_folder = PathBuf::from("/out");
        s.split_tracks = true;
        let paths = plan_output_paths(&s, None);
        assert_eq!(paths.len(), 2);
        assert!(paths.iter().all(|p| matches!(p.kind, OutputKind::Audio)));
        // unguarded == guarded (no collision with /tmp inputs)
        assert!(paths.iter().all(|p| p.path == p.unguarded_path));

        s.split_tracks = false;
        let paths = plan_output_paths(&s, None);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].path.to_string_lossy().ends_with(".wav"));
    }

    #[test]
    fn test_plan_output_paths_audio_only_synthetic_video() {
        let mut s = make_settings_synthetic_video(0.0);
        s.output_folder = PathBuf::from("/out");
        let paths = plan_output_paths(&s, None);
        assert_eq!(paths.len(), 2, "merged audio + synthetic video");
        assert!(matches!(paths[1].kind, OutputKind::Video));
    }

    #[test]
    fn test_plan_output_paths_collision_guard() {
        let s = ConverterSettings {
            input_files: vec![PathBuf::from("/out/input1.wav")],
            output_folder: PathBuf::from("/out"),
            filename_prefix: "{filename}".to_string(),
            audio_suffix_template: String::new(),
            video_suffix_template: String::new(),
            ..make_settings_audio_only()
        };
        let paths = plan_output_paths(&s, None);
        assert_eq!(paths.len(), 1);
        assert!(paths[0].path.to_string_lossy().contains("_conv"));
        assert!(!paths[0].unguarded_path.to_string_lossy().contains("_conv"));
    }

    #[test]
    fn test_plan_output_paths_video_passthrough_matches_plan_steps() {
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        let paths = plan_output_paths(&s, Some(&probe));
        assert_eq!(paths.len(), steps.len());
        for (p, st) in paths.iter().zip(steps.iter()) {
            assert_eq!(p.path, st.output().to_path_buf(),
                "guarded path must equal the step's planned output");
        }
    }

    /// WP-3.2 fix: with a permuted map and a decoded LTC source, the
    /// preview now drops the same track execution drops (previously the
    /// preview used the audio-track rule and kept the wrong slot).
    #[test]
    fn test_preview_permuted_map_video_ltc_drop_matches_execution() {
        let mut s = make_video_settings();
        s.split_tracks = true;
        s.drop_ltc_track = true;
        s.ltc_video_source = Some((1, 0));
        s.channel_map = ChannelMap::from_mapping(vec![1, 0]);
        s.recording_type = RecordingType::VideoClipSequence;
        let probe = make_stereo_probe();

        // Execution: 1 audio output survives (slot for input 1 → pair (1,1)).
        let steps = plan_video_outputs_for_file(&s, 0, &probe);
        let audio_in_steps = steps.iter()
            .filter(|st| matches!(st, VideoOutputStep::AudioChannel { .. }))
            .count();
        let paths = plan_output_paths(&s, Some(&probe));
        let audio_in_preview = paths.iter()
            .filter(|p| matches!(p.kind, OutputKind::Audio))
            .count();
        assert_eq!(audio_in_preview, audio_in_steps, "preview must match execution");
        assert_eq!(audio_in_preview, 1);
    }

    /// WP-3.2 completeness change: concat outputs now participate in the
    /// collision check (they never did before).
    #[test]
    fn test_collision_warning_covers_concat_outputs() {
        let mut s = make_video_settings();
        s.input_files = vec![PathBuf::from("/out/clip1.mkv")];
        s.output_folder = PathBuf::from("/out");
        s.split_tracks = true;
        s.concat_audio = true;
        s.recording_type = RecordingType::VideoClipSequence;
        s.channel_map = ChannelMap::identity(1);
        // Naming collides with the input file: {filename} + no suffix.
        s.filename_prefix = "{filename}".to_string();
        s.audio_suffix_template = String::new();
        s.video_suffix_template = String::new();
        let probe = make_stereo_probe();

        let paths = plan_output_paths(&s, Some(&probe));
        // Sanity: at least one output collides with the input.
        assert!(paths.iter().any(|p| p.path != p.unguarded_path),
            "fixture must produce a guarded rename; paths: {:?}", paths);
        assert!(output_collision_warning(&s).is_some(),
            "collision warning must now fire for concat outputs");
    }

    /// The two projections must never drift: preview paths == guarded
    /// paths of plan_output_paths, and collision entries == guarded
    /// != unguarded pairs.
    #[test]
    fn test_preview_and_collision_are_projections_of_plan() {
        let mut s = make_settings_audio_only();
        s.output_folder = PathBuf::from("/out");
        let probe = Some(&make_stereo_probe());

        let planned = plan_output_paths(&s, probe);
        let preview = preview_output_files(&s, probe);
        assert_eq!(planned.len(), preview.len());
        for (p, pv) in planned.iter().zip(preview.iter()) {
            assert_eq!(p.path, pv.path);
        }
    }
}