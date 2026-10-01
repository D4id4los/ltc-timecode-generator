use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use audio_core::{AudioCore, AudioEvent, DecodeConfig, DecodeProgress, LtcDetectionResult, WavChunkReader};
use log::{error, info, warn};

use crate::command::{ConverterCommand, GuiCommand};
use crate::config;
use crate::converter::{
    query_ffmpeg_capabilities,
    FfmpegCapabilities, ChannelMap, ConversionPipeline, ConverterSettings,
    RecordingType, duplicate_output_names, duplicate_output_warning, evaluate_readiness,
    output_collision_warning, preview_output_files, apply_available_defaults,
    spawn_conversion_job,
};
use crate::ffprobe::{self, VideoAudioProbe};
use crate::job::{self, JobEvent, JobFinal, JobItem, JobKind, JobOutcome, JobStatus, JobSupervisor, ProgressSnapshot, spawn_job};
use crate::offload::{DeviceNameSource, OffloadDeviceTotals, run_offload_copy_job, run_offload_scan_job};
use crate::state::{AppStateSnapshot, ClapLogItem, ClipDecodeState};
use crate::timecode;

const TICK_INTERVAL: Duration = Duration::from_millis(40);
const TARGET_ARM_ANGLE: f32 = -25.0 * std::f32::consts::PI / 180.0;
const ARM_SETTLE_EPS: f32 = 1.0 * std::f32::consts::PI / 180.0; // 1 degree
const MAX_RECOVERY_ATTEMPTS: u8 = 3;
const MAX_CLAP_LOGS: usize = 1000;
const BUFFER_SIZE: u32 = 0;



pub fn engine_main(cmd_rx: Receiver<GuiCommand>, state: Arc<ArcSwap<AppStateSnapshot>>, use_libltc: bool) {
    engine_main_with_probe(cmd_rx, state, use_libltc, query_ffmpeg_capabilities)
}

