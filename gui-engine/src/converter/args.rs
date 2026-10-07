use std::path::Path;

use crate::bext_meta;
use crate::camera_meta::CameraInfo;
use crate::converter::formats::container_to_ffmpeg_format;
use crate::converter::planning::{AudioKeep, VideoOutputStep};
use crate::converter::settings::ConverterSettings;
use crate::converter::timecode::{
    format_ffmpeg_timecode, time_reference_samples, TimecodeMetadata,
};
use crate::ffprobe::VideoAudioProbe;
use crate::video_codecs;

pub fn build_audio_to_audio_args(
    settings: &ConverterSettings,
    format: &str,
    timecode_meta: Option<&TimecodeMetadata>,
    sample_rate: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".to_string()];

    for (i, f) in settings.input_files.iter().enumerate() {
        let trim_secs = settings.trim_offsets_secs.get(i).copied().unwrap_or(0.0);
        push_input_with_trim(&mut args, f, trim_secs);
    }

    args.push("-vn".to_string());
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    if let Some(tc) = timecode_meta {
        push_audio_timecode_args(&mut args, tc, format, sample_rate);
    }

    push_metadata_args(&mut args, settings, None, format);
    push_output_trailer(&mut args, format);

    args
}

pub fn build_audio_to_synthetic_video_args(settings: &ConverterSettings) -> Vec<String> {
    let num_channels = settings.channel_map.num_channels();
    let mapping = settings.channel_map.mapping();
    let trim_secs = settings.trim_offsets_secs.first().copied().unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_hw_device_prelude(&mut args, settings);
    args.extend([
        "-f".to_string(),
        "lavfi".to_string(),
        "-i".to_string(),
        "color=c=blue:s=1280x720:r=25".to_string(),
    ]);

    for f in &settings.input_files {
        args.push("-i".to_string());
        args.push(f.to_string_lossy().to_string());
    }

    args.push("-map".to_string());
    args.push("0:v".to_string());

    let trim_enabled = trim_secs > 0.001;
    let mut filter_parts: Vec<String> = Vec::new();
    for (input_idx, &output_ch) in mapping.iter().enumerate().take(num_channels) {
        let idx = input_idx + 1;
        let trim_filter = if trim_enabled {
            format!("atrim=start={:.3}", trim_secs)
        } else {
            String::new()
        };
        if trim_filter.is_empty() {
            filter_parts.push(format!("[{}:a]volume=0dB[a{}]", idx, output_ch + 1));
        } else {
            filter_parts.push(format!(
                "[{}:a]{}[trimmed{}];[trimmed{}]volume=0dB[a{}]",
                idx,
                trim_filter,
                idx,
                idx,
                output_ch + 1,
            ));
        }
    }
    let filter_complex = filter_parts.join(";");
    args.push("-filter_complex".to_string());
    args.push(filter_complex);

    for output_ch in 1..=num_channels {
        args.push("-map".to_string());
        args.push(format!("[a{}]", output_ch));
    }

    push_video_encoder(&mut args, settings);
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.first() {
        push_timecode_args(&mut args, tc, true);
    }

    args.push("-shortest".to_string());
    let format = container_to_ffmpeg_format(&settings.container);
    push_metadata_args(&mut args, settings, None, format);
    push_output_trailer(&mut args, format);

    args
}

pub fn push_video_codec_args(args: &mut Vec<String>, settings: &ConverterSettings) {
    if settings.copy_video {
        args.push("-c:v".to_string());
        args.push("copy".to_string());
        return;
    }
    push_video_encoder(args, settings);
}

pub fn build_video_only_args(settings: &ConverterSettings, file_idx: usize) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings
        .trim_offsets_secs
        .get(file_idx)
        .copied()
        .unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_hw_device_prelude(&mut args, settings);
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push("0:v".to_string());
    args.push("-an".to_string());

    push_video_codec_args(&mut args, settings);

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_timecode_args(&mut args, tc, !settings.copy_video);
    }

    let format = container_to_ffmpeg_format(&settings.container);
    push_metadata_args(&mut args, settings, Some(file_idx), format);
    push_output_trailer(&mut args, format);
    args
}

pub fn build_video_mux_args(
    settings: &ConverterSettings,
    file_idx: usize,
    keep: &AudioKeep,
    probe: &VideoAudioProbe,
) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings
        .trim_offsets_secs
        .get(file_idx)
        .copied()
        .unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_hw_device_prelude(&mut args, settings);
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push("0:v".to_string());

    match keep {
        AudioKeep::AllAudio => push_all_audio_args(&mut args, settings),
        AudioKeep::ChannelsExcept(drop_pairs) => {
            push_channels_except_args(&mut args, settings, drop_pairs, probe)
        }
        AudioKeep::Reordered(ordered_pairs) => {
            push_reordered_args(&mut args, settings, ordered_pairs)
        }
    }

    push_video_codec_args(&mut args, settings);

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_timecode_args(&mut args, tc, !settings.copy_video);
    }

    let format = container_to_ffmpeg_format(&settings.container);
    push_metadata_args(&mut args, settings, Some(file_idx), format);
    push_output_trailer(&mut args, format);
    args
}

/// AllAudio arm: map every audio stream through unchanged (re-encoded
/// unless stream copy is requested).
fn push_all_audio_args(args: &mut Vec<String>, settings: &ConverterSettings) {
    args.push("-map".to_string());
    args.push("0:a?".to_string());
    args.push("-c:a".to_string());
    if settings.copy_video {
        args.push("copy".to_string());
    } else {
        args.push(settings.audio_encoder.clone());
    }
}

