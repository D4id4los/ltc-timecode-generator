//! Unit tests: defaults roundtrip, channel-resolution decision table,
//! preflight runner decision core (injected decode seam), and session
//! settings assembly.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::job::{CancelToken, JobContext, JobFinal, ProgressTracker, UnitSpec};
use crate::converter::RecordingType;
use audio_core::Timecode;

fn tc(h: u32, m: u32, s: u32, f: u32) -> Timecode {
    Timecode { hours: h, minutes: m, seconds: s, frames: f }
}

// ── Defaults store ──────────────────────────────────────────────────────

#[test]
fn defaults_roundtrip() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("session_defaults.json");
    let mut d = SessionConversionDefaults::default();
    d.channels.audio_ltc_channel_override = Some(2);
    d.video.container_override = Some("mkv".to_string());
    d.audio.ltc_file_idx = 3;
    save_session_defaults_to(&path, &d).expect("save");

    let loaded = load_session_defaults_from(&path);
    assert_eq!(loaded, d);
}

#[test]
fn defaults_missing_file_yields_schema_tagged_default() {
    let dir = tempfile::TempDir::new().unwrap();
    let loaded = load_session_defaults_from(&dir.path().join("absent.json"));
    assert_eq!(loaded, SessionConversionDefaults::default());
    assert_eq!(loaded.schema, SESSION_DEFAULTS_SCHEMA);
    assert_eq!(loaded.audio.ltc_file_idx, 1, "owner's TASCAM rig default");
    assert!(loaded.video.split_tracks && loaded.video.drop_ltc_track && loaded.video.set_start_from_ltc);
}

#[test]
fn defaults_corrupt_file_warns_and_falls_back() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("session_defaults.json");
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(b"{ not json").unwrap();
    let loaded = load_session_defaults_from(&path);
    assert_eq!(loaded, SessionConversionDefaults::default());
}

#[test]
fn defaults_unknown_fields_ignored_on_load() {
    let dir = tempfile::TempDir::new().unwrap();
    let path = dir.path().join("session_defaults.json");
    std::fs::write(
        &path,
        format!(
            "{{\"schema\": \"{}\", \"future_field\": 42, \"video\": {{}}, \"audio\": {{}}, \"channels\": {{}}}}",
            SESSION_DEFAULTS_SCHEMA
        ),
    )
    .unwrap();
    let loaded = load_session_defaults_from(&path);
    assert_eq!(loaded, SessionConversionDefaults::default());
}

// ── Channel resolution decision table ──────────────────────────────────

fn cand(stream: usize, channel: usize) -> ChannelCandidate {
    ChannelCandidate { stream, channel }
}

#[test]
fn channel_order_pin_then_configured_then_scan_video() {
    let order = channel_candidate_order(
        RecordingType::VideoClipSequence,
        Some(cand(0, 0)),
        Some(cand(1, 2)),
        &[2, 4],
    );
    assert_eq!(
        order,
        vec![
            (cand(1, 2), ChannelSource::Pinned),
            (cand(0, 0), ChannelSource::Configured),
            // Scan: stream-major, channels within stream, deduped.
            (cand(0, 1), ChannelSource::Scanned),
            (cand(1, 0), ChannelSource::Scanned),
            (cand(1, 1), ChannelSource::Scanned),
            (cand(1, 3), ChannelSource::Scanned),
        ]
    );
}

#[test]
fn channel_order_audio_scans_stream_zero_channels() {
    let order = channel_candidate_order(
        RecordingType::MultiTrackAudio,
        Some(cand(0, 2)),
        None,
        &[4],
    );
    assert_eq!(
        order,
        vec![
            (cand(0, 2), ChannelSource::Configured),
            (cand(0, 0), ChannelSource::Scanned),
            (cand(0, 1), ChannelSource::Scanned),
            (cand(0, 3), ChannelSource::Scanned),
        ]
    );
}

