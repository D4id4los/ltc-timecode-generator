use std::path::PathBuf;

use crate::converter::settings::ConverterSettings;
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
    VideoOnly { file_idx: usize, output: PathBuf },
    /// Mux mode: video with audio (possibly filtered).
    VideoMux { file_idx: usize, output: PathBuf, keep: AudioKeep },
    /// Extract a single audio channel to a separate file.
    AudioChannel { file_idx: usize, stream_idx: usize, channel_idx: usize, output: PathBuf, format: String },
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

pub fn plan_video_outputs_for_file(settings: &ConverterSettings, file_idx: usize, probe: &VideoAudioProbe) -> Vec<VideoOutputStep> {
    let ext = extension_for_container(&settings.container);
    let mut steps: Vec<VideoOutputStep> = Vec::new();
    let channels = probe_channel_list(probe).unwrap_or_default();
    let input_n = channels.len();
    let map_n = settings.channel_map.num_channels();

    if settings.split_tracks {
        let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
        steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out });

        let mut emitted = 0usize;
        for output_k in 0..map_n {
            let Some(input_i) = settings.channel_map.input_for_output(output_k) else {
                continue;
            };
            if input_i >= input_n {
                continue;
            }
            let (stream_idx, channel_idx) = channels[input_i];
            let ltc_match = settings.ltc_video_source == Some((stream_idx, channel_idx));
            if settings.drop_ltc_track && ltc_match {
                continue;
            }
            let (fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
            emitted += 1;
            let audio_out = settings.output_path_for_file("audio", file_idx, emitted, aext);
            steps.push(VideoOutputStep::AudioChannel {
                file_idx,
                stream_idx,
                channel_idx,
                output: audio_out,
                format: fmt.to_string(),
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
                });
            } else {
                let total_channels: usize = probe.streams.iter().map(|s| s.channels).sum();
                let dropped_count: usize = drop_pairs.iter().filter(|(s, c)| {
                    probe.streams.iter().any(|st| st.stream_index == *s && *c < st.channels)
                }).count();
                if dropped_count == total_channels {
                    steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out });
                } else {
                    steps.push(VideoOutputStep::VideoMux {
                        file_idx,
                        output: video_out,
                        keep: AudioKeep::ChannelsExcept(drop_pairs),
                    });
                }
            }
        } else {
            let mut ordered: Vec<(usize, usize)> = Vec::new();
            for output_k in 0..map_n {
                let Some(input_i) = settings.channel_map.input_for_output(output_k) else {
                    continue;
                };
                if input_i >= input_n {
                    continue;
                }
                let pair = channels[input_i];
                let ltc_match = settings.ltc_video_source == Some(pair);
                if settings.drop_ltc_track && ltc_match {
                    continue;
                }
                ordered.push(pair);
            }
            if ordered.is_empty() {
                steps.push(VideoOutputStep::VideoOnly { file_idx, output: video_out });
            } else {
                steps.push(VideoOutputStep::VideoMux {
                    file_idx,
                    output: video_out,
                    keep: AudioKeep::Reordered(ordered),
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
    let mut steps: Vec<VideoOutputStep> = Vec::new();
    let mut warnings = String::new();

    let clip_channels: Vec<Option<Vec<(usize, usize)>>> = all_probes
        .iter()
        .map(|p| p.as_ref().and_then(probe_channel_list))
        .collect();

    let reference = match clip_channels.iter().find_map(|c| c.as_ref()) {
        Some(r) => r.clone(),
        None => return (steps, warnings),
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
        let fallback: Vec<VideoOutputStep> = all_probes
            .iter()
            .enumerate()
            .filter_map(|(fi, p)| p.as_ref().map(|probe| (fi, probe)))
            .flat_map(|(fi, probe)| {
                let mut file_steps = Vec::new();
                for s in &probe.streams {
                    for ch in 0..s.channels {
                        let ltc_match = settings.ltc_video_source == Some((s.stream_index, ch));
                        if settings.drop_ltc_track && ltc_match {
                            continue;
                        }
                        let audio_idx = file_steps.len() + 1;
                        let audio_out = settings.output_path_for_file("audio", fi, audio_idx, aext);
                        file_steps.push(VideoOutputStep::AudioChannel {
                            file_idx: fi,
                            stream_idx: s.stream_index,
                            channel_idx: ch,
                            output: audio_out,
                            format: fmt.to_string(),
                        });
                    }
                }
                file_steps
            })
            .collect();
        return (fallback, warnings);
    }

    let map_n = settings.channel_map.num_channels();
    let mut emitted = 0usize;
    for output_k in 0..map_n {
        let Some(input_i) = settings.channel_map.input_for_output(output_k) else {
            continue;
        };
        if input_i >= num_tracks {
            continue;
        }
        if settings.drop_ltc_track {
            let (ref_stream, ref_ch) = reference[input_i];
            if settings.ltc_video_source == Some((ref_stream, ref_ch)) {
                continue;
            }
        }
        let segments: Vec<(usize, usize, usize)> = tracks_segments[input_i].to_vec();
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

    (steps, warnings)
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

pub fn preview_output_files(settings: &ConverterSettings, probe: Option<&VideoAudioProbe>) -> Vec<PreviewOutput> {
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
            let mut previews = Vec::new();

            if use_concat {
                for file_idx in 0..settings.input_files.len() {
                    let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
                    previews.push(PreviewOutput { kind: OutputKind::Video, path: video_out });
                }

                let probe_cloned = probe.cloned();
                let probes: Vec<Option<VideoAudioProbe>> = (0..settings.input_files.len())
                    .map(|_| probe_cloned.clone())
                    .collect();
                let (concat_steps, _warning) = plan_concat_outputs(&settings, &probes);
                for step in &concat_steps {
                    if let VideoOutputStep::AudioChannelConcat { output, .. } = step {
                        previews.push(PreviewOutput { kind: OutputKind::Audio, path: output.clone() });
                    }
                }
            } else if let Some(probe) = probe {
                for file_idx in 0..settings.input_files.len() {
                    for step in plan_video_outputs_for_file(&settings, file_idx, probe) {
                        previews.push(PreviewOutput {
                            kind: match step {
                                VideoOutputStep::AudioChannel { .. }
                                | VideoOutputStep::AudioChannelConcat { .. } => OutputKind::Audio,
                                _ => OutputKind::Video,
                            },
                            path: step.output().to_path_buf(),
                        });
                    }
                }
            } else {
                for file_idx in 0..settings.input_files.len() {
                    let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
                    previews.push(PreviewOutput { kind: OutputKind::Video, path: video_out });
                }
            }

            previews
        }
        super::settings::ConversionPipeline::AudioOnly { generate_synthetic_video } => {
            let (_fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
            let mut previews = Vec::new();

            if settings.split_tracks {
                for i in 0..settings.channel_map.num_channels() {
                    if settings.drop_ltc_track && i == settings.ltc_track_channel_index {
                        continue;
                    }
                    previews.push(PreviewOutput {
                        kind: OutputKind::Audio,
                        path: settings.output_path_for_index("audio", i + 1, aext),
                    });
                }
            } else {
                previews.push(PreviewOutput {
                    kind: OutputKind::Audio,
                    path: settings.merged_audio_output_path(aext),
                });
            }

            if generate_synthetic_video {
                let ext = extension_for_container(&settings.container);
                previews.push(PreviewOutput {
                    kind: OutputKind::Video,
                    path: settings.output_path_for_index("video", 1, ext),
                });
            }

            previews
        }
        super::settings::ConversionPipeline::MetadataOnly => {
            let (_fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
            let mut previews = Vec::new();
            if settings.recording_type == super::settings::RecordingType::VideoClipSequence {
                let num_channels = settings.channel_map.num_channels();
                if settings.split_tracks {
                    for i in 0..num_channels {
                        if settings.drop_ltc_track && i == settings.ltc_track_channel_index {
                            continue;
                        }
                        previews.push(PreviewOutput {
                            kind: OutputKind::Audio,
                            path: settings.output_path_for_index("audio", i + 1, aext),
                        });
                    }
                } else {
                    previews.push(PreviewOutput {
                        kind: OutputKind::Audio,
                        path: settings.merged_audio_output_path(aext),
                    });
                }
            }
            previews
        }
    }
}

pub fn output_collision_warning(settings: &ConverterSettings) -> Option<String> {
    let input_files = &settings.input_files;
    let mut colliding: Vec<(String, String)> = Vec::new();

    match settings.pipeline {
        super::settings::ConversionPipeline::VideoPassthrough => {
            for i in 0..input_files.len() {
                let ext = if settings.copy_video {
                    copy_mode_container_for_input(&settings.input_files[i])
                } else {
                    extension_for_container(&settings.container)
                };
                let (guarded, unguarded) = settings.output_path_for_file_checked("video", i, i + 1, ext);
                if guarded != unguarded {
                    colliding.push((
                        unguarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                        guarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                    ));
                }
            }
        }
        super::settings::ConversionPipeline::AudioOnly { generate_synthetic_video } => {
            let (_fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
            if settings.split_tracks {
                for i in 0..settings.channel_map.num_channels() {
                    if settings.drop_ltc_track && i == settings.ltc_track_channel_index {
                        continue;
                    }
                    let (guarded, unguarded) = settings.output_path_for_file_checked("audio", 0, i + 1, aext);
                    if guarded != unguarded {
                        colliding.push((
                            unguarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                            guarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                        ));
                    }
                }
            } else {
                let (guarded, unguarded) = settings.output_path_for_file_checked("audio", 0, 0, aext);
                if guarded != unguarded {
                    colliding.push((
                        unguarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                        guarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                    ));
                }
            }
            if generate_synthetic_video {
                let ext = extension_for_container(&settings.container);
                let (guarded, unguarded) = settings.output_path_for_file_checked("video", 0, 1, ext);
                if guarded != unguarded {
                    colliding.push((
                        unguarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                        guarded.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string(),
                    ));
                }
            }
        }
        super::settings::ConversionPipeline::MetadataOnly => {
            let (_fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
            if settings.recording_type == super::settings::RecordingType::VideoClipSequence {
                if settings.split_tracks {
                    for i in 0..settings.channel_map.num_channels() {
                        if settings.drop_ltc_track && i == settings.ltc_track_channel_index {
                            continue;
                        }
                        let (guarded, unguarded) =
                            settings.output_path_for_file_checked("audio", 0, i + 1, aext);
                        if guarded != unguarded {
                            colliding.push((
                                unguarded
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("?")
                                    .to_string(),
                                guarded
                                    .file_name()
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("?")
                                    .to_string(),
                            ));
                        }
                    }
                } else {
                    let (guarded, unguarded) =
                        settings.output_path_for_file_checked("audio", 0, 0, aext);
                    if guarded != unguarded {
                        colliding.push((
                            unguarded
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("?")
                                .to_string(),
                            guarded
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("?")
                                .to_string(),
                        ));
                    }
                }
            }
        }
    }

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
         Add {{clip}}, {{track}}, or {{filename}} to the naming templates to disambiguate.",
        list
    ))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use crate::converter::test_fixtures::*;
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
}