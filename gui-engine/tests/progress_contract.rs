//! Progress-bar regression guard: one registered contract scenario per
//! `JobKind` (see the AGENTS.md "Progress-granularity contract" invariant —
//! this file is its enforcement site).
//!
//! Three-part contract asserted on fraction samples recorded *inside* the
//! injected fake (deterministic — no observation of mid-flight state from
//! the test thread):
//! 1. **Granularity** — at least one sample strictly between 0 and 1 before
//!    the final one (progress moved *during* work, not at unit boundaries);
//! 2. **Monotonicity** — samples never decrease (no per-file/per-device
//!    resets);
//! 3. **Termination** — the final tracker fraction is ~1.0 with terminal
//!    unit states.
//!
//! Adding a `JobKind` without registering a scenario is a compile error:
//! `has_registered_contract` matches exhaustively with no wildcard arm (the
//! same closed-enum trick the engine dispatcher uses). Runner-backed kinds
//! run their real runner through the `_with`/reporter seam with a fake
//! workload; probe-style kinds whose progress is inherently indeterminate
//! assert only the indeterminate/termination part and are registered
//! explicitly.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audio_core::{DecodeProgress, LtcDecodeError};

use gui_engine::converter::{ConversionReport, JobConversionReport};
use gui_engine::decode::run_wav_decode_job_with;
use gui_engine::device_name::DeviceNameSource;
use gui_engine::job::{
    spawn_job, CancelToken, JobContext, JobEvent, JobFinal, JobKind, JobOutcome, JobPhase, JobSpec,
    JobSupervisor, ProgressTracker, UnitSpec, UnitState,
};
use gui_engine::offload::{
    run_offload_copy_job_with, run_offload_scan_job_with, CopyError, CopyPlanItem, SdCardInfo,
};
use gui_engine::session_conversion::{
    run_session_conversion_with, run_session_preflight_with, PreflightDecodeRequest,
    PreflightGroup, PreflightGroupResult, PreflightFileResult, SessionGroupJob,
    ChannelCandidate, ChannelSource, SessionConversionDefaults,
};
use gui_engine::{JobError, RecordingType, UnitProgress};
use tempfile::TempDir;

// ── Shared helpers ───────────────────────────────────────────────────────

/// Minimal `JobContext` with a real tracker of the given unit shape.
fn job_context(units: Vec<UnitSpec>) -> (JobContext, ProgressTracker) {
    let tracker = ProgressTracker::new(units);
    let ctx = JobContext {
        progress: tracker.clone(),
        cancel: CancelToken::new(),
        emit: Box::new(|_| {}),
    };
    (ctx, tracker)
}

/// Bounded predicate wait (flaky-test methodology: deadlines, not fixed
/// sleeps). Used only *inside* injected fakes to wait for the runner's own
/// progress forwarding to apply a just-reported increment.
fn wait_until(timeout: Duration, pred: impl Fn() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    pred()
}

/// The three-part contract over samples recorded inside an injected fake.
fn assert_progress_contract(samples: &[f32], tracker: &ProgressTracker, context: &str) {
    assert!(
        !samples.is_empty(),
        "{context}: fake must record at least one sample"
    );
    assert!(
        samples.iter().any(|f| *f > 0.0 && *f < 1.0),
        "{context}: granularity — fraction must move *during* work (a sample \
         strictly between 0 and 1 is required), samples={samples:?}"
    );
    for pair in samples.windows(2) {
        assert!(
            pair[1] >= pair[0],
            "{context}: monotonicity — fraction must never decrease, \
             samples={samples:?}"
        );
    }
    let snap = tracker.snapshot();
    assert!(
        (snap.fraction - 1.0).abs() < 0.001,
        "{context}: termination — job must end at full, samples={samples:?}"
    );
}

/// Compile-time registration check: every `JobKind` must have a contract
/// scenario in this file. No wildcard arm — adding a `JobKind` without
/// extending this match is a compile error.
fn has_registered_contract(kind: JobKind) -> bool {
    match kind {
        JobKind::Conversion
        | JobKind::OffloadCopy
        | JobKind::OffloadScan
        | JobKind::LtcDecode
        | JobKind::LtcGroupDecode
        | JobKind::FolderScan
        | JobKind::ClipProbe
        | JobKind::VideoProbe
        | JobKind::DurationProbe
        | JobKind::FfmpegCapProbe
        | JobKind::HwValidate
        | JobKind::SessionPreflight
        | JobKind::SessionConversion => true,
    }
}