#[test]
fn channel_order_no_config_no_pin_scans_everything() {
    let order = channel_candidate_order(RecordingType::MultiTrackAudio, None, None, &[3]);
    assert_eq!(order.len(), 3);
    assert!(order.iter().all(|(_, s)| *s == ChannelSource::Scanned));
}

#[test]
fn channel_order_no_probe_keeps_only_explicit_candidates() {
    let order = channel_candidate_order(
        RecordingType::VideoClipSequence,
        Some(cand(0, 1)),
        None,
        &[],
    );
    assert_eq!(order, vec![(cand(0, 1), ChannelSource::Configured)]);
}

#[test]
fn channel_order_no_candidates_at_all_is_empty_aborts() {
    let order = channel_candidate_order(RecordingType::MultiTrackAudio, None, None, &[]);
    assert!(order.is_empty(), "no probe + no defaults = nothing to try");
}

#[test]
fn pin_map_is_keyed_by_device_and_overwritten_by_newer_hits() {
    let mut pins = HashMap::new();
    record_channel_pin(&mut pins, "A6700", cand(0, 2));
    assert_eq!(pins.get("A6700"), Some(&cand(0, 2)));
    record_channel_pin(&mut pins, "DR70D", cand(0, 1));
    record_channel_pin(&mut pins, "A6700", cand(0, 3));
    assert_eq!(pins.get("A6700"), Some(&cand(0, 3)), "latest hit wins");
    assert_eq!(pins.get("DR70D"), Some(&cand(0, 1)));
}

// ── Preflight runner (injected decode seam) ─────────────────────────────

fn job_ctx(units: usize) -> (JobContext, ProgressTracker) {
    let tracker = ProgressTracker::new(
        (0..units).map(|i| UnitSpec { weight: 1.0 / units.max(1) as f32, label: format!("u{i}") }),
    );
    let ctx = JobContext {
        progress: tracker.clone(),
        cancel: CancelToken::new(),
        emit: Box::new(|_| {}),
    };
    (ctx, tracker)
}

/// Create real multi-channel silent WAVs so `probe_stream_channels` (a
/// hound header read) sees the channel count.
fn write_multichannel_wavs(dest: &Path, names: &[&str], channels: usize) {
    let spec = hound::WavSpec {
        channels: channels as u16,
        sample_rate: 48000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    for name in names {
        let mut w = hound::WavWriter::create(dest.join(name), spec).unwrap();
        for _ in 0..64 {
            for ch in 0..channels {
                w.write_sample((50 * ch) as i32).unwrap();
            }
        }
        w.flush().unwrap();
    }
}

fn group(kind: RecordingType, dest: &Path, files: &[&str], ltc_idx: usize) -> PreflightGroup {
    PreflightGroup {
        dest: dest.to_path_buf(),
        prefix: "grp".to_string(),
        kind,
        files: files.iter().map(|f| dest.join(f)).collect(),
        ltc_file_idx: ltc_idx,
    }
}

fn ok_result(channel: usize) -> LtcDetectionResult {
    let mut r = LtcDetectionResult::error("");
    r.status = LtcDecodeStatus::Success;
    r.detected_fps = 25.0;
    r.valid_frames = 100;
    r.avg_confidence = 0.9;
    r.timecodes = vec![audio_core::FrameTimecode {
        frame_index: 0,
        timecode: tc(10, 0, 0, channel as u32),
        timecode_secs: 0.0,
    }];
    r
}

fn sink_result() -> LtcDetectionResult {
    let mut r = LtcDetectionResult::error("");
    r.status = LtcDecodeStatus::NoSyncWord;
    r
}

/// The full happy path: first candidate fails (silent ch0), the configured
/// default (ch2) hits; both group files share the resolved channel; the
/// device pin is recorded for the second group with the same device.
#[test]
fn preflight_resolves_via_scan_and_pins_for_siblings() {
    let dir = tempfile::TempDir::new().unwrap();
    write_multichannel_wavs(dir.path(), &["a.wav", "b.wav", "c.wav", "d.wav"], 4);
    let groups = vec![
        group(RecordingType::MultiTrackAudio, dir.path(), &["a.wav", "b.wav"], 1),
        group(RecordingType::MultiTrackAudio, dir.path(), &["c.wav", "d.wav"], 1),
    ];
    let (ctx, tracker) = job_ctx(groups.len());
    // No configured override: group 1 must find LTC by scanning (ch 0 and
    // ch 1 silent), group 2 must reuse the device pin from group 1's hit.
    let defaults = SessionConversionDefaults::default();

    let mut decode = |req: &PreflightDecodeRequest<'_>| -> Result<LtcDetectionResult, PreflightDecodeError> {
        if req.channel == 2 {
            Ok(ok_result(2))
        } else {
            Ok(sink_result())
        }
    };
    let device_of = |_: &[PathBuf]| "DR70D".to_string();

    let out = run_session_preflight_with(
        &ctx,
        &groups,
        &defaults,
        25.0,
        false,
        &mut decode,
        &device_of,
    )
    .expect("preflight completes");
    let JobFinal::SessionPreflight { report } = &out else {
        panic!("expected preflight final, got {out:?}");
    };
    assert!(report.failures.is_empty());
    assert_eq!(report.groups.len(), 2);
    assert_eq!(report.groups[0].files[0].resolved, cand(0, 2));
    assert_eq!(report.groups[0].files[0].source, ChannelSource::Scanned);
    assert_eq!(report.groups[1].files[0].resolved, cand(0, 2));
    assert_eq!(
        report.groups[1].files[0].source,
        ChannelSource::Pinned,
        "sibling files of the same device reuse the pin"
    );
    for g in &report.groups {
        assert_eq!(g.files.len(), 2);
        assert_eq!(g.start_timecode(), Some(tc(10, 0, 0, 2)));
    }
    // Both units terminated full.
    let snap = tracker.snapshot();
    assert!((snap.fraction - 1.0).abs() < 0.01, "termination: {}", snap.fraction);
}

