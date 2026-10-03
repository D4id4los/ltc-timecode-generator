use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

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
    plan_concat_outputs, plan_video_outputs_for_file, selected_channel_pairs, VideoOutputStep,
};
use crate::converter::process::{
    run_ffmpeg_process, StepFailure,
};
use crate::converter::settings::{ConversionPipeline, ConverterSettings, RecordingType};
use crate::ffprobe::VideoAudioProbe;
use crate::job::{JobContext, JobError, JobFinal, UnitProgress};
use crate::naming;
use crate::video_codecs;

/// Reporting interface for conversion pipelines, abstracting over
/// production (JobContext-based) and test (in-memory) progress tracking.
pub trait ConversionReport: Send + Sync {
    fn is_cancelled(&self) -> bool;
    fn step_weight(&self) -> f32;
    fn set_step_weight(&self, w: f32);
    /// In-flight progress of the current step as a fraction of that step
    /// (`0.0..=1.0`). The step runner never sends `1.0` — ffmpeg reports
    /// `progress=end` / duration-reached `out_time` while still finalising
    /// the output, so in-flight fractions are capped below full. Step
    /// completion is signalled exclusively by [`Self::advance_step`].
    fn report_step_fraction(&self, step_fraction: f32, line: &str);
    fn advance_step(&self);
    fn set_log(&self, text: &str);
    fn append_log(&self, text: &str);
    fn set_message(&self, msg: &str);
    fn mark_failed(&self, log: &str);
    fn mark_completed(&self, summary: &str);
    /// Returns `true` if `mark_failed` was called at any point.
    fn is_failed(&self) -> bool;
    fn set_unit_count(&self, n: usize);
    fn unit(&self, idx: usize) -> Option<UnitProgress>;
    /// Called by `summarize_metadata_failures` with the recorded step
    /// failures, so tests can assert them structurally instead of parsing
    /// the `--- N STEP(S) FAILED ---` wording.
    fn set_failures(&self, _failures: &[StepFailureRecord]) {}
    /// Called by `check_cancelled` when the pipeline observes a cancellation
    /// (in addition to the human-readable CANCELLED log block). Tests use
    /// this to assert cancellation without matching log text.
    fn note_cancelled(&self) {}
    /// Returns an optional `&AtomicBool` for the ffmpeg watchdog's
    /// cancel‑checking loop (checked every ~100 ms regardless of stderr
    /// output).  `None` = no 100‑ms cancel check (cancellation is only
    /// detected between stderr lines).
    fn cancel_atomic(&self) -> Option<&std::sync::atomic::AtomicBool>;
}

/// Production implementation of `ConversionReport` that bridges into
/// the unified job infrastructure (`ProgressTracker` + `CancelToken`).
pub struct JobConversionReport<'a> {
    ctx: &'a JobContext,
    step_weight: AtomicU32,
    overall_progress: AtomicU32,
    overall_log: Mutex<String>,
    failed: AtomicBool,
}

impl<'a> JobConversionReport<'a> {
    pub fn new(ctx: &'a JobContext) -> Self {
        JobConversionReport {
            ctx,
            step_weight: AtomicU32::new(0),
            overall_progress: AtomicU32::new(0),
            overall_log: Mutex::new(String::new()),
            failed: AtomicBool::new(false),
        }
    }

    fn overall(&self) -> f32 {
        self.overall_progress.load(Ordering::Relaxed) as f32 / 1000.0
    }

