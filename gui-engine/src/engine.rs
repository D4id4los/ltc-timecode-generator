use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use audio_core::LtcDecodeError;
use audio_core::{AudioCore, AudioEvent, DecodeConfig, DecodeProgress, LtcDetectionResult};
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
use crate::ffprobe::VideoAudioProbe;
use crate::job::{self, JobEvent, JobFinal, JobItem, JobKind, JobOutcome, JobStatus, JobSupervisor, spawn_job};
use crate::job::CancelToken;
use crate::offload::{
    DeviceNameSource, SdCardInfo, ScanProgress,
    run_offload_copy_job, run_offload_scan_job_with,
};
use crate::state::{AppStateSnapshot, ClapLogItem, ClipDecodeState};
use crate::timecode;

const TICK_INTERVAL: Duration = Duration::from_millis(40);
const TARGET_ARM_ANGLE: f32 = -25.0 * std::f32::consts::PI / 180.0;
const ARM_SETTLE_EPS: f32 = 1.0 * std::f32::consts::PI / 180.0; // 1 degree
const MAX_RECOVERY_ATTEMPTS: u8 = 3;
const MAX_CLAP_LOGS: usize = 1000;
const BUFFER_SIZE: u32 = 0;



pub fn engine_main(
    cmd_rx: Receiver<GuiCommand>,
    state: Arc<ArcSwap<AppStateSnapshot>>,
    use_libltc: bool,
    event_tx: Sender<AudioEvent>,
) {
    engine_main_with_probe(cmd_rx, state, use_libltc, event_tx, query_ffmpeg_capabilities)
}

/// Injectable I/O sources for the engine loop (the project's `_with` seam
/// convention). Every field has a production default; tests replace only
/// the seams they exercise.
pub type ScanCardsFn = Arc<dyn Fn(&CancelToken, Option<&ScanProgress>) -> Result<Vec<SdCardInfo>, String> + Send + Sync>;

pub struct EngineSeams {
    /// ffmpeg capability probe (the `engine_main_with_probe` seam).
    pub ffmpeg_caps: Box<dyn FnOnce() -> FfmpegCapabilities + Send>,
    /// Card-detection source for offload `ScanCards` jobs. The real
    /// implementation walks `/proc/mounts` / `/sys/class/block`; tests
    /// hand-build `SdCardInfo` values. May block and poll `cancel`.
    pub scan_cards: ScanCardsFn,
}

impl Default for EngineSeams {
    fn default() -> Self {
        EngineSeams {
            ffmpeg_caps: Box::new(query_ffmpeg_capabilities),
            scan_cards: Arc::new(|_cancel, progress| Ok(crate::offload::detect_cards_with_progress(progress))),
        }
    }
}

/// Run the engine loop with injectable I/O seams (testing entry point).
pub fn engine_main_with_seams(
    cmd_rx: Receiver<GuiCommand>,
    state: Arc<ArcSwap<AppStateSnapshot>>,
    use_libltc: bool,
    event_tx: Sender<AudioEvent>,
    seams: EngineSeams,
) {
    engine_main_loop(cmd_rx, state, use_libltc, event_tx, seams)
}

/// Loop-carried mutable state of the engine thread. Owned by
/// [`engine_main_with_probe`] and passed as `&mut` to command/event
/// handlers. The job supervisor deliberately stays outside: many handlers
/// need `&mut` access to both it and `self.current`, which is only possible
/// through disjoint field borrows of separate bindings.
struct EngineLoopState {
    current: AppStateSnapshot,
    last_tick: Instant,
    recovery_attempts: u8,
    log_id_counter: u64,
    last_device_id: Option<String>,
    previous_device: Option<String>,
    /// Engine-internal auto-apply latches — user un-ticks survive decode re-runs
    last_auto_applied_ltc_gen: u64,
    last_auto_applied_group_ltc_gen: u64,
    /// Deferred SelectRecording while a folder scan is still in flight
    /// (set by SelectRecording, cleared by SelectFolder, applied when the
    /// scan result arrives).
    pending_recording: Option<usize>,
    /// Publish-gating: last snapshot stored into the ArcSwap.  Idle ticks
    /// produce a byte-identical snapshot, so the clone + store is
    /// skipped until something actually changes.
    last_published: Option<Arc<AppStateSnapshot>>,
    /// Command ack counter — bumped once per drained command and mirrored
    /// into the published snapshot so GUIs can confirm their edits landed
    /// (see `AppStateSnapshot.applied_command_seq`).  The GUI is the sole
    /// producer on the channel, so this matches the sender's seq 1:1.
    applied_command_seq: u64,
    /// Card-detection seam for offload `ScanCards` jobs (default = real
    /// detector; tests inject hand-built cards).
    scan_cards: ScanCardsFn,
}

impl EngineLoopState {
    fn new(initial: AppStateSnapshot) -> Self {
        EngineLoopState {
            current: initial,
            last_tick: Instant::now(),
            recovery_attempts: 0,
            log_id_counter: 0,
            last_device_id: None,
            previous_device: None,
            last_auto_applied_ltc_gen: 0,
            last_auto_applied_group_ltc_gen: 0,
            pending_recording: None,
            last_published: None,
            applied_command_seq: 0,
            scan_cards: Arc::new(|_cancel, progress| Ok(crate::offload::detect_cards_with_progress(progress))),
        }
    }
}

/// Like [`engine_main`] but accepts an injectable capability-probe function
/// for testing.
pub fn engine_main_with_probe<F>(
    cmd_rx: Receiver<GuiCommand>,
    state: Arc<ArcSwap<AppStateSnapshot>>,
    use_libltc: bool,
    event_tx: Sender<AudioEvent>,
    probe_fn: F,
) where
    F: FnOnce() -> FfmpegCapabilities + Send + 'static,
{
    engine_main_with_seams(
        cmd_rx, state, use_libltc, event_tx,
        EngineSeams { ffmpeg_caps: Box::new(probe_fn), ..EngineSeams::default() },
    )
}

/// Shared engine loop body, parameterised on the injectable seams.
fn engine_main_loop(
    cmd_rx: Receiver<GuiCommand>,
    state: Arc<ArcSwap<AppStateSnapshot>>,
    use_libltc: bool,
    event_tx: Sender<AudioEvent>,
    seams: EngineSeams,
) {
    let mut initial = AppStateSnapshot::initial();

    // Seed persisted paths into the engine snapshot (output folder,
    // offload parent dir). Input folder is restored by the GUI sending
    // SelectFolder during startup.
    let saved_cfg = config::load();
    config::seed_snapshot_from_config(&mut initial, &saved_cfg);

    let core = AudioCore::new();
    let mut els = EngineLoopState::new(initial);
    els.scan_cards = seams.scan_cards;

    // Job supervisor — single channel for all async task result events
    let mut supervisor = JobSupervisor::new();

    // Spawn the ffmpeg capability probe on a background thread (via job supervisor)
    {
        let spec = job::JobSpec {
            kind: JobKind::FfmpegCapProbe,
            name: "ffmpeg-probe",
            units: Vec::new(),
        };
        spawn_job::<JobFinal, _>(&mut supervisor, spec, move |ctx| {
            ctx.progress.set_indeterminate(true);
            let caps = (seams.ffmpeg_caps)();
            Ok(JobFinal::FfmpegCaps { caps: Some(caps) })
        });
    }

    'engine: loop {
        let now = Instant::now();
        let dt = (now - els.last_tick).as_secs_f32();
        els.last_tick = now;

        // 1. Drain all pending commands.  Every successfully received
        //    command bumps the ack counter before dispatch so the
        //    snapshot published at the end of this tick carries it.
        //    `process_command` is the single dispatch site for every
        //    command except Shutdown (which must break this loop).
        loop {
            match cmd_rx.try_recv() {
                Ok(GuiCommand::Shutdown) => {
                    let _ = core.stop_ltc();
                    let _ = core.stop_output();
                    info!("Engine shutdown via Shutdown command");
                    break 'engine;
                }
                Ok(cmd) => {
                    els.applied_command_seq += 1;
                    els.current.applied_command_seq = els.applied_command_seq;
                    process_command(cmd, &core, use_libltc, &event_tx, &mut els, &mut supervisor);
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
            let is_terminal = els.current.jobs.get(kind)
                .map(|s| matches!(s.phase(), job::JobPhase::Succeeded | job::JobPhase::Cancelled | job::JobPhase::Failed))
                .unwrap_or(false);
            if !is_terminal {
                els.current.jobs.insert(*kind, JobStatus::from_progress(snap));
            }
        }
        for event in supervisor.drain() {
            if job_event_is_stale(&supervisor.latest_job, &event) {
                continue;
            }
            handle_job_event(&mut els, &mut supervisor, event);
        }

        // 2. Poll current timecode if playing
        if els.current.is_playing {
            els.current.current_timecode = core.current_timecode();
        }

        // 3. Drain events from AudioCore and deliver them to the GUI
        // through the audio-event channel.  Events are a one-shot mailbox,
        // not state — they must not ride in the snapshot (which is also
        // publish-gated: an empty-events tick must compare equal).
        for event in core.drain_events() {
            handle_event(event, &core, &event_tx, &mut els);
        }

        // 4. Animation: flash alpha decays at 2.0/s
        if els.current.clapper.flash_alpha > 0.0 {
            els.current.clapper.flash_alpha = (els.current.clapper.flash_alpha - dt * 2.0).max(0.0);
        }

        // 5. Animation: arm angle exponential decay toward rest position at 4.0/s
        els.current.clapper.arm_angle += (TARGET_ARM_ANGLE - els.current.clapper.arm_angle)
            * (1.0 - (-4.0 * dt).exp());

        // 5.5 Determine whether clap animation is still visibly in progress.
        // The GUI uses this flag to decide whether to render at 60 fps
        // (for smooth animation) or to throttle to a lower rate.
        els.current.clapper.animating = els.current.clapper.flash_alpha > 0.0
            || (els.current.clapper.arm_angle - TARGET_ARM_ANGLE).abs() > ARM_SETTLE_EPS;

        // 6. Publish state — only when the snapshot actually changed.
        //    Compare against the last-published snapshot *before* cloning:
        //    the common idle tick then costs one structural PartialEq and
        //    no allocation/deep clone at all.
        let changed = els.last_published.as_ref()
            .map_or(true, |p| p.as_ref() != &els.current);
        if changed {
            let next = Arc::new(els.current.clone());
            state.store(next.clone());
            els.last_published = Some(next);
        }

        // 7. Sleep until next tick
        let next_tick = els.last_tick + TICK_INTERVAL;
        if let Some(sleep_dur) = next_tick.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep_dur);
        }
    }

    supervisor.shutdown(Duration::from_secs(2));
}