/// Any file still failing after candidate exhaustion → typed `NoLtc`
/// carrying every tried candidate (the prominent-abort contract).
#[test]
fn preflight_aborts_with_typed_noltc_after_candidate_exhaustion() {
    let dir = tempfile::TempDir::new().unwrap();
    write_multichannel_wavs(dir.path(), &["a.wav", "b.wav"], 4);
    let groups = vec![group(RecordingType::MultiTrackAudio, dir.path(), &["a.wav", "b.wav"], 1)];
    let (ctx, _tracker) = job_ctx(groups.len());
    let defaults = SessionConversionDefaults::default();

    let mut decode = |_req: &PreflightDecodeRequest<'_>| Ok(sink_result());
    let device_of =|_: &[PathBuf]| "DR70D".to_string();

    let out = run_session_preflight_with(&ctx, &groups, &defaults, 25.0, false, &mut decode, &device_of)
        .expect("preflight completes");
    let JobFinal::SessionPreflight { report } = &out else {
        panic!("expected preflight final, got {out:?}");
    };
    assert!(report.has_failures(), "NoLtc must block the run");
    assert!(report.groups.is_empty());
    match &report.failures[0] {
        PreflightFailure::NoLtc { tried, .. } => {
            assert_eq!(tried.len(), 4, "every scan candidate recorded: {tried:?}");
        }
        other => panic!("expected NoLtc, got {other:?}"),
    }
}

/// A decode that hard-fails on every candidate surfaces as `Decode`
/// failure, distinct from NoLtc.
#[test]
fn preflight_reports_typed_decode_failure() {
    let dir = tempfile::TempDir::new().unwrap();
    write_multichannel_wavs(dir.path(), &["a.wav"], 2);
    let groups = vec![group(RecordingType::MultiTrackAudio, dir.path(), &["a.wav"], 0)];
    let (ctx, _tracker) = job_ctx(groups.len());
    let defaults = SessionConversionDefaults::default();

    let mut decode =
        |_req: &PreflightDecodeRequest<'_>| -> Result<LtcDetectionResult, PreflightDecodeError> {
            Err(PreflightDecodeError::Failed("boom".to_string()))
        };
    let device_of =|_: &[PathBuf]| "DEV".to_string();

    let out = run_session_preflight_with(&ctx, &groups, &defaults, 25.0, false, &mut decode, &device_of)
        .expect("preflight completes");
    let JobFinal::SessionPreflight { report } = &out else {
        panic!("expected preflight final, got {out:?}");
    };
    assert!(matches!(&report.failures[0], PreflightFailure::Decode { .. }));
}