    fn set_overall(&self, f: f32) {
        self.overall_progress.store((f.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }
}

impl<'a> ConversionReport for JobConversionReport<'a> {
    fn is_cancelled(&self) -> bool {
        self.ctx.cancel.is_cancelled()
    }

    fn step_weight(&self) -> f32 {
        self.step_weight.load(Ordering::Relaxed) as f32 / 1000.0
    }

    fn set_step_weight(&self, w: f32) {
        self.step_weight.store((w.clamp(0.0, 1.0) * 1000.0) as u32, Ordering::Relaxed);
    }

    fn report_step_fraction(&self, step_fraction: f32, _line: &str) {
        let overall = self.overall();
        let sw = self.step_weight();
        let combined = overall + step_fraction * sw;
        let clamped = combined.min(1.0);
        self.ctx.progress.unit(0).set_fraction(clamped);
        self.ctx.progress.set_message(format!("Conversion: {:.0}%", clamped * 100.0));
    }

    fn advance_step(&self) {
        let overall = self.overall();
        let sw = self.step_weight();
        self.set_overall(overall + sw);
        self.ctx.progress.unit(0).set_fraction(self.overall().min(1.0));
    }

    fn set_log(&self, text: &str) {
        *self.overall_log.lock().unwrap() = text.to_string();
    }

    fn append_log(&self, text: &str) {
        self.overall_log.lock().unwrap().push_str(text);
        let log = self.overall_log.lock().unwrap().clone();
        self.ctx.progress.set_message(log);
    }

    fn set_message(&self, msg: &str) {
        self.ctx.progress.set_message(msg);
    }

    fn mark_failed(&self, log: &str) {
        self.failed.store(true, Ordering::Relaxed);
        self.ctx.progress.unit(0).set_fraction(0.0);
        let log_text = self.overall_log.lock().unwrap().clone();
        let full = if log.is_empty() { log_text } else { format!("{}\n{}", log_text, log) };
        // Also record the failure in the tracker's rolling log so the
        // failure context rides along in JobStatus.log and JobOutcome.log.
        if !log.is_empty() {
            self.ctx.progress.push_log(log);
        }
        self.ctx.progress.set_message(full.clone());
        self.ctx.progress.unit(0).set_fraction(0.0);
    }

    fn is_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    fn mark_completed(&self, summary: &str) {
        let log = self.overall_log.lock().unwrap().clone();
        let msg = format!("{}\n\n--- CONVERSION COMPLETED SUCCESSFULLY ---{}", log, summary);
        self.ctx.progress.unit(0).set_fraction(1.0);
        self.ctx.progress.set_message(msg);
    }

    fn set_unit_count(&self, n: usize) {
        self.ctx.progress.grow_to(n);
    }

    fn unit(&self, idx: usize) -> Option<UnitProgress> {
        if idx < self.ctx.progress.snapshot().units.len() {
            Some(self.ctx.progress.unit(idx))
        } else {
            None
        }
    }

    fn cancel_atomic(&self) -> Option<&std::sync::atomic::AtomicBool> {
        Some(self.ctx.cancel.inner())
    }
}

/// In-memory report for unit tests. Wraps shared mutable state in
/// `Arc` for thread-safety (tests run inside `spawn_job` threads).
#[derive(Clone)]
pub struct TestReport {
    pub cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub progress: std::sync::Arc<Mutex<f32>>,
    progress_history: std::sync::Arc<Mutex<Vec<f32>>>,
    pub log: std::sync::Arc<Mutex<String>>,
    pub message: std::sync::Arc<Mutex<String>>,
    pub failed: std::sync::Arc<Mutex<bool>>,
    pub completed: std::sync::Arc<Mutex<bool>>,
    /// Set by `set_failures` (the typed failure-ledger seam).
    failures: std::sync::Arc<Mutex<Vec<StepFailureRecord>>>,
    /// Set by `note_cancelled`.
    cancelled_notified: std::sync::Arc<std::sync::atomic::AtomicBool>,
    step_weight: std::sync::Arc<Mutex<f32>>,
    /// Completed-step weight accumulator, mirroring the production report's
    /// `overall_progress`: only `advance_step` mutates it. `progress` holds
    /// the published combined value (overall + in-flight contribution).
    overall: std::sync::Arc<Mutex<f32>>,
}

impl TestReport {
    pub fn new() -> Self {
        TestReport::default()
    }

    /// Structural view of the recorded step failures (empty if none).
    pub fn failures(&self) -> Vec<StepFailureRecord> {
        self.failures.lock().unwrap().clone()
    }

    /// Whether `note_cancelled` has been called.
    pub fn cancel_notified(&self) -> bool {
        self.cancelled_notified
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Every progress value observed after each `report_step_fraction` /
    /// `advance_step` call, in call order. All report calls happen
    /// synchronously inside `run_conversion` on the calling thread, so the
    /// history records the exact sequence deterministically.
    pub fn progress_history(&self) -> Vec<f32> {
        self.progress_history.lock().unwrap().clone()
    }
}

impl Default for TestReport {
    fn default() -> Self {
        TestReport {
            cancelled: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            progress: std::sync::Arc::new(Mutex::new(0.0)),
            progress_history: std::sync::Arc::new(Mutex::new(Vec::new())),
            log: std::sync::Arc::new(Mutex::new(String::new())),
            message: std::sync::Arc::new(Mutex::new(String::new())),
            failed: std::sync::Arc::new(Mutex::new(false)),
            completed: std::sync::Arc::new(Mutex::new(false)),
            failures: std::sync::Arc::new(Mutex::new(Vec::new())),
            cancelled_notified: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            step_weight: std::sync::Arc::new(Mutex::new(0.0)),
            overall: std::sync::Arc::new(Mutex::new(0.0)),
        }
    }
}

impl ConversionReport for TestReport {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Relaxed)
    }

    fn step_weight(&self) -> f32 {
        *self.step_weight.lock().unwrap()
    }

    fn set_step_weight(&self, w: f32) {
        *self.step_weight.lock().unwrap() = w;
    }

    fn report_step_fraction(&self, step_fraction: f32, _line: &str) {
        let overall = *self.overall.lock().unwrap();
        let sw = *self.step_weight.lock().unwrap();
        let combined = (overall + step_fraction * sw).min(1.0);
        *self.progress.lock().unwrap() = combined;
        self.progress_history.lock().unwrap().push(combined);
    }

    fn advance_step(&self) {
        let overall = *self.overall.lock().unwrap();
        let sw = *self.step_weight.lock().unwrap();
        let new_overall = (overall + sw).min(1.0);
        *self.overall.lock().unwrap() = new_overall;
        *self.progress.lock().unwrap() = new_overall;
        self.progress_history.lock().unwrap().push(new_overall);
    }

    fn set_log(&self, text: &str) {
        *self.log.lock().unwrap() = text.to_string();
    }

    fn append_log(&self, text: &str) {
        self.log.lock().unwrap().push_str(text);
    }

    fn set_message(&self, msg: &str) {
        *self.message.lock().unwrap() = msg.to_string();
    }

    fn mark_failed(&self, log: &str) {
        *self.failed.lock().unwrap() = true;
        // Mirror the production report: the failure text rides in the
        // message (the log buffer is owned by append_log).
        if !log.is_empty() {
            self.message.lock().unwrap().push_str(log);
        }
    }

    fn is_failed(&self) -> bool {
        *self.failed.lock().unwrap()
    }

    fn mark_completed(&self, _summary: &str) {
        *self.completed.lock().unwrap() = true;
    }

    fn set_unit_count(&self, _n: usize) {}

    fn set_failures(&self, failures: &[StepFailureRecord]) {
        *self.failures.lock().unwrap() = failures.to_vec();
    }

    fn note_cancelled(&self) {
        self.cancelled_notified
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    fn unit(&self, _idx: usize) -> Option<UnitProgress> {
        None
    }

    fn cancel_atomic(&self) -> Option<&std::sync::atomic::AtomicBool> {
        Some(&self.cancelled)
    }
}

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

    /// Encoders that failed initialization during this run (structurally
    /// recorded, replacing prose pins in tests).
    #[cfg(test)]
    pub fn failed(&self) -> &BTreeSet<String> {
        &self.failed
    }
}

/// Typed result of [`run_video_step_with_fallback`], replacing the former
/// `bool` return.
#[derive(Debug, Clone, PartialEq)]
pub enum StepOutcome {
    /// The step completed successfully (some encoder produced output).
    Succeeded,
    /// No encoder candidate succeeded — the chain was empty from the start
    /// or every candidate failed to initialize (or the run was cancelled
    /// mid-step; the caller distinguishes via `report.is_cancelled()`).
    Exhausted,
    /// A non-retryable ffmpeg failure terminated the step.
    Failed(StepFailure),
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
    report: &impl ConversionReport,
    total_steps: usize,
    current_step: usize,
) -> StepOutcome {
    let candidates = fallback.remaining();
    if candidates.is_empty() {
        let msg = "no video encoder candidate available".to_string();
        warn!("{}", msg);
        report.append_log(&format!("\n\n--- {} ---", msg));
        report.mark_failed(&msg);
        return StepOutcome::Exhausted;
    }

    let mut attempt = 0;
    while attempt < candidates.len() {
        let encoder = candidates[attempt].clone();
        if report.is_cancelled() {
            return StepOutcome::Exhausted;
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
            report.append_log(&format!("\n--- {} ---\n", msg));
            fallback.note_failure(&encoder);
            attempt += 1;
            continue;
        }

        let args = build_args(settings);
        match run_ffmpeg_process(
            &args,
            output,
            report,
            total_steps,
            current_step,
        ) {
            Ok(()) => {
                fallback.note_success(&encoder);
                return StepOutcome::Succeeded;
            }
            Err(f @ StepFailure::Fatal(_)) => {
                report.mark_failed(&f.to_string());
                return StepOutcome::Failed(f);
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
                    report.append_log(&format!("\n--- {} ---\n", msg));
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
    report.append_log(&format!("\n\n--- {} ---", msg));
    report.mark_failed(&msg);
    StepOutcome::Exhausted
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

/// Run the full conversion pipeline, reporting progress through the provided
/// report. This is the main entry point called by `spawn_conversion_job`.
pub fn run_conversion<R: ConversionReport>(
    report: &R,
    mut settings: ConverterSettings,
    caps: Option<&crate::converter::capabilities::FfmpegCapabilities>,
) -> (Option<String>, bool) {
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

    let copy_mode = settings.copy_video
        && matches!(settings.pipeline, ConversionPipeline::VideoPassthrough);
    let metadata_only = matches!(settings.pipeline, ConversionPipeline::MetadataOnly);
    if copy_mode {
        prepare_copy_mode(&mut settings);
    } else if metadata_only {
        settings.trim_offsets_secs = vec![0.0; settings.trim_offsets_secs.len()];
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

    report.set_message(&format!("Pipeline: {:?}, {} steps", settings.pipeline, total_steps));

match settings.pipeline {
        ConversionPipeline::AudioOnly { generate_synthetic_video: false } => {
            run_audio_to_audio(&settings, &output_format, extension, report);
        }
        ConversionPipeline::AudioOnly { generate_synthetic_video: true } => {
            run_audio_to_synthetic_video(&mut settings, extension, &mut fallback, report);
        }
        ConversionPipeline::VideoPassthrough => {
            run_video_to_video(&mut settings, extension, &mut fallback, report, &mut total_steps);
        }
        ConversionPipeline::MetadataOnly => {
            run_metadata_only(&settings, report, total_steps);
        }
    }

    if !report.is_failed() && !report.is_cancelled() {
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
        report.mark_completed(&encoder_line);
    }

    let encoder_used = fallback.resolved().map(|e| e.to_string());

    (encoder_used, metadata_only)
}

/// Version of the conversion that works with `spawn_job` from the unified
/// job infrastructure. Calls `run_conversion` directly on the job thread
/// without spawning an internal thread or using `SharedConversionState`.
pub fn spawn_conversion_job(
    ctx: &JobContext,
    settings: ConverterSettings,
    caps: Option<crate::converter::capabilities::FfmpegCapabilities>,
) -> Result<JobFinal, JobError> {
    ctx.progress.grow_to(1);

    let report = JobConversionReport::new(ctx);
    let (encoder_used, _metadata_only) = run_conversion(&report, settings, caps.as_ref());

    ctx.progress.unit(0).finish();
    ctx.progress.set_message("");

    if report.is_failed() {
        let log_msg = ctx.progress.snapshot().message.clone();
        let err_msg = if log_msg.is_empty() { "Unknown conversion failure".to_string() } else { log_msg };
        Err(JobError::Failed(err_msg))
    } else if report.is_cancelled() {
        info!("Conversion job cancelled");
        Err(JobError::Cancelled)
    } else {
        info!("Conversion job completed successfully");
        Ok(JobFinal::Conversion {
            encoder_used,
            steps_attempted: 0,
        })
    }
}

fn run_audio_to_audio(
    settings: &ConverterSettings,
    format: &str,
    extension: &str,
    report: &impl ConversionReport,
) {
    let sample_rate = settings
        .input_files
        .first()
        .and_then(|p| crate::converter::timecode::read_wav_sample_rate(p))
        .unwrap_or(48000);

    if settings.split_tracks {
        let map_n = settings.channel_map.num_channels();
        let physical: Vec<(usize, usize)> = (0..map_n).map(|i| (i, 0)).collect();
        let mut emitted = 0usize;
        for sel in selected_channel_pairs(settings, &physical) {
            if report.is_cancelled() { break; }

            let output_path = settings.output_path_for_index("audio", sel.output_k + 1, extension);
            let tc = settings
                .timecode_meta_per_file
                .get(sel.input_i)
                .and_then(|m| m.as_ref());
            let step_args = build_split_track_args(settings, format, sel.output_k, tc, sample_rate);
            let total_tracks = settings.channel_map.num_channels();
            let current = emitted + 1;
            report.set_step_weight(1.0 / total_tracks as f32);
            if let Err(e) =
                run_ffmpeg_process(&step_args, &output_path, report, total_tracks, current)
            {
                report.mark_failed(&e.to_string());
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
            report.append_log(&format!("\n\n--- {} ---", msg));
            report.mark_failed(&msg);
        }
    } else {
        let tc = settings
            .timecode_meta_per_file
            .first()
            .and_then(|m| m.as_ref());
        let base_args = build_audio_to_audio_args(settings, format, tc, sample_rate);
        let output_path = settings.output_path_for_index("audio", 0, extension);
        report.set_step_weight(1.0);
        if let Err(e) = run_ffmpeg_process(&base_args, &output_path, report, 1, 1) {
            report.mark_failed(&e.to_string());
        }
    }
}

fn run_audio_to_synthetic_video(
    settings: &mut ConverterSettings,
    extension: &str,
    fallback: &mut EncoderFallback,
    report: &impl ConversionReport,
) {
    let output_path = settings.output_path_for_index("video", 1, extension);
    let mut build_args =
        |s: &ConverterSettings| build_audio_to_synthetic_video_args(s);
    report.set_step_weight(1.0);
    run_video_step_with_fallback(
        settings,
        fallback,
        &mut build_args,
        &output_path,
        report,
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
    report: &impl ConversionReport,
    total_steps: &mut usize,
) {
    let mut probes: Vec<Option<VideoAudioProbe>> = Vec::new();

    for file_idx in 0..settings.input_files.len() {
        if report.is_cancelled() { break; }
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
            if report.is_cancelled() { break; }
            let video_out = settings.output_path_for_file("video", file_idx, file_idx + 1, ext);
            steps.push(StepEntry {
                step: VideoOutputStep::VideoOnly { file_idx, output: video_out, naming_index: file_idx + 1 },
                is_audio_only: false,
            });
        }

        let (concat_steps, warning) = plan_concat_outputs(settings, &probes);
        if !warning.is_empty() {
            warn!("{}", warning.trim());
            report.append_log(&format!("\n--- {}\n", warning.trim()));
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

    *total_steps = steps.len();

    for (step_idx, entry) in steps.iter().enumerate() {
        if report.is_cancelled() { break; }

        report.set_step_weight(1.0 / steps.len().max(1) as f32);

        if entry.is_audio_only {
            if let VideoOutputStep::AudioChannelConcat { segments, output, format, sample_rate } = &entry.step {
                if let Err(e) = run_ffmpeg_process(
                    &build_concat_audio_args(settings, segments, format, *sample_rate),
                    output, report, steps.len(), step_idx + 1,
                ) {
                    report.mark_failed(&e.to_string());
                    break;
                }
            }
        } else {
            let output = entry.step.output().to_path_buf();
            let probe = probes.first().cloned().flatten().unwrap_or(VideoAudioProbe {
                streams: Vec::new(),
                total_audio_channels: 0,
                is_video_file: true,
            });
            let mut build_args =
                |s: &ConverterSettings| build_video_to_video_args(s, &entry.step, &probe);

            let outcome = if settings.copy_video {
                match run_ffmpeg_process(
                    &build_args(settings),
                    &output,
                    report,
                    steps.len(),
                    step_idx + 1,
                ) {
                    Ok(()) => StepOutcome::Succeeded,
                    Err(e) => {
                        report.mark_failed(&e.to_string());
                        StepOutcome::Failed(e)
                    }
                }
            } else {
                run_video_step_with_fallback(
                    settings,
                    fallback,
                    &mut build_args,
                    &output,
                    report,
                    steps.len(),
                    step_idx + 1,
                )
            };
            if !matches!(outcome, StepOutcome::Succeeded) {
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

/// Appends the CANCELLED log block and marks the run failed. Returns
/// `true` when the caller should return immediately.
fn check_cancelled(report: &impl ConversionReport) -> bool {
    if !report.is_cancelled() {
        return false;
    }
    report.note_cancelled();
    report.append_log("\n--- CANCELLED ---\n");
    report.mark_failed("Cancelled by user");
    true
}

/// Typed record of one failed pipeline step in the metadata-only failure
/// ledger. `kind` discriminates the phase that failed; `detail` is the
/// human-readable line rendered in the `--- N STEP(S) FAILED ---` block.
#[derive(Debug, Clone, PartialEq)]
pub struct StepFailureRecord {
    pub kind: FailureKind,
    pub file: Option<PathBuf>,
    pub detail: String,
}

/// Which pipeline phase produced a [`StepFailureRecord`].
#[derive(Debug, Clone, PartialEq)]
pub enum FailureKind {
    Probe,
    /// Audio-extraction ffmpeg step; carries the typed step failure.
    Extraction(StepFailure),
    Tag,
    Rename,
    /// Any other ffmpeg step failure; carries the typed step failure.
    Ffmpeg(StepFailure),
}

impl FailureKind {
    /// Rendering of the embedded step failure, if any (used to compose the
    /// record's `detail` line).
    fn failure_detail(&self) -> String {
        match self {
            FailureKind::Extraction(f) | FailureKind::Ffmpeg(f) => f.to_string(),
            FailureKind::Probe | FailureKind::Tag | FailureKind::Rename => String::new(),
        }
    }
}

/// Failure ledger shared by the metadata-only phases: counts attempted
/// vs. successful steps and records one typed record per failure.
#[derive(Default)]
struct FailureLedger {
    attempted: usize,
    succeeded: usize,
    details: Vec<StepFailureRecord>,
}

impl FailureLedger {
    fn note_success(&mut self) {
        self.attempted += 1;
        self.succeeded += 1;
    }
}

/// Record an ffmpeg step failure with its typed payload: appends `msg` plus
/// the `StepFailure` detail to the log and tracks it in the ledger.
fn note_step_failure(
    ledger: &mut FailureLedger,
    report: &impl ConversionReport,
    kind: FailureKind,
    file: Option<&Path>,
    msg: &str,
) {
    ledger.attempted += 1;
    let detail = format!("{} ({})", msg.trim_end(), kind.failure_detail());
    ledger.details.push(StepFailureRecord {
        kind,
        file: file.map(Path::to_path_buf),
        detail: detail.clone(),
    });
    report.append_log(&format!("✗ {}\n", detail));
}

/// Record a best-effort failure (probe / tag / rename errors) in the ledger.
fn note_plaintext_failure(
    ledger: &mut FailureLedger,
    kind: FailureKind,
    file: Option<&Path>,
    detail: String,
) {
    ledger.attempted += 1;
    ledger.details.push(StepFailureRecord {
        kind,
        file: file.map(Path::to_path_buf),
        detail,
    });
}

/// Monotonic step label replacing the old `file_idx * 3 + 1` math.
fn step_label(cursor: usize, total: usize) -> String {
    format!("[{}/{}]", cursor, total)
}

fn run_metadata_only(
    settings: &ConverterSettings,
    report: &impl ConversionReport,
    total_steps: usize,
) {
    run_metadata_only_with(settings, report, total_steps, &mut |p: &Path| {
        crate::ffprobe::probe_video_audio(p).map_err(|e| e.to_string())
    })
}

/// Injectable-prober variant of [`run_metadata_only`] so logic-level tests
/// can drive the phases without ffprobe.
fn run_metadata_only_with(
    settings: &ConverterSettings,
    report: &impl ConversionReport,
    total_steps: usize,
    prober: &mut dyn FnMut(&Path) -> Result<crate::ffprobe::VideoAudioProbe, String>,
) {
    let mut ledger = FailureLedger::default();
    let Some(probes) = probe_all_metadata_files_with(settings, report, &mut ledger, prober) else {
        return;
    };
    let Some(()) = extract_metadata_audio(settings, &probes, report, total_steps, &mut ledger) else {
        return;
    };
    let Some(()) = tag_and_rename_files(settings, &probes, report, &mut ledger) else {
        return;
    };
    summarize_metadata_failures(report, &ledger);
}

/// Phase P — probe every input up front (video inputs only) so
/// `plan_concat_outputs` can see every clip. Returns `None` on cancel.
fn probe_all_metadata_files_with(
    settings: &ConverterSettings,
    report: &impl ConversionReport,
    ledger: &mut FailureLedger,
    prober: &mut dyn FnMut(&Path) -> Result<crate::ffprobe::VideoAudioProbe, String>,
) -> Option<Vec<Option<crate::ffprobe::VideoAudioProbe>>> {
    let is_video = settings.recording_type == RecordingType::VideoClipSequence;
    let mut probes = Vec::with_capacity(settings.input_files.len());
    for input_path in settings.input_files.iter() {
        if check_cancelled(report) {
            return None;
        }
        let probed: Option<crate::ffprobe::VideoAudioProbe> = if is_video {
            match prober(input_path) {
                Ok(p) => Some(p),
                Err(e) => {
                    let msg = format!(
                        "✗ {} — probe failed: {} (skipping audio extraction)\n",
                        input_path.display(),
                        e
                    );
                    log::warn!("{}", msg.trim());
                    report.append_log(&msg);
                    note_plaintext_failure(
                        ledger,
                        FailureKind::Probe,
                        Some(input_path),
                        format!("{} — probe failed: {}", input_path.display(), e),
                    );
                    None
                }
            }
        } else {
            None
        };
        probes.push(probed);
    }
    Some(probes)
}

/// Phase E — audio extraction, dispatching to the concat or per-file
/// strategy. Returns `None` on cancel.
fn extract_metadata_audio(
    settings: &ConverterSettings,
    probes: &[Option<crate::ffprobe::VideoAudioProbe>],
    report: &impl ConversionReport,
    total_steps: usize,
    ledger: &mut FailureLedger,
) -> Option<()> {
    let is_video = settings.recording_type == RecordingType::VideoClipSequence;
    let use_concat = is_video && settings.concat_audio && settings.split_tracks;
    if use_concat {
        extract_concat_audio(settings, probes, report, ledger)
    } else {
        extract_per_file_audio(settings, probes, report, total_steps, ledger, is_video)
    }
}

/// Phase E (concat) — one concatenated audio output per surviving track.
fn extract_concat_audio(
    settings: &ConverterSettings,
    probes: &[Option<crate::ffprobe::VideoAudioProbe>],
    report: &impl ConversionReport,
    ledger: &mut FailureLedger,
) -> Option<()> {
    let (concat_steps, warning) = plan_concat_outputs(settings, probes);
    if !warning.is_empty() {
        let w = warning.trim().to_string();
        log::warn!("{}", w);
        report.append_log(&format!("\n--- {}\n", w));
    }
    let total_actual = concat_steps.len() + settings.input_files.len();
    let step_weight = if total_actual > 0 {
        1.0 / total_actual as f32
    } else {
        0.0
    };
    report.set_step_weight(step_weight);

    for (cursor, step) in (1usize..).zip(concat_steps.iter()) {
        if check_cancelled(report) {
            return None;
        }
        match step {
            VideoOutputStep::AudioChannelConcat {
                segments,
                output,
                format,
                sample_rate,
            } => {
                if let Err(e) = run_ffmpeg_process(
                    &build_concat_audio_args(settings, segments, format, *sample_rate),
                    output,
                    report,
                    total_actual,
                    cursor,
                ) {
                    note_step_failure(
                        ledger,
                        report,
                        FailureKind::Extraction(e.clone()),
                        Some(output),
                        &format!("audio concatenation step {} failed", step_label(cursor, total_actual)),
                    );
                } else {
                    ledger.note_success();
                }
            }
            VideoOutputStep::AudioChannel {
                file_idx,
                stream_idx,
                channel_idx,
                output,
                format,
                ..
            } => {
                let sr = probes[*file_idx]
                    .as_ref()
                    .and_then(|p| {
                        p.streams.iter().find(|s| s.stream_index == *stream_idx)
                    })
                    .map(|s| s.sample_rate)
                    .unwrap_or(48000);
                if let Err(e) = run_ffmpeg_process(
                    &build_video_track_extract_args(
                        settings, *file_idx, *stream_idx, *channel_idx, format, sr,
                    ),
                    output,
                    report,
                    total_actual,
                    cursor,
                ) {
                    note_step_failure(
                        ledger,
                        report,
                        FailureKind::Extraction(e.clone()),
                        Some(settings.input_files[*file_idx].as_path()),
                        &format!(
                            "{} — audio extraction failed",
                            settings.input_files[*file_idx].display()
                        ),
                    );
                } else {
                    ledger.note_success();
                }
            }
            _ => {}
        }
    }
    Some(())
}

/// Phase E (per-file) — split or merged extraction for each probed clip.
fn extract_per_file_audio(
    settings: &ConverterSettings,
    probes: &[Option<crate::ffprobe::VideoAudioProbe>],
    report: &impl ConversionReport,
    total_steps: usize,
    ledger: &mut FailureLedger,
    is_video: bool,
) -> Option<()> {
    let (fmt, aext) = audio_encoder_to_output_format(&settings.audio_encoder);
    let total_actual = total_steps;
    report.set_step_weight(1.0 / total_actual.max(1) as f32);
    let mut cursor = 1usize;

    for (file_idx, probed) in probes.iter().enumerate() {
        if check_cancelled(report) {
            return None;
        }
        let input_path = &settings.input_files[file_idx];

        if let Some(ref probe) = probed {
            let channels: Vec<(usize, usize)> = probe
                .streams
                .iter()
                .flat_map(|s| (0..s.channels).map(move |ch| (s.stream_index, ch)))
                .collect();
            let use_split =
                settings.split_tracks && settings.channel_map.num_channels() > 0;

            if use_split {
                let mut emitted = 0usize;
                for sel in selected_channel_pairs(settings, &channels) {
                    if check_cancelled(report) {
                        return None;
                    }
                    let (stream_idx, ch_idx) = sel.pair;
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
                    if let Err(e) = run_ffmpeg_process(
                        &args,
                        &output_path,
                        report,
                        total_actual,
                        cursor,
                    ) {
                        note_step_failure(
                            ledger,
                            report,
                            FailureKind::Extraction(e.clone()),
                            Some(input_path),
                            &format!("{} — audio extraction failed", input_path.display()),
                        );
                    } else {
                        ledger.note_success();
                    }
                    cursor += 1;
                }
                if emitted == 0 {
                    report.advance_step();
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
                if let Err(e) = run_ffmpeg_process(
                    &args,
                    &output_path,
                    report,
                    total_actual,
                    cursor,
                ) {
                    note_step_failure(
                        ledger,
                        report,
                        FailureKind::Extraction(e.clone()),
                        Some(input_path),
                        &format!("{} — audio extraction failed", input_path.display()),
                    );
                } else {
                    ledger.note_success();
                }
                cursor += 1;
            }
        } else if !is_video {
            report.advance_step();
        }
    }
    Some(())
}

/// Phases T+R — per-file tagging and rename into the source directory.
fn tag_and_rename_files(
    settings: &ConverterSettings,
    probes: &[Option<crate::ffprobe::VideoAudioProbe>],
    report: &impl ConversionReport,
    ledger: &mut FailureLedger,
) -> Option<()> {
    for (file_idx, _probed) in probes.iter().enumerate() {
        if check_cancelled(report) {
            return None;
        }

        let input_path = &settings.input_files[file_idx];
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
                    report.append_log(&msg);
                }
                Err(e) => {
                    let detail =
                        format!("{} — tagging failed: {}", input_path.display(), e);
                    log::error!("{}", detail);
                    report.append_log(&format!("✗ {}\n", detail));
                    note_plaintext_failure(ledger, FailureKind::Tag, Some(input_path), detail);
                }
            }
        } else {
            let msg = format!(
                "⚠ {} — no start timecode available, skipping tagging\n",
                input_path.display()
            );
            log::warn!("{}", msg.trim());
            report.append_log(&msg);
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
                    report.append_log(&msg);
                } else {
                    match std::fs::rename(input_path, &new_path) {
                        Ok(()) => {
                            let msg = format!(
                                "✓ {} → {}\n",
                                input_path.display(),
                                new_path.display()
                            );
                            report.append_log(&msg);
                        }
                        Err(e) => {
                            let detail = format!(
                                "{} — rename failed: {}",
                                input_path.display(),
                                e
                            );
                            log::error!("{}", detail);
                            report.append_log(&format!("✗ {}\n", detail));
                            note_plaintext_failure(
                                ledger,
                                FailureKind::Rename,
                                Some(input_path),
                                detail,
                            );
                        }
                    }
                }
            }
        }

        report.advance_step();
    }
    Some(())
}

/// Epilogue — a run in which work was attempted but nothing succeeded is a
/// failure carrying the typed details; a partial failure completes with a
/// visible STEP(S) FAILED block in the log.
fn summarize_metadata_failures(report: &impl ConversionReport, ledger: &FailureLedger) {
    if ledger.details.is_empty() {
        return;
    }
    report.set_failures(&ledger.details);
    let summary = format!(
        "\n--- {} STEP(S) FAILED ---\n{}",
        ledger.details.len(),
        ledger.details
            .iter()
            .map(|r| r.detail.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
    if ledger.attempted > 0 && ledger.succeeded == 0 {
        report.append_log(&summary);
        report.mark_failed(&summary);
    } else {
        report.append_log(&summary);
    }
}

#[cfg(test)]
mod tests {
    use crate::converter::test_fixtures::*;
    use crate::ChannelMap;
    use super::*;

    #[test]
    fn test_run_audio_to_audio_split_all_dropped_fails() {
        let mut settings = make_settings_audio_only();
        settings.split_tracks = true;
        settings.drop_ltc_track = true;
        settings.channel_map = ChannelMap::identity(1);
        settings.ltc_track_channel_index = 0;

        let report = TestReport::new();

        run_audio_to_audio(
            &settings, "wav", "wav", &report,
        );

        assert!(
            *report.failed.lock().unwrap(),
            "expected Failed when all tracks are dropped"
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

        let report = TestReport::new();

        run_audio_to_audio(
            &settings, "wav", "wav", &report,
        );

        assert!(
            !*report.failed.lock().unwrap(),
            "should not be failed when one track survives"
        );
    }

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

    // ── PR-2 characterization tests (real ffmpeg; loud-skip) ─────────────

    fn ffmpeg_available() -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn skip_if_no_ffmpeg() -> bool {
        if !ffmpeg_available() {
            eprintln!("--- SKIPPED: ffmpeg not available");
            return true;
        }
        false
    }

    fn tc_meta() -> Option<crate::converter::timecode::TimecodeMetadata> {
        Some(crate::converter::timecode::TimecodeMetadata {
            start: audio_core::Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        })
    }

    #[test]
    fn test_run_metadata_only_wav_happy_path() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let wav1 = create_test_wav(dir.path(), "take1.wav", 48000, 0.25);
        let wav2 = create_test_wav(dir.path(), "take2.wav", 48000, 0.25);

        let mut settings = make_settings_audio_only();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![wav1.clone(), wav2.clone()];
        settings.output_folder = dir.path().to_path_buf();
        settings.timecode_meta_per_file = vec![tc_meta(), tc_meta()];

        let report = TestReport::new();
        run_conversion(&report, settings, None);

        assert!(!*report.failed.lock().unwrap(), "metadata-only wav run should not fail; log: {}",
            report.log.lock().unwrap());
        assert!(*report.completed.lock().unwrap(), "should complete");
        // Originals were tagged in place and renamed (prefix "output" is
        // non-empty), so the original names must be gone.
        assert!(!wav1.exists() && !wav2.exists(), "originals should have been renamed");
    }

    #[test]
    fn test_run_metadata_only_video_extraction() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let clip = dir.path().join("clip1.mp4");
        crate::converter::test_fixtures::create_test_video_with_tone(&clip, 1.0);

        let mut settings = make_video_settings();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![clip.clone()];
        settings.output_folder = dir.path().to_path_buf();
        settings.timecode_meta_per_file = vec![tc_meta()];

        let report = TestReport::new();
        run_conversion(&report, settings, None);

        assert!(!*report.failed.lock().unwrap(), "metadata-only video run should not fail; log: {}",
            report.log.lock().unwrap());
        // Extracted merged audio must exist in the output folder.
        let wavs: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "wav").unwrap_or(false))
            .collect();
        assert!(!wavs.is_empty(), "expected an extracted wav in {}", dir.path().display());
    }

    /// Pins current best-effort semantics: a probe failure logs "✗ … probe
    /// failed" but the run still completes (failed == false). PR-3 changes
    /// the all-steps-failed accounting deliberately.
    #[test]
    fn test_run_metadata_only_probe_failure_is_best_effort() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let clip = dir.path().join("clip1.mp4");
        crate::converter::test_fixtures::create_test_video_with_tone(&clip, 1.0);

        let mut settings = make_video_settings();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![clip, PathBuf::from("/nonexistent/clip2.mp4")];
        settings.output_folder = dir.path().to_path_buf();
        settings.timecode_meta_per_file = vec![tc_meta(), tc_meta()];

        let report = TestReport::new();
        let total = settings.input_files.len() * 3;
        run_metadata_only(&settings, &report, total);

        assert!(!*report.failed.lock().unwrap(),
            "probe failure is best-effort; run should not be marked failed");
        // clip1 was still processed: its extracted audio exists on disk.
        let wavs: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "wav").unwrap_or(false))
            .collect();
        assert!(!wavs.is_empty(), "clip1 should still be extracted: expected a wav in {}",
            dir.path().display());
        // The probe failure for clip2 is recorded structurally in the
        // ledger, together with its best-effort tag/rename failures.
        let failures = report.failures();
        assert_eq!(failures.len(), 3, "probe + tag + rename failures for clip2: {:?}",
            failures);
        let missing = Path::new("/nonexistent/clip2.mp4");
        assert!(failures.iter().any(|r| matches!(r.kind, FailureKind::Probe)
            && r.file.as_deref() == Some(missing)));
        assert!(failures.iter().any(|r| matches!(r.kind, FailureKind::Tag)
            && r.file.as_deref() == Some(missing)));
        assert!(failures.iter().any(|r| matches!(r.kind, FailureKind::Rename)
            && r.file.as_deref() == Some(missing)));
    }

    #[test]
    fn test_run_metadata_only_cancelled_before_start() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut settings = make_settings_audio_only();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![dir.path().join("a.wav")];
        settings.output_folder = dir.path().to_path_buf();

        let report = TestReport::new();
        report.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        run_metadata_only(&settings, &report, 2);

        assert!(*report.failed.lock().unwrap(), "cancelled run must be failed");
        assert!(report.cancel_notified(),
            "cancellation must be signalled through the typed report seam");
        assert!(report.failures().is_empty(), "no steps were attempted");
        assert!(std::fs::read_dir(dir.path()).unwrap().count() == 0, "no output files");
    }

    #[test]
    fn test_run_video_to_video_copy_mode_missing_input_fails() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let mut settings = make_copy_settings();
        settings.input_files = vec![PathBuf::from("/nonexistent/clip.mp4")];
        settings.output_folder = dir.path().to_path_buf();

        let report = TestReport::new();
        let mut fallback = EncoderFallback::new_with_hw(
            vec!["libx264".into()],
            HwDeviceContext { vaapi_device: None, vulkan_available: false },
        );
        let mut total = 1;
        run_video_to_video(&mut settings, "mp4", &mut fallback, &report, &mut total);

        assert!(*report.failed.lock().unwrap(),
            "copy-mode run on a missing input must fail; log: {}",
            report.log.lock().unwrap());
    }

    #[test]
    fn test_run_video_to_video_happy_path() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let clip = dir.path().join("clip1.mp4");
        crate::converter::test_fixtures::create_test_video_with_tone(&clip, 1.0);

        let mut settings = make_video_settings();
        settings.input_files = vec![clip];
        settings.output_folder = dir.path().to_path_buf();

        let report = TestReport::new();
        // Software-only chain: the static h264 chain leads with h264_nvenc,
        // which production filters out via the capability probe's test encode
        // but which this test bypasses — on GPU-less CI the nvenc attempt
        // fails after producing no output and the run would not be retried.
        // Encoder-fallback ordering is covered by video_codecs unit tests.
        let mut fallback = EncoderFallback::new_with_hw(
            vec!["libx264".into()],
            HwDeviceContext { vaapi_device: None, vulkan_available: false },
        );
        let mut total = 1;
        run_video_to_video(&mut settings, "mkv", &mut fallback, &report, &mut total);

        assert!(!*report.failed.lock().unwrap(), "video passthrough should succeed; log: {}",
            report.log.lock().unwrap());
        let outs: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "mkv").unwrap_or(false))
            .collect();
        assert!(!outs.is_empty(), "expected an .mkv output in {}", dir.path().display());
    }