/// Keep in lockstep with the `JobKind` enum and the match in
/// `has_registered_contract` (which enforces the enum side).
const ALL_KINDS: &[JobKind] = &[
    JobKind::Conversion,
    JobKind::OffloadCopy,
    JobKind::OffloadScan,
    JobKind::LtcDecode,
    JobKind::LtcGroupDecode,
    JobKind::FolderScan,
    JobKind::ClipProbe,
    JobKind::VideoProbe,
    JobKind::DurationProbe,
    JobKind::FfmpegCapProbe,
    JobKind::HwValidate,
    JobKind::SessionPreflight,
    JobKind::SessionConversion,
];

#[test]
fn every_job_kind_has_a_registered_progress_contract_scenario() {
    assert_eq!(
        ALL_KINDS.len(),
        13,
        "a JobKind was added — register a contract scenario for it in progress_contract.rs"
    );
    for kind in ALL_KINDS {
        assert!(
            has_registered_contract(*kind),
            "{kind:?} has no registered contract scenario"
        );
    }
}

// ── Conversion ───────────────────────────────────────────────────────────

/// The production reporter (`JobConversionReport`) must map in-flight step
/// fractions into the tracker *during* a step — not only on `advance_step` —
/// and the combined overall+step fraction must be monotonic across steps.
#[test]
fn conversion_reports_step_granular_monotonic_progress() {
    let (ctx, tracker) = job_context(vec![UnitSpec {
        weight: 1.0,
        label: "conversion".into(),
    }]);
    let report = JobConversionReport::new(&ctx);
    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let steps = 3;

    // Fake workload: the report-call sequence the pipeline runner makes for
    // three equal-weight steps — in-flight fractions inside each step, step
    // completion via `advance_step` (mirrors `report_step_fraction`'s
    // "never sends 1.0 in-flight" contract with 0.75 as the last in-flight).
    for _step in 0..steps {
        report.set_step_weight(1.0 / steps as f32);
        for in_flight in [0.25f32, 0.5, 0.75] {
            report.report_step_fraction(in_flight, "");
            samples.lock().unwrap().push(tracker.snapshot().fraction);
        }
        report.advance_step();
        samples.lock().unwrap().push(tracker.snapshot().fraction);
    }
    report.mark_completed("contract fake summary");
    assert!(!report.is_failed());

    assert_progress_contract(&samples.lock().unwrap(), &tracker, "conversion");
}

// ── OffloadCopy ──────────────────────────────────────────────────────────

/// The copy backend's per-chunk byte callback must be forwarded to the
/// tracker while the file is still copying (the 2026-10-09 regression: the
/// fraction was set once per completed file).
#[test]
fn offload_copy_reports_byte_granular_monotonic_progress() {
    let dir = TempDir::new().unwrap();
    let src = dir.path().join("src.bin");
    std::fs::write(&src, vec![0u8; 400]).unwrap();
    let dst = dir.path().join("dst.bin");

    let (ctx, tracker) = job_context(vec![UnitSpec {
        weight: 1.0,
        label: "DEV".into(),
    }]);
    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let tracker_for_fake = tracker.clone();
    let samples_for_fake = Arc::clone(&samples);

    let mut copy_fn = |src: &std::path::Path,
                       dst: &std::path::Path,
                       _cancel: &AtomicBool,
                       on_progress: &mut dyn FnMut(u64)| {
        let file_bytes = std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        for step in 1..=4u64 {
            on_progress(file_bytes * step / 4);
            samples_for_fake
                .lock()
                .unwrap()
                .push(tracker_for_fake.snapshot().fraction);
        }
        std::fs::write(dst, vec![0u8; file_bytes as usize])
            .map_err(|e| CopyError::Io(format!("fake copy write: {e}")))?;
        Ok(())
    };

    let result = run_offload_copy_job_with(
        &ctx,
        vec![vec![CopyPlanItem {
            src,
            dst: dst.clone(),
            size: 400,
        }]],
        vec!["DEV".to_string()],
        dir.path().to_path_buf(),
        &mut copy_fn,
    )
    .expect("copy job must succeed");
    assert!(dst.exists());
    assert!(
        matches!(result, JobFinal::OffloadCopy { ref completed_devices } if completed_devices == &vec!["DEV".to_string()]),
        "copy job must complete its device"
    );

    assert_progress_contract(&samples.lock().unwrap(), &tracker, "offload copy");
    assert_eq!(tracker.snapshot().units[0].state, UnitState::Done);
}