// ── Session settings assembly ───────────────────────────────────────────

fn preflight_group_result(kind: RecordingType, dest: &Path, files: &[&str]) -> PreflightGroupResult {
    PreflightGroupResult {
        dest: dest.to_path_buf(),
        prefix: "TASCAM_0098".to_string(),
        kind,
        ltc_carrier: dest.join(files.first().unwrap_or(&"")),
        detected_fps: 25.0,
        drop_frame: false,
        files: files
            .iter()
            .map(|f| PreflightFileResult {
                path: dest.join(f),
                device: "DR70D".to_string(),
                resolved: cand(0, 1),
                source: ChannelSource::Pinned,
                start_timecode: Some(tc(11, 22, 33, 0)),
                confidence: Some(0.9),
                valid_frames: 500,
            })
            .collect(),
    }
}

/// Owner decision Q2-audio: the LTC file is the TC source only — it must
/// produce no output. `drop_ltc_track` + `ltc_track_channel_index` is the
/// single home of that rule (asserted via `selected_channel_pairs`).
#[test]
fn audio_session_settings_exclude_the_ltc_file_from_outputs() {
    let dir = tempfile::TempDir::new().unwrap();
    let g = preflight_group_result(
        RecordingType::MultiTrackAudio,
        dir.path(),
        &["S1.wav", "S2.wav", "S3.wav", "S4.wav"],
    );
    let defaults = SessionConversionDefaults::default(); // ltc_file_idx = 1
    let s = assemble_session_group_settings(&g, &defaults, ("wav".into(), "h265".into(), "pcm_s24le".into()));

    assert!(s.drop_ltc_track, "audio pipeline always drops the LTC track");
    assert!(s.split_tracks);
    assert!(s.set_start_from_ltc);
    assert_eq!(s.ltc_track_channel_index, 1);

    // The drop rule: no selected pair may map the LTC input.
    let pairs = crate::converter::planning::selected_channel_pairs(&s, &[]);
    assert_eq!(pairs.len(), 3, "S2 (LTC source) excluded");
    assert!(!pairs.iter().any(|p| p.input_i == 1));
}

#[test]
fn video_session_settings_use_defaults_and_resolved_channel() {
    let dir = tempfile::TempDir::new().unwrap();
    let g = preflight_group_result(RecordingType::VideoClipSequence, dir.path(), &["C0001.MP4"]);
    let mut defaults = SessionConversionDefaults::default();
    defaults.video.container_override = Some("mp4".into());
    defaults.video.video_codec_override = Some("h264".into());
    let s = assemble_session_group_settings(&g, &defaults, ("mov".into(), "h265".into(), "aac".into()));

    assert!(matches!(s.pipeline, ConversionPipeline::VideoPassthrough));
    assert!(!s.copy_video, "session default is full re-encode");
    assert_eq!(s.container, "mp4", "override wins");
    assert_eq!(s.video_encoder, "h264", "override wins");
    assert_eq!(s.audio_encoder, "aac", "absent override → live combination");
    assert_eq!(s.ltc_video_source, Some((0, 1)), "resolved channel from preflight");
    assert!(s.trim_offsets_secs.iter().all(|t| *t == 0.0), "no trim in session pipeline");
    assert_eq!(s.output_folder, g.dest, "deliverables written next to sources");
    assert_eq!(
        s.timecode_meta_per_file[0].as_ref().unwrap().start,
        tc(11, 22, 33, 0),
        "TC metadata from the preflight LTC decode"
    );
}