    // ── PR-3: phase helpers + failure accounting ─────────────────────────

    #[test]
    fn test_check_cancelled_marks_failed_and_logs() {
        let report = TestReport::new();
        assert!(!check_cancelled(&report), "no mutation when not cancelled");
        assert!(!*report.failed.lock().unwrap());
        assert!(report.log.lock().unwrap().is_empty());

        report.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(check_cancelled(&report));
        assert!(*report.failed.lock().unwrap());
        assert!(report.cancel_notified(),
            "check_cancelled must signal cancellation through the typed seam");
    }

    #[test]
    fn test_note_step_failure_carries_typed_payload() {
        let report = TestReport::new();
        let mut ledger = FailureLedger::default();
        let failure = StepFailure::Fatal("ffmpeg exited with code 1".to_string());
        note_step_failure(
            &mut ledger,
            &report,
            FailureKind::Ffmpeg(failure.clone()),
            None,
            "extract failed",
        );

        assert_eq!(ledger.attempted, 1);
        assert_eq!(ledger.succeeded, 0);
        assert_eq!(ledger.details.len(), 1);
        assert!(matches!(
            ledger.details[0].kind,
            FailureKind::Ffmpeg(StepFailure::Fatal(_))
        ));
        assert_eq!(ledger.details[0].file, None);
    }