/// Like [`engine_main`] but accepts an injectable capability-probe function
/// for testing.
pub fn engine_main_with_probe<F>(
    cmd_rx: Receiver<GuiCommand>,
    state: Arc<ArcSwap<AppStateSnapshot>>,
    use_libltc: bool,
    probe_fn: F,
) where
    F: FnOnce() -> FfmpegCapabilities + Send + 'static,
{
    let mut current = AppStateSnapshot::initial();

    // Seed persisted paths into the engine snapshot (output folder,
    // offload parent dir). Input folder is restored by the GUI sending
    // SelectFolder during startup.
    let saved_cfg = config::load();
    config::seed_snapshot_from_config(&mut current, &saved_cfg);

    let core = AudioCore::new();
    let mut last_tick = Instant::now();

    let mut recovery_attempts: u8 = 0;
    let mut log_id_counter: u64 = 0;
    let mut last_device_id: Option<String> = None;
    let mut previous_device: Option<String> = None;

    // Engine-internal auto-apply latches — user un-ticks survive decode re-runs
    let mut last_auto_applied_ltc_gen: u64 = 0;
    let mut last_auto_applied_group_ltc_gen: u64 = 0;

    // Deferred SelectRecording while a folder scan is still in flight
    // (set by SelectRecording, cleared by SelectFolder, applied in step 0
    // when the scan result arrives).
    let mut pending_recording: Option<usize> = None;

    // Job supervisor — single channel for all async task result events
    let mut supervisor = JobSupervisor::new();

    // Publish-gating: last snapshot stored into the ArcSwap.  Idle ticks
    // produce a byte-identical snapshot, so the deep clone + store is
    // skipped until something actually changes.
    let mut last_published: Option<Arc<AppStateSnapshot>> = None;

    // Spawn the ffmpeg capability probe on a background thread (via job supervisor)
    {
        let spec = job::JobSpec {
            kind: JobKind::FfmpegCapProbe,
            name: "ffmpeg-probe",
            units: Vec::new(),
        };
        spawn_job::<JobFinal, _>(&mut supervisor, spec, move |ctx| {
            ctx.progress.set_indeterminate(true);
            let caps = probe_fn();
            Ok(JobFinal::FfmpegCaps { caps: Some(caps) })
        });
    }

    'engine: loop {
        let now = Instant::now();
        let dt = (now - last_tick).as_secs_f32();
        last_tick = now;

        // 1. Drain all pending commands
        loop {
            match cmd_rx.try_recv() {
                Ok(GuiCommand::Shutdown) => {
                    let _ = core.stop_ltc();
                    let _ = core.stop_output();
                    info!("Engine shutdown via Shutdown command");
                    break 'engine;
                }
                Ok(GuiCommand::ProbeFileDurations(paths)) => {
                    if supervisor.is_running(JobKind::DurationProbe) {
                        info!("Duration probe already in progress — ignoring duplicate ProbeFileDurations");
                        continue;
                    }
                    current.file_durations.clear();
                    let spec = job::JobSpec {
                        kind: JobKind::DurationProbe,
                        name: "duration-probe",
                        units: vec![job::UnitSpec { weight: 1.0, label: "duration probe".into() }],
                    };
                    spawn_job::<JobFinal, _>(&mut supervisor, spec, move |ctx| {
                        ctx.progress.set_indeterminate(true);
                        for path in paths {
                            if ctx.cancel.is_cancelled() {
                                return Ok(JobFinal::DurationsDone);
                            }
                            let secs = crate::duration::file_duration_secs(&path);
                            ctx.emit(JobItem::DurationResult { path, secs });
                        }
                        Ok(JobFinal::DurationsDone)
                    });
                }
                Ok(GuiCommand::Converter(ConverterCommand::SelectFolder(path))) => {
                    current.converter.groups.clear();
                    current.converter.groups_folder = Some(path.clone());
                    pending_recording = None; // new scan invalidates any deferred selection
                    current.converter.selected_group_idx = None;
                    current.converter.probes.clear();
                    current.converter.probes_generation = 0;
                    current.jobs.entry(JobKind::Conversion).or_insert_with(JobStatus::idle);
                    current.jobs.entry(JobKind::FolderScan).or_insert_with(JobStatus::idle);
                    current.jobs.entry(JobKind::ClipProbe).or_insert_with(JobStatus::idle);
                    // Fresh scan — reset user-set flag so the next recording
                    // selection re-defaults output_folder to the record's
                    // parent dir.
                    current.converter.settings.output_folder_user_set = false;
                    // Persist input folder
                    config::save_input_folder(&path);
                    let scan_path = path.clone();
                    let spec = job::JobSpec {
                        kind: JobKind::FolderScan,
                        name: "folder-scan",
                        units: Vec::new(),
                    };
                    spawn_job::<JobFinal, _>(&mut supervisor, spec, move |ctx| {
                        ctx.progress.set_indeterminate(true);
                        let groups = crate::file_pattern::match_files_all_patterns(&scan_path);
                        info!(
                            "Folder scan complete: {} — {} group(s) matched",
                            scan_path.display(),
                            groups.len(),
                        );
                        Ok(JobFinal::FolderScan { path: scan_path, groups })
                    });
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SelectRecording(idx))) => {
                    if supervisor.is_running(JobKind::FolderScan) {
                        info!(
                            "SelectRecording({}) deferred — folder scan still in progress",
                            idx,
                        );
                        pending_recording = Some(idx);
                    } else {
                        apply_recording_selection(
                            &mut current, idx,
                            &mut supervisor,
                            &mut last_auto_applied_ltc_gen,
                            &mut last_auto_applied_group_ltc_gen,
                        );
                    }
                }
                Ok(GuiCommand::Offload(cmd)) => {
                    handle_offload_command(cmd, &mut current, &mut supervisor);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetMetadataOnly(val)))=> {
                    current.converter.settings.metadata_only = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetGenerateSyntheticVideo(val)))=> {
                    current.converter.settings.generate_synthetic_video = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetCopyVideo(val)))=> {
                    current.converter.settings.copy_video = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetSplitTracks(val)))=> {
                    current.converter.settings.split_tracks = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetDropLtcTrack(val)))=> {
                    current.converter.settings.drop_ltc_track = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetConcatAudio(val)))=> {
                    current.converter.settings.concat_audio = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetStartFromLtc(val)))=> {
                    current.converter.settings.set_start_from_ltc = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetEmbedCameraMetadata(val)))=> {
                    current.converter.settings.embed_camera_metadata = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetLtcFileIndex(val)))=> {
                    current.converter.settings.ltc_file_idx = val;
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SwapChannelMapCells(row, col)))=> {
                    current.converter.settings.channel_map.swap(row, col);
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetContainer(container))) => {
                    current.converter.settings.container = container.clone();
                    // Re-select best defaults for the new container
                    if let Some(ref caps) = current.ffmpeg_caps {
                        apply_available_defaults(
                            &mut current.converter.settings.container,
                            &mut current.converter.settings.video_encoder,
                            &mut current.converter.settings.audio_encoder,
                            caps,
                        );
                    }
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetVideoCodec(codec))) => {
                    current.converter.settings.video_encoder = codec.clone();
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetAudioEncoder(encoder))) => {
                    current.converter.settings.audio_encoder = encoder.clone();
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetOutputFolder(folder))) => {
                    apply_set_output_folder(&mut current, folder);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetFilenamePrefix(prefix))) => {
                    current.converter.settings.filename_prefix = prefix.clone();
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetAudioSuffixTemplate(tmpl))) => {
                    current.converter.settings.audio_suffix_template = tmpl.clone();
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::SetVideoSuffixTemplate(tmpl))) => {
                    current.converter.settings.video_suffix_template = tmpl.clone();
                    recompute_converter_derived(&mut current);
                }
                Ok(GuiCommand::Converter(ConverterCommand::StartConversion)) => {
                    if current.job(JobKind::Conversion).phase() == job::JobPhase::Running {
                        warn!("Conversion already in progress — ignoring duplicate StartConversion");
                    } else if let Some(settings) = assemble_converter_settings(&current) {
                        let caps = current.ffmpeg_caps.clone();
                        // Cancel any previous conversion job first
                        supervisor.cancel(JobKind::Conversion);
                        let spec = job::JobSpec {
                            kind: JobKind::Conversion,
                            name: "conversion",
                            units: vec![job::UnitSpec { weight: 1.0, label: "conversion".into() }],
                        };
                        spawn_job::<JobFinal, _>(&mut supervisor, spec, move |ctx| {
                            spawn_conversion_job(ctx, settings, caps)
                        });
                        // Immediately reflect running state in snapshot
                        let mut status = JobStatus::idle();
                        status.progress.phase = job::JobPhase::Running;
                        status.progress.message = "Conversion started…".to_string();
                        current.jobs.insert(JobKind::Conversion, status);
                        current.status.set_converter("Conversion started…");
                        info!("Conversion started via engine StartConversion command (job-based)");
                    } else {
                        let msg = "Cannot start conversion — no recording group selected or settings incomplete".to_string();
                        current.status.set_converter(msg.clone());
                        warn!("{}", msg);
                    }
                }
                Ok(GuiCommand::Converter(ConverterCommand::CancelConversion)) => {
                    supervisor.cancel(JobKind::Conversion);
                    if let Some(status) = current.jobs.get_mut(&JobKind::Conversion) {
                        status.progress.phase = job::JobPhase::Cancelled;
                        status.error = Some("Cancelled by user".to_string());
                    }
                    current.status.set_converter("Conversion canceled");
                    info!("Conversion cancel signaled via engine CancelConversion command (job-based)");
                }
                Ok(GuiCommand::ProbeVideo(path)) => {
                    if supervisor.is_running(JobKind::VideoProbe) {
                        info!("Video probe already in progress — ignoring duplicate ProbeVideo");
                        continue;
                    }
                    info!("Probing video file for audio streams (async via job): {}", path);
                    current.decode.probe = None;
                    current.decode.error = None;
                    current.status.set_decode(format!("Probing video: {}", path));
                    let path_clone = path.clone();
                    let spec = job::JobSpec {
                        kind: JobKind::VideoProbe,
                        name: "video-probe",
                        units: Vec::new(),
                    };
                    spawn_job::<JobFinal, _>(&mut supervisor, spec, move |ctx| {
                        ctx.progress.set_indeterminate(true);
                        let result = crate::ffprobe::probe_video_audio(Path::new(&path_clone))
                            .map_err(|e| e.to_string());
                        Ok(JobFinal::VideoProbe { result })
                    });
                }
                Ok(cmd) => {
                    process_command(
                        cmd,
                        &core,
                        use_libltc,
                        &mut current,
                        &mut recovery_attempts,
                        &mut log_id_counter,
                        &mut last_device_id,
                        &mut previous_device,
                        &mut supervisor,
                    );
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    info!("Engine shutdown via channel disconnect");
                    let _ = core.stop_ltc();
                    let _ = core.stop_output();
                    break 'engine;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
            }
        }

        // 1.4 Poll supervisor for finished jobs and drain events — unified dispatcher
        //     for all async job results that were started via spawn_job.
        let progress_snapshots = supervisor.poll();
        for (_id, kind, snap) in &progress_snapshots {
            // Only overwrite job status from poll if the current phase is not
            // a terminal state (Succeeded/Cancelled/Failed). The poll snapshot
            // always shows Running; the terminal phase comes from the Finished
            // event which apply_outcome sets, and must not be clobbered if the
            // thread hasn't finished yet but the event already arrived.
            let is_terminal = current.jobs.get(kind)
                .map(|s| matches!(s.phase(), job::JobPhase::Succeeded | job::JobPhase::Cancelled | job::JobPhase::Failed))
                .unwrap_or(false);
            if !is_terminal {
                current.jobs.insert(*kind, JobStatus::from_progress(snap));
            }
        }
        for event in supervisor.drain() {
            // Stale-result gating: reject events from superseded jobs by
            // comparing the event's JobId to the most-recently-spawned one
            // for that kind (tracked in supervisor.latest_job).
            let (event_job, event_kind) = match &event {
                JobEvent::Finished { job, kind, .. } => (*job, *kind),
                JobEvent::Item { job, kind, .. } => (*job, *kind),
            };
            if supervisor.latest_job.get(&event_kind) != Some(&event_job) {
                continue;
            }
            match event {
                JobEvent::Finished { kind: JobKind::Conversion, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::Conversion) {
                        status.apply_outcome(&outcome);
                    }
                    match outcome {
                        JobOutcome::Succeeded { .. } => {
                            recompute_converter_derived(&mut current);
                            current.status.set_converter("Conversion completed");
                            info!("Engine-owned conversion completed successfully (job)");
                        }
                        JobOutcome::Cancelled { .. } => {
                            recompute_converter_derived(&mut current);
                            current.status.set_converter("Conversion canceled");
                            info!("Engine-owned conversion cancelled (job)");
                        }
                        JobOutcome::Failed { error, .. } => {
                            if let Some(status) = current.jobs.get_mut(&JobKind::Conversion) {
                                status.error = Some(error.clone());
                            }
                            recompute_converter_derived(&mut current);
                            current.status.set_converter(format!("Conversion failed: {}", error));
                            warn!("Engine-owned conversion failed: {}", error);
                        }
                    }
                }
                JobEvent::Finished { kind: JobKind::FfmpegCapProbe, payload: JobFinal::FfmpegCaps { caps }, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::FfmpegCapProbe) {
                        status.apply_outcome(&outcome);
                    }
                    if let Some(caps) = caps {
                        apply_ffmpeg_probe_result(&mut current, caps);
                    } else {
                        warn!("FFmpeg capability probe returned no caps");
                        recompute_converter_derived(&mut current);
                    }
                }
                JobEvent::Finished { kind: JobKind::FolderScan, payload: JobFinal::FolderScan { path, groups }, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::FolderScan) {
                        status.apply_outcome(&outcome);
                    }
                    if Some(&path) == current.converter.groups_folder.as_ref() {
                        current.converter.groups = groups;
                        info!("Folder scan complete: {} group(s)", current.converter.groups.len());
                        if let Some(idx) = pending_recording.take() {
                            info!("Applying deferred SelectRecording({}) after folder scan", idx);
                            apply_recording_selection(
                                &mut current, idx,
                                &mut supervisor,
                                &mut last_auto_applied_ltc_gen,
                                &mut last_auto_applied_group_ltc_gen,
                            );
                        }
                    } else {
                        warn!("Discarding stale supervisor folder scan result (path mismatch)");
                    }
                }
                JobEvent::Finished { kind: JobKind::VideoProbe, payload: JobFinal::VideoProbe { result }, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::VideoProbe) {
                        status.apply_outcome(&outcome);
                    }
                    match result {
                        Ok(probe) => {
                            current.decode.probe = Some(probe.clone());
                            current.decode.selected_stream = 0;
                            current.decode.selected_channel = 0;
                            current.decode.error = None;
                            current.status.set_decode(format!(
                                "Video probed: {} audio stream(s), {} total channel(s)",
                                probe.streams.len(),
                                probe.total_audio_channels,
                            ));
                            info!("Video probe succeeded (job): {} streams, {} channels", probe.streams.len(), probe.total_audio_channels);
                        }
                        Err(e) => {
                            current.decode.probe = None;
                            current.decode.error = Some(e.clone());
                            current.status.set_decode(format!("Video probe failed: {}", e));
                            error!("Video probe failed (job): {}", e);
                        }
                    }
                }
                JobEvent::Finished { kind: JobKind::OffloadScan, payload: JobFinal::OffloadScan { mut cards }, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::OffloadScan) {
                        status.apply_outcome(&outcome);
                    }
                    // Apply default selection (latest recording day) to each card.
                    let file_paths: Vec<PathBuf> = cards.iter()
                        .flat_map(|c| c.files.iter().map(|f| f.path.clone()))
                        .collect();
                    for card in &mut cards {
                        let sel = crate::offload::default_selection(&card.files);
                        crate::offload::apply_selection(card, sel);
                    }
                    current.offload.cards = cards;
                    current.offload.file_durations.clear();
                    current.offload.durations_version =
                        current.offload.durations_version.wrapping_add(1);

                    if current.offload.cards.is_empty() {
                        info!(
                            "Offload card scan complete: 0 cards — if your card reader \
                             is connected, check that the card is mounted and has \
                             video/audio files"
                        );
                    } else {
                        info!(
                            "Offload card scan complete: {} card(s)",
                            current.offload.cards.len()
                        );
                        // Spawn async duration probe for all scanned files
                        let spec = job::JobSpec {
                            kind: JobKind::DurationProbe,
                            name: "offload-dur-probe",
                            units: Vec::new(),
                        };
                        spawn_job::<JobFinal, _>(&mut supervisor, spec, move |ctx| {
                            ctx.progress.set_indeterminate(true);
                            for path in file_paths {
                                if ctx.cancel.is_cancelled() {
                                    break;
                                }
                                let secs = crate::duration::file_duration_secs(&path);
                                ctx.emit(job::JobItem::DurationResult { path, secs });
                            }
                            Ok(JobFinal::DurationsDone)
                        });
                    }
                }
                JobEvent::Item { kind: JobKind::DurationProbe, item: JobItem::DurationResult { path, secs }, .. } => {
                    // Write to converter file_durations
                    current.file_durations.insert(path.clone(), secs);
                    // Write to offload file_durations
                    current.offload.file_durations.insert(path, secs);
                    current.offload.durations_version =
                        current.offload.durations_version.wrapping_add(1);
                }
                JobEvent::Finished { kind: JobKind::DurationProbe, payload: JobFinal::DurationsDone, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::DurationProbe) {
                        status.apply_outcome(&outcome);
                    }
                    info!("Duration probe complete");
                }
                JobEvent::Finished { kind: JobKind::OffloadCopy, payload: JobFinal::OffloadCopy { completed_devices }, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::OffloadCopy) {
                        status.apply_outcome(&outcome);
                    }
                    let was_cancelled = matches!(&outcome, JobOutcome::Cancelled { .. });
                    if was_cancelled {
                        info!(
                            "Offload copy was cancelled by user: {} device(s) completed",
                            completed_devices.len(),
                        );
                        current.offload.error = Some("Canceled by user".to_string());
                        current.status.set_offload("Offload canceled");
                    } else {
                        info!(
                            "Offload copy complete: {} device(s) offloaded of {}",
                            completed_devices.len(),
                            current.offload.device_totals.len(),
                        );
                        let parent = current.offload.parent_folder.clone();
                        for name in &completed_devices {
                            if !current.offload.completed_devices.contains(name) {
                                current.offload.completed_devices.push(name.clone());
                            }
                        }
                        let target = parent.map(|p| p.join(&current.offload.parent_name));
                        current.offload.last_offload_parent = target;
                        current.offload.last_offload_version =
                            current.offload.last_offload_version.wrapping_add(1);
                        current.status.set_offload(format!(
                            "Offload complete: {} device(s) copied",
                            completed_devices.len(),
                        ));
                    }
                }
                JobEvent::Finished { kind: JobKind::LtcDecode, payload: JobFinal::Decode { result, path }, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::LtcDecode) {
                        status.apply_outcome(&outcome);
                    }
                    let generation = current.decode.generation;
                    match result {
                        Ok(r) => {
                            let first_offset = r.first_ltc_timecode_secs;
                            info!(
                                "LTC decode result: path={}, fps={:.2}, valid={}/{}, \
                                 confidence={:.1}%, first_ltc_timecode_secs={:.3}s",
                                path.display(), r.detected_fps, r.valid_frames, r.total_possible_frames,
                                r.avg_confidence * 100.0, first_offset,
                            );
                            current.decode.result = Some(r.clone());
                            current.decode.error = None;
                            let summary = format!(
                                "LTC decode: {} frames (confidence {:.1}%, {} fps{})",
                                r.valid_frames, r.avg_confidence * 100.0,
                                r.detected_fps, if r.drop_frame { " DF" } else { "" },
                            );
                            current.status.set_decode(summary);
                            if generation > last_auto_applied_ltc_gen {
                                auto_apply_ltc_to_settings(&mut current, &path.to_string_lossy());
                                last_auto_applied_ltc_gen = generation;
                                recompute_converter_derived(&mut current);
                            }
                        }
                        Err(e) => {
                            let is_cancel = e == "Decode canceled by user";
                            current.decode.result = None;
                            current.decode.error = if is_cancel { None } else { Some(e.clone()) };
                            current.status.set_decode(if is_cancel {
                                "Decode canceled".to_string()
                            } else {
                                format!("Parse failed: {}", e)
                            });
                            if !is_cancel {
                                error!("LTC decode failed: {} — {}", path.display(), e);
                            }
                        }
                    }
                }
                JobEvent::Item { kind: JobKind::LtcGroupDecode, item: JobItem::ClipLtcResult { index, result }, .. } => {
                    if current.decode.group_results.len() > index {
                        current.decode.group_results[index] = ClipDecodeState::Done(result);
                        let done = current.decode.group_results.iter().filter(|r| r.is_done()).count();
                        current.status.set_decode(format!(
                            "Decoding group: {}/{} clips",
                            done, current.decode.group_results.len(),
                        ));
                        if let ClipDecodeState::Done(Ok(r)) = &current.decode.group_results[index] {
                            info!(
                                "LTC group decode [{}/{}]: {} frames (confidence {:.1}%)",
                                done, current.decode.group_results.len(),
                                r.valid_frames, r.avg_confidence * 100.0,
                            );
                        } else if let ClipDecodeState::Done(Err(e)) = &current.decode.group_results[index] {
                            warn!("LTC group decode [{}/{}]: failed: {}",
                                done, current.decode.group_results.len(), e);
                        }
                    }
                }
                JobEvent::Finished { kind: JobKind::LtcGroupDecode, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::LtcGroupDecode) {
                        status.apply_outcome(&outcome);
                    }
                    // When the user cancelled and a new decode was spawned,
                    // this event is already rejected by the latest_job gate above.
                    let total = current.decode.group_results.len();
                    let successes = current.decode.group_results.iter().filter(|r| r.ok().is_some()).count();
                    let failures = total - successes;
                    let gen = current.decode.group_generation;
                    let tc_info = if successes > 0 {
                        if let Some(ClipDecodeState::Done(Ok(r))) = current.decode.group_results.first() {
                            format!(
                                "{} clips decoded ({} ok, {} fail) — {} fps{}",
                                total, successes, failures,
                                r.detected_fps, if r.drop_frame { " DF" } else { "" },
                            )
                        } else {
                            format!("{} clips decoded ({} ok, {} fail)", total, successes, failures)
                        }
                    } else {
                        format!("Group decode complete (all {} clips failed)", failures)
                    };
                    current.status.set_decode(tc_info);
                    info!("LTC group decode complete: {}/{} ok, {}/{} failed",
                        successes, total, failures, total);
                    if gen > last_auto_applied_group_ltc_gen {
                        auto_apply_group_ltc_to_settings(&mut current);
                        last_auto_applied_group_ltc_gen = gen;
                        recompute_converter_derived(&mut current);
                    }
                }
                JobEvent::Finished { kind: JobKind::ClipProbe, payload: JobFinal::ClipProbes { probes, cameras, device_name }, outcome, .. } => {
                    if let Some(status) = current.jobs.get_mut(&JobKind::ClipProbe) {
                        status.apply_outcome(&outcome);
                    }
                    let probe_generation = current.converter.probes_generation;
                    if probe_generation > 0 {
                        if let Some(ref idx) = current.converter.selected_group_idx {
                            if let Some(group) = current.converter.groups.get(*idx) {
                                if group.recording_type == RecordingType::VideoClipSequence {
                                    if let Some(Ok(ref probe)) = probes.iter().find(|r| r.is_ok()) {
                                        current.decode.probe = Some(probe.clone());
                                        current.decode.selected_stream = 0;
                                        current.decode.selected_channel = 0;
                                        current.decode.error = None;
                                    } else {
                                        let err = probes.iter().find_map(|r| {
                                            if let Err(ref e) = r { Some(e.clone()) } else { None }
                                        }).unwrap_or_else(|| "No audio streams detected.".to_string());
                                        current.decode.error = Some(format!("LTC source probe failed: {}", err));
                                        current.status.set_decode(format!("Video probe failed: {}", err));
                                    }
                                }
                            }
                        }
                        current.converter.probes = probes.iter().map(|r| r.as_ref().ok().cloned()).collect();
                        current.converter.camera_meta = cameras;
                        current.converter.device_name = device_name;
                        info!("Converter clip probe complete: {} files", current.converter.probes.len());
                        let is_video_group = current.converter.selected_group_idx
                            .and_then(|i| current.converter.groups.get(i))
                            .map(|g| g.recording_type == RecordingType::VideoClipSequence)
                            .unwrap_or(false);
                        if is_video_group {
                            let probe_channels = current.converter.probes.first()
                                .and_then(|p| p.as_ref())
                                .map(|p| p.total_audio_channels);
                            let map_channels = current.converter.settings.channel_map.num_channels();
                            if let Some(ch) = probe_channels {
                                if ch > 0 && ch != map_channels {
                                    current.converter.settings.channel_map = ChannelMap::identity(ch);
                                }
                            }
                        }
                        recompute_converter_derived(&mut current);
                    }
                }
                _ => {}
            }
        }

        // 2. Poll current timecode if playing
        if current.is_playing {
            current.current_timecode = core.current_timecode();
        }

        // 3. Drain events from AudioCore
        // Clear previous-tick events first so the GUIs only see each event once.
        current.events.clear();
        for event in core.drain_events() {
            handle_event(event, &core, &mut current, &mut recovery_attempts, &mut last_device_id);
        }

        // 4. Animation: flash alpha decays at 2.0/s
        if current.clapper.flash_alpha > 0.0 {
            current.clapper.flash_alpha = (current.clapper.flash_alpha - dt * 2.0).max(0.0);
        }

        // 5. Animation: arm angle exponential decay toward rest position at 4.0/s
        current.clapper.arm_angle += (TARGET_ARM_ANGLE - current.clapper.arm_angle)
            * (1.0 - (-4.0 * dt).exp());

        // 5.5 Determine whether clap animation is still visibly in progress.
        // The GUI uses this flag to decide whether to render at 60 fps
        // (for smooth animation) or to throttle to a lower rate.
        current.clapper.animating = current.clapper.flash_alpha > 0.0
            || (current.clapper.arm_angle - TARGET_ARM_ANGLE).abs() > ARM_SETTLE_EPS;

        // 6. Publish state — only when the snapshot actually changed
        let next = Arc::new(current.clone());
        if last_published.as_ref().map_or(true, |p| next.as_ref() != p.as_ref()) {
            state.store(next.clone());
            last_published = Some(next);
        }

        // 7. Sleep until next tick
        let next_tick = last_tick + TICK_INTERVAL;
        if let Some(sleep_dur) = next_tick.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep_dur);
        }
    }

    supervisor.shutdown(Duration::from_secs(2));
}