#[test]
fn video_session_settings_fall_back_to_live_combination() {
    let dir = tempfile::TempDir::new().unwrap();
    let g = preflight_group_result(RecordingType::VideoClipSequence, dir.path(), &["C0001.MP4"]);
    let defaults = SessionConversionDefaults::default();
    let s = assemble_session_group_settings(&g, &defaults, ("mov".into(), "h265".into(), "pcm_s24le".into()));
    assert_eq!(s.container, "mov");
    assert_eq!(s.video_encoder, "h265");
}

// ── Conversion runner (injected group seam) ─────────────────────────────

#[test]
fn session_run_continues_after_encode_failure_and_reports_partial() {
    let dir = tempfile::TempDir::new().unwrap();
    let g1 = preflight_group_result(RecordingType::VideoClipSequence, dir.path(), &["C0001.MP4"]);
    let g2 = preflight_group_result(RecordingType::MultiTrackAudio, dir.path(), &["S1.wav", "S2.wav"]);
    let jobs = vec![
        SessionGroupJob { group: g1, settings: crate::converter::test_fixtures::make_settings_audio_only() },
        SessionGroupJob { group: g2, settings: crate::converter::test_fixtures::make_settings_audio_only() },
    ];
    let (ctx, tracker) = job_ctx(jobs.len());

    let mut run = |_idx: usize,
                   unit: &UnitProgress,
                   _settings: &ConverterSettings,
                   _caps: Option<&crate::converter::FfmpegCapabilities>|
     -> Result<Option<String>, JobError> {
        // Fake encode work: advance the fraction *inside* the seam (the
        // granularity contract's sampling point), then fail the first group.
        unit.set_fraction(0.5);
        unit.set_fraction(1.0);
        Err(JobError::Failed("encoder init failed".to_string()))
    };
    // Second group succeeds — needs a different closure behaviour; emulate
    // by counting calls.
    let calls = AtomicUsize::new(0);
    let mut run = move |_idx: usize,
                        unit: &UnitProgress,
                        _settings: &ConverterSettings,
                        _caps: Option<&crate::converter::FfmpegCapabilities>|
     -> Result<Option<String>, JobError> {
        let n = calls.fetch_add(1, Ordering::SeqCst);
        unit.set_fraction(0.5);
        unit.set_fraction(1.0);
        if n == 0 {
            Err(JobError::Failed("encoder init failed".to_string()))
        } else {
            Ok(Some("h264_qsv".to_string()))
        }
    };

    let out = run_session_conversion_with(&ctx, jobs, None, &mut run).expect("run completes");
    let JobFinal::SessionConversion { summary } = &out else {
        panic!("expected session conversion final, got {out:?}");
    };
    assert!(summary.partial, "encode failure = honest partial, not whole-run failure");
    assert_eq!(summary.outcomes.len(), 2);
    assert!(!summary.outcomes[0].ok);
    assert!(summary.outcomes[0].error.is_some());
    assert!(summary.outcomes[1].ok);
    assert_eq!(summary.outcomes[1].encoder_used.as_deref(), Some("h264_qsv"));
    assert_eq!(summary.completed_count(), 1);
    let snap = tracker.snapshot();
    assert!((snap.fraction - 1.0).abs() < 0.01, "termination: {}", snap.fraction);
}

#[test]
fn session_run_all_success_is_not_partial() {
    let dir = tempfile::TempDir::new().unwrap();
    let g = preflight_group_result(RecordingType::VideoClipSequence, dir.path(), &["C0001.MP4"]);
    let jobs = vec![SessionGroupJob { group: g, settings: crate::converter::test_fixtures::make_settings_audio_only() }];
    let (ctx, _tracker) = job_ctx(jobs.len());
    let mut run = |_idx: usize,
                   unit: &UnitProgress,
                   _s: &ConverterSettings,
                   _c: Option<&crate::converter::FfmpegCapabilities>|
     -> Result<Option<String>, JobError> {
        unit.set_fraction(1.0);
        Ok(None)
    };
    let out = run_session_conversion_with(&ctx, jobs, None, &mut run).expect("run completes");
    let JobFinal::SessionConversion { summary } = &out else {
        panic!("expected session conversion final, got {out:?}");
    };
    assert!(!summary.partial);
}