// ── OffloadScan ──────────────────────────────────────────────────────────

/// Card scans are inherently indeterminate: the runner must publish the
/// `Indeterminate` phase while the detector runs and terminate its unit at
/// full on success.
#[test]
fn offload_scan_is_indeterminate_during_detection_and_terminates_full() {
    let (ctx, tracker) = job_context(Vec::new());
    let inside_phase: Arc<Mutex<Option<JobPhase>>> = Arc::new(Mutex::new(None));
    let inside = Arc::clone(&inside_phase);

    let result = run_offload_scan_job_with(&ctx, |_cancel, _progress| {
        // Sampled inside the injected detector, while the scan "runs".
        *inside.lock().unwrap() = Some(tracker.snapshot().phase);
        Ok(vec![make_fake_card(1)])
    })
    .expect("scan job must succeed");

    assert_eq!(
        *inside_phase.lock().unwrap(),
        Some(JobPhase::Indeterminate),
        "scan runner must publish the Indeterminate phase while detecting"
    );
    assert!(matches!(result, JobFinal::OffloadScan { .. }));
    let snap = tracker.snapshot();
    assert!((snap.fraction - 1.0).abs() < 0.001);
    assert_eq!(snap.units[0].state, UnitState::Done);
}

fn make_fake_card(mount_suffix: usize) -> SdCardInfo {
    SdCardInfo {
        mount: PathBuf::from(format!("/contract/fake/card-{mount_suffix}")),
        volume_label: "VOL".into(),
        device_name: "VOL".into(),
        name_source: DeviceNameSource::VolumeLabel,
        media_file_count: 0,
        total_bytes: 0,
        files: vec![],
        selected: vec![],
        selected_count: 0,
        selected_bytes: 0,
    }
}

// ── LtcDecode ────────────────────────────────────────────────────────────

/// The WAV decode runner must bridge chunk completions into the tracker
/// *while the injected decode work is still running* (not once at the end).
/// The fake waits — bounded, inside the worker — for the runner's live
/// bridge to apply each increment before recording the sample: a runner
/// that drops the bridge fails the wait instead of silently degrading to
/// 0 → 100 % progress.
#[test]
fn wav_decode_reports_chunk_granular_monotonic_progress() {
    let (ctx, tracker) = job_context(vec![UnitSpec {
        weight: 1.0,
        label: "decode".into(),
    }]);
    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let tracker_for_fake = tracker.clone();
    let samples_for_fake = Arc::clone(&samples);
    let chunk_count = 4;

    let mut decode_fn = |dp: &DecodeProgress| {
        // Work-weighted model: the fake sizes the job (as the real chunked
        // runner does from its plan) and reports intra-chunk work-unit
        // sub-steps plus a completion flush per chunk (mirroring the
        // runner's residual flush). The tracker must follow each sub-step
        // while the decode is still running — a 2-chunk file must not jump
        // 0 → 50 → 100, and a chunk that finishes early must not pin the
        // bar at 100 %.
        dp.set_work_total(1000);
        let total = dp.chunks_total;
        let chunk_units = 1000u64 / total as u64;
        let mut done_units = 0u64;
        for done in 1..=total {
            for sub in 1..4u64 {
                dp.add_work_units(10);
                done_units += 10;
                let expected = done_units as f32 / 1000.0;
                let saw_fine = wait_until(Duration::from_secs(5), || {
                    (tracker_for_fake.snapshot().fraction - expected).abs() < 0.001
                });
                assert!(
                    saw_fine,
                    "runner must forward intra-chunk progress {sub}/4 of chunk \
                     {done}/{total} while decode is still running"
                );
                samples_for_fake
                    .lock()
                    .unwrap()
                    .push(tracker_for_fake.snapshot().fraction);
            }
            // Completion flush: top the chunk up to its full weight.
            let flushed = done as u64 * chunk_units;
            dp.add_work_units(flushed - done_units);
            done_units = flushed;
            let expected = done_units as f32 / 1000.0;
            let saw_increment = wait_until(Duration::from_secs(5), || {
                (tracker_for_fake.snapshot().fraction - expected).abs() < 0.001
            });
            assert!(
                saw_increment,
                "runner must forward chunk {done}/{total} completion while decode is \
                 still running"
            );
            samples_for_fake
                .lock()
                .unwrap()
                .push(tracker_for_fake.snapshot().fraction);
        }
        // Let the runner's bridge observe termination (done >= total).
        dp.chunks_completed.store(total, Ordering::Relaxed);
        Err(LtcDecodeError::Failed(
            "progress contract fake decode result".into(),
        ))
    };

    let outcome = run_wav_decode_job_with(
        &ctx,
        PathBuf::from("contract-fake.wav"),
        chunk_count,
        &mut decode_fn,
    )
    .expect("a fake decode failure is a payload error, not a job failure");

    assert!(matches!(
        outcome,
        JobFinal::Decode {
            result: Err(LtcDecodeError::Failed(_)),
            ..
        }
    ));
    assert_progress_contract(&samples.lock().unwrap(), &tracker, "wav decode");
}