fn process_command(
    cmd: GuiCommand,
    core: &AudioCore,
    use_libltc: bool,
    event_tx: &Sender<AudioEvent>,
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
) {
    match cmd {
        GuiCommand::StartLtc => {
            ensure_audio_init(core, els, event_tx);
            if !els.current.audio_initialized {
                els.current.status.set_audio("Cannot start — audio not initialized");
                return;
            }
            let state = &mut els.current;
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
            els.current.is_playing = false;
            els.current.status.set_audio("Stopped");
        }

        GuiCommand::Reset => {
            let _ = core.reset_ltc(els.current.start_timecode);
            els.current.current_timecode = els.current.start_timecode;
            els.current.status.set_audio("Reset");
        }

        GuiCommand::ToggleLock => {
            els.current.is_locked = !els.current.is_locked;
        }

        GuiCommand::Clap => {
            let state = &mut els.current;
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
            els.log_id_counter += 1;
            let ts = timecode::chrono_now_string();
            let note = format!("Scene {}", state.clapper.scene);

            state.clapper.logs.push(ClapLogItem {
                id: els.log_id_counter,
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
            els.current.start_timecode = tc;
        }

        GuiCommand::SetFpsIndex(index) => {
            if index < timecode::FPS_OPTIONS.len() {
                els.current.fps_index = index;
            }
        }

        GuiCommand::SetSampleRate(rate) => {
            els.current.sample_rate = rate;
        }

        GuiCommand::SetDevice(device_id) => {
            if els.current.is_playing {
                let _ = core.stop_ltc();
                els.current.is_playing = false;
            }

            let _ = core.stop_output();
            els.current.audio_initialized = false;

            els.previous_device = els.current.selected_device.clone();
            els.current.selected_device = Some(device_id);

            if !try_init_device(core, els) {
                // Revert to previous device
                els.current.selected_device = els.previous_device.take();
                if try_init_device(core, els) {
                    els.current.status.set_audio("Device selection reverted to previous");
                } else {
                    // Previous selection may have been automatic (None);
                    // fall back to the default/first device.
                    ensure_audio_init(core, els, event_tx);
                }
            }
        }

        GuiCommand::RefreshDevices => {
            match audio_core::list_audio_devices() {
                Ok(devices) => {
                    els.current.devices = devices;
                    // Drop the selection if the chosen device vanished;
                    // the next init falls back to the default/first device.
                    if let Some(ref id) = els.current.selected_device {
                        if !els.current.devices.iter().any(|d| &d.id == id) {
                            els.current.selected_device = None;
                        }
                    }
                    els.current.status.set_audio(format!("{} devices found", els.current.devices.len()));
                }
                Err(e) => {
                    error!("Failed to list devices: {}", e);
                    els.current.status.set_audio(format!("Device scan failed: {}", e));
                }
            }
        }

        GuiCommand::InitAudio => {
            ensure_audio_init(core, els, event_tx);
        }

        GuiCommand::SetLtcChannel(ch) => {
            els.current.ltc_channel = ch;
        }
        GuiCommand::SetBeepChannel(ch) => {
            els.current.beep_channel = ch;
        }
        GuiCommand::SetLtcVolume(vol) => {
            els.current.ltc_volume = vol;
        }
        GuiCommand::SetBeepVolume(vol) => {
            els.current.beep_volume = vol;
        }
        GuiCommand::SetBeepFrequency(freq) => {
            els.current.beep_frequency = freq;
        }
        GuiCommand::SetBeepDuration(dur) => {
            els.current.beep_duration = dur;
        }
        GuiCommand::SetScene(scene) => {
            els.current.clapper.scene = scene;
        }
        GuiCommand::SetTake(take) => {
            els.current.clapper.take = take;
        }
        GuiCommand::SetRoll(roll) => {
            els.current.clapper.roll = roll;
        }
        GuiCommand::SetAutoIncrement(val) => {
            els.current.clapper.auto_increment_take = val;
        }
        GuiCommand::SetTheme(dark) => {
            els.current.is_dark_theme = dark;
        }
        GuiCommand::ToggleTheme => {
            els.current.is_dark_theme = !els.current.is_dark_theme;
        }
        GuiCommand::ClearLogs => {
            els.current.clapper.logs.clear();
        }

        GuiCommand::SetDecodeFpsIndex(index) => {
            if let Some(opt) = timecode::FPS_OPTIONS.get(index) {
                els.current.decode.fps_index = index;
                info!("Decode FPS set to: {} (index={})", opt.name, index);
            }
        }

        GuiCommand::CancelDecode => {
            let state = &mut els.current;
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
            cmd_decode_ltc_video_group(els, supervisor, &paths, stream_index, channel_index, use_libltc);
        }

        GuiCommand::ClearRecordingDecodeState => {
            let state = &mut els.current;
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

        GuiCommand::ProbeVideo(path) => {
            cmd_probe_video(els, supervisor, path);
        }

        GuiCommand::ParseLtcVideo(path, stream_index, channel_index) => {
            cmd_parse_ltc_video(els, supervisor, &path, stream_index, channel_index, use_libltc);
        }

        GuiCommand::SetLtcDecodeStream(idx) => {
            els.current.decode.selected_stream = idx;
        }

        GuiCommand::SetLtcDecodeChannel(idx) => {
            els.current.decode.selected_channel = idx;
        }

        GuiCommand::ParseLtcWavFile(path) => {
            cmd_parse_ltc_wav_file(els, supervisor, &path, use_libltc);
        }

        GuiCommand::SceneUp => {
            els.current.clapper.scene = els.current.clapper.scene.saturating_add(1);
        }
        GuiCommand::SceneDown => {
            els.current.clapper.scene = els.current.clapper.scene.saturating_sub(1);
        }
        GuiCommand::TakeUp => {
            els.current.clapper.take = els.current.clapper.take.saturating_add(1);
        }
        GuiCommand::TakeDown => {
            els.current.clapper.take = els.current.clapper.take.saturating_sub(1);
        }
        GuiCommand::HourUp => { stepper_hour(&mut els.current, 1); }
        GuiCommand::HourDown => { stepper_hour(&mut els.current, -1); }
        GuiCommand::MinuteUp => { stepper_minute(&mut els.current, 1); }
        GuiCommand::MinuteDown => { stepper_minute(&mut els.current, -1); }
        GuiCommand::SecondUp => { stepper_second(&mut els.current, 1); }
        GuiCommand::SecondDown => { stepper_second(&mut els.current, -1); }
        GuiCommand::FrameUp => { stepper_frame(&mut els.current, 1); }
        GuiCommand::FrameDown => { stepper_frame(&mut els.current, -1); }

        GuiCommand::ProbeFileDurations(paths) => {
            cmd_probe_file_durations(els, supervisor, paths);
        }

        GuiCommand::Converter(cmd) => {
            handle_converter_command(cmd, els, supervisor);
        }

        GuiCommand::Offload(cmd) => {
            handle_offload_command(cmd, els, supervisor);
        }

        GuiCommand::Shutdown => {
            // Handled in the engine loop's command drain (it must break the
            // tick loop, which a handler function cannot do). Reaching this
            // arm would mean the drain loop's early dispatch was bypassed.
            warn!("GuiCommand::Shutdown reached process_command — ignored");
        }
    }
}

// ── Converter command handling ─────────────────────────────────────────

/// Apply a converter command. Commands that are simple field assignments go
/// through [`apply_simple_converter_setting`]; commands with additional
/// side-effects get their own handler.
fn handle_converter_command(
    cmd: ConverterCommand,
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
) {
    use ConverterCommand as C;
    match cmd {
        C::SelectFolder(path) => cmd_select_folder(els, supervisor, path),
        C::SelectRecording(idx) => {
            if supervisor.is_running(JobKind::FolderScan) {
                info!(
                    "SelectRecording({}) deferred — folder scan still in progress",
                    idx,
                );
                els.pending_recording = Some(idx);
            } else {
                apply_recording_selection(els, supervisor, idx);
            }
        }
        C::SetContainer(container) => {
            els.current.converter.settings.container = container;
            // Re-select best defaults for the new container
            if let Some(ref caps) = els.current.ffmpeg_caps {
                apply_available_defaults(
                    &mut els.current.converter.settings.container,
                    &mut els.current.converter.settings.video_encoder,
                    &mut els.current.converter.settings.audio_encoder,
                    caps,
                );
            }
            recompute_converter_derived(&mut els.current);
        }
        C::SwapChannelMapCells(row, col) => {
            els.current.converter.settings.channel_map.swap(row, col);
            recompute_converter_derived(&mut els.current);
        }
        C::SetOutputFolder(folder) => apply_set_output_folder(&mut els.current, folder),
        C::StartConversion => cmd_start_conversion(els, supervisor),
        C::CancelConversion => cmd_cancel_conversion(els, supervisor),
        other => {
            if apply_simple_converter_setting(&mut els.current.converter.settings, &other) {
                recompute_converter_derived(&mut els.current);
            } else {
                warn!("Unhandled converter command: {:?}", other);
            }
        }
    }
}

/// Try to apply `cmd` as a simple "assign one settings field" converter
/// command. Returns false for commands with side-effects beyond a field
/// assignment (`SetContainer`, `SwapChannelMapCells`, `SetOutputFolder`,
/// `SelectFolder`, `SelectRecording`, `StartConversion`, `CancelConversion`)
/// — those have dedicated arms in [`handle_converter_command`].
fn apply_simple_converter_setting(
    settings: &mut crate::state::ConverterUserSettings,
    cmd: &ConverterCommand,
) -> bool {
    use ConverterCommand as C;
    match cmd {
        C::SetMetadataOnly(v)           => settings.metadata_only = *v,
        C::SetGenerateSyntheticVideo(v) => settings.generate_synthetic_video = *v,
        C::SetCopyVideo(v)              => settings.copy_video = *v,
        C::SetSplitTracks(v)            => settings.split_tracks = *v,
        C::SetDropLtcTrack(v)           => settings.drop_ltc_track = *v,
        C::SetConcatAudio(v)            => settings.concat_audio = *v,
        C::SetStartFromLtc(v)           => settings.set_start_from_ltc = *v,
        C::SetEmbedCameraMetadata(v)    => settings.embed_camera_metadata = *v,
        C::SetLtcFileIndex(v)           => settings.ltc_file_idx = *v,
        C::SetVideoCodec(c)             => settings.video_encoder = c.clone(),
        C::SetAudioEncoder(e)           => settings.audio_encoder = e.clone(),
        C::SetFilenamePrefix(p)         => settings.filename_prefix = p.clone(),
        C::SetAudioSuffixTemplate(t)    => settings.audio_suffix_template = t.clone(),
        C::SetVideoSuffixTemplate(t)    => settings.video_suffix_template = t.clone(),
        _ => return false,
    }
    true
}

fn cmd_select_folder(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    path: PathBuf,
) {
    let current = &mut els.current;
    current.converter.groups.clear();
    current.converter.groups_folder = Some(path.clone());
    els.pending_recording = None; // new scan invalidates any deferred selection
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
    spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
        ctx.progress.set_indeterminate(true);
        let groups = crate::file_pattern::match_files_all_patterns(&scan_path);
        info!(
            "Folder scan complete: {} — {} group(s) matched",
            scan_path.display(),
            groups.len(),
        );
        Ok(JobFinal::FolderScan { path: scan_path, groups })
    });
    recompute_converter_derived(current);
}

fn cmd_start_conversion(els: &mut EngineLoopState, supervisor: &mut JobSupervisor) {
    if els.current.job(JobKind::Conversion).phase() == job::JobPhase::Running {
        warn!("Conversion already in progress — ignoring duplicate StartConversion");
        return;
    }
    let Some(settings) = assemble_converter_settings(&els.current) else {
        let msg = "Cannot start conversion — no recording group selected or settings incomplete".to_string();
        els.current.status.set_converter(msg.clone());
        warn!("{}", msg);
        return;
    };
    let caps = els.current.ffmpeg_caps.clone();
    // Cancel any previous conversion job first
    supervisor.cancel(JobKind::Conversion);
    let spec = job::JobSpec {
        kind: JobKind::Conversion,
        name: "conversion",
        units: vec![job::UnitSpec { weight: 1.0, label: "conversion".into() }],
    };
    spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
        spawn_conversion_job(ctx, settings, caps)
    });
    // Immediately reflect running state in snapshot
    els.current.jobs.insert(JobKind::Conversion, JobStatus::running("Conversion started…"));
    els.current.status.set_converter("Conversion started…");
    info!("Conversion started via engine StartConversion command (job-based)");
}