    #[test]
    fn test_step_label_monotonic() {
        assert_eq!(step_label(1, 5), "[1/5]");
        assert_eq!(step_label(3, 5), "[3/5]");
    }

    /// WP-3.3 intended change: a metadata-only run in which every attempted
    /// step failed now ends in failure with the typed details (previously it
    /// completed "successfully").
    #[test]
    fn test_run_metadata_only_all_steps_failed_marks_failed() {
        if skip_if_no_ffmpeg() { return; }
        let mut settings = make_video_settings();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![
            PathBuf::from("/nonexistent/clip1.mp4"),
            PathBuf::from("/nonexistent/clip2.mp4"),
        ];
        settings.timecode_meta_per_file = vec![tc_meta(), tc_meta()];

        let report = TestReport::new();
        let total = settings.input_files.len() * 3;
        run_metadata_only(&settings, &report, total);

        assert!(*report.failed.lock().unwrap(),
            "a 100%% failed run must not report success");
        // Every attempted step for both nonexistent clips failed:
        // probe + tag + rename per clip, none succeeded.
        let failures = report.failures();
        assert_eq!(failures.len(), 6, "probe + tag + rename per clip: {:?}", failures);
        let count_kind = |pred: &dyn Fn(&&StepFailureRecord) -> bool| failures.iter().filter(pred).count();
        assert_eq!(count_kind(&|r| matches!(r.kind, FailureKind::Probe)), 2);
        assert_eq!(count_kind(&|r| matches!(r.kind, FailureKind::Tag)), 2);
        assert_eq!(count_kind(&|r| matches!(r.kind, FailureKind::Rename)), 2);
        assert!(failures.iter().all(|r| !matches!(r.kind, FailureKind::Ffmpeg(_))),
            "no ffmpeg step ever ran");
    }