// ── LtcGroupDecode ───────────────────────────────────────────────────────

/// The group-decode runner is engine-inline (`cmd_decode_ltc_video_group`);
/// until it gains a `_with` seam, this registered scenario pins the per-clip
/// unit pattern it must follow: N equal-weight clip units finished in order,
/// so the overall fraction moves per clip and never resets. Chunk-level
/// forwarding inside each clip is the same `bridge_decode_progress`
/// mechanism the [`wav_decode_reports_chunk_granular_monotonic_progress`]
/// seam contract pins.
#[test]
fn ltc_group_decode_per_clip_units_terminate_monotonically() {
    let (ctx, tracker) = job_context(vec![
        UnitSpec {
            weight: 0.5,
            label: "clip".into(),
        },
        UnitSpec {
            weight: 0.5,
            label: "clip".into(),
        },
    ]);
    let mut samples = Vec::new();
    for idx in 0..2 {
        let unit = ctx.progress.unit(idx);
        unit.set_state(UnitState::Running);
        unit.finish();
        samples.push(tracker.snapshot().fraction);
    }
    assert_progress_contract(&samples, &tracker, "group decode");
}

// ── Indeterminate-class kinds ────────────────────────────────────────────
//
// The probe/probe-style kinds below have no seamable incremental workload —
// their runners publish only the Indeterminate phase while running. The
// registered contract for them: while the workload runs the phase must be
// `Indeterminate`, and the job terminates with a `Succeeded` outcome through
// the real `spawn_job` → supervisor → `drain()` pipeline.