/// ChannelsExcept arm: keep all audio except `drop_pairs`, emitting one
/// pan filter per trimmed stream and direct maps for untouched streams.
fn push_channels_except_args(
    args: &mut Vec<String>,
    settings: &ConverterSettings,
    drop_pairs: &[(usize, usize)],
    probe: &VideoAudioProbe,
) {
    let mut filter_parts: Vec<String> = Vec::new();
    let mut output_labels: Vec<String> = Vec::new();
    let mut filter_idx = 0;

    for stream in &probe.streams {
        let surviving = surviving_channels(stream.stream_index, stream.channels, drop_pairs);
        if surviving.is_empty() {
            continue;
        }

        if surviving.len() == stream.channels {
            output_labels.push(format!("0:{}", stream.stream_index));
        } else {
            let label = format!("a{}", filter_idx);
            filter_idx += 1;
            let pan = pan_filter(&surviving);
            filter_parts.push(format!("[0:{}]{}[{}]", stream.stream_index, pan, label));
            output_labels.push(format!("[{}]", label));
        }
    }

    if output_labels.is_empty() {
        args.push("-an".to_string());
    } else {
        if !filter_parts.is_empty() {
            args.push("-filter_complex".to_string());
            args.push(filter_parts.join(";"));
        }
        for label in &output_labels {
            args.push("-map".to_string());
            args.push(label.clone());
        }
        args.push("-c:a".to_string());
        args.push(settings.audio_encoder.clone());
    }
}

/// Reordered arm: one mono pan per pair, merged into a single audio
/// output in the given order.
fn push_reordered_args(
    args: &mut Vec<String>,
    settings: &ConverterSettings,
    ordered_pairs: &[(usize, usize)],
) {
    let mut filter_parts: Vec<String> = Vec::new();
    for (i, &(stream_idx, channel_idx)) in ordered_pairs.iter().enumerate() {
        let label = format!("a{}", i);
        filter_parts.push(format!(
            "[0:{}]pan=mono|FC=c{}[{}]",
            stream_idx, channel_idx, label
        ));
    }
    let n = ordered_pairs.len();
    let merge_inputs: Vec<String> = (0..n).map(|i| format!("[a{}]", i)).collect();
    filter_parts.push(format!("{}amerge=inputs={}[out]", merge_inputs.join(""), n));
    args.push("-filter_complex".to_string());
    args.push(filter_parts.join(";"));
    args.push("-map".to_string());
    args.push("[out]".to_string());
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());
}

/// Channel indices of a stream that survive `drop_pairs`, in order.
fn surviving_channels(
    stream_index: usize,
    channels: usize,
    drop_pairs: &[(usize, usize)],
) -> Vec<usize> {
    (0..channels)
        .filter(|ch| !drop_pairs.contains(&(stream_index, *ch)))
        .collect()
}

/// A `pan=` filter expression placing `surviving` input channels on
/// consecutive outputs with the smallest matching layout. A single
/// surviving channel uses the direct `FC=` routing form.
fn pan_filter(surviving: &[usize]) -> String {
    if surviving.len() == 1 {
        return format!("pan=mono|FC=c{}", surviving[0]);
    }
    let ch_maps: Vec<String> = surviving
        .iter()
        .enumerate()
        .map(|(out_ch, in_ch)| format!("c{}={}", out_ch, in_ch))
        .collect();
    format!(
        "pan={}|{}",
        layout_for_count(surviving.len()),
        ch_maps.join("|")
    )
}

/// Smallest common channel layout carrying `n` channels.
fn layout_for_count(n: usize) -> &'static str {
    match n {
        1 => "mono",
        2 => "stereo",
        _ => "5.1",
    }
}

pub fn build_video_track_extract_args(
    settings: &ConverterSettings,
    file_idx: usize,
    stream_idx: usize,
    channel_idx: usize,
    format: &str,
    sample_rate: u32,
) -> Vec<String> {
    let input = &settings.input_files[file_idx];
    let trim_secs = settings
        .trim_offsets_secs
        .get(file_idx)
        .copied()
        .unwrap_or(0.0);

    let mut args: Vec<String> = vec!["-y".to_string()];
    push_input_with_trim(&mut args, input, trim_secs);
    args.push("-map".to_string());
    args.push(format!("0:{}", stream_idx));
    args.push("-af".to_string());
    args.push(format!("pan=mono|FC=c{}", channel_idx));

    if format == "wav" {
        args.push("-c:a".to_string());
        args.push("pcm_s24le".to_string());
    } else if format == "adts" {
        args.push("-c:a".to_string());
        args.push("aac".to_string());
    } else {
        args.push("-c:a".to_string());
        args.push(settings.audio_encoder.clone());
    }

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.get(file_idx) {
        push_audio_timecode_args(&mut args, tc, format, sample_rate);
    }

    push_metadata_args(&mut args, settings, Some(file_idx), format);
    push_output_trailer(&mut args, format);
    args
}