/// Extract a single audio channel from a video file and decode LTC from it.
/// Returns the LTC detection result or an error string.
///
/// `cancel` — shared atomic bool checked during extraction and decode.
/// `on_extract_progress` — called during extraction with fraction 0.0..1.0.
/// `decode_unit` — if `Some`, a progress bridge thread is spawned during the
/// chunked decode phase to report "Chunk {done}/{total}" progress into this
/// unit. Pass `None` to skip bridging (e.g. when the caller has no unit, or
/// when the caller will bridge externally).
fn decode_one_video_clip<F: Fn(f32)>(
    path: &str,
    stream_index: usize,
    channel_index: usize,
    use_libltc: bool,
    decode_fps: f64,
    decode_drop_frame: bool,
    capture_gen: u64,
    cancel: &Arc<AtomicBool>,
    on_extract_progress: &F,
    decode_unit: Option<job::UnitProgress>,
) -> Result<LtcDetectionResult, String> {
    static TMP_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let counter = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pipeline_start = Instant::now();

    let tmp_dir = std::env::temp_dir();
    let tmp_wav = tmp_dir.join(format!(
        "ltc_extract_{}_{}_{}_{}_{}.wav",
        std::process::id(),
        capture_gen,
        stream_index,
        channel_index,
        counter,
    ));

    // Phase 1: audio extraction via ffmpeg
    let duration = ffprobe::probe_stream_duration_secs(Path::new(&path), stream_index);

    ffprobe::extract_audio_channel_with_progress(
        Path::new(&path),
        stream_index,
        channel_index,
        &tmp_wav,
        duration,
        Some(Arc::as_ref(cancel)),
        on_extract_progress,
    ).map_err(|e| format!("Audio extraction failed: {}", e))?;

    // Check cancel after extraction, before decode
    if cancel.load(Ordering::Relaxed) {
        let _ = std::fs::remove_file(&tmp_wav);
        return Err("Decode canceled by user".to_string());
    }

    let wav_path = tmp_wav.clone();

    // Phase 2: chunked LTC decode from extracted WAV
    let chunks_done = Arc::new(AtomicUsize::new(0));
    let result = match WavChunkReader::open(&wav_path) {
        Ok((reader, _start)) => {
            let total_mono = reader.total_mono_samples();
            let sr = reader.sample_rate();
            let ch = reader.channels() as u16;
            let bps = reader.spec().bits_per_sample;
            drop(reader);

            let config = DecodeConfig::default();
            let chunk_count = audio_core::count_chunks(total_mono, sr, ch, bps, &config);

            if chunk_count <= 1 || total_mono == 0 {
                let result = audio_core::decode_ltc_with_decoder(
                    &wav_path, use_libltc, decode_fps, decode_drop_frame, Some(Arc::as_ref(cancel)),
                );
                chunks_done.store(1, Ordering::Relaxed);
                result
            } else {
                let decode_progress = DecodeProgress {
                    chunks_total: chunk_count,
                    chunks_completed: chunks_done.clone(),
                    cancel_flag: cancel.clone(),
                };

                let bridge = decode_unit.as_ref().map(|unit| {
                    bridge_decode_progress(decode_progress.clone(), unit.clone(), None)
                });

                let result = audio_core::decode_ltc_chunked(
                    &wav_path, use_libltc, decode_fps, decode_drop_frame,
                    config, &decode_progress,
                );

                if let Some(h) = bridge {
                    let _ = h.join();
                }

                result
            }
        }
        Err(e) => Err(format!("Failed to open extracted WAV: {}", e)),
    };

    // Stamp total pipeline time (extraction + decode) onto the result
    let result = result.map(|mut r| {
        r.processing_time_ms = pipeline_start.elapsed().as_secs_f64() * 1000.0;
        r
    });

    let _ = std::fs::remove_file(&tmp_wav);
    result
}