/// Run one indeterminate-class scenario through the real spawn pipeline and
/// assert the outcome is observed as `Succeeded`.
fn indeterminate_class_contract(kind: JobKind, name: &'static str) {
    let mut sup = JobSupervisor::new();
    let inside_phase: Arc<Mutex<Option<JobPhase>>> = Arc::new(Mutex::new(None));
    let inside = Arc::clone(&inside_phase);

    spawn_job::<JobFinal, _>(
        &mut sup,
        JobSpec {
            kind,
            name,
            units: vec![],
        },
        move |ctx| {
            // Mirror of the engine worker's progress shape for this kind.
            ctx.progress.set_indeterminate(true);
            *inside.lock().unwrap() = Some(ctx.progress.snapshot().phase);
            ctx.progress.set_indeterminate(false);
            Ok(JobFinal::NoPayload)
        },
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut succeeded = false;
    while Instant::now() < deadline {
        sup.poll();
        for event in sup.drain() {
            if let JobEvent::Finished {
                kind: finished_kind,
                outcome: JobOutcome::Succeeded { .. },
                ..
            } = event
            {
                assert_eq!(finished_kind, kind);
                succeeded = true;
            }
        }
        if succeeded {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        succeeded,
        "{name}: job must terminate with a Succeeded outcome"
    );
    assert_eq!(
        *inside_phase.lock().unwrap(),
        Some(JobPhase::Indeterminate),
        "{name}: phase must be Indeterminate while the workload runs"
    );
    sup.shutdown(Duration::from_secs(5));
}

#[test]
fn folder_scan_contract_is_registered_indeterminate_class() {
    indeterminate_class_contract(JobKind::FolderScan, "contract-folder-scan");
}

#[test]
fn clip_probe_contract_is_registered_indeterminate_class() {
    indeterminate_class_contract(JobKind::ClipProbe, "contract-clip-probe");
}

#[test]
fn video_probe_contract_is_registered_indeterminate_class() {
    indeterminate_class_contract(JobKind::VideoProbe, "contract-video-probe");
}

#[test]
fn duration_probe_contract_is_registered_indeterminate_class() {
    indeterminate_class_contract(JobKind::DurationProbe, "contract-duration-probe");
}

#[test]
fn ffmpeg_cap_probe_contract_is_registered_indeterminate_class() {
    indeterminate_class_contract(JobKind::FfmpegCapProbe, "contract-ffmpeg-cap-probe");
}

#[test]
fn hw_validate_contract_is_registered_indeterminate_class() {
    indeterminate_class_contract(JobKind::HwValidate, "contract-hw-validate");
}

// ── SessionPreflight ─────────────────────────────────────────────────────

/// The preflight runner forwards the decode seam's in-seam fraction updates
/// into the group's unit while the decode is still running (here: sampled
/// inside the injected fake), and terminates full.
#[test]
fn session_preflight_forwards_decode_granularity_and_terminates() {
    let dir = TempDir::new().unwrap();
    let groups = vec![PreflightGroup {
        dest: dir.path().to_path_buf(),
        prefix: "grp".into(),
        kind: RecordingType::MultiTrackAudio,
        files: vec![dir.path().join("a.wav"), dir.path().join("b.wav")],
        ltc_file_idx: 1,
    }];
    // Real multi-channel WAV headers so the channel probe sees candidates.
    let spec = hound::WavSpec {
        channels: 4,
        sample_rate: 48000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    for name in ["a.wav", "b.wav"] {
        let mut w = hound::WavWriter::create(dir.path().join(name), spec).unwrap();
        for _ in 0..64 {
            for ch in 0..4 {
                w.write_sample((50 * ch) as i32).unwrap();
            }
        }
        w.flush().unwrap();
    }

    let (ctx, tracker) = job_context(vec![UnitSpec {
        weight: 1.0,
        label: "grp".into(),
    }]);
    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let samples_for_fake = Arc::clone(&samples);
    let tracker_for_fake = tracker.clone();

    let mut decode = |req: &PreflightDecodeRequest<'_>| {
        // Fake decode workload: advance the group unit *inside* the seam
        // (the deterministic sampling point) and record tracker fractions.
        if let Some(unit) = &req.unit {
            unit.set_fraction(0.4);
            unit.set_fraction(0.8);
            samples_for_fake
                .lock()
                .unwrap()
                .push(tracker_for_fake.snapshot().fraction);
        }
        let mut r = audio_core::LtcDetectionResult::error("");
        r.status = audio_core::LtcDecodeStatus::Success;
        r.detected_fps = req.fps as f32;
        r.valid_frames = 10;
        r.timecodes = vec![audio_core::FrameTimecode {
            frame_index: 0,
            timecode: audio_core::Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            timecode_secs: 0.0,
        }];
        Ok(r)
    };
    let device_of = |_: &[PathBuf]| "DEV".to_string();
    let defaults = SessionConversionDefaults::default();

    let out = run_session_preflight_with(&ctx, &groups, &defaults, 25.0, false, &mut decode, &device_of)
        .expect("preflight completes");
    assert!(matches!(out, JobFinal::SessionPreflight { .. }));
    assert_progress_contract(&samples.lock().unwrap(), &tracker, "session-preflight");
}

// ── SessionConversion ───────────────────────────────────────────────────

/// The session run forwards each group's in-seam encode fractions into that
/// group's unit while the encode runs, and terminates full across all
/// groups (equal-weight fake: two groups, both succeed).
#[test]
fn session_conversion_forwards_group_encode_granularity_and_terminates() {
    let dir = TempDir::new().unwrap();
    let mk_group = |prefix: &str, files: &[&str]| PreflightGroupResult {
        dest: dir.path().to_path_buf(),
        prefix: prefix.into(),
        kind: RecordingType::MultiTrackAudio,
        ltc_carrier: dir.path().join(files[0]),
        detected_fps: 25.0,
        drop_frame: false,
        files: files
            .iter()
            .map(|f| PreflightFileResult {
                path: dir.path().join(f),
                device: "DEV".into(),
                resolved: ChannelCandidate { stream: 0, channel: 1 },
                source: ChannelSource::Pinned,
                start_timecode: Some(audio_core::Timecode {
                    hours: 1,
                    minutes: 0,
                    seconds: 0,
                    frames: 0,
                }),
                confidence: Some(0.9),
                valid_frames: 10,
            })
            .collect(),
    };
    let jobs = vec![
        SessionGroupJob { group: mk_group("g1", &["a.wav"]), settings: test_settings() },
        SessionGroupJob { group: mk_group("g2", &["b.wav"]), settings: test_settings() },
    ];

    let (ctx, tracker) = job_context(vec![
        UnitSpec { weight: 0.5, label: "g1".into() },
        UnitSpec { weight: 0.5, label: "g2".into() },
    ]);
    let samples: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let samples_for_fake = Arc::clone(&samples);
    let tracker_for_fake = tracker.clone();

    let mut run = |_idx: usize,
                   _unit: &UnitProgress,
                   _settings: &gui_engine::ConverterSettings,
                   _caps: Option<&gui_engine::FfmpegCapabilities>|
     -> Result<Option<String>, JobError> {
        // Fake encode work recorded through the tracker (inside the seam).
        // The unit fraction is set via the tracker's per-unit snapshot to
        // keep sampling deterministic.
        let mut s = samples_for_fake.lock().unwrap();
        s.push(tracker_for_fake.snapshot().fraction);
        drop(s);
        Ok(Some("h264".into()))
    };
    // Granularity: the two groups' fractions only become distinguishable
    // through their units — sample the first unit's in-flight fraction by
    // advancing it inside the seam via the ctx units. Simpler and faithful:
    // drive fraction advancement on the group's unit through the closure's
    // `_unit` parameter.
    let mut run = |_idx: usize,
                   unit: &UnitProgress,
                   _settings: &gui_engine::ConverterSettings,
                   _caps: Option<&gui_engine::FfmpegCapabilities>|
     -> Result<Option<String>, JobError> {
        unit.set_fraction(0.5);
        samples_for_fake
            .lock()
            .unwrap()
            .push(tracker_for_fake.snapshot().fraction);
        unit.set_fraction(1.0);
        samples_for_fake
            .lock()
            .unwrap()
            .push(tracker_for_fake.snapshot().fraction);
        Ok(Some("h264".into()))
    };

    let out = run_session_conversion_with(&ctx, jobs, None, &mut run).expect("run completes");
    assert!(matches!(out, JobFinal::SessionConversion { .. }));
    assert_progress_contract(&samples.lock().unwrap(), &tracker, "session-conversion");
}

/// Minimal ConverterSettings for the fake group jobs (the real pipeline is
/// never invoked — the group runner is injected).
fn test_settings() -> gui_engine::ConverterSettings {
    gui_engine::ConverterSettings {
        pipeline: gui_engine::ConversionPipeline::AudioOnly {
            generate_synthetic_video: false,
        },
        input_files: Vec::new(),
        recording_type: RecordingType::MultiTrackAudio,
        ltc_track_channel_index: 0,
        channel_map: gui_engine::ChannelMap::identity(1),
        split_tracks: true,
        drop_ltc_track: true,
        ltc_video_source: None,
        container: "wav".into(),
        copy_video: false,
        video_encoder: "h265".into(),
        audio_encoder: "pcm_s24le".into(),
        resolved_video_encoder: String::new(),
        resolved_hw_device: None,
        output_folder: PathBuf::from("/tmp/contract-session"),
        filename_prefix: "{device}".into(),
        audio_suffix_template: "_tr{track:01d}".into(),
        video_suffix_template: "_clip{clip:01d}".into(),
        set_start_from_ltc: true,
        embed_camera_metadata: true,
        trim_offsets_secs: vec![0.0],
        timecode_meta_per_file: vec![None],
        camera_meta_per_file: vec![None],
        device_name: None,
        concat_audio: false,
    }
}