pub fn build_concat_audio_args(
    settings: &ConverterSettings,
    segments: &[(usize, usize, usize)],
    format: &str,
    sample_rate: u32,
) -> Vec<String> {
    let mut args: Vec<String> = vec!["-y".to_string()];

    for &(file_idx, _stream_idx, _channel_idx) in segments.iter() {
        let trim_secs = settings
            .trim_offsets_secs
            .get(file_idx)
            .copied()
            .unwrap_or(0.0);
        push_input_with_trim(&mut args, &settings.input_files[file_idx], trim_secs);
    }

    let n = segments.len();
    let mut filter_parts: Vec<String> = Vec::new();

    for (i, &(_file_idx, stream_idx, channel_idx)) in segments.iter().enumerate() {
        filter_parts.push(format!(
            "[{}:{}]pan=mono|FC=c{}[a{}]",
            i, stream_idx, channel_idx, i
        ));
    }

    let concat_inputs: String = (0..n)
        .map(|i| format!("[a{}]", i))
        .collect::<Vec<_>>()
        .join("");
    filter_parts.push(format!("{}concat=n={}:v=0:a=1[out]", concat_inputs, n));

    args.push("-filter_complex".to_string());
    args.push(filter_parts.join(";"));
    args.push("-map".to_string());
    args.push("[out]".to_string());
    args.push("-vn".to_string());
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    if let Some(Some(ref tc)) = settings.timecode_meta_per_file.first() {
        push_audio_timecode_args(&mut args, tc, format, sample_rate);
    }

    push_metadata_args(&mut args, settings, None, format);
    push_output_trailer(&mut args, format);
    args
}

pub fn build_video_to_video_args(
    settings: &ConverterSettings,
    step: &VideoOutputStep,
    probe: &VideoAudioProbe,
) -> Vec<String> {
    match step {
        VideoOutputStep::VideoOnly { file_idx, .. } => build_video_only_args(settings, *file_idx),
        VideoOutputStep::VideoMux { file_idx, keep, .. } => {
            build_video_mux_args(settings, *file_idx, keep, probe)
        }
        VideoOutputStep::AudioChannel {
            file_idx,
            stream_idx,
            channel_idx,
            format,
            ..
        } => {
            let sample_rate = probe
                .streams
                .iter()
                .find(|s| s.stream_index == *stream_idx)
                .map(|s| s.sample_rate)
                .unwrap_or(48000);
            build_video_track_extract_args(
                settings,
                *file_idx,
                *stream_idx,
                *channel_idx,
                format,
                sample_rate,
            )
        }
        VideoOutputStep::AudioChannelConcat { .. } => {
            panic!("AudioChannelConcat must be executed directly via run_ffmpeg_process, not through build_video_to_video_args")
        }
    }
}

pub fn push_input_with_trim(args: &mut Vec<String>, file: &Path, trim_secs: f64) {
    if trim_secs > 0.001 {
        args.push("-ss".to_string());
        args.push(format!("{:.3}", trim_secs));
    }
    args.push("-i".to_string());
    args.push(file.to_string_lossy().to_string());
}

/// Push metadata (camera make/model, bext originator) arguments to the
/// ffmpeg arg list, iff `settings.embed_camera_metadata` is enabled.
///
/// `file_idx` selects the per-file camera info; `format` is the ffmpeg output
/// format (e.g. `"wav"`, `"mov"`, `"mp4"`, `"matroska"`).
pub fn push_metadata_args(
    args: &mut Vec<String>,
    settings: &ConverterSettings,
    file_idx: Option<usize>,
    format: &str,
) {
    if !settings.embed_camera_metadata {
        return;
    }

    let camera = file_idx
        .and_then(|i| settings.camera_meta_per_file.get(i))
        .or_else(|| settings.camera_meta_per_file.first())
        .and_then(|c| c.as_ref());

    let file_path = file_idx
        .and_then(|i| settings.input_files.get(i))
        .or_else(|| settings.input_files.first());

    if format == "wav" || format == "rf64" {
        bext_meta::push_wav_bext_args(args, camera, file_path.map(|p| p.as_path()));
    } else {
        let is_mov_like = matches!(format, "mov" | "mp4" | "m4v" | "ipod");
        push_camera_metadata(args, camera, is_mov_like, format);
    }
}

/// Push camera metadata keys for non-WAV outputs.
///
/// This helper is public so `tagger.rs` (ffmpeg fallback) can reuse it.
/// `is_mov_like` controls Apple-compatible key naming; `format` is used for
/// `-movflags` (only for mov/mp4/m4v when extended keys are present).
pub fn push_camera_metadata(
    args: &mut Vec<String>,
    camera: Option<&CameraInfo>,
    is_mov_like: bool,
    _format: &str,
) {
    let c = match camera {
        Some(c) => c,
        None => return,
    };

    // make / model
    if let Some(ref make_str) = c.make {
        if is_mov_like {
            args.push("-metadata".to_string());
            args.push(format!("com.apple.quicktime.make={}", make_str));
        } else {
            args.push("-metadata".to_string());
            args.push(format!("make={}", make_str));
        }
    }
    if let Some(ref model_str) = c.model {
        if is_mov_like {
            args.push("-metadata".to_string());
            args.push(format!("com.apple.quicktime.model={}", model_str));
        } else {
            args.push("-metadata".to_string());
            args.push(format!("model={}", model_str));
        }
    }

    // Extended keys — check if any are present before pushing metadata flags
    let has_extended = c.lens.is_some()
        || c.serial.is_some()
        || c.gamma.is_some()
        || c.exposure_summary.is_some()
        || c.creation_time.is_some();

    if !has_extended {
        return;
    }

    // For mov/mp4/m4v, arbitrary keys need -movflags use_metadata_tags.
    // Multiple occurrences are harmless (ffmpeg uses the last value).
    if is_mov_like {
        args.push("-movflags".to_string());
        args.push("use_metadata_tags".to_string());
    }

    if let Some(ref lens) = c.lens {
        args.push("-metadata".to_string());
        args.push(format!("lens={}", lens));
    }
    if let Some(ref serial) = c.serial {
        args.push("-metadata".to_string());
        args.push(format!("serial={}", serial));
    }
    if let Some(ref gamma) = c.gamma {
        args.push("-metadata".to_string());
        args.push(format!("gamma={}", gamma));
    }
    if let Some(ref ct) = c.creation_time {
        // creation_time is a whitelisted key → maps to ©day in MP4/MOV,
        // DateUTC in Matroska.
        args.push("-metadata".to_string());
        args.push(format!("creation_time={}", ct));
    }
    if let Some(ref summary) = c.exposure_summary {
        // comment maps to ©cmt in MP4/MOV, COMMENT in Matroska
        args.push("-metadata".to_string());
        args.push(format!("comment={}", summary));
    }
}