fn cmd_cancel_conversion(els: &mut EngineLoopState, supervisor: &mut JobSupervisor) {
    supervisor.cancel(JobKind::Conversion);
    if let Some(status) = els.current.jobs.get_mut(&JobKind::Conversion) {
        status.progress.phase = job::JobPhase::Cancelled;
        status.error = Some("Cancelled by user".to_string());
    }
    els.current.status.set_converter("Conversion canceled");
    info!("Conversion cancel signaled via engine CancelConversion command (job-based)");
}

fn cmd_probe_file_durations(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    paths: Vec<PathBuf>,
) {
    if supervisor.is_running(JobKind::DurationProbe) {
        info!("Duration probe already in progress — ignoring duplicate ProbeFileDurations");
        return;
    }
    els.current.file_durations.clear();
    let spec = job::JobSpec {
        kind: JobKind::DurationProbe,
        name: "duration-probe",
        units: vec![job::UnitSpec { weight: 1.0, label: "duration probe".into() }],
    };
    spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
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

fn cmd_probe_video(els: &mut EngineLoopState, supervisor: &mut JobSupervisor, path: String) {
    if supervisor.is_running(JobKind::VideoProbe) {
        info!("Video probe already in progress — ignoring duplicate ProbeVideo");
        return;
    }
    info!("Probing video file for audio streams (async via job): {}", path);
    els.current.decode.probe = None;
    els.current.decode.error = None;
    els.current.status.set_decode(format!("Probing video: {}", path));
    let path_clone = path.clone();
    let spec = job::JobSpec {
        kind: JobKind::VideoProbe,
        name: "video-probe",
        units: Vec::new(),
    };
    spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
        ctx.progress.set_indeterminate(true);
        let result = crate::ffprobe::probe_video_audio(Path::new(&path_clone))
            .map_err(|e| e.to_string());
        Ok(JobFinal::VideoProbe { result })
    });
}

// ── Decode command handlers ────────────────────────────────────────────

fn cmd_decode_ltc_video_group(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    paths: &[String],
    stream_index: usize,
    channel_index: usize,
    use_libltc: bool,
) {
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
        total, stream_index, channel_index, decoder_name, els.current.decode_fps(),
    );

    // Reset group decode state with new generation
    let state = &mut els.current;
    state.decode.group_generation = state.decode.group_generation.wrapping_add(1);
    state.decode.group_paths = paths.iter().map(PathBuf::from).collect();
    state.decode.group_results = vec![ClipDecodeState::Pending; total];
    // Pre-populate job status so CancelDecode can eager-cancel before poll()
    state.jobs.insert(
        JobKind::LtcGroupDecode,
        JobStatus::running(format!("Decoding LTC group: 0/{} clips", total)),
    );
    state.status.set_decode(format!("Decoding LTC group: 0/{} clips", total));

    let capture_gen = state.decode.group_generation;
    let decode_fps = state.decode_fps();
    let decode_drop_frame = state.decode_drop_frame();

    let paths: Vec<String> = paths.to_vec();
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
            let result = crate::decode::decode_video_channel(
                crate::decode::VideoDecodeRequest {
                    path: Path::new(path),
                    stream_index,
                    channel_index,
                    use_libltc,
                    decode_fps,
                    decode_drop_frame,
                    cancel: Some(&cancel),
                },
                capture_gen,
                &|_| {},
                Some(clip_unit),
            )
            .map(Box::new)
            .map_err(|e| e.to_string());
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

fn cmd_parse_ltc_video(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    path: &str,
    stream_index: usize,
    channel_index: usize,
    use_libltc: bool,
) {
    if supervisor.is_running(JobKind::LtcDecode) {
        info!("LTC decode already in progress — ignoring duplicate ParseLtcVideo");
        return;
    }
    let decoder_name = if use_libltc { "libltc" } else { "builtin" };
    info!(
        "LTC video decode requested: {} (stream={}, channel={}, decoder={}, fps={})",
        path, stream_index, channel_index, decoder_name, els.current.decode_fps(),
    );

    let state = &mut els.current;
    state.decode.result = None;
    state.decode.error = None;
    state.decode.generation = state.decode.generation.wrapping_add(1);
    let msg = format!(
        "Extracting audio from: {} stream={} ch={}",
        path, stream_index, channel_index,
    );
    state.status.set_decode(msg.clone());
    state.jobs.insert(JobKind::LtcDecode, JobStatus::running(msg));

    let capture_gen = state.decode.generation;
    let decode_fps = state.decode_fps();
    let decode_drop_frame = state.decode_drop_frame();

    // Hardening: validate the selection against the probe data so a
    // stale or out-of-range GUI state fails fast with a clear message
    // instead of invoking ffmpeg on a nonexistent stream.
    if let Some(ref probe) = state.decode.probe {
        if let Err(e) = validate_stream_channel_selection(probe, stream_index, channel_index) {
            let available: Vec<usize> = probe.streams.iter().map(|s| s.stream_index).collect();
            let msg = match &e {
                StreamSelectionError::MissingStream { stream } => format!(
                    "Stream {stream} not found in '{path}' (available audio streams: {available:?})"
                ),
                StreamSelectionError::ChannelOutOfRange { channel, channels } => format!(
                    "Channel {channel} out of range for stream {stream_index} in '{path}' ({channels} channels available)"
                ),
            };
            error!("LTC video decode rejected: {}", msg);
            if let Some(status) = state.jobs.get_mut(&JobKind::LtcDecode) {
                status.progress.phase = job::JobPhase::Failed;
                status.error = Some(msg.clone());
            }
            state.decode.error = Some(msg.clone());
            state.status.set_decode(format!("Parse failed: {}", msg));
            return;
        }
    }

    let path_job = path.to_string();
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

        let result = crate::decode::decode_video_channel(
            crate::decode::VideoDecodeRequest {
                path: Path::new(&path_job),
                stream_index,
                channel_index,
                use_libltc,
                decode_fps,
                decode_drop_frame,
                cancel: Some(&cancel),
            },
            capture_gen,
            &on_extract,
            Some(decode_unit),
        );
        extract_unit.set_fraction(1.0);

        match result {
            Ok(r) => Ok(JobFinal::Decode { result: Ok(r), path: PathBuf::from(&path_job) }),
            Err(LtcDecodeError::Cancelled) => Err(job::JobError::Cancelled),
            Err(e) => Ok(JobFinal::Decode { result: Err(e), path: PathBuf::from(&path_job) }),
        }
    });
}

/// Why a stream/channel selection cannot be decoded from a probed video
/// file. Internal to the engine; call sites render the user-facing message
/// (including path and available-stream context) into the snapshot strings.
#[derive(Debug, Clone, PartialEq, Eq)]
enum StreamSelectionError {
    MissingStream { stream: usize },
    ChannelOutOfRange { channel: usize, channels: usize },
}

impl std::fmt::Display for StreamSelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamSelectionError::MissingStream { stream } => {
                write!(f, "Stream {stream} not found")
            }
            StreamSelectionError::ChannelOutOfRange { channel, channels } => {
                write!(f, "Channel {channel} out of range ({channels} channels available)")
            }
        }
    }
}

