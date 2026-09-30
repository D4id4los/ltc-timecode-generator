use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::thread::JoinHandle;

use log::{info, warn};

use crate::converter::args::{
    build_audio_to_audio_args, build_audio_to_synthetic_video_args,
    build_concat_audio_args, build_split_track_args, build_video_track_extract_args,
    build_video_to_video_args,
};
use crate::converter::capabilities::{HwDeviceContext, ResolvedHwDevice};
use crate::converter::formats::{
    audio_encoder_to_output_format, container_to_ffmpeg_format, extension_for_container,
};
use crate::converter::planning::{
    plan_concat_outputs, plan_video_outputs_for_file, VideoOutputStep,
};
use crate::converter::process::{
    mark_conversion_failed, run_ffmpeg_process, StepFailure,
};
use crate::converter::progress::{CancelFlag, ConversionStatus, SharedConversionState};
use crate::converter::settings::{ConversionPipeline, ConverterSettings, RecordingType};
use crate::ffprobe::VideoAudioProbe;
use crate::job::{JobContext, JobError, JobFinal};
use crate::naming;
use crate::video_codecs;

pub struct EncoderFallback {
    chain: Vec<String>,
    failed: BTreeSet<String>,
    resolved: Option<String>,
    hw_ctx: HwDeviceContext,
}

impl EncoderFallback {
    pub fn new_with_hw(chain: Vec<String>, hw_ctx: HwDeviceContext) -> Self {
        EncoderFallback {
            chain,
            failed: BTreeSet::new(),
            resolved: None,
            hw_ctx,
        }
    }

    fn remaining(&self) -> Vec<String> {
        if let Some(ref resolved) = self.resolved {
            return vec![resolved.clone()];
        }
        self.chain
            .iter()
            .filter(|e| !self.failed.contains(*e))
            .cloned()
            .collect()
    }

    fn note_success(&mut self, encoder: &str) {
        self.resolved = Some(encoder.to_string());
    }

    fn note_failure(&mut self, encoder: &str) {
        self.failed.insert(encoder.to_string());
    }

    pub fn resolved(&self) -> Option<&str> {
        self.resolved.as_deref()
    }
}

fn resolve_device_for_candidate(encoder: &str, ctx: &HwDeviceContext) -> Option<ResolvedHwDevice> {
    match video_codecs::hw_frames_for(encoder) {
        Some(video_codecs::HwFramePath::Vaapi) => {
            ctx.vaapi_device.clone().map(|path| ResolvedHwDevice::Vaapi { device_path: path })
        }
        Some(video_codecs::HwFramePath::Vulkan) if ctx.vulkan_available => {
            Some(ResolvedHwDevice::Vulkan)
        }
        _ => None,
    }
}

fn run_video_step_with_fallback(
    settings: &mut ConverterSettings,
    fallback: &mut EncoderFallback,
    build_args: &mut dyn FnMut(&ConverterSettings) -> Vec<String>,
    output: &Path,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    step_progress_weight: f32,
    overall_progress: &mut f32,
    overall_log: &mut String,
    total_steps: usize,
    current_step: usize,
) -> bool {
    let candidates = fallback.remaining();
    if candidates.is_empty() {
        let msg = "no video encoder candidate available".to_string();
        warn!("{}", msg);
        overall_log.push_str(&format!("\n\n--- {} ---", msg));
        mark_conversion_failed(state, overall_log);
        return false;
    }

    let mut attempt = 0;
    while attempt < candidates.len() {
        let encoder = candidates[attempt].clone();
        if cancel.load(Ordering::Relaxed) {
            return false;
        }
        settings.resolved_video_encoder = encoder.clone();

        let hw_ctx = &fallback.hw_ctx;
        settings.resolved_hw_device = resolve_device_for_candidate(&encoder, hw_ctx);

        if video_codecs::hw_frames_for(&encoder).is_some() && settings.resolved_hw_device.is_none() {
            let msg = format!(
                "--- no hardware device available for '{}'; skipping ---",
                encoder
            );
            warn!("{}", msg);
            overall_log.push_str(&format!("\n--- {} ---\n", msg));
            fallback.note_failure(&encoder);
            attempt += 1;
            continue;
        }

        let args = build_args(settings);
        match run_ffmpeg_process(
            &args,
            output,
            state,
            cancel,
            step_progress_weight,
            overall_progress,
            overall_log,
            total_steps,
            current_step,
        ) {
            Ok(()) => {
                fallback.note_success(&encoder);
                return true;
            }
            Err(StepFailure::Fatal(_)) => {
                mark_conversion_failed(state, overall_log);
                return false;
            }
            Err(StepFailure::EncoderInit(_)) => {
                fallback.note_failure(&encoder);
                attempt += 1;
                if attempt < candidates.len() {
                    let msg = format!(
                        "--- encoder '{}' failed to initialize; falling back to '{}' ---",
                        encoder, candidates[attempt]
                    );
                    warn!("{} (output: {})", msg, output.display());
                    overall_log.push_str(&format!("\n--- {} ---\n", msg));
                }
            }
        }
    }

    let msg = format!(
        "all encoder candidates for codec '{}' failed to initialize ({})",
        video_codecs::normalize_video_codec(&settings.video_encoder),
        fallback.failed.iter().cloned().collect::<Vec<_>>().join(", ")
    );
    warn!("{}", msg);
    overall_log.push_str(&format!("\n\n--- {} ---", msg));
    mark_conversion_failed(state, overall_log);
    false
}