/// Bridge a `DecodeProgress` (from audio-core chunked decode) to a
/// `UnitProgress` by polling `chunks_completed` on a short-lived helper
/// thread.  Returns a `JoinHandle` the caller should join before the unit
/// finishes.
fn bridge_decode_progress(
    dp: DecodeProgress,
    unit: job::UnitProgress,
    msg_prefix: Option<String>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("dp-bridge".into())
        .spawn(move || {
            loop {
                let done = dp.chunks_completed.load(Ordering::Relaxed);
                let total = dp.chunks_total;
                if total > 0 {
                    unit.set_fraction(done as f32 / total as f32);
                    let msg = match &msg_prefix {
                        Some(prefix) => format!("{} — Chunk {}/{}", prefix, done.min(total), total),
                        None => format!("Chunk {}/{}", done.min(total), total),
                    };
                    unit.set_message(msg);
                }
                if done >= total || dp.cancel_flag.load(Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
        .expect("failed to spawn decode-progress bridge thread")
}

#[allow(clippy::too_many_arguments)]
fn process_command(
    cmd: GuiCommand,
    core: &AudioCore,
    use_libltc: bool,
    state: &mut AppStateSnapshot,
    recovery_attempts: &mut u8,
    log_id_counter: &mut u64,
    last_device_id: &mut Option<String>,
    previous_device: &mut Option<String>,
    supervisor: &mut JobSupervisor,
) {
    match cmd {
        GuiCommand::StartLtc => {
            ensure_audio_init(core, state, last_device_id, recovery_attempts);
            if !state.audio_initialized {
                state.status.set_audio("Cannot start — audio not initialized");
                return;
            }
            match core.start_ltc(
                state.start_timecode,
                state.fps(),
                state.drop_frame(),
                state.ltc_channel,
                state.ltc_volume,
            ) {
                Ok(()) => {
                    state.is_playing = true;
                    state.status.set_audio("Streaming LTC");
                    state.current_timecode = state.start_timecode;
                }
                Err(e) => {
                    error!("Failed to start LTC: {}", e);
                    state.status.set_audio(format!("Start failed: {}", e));
                }
            }
        }

        GuiCommand::StopLtc => {
            let _ = core.stop_ltc();
            state.is_playing = false;
            state.status.set_audio("Stopped");
        }

        GuiCommand::Reset => {
            let _ = core.reset_ltc(state.start_timecode);
            state.current_timecode = state.start_timecode;
            state.status.set_audio("Reset");
        }

        GuiCommand::ToggleLock => {
            state.is_locked = !state.is_locked;
        }

        GuiCommand::Clap => {
            let _ = core.play_beep(
                state.sample_rate,
                state.beep_frequency,
                state.beep_duration,
                state.beep_volume,
                state.beep_channel,
            );
            state.clapper.flash_alpha = 1.0;
            state.clapper.arm_angle = 0.0;

            let tc_str = timecode::timecode_to_string(state.current_timecode, state.drop_frame());
            let ms_str =
                timecode::timecode_to_ms_string(state.current_timecode, state.fps());
            *log_id_counter += 1;
            let ts = timecode::chrono_now_string();
            let note = format!("Scene {}", state.clapper.scene);

            state.clapper.logs.push(ClapLogItem {
                id: *log_id_counter,
                timestamp: ts,
                timecode: tc_str,
                milliseconds: ms_str,
                note,
            });
            while state.clapper.logs.len() > MAX_CLAP_LOGS {
                state.clapper.logs.remove(0);
            }

            if state.clapper.auto_increment_take {
                state.clapper.take = state.clapper.take.saturating_add(1);
            }
            state.status.set_audio("Clap!");
        }

        GuiCommand::SetStartTimecode(tc) => {
            state.start_timecode = tc;
        }

        GuiCommand::SetFpsIndex(index) => {
            if index < timecode::FPS_OPTIONS.len() {
                state.fps_index = index;
            }
        }

        GuiCommand::SetSampleRate(rate) => {
            state.sample_rate = rate;
        }

        GuiCommand::SetDevice(device_id) => {
            if state.is_playing {
                let _ = core.stop_ltc();
                state.is_playing = false;
            }

            let _ = core.stop_output();
            state.audio_initialized = false;

            *previous_device = state.selected_device.clone();
            state.selected_device = Some(device_id);

            if !try_init_device(core, state, last_device_id, recovery_attempts) {
                // Revert to previous device
                state.selected_device = previous_device.take();
                if try_init_device(core, state, last_device_id, recovery_attempts) {
                    state.status.set_audio("Device selection reverted to previous");
                } else {
                    // Previous selection may have been automatic (None);
                    // fall back to the default/first device.
                    ensure_audio_init(core, state, last_device_id, recovery_attempts);
                }
            }
        }

        GuiCommand::RefreshDevices => {
            match audio_core::list_audio_devices() {
                Ok(devices) => {
                    state.devices = devices;
                    // Drop the selection if the chosen device vanished;
                    // the next init falls back to the default/first device.
                    if let Some(ref id) = state.selected_device {
                        if !state.devices.iter().any(|d| &d.id == id) {
                            state.selected_device = None;
                        }
                    }
                    state.status.set_audio(format!("{} devices found", state.devices.len()));
                }
                Err(e) => {
                    error!("Failed to list devices: {}", e);
                    state.status.set_audio(format!("Device scan failed: {}", e));
                }
            }
        }

        GuiCommand::InitAudio => {
            ensure_audio_init(core, state, last_device_id, recovery_attempts);
        }

        GuiCommand::SetLtcChannel(ch) => {
            state.ltc_channel = ch;
        }
        GuiCommand::SetBeepChannel(ch) => {
            state.beep_channel = ch;
        }
        GuiCommand::SetLtcVolume(vol) => {
            state.ltc_volume = vol;
        }
        GuiCommand::SetBeepVolume(vol) => {
            state.beep_volume = vol;
        }
        GuiCommand::SetBeepFrequency(freq) => {
            state.beep_frequency = freq;
        }
        GuiCommand::SetBeepDuration(dur) => {
            state.beep_duration = dur;
        }
        GuiCommand::SetScene(scene) => {
            state.clapper.scene = scene;
        }
        GuiCommand::SetTake(take) => {
            state.clapper.take = take;
        }
        GuiCommand::SetRoll(roll) => {
            state.clapper.roll = roll;
        }
        GuiCommand::SetAutoIncrement(val) => {
            state.clapper.auto_increment_take = val;
        }
        GuiCommand::SetTheme(dark) => {
            state.is_dark_theme = dark;
        }
        GuiCommand::ToggleTheme => {
            state.is_dark_theme = !state.is_dark_theme;
        }
        GuiCommand::ClearLogs => {
            state.clapper.logs.clear();
        }

        GuiCommand::SetDecodeFpsIndex(index) => {
            if let Some(opt) = timecode::FPS_OPTIONS.get(index) {
                state.decode.fps_index = index;
                info!("Decode FPS set to: {} (index={})", opt.name, index);
            }
        }

        GuiCommand::CancelDecode => {
            supervisor.cancel(JobKind::LtcDecode);
            supervisor.cancel(JobKind::LtcGroupDecode);
            if let Some(status) = state.jobs.get_mut(&JobKind::LtcDecode) {
                status.progress.phase = job::JobPhase::Cancelled;
                status.progress.message = "Canceled by user".to_string();
            }
            if let Some(status) = state.jobs.get_mut(&JobKind::LtcGroupDecode) {
                status.progress.phase = job::JobPhase::Cancelled;
                status.progress.message = "Canceled by user".to_string();
            }
            state.status.set_decode("Decode canceled by user");
        }

        GuiCommand::DecodeLtcVideoGroup { paths, stream_index, channel_index } => {
            if paths.is_empty() {
                return;
            }
            if supervisor.is_running(JobKind::LtcGroupDecode) {
                info!("LTC group decode already in progress — ignoring duplicate DecodeLtcVideoGroup");
                return;
            }
            let total = paths.len();
            let decoder_name = if use_libltc { "libltc" } else { "builtin" };
            info!(
                "LTC group decode requested: {} clip(s), stream={}, channel={}, decoder={}, fps={}",
                total, stream_index, channel_index, decoder_name, state.decode_fps(),
            );

            // Reset group decode state with new generation
            state.decode.group_generation = state.decode.group_generation.wrapping_add(1);
            state.decode.group_paths = paths.iter().map(std::path::PathBuf::from).collect();
            state.decode.group_results = vec![ClipDecodeState::Pending; total];
            // Pre-populate job status so CancelDecode can eager-cancel before poll()
            state.jobs.insert(JobKind::LtcGroupDecode, job::JobStatus {
                progress: ProgressSnapshot {
                    phase: job::JobPhase::Running,
                    fraction: 0.0,
                    message: format!("Decoding LTC group: 0/{} clips", total),
                    speed: None,
                    units: Vec::new(),
                    log: String::new(),
                },
                error: None,
            });
            state.status.set_decode(format!("Decoding LTC group: 0/{} clips", total));

            let capture_gen = state.decode.group_generation;
            let decode_fps = state.decode_fps();
            let decode_drop_frame = state.decode_drop_frame();

            let spec = job::JobSpec {
                kind: JobKind::LtcGroupDecode,
                name: "ltc-group-decode",
                units: (0..total).map(|_| job::UnitSpec {
                    weight: 1.0 / total as f32,
                    label: "clip".into(),
                }).collect(),
            };
            spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
                let cancel = ctx.cancel.inner().clone();
                for (idx, path) in paths.iter().enumerate() {
                    if cancel.load(Ordering::Relaxed) {
                        info!("LTC group decode canceled at clip {}/{}", idx, total);
                        return Err(job::JobError::Cancelled);
                    }

                    let clip_unit = ctx.progress.unit(idx);
                    let clip_name = path.rsplit('/').next()
                        .or_else(|| path.rsplit('\\').next())
                        .unwrap_or(path);
                    clip_unit.set_label(clip_name.to_string());
                    clip_unit.set_message(format!("Clip {}/{}", idx + 1, total));

                    info!("LTC group decode clip {}/{} started: {}", idx + 1, total, path);
                    let result = decode_one_video_clip(
                        path, stream_index, channel_index,
                        use_libltc, decode_fps, decode_drop_frame, capture_gen,
                        &cancel, &|_| {},
                        Some(clip_unit),
                    ).map(Box::new);
                    if ctx.cancel.is_cancelled() {
                        return Err(job::JobError::Cancelled);
                    }
                    ctx.progress.unit(idx).finish();

                    info!("LTC group decode clip {}/{} finished: {} — {}",
                        idx + 1, total, path,
                        if result.is_ok() { "OK" } else { "FAILED" },
                    );

                    ctx.emit(JobItem::ClipLtcResult {
                        index: idx,
                        result,
                    });
                }
                info!("LTC group decode thread finished — all {} clips processed", total);
                Ok(JobFinal::NoPayload)
            });
        }

        GuiCommand::ClearRecordingDecodeState => {
            state.decode.result = None;
            state.decode.error = None;
            state.decode.probe = None;
            supervisor.cancel(JobKind::LtcDecode);
            state.jobs.insert(JobKind::LtcDecode, job::JobStatus::idle());
            state.decode.generation = state.decode.generation.wrapping_add(1);
            state.decode.group_paths = Vec::new();
            state.decode.group_results = Vec::new();
            supervisor.cancel(JobKind::LtcGroupDecode);
            state.jobs.insert(JobKind::LtcGroupDecode, job::JobStatus::idle());
            state.decode.group_generation = state.decode.group_generation.wrapping_add(1);
        }

        GuiCommand::ProbeVideo(_path) => {
            // Handled inline in engine_main's command drain — this arm is
            // never reached in practice; kept for match exhaustiveness.
            unreachable!("ProbeVideo is handled in the main loop, not here");
        }

        GuiCommand::ParseLtcVideo(path, stream_index, channel_index) => {
            if supervisor.is_running(JobKind::LtcDecode) {
                info!("LTC decode already in progress — ignoring duplicate ParseLtcVideo");
                return;
            }
            let decoder_name = if use_libltc { "libltc" } else { "builtin" };
            info!(
                "LTC video decode requested: {} (stream={}, channel={}, decoder={}, fps={})",
                path, stream_index, channel_index, decoder_name, state.decode_fps(),
            );

            state.decode.result = None;
            state.decode.error = None;
            state.decode.generation = state.decode.generation.wrapping_add(1);
            let msg = format!(
                "Extracting audio from: {} stream={} ch={}",
                path, stream_index, channel_index,
            );
            state.status.set_decode(msg.clone());
            state.jobs.insert(JobKind::LtcDecode, job::JobStatus {
                progress: ProgressSnapshot {
                    phase: job::JobPhase::Running,
                    fraction: 0.0,
                    message: msg,
                    speed: None,
                    units: Vec::new(),
                    log: String::new(),
                },
                error: None,
            });

            let capture_gen = state.decode.generation;
            let decode_fps = state.decode_fps();
            let decode_drop_frame = state.decode_drop_frame();

            // Hardening: validate the selection against the probe data so a
            // stale or out-of-range GUI state fails fast with a clear message
            // instead of invoking ffmpeg on a nonexistent stream.
            if let Some(ref probe) = state.decode.probe {
                let available: Vec<usize> =
                    probe.streams.iter().map(|s| s.stream_index).collect();
                let validation_error = match probe
                    .streams
                    .iter()
                    .find(|s| s.stream_index == stream_index)
                {
                    None => Some(format!(
                        "Stream {} not found in '{}' (available audio streams: {:?})",
                        stream_index, path, available
                    )),
                    Some(s) if channel_index >= s.channels => Some(format!(
                        "Channel {} out of range for stream {} in '{}' ({} channels available)",
                        channel_index, stream_index, path, s.channels
                    )),
                    Some(_) => None,
                };
                if let Some(e) = validation_error {
                    error!("LTC video decode rejected: {}", e);
                    if let Some(status) = state.jobs.get_mut(&JobKind::LtcDecode) {
                        status.progress.phase = job::JobPhase::Failed;
                        status.error = Some(e.clone());
                    }
                    state.decode.error = Some(e.clone());
                    state.status.set_decode(format!("Parse failed: {}", e));
                    return;
                }
            }

            let path_job = path.clone();
            let spec = job::JobSpec {
                kind: JobKind::LtcDecode,
                name: "ltc-video-decode",
                units: vec![
                    job::UnitSpec { weight: 0.5, label: "extract".into() },
                    job::UnitSpec { weight: 0.5, label: "decode".into() },
                ],
            };
            spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
                let cancel = ctx.cancel.inner().clone();
                let extract_unit = ctx.progress.unit(0);
                extract_unit.set_message("extracting audio…");
                let on_extract = |frac: f32| {
                    extract_unit.set_fraction(frac);
                };

                let decode_unit = ctx.progress.unit(1);
                decode_unit.set_message("decoding LTC…");

                let result = decode_one_video_clip(
                    &path_job, stream_index, channel_index,
                    use_libltc, decode_fps, decode_drop_frame, capture_gen,
                    &cancel, &on_extract,
                    Some(decode_unit),
                );
                extract_unit.set_fraction(1.0);

                Ok(result
                    .map(|r| JobFinal::Decode { result: Ok(r), path: PathBuf::from(path_job.clone()) })
                    .unwrap_or_else(|e| JobFinal::Decode { result: Err(e), path: PathBuf::from(path_job) }))
            });
        }

        GuiCommand::SetLtcDecodeStream(idx) => {
            state.decode.selected_stream = idx;
        }

        GuiCommand::SetLtcDecodeChannel(idx) => {
            state.decode.selected_channel = idx;
        }

        GuiCommand::ParseLtcWavFile(path) => {
            if supervisor.is_running(JobKind::LtcDecode) {
                info!("LTC decode already in progress — ignoring duplicate ParseLtcWavFile");
                return;
            }
            let decoder_name = if use_libltc { "libltc" } else { "builtin" };
            info!("LTC decode requested for: {} (decoder: {}, fps: {})", path, decoder_name, state.decode_fps());

            // Quick open to calculate chunk count
            let config = DecodeConfig::default();
            let chunk_count = match audio_core::count_chunks_in_wav(Path::new(&path), &config) {
                Ok(c) => c,
                Err(e) => {
                    error!("Failed to open WAV for chunked decode: {}", e);
                    state.decode.error = Some(e.clone());
                    state.status.set_decode(format!("Parse failed: {}", e));
                    if let Some(status) = state.jobs.get_mut(&JobKind::LtcDecode) {
                        status.progress.phase = job::JobPhase::Failed;
                        status.error = Some(e.clone());
                    }
                    return;
                }
            };

            state.decode.result = None;
            state.decode.error = None;
            state.decode.generation = state.decode.generation.wrapping_add(1);
            let msg = format!(
                "Decoding LTC from: {} [{}] at {:.2} fps ({} chunks)",
                path, decoder_name, state.decode_fps(), chunk_count
            );
            state.status.set_decode(msg.clone());
            state.jobs.insert(JobKind::LtcDecode, job::JobStatus {
                progress: ProgressSnapshot {
                    phase: job::JobPhase::Running,
                    fraction: 0.0,
                    message: msg,
                    speed: None,
                    units: Vec::new(),
                    log: String::new(),
                },
                error: None,
            });

            let decode_fps = state.decode_fps();
            let decode_drop_frame = state.decode_drop_frame();

            info!("Spawning chunked decode ({} chunks, decoder={}, fps={})",
                chunk_count, decoder_name, decode_fps);

            let path_job = path.clone();
            let spec = job::JobSpec {
                kind: JobKind::LtcDecode,
                name: "ltc-wav-decode",
                units: vec![job::UnitSpec { weight: 1.0, label: "decode".into() }],
            };
            spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
                let cancel = ctx.cancel.inner().clone();
                let unit = ctx.progress.unit(0);
                unit.set_message("decoding LTC…");

                // Share chunks_done and cancel_flag atomics so the bridge thread
                // can update UnitProgress while decode_ltc_chunked runs.
                let chunks_done = Arc::new(AtomicUsize::new(0));
                let dp = DecodeProgress {
                    chunks_total: chunk_count,
                    chunks_completed: chunks_done.clone(),
                    cancel_flag: cancel.clone(),
                };

                let dp_bridge = bridge_decode_progress(dp.clone(), unit, None);

                let result = if chunk_count <= 1 {
                    let r = audio_core::decode_ltc_with_decoder(
                        Path::new(&path_job), use_libltc, decode_fps, decode_drop_frame,
                        Some(&cancel),
                    );
                    chunks_done.store(1, Ordering::Relaxed);
                    r
                } else {
                    let config = DecodeConfig::default();
                    audio_core::decode_ltc_chunked(
                        Path::new(&path_job), use_libltc, decode_fps, decode_drop_frame,
                        config, &dp,
                    )
                };

                let _ = dp_bridge.join();

                let path_final = path_job;
                Ok(JobFinal::Decode {
                    result,
                    path: PathBuf::from(path_final),
                })
            });
        }

        GuiCommand::SceneUp => {
            state.clapper.scene = state.clapper.scene.saturating_add(1);
        }
        GuiCommand::SceneDown => {
            state.clapper.scene = state.clapper.scene.saturating_sub(1);
        }
        GuiCommand::TakeUp => {
            state.clapper.take = state.clapper.take.saturating_add(1);
        }
        GuiCommand::TakeDown => {
            state.clapper.take = state.clapper.take.saturating_sub(1);
        }
        GuiCommand::HourUp => { stepper_hour(state, 1); }
        GuiCommand::HourDown => { stepper_hour(state, -1); }
        GuiCommand::MinuteUp => { stepper_minute(state, 1); }
        GuiCommand::MinuteDown => { stepper_minute(state, -1); }
        GuiCommand::SecondUp => { stepper_second(state, 1); }
        GuiCommand::SecondDown => { stepper_second(state, -1); }
        GuiCommand::FrameUp => { stepper_frame(state, 1); }
        GuiCommand::FrameDown => { stepper_frame(state, -1); }

        GuiCommand::ProbeFileDurations(_) => {
            // Handled in the command drain loop (engine_main) before reaching process_command
        }

        GuiCommand::Converter(_) => {
            // All ConverterCommand variants are handled in the main engine
            // command drain loop (before process_command).
        },

        GuiCommand::Offload(_) => {
            // All Offload commands are handled in the drain loop before
            // reaching process_command.
        }

        GuiCommand::Shutdown => {
            // Handled in the command drain loop before reaching process_command
        }
    }
}

// ── Stepper helpers ─────────────────────────────────────────────────────