/// Validate a stream/channel selection against a video probe. Returns a
/// typed error when the selection cannot be decoded, `Ok(())` when the
/// selection is valid.
fn validate_stream_channel_selection(
    probe: &VideoAudioProbe,
    stream_index: usize,
    channel_index: usize,
) -> Result<(), StreamSelectionError> {
    match probe.streams.iter().find(|s| s.stream_index == stream_index) {
        None => Err(StreamSelectionError::MissingStream { stream: stream_index }),
        Some(s) if channel_index >= s.channels => Err(StreamSelectionError::ChannelOutOfRange {
            channel: channel_index,
            channels: s.channels,
        }),
        Some(_) => Ok(()),
    }
}

fn cmd_parse_ltc_wav_file(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    path: &str,
    use_libltc: bool,
) {
    // Allow a re-decode once the published phase is terminal: the Finished
    // event already arrived, but the supervisor reaps the finished thread in
    // the next tick's poll, so `is_running` stays true for one tick. During
    // that window a user's cancel-then-redecode command must not be silently
    // ignored (the stale-event gate discards the old job's late events).
    let decode_phase_terminal = els
        .current
        .jobs
        .get(&JobKind::LtcDecode)
        .map(|s| matches!(s.phase(), job::JobPhase::Succeeded | job::JobPhase::Cancelled | job::JobPhase::Failed))
        .unwrap_or(false);
    if supervisor.is_running(JobKind::LtcDecode) && !decode_phase_terminal {
        info!("LTC decode already in progress — ignoring duplicate ParseLtcWavFile");
        return;
    }
    let decoder_name = if use_libltc { "libltc" } else { "builtin" };
    info!("LTC decode requested for: {} (decoder: {}, fps: {})", path, decoder_name, els.current.decode_fps());

    // Quick open to calculate chunk count
    let config = DecodeConfig::default();
    let chunk_count = match audio_core::count_chunks_in_wav(Path::new(path), &config) {
        Ok(c) => c,
        Err(e) => {
            error!("Failed to open WAV for chunked decode: {}", e);
            let state = &mut els.current;
            state.decode.error = Some(e.clone());
            state.status.set_decode(format!("Parse failed: {}", e));
            if let Some(status) = state.jobs.get_mut(&JobKind::LtcDecode) {
                status.progress.phase = job::JobPhase::Failed;
                status.error = Some(e.clone());
            }
            return;
        }
    };

    let state = &mut els.current;
    state.decode.result = None;
    state.decode.error = None;
    state.decode.generation = state.decode.generation.wrapping_add(1);
    let msg = format!(
        "Decoding LTC from: {} [{}] at {:.2} fps ({} chunks)",
        path, decoder_name, state.decode_fps(), chunk_count
    );
    state.status.set_decode(msg.clone());
    state.jobs.insert(JobKind::LtcDecode, JobStatus::running(msg));

    let decode_fps = state.decode_fps();
    let decode_drop_frame = state.decode_drop_frame();

    info!("Spawning chunked decode ({} chunks, decoder={}, fps={})",
        chunk_count, decoder_name, decode_fps);

    let path_job = path.to_string();
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

        let dp_bridge = crate::decode::bridge_decode_progress(dp.clone(), unit, None);

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

        match result {
            Ok(r) => Ok(JobFinal::Decode { result: Ok(r), path: PathBuf::from(path_job) }),
            Err(LtcDecodeError::Cancelled) => Err(job::JobError::Cancelled),
            Err(e) => Ok(JobFinal::Decode { result: Err(e), path: PathBuf::from(path_job) }),
        }
    });
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
    els: &mut EngineLoopState,
    event_tx: &Sender<AudioEvent>,
) -> bool {
    let state = &mut els.current;
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
                els.last_device_id = Some(device_id.clone());
                els.recovery_attempts = 0;
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
    let _ = event_tx.send(AudioEvent::StreamError(last_error.clone()));
    false
}

fn try_init_device(
    core: &AudioCore,
    els: &mut EngineLoopState,
) -> bool {
    let state = &mut els.current;
    let device_id = match &state.selected_device {
        Some(id) if state.devices.iter().any(|d| &d.id == id) => id.clone(),
        _ => return false,
    };

    match core.init_output(&device_id, state.sample_rate, BUFFER_SIZE) {
        Ok(actual_rate) => {
            state.sample_rate = actual_rate;
            state.audio_initialized = true;
            state.sample_format_name = core.sample_format_name();
            els.last_device_id = Some(device_id);
            els.recovery_attempts = 0;
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
    event_tx: &Sender<AudioEvent>,
    els: &mut EngineLoopState,
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
            els.current.status.set_audio("Stream dead — performing hard reset");
            attempt_recovery(core, els, event_tx);
        }
        AudioEvent::RecoveryNeeded { .. } | AudioEvent::StreamDied => {
            if els.recovery_attempts < MAX_RECOVERY_ATTEMPTS {
                els.recovery_attempts += 1;
                els.current.status.set_audio(format!("Recovery attempt {}/{}", els.recovery_attempts, MAX_RECOVERY_ATTEMPTS));
                attempt_recovery(core, els, event_tx);
            } else {
                els.current.is_playing = false;
                els.current.status.set_audio("Recovery exhausted");
            }
        }
        _ => {}
    }

    let _ = event_tx.send(event);
}

fn attempt_recovery(
    core: &AudioCore,
    els: &mut EngineLoopState,
    event_tx: &Sender<AudioEvent>,
) {
    let was_playing = els.current.is_playing;
    let stored_tc = els.current.current_timecode;

    let _ = core.stop_ltc();
    let _ = core.stop_output();
    els.current.audio_initialized = false;
    els.current.is_playing = false;

    // Allow 150ms for the OS audio driver to release the hardware lock
    // (ALSA/PulseAudio/PipeWire cleanup after dropping the cpal::Stream)
    std::thread::sleep(Duration::from_millis(150));

    if ensure_audio_init(core, els, event_tx) && was_playing {
        let _ = core.reset_ltc(stored_tc);
        match core.start_ltc(
            stored_tc,
            els.current.fps(),
            els.current.drop_frame(),
            els.current.ltc_channel,
            els.current.ltc_volume,
        ) {
            Ok(()) => {
                els.current.is_playing = true;
                els.current.current_timecode = stored_tc;
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

// ── Job-event dispatch ──────────────────────────────────────────────────

/// Dispatch a (non-stale) job event to its per-kind handler. The outer
/// match is on `JobKind` — a closed enum with no wildcard arm, so adding a
/// new `JobKind` without a handler is a compile error. Payload mismatches
/// within a kind are logged, never silently dropped.
/// True when `event` belongs to a superseded job: its JobId does not match
/// the most recently spawned job for that kind (tracked in
/// `JobSupervisor.latest_job`). Events from jobs whose kind was never
/// spawned are also stale (defensive). Stale-result gating: without this,
/// a cancelled job's late Finished event would clobber the state written
/// by its replacement.
fn job_event_is_stale(latest_job: &std::collections::HashMap<JobKind, job::JobId>, event: &JobEvent) -> bool {
    let (event_job, event_kind) = match event {
        JobEvent::Finished { job, kind, .. } => (*job, *kind),
        JobEvent::Item { job, kind, .. } => (*job, *kind),
    };
    latest_job.get(&event_kind) != Some(&event_job)
}

fn handle_job_event(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    event: JobEvent,
) {
    match event {
        JobEvent::Finished { kind, outcome, payload, .. } => match kind {
            JobKind::Conversion => on_conversion_finished(els, outcome, payload),
            JobKind::FfmpegCapProbe => on_ffmpeg_caps_finished(els, outcome, payload),
            JobKind::FolderScan => on_folder_scan_finished(els, supervisor, outcome, payload),
            JobKind::VideoProbe => on_video_probe_finished(els, outcome, payload),
            JobKind::OffloadScan => on_offload_scan_finished(els, supervisor, outcome, payload),
            JobKind::DurationProbe => on_duration_probe_finished(els, outcome, payload),
            JobKind::OffloadCopy => on_offload_copy_finished(els, outcome, payload),
            JobKind::LtcDecode => on_ltc_decode_finished(els, outcome, payload),
            JobKind::LtcGroupDecode => on_group_decode_finished(els, outcome, payload),
            JobKind::ClipProbe => on_clip_probes_finished(els, outcome, payload),
        },
        JobEvent::Item { kind, item, .. } => match (kind, item) {
            (JobKind::DurationProbe, JobItem::DurationResult { path, secs }) => {
                on_duration_result(els, path, secs);
            }
            (JobKind::LtcGroupDecode, JobItem::ClipLtcResult { index, result }) => {
                on_group_clip_result(els, index, result);
            }
            (kind, item) => {
                warn!("Unhandled job item event for kind {:?}: {:?}", kind, item);
            }
        },
    }
}

fn on_conversion_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::Conversion) {
        status.apply_outcome(&outcome);
    }
    let payload = match payload {
        JobFinal::Conversion { encoder_used, steps_attempted } => {
            info!(
                "Conversion finished: encoder_used={:?}, steps_attempted={}",
                encoder_used, steps_attempted,
            );
            Some(encoder_used)
        }
        other => {
            warn!("JobKind::Conversion finished with unexpected payload: {:?}", other);
            None
        }
    };
    match (payload, outcome) {
        (Some(encoder_used), JobOutcome::Succeeded { .. }) => {
            recompute_converter_derived(&mut els.current);
            match encoder_used {
                Some(enc) => els.current.status.set_converter(
                    format!("Conversion completed — video encoder: {}", enc),
                ),
                None => els.current.status.set_converter("Conversion completed"),
            }
            info!("Engine-owned conversion completed successfully (job)");
        }
        (None, JobOutcome::Succeeded { .. }) => {
            // Outcome applied, but the payload was not the expected one —
            // do not fabricate a completion message.
            recompute_converter_derived(&mut els.current);
        }
        (_, JobOutcome::Cancelled { .. }) => {
            recompute_converter_derived(&mut els.current);
            els.current.status.set_converter("Conversion canceled");
            info!("Engine-owned conversion cancelled (job)");
        }
        (_, JobOutcome::Failed { error, .. }) => {
            if let Some(status) = els.current.jobs.get_mut(&JobKind::Conversion) {
                status.error = Some(error.clone());
            }
            recompute_converter_derived(&mut els.current);
            els.current.status.set_converter(format!("Conversion failed: {}", error));
            warn!("Engine-owned conversion failed: {}", error);
        }
    }
}

fn on_ffmpeg_caps_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::FfmpegCapProbe) {
        status.apply_outcome(&outcome);
    }
    match payload {
        JobFinal::FfmpegCaps { caps: Some(caps) } => {
            apply_ffmpeg_probe_result(&mut els.current, caps);
        }
        JobFinal::FfmpegCaps { caps: None } => {
            warn!("FFmpeg capability probe returned no caps");
            recompute_converter_derived(&mut els.current);
        }
        other => warn!("JobKind::FfmpegCapProbe finished with unexpected payload: {:?}", other),
    }
}