fn prepare_copy_mode(settings: &mut ConverterSettings) {
    if let Some(first) = settings.input_files.first() {
        let container = crate::converter::formats::copy_mode_container_for_input(first);
        let mixed = settings
            .input_files
            .iter()
            .any(|f| crate::converter::formats::copy_mode_container_for_input(f) != container);
        if mixed {
            warn!(
                "Mixed input containers in copy mode; using '{}' for all outputs",
                container
            );
        }
        settings.container = container.to_string();
    }

    for i in 0..settings.trim_offsets_secs.len() {
        let raw = settings.trim_offsets_secs[i];
        if raw <= 0.001 {
            continue;
        }
        let Some(path) = settings.input_files.get(i) else {
            continue;
        };
        let snapped = crate::ffprobe::snap_trim_to_keyframe(path, raw);
        let delta = raw - snapped;
        if delta <= 0.001 {
            continue;
        }
        info!(
            "Copy mode: trim for '{}' snapped {:.3}s → {:.3}s (keyframe)",
            path.display(),
            raw,
            snapped
        );
        settings.trim_offsets_secs[i] = snapped;
        if let Some(Some(meta)) = settings.timecode_meta_per_file.get_mut(i) {
            meta.start = crate::converter::timecode::shift_timecode_back(
                &meta.start, delta, meta.fps, meta.drop_frame,
            );
        }
    }
}