pub fn push_output_trailer(args: &mut Vec<String>, format: &str) {
    args.push("-progress".to_string());
    args.push("pipe:2".to_string());
    args.push("-f".to_string());
    args.push(format.to_string());
}

pub fn push_hw_device_prelude(args: &mut Vec<String>, settings: &ConverterSettings) {
    if settings.copy_video {
        return;
    }
    if let Some(ref hw) = settings.resolved_hw_device {
        args.extend(hw.prelude_args());
    }
}

pub fn push_video_encoder(args: &mut Vec<String>, settings: &ConverterSettings) {
    let codec_id = video_codecs::normalize_video_codec(&settings.video_encoder);
    let encoder = settings.effective_video_encoder();
    args.push("-c:v".to_string());
    args.push(encoder.clone());

    let needs_hw_upload = video_codecs::hw_frames_for(&encoder).is_some();

    for (key, value) in video_codecs::codec_args(codec_id) {
        args.push(format!("-{}", key));
        args.push((*value).to_string());
    }
    for (key, value) in video_codecs::candidate_args(&encoder) {
        args.push(format!("-{}", key));
        args.push((*value).to_string());
    }
    if needs_hw_upload {
        args.push("-vf".to_string());
        args.push("format=nv12,hwupload".to_string());
    }
}

pub fn push_timecode_args(args: &mut Vec<String>, tc: &TimecodeMetadata, force_frame_rate: bool) {
    let tc_str = format_ffmpeg_timecode(&tc.start, tc.drop_frame);
    args.push("-timecode".to_string());
    args.push(tc_str);
    args.push("-write_tmcd".to_string());
    args.push("1".to_string());
    if force_frame_rate {
        args.push("-r".to_string());
        args.push(format!("{:.3}", tc.fps));
    }
}

pub fn push_audio_timecode_args(
    args: &mut Vec<String>,
    tc: &TimecodeMetadata,
    format: &str,
    sample_rate: u32,
) {
    let tc_str = format_ffmpeg_timecode(&tc.start, tc.drop_frame);
    if format == "wav" {
        let time_reference = time_reference_samples(tc, sample_rate);
        args.push("-write_bext".to_string());
        args.push("1".to_string());
        args.push("-metadata".to_string());
        args.push(format!("time_reference={}", time_reference));
    }
    args.push("-timecode".to_string());
    args.push(tc_str);
}