fn on_folder_scan_finished(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    outcome: JobOutcome,
    payload: JobFinal,
) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::FolderScan) {
        status.apply_outcome(&outcome);
    }
    match payload {
        JobFinal::FolderScan { path, groups } => {
            if Some(&path) == els.current.converter.groups_folder.as_ref() {
                els.current.converter.groups = groups;
                info!("Folder scan complete: {} group(s)", els.current.converter.groups.len());
                if let Some(idx) = els.pending_recording.take() {
                    info!("Applying deferred SelectRecording({}) after folder scan", idx);
                    apply_recording_selection(els, supervisor, idx);
                }
            } else {
                warn!("Discarding stale supervisor folder scan result (path mismatch)");
            }
        }
        other => warn!("JobKind::FolderScan finished with unexpected payload: {:?}", other),
    }
}

fn on_video_probe_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::VideoProbe) {
        status.apply_outcome(&outcome);
    }
    match payload {
        JobFinal::VideoProbe { result } => match result {
            Ok(probe) => {
                els.current.decode.probe = Some(probe.clone());
                els.current.decode.selected_stream = 0;
                els.current.decode.selected_channel = 0;
                els.current.decode.error = None;
                els.current.status.set_decode(format!(
                    "Video probed: {} audio stream(s), {} total channel(s)",
                    probe.streams.len(),
                    probe.total_audio_channels,
                ));
                info!("Video probe succeeded (job): {} streams, {} channels", probe.streams.len(), probe.total_audio_channels);
            }
            Err(e) => {
                els.current.decode.probe = None;
                els.current.decode.error = Some(e.clone());
                els.current.status.set_decode(format!("Video probe failed: {}", e));
                error!("Video probe failed (job): {}", e);
            }
        },
        other => warn!("JobKind::VideoProbe finished with unexpected payload: {:?}", other),
    }
}

fn on_offload_scan_finished(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    outcome: JobOutcome,
    payload: JobFinal,
) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::OffloadScan) {
        status.apply_outcome(&outcome);
    }
    let mut cards = match payload {
        JobFinal::OffloadScan { cards } => cards,
        other => {
            warn!("JobKind::OffloadScan finished with unexpected payload: {:?}", other);
            return;
        }
    };
    // Apply default selection (latest recording day) to each card.
    let file_paths: Vec<PathBuf> = cards.iter()
        .flat_map(|c| c.files.iter().map(|f| f.path.clone()))
        .collect();
    for card in &mut cards {
        let sel = crate::offload::default_selection(&card.files);
        crate::offload::apply_selection(card, sel);
    }
    els.current.offload.cards = cards;
    els.current.offload.file_durations.clear();
    els.current.offload.durations_version =
        els.current.offload.durations_version.wrapping_add(1);

    if els.current.offload.cards.is_empty() {
        info!(
            "Offload card scan complete: 0 cards — if your card reader \
             is connected, check that the card is mounted and has \
             video/audio files"
        );
    } else {
        info!(
            "Offload card scan complete: {} card(s)",
            els.current.offload.cards.len()
        );
        // Spawn async duration probe for all scanned files
        let spec = job::JobSpec {
            kind: JobKind::DurationProbe,
            name: "offload-dur-probe",
            units: Vec::new(),
        };
        spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
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

fn on_duration_result(els: &mut EngineLoopState, path: PathBuf, secs: Option<f64>) {
    // Write to converter file_durations
    els.current.file_durations.insert(path.clone(), secs);
    // Write to offload file_durations
    els.current.offload.file_durations.insert(path, secs);
    els.current.offload.durations_version =
        els.current.offload.durations_version.wrapping_add(1);
}

fn on_duration_probe_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::DurationProbe) {
        status.apply_outcome(&outcome);
    }
    match payload {
        JobFinal::DurationsDone => info!("Duration probe complete"),
        other => warn!("JobKind::DurationProbe finished with unexpected payload: {:?}", other),
    }
}

fn on_offload_copy_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::OffloadCopy) {
        status.apply_outcome(&outcome);
    }
    let completed_devices = match payload {
        JobFinal::OffloadCopy { completed_devices } => completed_devices,
        other => {
            warn!("JobKind::OffloadCopy finished with unexpected payload: {:?}", other);
            return;
        }
    };
    if matches!(outcome, JobOutcome::Cancelled { .. }) {
        info!(
            "Offload copy was cancelled by user: {} device(s) completed",
            completed_devices.len(),
        );
        els.current.offload.error = Some("Canceled by user".to_string());
        els.current.status.set_offload("Offload canceled");
    } else {
        info!(
            "Offload copy complete: {} device(s) offloaded of {}",
            completed_devices.len(),
            els.current.offload.device_totals.len(),
        );
        let parent = els.current.offload.parent_folder.clone();
        for name in &completed_devices {
            if !els.current.offload.completed_devices.contains(name) {
                els.current.offload.completed_devices.push(name.clone());
            }
        }
        // The offload→converter handoff (version + parent) must only fire
        // when the destination actually received files: a fully-failed or
        // empty offload must not yank the converter to an empty folder.
        if !completed_devices.is_empty() {
            let target = parent.map(|p| p.join(&els.current.offload.parent_name));
            els.current.offload.last_offload_parent = target;
            els.current.offload.last_offload_version =
                els.current.offload.last_offload_version.wrapping_add(1);
        }
        els.current.status.set_offload(format!(
            "Offload complete: {} device(s) copied",
            completed_devices.len(),
        ));
    }
}

fn on_ltc_decode_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::LtcDecode) {
        status.apply_outcome(&outcome);
    }
    if matches!(outcome, job::JobOutcome::Cancelled { .. }) {
        els.current.decode.result = None;
        els.current.decode.error = None;
        els.current.status.set_decode("Decode canceled");
        return;
    }
    let (result, path) = match payload {
        JobFinal::Decode { result, path } => (result, path),
        other => {
            warn!("JobKind::LtcDecode finished with unexpected payload: {:?}", other);
            return;
        }
    };
    let generation = els.current.decode.generation;
    match result {
        Ok(r) => {
            let first_offset = r.first_ltc_timecode_secs;
            info!(
                "LTC decode result: path={}, fps={:.2}, valid={}/{}, \
                 confidence={:.1}%, first_ltc_timecode_secs={:.3}s",
                path.display(), r.detected_fps, r.valid_frames, r.total_possible_frames,
                r.avg_confidence * 100.0, first_offset,
            );
            els.current.decode.result = Some(r.clone());
            els.current.decode.error = None;
            let summary = format!(
                "LTC decode: {} frames (confidence {:.1}%, {} fps{})",
                r.valid_frames, r.avg_confidence * 100.0,
                r.detected_fps, if r.drop_frame { " DF" } else { "" },
            );
            els.current.status.set_decode(summary);
            if generation > els.last_auto_applied_ltc_gen {
                auto_apply_ltc_to_settings(&mut els.current, &path.to_string_lossy());
                els.last_auto_applied_ltc_gen = generation;
                recompute_converter_derived(&mut els.current);
            }
        }
        Err(LtcDecodeError::Cancelled) => {
            els.current.decode.result = None;
            els.current.decode.error = None;
            els.current.status.set_decode("Decode canceled");
        }
        Err(e) => {
            let msg = e.to_string();
            els.current.decode.result = None;
            els.current.decode.error = Some(msg.clone());
            els.current.status.set_decode(format!("Parse failed: {}", msg));
            error!("LTC decode failed: {} — {}", path.display(), msg);
        }
    }
}

fn on_group_clip_result(els: &mut EngineLoopState, index: usize, result: Result<Box<LtcDetectionResult>, String>) {
    if els.current.decode.group_results.len() > index {
        els.current.decode.group_results[index] = ClipDecodeState::Done(result);
        let done = els.current.decode.group_results.iter().filter(|r| r.is_done()).count();
        els.current.status.set_decode(format!(
            "Decoding group: {}/{} clips",
            done, els.current.decode.group_results.len(),
        ));
        if let ClipDecodeState::Done(Ok(r)) = &els.current.decode.group_results[index] {
            info!(
                "LTC group decode [{}/{}]: {} frames (confidence {:.1}%)",
                done, els.current.decode.group_results.len(),
                r.valid_frames, r.avg_confidence * 100.0,
            );
        } else if let ClipDecodeState::Done(Err(e)) = &els.current.decode.group_results[index] {
            warn!("LTC group decode [{}/{}]: failed: {}",
                done, els.current.decode.group_results.len(), e);
        }
    }
}