    // ── PR-7: logic-level tests via the prober seam ──────────────────────

    use crate::converter::test_fixtures::{make_stereo_probe, make_video_settings as fixture_video};

    #[test]
    fn test_metadata_only_with_probe_failure_branch() {
        // Every probe fails (fake prober) → nothing succeeds → run fails
        // with the per-file details, without spawning any ffmpeg process.
        let mut settings = fixture_video();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![
            PathBuf::from("/nonexistent/clip1.mp4"),
            PathBuf::from("/nonexistent/clip2.mp4"),
        ];
        settings.timecode_meta_per_file = vec![tc_meta(), tc_meta()];

        let report = TestReport::new();
        let mut prober = |_p: &Path| Err("no such file".to_string());
        run_metadata_only_with(&settings, &report, 6, &mut prober);

        assert!(*report.failed.lock().unwrap(), "all probes failed → run failed");
        let failures = report.failures();
        // Probe, tag, and rename all fail per clip (best-effort phases keep
        // running after the probe failure); no ffmpeg step ever runs.
        assert_eq!(failures.len(), 6, "probe + tag + rename per clip: {:?}", failures);
        assert_eq!(
            failures.iter().filter(|r| matches!(r.kind, FailureKind::Probe)).count(),
            2,
            "one probe failure per clip"
        );
        assert!(failures.iter().all(|r| !matches!(r.kind, FailureKind::Ffmpeg(_))));
    }