pub fn build_split_track_args(
    settings: &ConverterSettings,
    format: &str,
    track_idx: usize,
    timecode_meta: Option<&TimecodeMetadata>,
    sample_rate: u32,
) -> Vec<String> {
    let mapping = settings.channel_map.mapping();
    let input_idx = mapping
        .iter()
        .position(|&o| o == track_idx)
        .unwrap_or(track_idx);
    let mut args: Vec<String> = vec!["-y".to_string()];
    let trim_secs = settings
        .trim_offsets_secs
        .get(input_idx)
        .copied()
        .unwrap_or(0.0);

    if input_idx < settings.input_files.len() {
        push_input_with_trim(&mut args, &settings.input_files[input_idx], trim_secs);
    }
    args.push("-vn".to_string());
    args.push("-c:a".to_string());
    args.push(settings.audio_encoder.clone());

    if let Some(tc) = timecode_meta {
        push_audio_timecode_args(&mut args, tc, format, sample_rate);
    }

    push_metadata_args(&mut args, settings, None, format);
    push_output_trailer(&mut args, format);
    args
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_meta::{CameraInfo, CameraMetaSource};
    use crate::converter::planning::VideoOutputStep;
    use crate::converter::test_fixtures::*;
    use audio_core::Timecode;
    use std::path::Path;

    #[test]
    fn test_build_synthetic_video_args_contains_timecode() {
        let mut s = make_settings_synthetic_video(0.0);
        s.timecode_meta_per_file = vec![Some(crate::converter::TimecodeMetadata {
            start: Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            fps: 25.0,
            drop_frame: false,
        })];
        let args = build_audio_to_synthetic_video_args(&s);
        let args_str = args.join(" ");
        assert!(
            args_str.contains("-timecode"),
            "synthetic video args should contain -timecode"
        );
    }

    #[test]
    fn test_build_synthetic_video_args_contains_filter_complex() {
        let s = make_settings_synthetic_video(0.0);
        let args = build_audio_to_synthetic_video_args(&s);
        assert!(
            args.iter().any(|a| a.contains("filter_complex")),
            "synthetic video args should contain filter_complex"
        );
    }

    #[test]
    fn test_build_audio_to_audio_args_no_timecode() {
        let s = make_settings_audio_only();
        let args = build_audio_to_audio_args(&s, "wav", None, 48000);
        assert!(args.contains(&"-vn".to_string()));
        assert!(!args.iter().any(|a| a.starts_with("-timecode")));
    }

    #[test]
    fn test_build_audio_to_audio_args_wav_timecode() {
        let s = make_settings_audio_only();
        let tc = crate::converter::TimecodeMetadata {
            start: Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            fps: 25.0,
            drop_frame: false,
        };
        let args = build_audio_to_audio_args(&s, "wav", Some(&tc), 48000);
        let args_str = args.join(" ");
        assert!(args_str.contains("-timecode"));
        assert!(args_str.contains("-write_bext"));
    }

    #[test]
    fn test_build_audio_to_audio_args_mov_format_no_bext() {
        let s = make_settings_audio_only();
        let tc = crate::converter::TimecodeMetadata {
            start: Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            fps: 25.0,
            drop_frame: false,
        };
        let args = build_audio_to_audio_args(&s, "adts", Some(&tc), 48000);
        let args_str = args.join(" ");
        assert!(args_str.contains("-timecode"));
        assert!(!args_str.contains("-write_bext"));
    }

    #[test]
    fn test_push_video_encoder_codec_and_candidate_args() {
        let mut s = make_settings_audio_only();
        s.video_encoder = "h264".to_string();
        let mut args = vec![];
        push_video_encoder(&mut args, &s);
        assert!(args.contains(&"-c:v".to_string()));
    }

    #[test]
    fn test_push_hw_device_prelude_none_noop() {
        let mut s = make_settings_audio_only();
        s.copy_video = false;
        s.resolved_hw_device = None;
        let mut args = vec![];
        push_hw_device_prelude(&mut args, &s);
        assert!(args.is_empty(), "no prelude when no hw device");
    }

    #[test]
    fn test_push_hw_device_prelude_copy_mode_noop() {
        let mut s = make_settings_audio_only();
        s.copy_video = true;
        let mut args = vec![];
        push_hw_device_prelude(&mut args, &s);
        assert!(args.is_empty(), "no prelude in copy mode");
    }

    #[test]
    fn test_build_video_only_has_map_v_and_an() {
        let s = make_video_settings();
        let args = build_video_only_args(&s, 0);
        let args_str = args.join(" ");
        assert!(args_str.contains("-map"));
        assert!(args_str.contains("0:v"));
        assert!(args_str.contains("-an"));
    }

    #[test]
    fn test_build_video_mux_no_drop_has_audio_map() {
        let s = make_video_settings();
        let probe = make_probe(1, 2, 48000);
        let args = build_video_mux_args(&s, 0, &AudioKeep::AllAudio, &probe);
        let args_str = args.join(" ");
        assert!(args_str.contains("0:a?"));
    }

    // ── build_video_mux_args: ChannelsExcept arm ──────────────────────

    /// ffmpeg arg vectors are legitimate string contracts (AGENTS.md).
    fn map_operand(args: &[String], occurrence: usize) -> &str {
        let mut seen = 0;
        for (i, a) in args.iter().enumerate() {
            if a == "-map" {
                if seen == occurrence {
                    return &args[i + 1];
                }
                seen += 1;
            }
        }
        panic!("no -map occurrence {occurrence}");
    }

    #[test]
    fn test_build_video_mux_channels_except_single_survivor_mono_pan() {
        let s = make_video_settings();
        // Stream 0 with 2 channels; dropping channel 0 leaves one survivor
        // → mono pan filter.
        let probe = make_probe(0, 2, 48000);
        let args = build_video_mux_args(&s, 0, &AudioKeep::ChannelsExcept(vec![(0, 0)]), &probe);
        assert!(args.contains(&"-filter_complex".to_string()));
        assert!(args.contains(&"[0:0]pan=mono|FC=c1[a0]".to_string()));
        assert_eq!(
            map_operand(&args, 1),
            "[a0]",
            "audio maps from the filter label"
        );
        assert!(
            args.contains(&"pcm_s24le".to_string()),
            "re-encoded with the audio encoder"
        );
    }

    #[test]
    fn test_build_video_mux_channels_except_multiple_survivors_pan_layout() {
        let s = make_video_settings();
        // Dropping channel 0 of 3 leaves survivors [1, 2] → stereo pan
        // with channels remapped in surviving order.
        let probe = make_probe(0, 3, 48000);
        let args = build_video_mux_args(&s, 0, &AudioKeep::ChannelsExcept(vec![(0, 0)]), &probe);
        assert!(args.contains(&"[0:0]pan=stereo|c0=1|c1=2[a0]".to_string()));
    }

    #[test]
    fn test_build_video_mux_channels_except_nothing_surviving_is_an() {
        let s = make_video_settings();
        let probe = make_probe(0, 2, 48000);
        let args = build_video_mux_args(
            &s,
            0,
            &AudioKeep::ChannelsExcept(vec![(0, 0), (0, 1)]),
            &probe,
        );
        assert!(args.contains(&"-an".to_string()));
        assert!(!args.contains(&"-filter_complex".to_string()));
        assert!(
            !args.iter().any(|a| a == "0:a?" || a.starts_with("[a")),
            "no audio maps"
        );
    }

    #[test]
    fn test_build_video_mux_channels_except_untouched_stream_maps_directly() {
        let s = make_video_settings();
        // Drop a channel that does not exist: stream 0 keeps all channels
        // → mapped directly as "0:0" with no filter.
        let probe = make_probe(0, 2, 48000);
        let args = build_video_mux_args(&s, 0, &AudioKeep::ChannelsExcept(vec![(0, 9)]), &probe);
        assert!(!args.contains(&"-filter_complex".to_string()));
        assert_eq!(map_operand(&args, 1), "0:0");
    }

    #[test]
    fn test_build_video_mux_channels_except_mixed_streams() {
        let s = make_video_settings();
        // Two streams: stream 0 loses channel 0 (filter), stream 1 untouched
        // (direct map). Labels appear in stream order.
        let probe = VideoAudioProbe {
            streams: vec![
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 0,
                    channels: 2,
                    codec_name: "pcm_s24le".to_string(),
                    sample_rate: 48000,
                },
                crate::ffprobe::AudioStreamInfo {
                    stream_index: 1,
                    channels: 2,
                    codec_name: "pcm_s24le".to_string(),
                    sample_rate: 48000,
                },
            ],
            total_audio_channels: 4,
            is_video_file: true,
        };
        let args = build_video_mux_args(&s, 0, &AudioKeep::ChannelsExcept(vec![(0, 0)]), &probe);
        assert!(args.contains(&"[0:0]pan=mono|FC=c1[a0]".to_string()));
        assert_eq!(map_operand(&args, 1), "[a0]", "filtered stream first");
        assert_eq!(
            map_operand(&args, 2),
            "0:1",
            "untouched stream mapped directly"
        );
    }

    // ── build_video_mux_args: Reordered arm ───────────────────────────

    #[test]
    fn test_build_video_mux_reordered_pan_and_amerge() {
        let s = make_video_settings();
        let probe = make_probe(0, 2, 48000);
        let args = build_video_mux_args(&s, 0, &AudioKeep::Reordered(vec![(0, 1), (0, 0)]), &probe);
        assert!(args.contains(
            &"[0:0]pan=mono|FC=c1[a0];[0:0]pan=mono|FC=c0[a1];[a0][a1]amerge=inputs=2[out]"
                .to_string()
        ));
        assert_eq!(map_operand(&args, 1), "[out]");
        assert!(args.contains(&"pcm_s24le".to_string()));
    }

    #[test]
    fn test_build_video_mux_reordered_four_inputs() {
        let s = make_video_settings();
        let probe = make_probe(0, 4, 48000);
        let args = build_video_mux_args(
            &s,
            0,
            &AudioKeep::Reordered(vec![(0, 3), (0, 2), (0, 1), (0, 0)]),
            &probe,
        );
        let joined = args.join(" ");
        assert!(
            joined.contains("amerge=inputs=4"),
            "one merged output per pair"
        );
        assert!(joined.contains("FC=c3"));
        assert!(joined.contains("FC=c0"));
        assert_eq!(map_operand(&args, 1), "[out]");
    }

    #[test]
    fn test_build_video_track_extract_args_wav() {
        let s = make_video_settings();
        let args = build_video_track_extract_args(&s, 0, 1, 0, "wav", 48000);
        assert!(args.contains(&"-c:a".to_string()));
        assert!(args.contains(&"pcm_s24le".to_string()));
    }

    #[test]
    fn test_build_video_track_extract_args_aac() {
        let s = make_video_settings();
        let args = build_video_track_extract_args(&s, 0, 1, 0, "adts", 48000);
        assert!(args.contains(&"aac".to_string()));
    }

    #[test]
    fn test_build_concat_audio_args_basic() {
        let s = make_video_settings();
        let segments = vec![(0, 1, 0), (0, 1, 1)];
        let args = build_concat_audio_args(&s, &segments, "wav", 48000);
        let args_str = args.join(" ");
        assert!(args_str.contains("concat=n=2"));
        assert!(args_str.contains("pan=mono"));
    }

    #[test]
    fn test_build_video_to_video_args_dispatch_video_only() {
        let step = VideoOutputStep::VideoOnly {
            file_idx: 0,
            output: Path::new("/tmp/out.mkv").to_path_buf(),
            naming_index: 1,
        };
        let s = make_video_settings();
        let probe = make_probe(1, 2, 48000);
        let args = build_video_to_video_args(&s, &step, &probe);
        assert!(args.contains(&"-an".to_string()));
    }

    // ── push_metadata_args ────────────────────────────────────────────

    fn make_camera_settings() -> ConverterSettings {
        let mut s = make_settings_audio_only();
        s.embed_camera_metadata = true;
        s.camera_meta_per_file = vec![
            Some(CameraInfo {
                make: Some("Sony".to_string()),
                model: Some("NEX-FS100EK".to_string()),
                source: CameraMetaSource::ExifTool,
                creation_date: Some("2024-01-01".to_string()),
                lens: None,
                serial: None,
                creation_time: None,
                gamma: None,
                native_timecode: None,
                exposure_summary: None,
            }),
            None,
        ];
        s
    }

    fn make_extended_camera_settings() -> ConverterSettings {
        let mut s = make_settings_audio_only();
        s.embed_camera_metadata = true;
        s.camera_meta_per_file = vec![
            Some(CameraInfo {
                make: Some("Sony".to_string()),
                model: Some("ILCE-6700".to_string()),
                source: CameraMetaSource::ExifTool,
                creation_date: Some("2026-09-25".to_string()),
                lens: Some("E PZ 18-105mm F4 G OSS".to_string()),
                serial: None,
                creation_time: Some("2026-09-25T20:45:22+01:00".to_string()),
                gamma: Some("ex-cine4".to_string()),
                native_timecode: Some("08:09:59:15".to_string()),
                exposure_summary: Some("Manual exp".to_string()),
            }),
            None,
        ];
        s
    }

    #[test]
    fn test_push_metadata_disabled() {
        let mut s = make_camera_settings();
        s.embed_camera_metadata = false;
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "wav");
        assert!(args.is_empty(), "no args when disabled");
    }

    #[test]
    fn test_push_metadata_wav_with_camera() {
        let s = make_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "wav");
        let joined = args.join(" ");
        assert!(joined.contains("-write_bext 1"));
        assert!(joined.contains("originator=Sony NEX-FS100EK"));
        assert!(joined.contains("origination_date=2024-01-01"));
    }

    #[test]
    fn test_push_metadata_wav_without_camera_default_originator() {
        let s = make_settings_audio_only();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "wav");
        let joined = args.join(" ");
        assert!(joined.contains("-write_bext 1"));
        assert!(joined.contains("originator=LTC Timecode Generator"));
    }

    #[test]
    fn test_push_metadata_mov() {
        let s = make_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "mov");
        let joined = args.join(" ");
        assert!(joined.contains("com.apple.quicktime.make=Sony"));
        assert!(joined.contains("com.apple.quicktime.model=NEX-FS100EK"));
    }

    #[test]
    fn test_push_metadata_mkv_make_model() {
        let s = make_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "matroska");
        let joined = args.join(" ");
        assert!(joined.contains("make=Sony"));
        assert!(joined.contains("model=NEX-FS100EK"));
        assert!(!joined.contains("com.apple.quicktime"));
    }

    #[test]
    fn test_push_metadata_model_only() {
        let mut s = make_camera_settings();
        s.camera_meta_per_file = vec![Some(CameraInfo {
            make: None,
            model: Some("GH6".to_string()),
            source: CameraMetaSource::FfprobeTags,
            creation_date: None,
            lens: None,
            serial: None,
            creation_time: None,
            gamma: None,
            native_timecode: None,
            exposure_summary: None,
        })];
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "matroska");
        let joined = args.join(" ");
        assert!(!joined.contains("make="));
        assert!(joined.contains("model=GH6"));
    }

    #[test]
    fn test_push_metadata_with_out_of_bounds_idx_falls_back_to_first() {
        let s = make_camera_settings();
        // Index out of bounds (only 2 entries) → falls back to first entry
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(99), "wav");
        let joined = args.join(" ");
        assert!(joined.contains("originator=Sony NEX-FS100EK"));
    }

    #[test]
    fn test_push_metadata_idx_returns_none_when_entry_is_none() {
        let s = make_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(1), "wav");
        let joined = args.join(" ");
        assert!(joined.contains("-write_bext 1"));
        assert!(joined.contains("originator=LTC Timecode Generator"));
    }

    // ── Extended metadata tests ─────────────────────────────────────

    fn has_movflags(joined: &str) -> bool {
        joined.contains("-movflags use_metadata_tags")
    }

    #[test]
    fn test_push_metadata_wav_extended_description() {
        let s = make_extended_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "wav");
        let joined = args.join(" ");
        assert!(joined.contains("-write_bext 1"));
        assert!(joined.contains("originator=Sony ILCE-6700"));
        assert!(joined.contains("description=ILCE-6700 / E PZ 18-105mm F4 G OSS"));
    }

    #[test]
    fn test_push_metadata_mov_extended_has_movflags_and_extended_keys() {
        let s = make_extended_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "mov");
        let joined = args.join(" ");
        assert!(joined.contains("com.apple.quicktime.make=Sony"));
        assert!(joined.contains("com.apple.quicktime.model=ILCE-6700"));
        assert!(
            has_movflags(&joined),
            "mov output with extended keys needs -movflags use_metadata_tags"
        );
        assert!(joined.contains("lens=E PZ 18-105mm F4 G OSS"));
        assert!(joined.contains("creation_time=2026-09-25T20:45:22+01:00"));
        assert!(joined.contains("gamma=ex-cine4"));
        assert!(joined.contains("comment=Manual exp"));
        assert!(!joined.contains("serial="));
    }

    #[test]
    fn test_push_metadata_mkv_extended_keys() {
        let s = make_extended_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "matroska");
        let joined = args.join(" ");
        assert!(joined.contains("make=Sony"));
        assert!(joined.contains("model=ILCE-6700"));
        assert!(!has_movflags(&joined));
        assert!(joined.contains("lens=E PZ 18-105mm F4 G OSS"));
        assert!(joined.contains("creation_time=2026-09-25T20:45:22+01:00"));
        assert!(joined.contains("gamma=ex-cine4"));
        assert!(joined.contains("comment=Manual exp"));
    }

    #[test]
    fn test_push_metadata_mov_no_extended_keys_no_movflags() {
        // CameraInfo with only make/model (no extended fields) → no -movflags
        let s = make_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "mov");
        let joined = args.join(" ");
        assert!(joined.contains("com.apple.quicktime.make=Sony"));
        assert!(joined.contains("com.apple.quicktime.model=NEX-FS100EK"));
        assert!(
            !has_movflags(&joined),
            "no -movflags when only make/model are present"
        );
    }

    #[test]
    fn test_push_metadata_mp4_extended_has_movflags() {
        let s = make_extended_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "mp4");
        let joined = args.join(" ");
        assert!(joined.contains("com.apple.quicktime.make=Sony"));
        assert!(has_movflags(&joined));
    }

    #[test]
    fn test_push_metadata_push_camera_metadata_empty() {
        let mut args = vec![];
        push_camera_metadata(&mut args, None, true, "mov");
        assert!(args.is_empty(), "no args when camera is None");
    }

    #[test]
    fn test_push_metadata_wav_no_description_when_only_make_model() {
        let s = make_camera_settings();
        let mut args = vec![];
        push_metadata_args(&mut args, &s, Some(0), "wav");
        let joined = args.join(" ");
        // No lens → no description tag expected
        assert!(
            !joined.contains("description="),
            "no description when camera has no lens/model info beyond originator"
        );
    }

    // ── push_timecode_args / push_audio_timecode_args (WP-B) ─────────────

    fn tc_meta(fps: f64) -> crate::converter::TimecodeMetadata {
        crate::converter::TimecodeMetadata {
            start: Timecode {
                hours: 9,
                minutes: 30,
                seconds: 1,
                frames: 12,
            },
            fps,
            drop_frame: false,
        }
    }

    /// ffmpeg arg vectors are legitimate string contracts (AGENTS.md).
    #[test]
    fn push_timecode_args_force_frame_rate_appends_r() {
        let mut forced = vec![];
        push_timecode_args(&mut forced, &tc_meta(25.0), true);
        assert!(forced.windows(2).any(|w| w[0] == "-timecode"));
        assert!(forced
            .windows(2)
            .any(|w| w[0] == "-write_tmcd" && w[1] == "1"));
        assert_eq!(
            forced
                .windows(2)
                .find(|w| w[0] == "-r")
                .map(|w| w[1].as_str()),
            Some("25.000"),
            "force_frame_rate must pin -r to the metadata fps"
        );

        let mut plain = vec![];
        push_timecode_args(&mut plain, &tc_meta(25.0), false);
        assert!(
            !plain.iter().any(|a| a == "-r"),
            "without force, -r must not be emitted"
        );
    }

    #[test]
    fn push_audio_timecode_args_wav_gets_bext_and_time_reference() {
        // 1 h 30 m 1 s 12 f at 25 fps → 30*3600 + 1*60 + 1 = 108061 s.
        let mut args = vec![];
        push_audio_timecode_args(&mut args, &tc_meta(25.0), "wav", 48000);
        assert!(args
            .windows(2)
            .any(|w| w[0] == "-write_bext" && w[1] == "1"));
        assert_eq!(
            args.windows(2)
                .find(|w| w[0] == "-metadata")
                .map(|w| w[1].as_str()),
            Some("time_reference=1641671040"),
            "time_reference must be start seconds × sample rate (34201.48 × 48000)"
        );
        assert!(args.windows(2).any(|w| w[0] == "-timecode"));

        // Non-WAV formats carry -timecode but no WAV bext block.
        let mut args = vec![];
        push_audio_timecode_args(&mut args, &tc_meta(25.0), "adts", 48000);
        assert!(!args.iter().any(|a| a == "-write_bext"));
        assert!(!args.iter().any(|a| a.starts_with("time_reference=")));
        assert!(args.windows(2).any(|w| w[0] == "-timecode"));
    }

    // ── build_video_to_video_args dispatch arms ──────────────────────────

    #[test]
    fn build_video_to_video_args_dispatches_audio_channel_with_probe_sample_rate() {
        let s = make_video_settings();
        // Probe reports stream 1 at 44100 — the dispatch must pick it up.
        let probe = VideoAudioProbe {
            streams: vec![crate::ffprobe::AudioStreamInfo {
                stream_index: 1,
                channels: 2,
                codec_name: "pcm_s16le".into(),
                sample_rate: 44100,
            }],
            total_audio_channels: 2,
            is_video_file: true,
        };
        let step = VideoOutputStep::AudioChannel {
            file_idx: 0,
            stream_idx: 1,
            channel_idx: 0,
            output: std::path::PathBuf::from("/tmp/out.wav"),
            format: "wav".to_string(),
            naming_index: 0,
        };
        let args = build_video_to_video_args(&s, &step, &probe);
        let joined = args.join(" ");
        assert!(joined.contains("pan=mono|FC=c0"));
        assert!(joined.contains("-c:a pcm_s24le"), "wav format forces PCM");
    }

    #[test]
    #[should_panic]
    fn build_video_to_video_args_panics_on_concat_step() {
        let s = make_video_settings();
        let probe = make_stereo_probe();
        let step = VideoOutputStep::AudioChannelConcat {
            segments: vec![(0, 1, 0)],
            output: std::path::PathBuf::from("/tmp/out.wav"),
            format: "wav".to_string(),
            sample_rate: 48000,
        };
        let _ = build_video_to_video_args(&s, &step, &probe);
    }

    // ── layout_for_count (smallest layout table) ─────────────────────────

    #[test]
    fn layout_for_count_table() {
        assert_eq!(layout_for_count(1), "mono");
        assert_eq!(layout_for_count(2), "stereo");
        assert_eq!(layout_for_count(3), "5.1");
        assert_eq!(layout_for_count(6), "5.1");
        assert_eq!(layout_for_count(8), "5.1");
        // Reflected through pan_filter for a 3-survivor routing.
        let f = pan_filter(&[0, 2, 4]);
        assert!(f.starts_with("pan=5.1|"), "got {}", f);
        assert!(f.contains("c0=0") && f.contains("c1=2") && f.contains("c2=4"));
    }
}