fn on_group_decode_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::LtcGroupDecode) {
        status.apply_outcome(&outcome);
    }
    if !matches!(payload, JobFinal::NoPayload) {
        warn!("JobKind::LtcGroupDecode finished with unexpected payload: {:?}", payload);
    }
    // When the user cancelled and a new decode was spawned,
    // this event is already rejected by the latest_job gate above.
    let total = els.current.decode.group_results.len();
    let successes = els.current.decode.group_results.iter().filter(|r| r.ok().is_some()).count();
    let failures = total - successes;
    let gen = els.current.decode.group_generation;
    let tc_info = if successes > 0 {
        if let Some(ClipDecodeState::Done(Ok(r))) = els.current.decode.group_results.first() {
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
    els.current.status.set_decode(tc_info);
    info!("LTC group decode complete: {}/{} ok, {}/{} failed",
        successes, total, failures, total);
    if gen > els.last_auto_applied_group_ltc_gen {
        auto_apply_group_ltc_to_settings(&mut els.current);
        els.last_auto_applied_group_ltc_gen = gen;
        recompute_converter_derived(&mut els.current);
    }
}

fn on_clip_probes_finished(els: &mut EngineLoopState, outcome: JobOutcome, payload: JobFinal) {
    if let Some(status) = els.current.jobs.get_mut(&JobKind::ClipProbe) {
        status.apply_outcome(&outcome);
    }
    let (probes, cameras, device_name) = match payload {
        JobFinal::ClipProbes { probes, cameras, device_name } => (probes, cameras, device_name),
        other => {
            warn!("JobKind::ClipProbe finished with unexpected payload: {:?}", other);
            return;
        }
    };
    let probe_generation = els.current.converter.probes_generation;
    if probe_generation > 0 {
        if let Some(ref idx) = els.current.converter.selected_group_idx {
            if let Some(group) = els.current.converter.groups.get(*idx) {
                if group.recording_type == RecordingType::VideoClipSequence {
                    if let Some(Ok(ref probe)) = probes.iter().find(|r| r.is_ok()) {
                        els.current.decode.probe = Some(probe.clone());
                        els.current.decode.selected_stream = 0;
                        els.current.decode.selected_channel = 0;
                        els.current.decode.error = None;
                    } else {
                        let err = probes.iter().find_map(|r| {
                            if let Err(ref e) = r { Some(e.clone()) } else { None }
                        }).unwrap_or_else(|| "No audio streams detected.".to_string());
                        els.current.decode.error = Some(format!("LTC source probe failed: {}", err));
                        els.current.status.set_decode(format!("Video probe failed: {}", err));
                    }
                }
            }
        }
        els.current.converter.probes = probes.iter().map(|r| r.as_ref().ok().cloned()).collect();
        els.current.converter.camera_meta = cameras;
        els.current.converter.device_name = device_name;
        info!("Converter clip probe complete: {} files", els.current.converter.probes.len());
        let is_video_group = els.current.converter.selected_group_idx
            .and_then(|i| els.current.converter.groups.get(i))
            .map(|g| g.recording_type == RecordingType::VideoClipSequence)
            .unwrap_or(false);
        if is_video_group {
            let probe_channels = els.current.converter.probes.first()
                .and_then(|p| p.as_ref())
                .map(|p| p.total_audio_channels);
            let map_channels = els.current.converter.settings.channel_map.num_channels();
            if let Some(ch) = probe_channels {
                if ch > 0 && ch != map_channels {
                    els.current.converter.settings.channel_map = ChannelMap::identity(ch);
                }
            }
        }
        recompute_converter_derived(&mut els.current);
    }
}

/// Handle an offload command from within the engine command drain loop.
fn handle_offload_command(
    cmd: crate::command::OffloadCommand,
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
) {
    let state = &mut els.current;
    match cmd {
        crate::command::OffloadCommand::ScanCards => {
            if supervisor.is_running(JobKind::OffloadScan) {
                info!("Offload scan already in progress — ignoring duplicate ScanCards");
                return;
            }
            state.offload.error = None;
            state.offload.plan_error = None;
            let scan_seam = els.scan_cards.clone();
            let spec = job::JobSpec {
                kind: JobKind::OffloadScan,
                name: "offload-scan",
                units: vec![job::UnitSpec { weight: 1.0, label: "scan".into() }],
            };
            spawn_job::<JobFinal, _>(supervisor, spec, move |ctx| {
                run_offload_scan_job_with(ctx, |cancel, progress| scan_seam(cancel, progress))
            });
        }

        crate::command::OffloadCommand::SetParentFolder(path) => {
            state.offload.parent_folder = Some(path.clone());
            state.offload.error = None;
            state.offload.plan_error = None;
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

        crate::command::OffloadCommand::SetDeviceNameByMount(mount, name) => {
            if let Some(card) = state.offload.cards.iter_mut().find(|c| c.mount == mount) {
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
                state.offload.plan_error = Some(crate::offload::OffloadPlanError::NoCards);
                return;
            }
            let parent_folder = match state.offload.parent_folder.clone() {
                Some(p) => p,
                None => {
                    state.offload.error = Some("No parent folder selected.".to_string());
                    state.offload.plan_error = Some(crate::offload::OffloadPlanError::NoParentFolder);
                    return;
                }
            };
            let plan = match crate::offload::build_copy_plan(
                &state.offload.cards,
                &parent_folder,
                &state.offload.parent_name,
            ) {
                Ok(plan) => plan,
                Err(e) => {
                    state.offload.error = Some(e.user_message().to_string());
                    state.offload.plan_error = Some(e);
                    return;
                }
            };

            state.offload.error = None;
            state.offload.plan_error = None;
            info!(
                "Offload started: {} device(s), {} file(s), {} MB → {:?}",
                plan.device_names.len(),
                plan.total_files(),
                plan.total_bytes / (1024 * 1024),
                plan.dest_parent,
            );
            state.offload.device_totals = plan.device_totals;

            let dest_parent_job = plan.dest_parent.clone();
            let names_job = plan.device_names.clone();
            let plans_job = plan.device_plans;

            let spec = job::JobSpec {
                kind: JobKind::OffloadCopy,
                name: "offload-copy",
                units: names_job.iter().map(|n| job::UnitSpec {
                    weight: 1.0 / names_job.len() as f32,
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

/// Apply `SelectRecording(idx)` to state: reset per-recording state, clear
/// LTC decode state, cancel in-flight decodes, and spawn converter clip probe
/// via the unified job supervisor.
///
/// Extracted so it can be called both from the command handler (directly when
/// groups are ready) and from the folder-scan result drain (when a deferred
/// `SelectRecording` was queued during a pending folder scan).
fn apply_recording_selection(
    els: &mut EngineLoopState,
    supervisor: &mut JobSupervisor,
    idx: usize,
) {
    let state = &mut els.current;
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
    els.last_auto_applied_ltc_gen = 0;
    els.last_auto_applied_group_ltc_gen = 0;
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
            let (probes, cameras, device_name) = crate::clip_probe::probe_clip_set(&files);
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
    use crate::ffprobe::AudioStreamInfo;

    // ── re-decode after cancel (one-tick reaping window) ─────────────────

    /// Regression test for a flake surfaced by
    /// `test_cancel_decode_then_redecode_succeeds` under load: the cancel's
    /// terminal phase is published by the Finished event, but the supervisor
    /// only reaps the finished thread in the *next* tick's poll. During that
    /// window `is_running` is still true and a re-decode command was
    /// silently ignored. A terminal published phase must allow the respawn.
    #[test]
    fn parse_ltc_wav_file_respawns_after_terminal_phase_despite_unreaped_job() {
        use crate::job::{JobPhase, JobSpec, JobStatus};

        let dir = tempfile::TempDir::new().unwrap();
        let wav = dir.path().join("tiny.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&wav, spec).unwrap();
        for _ in 0..48000 {
            writer.write_sample(0i16).unwrap();
        }
        writer.finalize().unwrap();

        let mut sup = crate::job::JobSupervisor::new();
        let cancel = crate::job::CancelToken::new();
        let parked_cancel = cancel.clone();
        // Register a still-running LtcDecode job so `is_running` stays true
        // for the whole test (the unreaped-thread window).
        spawn_job::<JobFinal, _>(
            &mut sup,
            JobSpec { kind: crate::job::JobKind::LtcDecode, name: "parked decode", units: vec![] },
            move |_ctx| {
                while !parked_cancel.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Ok(JobFinal::NoPayload)
            },
        );
        let parked_id = *sup.latest_job.get(&crate::job::JobKind::LtcDecode).unwrap();

        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        // The engine published the terminal Cancelled phase for the previous job.
        let mut status = JobStatus::running("decode");
        status.progress.phase = JobPhase::Cancelled;
        els.current.jobs.insert(crate::job::JobKind::LtcDecode, status);

        cmd_parse_ltc_wav_file(&mut els, &mut sup, wav.to_str().unwrap(), false);

        let respawned = *sup.latest_job.get(&crate::job::JobKind::LtcDecode).unwrap();
        assert_ne!(respawned, parked_id,
            "a re-decode must spawn a new job once the published phase is terminal");
        cancel.cancel();
        sup.shutdown(std::time::Duration::from_secs(5));
    }

    // ── job_event_is_stale (stale-event gate) ────────────────────────────

    use crate::job::{JobId, JobItem, JobOutcome, JobFinal};

    fn finished(kind: JobKind, job: u64) -> JobEvent {
        JobEvent::Finished {
            job: JobId(job),
            kind,
            outcome: JobOutcome::Succeeded { log: String::new() },
            payload: JobFinal::Conversion { encoder_used: None, steps_attempted: 1 },
        }
    }

    fn item(kind: JobKind, job: u64) -> JobEvent {
        JobEvent::Item {
            job: JobId(job),
            kind,
            item: JobItem::DurationResult { path: PathBuf::from("x"), secs: Some(1.0) },
        }
    }

    #[test]
    fn stale_finished_event_is_rejected() {
        let mut latest = std::collections::HashMap::new();
        latest.insert(JobKind::LtcDecode, JobId(2));
        assert!(job_event_is_stale(&latest, &finished(JobKind::LtcDecode, 1)));
    }

    #[test]
    fn fresh_finished_event_passes() {
        let mut latest = std::collections::HashMap::new();
        latest.insert(JobKind::LtcDecode, JobId(2));
        assert!(!job_event_is_stale(&latest, &finished(JobKind::LtcDecode, 2)));
    }

    #[test]
    fn stale_item_event_is_rejected() {
        let mut latest = std::collections::HashMap::new();
        latest.insert(JobKind::DurationProbe, JobId(7));
        assert!(job_event_is_stale(&latest, &item(JobKind::DurationProbe, 6)));
    }

    #[test]
    fn fresh_item_event_passes() {
        let mut latest = std::collections::HashMap::new();
        latest.insert(JobKind::DurationProbe, JobId(7));
        assert!(!job_event_is_stale(&latest, &item(JobKind::DurationProbe, 7)));
    }

    #[test]
    fn event_for_never_spawned_kind_is_stale() {
        let latest: std::collections::HashMap<JobKind, JobId> = std::collections::HashMap::new();
        assert!(job_event_is_stale(&latest, &finished(JobKind::Conversion, 1)));
        assert!(job_event_is_stale(&latest, &item(JobKind::Conversion, 1)));
    }

    // ── EngineLoopState ───────────────────────────────────────────────────

    #[test]
    fn engine_loop_state_defaults() {
        let els = EngineLoopState::new(AppStateSnapshot::initial());
        assert_eq!(els.recovery_attempts, 0);
        assert_eq!(els.log_id_counter, 0);
        assert_eq!(els.last_device_id, None);
        assert_eq!(els.previous_device, None);
        assert_eq!(els.last_auto_applied_ltc_gen, 0);
        assert_eq!(els.last_auto_applied_group_ltc_gen, 0);
        assert_eq!(els.pending_recording, None);
        assert_eq!(els.last_published, None);
        assert_eq!(els.applied_command_seq, 0);
    }

    // ── apply_simple_converter_setting ──────────────────────────────────

    #[test]
    fn simple_converter_setters_map_one_field_each() {
        use crate::command::ConverterCommand;
        let mut s = crate::state::ConverterUserSettings::initial();

        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetMetadataOnly(true)));
        assert!(s.metadata_only);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetGenerateSyntheticVideo(true)));
        assert!(s.generate_synthetic_video);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetCopyVideo(true)));
        assert!(s.copy_video);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetSplitTracks(true)));
        assert!(s.split_tracks);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetDropLtcTrack(true)));
        assert!(s.drop_ltc_track);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetConcatAudio(true)));
        assert!(s.concat_audio);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetStartFromLtc(true)));
        assert!(s.set_start_from_ltc);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetEmbedCameraMetadata(false)));
        assert!(!s.embed_camera_metadata);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetLtcFileIndex(3)));
        assert_eq!(s.ltc_file_idx, 3);
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetVideoCodec("av1".into())));
        assert_eq!(s.video_encoder, "av1");
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetAudioEncoder("aac".into())));
        assert_eq!(s.audio_encoder, "aac");
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetFilenamePrefix("pre".into())));
        assert_eq!(s.filename_prefix, "pre");
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetAudioSuffixTemplate("_a".into())));
        assert_eq!(s.audio_suffix_template, "_a");
        assert!(apply_simple_converter_setting(&mut s, &ConverterCommand::SetVideoSuffixTemplate("_v".into())));
        assert_eq!(s.video_suffix_template, "_v");
    }

    #[test]
    fn special_converter_commands_are_not_simple_setters() {
        use crate::command::ConverterCommand;
        let mut s = crate::state::ConverterUserSettings::initial();
        let before = s.clone();

        assert!(!apply_simple_converter_setting(&mut s, &ConverterCommand::SetContainer("mkv".into())));
        assert!(!apply_simple_converter_setting(&mut s, &ConverterCommand::SwapChannelMapCells(0, 1)));
        assert!(!apply_simple_converter_setting(&mut s, &ConverterCommand::SetOutputFolder(PathBuf::from("/x"))));
        assert!(!apply_simple_converter_setting(&mut s, &ConverterCommand::SelectFolder(PathBuf::from("/x"))));
        assert!(!apply_simple_converter_setting(&mut s, &ConverterCommand::SelectRecording(0)));
        assert!(!apply_simple_converter_setting(&mut s, &ConverterCommand::StartConversion));
        assert!(!apply_simple_converter_setting(&mut s, &ConverterCommand::CancelConversion));
        assert_eq!(s, before, "special commands must not mutate settings in the simple setter");
    }

    // ── job-event handlers ───────────────────────────────────────────────

    fn make_ltc_result() -> audio_core::LtcDetectionResult {
        audio_core::LtcDetectionResult {
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
            chunk_summaries: Vec::new(),
        }
    }

    #[test]
    fn on_conversion_finished_consumes_encoder_used() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        els.current.jobs.insert(JobKind::Conversion, JobStatus::running("conversion"));
        on_conversion_finished(
            &mut els,
            JobOutcome::Succeeded { log: String::new() },
            JobFinal::Conversion { encoder_used: Some("av1_nvenc".into()), steps_attempted: 2 },
        );
        // The Some("av1_nvenc") payload above must be consumed: the handler
        // writes the converter status channel and the job ends Succeeded.
        assert_eq!(els.current.status.last, crate::state::StatusChannel::Converter);
        assert_eq!(els.current.job(JobKind::Conversion).phase(), JobPhase::Succeeded);
    }

    #[test]
    fn on_conversion_finished_encoder_none_keeps_plain_message() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        els.current.jobs.insert(JobKind::Conversion, JobStatus::running("conversion"));
        on_conversion_finished(
            &mut els,
            JobOutcome::Succeeded { log: String::new() },
            JobFinal::Conversion { encoder_used: None, steps_attempted: 1 },
        );
        // encoder_used: None payload still terminates successfully through
        // the converter status channel.
        assert_eq!(els.current.status.last, crate::state::StatusChannel::Converter);
        assert_eq!(els.current.job(JobKind::Conversion).phase(), JobPhase::Succeeded);
    }

    #[test]
    fn on_conversion_finished_failed_sets_error() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        els.current.jobs.insert(JobKind::Conversion, JobStatus::running("conversion"));
        on_conversion_finished(
            &mut els,
            JobOutcome::Failed { error: "boom".into(), log: String::new(), panicked: false },
            JobFinal::Conversion { encoder_used: None, steps_attempted: 1 },
        );
        assert_eq!(els.current.job(JobKind::Conversion).error.as_deref(), Some("boom"));
        assert_eq!(els.current.job(JobKind::Conversion).phase(), JobPhase::Failed);
    }

    #[test]
    fn on_ltc_decode_finished_applies_once_per_generation() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        els.current.converter.groups = vec![MatchedGroup {
            prefix: "T".into(),
            rel_dir: String::new(),
            recording_type: crate::converter::RecordingType::MultiTrackAudio,
            files: vec![PathBuf::from("/tmp/T_S01.wav")],
        }];
        els.current.converter.selected_group_idx = Some(0);
        els.current.decode.generation = 5;
        els.current.decode.generation = 5;

        let payload = || JobFinal::Decode {
            result: Ok(make_ltc_result()),
            path: PathBuf::from("/tmp/T_S01.wav"),
        };
        let outcome = || JobOutcome::Succeeded { log: String::new() };

        on_ltc_decode_finished(&mut els, outcome(), payload());
        assert!(els.current.converter.settings.split_tracks, "first finish auto-applies");
        assert!(els.current.converter.settings.set_start_from_ltc);

        // User un-ticks, then a duplicate event for the same generation arrives.
        els.current.converter.settings.split_tracks = false;
        on_ltc_decode_finished(&mut els, outcome(), payload());
        assert!(!els.current.converter.settings.split_tracks, "latch must prevent re-apply within a generation");

        // New generation → auto-apply again.
        els.current.decode.generation = 6;
        on_ltc_decode_finished(&mut els, outcome(), payload());
        assert!(els.current.converter.settings.split_tracks, "new generation re-applies");
    }

    #[test]
    fn on_ltc_decode_finished_cancel_error_clears_result() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        els.current.jobs.insert(JobKind::LtcDecode, JobStatus::running("decoding"));
        els.current.decode.result = Some(make_ltc_result());
        on_ltc_decode_finished(
            &mut els,
            job::JobOutcome::Cancelled { log: String::new() },
            job::JobFinal::NoPayload,
        );
        assert!(els.current.decode.result.is_none());
        assert!(els.current.decode.error.is_none(), "cancel must not surface as an error");
        assert_eq!(
            els.current.jobs.get(&JobKind::LtcDecode).map(|s| s.progress.phase),
            Some(job::JobPhase::Cancelled),
            "a cancelled decode must end in the Cancelled phase",
        );
    }

    #[test]
    fn on_ltc_decode_finished_failed_error_surfaces() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        els.current.decode.result = Some(make_ltc_result());
        on_ltc_decode_finished(
            &mut els,
            job::JobOutcome::Failed { error: "boom".into(), log: String::new(), panicked: false },
            job::JobFinal::Decode {
                result: Err(LtcDecodeError::Failed("x".into())),
                path: PathBuf::from("/tmp/x.wav"),
            },
        );
        assert!(els.current.decode.error.is_some(), "failures must surface as an error");
        assert!(els.current.decode.result.is_none());
        assert_ne!(
            els.current.jobs.get(&JobKind::LtcDecode).map(|s| s.progress.phase),
            Some(job::JobPhase::Cancelled),
            "a failed decode must not be reported as cancelled",
        );
    }

    #[test]
    fn handle_job_event_ignores_wrong_payload_without_panicking() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        els.current.jobs.insert(JobKind::Conversion, JobStatus::running("conversion"));
        handle_job_event(
            &mut els,
            &mut JobSupervisor::new(),
            JobEvent::Finished {
                job: job::JobId(1),
                kind: JobKind::Conversion,
                outcome: JobOutcome::Succeeded { log: String::new() },
                payload: JobFinal::DurationsDone, // wrong payload for Conversion
            },
        );
        // Outcome is still applied, but the (missing) encoder payload must
        // not produce the completion message.
        assert_eq!(els.current.job(JobKind::Conversion).phase(), JobPhase::Succeeded);
        assert_ne!(
            els.current.status.last,
            crate::state::StatusChannel::Converter,
            "wrong payload must not write the converter status channel",
        );
    }

    #[test]
    fn handle_job_event_item_mismatch_is_ignored_safely() {
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        handle_job_event(
            &mut els,
            &mut JobSupervisor::new(),
            JobEvent::Item {
                job: job::JobId(1),
                kind: JobKind::Conversion, // wrong kind for a DurationResult item
                item: JobItem::DurationResult { path: PathBuf::from("/x.wav"), secs: Some(1.0) },
            },
        );
        assert!(els.current.file_durations.is_empty(), "mismatched item must not be applied");
    }

    // ── validate_stream_channel_selection ────────────────────────────────

    fn probe_with_streams(streams: Vec<AudioStreamInfo>) -> VideoAudioProbe {
        VideoAudioProbe {
            total_audio_channels: streams.iter().map(|s| s.channels).sum(),
            streams,
            is_video_file: true,
        }
    }

    #[test]
    fn validate_stream_selection_accepts_valid_pair() {
        let probe = probe_with_streams(vec![AudioStreamInfo {
            stream_index: 2, channels: 2, codec_name: "pcm_s16le".into(), sample_rate: 48000,
        }]);
        assert!(validate_stream_channel_selection(&probe, 2, 1).is_ok());
    }

    #[test]
    fn validate_stream_selection_rejects_missing_stream() {
        let probe = probe_with_streams(vec![AudioStreamInfo {
            stream_index: 2, channels: 2, codec_name: "pcm_s16le".into(), sample_rate: 48000,
        }]);
        let err = validate_stream_channel_selection(&probe, 0, 0).unwrap_err();
        assert_eq!(err, StreamSelectionError::MissingStream { stream: 0 });
    }

    #[test]
    fn validate_stream_selection_rejects_out_of_range_channel() {
        let probe = probe_with_streams(vec![AudioStreamInfo {
            stream_index: 1, channels: 2, codec_name: "pcm_s16le".into(), sample_rate: 48000,
        }]);
        let err = validate_stream_channel_selection(&probe, 1, 2).unwrap_err();
        assert_eq!(err, StreamSelectionError::ChannelOutOfRange { channel: 2, channels: 2 });
    }

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
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        // ToggleLock on: false → true
        process_command(
            GuiCommand::ToggleLock, &core, true, &event_tx, &mut els, &mut supervisor,
        );
        assert!(els.current.is_locked);
    }

    #[test]
    fn test_process_command_toggle_lock_twice() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::ToggleLock, &core, true, &event_tx, &mut els, &mut supervisor);
        process_command(GuiCommand::ToggleLock, &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(!els.current.is_locked);
    }

    #[test]
    fn test_process_command_set_fps_index_valid() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetFpsIndex(4), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.fps_index, 4);
        assert_eq!(els.current.fps(), 30.0);
        assert!(!els.current.drop_frame());
    }

    #[test]
    fn test_process_command_set_fps_index_drop_frame() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetFpsIndex(3), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.fps_index, 3);
        assert!((els.current.fps() - 29.97).abs() < 0.01);
        assert!(els.current.drop_frame());
    }

    #[test]
    fn test_process_command_set_fps_index_out_of_range() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetFpsIndex(99), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.fps_index, 1);
        assert_eq!(els.current.fps(), 25.0);
    }

    #[test]
    fn test_process_command_set_theme() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetTheme(true), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(els.current.is_dark_theme);

        process_command(GuiCommand::SetTheme(false), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(!els.current.is_dark_theme);
    }

    #[test]
    fn test_process_command_toggle_theme() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::ToggleTheme, &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(els.current.is_dark_theme, "toggle from initial false → true");

        process_command(GuiCommand::ToggleTheme, &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(!els.current.is_dark_theme, "toggle again true → false");
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
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::ClearLogs, &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(els.current.clapper.logs.is_empty());
    }

    #[test]
    fn test_process_command_set_ltc_channel() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetLtcChannel(ChannelSel::Both), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.ltc_channel, ChannelSel::Both);
    }

    #[test]
    fn test_process_command_set_beep_volume() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetBeepVolume(0.75), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!((els.current.beep_volume - 0.75).abs() < 1e-6);
    }

    #[test]
    fn test_process_command_set_beep_channel() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetBeepChannel(ChannelSel::Right), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.beep_channel, ChannelSel::Right);
    }

    #[test]
    fn test_process_command_set_ltc_volume() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetLtcVolume(0.4), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!((els.current.ltc_volume - 0.4).abs() < 1e-6);
    }

    #[test]
    fn test_process_command_set_beep_frequency() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetBeepFrequency(1200.0), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!((els.current.beep_frequency - 1200.0).abs() < 1e-3);
    }

    #[test]
    fn test_process_command_set_beep_duration() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetBeepDuration(0.8), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!((els.current.beep_duration - 0.8).abs() < 1e-6);
    }

    #[test]
    fn test_process_command_set_ltc_decode_stream() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetLtcDecodeStream(2), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.decode.selected_stream, 2);
    }

    #[test]
    fn test_process_command_set_ltc_decode_channel() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetLtcDecodeChannel(1), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.decode.selected_channel, 1);
    }

    #[test]
    fn test_process_command_reset_current_timecode() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        els.current.current_timecode = Timecode { hours: 5, minutes: 4, seconds: 3, frames: 2 };
        process_command(GuiCommand::Reset, &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.current_timecode, els.current.start_timecode);
    }

    #[test]
    fn test_process_command_clap_without_auto_increment() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetAutoIncrement(false), &core, true, &event_tx, &mut els, &mut supervisor);
        let take_before = els.current.clapper.take;
        process_command(GuiCommand::Clap, &core, true, &event_tx, &mut els, &mut supervisor);
        process_command(GuiCommand::Clap, &core, true, &event_tx, &mut els, &mut supervisor);

        assert_eq!(els.current.clapper.logs.len(), 2, "each clap appends one log entry");
        assert_eq!(els.current.clapper.take, take_before, "take must not move when auto-increment is off");
    }

    #[test]
    fn test_process_command_set_start_timecode() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);
        let tc = Timecode { hours: 10, minutes: 20, seconds: 30, frames: 15 };

        process_command(GuiCommand::SetStartTimecode(tc), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.start_timecode, tc);
    }

    #[test]
    fn test_process_command_set_scene_take_roll() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetScene(42), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.scene, 42);

        process_command(GuiCommand::SetTake(7), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.take, 7);

        process_command(GuiCommand::SetRoll("B002".into()), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.roll, "B002");
    }

    #[test]
    fn test_process_command_scene_up_down() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        els.current.clapper.scene = 5;
        process_command(GuiCommand::SceneUp, &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.scene, 6);

        process_command(GuiCommand::SceneDown, &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.scene, 5);
    }

    #[test]
    fn test_process_command_take_up_down() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        els.current.clapper.take = 3;
        process_command(GuiCommand::TakeUp, &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.take, 4);

        process_command(GuiCommand::TakeDown, &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.take, 3);
    }

    #[test]
    fn test_process_command_scene_down_at_zero() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        els.current.clapper.scene = 0;
        process_command(GuiCommand::SceneDown, &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.scene, 0, "scene should not go below 0");
    }

    #[test]
    fn test_process_command_take_down_at_zero() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        els.current.clapper.take = 0;
        process_command(GuiCommand::TakeDown, &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.clapper.take, 0, "take should not go below 0");
    }

    #[test]
    fn test_process_command_set_sample_rate() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetSampleRate(48000), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.sample_rate, 48000);
    }

    #[test]
    fn test_process_command_set_auto_increment() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetAutoIncrement(false), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(!els.current.clapper.auto_increment_take);

        process_command(GuiCommand::SetAutoIncrement(true), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!(els.current.clapper.auto_increment_take);
    }

    #[test]
    fn test_process_command_set_decode_fps_index() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetDecodeFpsIndex(4), &core, true, &event_tx, &mut els, &mut supervisor);
        assert_eq!(els.current.decode.fps_index, 4);
        assert_eq!(els.current.decode_fps(), 30.0);
        assert!(!els.current.decode_drop_frame());
    }

    #[test]
    fn test_process_command_set_decode_fps_index_drop_frame() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetDecodeFpsIndex(3), &core, true, &event_tx, &mut els, &mut supervisor);
        assert!((els.current.decode_fps() - 29.97).abs() < 0.01);
        assert!(els.current.decode_drop_frame());
    }

    #[test]
    fn test_process_command_set_decode_fps_index_out_of_range() {
        let state = setup_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(GuiCommand::SetDecodeFpsIndex(99), &core, true, &event_tx, &mut els, &mut supervisor);
        // Should not change since index is out of range
        assert_eq!(els.current.decode.fps_index, 1);
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
            chunk_summaries: Vec::new(),
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
        let state = setup_decode_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(
            GuiCommand::ClearRecordingDecodeState, &core, true, &event_tx, &mut els, &mut supervisor,
        );

        assert!(els.current.decode.result.is_none(), "single result cleared");
        assert!(els.current.decode.error.is_none(), "error cleared");
        assert!(els.current.decode.probe.is_none(), "probe cleared");
        assert_eq!(els.current.job(JobKind::LtcDecode).phase(), JobPhase::Idle, "LtcDecode job reset to Idle");
        assert_eq!(els.current.decode.generation, 43, "decode gen bumped from 42");
    }

    #[test]
    fn test_clear_recording_decode_state_clears_group_state() {
        let state = setup_decode_state();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(
            GuiCommand::ClearRecordingDecodeState, &core, true, &event_tx, &mut els, &mut supervisor,
        );

        assert!(els.current.decode.group_paths.is_empty(), "group paths cleared");
        assert!(els.current.decode.group_results.is_empty(), "group results cleared");
        assert_eq!(els.current.job(JobKind::LtcGroupDecode).phase(), JobPhase::Idle, "LtcGroupDecode job reset to Idle");
        assert_eq!(els.current.decode.group_generation, 100, "group decode gen bumped from 99");
    }

    #[test]
    fn test_clear_recording_decode_state_generation_noop_when_empty() {
        let state = AppStateSnapshot::initial();
        let core = audio_core::AudioCore::new();
        let mut supervisor = JobSupervisor::new();
        let (event_tx, _event_rx): (std::sync::mpsc::Sender<audio_core::AudioEvent>, std::sync::mpsc::Receiver<audio_core::AudioEvent>) = std::sync::mpsc::channel();
        let mut els = EngineLoopState::new(state);

        process_command(
            GuiCommand::ClearRecordingDecodeState, &core, true, &event_tx, &mut els, &mut supervisor,
        );

        // Should not panic on empty state, just bump generations
        assert_eq!(els.current.decode.generation, 1, "decode gen bumped from 0");
        assert_eq!(els.current.decode.group_generation, 1, "group decode gen bumped from 0");
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
        let mut els = EngineLoopState::new(AppStateSnapshot::initial());
        std::mem::swap(&mut els.current, state);
        let mut supervisor = JobSupervisor::new();
        apply_recording_selection(&mut els, &mut supervisor, idx);
        std::mem::swap(state, &mut els.current);
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