fn stepper_hour(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    tc.hours = (tc.hours as i32 + delta).rem_euclid(24) as u32;
    state.start_timecode = tc;
}

fn stepper_minute(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    tc.minutes = (tc.minutes as i32 + delta).rem_euclid(60) as u32;
    state.start_timecode = tc;
}

fn stepper_second(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    tc.seconds = (tc.seconds as i32 + delta).rem_euclid(60) as u32;
    state.start_timecode = tc;
}

fn stepper_frame(state: &mut AppStateSnapshot, delta: i32) {
    let mut tc = state.start_timecode;
    let max_frame = (state.fps().round() as u32).saturating_sub(1);
    tc.frames = (tc.frames as i32 + delta).rem_euclid(max_frame as i32 + 1) as u32;
    state.start_timecode = tc;
}

// ── Audio init ──────────────────────────────────────────────────────────

fn ensure_audio_init(
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    last_device_id: &mut Option<String>,
    recovery_attempts: &mut u8,
) -> bool {
    if state.audio_initialized {
        return true;
    }

    let device_id = match &state.selected_device {
        Some(id) if state.devices.iter().any(|d| &d.id == id) => id.clone(),
        _ if !state.devices.is_empty() => {
            let fallback = state.devices[0].id.clone();
            state.selected_device = Some(fallback.clone());
            fallback
        }
        _ => String::new(),
    };

    let max_attempts = 3;
    let mut last_error = String::new();

    for attempt in 1..=max_attempts {
        match core.init_output(&device_id, state.sample_rate, BUFFER_SIZE) {
            Ok(actual_rate) => {
                state.sample_rate = actual_rate;
                state.audio_initialized = true;
                state.sample_format_name = core.sample_format_name();
                *last_device_id = Some(device_id.clone());
                *recovery_attempts = 0;
                info!("Audio initialized at {} Hz on device {}", actual_rate, device_id);
                return true;
            }
            Err(e) => {
                last_error = e;
                if audio_core::is_permanent_device_error(&last_error) {
                    error!("Permanent audio error: {}", last_error);
                    break;
                }
                if attempt < max_attempts {
                    let delay = Duration::from_millis(50 * (1 << (attempt - 1)));
                    warn!(
                        "Audio init attempt {}/{} failed ({}), retrying in {:?}",
                        attempt, max_attempts, last_error, delay
                    );
                    std::thread::sleep(delay);
                }
            }
        }
    }

    error!("Audio init failed after {} attempts: {}", max_attempts, last_error);
    state.status.set_audio(format!("Audio init failed: {}", last_error));
    state.events.push(AudioEvent::StreamError(last_error.clone()));
    false
}

fn try_init_device(
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    last_device_id: &mut Option<String>,
    recovery_attempts: &mut u8,
) -> bool {
    let device_id = match &state.selected_device {
        Some(id) if state.devices.iter().any(|d| &d.id == id) => id.clone(),
        _ => return false,
    };

    match core.init_output(&device_id, state.sample_rate, BUFFER_SIZE) {
        Ok(actual_rate) => {
            state.sample_rate = actual_rate;
            state.audio_initialized = true;
            state.sample_format_name = core.sample_format_name();
            *last_device_id = Some(device_id);
            *recovery_attempts = 0;
            info!("Device switched, audio at {} Hz", actual_rate);
            true
        }
        Err(e) => {
            error!("Device init failed: {}", e);
            state.audio_initialized = false;
            false
        }
    }
}

// ── Event handling ──────────────────────────────────────────────────────

fn handle_event(
    event: AudioEvent,
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    recovery_attempts: &mut u8,
    last_device_id: &mut Option<String>,
) {
    let event_str = match &event {
        AudioEvent::StreamError(msg) => format!("Audio stream error: {}", msg),
        AudioEvent::StreamDied => "Audio stream died".to_string(),
        AudioEvent::StreamRecovering { attempt } => {
            format!("Stream recovering (attempt {})", attempt)
        }
        AudioEvent::StreamDead => "Audio stream permanently dead".to_string(),
        AudioEvent::RecoveryNeeded { reason } => {
            format!("Recovery needed: {}", reason)
        }
        AudioEvent::Underrun => "Audio underrun".to_string(),
        AudioEvent::FramesDropped { total } => format!("{} frames dropped", total),
    };

    warn!("{}", event_str);

    match event {
        AudioEvent::StreamDead => {
            // Full teardown-and-recreate: drop the orphaned cpal::Stream,
            // wait for OS driver cleanup, then re-init and restart if playing.
            // The scheduler watchdog already exhausted 3 soft-recovery attempts
            // before emitting StreamDead, so this is the final hard reset.
            state.status.set_audio("Stream dead — performing hard reset");
            attempt_recovery(core, state, recovery_attempts, last_device_id);
        }
        AudioEvent::RecoveryNeeded { .. } | AudioEvent::StreamDied => {
            if *recovery_attempts < MAX_RECOVERY_ATTEMPTS {
                *recovery_attempts += 1;
                state.status.set_audio(format!("Recovery attempt {}/{}", recovery_attempts, MAX_RECOVERY_ATTEMPTS));
                attempt_recovery(core, state, recovery_attempts, last_device_id);
            } else {
                state.is_playing = false;
                state.status.set_audio("Recovery exhausted");
            }
        }
        _ => {}
    }

    state.events.push(event);
}

fn attempt_recovery(
    core: &AudioCore,
    state: &mut AppStateSnapshot,
    recovery_attempts: &mut u8,
    last_device_id: &mut Option<String>,
) {
    let was_playing = state.is_playing;
    let stored_tc = state.current_timecode;

    let _ = core.stop_ltc();
    let _ = core.stop_output();
    state.audio_initialized = false;
    state.is_playing = false;

    // Allow 150ms for the OS audio driver to release the hardware lock
    // (ALSA/PulseAudio/PipeWire cleanup after dropping the cpal::Stream)
    std::thread::sleep(Duration::from_millis(150));

    if ensure_audio_init(core, state, last_device_id, recovery_attempts)
        && was_playing
    {
        let _ = core.reset_ltc(stored_tc);
        match core.start_ltc(
            stored_tc,
            state.fps(),
            state.drop_frame(),
            state.ltc_channel,
            state.ltc_volume,
        ) {
            Ok(()) => {
                state.is_playing = true;
                state.current_timecode = stored_tc;
                info!("Recovery succeeded");
            }
            Err(e) => {
                error!("Recovery start_ltc failed: {}", e);
            }
        }
    }
}

/// Apply an ffmpeg capability probe result to the current snapshot.
fn apply_ffmpeg_probe_result(current: &mut AppStateSnapshot, caps: FfmpegCapabilities) {
    current.ffmpeg_caps = Some(caps.clone());
    info!(
        "ffmpeg capability probe complete: {} encoder(s), {} format(s), hw_vaapi={}, hw_vulkan={}",
        caps.available_encoders.len(),
        caps.available_formats.len(),
        caps.hw.vaapi_device.is_some(),
        caps.hw.vulkan_available,
    );
    // Repair user settings defaults now that caps are available
    apply_available_defaults(
        &mut current.converter.settings.container,
        &mut current.converter.settings.video_encoder,
        &mut current.converter.settings.audio_encoder,
        &caps,
    );
    recompute_converter_derived(current);
}

/// Handle an offload command from within the engine command drain loop.
fn handle_offload_command(
    cmd: crate::command::OffloadCommand,
    state: &mut AppStateSnapshot,
    supervisor: &mut JobSupervisor,
) {
    match cmd {
        crate::command::OffloadCommand::ScanCards => {
            if supervisor.is_running(JobKind::OffloadScan) {
                info!("Offload scan already in progress — ignoring duplicate ScanCards");
                return;
            }
            state.offload.error = None;
            let spec = job::JobSpec {
                kind: JobKind::OffloadScan,
                name: "offload-scan",
                units: vec![job::UnitSpec { weight: 1.0, label: "scan".into() }],
            };
            spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
                run_offload_scan_job(ctx)
            });
        }

        crate::command::OffloadCommand::SetParentFolder(path) => {
            state.offload.parent_folder = Some(path.clone());
            state.offload.error = None;
            config::save_offload_parent(&path);
        }

        crate::command::OffloadCommand::SetParentName(name) => {
            state.offload.parent_name = name;
        }

        crate::command::OffloadCommand::SetDeviceName(idx, name) => {
            if let Some(card) = state.offload.cards.get_mut(idx) {
                card.device_name = name;
                card.name_source = DeviceNameSource::Manual;
            }
        }

        crate::command::OffloadCommand::SetFileSelected(card_idx, file_idx, selected) => {
            if let Some(card) = state.offload.cards.get_mut(card_idx) {
                if file_idx < card.selected.len() {
                    card.selected[file_idx] = selected;
                    card.selected_count = card.selected.iter().filter(|&&s| s).count();
                    card.selected_bytes = card.files.iter()
                        .zip(card.selected.iter())
                        .filter(|(_, &sel)| sel)
                        .map(|(f, _)| f.size_bytes)
                        .sum();
                }
            }
        }

        crate::command::OffloadCommand::SetAllFilesSelected(card_idx, selected) => {
            if let Some(card) = state.offload.cards.get_mut(card_idx) {
                let sel = vec![selected; card.files.len()];
                crate::offload::apply_selection(card, sel);
            }
        }

        crate::command::OffloadCommand::SelectLatestDay(card_idx) => {
            if let Some(card) = state.offload.cards.get_mut(card_idx) {
                let sel = crate::offload::default_selection(&card.files);
                crate::offload::apply_selection(card, sel);
            }
        }

        crate::command::OffloadCommand::StartOffload => {
            if supervisor.is_running(JobKind::OffloadCopy) {
                info!("Offload already running — ignoring duplicate StartOffload");
                return;
            }
            if state.offload.cards.is_empty() {
                state.offload.error = Some("No media cards detected.".to_string());
                return;
            }
            let parent_folder = match state.offload.parent_folder.clone() {
                Some(p) => p,
                None => {
                    state.offload.error = Some("No parent folder selected.".to_string());
                    return;
                }
            };
            let dest_parent = parent_folder.join(&state.offload.parent_name);

            // Build plans per device from selected files only
            let cards = state.offload.cards.clone();
            let names: Vec<String> = cards.iter().map(|c| c.device_name.clone()).collect();
            let device_plans: Vec<Vec<crate::offload::CopyPlanItem>> = cards
                .iter()
                .map(|card| {
                    let selected_with_sizes: Vec<(PathBuf, u64)> = card.files.iter()
                        .zip(card.selected.iter())
                        .filter(|(_, &sel)| sel)
                        .map(|(f, _)| (f.path.clone(), f.size_bytes))
                        .collect();
                    crate::offload::plan_copies_for_files_with_sizes(
                        &selected_with_sizes,
                        &card.device_name,
                        &dest_parent,
                    )
                })
                .collect();

            let total_selected: usize = device_plans.iter().map(|p| p.len()).sum();
            if total_selected == 0 {
                state.offload.error = Some("No files selected.".to_string());
                return;
            }

            state.offload.error = None;
            // Build per-device totals from the copy plans
            let device_totals: Vec<OffloadDeviceTotals> = names.iter().zip(device_plans.iter()).map(|(name, plans)| {
                OffloadDeviceTotals {
                    name: name.clone(),
                    files_total: plans.len(),
                    bytes_total: plans.iter().map(|p| p.size).sum(),
                }
            }).collect();
            state.offload.device_totals = device_totals;
            let total_files: usize = device_plans.iter().map(|p| p.len()).sum();
            let total_bytes: u64 = device_plans.iter().flat_map(|p| p.iter().map(|i| i.size)).sum();
            info!(
                "Offload started: {} device(s), {} file(s), {} MB → {:?}",
                names.len(),
                total_files,
                total_bytes / (1024 * 1024),
                dest_parent,
            );

            let dest_parent_job = dest_parent.clone();
            let names_job = names.clone();
            let plans_job = device_plans;

            let spec = job::JobSpec {
                kind: JobKind::OffloadCopy,
                name: "offload-copy",
                units: names.iter().map(|n| job::UnitSpec {
                    weight: 1.0 / names.len() as f32,
                    label: n.clone(),
                }).collect(),
            };
            spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
                run_offload_copy_job(ctx, plans_job, names_job, dest_parent_job)
            });
        }

        crate::command::OffloadCommand::CancelOffload => {
            supervisor.cancel(JobKind::OffloadCopy);
            supervisor.cancel(JobKind::OffloadScan);
            state.offload.error = Some("Canceled by user".to_string());
        }
    }
}

// ── Converter helper functions ─────────────────────────────────────────