// ── Unit specs ──────────────────────────────────────────────────────────

#[test]
fn unit_specs_are_byte_weighted_and_cover_every_group() {
    let dir = tempfile::TempDir::new().unwrap();
    let big = dir.path().join("big.wav");
    std::fs::write(&big, vec![0u8; 900]).unwrap();
    let small = dir.path().join("small.wav");
    std::fs::write(&small, vec![0u8; 100]).unwrap();

    let mut g1 = group(RecordingType::MultiTrackAudio, dir.path(), &["x.wav"], 0);
    g1.files = vec![big];
    let mut g2 = group(RecordingType::MultiTrackAudio, dir.path(), &["y.wav"], 0);
    g2.files = vec![small];

    let specs = session_unit_specs(&[g1, g2]);
    assert_eq!(specs.len(), 2);
    assert!(
        specs[0].weight > specs[1].weight * 5.0,
        "byte weighting: big group owns most of the bar ({:?})",
        specs.iter().map(|s| s.weight).collect::<Vec<_>>()
    );
}

#[test]
fn unit_specs_fall_back_to_equal_weights_without_sizes() {
    let g1 = group(RecordingType::MultiTrackAudio, Path::new("/nonexistent"), &["a.wav"], 0);
    let g2 = group(RecordingType::MultiTrackAudio, Path::new("/nonexistent"), &["b.wav"], 0);
    let specs = session_unit_specs(&[g1, g2]);
    assert_eq!(specs[0].weight, specs[1].weight);
    assert!((specs[0].weight + specs[1].weight - 1.0).abs() < 1e-4);
}

// ── Groups collection ───────────────────────────────────────────────────

#[test]
fn collect_preflight_groups_scans_destinations_via_patterns() {
    let dir = tempfile::TempDir::new().unwrap();
    // TASCAM pattern: prefix_S<n>.wav — matches the BUILTIN_PATTERNS[0].
    std::fs::write(dir.path().join("TASCAM_0098S1.wav"), b"x").unwrap();
    std::fs::write(dir.path().join("TASCAM_0098S2.wav"), b"x").unwrap();
    std::fs::write(dir.path().join("unrelated.txt"), b"x").unwrap();

    let groups = collect_preflight_groups(&[dir.path().to_path_buf()], 1);
    assert_eq!(groups.len(), 1, "one TASCAM group, stray files ignored");
    assert_eq!(groups[0].kind, RecordingType::MultiTrackAudio);
    assert_eq!(groups[0].files.len(), 2);
    assert_eq!(groups[0].ltc_file_idx, 1);
    assert_eq!(groups[0].dest, dir.path());
}

#[test]
fn probe_stream_channels_counts_wav_channels() {
    let dir = tempfile::TempDir::new().unwrap();
    let wav = dir.path().join("st.wav");
    let spec = hound::WavSpec {
        channels: 4,
        sample_rate: 48000,
        bits_per_sample: 24,
        sample_format: hound::SampleFormat::Int,
    };
    let mut w = hound::WavWriter::create(&wav, spec).unwrap();
    for _ in 0..16 {
        for ch in 0..4 {
            w.write_sample((100 * ch) as i32).unwrap();
        }
    }
    w.flush().unwrap();
    assert_eq!(
        probe_stream_channels(&wav, &RecordingType::MultiTrackAudio),
        vec![4]
    );
}

#[test]
fn probe_stream_channels_video_requires_ffprobe_or_degrades_empty() {
    // No real ffprobe assertion here (loud skip); the degrade-to-empty path
    // is the portable assertion.
    let missing = Path::new("/nonexistent/no-such-file.mp4");
    let ch = probe_stream_channels(missing, &RecordingType::VideoClipSequence);
    assert!(ch.is_empty(), "unprobeable file must degrade to no candidates");
}

// Silence unused-import warnings for helpers used conditionally.
#[allow(unused)]
fn _unused(p: &Path) -> PathBuf {
    p.to_path_buf()
}