    #[test]
    fn test_metadata_only_cancel_between_phases() {
        // The prober flips the cancel flag, so the run cancels at the start
        // of the extraction phase — before any ffmpeg spawn.
        let dir = tempfile::TempDir::new().unwrap();
        let mut settings = fixture_video();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![dir.path().join("clip1.mp4")];
        settings.output_folder = dir.path().to_path_buf();
        settings.timecode_meta_per_file = vec![tc_meta()];

        let report = TestReport::new();
        let cancelled = report.cancelled.clone();
        let mut prober = move |_p: &Path| {
            cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(make_stereo_probe())
        };
        run_metadata_only_with(&settings, &report, 3, &mut prober);

        assert!(*report.failed.lock().unwrap(), "cancel between phases fails the run");
        assert!(report.cancel_notified(),
            "cancel between phases must signal through the typed seam");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "no outputs produced");
    }

    #[test]
    fn test_metadata_only_concat_planning_path() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let clip1 = dir.path().join("clip1.mp4");
        let clip2 = dir.path().join("clip2.mp4");
        crate::converter::test_fixtures::create_test_video_with_tone(&clip1, 0.5);
        crate::converter::test_fixtures::create_test_video_with_tone(&clip2, 0.5);

        let mut settings = fixture_video();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![clip1, clip2];
        settings.output_folder = dir.path().to_path_buf();
        settings.split_tracks = true;
        settings.concat_audio = true;
        settings.channel_map = crate::ChannelMap::identity(2);
        settings.timecode_meta_per_file = vec![tc_meta(), tc_meta()];

        let report = TestReport::new();
        let mut prober = |_p: &Path| Ok(make_stereo_probe());
        run_metadata_only_with(&settings, &report, 8, &mut prober);

        // The concat plan was built and executed: one concatenated audio
        // output per surviving track (identity map over 2 channels).
        assert!(!*report.failed.lock().unwrap(), "concat run should not fail; log: {}",
            report.log.lock().unwrap());
        let wavs: Vec<_> = std::fs::read_dir(dir.path()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "wav").unwrap_or(false))
            .collect();
        assert_eq!(wavs.len(), 2, "one concat audio output per surviving track: {:?}",
            wavs);
    }

    #[test]
    fn test_metadata_only_extraction_failure_accounting() {
        if skip_if_no_ffmpeg() { return; }
        // Probe succeeds (fake), but extraction fails on the nonexistent
        // input → typed ffmpeg failure recorded; nothing succeeds → run
        // marked failed.
        let dir = tempfile::TempDir::new().unwrap();
        let mut settings = fixture_video();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![PathBuf::from("/nonexistent/clip1.mp4")];
        settings.output_folder = dir.path().to_path_buf();
        settings.timecode_meta_per_file = vec![tc_meta()];

        let report = TestReport::new();
        let mut prober = |_p: &Path| Ok(make_stereo_probe());
        run_metadata_only_with(&settings, &report, 3, &mut prober);

        assert!(*report.failed.lock().unwrap(), "all steps failed → failed");
        let failures = report.failures();
        // The extraction ffmpeg step fails (typed Extraction record), and
        // the best-effort tag/rename phases fail on the nonexistent input.
        assert_eq!(failures.len(), 3, "extraction + tag + rename: {:?}", failures);
        assert_eq!(
            failures.iter().filter(|r| matches!(r.kind, FailureKind::Extraction(_))).count(),
            1,
            "the failed extraction is recorded"
        );
    }

    #[test]
    fn test_video_to_video_concat_path_multi_clip() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let clip1 = dir.path().join("clip1.mp4");
        let clip2 = dir.path().join("clip2.mp4");
        crate::converter::test_fixtures::create_test_video_with_tone(&clip1, 0.5);
        crate::converter::test_fixtures::create_test_video_with_tone(&clip2, 0.5);

        let mut settings = fixture_video();
        settings.input_files = vec![clip1, clip2];
        settings.output_folder = dir.path().to_path_buf();
        settings.split_tracks = true;
        settings.concat_audio = true;
        settings.channel_map = crate::ChannelMap::identity(2);

        let report = TestReport::new();
        let mut fallback = EncoderFallback::new_with_hw(
            video_codecs::static_encoder_chain("h264"),
            HwDeviceContext { vaapi_device: None, vulkan_available: false },
        );
        let mut total = 0;
        run_video_to_video(&mut settings, "mkv", &mut fallback, &report, &mut total);

        assert!(!*report.failed.lock().unwrap(), "concat run should succeed; log: {}",
            report.log.lock().unwrap());
        // Per-step weight must be exactly 1.0 / steps.len() (steps.len()
        // is published via the total out-param).
        assert!(total > 0);
        assert!((report.step_weight() - 1.0 / total as f32).abs() < 1e-6,
            "set_step_weight = 1.0 / steps.len(); got {} for {} steps",
            report.step_weight(), total);
    }

    #[test]
    fn test_video_to_video_bogus_codec_falls_through_and_fails() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let clip = dir.path().join("clip1.mp4");
        crate::converter::test_fixtures::create_test_video_with_tone(&clip, 0.5);

        let mut settings = fixture_video();
        settings.input_files = vec![clip];
        settings.output_folder = dir.path().to_path_buf();
        settings.video_encoder = "no-such-codec".to_string();

        let report = TestReport::new();
        let mut fallback = EncoderFallback::new_with_hw(
            video_codecs::static_encoder_chain("no-such-codec"),
            HwDeviceContext { vaapi_device: None, vulkan_available: false },
        );
        let mut total = 0;
        run_video_to_video(&mut settings, "mkv", &mut fallback, &report, &mut total);

        assert!(*report.failed.lock().unwrap(), "bogus codec must fail the run");
        assert!(
            fallback.failed().contains("no-such-codec"),
            "failed-encoder set records the bogus codec: {:?}",
            fallback.failed()
        );
    }

    /// Direct StepOutcome check: a chain whose only candidate fails encoder
    /// init ends in `StepOutcome::Exhausted` with the encoder recorded in
    /// the fallback's failed set.
    #[test]
    fn test_run_video_step_with_fallback_exhausted_reports_outcome() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let mut settings = fixture_video();
        settings.output_folder = dir.path().to_path_buf();
        let output = dir.path().join("out.mkv");
        let output_str = output.to_string_lossy().to_string();

        let report = TestReport::new();
        let mut fallback = EncoderFallback::new_with_hw(
            vec!["no-such-encoder".into()],
            HwDeviceContext { vaapi_device: None, vulkan_available: false },
        );
        let mut build_args = |s: &ConverterSettings| vec![
            "-f".to_string(), "lavfi".to_string(),
            "-i".to_string(), "testsrc=size=64x64:rate=10".to_string(),
            "-t".to_string(), "0.1".to_string(),
            "-c:v".to_string(), s.resolved_video_encoder.clone(),
            output_str.clone(),
        ];

        let outcome = run_video_step_with_fallback(
            &mut settings,
            &mut fallback,
            &mut build_args,
            &output,
            &report,
            1,
            1,
        );

        assert_eq!(outcome, StepOutcome::Exhausted);
        assert!(fallback.failed().contains("no-such-encoder"));
        assert!(*report.failed.lock().unwrap(), "exhausted chain marks the run failed");
    }

    /// Partial failure still completes, with every attempted step recorded
    /// exactly once in the FailureLedger (asserted structurally via the
    /// report seam, not by parsing the human-facing summary wording).
    #[test]
    fn test_run_metadata_only_partial_failure_completes_with_warning() {
        if skip_if_no_ffmpeg() { return; }
        let dir = tempfile::TempDir::new().unwrap();
        let clip = dir.path().join("clip1.mp4");
        crate::converter::test_fixtures::create_test_video_with_tone(&clip, 1.0);

        let mut settings = make_video_settings();
        settings.pipeline = ConversionPipeline::MetadataOnly;
        settings.input_files = vec![clip, PathBuf::from("/nonexistent/clip2.mp4")];
        settings.output_folder = dir.path().to_path_buf();
        settings.timecode_meta_per_file = vec![tc_meta(), tc_meta()];

        let report = TestReport::new();
        let total = settings.input_files.len() * 3;
        run_metadata_only(&settings, &report, total);

        assert!(!*report.failed.lock().unwrap(), "partial failure completes");
        // Probe, tag, and rename all fail for the nonexistent clip → exactly
        // 3 recorded step failures; an exact count is correct here because
        // these failures *are* the specified behavior.
        assert_eq!(report.failures().len(), 3);
    }
}