/// Recompute derived converter UI data (readiness, collision warning,
/// output preview, encoder chain description) from current state.
/// Called by the engine after any settings/groups/caps change.
fn recompute_converter_derived(state: &mut AppStateSnapshot) {
    let has_group = state.converter.selected_group_idx.is_some();
    let prefix_empty = state.converter.settings.filename_prefix.is_empty();
    let output_empty = state.converter.settings.output_folder.as_os_str().is_empty();
    let caps = state.ffmpeg_caps.clone();

    // Readiness — no mutable borrow of converter yet
    let blockers = evaluate_readiness(has_group, prefix_empty, output_empty, caps.as_ref()).blockers;

    // Compute warnings via a temporary settings assembly. The output preview
    // is only needed here (as input to the duplicate-name warning); the GUIs
    // compute their own previews from their local edit buffers.
    let settings = assemble_converter_settings(state);
    let (collision_warning, duplicate_warning) = if let Some(ref s) = settings {
        let probe = state.converter.probes.first().and_then(|p| p.as_ref());
        let collision = output_collision_warning(s);
        let preview = preview_output_files(s, probe);
        let dupe_paths: Vec<std::path::PathBuf> = preview.iter().map(|p| p.path.clone()).collect();
        let dupe_names = duplicate_output_names(&dupe_paths);
        let dupe_warning = duplicate_output_warning(&dupe_names);
        (collision, dupe_warning)
    } else {
        (None, None)
    };

    // Now borrow converter mutably to publish all derived fields at once
    let c = &mut state.converter;
    c.readiness = blockers;
    c.collision_warning = collision_warning;
    c.duplicate_output_warning = duplicate_warning;
}

/// Assemble a `ConverterSettings` from the engine's current state snapshot.
/// Returns `None` when no recording group is selected.
fn assemble_converter_settings(state: &AppStateSnapshot) -> Option<ConverterSettings> {
    let idx = state.converter.selected_group_idx?;
    let group = state.converter.groups.get(idx)?;

    let s = &state.converter.settings;

    let pipeline = if s.metadata_only {
        crate::converter::ConversionPipeline::MetadataOnly
    } else if group.recording_type == RecordingType::MultiTrackAudio {
        crate::converter::ConversionPipeline::AudioOnly {
            generate_synthetic_video: s.generate_synthetic_video,
        }
    } else {
        crate::converter::ConversionPipeline::VideoPassthrough
    };

    // Derive timecode metadata from LTC decode results
    let mut timecode_meta_per_file = if s.set_start_from_ltc {
        if let Some(ref result) = state.decode.result {
            // Single-file decode
            let meta = crate::converter::start_timecode_from_ltc(result);
            (0..group.files.len()).map(|_| meta.clone()).collect()
        } else if !state.decode.group_results.is_empty() {
            // Group decode — one per clip
            state.decode.group_results.iter()
                .map(|r| r.ok().and_then(crate::converter::start_timecode_from_ltc))
                .collect()
        } else {
            vec![None; group.files.len()]
        }
    } else {
        vec![None; group.files.len()]
    };

    // Native camera TC fallback + cross-check.
    // When LTC decode found nothing for a file, use the camera's own embedded
    // start timecode (XAVC LtcChangeTableLtcChangeValue or AVCHD
    // H264:TimeCode) for the timecode-embedding step.
    // When both LTC and native TC exist, log a warning if they differ.
    if s.set_start_from_ltc {
        let native_fps = state.fps();
        let native_df = state.drop_frame();
        for (i, meta) in timecode_meta_per_file.iter_mut().enumerate() {
            let camera_native = state.converter.camera_meta.get(i)
                .and_then(|c| c.as_ref())
                .and_then(|c| c.native_timecode.as_ref())
                .and_then(|s| crate::converter::timecode::parse_native_timecode(s));
            match (meta.as_ref(), camera_native) {
                (Some(ltc_meta), Some(native_tc)) => {
                    if ltc_meta.start != native_tc {
                        let ltc_str = crate::converter::format_ffmpeg_timecode(
                            &ltc_meta.start, ltc_meta.drop_frame);
                        log::warn!(
                            "Native camera TC differs from LTC decode for file {}: \
                             native={:02}:{:02}:{:02}:{:02}, LTC={}",
                            i,
                            native_tc.hours, native_tc.minutes, native_tc.seconds,
                            native_tc.frames, ltc_str,
                        );
                    }
                }
                (None, Some(native_tc)) => {
                    let tc_str = format!("{:02}:{:02}:{:02}:{:02}",
                        native_tc.hours, native_tc.minutes, native_tc.seconds, native_tc.frames);
                    log::info!(
                        "Using native camera timecode {} for file {} (LTC decode: none)",
                        tc_str, i
                    );
                    *meta = Some(crate::converter::TimecodeMetadata {
                        start: native_tc,
                        fps: native_fps,
                        drop_frame: native_df,
                    });
                }
                _ => {}
            }
        }
    }

    let ltc_video_source = if group.recording_type == RecordingType::VideoClipSequence {
        Some((state.decode.selected_stream, state.decode.selected_channel))
    } else {
        None
    };

    // Defensive repair: for MultiTrackAudio groups the channel map dimension
    // must equal the number of input files (each file = one track). If the
    // stored map has a different size (e.g. from stale probe-based resize or
    // identity(0) after selection), rebuild the correct identity permutation.
    let channel_map = if matches!(pipeline, ConversionPipeline::AudioOnly { .. }) {
        let correct_n = group.files.len();
        if s.channel_map.num_channels() != correct_n {
            log::info!(
                "Repaired stale channel map: identity({}) → identity({}) \
                 for MultiTrackAudio group '{}' ({} files)",
                s.channel_map.num_channels(), correct_n, group.prefix, correct_n,
            );
            ChannelMap::identity(correct_n)
        } else {
            s.channel_map.clone()
        }
    } else {
        s.channel_map.clone()
    };

    Some(ConverterSettings {
        pipeline,
        input_files: group.files.clone(),
        recording_type: group.recording_type.clone(),
        ltc_track_channel_index: s.ltc_file_idx,
        channel_map,
        split_tracks: s.split_tracks,
        drop_ltc_track: s.drop_ltc_track,
        ltc_video_source,
        container: s.container.clone(),
        copy_video: s.copy_video,
        video_encoder: s.video_encoder.clone(),
        audio_encoder: s.audio_encoder.clone(),
        resolved_video_encoder: String::new(),
        resolved_hw_device: None,
        output_folder: s.output_folder.clone(),
        filename_prefix: s.filename_prefix.clone(),
        audio_suffix_template: s.audio_suffix_template.clone(),
        video_suffix_template: s.video_suffix_template.clone(),
        set_start_from_ltc: s.set_start_from_ltc,
        embed_camera_metadata: s.embed_camera_metadata,
        trim_offsets_secs: vec![0.0; group.files.len()],
        timecode_meta_per_file,
        camera_meta_per_file: state.converter.camera_meta.clone(),
        device_name: state.converter.device_name.clone(),
        concat_audio: s.concat_audio,
    })
}

/// Apply `SetOutputFolder` to state: only marks the folder as user-set
/// when the value actually changes, so echo-back paths can't freeze the
/// output folder on subsequent recording switches.
fn apply_set_output_folder(state: &mut AppStateSnapshot, folder: PathBuf) {
    let changed = state.converter.settings.output_folder != folder;
    state.converter.settings.output_folder = folder;
    if changed {
        state.converter.settings.output_folder_user_set = true;
    }
    recompute_converter_derived(state);
}

/// Auto-apply LTC decode results to converter settings (split tracks,
/// drop LTC track, set start-from-LTC) once per decode generation.
/// User manual un-ticks survive until the next decode re-run.
/// Apply `SelectRecording(idx)` to state: reset per-recording state, clear
/// LTC decode state, cancel in-flight decodes, and spawn converter clip probe
/// via the unified job supervisor.
///
/// Extracted so it can be called both from the command handler (directly when
/// groups are ready) and from the folder-scan result drain (when a deferred
/// `SelectRecording` was queued during a pending folder scan).
fn apply_recording_selection(
    state: &mut AppStateSnapshot,
    idx: usize,
    supervisor: &mut JobSupervisor,
    last_auto_applied_ltc_gen: &mut u64,
    last_auto_applied_group_ltc_gen: &mut u64,
) {
    let group_count = state.converter.groups.len();
    // Reset per-recording state
    state.converter.selected_group_idx = Some(idx);
    state.converter.probes.clear();
    state.converter.camera_meta.clear();
    state.converter.device_name = None;
    state.converter.probes_generation += 1;
    state.jobs.entry(JobKind::Conversion).or_insert_with(JobStatus::idle);
    state.jobs.entry(JobKind::ClipProbe).or_insert_with(JobStatus::idle);
    // Reset per-recording settings flags
    let channel_count = state.converter.groups.get(idx)
        .map(|g| {
            if g.recording_type == RecordingType::MultiTrackAudio {
                log::info!(
                    "Setting channel map to identity({}) for MultiTrackAudio group '{}'",
                    g.files.len(), g.prefix,
                );
                g.files.len()
            } else {
                0
            }
        })
        .unwrap_or(0);
    let s = &mut state.converter.settings;
    s.set_start_from_ltc = false;
    s.split_tracks = false;
    s.drop_ltc_track = false;
    s.concat_audio = false;
    s.ltc_file_idx = 0;
    s.channel_map = ChannelMap::identity(channel_count);
    // Default output folder = the selected recording's parent dir
    // (scan root or one of its subdirectories).  Re-defaults on every
    // recording selection unless the user has manually set it.
    if !s.output_folder_user_set {
        let record_dir = state.converter.groups.get(idx)
            .and_then(|g| g.files.first())
            .and_then(|f| f.parent())
            .map(|p| p.to_path_buf())
            .or_else(|| state.converter.groups_folder.clone());
        if let Some(dir) = record_dir {
            s.output_folder = dir;
        }
    }
    // Clear stale LTC decode state
    state.decode.probe = None;
    state.decode.selected_stream = 0;
    state.decode.selected_channel = 0;
    state.decode.result = None;
    state.decode.error = None;
    state.decode.group_results.clear();
    state.decode.group_paths.clear();
    // Cancel any running decode jobs via supervisor
    supervisor.cancel(JobKind::LtcDecode);
    supervisor.cancel(JobKind::LtcGroupDecode);
    // Bump decode generations so in-flight results from the old
    // recording are discarded by the generation check.
    state.decode.generation = state.decode.generation.wrapping_add(1);
    state.decode.group_generation = state.decode.group_generation.wrapping_add(1);
    // Reset auto-apply latches so next decode re-applies defaults
    *last_auto_applied_ltc_gen = 0;
    *last_auto_applied_group_ltc_gen = 0;
    state.status.set_converter("Recording selected — probing…");

    // Spawn background probing of all files in the group via unified job infrastructure
    if let Some(group) = state.converter.groups.get(idx) {
        if supervisor.is_running(JobKind::ClipProbe) {
            info!("Clip probe already in progress — cancelling old probe for new recording");
            supervisor.cancel(JobKind::ClipProbe);
        }
        info!(
            "Recording selected: idx={} of {} engine group(s), type={:?}, {} file(s) — spawning clip probe",
            idx, group_count, group.recording_type, group.files.len(),
        );
        let files = group.files.clone();
        let spec = job::JobSpec {
            kind: JobKind::ClipProbe,
            name: "clip-probe",
            units: Vec::new(),
        };
        spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
            ctx.progress.set_indeterminate(true);
            info!("Converter clip probe started: {} file(s)", files.len());
            let probes: Vec<Result<VideoAudioProbe, String>> = files.iter()
                .map(|f| crate::ffprobe::probe_video_audio(f).map_err(|e| e.to_string()))
                .collect();
            let cameras: Vec<Option<crate::CameraInfo>> = files.iter()
                .enumerate()
                .map(|(i, f)| {
                    if i < crate::device_name::DEVICE_NAME_PROBE_SAMPLE {
                        crate::camera_meta::probe_camera_info(f)
                    } else {
                        None
                    }
                })
                .collect();
            let (device_name, _, _) = crate::device_name::resolve_device_name(&files, "", Some(&cameras));
            Ok(JobFinal::ClipProbes { probes, cameras, device_name: Some(device_name) })
        });
    } else {
        warn!(
            "Recording selected: idx={} but engine has {} group(s) — probe skipped (was the folder sent to the engine?)",
            idx, group_count,
        );
    }
    recompute_converter_derived(state);
}

fn auto_apply_ltc_to_settings(state: &mut AppStateSnapshot, decoded_path: &str) {
    state.converter.settings.split_tracks = true;
    state.converter.settings.drop_ltc_track = true;
    state.converter.settings.set_start_from_ltc = true;

    // Derive ltc_file_idx from the decoded file's position in the selected
    // audio group (mirrors how video groups transfer decoded stream/channel
    // into ltc_video_source at assemble time).
    if let Some(idx) = state
        .converter
        .selected_group_idx
        .and_then(|gi| state.converter.groups.get(gi))
        .filter(|g| g.recording_type == RecordingType::MultiTrackAudio)
        .and_then(|g| g.files.iter().position(|f| f.to_string_lossy() == decoded_path))
    {
        state.converter.settings.ltc_file_idx = idx;
        info!(
            "Auto-synced LTC track index to {} from decoded file (path contained in selected audio group)",
            idx,
        );
    }
}