pub fn spawn_conversion(
    settings: ConverterSettings,
    state: SharedConversionState,
    cancel: CancelFlag,
    caps: Option<&crate::converter::capabilities::FfmpegCapabilities>,
) -> JoinHandle<()> {
    let no_encoder_needed = matches!(settings.pipeline, ConversionPipeline::MetadataOnly)
        || (settings.copy_video
            && matches!(settings.pipeline, ConversionPipeline::VideoPassthrough));
    let codec_id = video_codecs::normalize_video_codec(&settings.video_encoder).to_string();
    let mut chain: Vec<String> = if no_encoder_needed {
        Vec::new()
    } else {
        caps.filter(|c| c.has_ffmpeg)
            .map(|c| video_codecs::resolve_encoder_chain(&codec_id, c))
            .unwrap_or_default()
    };
    if chain.is_empty() && !no_encoder_needed {
        chain = video_codecs::static_encoder_chain(&codec_id);
    }

    let hw_ctx = caps
        .filter(|c| c.has_ffmpeg)
        .map(|c| HwDeviceContext {
            vaapi_device: c.hw.vaapi_device.clone(),
            vulkan_available: c.hw.vulkan_available,
        })
        .unwrap_or_default();

    std::thread::spawn(move || {
        let mut settings = settings;
        let copy_mode = settings.copy_video
            && matches!(settings.pipeline, ConversionPipeline::VideoPassthrough);
        let metadata_only = matches!(settings.pipeline, ConversionPipeline::MetadataOnly);
        if copy_mode {
            prepare_copy_mode(&mut settings);
        } else if metadata_only {
            settings.trim_offsets_secs =
                vec![0.0; settings.trim_offsets_secs.len()];
        }
        let mut fallback = EncoderFallback::new_with_hw(chain, hw_ctx);
        if copy_mode {
            info!(
                "Video stream copy mode: video will not be re-encoded \
                 (container '{}', video codec selection ignored)",
                settings.container
            );
        } else {
            info!(
                "Encoder chain for codec '{}': {}",
                codec_id,
                fallback.remaining().join(" → ")
            );
        }

        let (output_format, output_extension) = match settings.pipeline {
            ConversionPipeline::AudioOnly { generate_synthetic_video: false } => {
                let (fmt, ext) = audio_encoder_to_output_format(&settings.audio_encoder);
                (fmt.to_string(), ext.to_string())
            }
            ConversionPipeline::MetadataOnly => {
                let (fmt, ext) = audio_encoder_to_output_format(&settings.audio_encoder);
                (fmt.to_string(), ext.to_string())
            }
            _ => {
                let ext = extension_for_container(&settings.container);
                (container_to_ffmpeg_format(&settings.container).to_string(), ext.to_string())
            }
        };
        let extension = &output_extension;
        let mut total_steps = match settings.pipeline {
            ConversionPipeline::VideoPassthrough => settings.input_files.len(),
            ConversionPipeline::MetadataOnly => {
                let per_file_steps = if settings.recording_type == RecordingType::VideoClipSequence {
                    2 + if settings.split_tracks { settings.channel_map.num_channels().max(1) } else { 1 }
                } else {
                    2
                };
                settings.input_files.len() * per_file_steps
            }
            _ => if settings.split_tracks { settings.channel_map.num_channels() } else { 1 },
        };
        let mut overall_progress: f32 = 0.0;
        let mut overall_log = String::new();
        let input_count = settings.input_files.len();

        info!(
            "Starting conversion: {} input(s), pipeline={:?}, split={}, drop_ltc={}, video codec={}, audio encoder={}",
            input_count,
            settings.pipeline,
            settings.split_tracks,
            settings.drop_ltc_track,
            settings.video_encoder,
            settings.audio_encoder,
        );

        {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Running { progress: 0.0 };
            s.ffmpeg_output = format!("Pipeline: {:?}, {} steps", settings.pipeline, total_steps);
            s.current_line = String::new();
        }

        if cancel.load(Ordering::Relaxed) {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Failed { error_log: "Canceled before start".into() };
            return;
        }

        match settings.pipeline {
            ConversionPipeline::AudioOnly { generate_synthetic_video: false } => {
                run_audio_to_audio(&settings, &output_format, extension, &state, &cancel, total_steps, &mut overall_progress, &mut overall_log);
            }
            ConversionPipeline::AudioOnly { generate_synthetic_video: true } => {
                run_audio_to_synthetic_video(&mut settings, extension, &mut fallback, &state, &cancel, &mut overall_progress, &mut overall_log);
            }
            ConversionPipeline::VideoPassthrough => {
                run_video_to_video(&mut settings, extension, &mut fallback, &state, &cancel, &mut total_steps, &mut overall_progress, &mut overall_log);
            }
            ConversionPipeline::MetadataOnly => {
                run_metadata_only(&settings, &state, &cancel, total_steps, &mut overall_progress, &mut overall_log);
            }
        }

        let final_status = {
            let s = state.lock().unwrap();
            s.status.clone()
        };
        if matches!(final_status, ConversionStatus::Failed { .. }) {
            info!("Conversion failed - see log for details.");
        } else {
            let encoder_line = if metadata_only {
                "\nTags written in place, audio extracted as PCM WAV".to_string()
            } else if copy_mode {
                "\nVideo stream: copied (no re-encode)".to_string()
            } else {
                fallback
                    .resolved()
                    .map(|e| format!("\nVideo encoder used: {}", e))
                    .unwrap_or_default()
            };
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Completed;
            s.ffmpeg_output = format!(
                "{}\n\n--- CONVERSION COMPLETED SUCCESSFULLY ---{}",
                overall_log, encoder_line
            );
        }
    })
}