fn auto_apply_group_ltc_to_settings(state: &mut AppStateSnapshot) {
    state.converter.settings.split_tracks = true;
    state.converter.settings.drop_ltc_track = true;
    state.converter.settings.set_start_from_ltc = true;
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use audio_core::ChannelSel;
    use super::*;
    use crate::converter::HwDeviceCapabilities;
    use crate::file_pattern::MatchedGroup;
    use crate::job::JobPhase;
    use crate::state::AppStateSnapshot;
    use audio_core::Timecode;
    use std::collections::BTreeSet;

    fn setup_state() -> AppStateSnapshot {
        AppStateSnapshot::initial()
    }

    // ── stepper_hour ──────────────────────────────────────────────────────

    #[test]
    fn test_stepper_hour_up() {
        let mut state = setup_state();
        state.start_timecode.hours = 0;
        stepper_hour(&mut state, 1);
        assert_eq!(state.start_timecode.hours, 1);
    }

    #[test]
    fn test_stepper_hour_down() {
        let mut state = setup_state();
        state.start_timecode.hours = 5;
        stepper_hour(&mut state, -1);
        assert_eq!(state.start_timecode.hours, 4);
    }

    #[test]
    fn test_stepper_hour_wrap_forward() {
        let mut state = setup_state();
        state.start_timecode.hours = 23;
        stepper_hour(&mut state, 1);
        assert_eq!(state.start_timecode.hours, 0);
    }

    #[test]
    fn test_stepper_hour_wrap_backward() {
        let mut state = setup_state();
        state.start_timecode.hours = 0;
        stepper_hour(&mut state, -1);
        assert_eq!(state.start_timecode.hours, 23);
    }

    #[test]
    fn test_stepper_hour_multiple_steps() {
        let mut state = setup_state();
        state.start_timecode.hours = 22;
        stepper_hour(&mut state, 5);
        assert_eq!(state.start_timecode.hours, 3);
    }

    #[test]
    fn test_stepper_hour_other_fields_unchanged() {
        let mut state = setup_state();
        state.start_timecode = Timecode { hours: 5, minutes: 30, seconds: 15, frames: 10 };
        stepper_hour(&mut state, 1);
        assert_eq!(state.start_timecode.minutes, 30);
        assert_eq!(state.start_timecode.seconds, 15);
        assert_eq!(state.start_timecode.frames, 10);
    }

    // ── auto_apply_ltc_to_settings ───────────────────────────────────────

    #[test]
    fn auto_apply_ltc_sets_converter_flags() {
        let mut state = setup_state();
        state.converter.settings.split_tracks = false;
        state.converter.settings.drop_ltc_track = false;
        state.converter.settings.set_start_from_ltc = false;
        auto_apply_ltc_to_settings(&mut state, "any.wav");
        assert!(state.converter.settings.split_tracks);
        assert!(state.converter.settings.drop_ltc_track);
        assert!(state.converter.settings.set_start_from_ltc);
    }

    #[test]
    fn auto_apply_group_ltc_sets_converter_flags() {
        let mut state = setup_state();
        state.converter.settings.split_tracks = false;
        state.converter.settings.drop_ltc_track = false;
        state.converter.settings.set_start_from_ltc = false;
        auto_apply_group_ltc_to_settings(&mut state);
        assert!(state.converter.settings.split_tracks);
        assert!(state.converter.settings.drop_ltc_track);
        assert!(state.converter.settings.set_start_from_ltc);
    }

    #[test]
    fn auto_apply_ltc_derives_idx_from_decoded_audio_path() {
        let mut state = setup_state();
        let files = vec![
            PathBuf::from("TEST_S01.wav"),
            PathBuf::from("TEST_S02.wav"),
            PathBuf::from("TEST_S03.wav"),
            PathBuf::from("TEST_S04.wav"),
        ];
        state.converter.groups = vec![MatchedGroup {
            prefix: "TEST".into(),
            rel_dir: String::new(),
            recording_type: crate::converter::RecordingType::MultiTrackAudio,
            files: files.clone(),
        }];
        state.converter.selected_group_idx = Some(0);
        state.converter.settings.ltc_file_idx = 999;

        auto_apply_ltc_to_settings(&mut state, "TEST_S02.wav");

        assert_eq!(state.converter.settings.ltc_file_idx, 1,
            "must derive index from decoded path position in audio group");
        assert!(state.converter.settings.split_tracks);
        assert!(state.converter.settings.drop_ltc_track);
        assert!(state.converter.settings.set_start_from_ltc);
    }

    #[test]
    fn auto_apply_ltc_skips_idx_for_path_not_in_group() {
        let mut state = setup_state();
        let files = vec![
            PathBuf::from("TEST_S01.wav"),
            PathBuf::from("TEST_S02.wav"),
        ];
        state.converter.groups = vec![MatchedGroup {
            prefix: "TEST".into(),
            rel_dir: String::new(),
            recording_type: crate::converter::RecordingType::MultiTrackAudio,
            files: files.clone(),
        }];
        state.converter.selected_group_idx = Some(0);
        state.converter.settings.ltc_file_idx = 999;

        auto_apply_ltc_to_settings(&mut state, "NONEXISTENT.wav");

        assert_eq!(state.converter.settings.ltc_file_idx, 999,
            "must not change idx when decoded path is not in group");
        assert!(state.converter.settings.split_tracks);
    }

    #[test]
    fn auto_apply_ltc_skips_idx_for_video_group() {
        let mut state = setup_state();
        state.converter.groups = vec![MatchedGroup {
            prefix: "CLIP".into(),
            rel_dir: String::new(),
            recording_type: crate::converter::RecordingType::VideoClipSequence,
            files: vec![
                PathBuf::from("GOPR0001.MP4"),
                PathBuf::from("GOPR0002.MP4"),
            ],
        }];
        state.converter.selected_group_idx = Some(0);
        state.converter.settings.ltc_file_idx = 999;

        auto_apply_ltc_to_settings(&mut state, "GOPR0001.MP4");

        assert_eq!(state.converter.settings.ltc_file_idx, 999,
            "must not change idx for VideoClipSequence groups (uses ltc_video_source)");
    }

    // ── stepper_minute ────────────────────────────────────────────────────

    #[test]
    fn test_stepper_minute_up() {
        let mut state = setup_state();
        state.start_timecode.minutes = 0;
        stepper_minute(&mut state, 1);
        assert_eq!(state.start_timecode.minutes, 1);
    }

    #[test]
    fn test_stepper_minute_down() {
        let mut state = setup_state();
        state.start_timecode.minutes = 30;
        stepper_minute(&mut state, -1);
        assert_eq!(state.start_timecode.minutes, 29);
    }

    #[test]
    fn test_stepper_minute_wrap_forward() {
        let mut state = setup_state();
        state.start_timecode.minutes = 59;
        stepper_minute(&mut state, 1);
        assert_eq!(state.start_timecode.minutes, 0);
    }

    #[test]
    fn test_stepper_minute_wrap_backward() {
        let mut state = setup_state();
        state.start_timecode.minutes = 0;
        stepper_minute(&mut state, -1);
        assert_eq!(state.start_timecode.minutes, 59);
    }

    #[test]
    fn test_stepper_minute_large_delta() {
        let mut state = setup_state();
        state.start_timecode.minutes = 5;
        stepper_minute(&mut state, 100);
        assert_eq!(state.start_timecode.minutes, 45);
    }

    // ── stepper_second ────────────────────────────────────────────────────

    #[test]
    fn test_stepper_second_up() {
        let mut state = setup_state();
        state.start_timecode.seconds = 0;
        stepper_second(&mut state, 1);
        assert_eq!(state.start_timecode.seconds, 1);
    }

    #[test]
    fn test_stepper_second_wrap_forward() {
        let mut state = setup_state();
        state.start_timecode.seconds = 59;
        stepper_second(&mut state, 1);
        assert_eq!(state.start_timecode.seconds, 0);
    }

    #[test]
    fn test_stepper_second_wrap_backward() {
        let mut state = setup_state();
        state.start_timecode.seconds = 0;
        stepper_second(&mut state, -1);
        assert_eq!(state.start_timecode.seconds, 59);
    }

    #[test]
    fn test_stepper_second_down() {
        let mut state = setup_state();
        state.start_timecode.seconds = 30;
        stepper_second(&mut state, -5);
        assert_eq!(state.start_timecode.seconds, 25);
    }

    // ── stepper_frame ─────────────────────────────────────────────────────

    #[test]
    fn test_stepper_frame_up() {
        let mut state = setup_state();
        state.fps_index = 1;
        state.start_timecode.frames = 0;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 1);
    }

    #[test]
    fn test_stepper_frame_wrap_forward_25fps() {
        let mut state = setup_state();
        state.fps_index = 1;
        state.start_timecode.frames = 24;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0);
    }

    #[test]
    fn test_stepper_frame_wrap_forward_30fps() {
        let mut state = setup_state();
        state.fps_index = 4;
        state.start_timecode.frames = 29;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0);
    }

    #[test]
    fn test_stepper_frame_wrap_forward_24fps() {
        let mut state = setup_state();
        state.fps_index = 0;
        state.start_timecode.frames = 23;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0);
    }

    #[test]
    fn test_stepper_frame_down() {
        let mut state = setup_state();
        state.fps_index = 1;
        state.start_timecode.frames = 15;
        stepper_frame(&mut state, -1);
        assert_eq!(state.start_timecode.frames, 14);
    }

    #[test]
    fn test_stepper_frame_wrap_backward_25fps() {
        let mut state = setup_state();
        state.fps_index = 1;
        state.start_timecode.frames = 0;
        stepper_frame(&mut state, -1);
        assert_eq!(state.start_timecode.frames, 24);
    }

    #[test]
    fn test_stepper_frame_wrap_backward_30fps() {
        let mut state = setup_state();
        state.fps_index = 4;
        state.start_timecode.frames = 0;
        stepper_frame(&mut state, -1);
        assert_eq!(state.start_timecode.frames, 29);
    }

    #[test]
    fn test_stepper_frame_fps_changes_max() {
        let mut state = setup_state();
        state.fps_index = 1;
        state.start_timecode.frames = 24;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0, "25fps: 24→0");

        state.fps_index = 4;
        state.start_timecode.frames = 29;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0, "30fps: 29→0");

        state.fps_index = 0;
        state.start_timecode.frames = 23;
        stepper_frame(&mut state, 1);
        assert_eq!(state.start_timecode.frames, 0, "24fps: 23→0");
    }

    #[test]
    fn test_stepper_frame_large_delta() {
        let mut state = setup_state();
        state.fps_index = 1;
        state.start_timecode.frames = 5;
        stepper_frame(&mut state, 50);
        assert_eq!(state.start_timecode.frames, 5);
    }

    // ── ProcessCommand: basic command effects on state ────────────────────

    #[test]
    fn test_process_command_toggle_lock() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        // ToggleLock on: false → true
        process_command(
            GuiCommand::ToggleLock, &core, true, &mut state, &mut recovery,
            &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor,
        );
        assert!(state.is_locked);
    }

    #[test]
    fn test_process_command_toggle_lock_twice() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::ToggleLock, &core, true, &mut state, &mut recovery,
            &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        process_command(GuiCommand::ToggleLock, &core, true, &mut state, &mut recovery,
            &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(!state.is_locked);
    }

    #[test]
    fn test_process_command_set_fps_index_valid() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetFpsIndex(4), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.fps_index, 4);
        assert_eq!(state.fps(), 30.0);
        assert!(!state.drop_frame());
    }

    #[test]
    fn test_process_command_set_fps_index_drop_frame() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetFpsIndex(3), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.fps_index, 3);
        assert!((state.fps() - 29.97).abs() < 0.01);
        assert!(state.drop_frame());
    }

    #[test]
    fn test_process_command_set_fps_index_out_of_range() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetFpsIndex(99), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.fps_index, 1);
        assert_eq!(state.fps(), 25.0);
    }

    #[test]
    fn test_process_command_set_theme() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetTheme(true), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(state.is_dark_theme);

        process_command(GuiCommand::SetTheme(false), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(!state.is_dark_theme);
    }

    #[test]
    fn test_process_command_toggle_theme() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::ToggleTheme, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(state.is_dark_theme, "toggle from initial false → true");

        process_command(GuiCommand::ToggleTheme, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(!state.is_dark_theme, "toggle again true → false");
    }

    #[test]
    fn test_process_command_clear_logs() {
        let mut state = setup_state();
        state.clapper.logs.push(ClapLogItem {
            id: 1, timestamp: "12:00:00".into(),
            timecode: "01:00:00:00".into(), milliseconds: "0".into(), note: "test".into(),
        });
        state.clapper.logs.push(ClapLogItem {
            id: 2, timestamp: "12:00:01".into(),
            timecode: "01:00:00:01".into(), milliseconds: "0".into(), note: "test2".into(),
        });
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::ClearLogs, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(state.clapper.logs.is_empty());
    }

    #[test]
    fn test_process_command_set_ltc_channel() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetLtcChannel(ChannelSel::Both), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.ltc_channel, ChannelSel::Both);
    }

    #[test]
    fn test_process_command_set_beep_volume() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetBeepVolume(0.75), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!((state.beep_volume - 0.75).abs() < 1e-6);
    }

    #[test]
    fn test_process_command_set_start_timecode() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();
        let tc = Timecode { hours: 10, minutes: 20, seconds: 30, frames: 15 };

        process_command(GuiCommand::SetStartTimecode(tc), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.start_timecode, tc);
    }

    #[test]
    fn test_process_command_set_scene_take_roll() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetScene(42), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.scene, 42);

        process_command(GuiCommand::SetTake(7), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.take, 7);

        process_command(GuiCommand::SetRoll("B002".into()), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.roll, "B002");
    }

    #[test]
    fn test_process_command_scene_up_down() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        state.clapper.scene = 5;
        process_command(GuiCommand::SceneUp, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.scene, 6);

        process_command(GuiCommand::SceneDown, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.scene, 5);
    }

    #[test]
    fn test_process_command_take_up_down() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        state.clapper.take = 3;
        process_command(GuiCommand::TakeUp, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.take, 4);

        process_command(GuiCommand::TakeDown, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.take, 3);
    }

    #[test]
    fn test_process_command_scene_down_at_zero() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        state.clapper.scene = 0;
        process_command(GuiCommand::SceneDown, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.scene, 0, "scene should not go below 0");
    }

    #[test]
    fn test_process_command_take_down_at_zero() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        state.clapper.take = 0;
        process_command(GuiCommand::TakeDown, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.clapper.take, 0, "take should not go below 0");
    }

    #[test]
    fn test_process_command_set_sample_rate() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetSampleRate(48000), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.sample_rate, 48000);
    }

    #[test]
    fn test_process_command_set_auto_increment() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetAutoIncrement(false), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(!state.clapper.auto_increment_take);

        process_command(GuiCommand::SetAutoIncrement(true), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!(state.clapper.auto_increment_take);
    }

    #[test]
    fn test_process_command_set_decode_fps_index() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetDecodeFpsIndex(4), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert_eq!(state.decode.fps_index, 4);
        assert_eq!(state.decode_fps(), 30.0);
        assert!(!state.decode_drop_frame());
    }

    #[test]
    fn test_process_command_set_decode_fps_index_drop_frame() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetDecodeFpsIndex(3), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        assert!((state.decode_fps() - 29.97).abs() < 0.01);
        assert!(state.decode_drop_frame());
    }

    #[test]
    fn test_process_command_set_decode_fps_index_out_of_range() {
        let mut state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(GuiCommand::SetDecodeFpsIndex(99), &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor);
        // Should not change since index is out of range
        assert_eq!(state.decode.fps_index, 1);
    }

    // ── drain_ffmpeg_probe_result ─────────────────────────────────────────

    #[test]
    fn test_apply_ffmpeg_probe_result_stores_caps_and_clears_running() {
        let caps = FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: BTreeSet::new(),
            available_formats: BTreeSet::new(),
            hw: HwDeviceCapabilities::default(),
            error_message: None,
        };

        let mut state = AppStateSnapshot::initial();

        apply_ffmpeg_probe_result(&mut state, caps.clone());

        assert!(state.ffmpeg_caps.is_some(), "caps should be stored");
        let stored = state.ffmpeg_caps.as_ref().unwrap();
        assert!(stored.has_ffmpeg);
    }

    // ── ClearRecordingDecodeState ──────────────────────────────────────────

    fn setup_decode_state() -> AppStateSnapshot {
        use audio_core::LtcDetectionResult;
        let mut s = AppStateSnapshot::initial();
        s.decode.result = Some(LtcDetectionResult {
            status: audio_core::LtcDecodeStatus::Success,
            detected_fps: 25.0,
            drop_frame: false,
            total_possible_frames: 100,
            valid_frames: 100,
            timecodes: vec![],
            avg_confidence: 0.95,
            details: vec![],
            total_audio_duration_secs: 4.0,
            sample_rate: 48000,
            processing_time_ms: 10.0,
            first_ltc_timecode_secs: 0.0,
            quality: None,
        });
        s.decode.error = Some("old error".into());
        s.decode.probe = Some(crate::ffprobe::VideoAudioProbe {
            total_audio_channels: 2,
            streams: vec![],
            is_video_file: true,
        });
        s.decode.group_paths = vec![std::path::PathBuf::from("clip.mp4")];
        s.decode.group_results = vec![ClipDecodeState::Pending];
        s.decode.generation = 42;
        s.decode.group_generation = 99;
        s
    }

    #[test]
    fn test_clear_recording_decode_state_clears_single_state() {
        let mut state = setup_decode_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(
            GuiCommand::ClearRecordingDecodeState, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor,
        );

        assert!(state.decode.result.is_none(), "single result cleared");
        assert!(state.decode.error.is_none(), "error cleared");
        assert!(state.decode.probe.is_none(), "probe cleared");
        assert_eq!(state.job(JobKind::LtcDecode).phase(), JobPhase::Idle, "LtcDecode job reset to Idle");
        assert_eq!(state.decode.generation, 43, "decode gen bumped from 42");
    }

    #[test]
    fn test_clear_recording_decode_state_clears_group_state() {
        let mut state = setup_decode_state();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(
            GuiCommand::ClearRecordingDecodeState, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor,
        );

        assert!(state.decode.group_paths.is_empty(), "group paths cleared");
        assert!(state.decode.group_results.is_empty(), "group results cleared");
        assert_eq!(state.job(JobKind::LtcGroupDecode).phase(), JobPhase::Idle, "LtcGroupDecode job reset to Idle");
        assert_eq!(state.decode.group_generation, 100, "group decode gen bumped from 99");
    }

    #[test]
    fn test_clear_recording_decode_state_generation_noop_when_empty() {
        let mut state = AppStateSnapshot::initial();
        let core = audio_core::AudioCore::new();
        let mut recovery = 0;
        let mut log_id = 0;
        let mut last_dev = None;
        let mut prev_dev = None;
        let mut supervisor = JobSupervisor::new();

        process_command(
            GuiCommand::ClearRecordingDecodeState, &core, true, &mut state,
            &mut recovery, &mut log_id, &mut last_dev, &mut prev_dev, &mut supervisor,
        );

        // Should not panic on empty state, just bump generations
        assert_eq!(state.decode.generation, 1, "decode gen bumped from 0");
        assert_eq!(state.decode.group_generation, 1, "group decode gen bumped from 0");
    }

    // ── assemble_converter_settings ─────────────────────────────────────

    fn make_audio_group(files: Vec<PathBuf>) -> MatchedGroup {
        MatchedGroup {
            prefix: "TASCAM_0097".to_string(),
            rel_dir: String::new(),
            files,
            recording_type: crate::converter::RecordingType::MultiTrackAudio,
        }
    }

    #[test]
    fn test_assemble_converter_settings_repairs_stale_map_for_audio() {
        let mut state = setup_state();
        let files: Vec<PathBuf> = (0..4).map(|i| {
            PathBuf::from(format!("/tmp/TASCAM_0097S{}.wav", i + 1))
        }).collect();
        state.converter.groups = vec![make_audio_group(files)];
        state.converter.selected_group_idx = Some(0);
        state.converter.settings.channel_map = ChannelMap::identity(1);
        state.converter.settings.ltc_file_idx = 0;
        state.converter.settings.filename_prefix = "test".to_string();
        state.converter.settings.output_folder = PathBuf::from("/tmp");
        state.converter.settings.split_tracks = true;
        state.converter.settings.drop_ltc_track = true;

        let settings = assemble_converter_settings(&state)
            .expect("assemble_converter_settings should return Some for valid state");

        assert_eq!(
            settings.channel_map.num_channels(),
            4,
            "audio group channel map should be repaired to file count (4), got {}",
            settings.channel_map.num_channels()
        );
        assert_eq!(
            settings.recording_type,
            crate::converter::RecordingType::MultiTrackAudio,
        );
        assert_eq!(
            settings.ltc_track_channel_index,
            0,
        );
        assert!(matches!(settings.pipeline, crate::converter::ConversionPipeline::AudioOnly { .. }));
    }

    #[test]
    fn test_assemble_converter_settings_does_not_repair_video_map() {
        let mut state = setup_state();
        state.converter.groups = vec![MatchedGroup {
            prefix: "C0001".to_string(),
            rel_dir: String::new(),
            files: vec![PathBuf::from("/tmp/clip1.mp4")],
            recording_type: crate::converter::RecordingType::VideoClipSequence,
        }];
        state.converter.selected_group_idx = Some(0);
        state.converter.settings.channel_map = ChannelMap::identity(2);
        state.converter.settings.filename_prefix = "test".to_string();
        state.converter.settings.output_folder = PathBuf::from("/tmp");

        let settings = assemble_converter_settings(&state)
            .expect("assemble_converter_settings should return Some");

        assert_eq!(
            settings.channel_map.num_channels(),
            2,
            "video group channel map must NOT be repaired, should stay at probe size (2)"
        );
        assert!(matches!(settings.pipeline, crate::converter::ConversionPipeline::VideoPassthrough));
    }

    // ── output_folder defaulting ─────────────────────────────────────────

    /// Helper: call `apply_recording_selection` with minimal argument
    /// plumbing.
    fn apply_sel(state: &mut AppStateSnapshot, idx: usize) {
        let mut supervisor = JobSupervisor::new();
        apply_recording_selection(
            state, idx, &mut supervisor, &mut 0, &mut 0,
        );
    }

    /// Helper: a video group in a subdirectory (one that would be found by
    /// recursive scan).  All files are rooted under `parent`.
    fn make_video_group(parent: &Path) -> MatchedGroup {
        MatchedGroup {
            prefix: "C0001".to_string(),
            rel_dir: parent.file_name().unwrap().to_string_lossy().to_string(),
            files: vec![parent.join("C0001.MP4")],
            recording_type: crate::converter::RecordingType::VideoClipSequence,
        }
    }

    #[test]
    fn defaults_to_record_parent_dir_subdir() {
        let root = Path::new("/root");
        let first_dir = root.join("day1");
        let second_dir = root.join("day2");
        let mut state = setup_state();
        state.converter.groups_folder = Some(root.to_path_buf());
        state.converter.groups = vec![
            make_video_group(&first_dir),
            make_video_group(&second_dir),
        ];
        state.converter.settings.output_folder = PathBuf::new();

        apply_sel(&mut state, 0);

        assert_eq!(state.converter.settings.output_folder, first_dir,
            "default output for a subdir recording should be the recording's parent dir");
    }

    #[test]
    fn defaults_to_scan_root_for_root_files() {
        let root = Path::new("/root");
        let mut state = setup_state();
        state.converter.groups_folder = Some(root.to_path_buf());
        state.converter.groups = vec![MatchedGroup {
            prefix: "C0001".to_string(),
            rel_dir: String::new(),
            files: vec![root.join("C0001.MP4")],
            recording_type: crate::converter::RecordingType::VideoClipSequence,
        }];
        state.converter.settings.output_folder = PathBuf::new();

        apply_sel(&mut state, 0);

        assert_eq!(state.converter.settings.output_folder, root,
            "default output for a root-level recording should be the scan root");
    }

    #[test]
    fn output_default_follows_recording_switch() {
        let root = Path::new("/root");
        let first_dir = root.join("day1");
        let second_dir = root.join("day2");
        let mut state = setup_state();
        state.converter.groups_folder = Some(root.to_path_buf());
        state.converter.groups = vec![
            make_video_group(&first_dir),
            make_video_group(&second_dir),
        ];
        state.converter.settings.output_folder = PathBuf::new();

        apply_sel(&mut state, 0);
        assert_eq!(state.converter.settings.output_folder, first_dir,
            "first selection defaults to first dir");

        apply_sel(&mut state, 1);
        assert_eq!(state.converter.settings.output_folder, second_dir,
            "second selection defaults to second dir when not user-set");
    }

    #[test]
    fn manual_output_override_survives_recording_switch() {
        let root = Path::new("/root");
        let first_dir = root.join("day1");
        let second_dir = root.join("day2");
        let custom = Path::new("/custom/output");
        let mut state = setup_state();
        state.converter.groups_folder = Some(root.to_path_buf());
        state.converter.groups = vec![
            make_video_group(&first_dir),
            make_video_group(&second_dir),
        ];

        state.converter.settings.output_folder = custom.to_path_buf();
        state.converter.settings.output_folder_user_set = true;

        apply_sel(&mut state, 0);
        assert_eq!(state.converter.settings.output_folder, custom,
            "user-set output must survive recording selection");

        apply_sel(&mut state, 1);
        assert_eq!(state.converter.settings.output_folder, custom,
            "user-set output must survive selection switch");
    }

    #[test]
    fn set_output_folder_to_new_path_marks_user_set() {
        let mut state = setup_state();
        state.converter.settings.output_folder = PathBuf::from("/old");
        state.converter.settings.output_folder_user_set = false;

        apply_set_output_folder(&mut state, PathBuf::from("/new"));

        assert_eq!(state.converter.settings.output_folder, PathBuf::from("/new"));
        assert!(state.converter.settings.output_folder_user_set,
            "setting a different folder must mark it as user-set");
    }

    #[test]
    fn set_output_folder_echo_does_not_freeze_folder() {
        let root = Path::new("/root");
        let first_dir = root.join("day1");
        let second_dir = root.join("day2");
        let mut state = setup_state();
        state.converter.groups_folder = Some(root.to_path_buf());
        state.converter.groups = vec![
            make_video_group(&first_dir),
            make_video_group(&second_dir),
        ];
        state.converter.settings.output_folder = PathBuf::new();

        apply_sel(&mut state, 0);
        assert_eq!(state.converter.settings.output_folder, first_dir,
            "first selection defaults to first dir");
        assert!(!state.converter.settings.output_folder_user_set,
            "flag must be false after first auto-default");

        // Simulate the GUI echo-back: SetOutputFolder with the same value
        apply_set_output_folder(&mut state, first_dir.clone());
        assert_eq!(state.converter.settings.output_folder, first_dir,
            "folder unchanged by no-op echo");
        assert!(!state.converter.settings.output_folder_user_set,
            "echo must NOT set the flag — regression: echo froze the folder");

        apply_sel(&mut state, 1);
        assert_eq!(state.converter.settings.output_folder, second_dir,
            "second selection must still re-default after a no-op echo");
    }

    }