/// Version of the conversion that works with `spawn_job` from the unified
/// job infrastructure. Uses the existing `spawn_conversion` internally and
/// bridges the progress reporting. Returns `JobFinal::Conversion`.
pub fn spawn_conversion_job(
    ctx: &JobContext,
    settings: ConverterSettings,
    caps: Option<crate::converter::capabilities::FfmpegCapabilities>,
) -> Result<JobFinal, JobError> {
    let state: SharedConversionState = std::sync::Arc::new(std::sync::Mutex::new(
        crate::converter::progress::ConversionState::idle(),
    ));
    let cancel: CancelFlag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // Forward cancel from JobContext to internal cancel flag
    let cancel_inner = cancel.clone();
    let ctx_cancel = ctx.cancel.inner().clone();
    std::thread::spawn(move || {
        while !ctx_cancel.load(std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        cancel_inner.store(true, std::sync::atomic::Ordering::Relaxed);
    });

    ctx.progress.set_indeterminate(true);

    // Spawn the actual conversion thread
    let handle = spawn_conversion(settings, state.clone(), cancel, caps.as_ref());

    // Poll for completion, bridging progress to JobContext
    ctx.progress.set_indeterminate(false);
    // Resize to 1 unit for overall progress
    ctx.progress.resize(1);

    while !handle.is_finished() {
        if let Ok(s) = state.lock() {
            match &s.status {
                ConversionStatus::Running { progress } => {
                    ctx.progress.unit(0).set_fraction(*progress);
                    ctx.progress.set_message(format!("Conversion: {:.0}%", progress * 100.0));
                }
                _ => {}
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }

    let _ = handle.join();

    // Read final state
    let (final_status, final_log) = {
        let s = state.lock().unwrap();
        (s.status.clone(), s.ffmpeg_output.clone())
    };

    // Mark unit as done
    ctx.progress.unit(0).finish();
    ctx.progress.set_message("");

    match final_status {
        ConversionStatus::Completed => {
            // Extract encoder info from log
            let encoder_used = if final_log.contains("Video encoder used:") {
                final_log.lines()
                    .find(|l| l.contains("Video encoder used"))
                    .map(|l| l.trim_start_matches("Video encoder used: ").to_string())
            } else if final_log.contains("stream copy") || final_log.contains("stream-copy") {
                Some("stream-copy".to_string())
            } else if final_log.contains("Tags written") {
                None
            } else {
                None
            };
            info!("Conversion job completed successfully");
            Ok(JobFinal::Conversion {
                encoder_used,
                steps_attempted: 0,
            })
        }
        ConversionStatus::Failed { error_log } => {
            if final_log.contains("CANCELLED") || error_log.contains("Cancel") || error_log.contains("cancel") {
                info!("Conversion job cancelled");
                Err(JobError::Cancelled)
            } else {
                let err = if error_log.is_empty() { final_log } else { error_log };
                Err(JobError::Failed(err))
            }
        }
        _ => {
            Err(JobError::Failed("Unexpected conversion state".to_string()))
        }
    }
}

fn run_audio_to_audio(
    settings: &ConverterSettings,
    format: &str,
    extension: &str,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    total_steps: usize,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    let sample_rate = settings
        .input_files
        .first()
        .and_then(|p| crate::converter::timecode::read_wav_sample_rate(p))
        .unwrap_or(48000);

    if settings.split_tracks {
        let mut emitted = 0usize;
        for track_idx in 0..settings.channel_map.num_channels() {
            if settings.is_ltc_output_track(track_idx) {
                continue;
            }
            if cancel.load(Ordering::Relaxed) { break; }

            let output_path = settings.output_path_for_index("audio", track_idx + 1, extension);
            let mapping = settings.channel_map.mapping();
            let input_idx = mapping.iter().position(|&o| o == track_idx).unwrap_or(track_idx);
            let tc = settings
                .timecode_meta_per_file
                .get(input_idx)
                .and_then(|m| m.as_ref());
            let step_args = build_split_track_args(settings, format, track_idx, tc, sample_rate);
            let step_progress = 1.0 / total_steps as f32;
            if run_ffmpeg_process(&step_args, &output_path, state, cancel, step_progress, overall_progress, overall_log, total_steps, 1).is_err() {
                mark_conversion_failed(state, overall_log);
                return;
            }
            emitted += 1;
        }
        if emitted == 0 {
            let msg = format!(
                "No output tracks produced: all {} track(s) were dropped \
                 (split_tracks={}, drop_ltc_track={}, ltc_track_channel_index={}, \
                  channel_map channels={}). Check LTC track selection.",
                settings.channel_map.num_channels(),
                settings.split_tracks,
                settings.drop_ltc_track,
                settings.ltc_track_channel_index,
                settings.channel_map.num_channels(),
            );
            log::warn!("{}", msg);
            overall_log.push_str(&format!("\n\n--- {} ---", msg));
            mark_conversion_failed(state, overall_log);
        }
    } else {
        let tc = settings
            .timecode_meta_per_file
            .first()
            .and_then(|m| m.as_ref());
        let base_args = build_audio_to_audio_args(settings, format, tc, sample_rate);
        let output_path = settings.output_path_for_index("audio", 0, extension);
        if run_ffmpeg_process(&base_args, &output_path, state, cancel, 1.0, overall_progress, overall_log, 1, 1).is_err() {
            mark_conversion_failed(state, overall_log);
        }
    }
}

fn run_audio_to_synthetic_video(
    settings: &mut ConverterSettings,
    extension: &str,
    fallback: &mut EncoderFallback,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    let output_path = settings.output_path_for_index("video", 1, extension);
    let mut build_args =
        |s: &ConverterSettings| build_audio_to_synthetic_video_args(s);
    run_video_step_with_fallback(
        settings,
        fallback,
        &mut build_args,
        &output_path,
        state,
        cancel,
        1.0,
        overall_progress,
        overall_log,
        1,
        1,
    );
}

struct StepEntry {
    step: VideoOutputStep,
    is_audio_only: bool,
}

fn run_video_to_video(
    settings: &mut ConverterSettings,
    _extension: &str,
    fallback: &mut EncoderFallback,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    _total_steps: &mut usize,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    let mut probes: Vec<Option<VideoAudioProbe>> = Vec::new();

    for file_idx in 0..settings.input_files.len() {
        if cancel.load(Ordering::Relaxed) { break; }
        let input = &settings.input_files[file_idx];
        match crate::ffprobe::probe_video_audio(input) {
            Ok(probe) => {
                probes.push(Some(probe));
            }
            Err(e) => {
                warn!("Probe failed for '{}': {} — treating as no-audio", input.display(), e);
                probes.push(None);
            }
        }
    }

    let mut steps: Vec<StepEntry> = Vec::new();

    let use_concat = settings.concat_audio
        && settings.split_tracks
        && settings.recording_type == RecordingType::VideoClipSequence;

    if use_concat {
        let ext = extension_for_container(&settings.container);
        for file_idx in 0..settings.input_files.len() {
            if cancel.load(Ordering::Relaxed) { break; }
            let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
            steps.push(StepEntry {
                step: VideoOutputStep::VideoOnly { file_idx, output: video_out },
                is_audio_only: false,
            });
        }

        let (concat_steps, warning) = plan_concat_outputs(settings, &probes);
        if !warning.is_empty() {
            warn!("{}", warning.trim());
            overall_log.push_str(&format!("\n--- {}\n", warning.trim()));
        }
        for cs in concat_steps {
            steps.push(StepEntry { step: cs, is_audio_only: true });
        }
    } else {
        for (file_idx, probe_opt) in probes.iter().enumerate() {
            let probe = match probe_opt {
                Some(p) => p.clone(),
                None => VideoAudioProbe {
                    streams: Vec::new(),
                    total_audio_channels: 0,
                    is_video_file: true,
                },
            };
            for s in plan_video_outputs_for_file(settings, file_idx, &probe) {
                steps.push(StepEntry { step: s, is_audio_only: false });
            }
        }
    }

    *_total_steps = steps.len();

    for (step_idx, entry) in steps.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) { break; }

        if entry.is_audio_only {
            if let VideoOutputStep::AudioChannelConcat { segments, output, format, sample_rate } = &entry.step {
                let step_progress = 1.0 / steps.len().max(1) as f32;
                if run_ffmpeg_process(
                    &build_concat_audio_args(settings, segments, format, *sample_rate),
                    output, state, cancel, step_progress, overall_progress, overall_log,
                    steps.len(), step_idx + 1,
                ).is_err() {
                    mark_conversion_failed(state, overall_log);
                    break;
                }
            }
        } else {
            let output = entry.step.output().to_path_buf();
            let step_progress = 1.0 / steps.len().max(1) as f32;
            let probe = probes.first().cloned().flatten().unwrap_or(VideoAudioProbe {
                streams: Vec::new(),
                total_audio_channels: 0,
                is_video_file: true,
            });
            let mut build_args =
                |s: &ConverterSettings| build_video_to_video_args(s, &entry.step, &probe);

            let ok = if settings.copy_video {
                match run_ffmpeg_process(
                    &build_args(settings),
                    &output,
                    state,
                    cancel,
                    step_progress,
                    overall_progress,
                    overall_log,
                    steps.len(),
                    step_idx + 1,
                ) {
                    Ok(()) => true,
                    Err(_) => false,
                }
            } else {
                run_video_step_with_fallback(
                    settings,
                    fallback,
                    &mut build_args,
                    &output,
                    state,
                    cancel,
                    step_progress,
                    overall_progress,
                    overall_log,
                    steps.len(),
                    step_idx + 1,
                )
            };
            if !ok {
                if settings.copy_video {
                    mark_conversion_failed(state, overall_log);
                }
                break;
            }
        }
    }
}

fn rename_target_in_source_dir(settings: &ConverterSettings, file_idx: usize, ext: &str) -> PathBuf {
    let naming_ctx = naming::NamingContext {
        filename: settings
            .input_files
            .get(file_idx)
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string(),
        device: settings.device_name.clone().unwrap_or_else(|| "unknown".into()),
        clip: file_idx + 1,
        track: (file_idx + 1).max(1),
    };

    let prefix_expanded = naming::NameTemplate::parse(&settings.filename_prefix)
        .unwrap_or_else(|_| naming::NameTemplate::parse("{filename}").unwrap())
        .expand(&naming_ctx);

    let suffix_expanded = if settings.video_suffix_template.is_empty() {
        String::new()
    } else {
        naming::NameTemplate::parse(&settings.video_suffix_template)
            .unwrap_or_else(|_| naming::NameTemplate::parse("").unwrap())
            .expand(&naming_ctx)
    };

    let new_filename = format!("{}{}.{}", prefix_expanded, suffix_expanded, ext);

    let source_dir = settings
        .input_files
        .get(file_idx)
        .and_then(|p| p.parent())
        .unwrap_or(Path::new("."));
    source_dir.join(&new_filename)
}

fn run_metadata_only(
    settings: &ConverterSettings,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    total_steps: usize,
    overall_progress: &mut f32,
    overall_log: &mut String,
) {
    let input_files = &settings.input_files;
    let (fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
    let is_video = settings.recording_type == RecordingType::VideoClipSequence;
    let use_concat = is_video && settings.concat_audio && settings.split_tracks;

    // Phase A: probe all files upfront so plan_concat_outputs can see every clip
    let mut probes: Vec<Option<crate::ffprobe::VideoAudioProbe>> =
        Vec::with_capacity(input_files.len());
    for input_path in input_files.iter() {
        if cancel.load(Ordering::Relaxed) {
            overall_log.push_str("\n--- CANCELLED ---\n");
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Failed {
                error_log: "Cancelled by user".into(),
            };
            return;
        }

        let probed: Option<crate::ffprobe::VideoAudioProbe> = if is_video {
            match crate::ffprobe::probe_video_audio(input_path) {
                Ok(p) => Some(p),
                Err(e) => {
                    let msg = format!(
                        "✗ {} — probe failed: {} (skipping audio extraction)\n",
                        input_path.display(),
                        e
                    );
                    log::warn!("{}", msg.trim());
                    overall_log.push_str(&msg);
                    None
                }
            }
        } else {
            None
        };
        probes.push(probed);
    }

    // Phase B: audio extraction
    let total_actual;
    if use_concat {
        let (concat_steps, warning) = plan_concat_outputs(settings, &probes);
        if !warning.is_empty() {
            let w = warning.trim().to_string();
            warn!("{}", w);
            overall_log.push_str(&format!("\n--- {}\n", w));
        }
        let extraction_count = concat_steps.len();
        total_actual = extraction_count + input_files.len();
        let step_weight = if total_actual > 0 {
            1.0 / total_actual as f32
        } else {
            0.0
        };

        for (step_idx, step) in concat_steps.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                overall_log.push_str("\n--- CANCELLED ---\n");
                let mut s = state.lock().unwrap();
                s.status = ConversionStatus::Failed {
                    error_log: "Cancelled by user".into(),
                };
                return;
            }
            match step {
                VideoOutputStep::AudioChannelConcat {
                    segments,
                    output,
                    format,
                    sample_rate,
                } => {
                    if run_ffmpeg_process(
                        &build_concat_audio_args(settings, segments, format, *sample_rate),
                        output,
                        state,
                        cancel,
                        step_weight,
                        overall_progress,
                        overall_log,
                        total_actual,
                        step_idx + 1,
                    )
                    .is_err()
                    {
                        let msg = format!(
                            "✗ audio concatenation step {} failed\n",
                            step_idx + 1
                        );
                        overall_log.push_str(&msg);
                    }
                }
                VideoOutputStep::AudioChannel {
                    file_idx,
                    stream_idx,
                    channel_idx,
                    output,
                    format,
                } => {
                    let sr = probes[*file_idx]
                        .as_ref()
                        .and_then(|p| {
                            p.streams.iter().find(|s| s.stream_index == *stream_idx)
                        })
                        .map(|s| s.sample_rate)
                        .unwrap_or(48000);
                    if run_ffmpeg_process(
                        &build_video_track_extract_args(
                            settings, *file_idx, *stream_idx, *channel_idx, format, sr,
                        ),
                        output,
                        state,
                        cancel,
                        step_weight,
                        overall_progress,
                        overall_log,
                        total_actual,
                        step_idx + 1,
                    )
                    .is_err()
                    {
                        let msg = format!(
                            "✗ {} — audio extraction failed\n",
                            input_files[*file_idx].display()
                        );
                        overall_log.push_str(&msg);
                    }
                }
                _ => {}
            }
        }
    } else {
        total_actual = total_steps;
        for (file_idx, probed) in probes.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                overall_log.push_str("\n--- CANCELLED ---\n");
                let mut s = state.lock().unwrap();
                s.status = ConversionStatus::Failed {
                    error_log: "Cancelled by user".into(),
                };
                return;
            }

            let input_path = &input_files[file_idx];

            if let Some(ref probe) = probed {
                let channels: Vec<(usize, usize)> = probe
                    .streams
                    .iter()
                    .flat_map(|s| (0..s.channels).map(move |ch| (s.stream_index, ch)))
                    .collect();
                let map_n = settings.channel_map.num_channels();
                let use_split = settings.split_tracks && map_n > 0;
                let step_weight = 1.0 / total_steps as f32;

                if use_split {
                    let mut emitted = 0usize;
                    for output_k in 0..map_n {
                        if cancel.load(Ordering::Relaxed) {
                            break;
                        }
                        let Some(input_i) =
                            settings.channel_map.input_for_output(output_k)
                        else {
                            continue;
                        };
                        if input_i >= channels.len() {
                            continue;
                        }
                        let (stream_idx, ch_idx) = channels[input_i];
                        let is_ltc =
                            settings.ltc_video_source == Some((stream_idx, ch_idx));
                        if settings.drop_ltc_track && is_ltc {
                            continue;
                        }
                        emitted += 1;
                        let output_path =
                            settings.output_path_for_file("audio", file_idx, emitted, aext);
                        let sample_rate = probe
                            .streams
                            .iter()
                            .find(|s| s.stream_index == stream_idx)
                            .map(|s| s.sample_rate)
                            .unwrap_or(48000);
                        let args = build_video_track_extract_args(
                            settings, file_idx, stream_idx, ch_idx, fmt, sample_rate,
                        );
                        if run_ffmpeg_process(
                            &args,
                            &output_path,
                            state,
                            cancel,
                            step_weight,
                            overall_progress,
                            overall_log,
                            total_steps,
                            file_idx * 3 + 1,
                        )
                        .is_err()
                        {
                            let msg = format!(
                                "✗ {} — audio extraction failed\n",
                                input_path.display()
                            );
                            overall_log.push_str(&msg);
                        }
                    }
                    if emitted == 0 {
                        *overall_progress += step_weight;
                    }
                } else {
                    let output_path = settings.merged_audio_output_path(aext);
                    let (stream_idx, ch_idx) =
                        channels.first().copied().unwrap_or((0, 0));
                    let sample_rate = probe
                        .streams
                        .first()
                        .map(|s| s.sample_rate)
                        .unwrap_or(48000);
                    let args = build_video_track_extract_args(
                        settings, file_idx, stream_idx, ch_idx, fmt, sample_rate,
                    );
                    if run_ffmpeg_process(
                        &args,
                        &output_path,
                        state,
                        cancel,
                        step_weight,
                        overall_progress,
                        overall_log,
                        total_steps,
                        file_idx * 3 + 1,
                    )
                    .is_err()
                    {
                        let msg = format!(
                            "✗ {} — audio extraction failed\n",
                            input_path.display()
                        );
                        overall_log.push_str(&msg);
                    }
                }
            } else if !is_video {
                let sw = 1.0 / total_steps as f32;
                *overall_progress += sw;
            }
        }
    }

    // Phase C: per-file tagging + rename
    for (file_idx, _probed) in probes.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            overall_log.push_str("\n--- CANCELLED ---\n");
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Failed {
                error_log: "Cancelled by user".into(),
            };
            return;
        }

        let input_path = &input_files[file_idx];
        let tc = settings
            .timecode_meta_per_file
            .get(file_idx)
            .and_then(|m| m.as_ref());

        let camera = settings
            .camera_meta_per_file
            .get(file_idx)
            .and_then(|c| c.as_ref());
        if let Some(tc_meta) = tc {
            match crate::tagger::tag_file(input_path, tc_meta, camera) {
                Ok(outcome) => {
                    let msg = match outcome {
                        crate::tagger::TagOutcome::TaggedInPlace => {
                            format!("✓ {} — tagged in place\n", input_path.display())
                        }
                        crate::tagger::TagOutcome::TaggedViaFfmpeg => {
                            format!("✓ {} — tagged via ffmpeg\n", input_path.display())
                        }
                        crate::tagger::TagOutcome::Skipped { reason } => {
                            format!(
                                "⚠ {} — skipped tagging: {}\n",
                                input_path.display(),
                                reason
                            )
                        }
                    };
                    overall_log.push_str(&msg);
                }
                Err(e) => {
                    let msg = format!(
                        "✗ {} — tagging failed: {}\n",
                        input_path.display(),
                        e
                    );
                    log::error!("{}", msg.trim());
                    overall_log.push_str(&msg);
                }
            }
        } else {
            let msg = format!(
                "⚠ {} — no start timecode available, skipping tagging\n",
                input_path.display()
            );
            log::warn!("{}", msg.trim());
            overall_log.push_str(&msg);
        }

        if !settings.filename_prefix.is_empty() {
            let ext = input_path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("bin");
            let new_path = rename_target_in_source_dir(settings, file_idx, ext);
            if new_path != *input_path {
                if new_path.exists() {
                    let msg = format!(
                        "⚠ {} — rename target '{}' already exists, skipping rename\n",
                        input_path.display(),
                        new_path.display()
                    );
                    log::warn!("{}", msg.trim());
                    overall_log.push_str(&msg);
                } else {
                    match std::fs::rename(input_path, &new_path) {
                        Ok(()) => {
                            let msg = format!(
                                "✓ {} → {}\n",
                                input_path.display(),
                                new_path.display()
                            );
                            overall_log.push_str(&msg);
                        }
                        Err(e) => {
                            let msg = format!(
                                "✗ {} — rename failed: {}\n",
                                input_path.display(),
                                e
                            );
                            log::error!("{}", msg.trim());
                            overall_log.push_str(&msg);
                        }
                    }
                }
            }
        }

        let step_weight = if total_actual > 0 {
            1.0 / total_actual as f32
        } else {
            0.0
        };
        *overall_progress += step_weight;
        {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Running {
                progress: overall_progress.min(1.0),
            };
            s.current_line = format!(
                "[{}/{}] {} — done",
                file_idx + 1,
                input_files.len(),
                input_path.display()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::AtomicBool;
    use crate::converter::test_fixtures::*;
    use crate::{ChannelMap, CancelFlag, ConversionState, ConversionStatus, SharedConversionState};
    use super::*;

    fn fresh_state() -> SharedConversionState {
        Arc::new(Mutex::new(ConversionState::idle()))
    }

    fn fresh_cancel() -> CancelFlag {
        Arc::new(AtomicBool::new(false))
    }

    /// Create a short PCM 16-bit mono WAV file in the given directory.
    fn create_test_wav(dir: &std::path::Path, name: &str, sample_rate: u32, duration_secs: f64) -> std::path::PathBuf {
        let path = dir.join(name);
        let num_samples = (sample_rate as f64 * duration_secs) as u32;
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..num_samples {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn test_run_audio_to_audio_split_all_dropped_fails() {
        let mut settings = make_settings_audio_only();
        settings.split_tracks = true;
        settings.drop_ltc_track = true;
        settings.channel_map = ChannelMap::identity(1);
        settings.ltc_track_channel_index = 0;

        let state = fresh_state();
        let cancel = fresh_cancel();
        let mut progress = 0.0f32;
        let mut log = String::new();
        let total_steps = 1;

        run_audio_to_audio(
            &settings, "wav", "wav", &state, &cancel,
            total_steps, &mut progress, &mut log,
        );

        let s = state.lock().unwrap();
        assert!(
            matches!(&s.status, ConversionStatus::Failed { .. }),
            "expected Failed when all tracks are dropped, got {:?}",
            s.status
        );
        assert!(
            progress == 0.0,
            "progress must remain 0 when no ffmpeg ran, got {}",
            progress
        );
    }

    #[test]
    fn test_run_audio_to_audio_split_one_survives_ok() {
        let dir = tempfile::TempDir::new().unwrap();
        let wav1 = create_test_wav(dir.path(), "ch1.wav", 48000, 0.25);
        let wav2 = create_test_wav(dir.path(), "ch2.wav", 48000, 0.25);

        let mut settings = make_settings_audio_only();
        settings.input_files = vec![wav1, wav2];
        settings.split_tracks = true;
        settings.drop_ltc_track = true;
        settings.channel_map = ChannelMap::identity(2);
        settings.ltc_track_channel_index = 1;

        let state = fresh_state();
        let cancel = fresh_cancel();
        let mut progress = 0.0f32;
        let mut log = String::new();
        let total_steps = 2;

        run_audio_to_audio(
            &settings, "wav", "wav", &state, &cancel,
            total_steps, &mut progress, &mut log,
        );

        let s = state.lock().unwrap();
        assert!(
            matches!(&s.status, ConversionStatus::Running { .. } | ConversionStatus::Completed),
            "expected Running or Completed when one track survives, got {:?}",
            s.status
        );
    }

    #[test]
    fn test_encoder_fallback_remaining_skips_failed() {
        let hw = HwDeviceContext { vaapi_device: None, vulkan_available: false };
        let mut fb = EncoderFallback::new_with_hw(
            vec!["enc_a".into(), "enc_b".into(), "enc_c".into()],
            hw,
        );
        fb.note_failure("enc_a");
        let remaining = fb.remaining();
        assert_eq!(remaining, vec!["enc_b".to_string(), "enc_c".to_string()]);
    }

    #[test]
    fn test_encoder_fallback_pins_resolved_encoder() {
        let hw = HwDeviceContext { vaapi_device: None, vulkan_available: false };
        let mut fb = EncoderFallback::new_with_hw(
            vec!["enc_a".into(), "enc_b".into()],
            hw,
        );
        fb.note_success("enc_a");
        assert_eq!(fb.resolved(), Some("enc_a"));
        assert_eq!(fb.remaining(), vec!["enc_a".to_string()]);
    }

    #[test]
    fn test_encoder_fallback_exhausted_chain() {
        let hw = HwDeviceContext { vaapi_device: None, vulkan_available: false };
        let mut fb = EncoderFallback::new_with_hw(
            vec!["enc_a".into()],
            hw,
        );
        fb.note_failure("enc_a");
        assert!(fb.remaining().is_empty());
    }

    #[test]
    fn test_resolve_device_for_candidate_software_encoder() {
        let ctx = HwDeviceContext { vaapi_device: None, vulkan_available: false };
        assert!(resolve_device_for_candidate("libx264", &ctx).is_none());
        assert!(resolve_device_for_candidate("pcm_s24le", &ctx).is_none());
    }

    #[test]
    fn test_effective_video_encoder_prefers_resolved() {
        let s = make_video_settings();
        assert!(!s.effective_video_encoder().is_empty());
    }
}