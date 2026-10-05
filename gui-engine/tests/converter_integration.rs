use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gui_engine::converter::{
    query_ffmpeg_capabilities, run_conversion, ChannelMap, ConversionPipeline, ConversionReport,
    ConverterSettings, RecordingType, TestReport,
};
use gui_engine::video_codecs::resolve_encoder_chain;

/// Create a short PCM 16-bit mono WAV file with silence.
fn create_test_wav(path: &Path, sample_rate: u32, duration_secs: f64, amplitude: i16) {
    let num_samples = (sample_rate as f64 * duration_secs) as u32;
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).unwrap();
    for _ in 0..num_samples {
        writer.write_sample(amplitude).unwrap();
    }
    writer.finalize().unwrap();
}

// ── Tests ────────────────────────────────────────────────────────────────

#[test]
fn test_conversion_progress_tracking() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available");
        return;
    }
    if resolve_encoder_chain("h264", &caps).is_empty() {
        eprintln!("--- SKIPPED: no H.264 encoder available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");

    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let settings = make_test_settings(dir.path(), vec![wav1, wav2]);

    let report = TestReport::new();
    let (_encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        *report.completed.lock().unwrap(),
        "Expected Completed, got failed={}",
        *report.failed.lock().unwrap(),
    );

    // The runner normalizes step weights to 1.0; a completed conversion must
    // have driven the report's progress to full (0.99 guards float error).
    assert!(
        *report.progress.lock().unwrap() >= 0.99,
        "progress must reach ~1.0 after completion, got {}",
        *report.progress.lock().unwrap()
    );

    let expected_output = dir.path().join("test_video_clip01.mkv");
    assert!(
        expected_output.exists(),
        "Output file was not created: {} (dir contents: {:?})",
        expected_output.display(),
        std::fs::read_dir(dir.path())
            .map(|e| e
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect::<Vec<_>>())
            .unwrap_or_default(),
    );
    assert!(
        expected_output
            .metadata()
            .map(|m| m.len() > 0)
            .unwrap_or(false),
        "Output file is empty: {}",
        expected_output.display()
    );
}

#[test]
fn test_conversion_cancellation() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available (test_conversion_cancellation)");
        return;
    }
    if resolve_encoder_chain("h264", &caps).is_empty() {
        eprintln!("--- SKIPPED: no H.264 encoder available (test_conversion_cancellation)");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");

    create_test_wav(&wav1, 48000, 60.0, 8000);
    create_test_wav(&wav2, 48000, 60.0, -8000);

    let settings = make_test_settings(dir.path(), vec![wav1, wav2]);

    let report = TestReport::new();

    // Spawn a canceller thread that first *observes* conversion start —
    // progress > 0 means the first ffmpeg step is running and streaming
    // progress (the pre-step "Pipeline:" message alone is too early: a
    // cancel before the ffmpeg spawn returns without the CANCELLED log
    // block). Then it raises the cancel flag. On a loaded machine the whole
    // conversion could otherwise finish inside a fixed delay, so the flag
    // would rise after the run.
    let report_for_canceller = report.clone();
    let cancel_flag = report.cancelled.clone();
    let started = Arc::new(AtomicBool::new(false));
    let started_c = started.clone();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if *report_for_canceller.progress.lock().unwrap() > 0.0 {
                started_c.store(true, Ordering::Relaxed);
                break;
            }
            if Instant::now() > deadline {
                break; // started stays false → post-hoc assert fails loudly
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cancel_flag.store(true, Ordering::Relaxed);
    });

    let (_encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        started.load(Ordering::Relaxed),
        "conversion never signalled start within 30s — cannot exercise cancellation",
    );

    // The conversion should have been cancelled
    let log_msg = report.log.lock().unwrap().clone();
    assert!(
        log_msg.contains("CANCELLED"),
        "Expected cancellation message in log. Log:\n{}",
        log_msg,
    );
}

fn make_test_settings(dir: &Path, input_files: Vec<std::path::PathBuf>) -> ConverterSettings {
    ConverterSettings {
        pipeline: ConversionPipeline::AudioOnly {
            generate_synthetic_video: true,
        },
        input_files,
        recording_type: RecordingType::MultiTrackAudio,
        ltc_track_channel_index: 0,
        channel_map: ChannelMap::identity(2),
        split_tracks: false,
        drop_ltc_track: false,
        ltc_video_source: None,
        container: "mkv".to_string(),
        copy_video: false,
        video_encoder: "h264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: dir.to_path_buf(),
        filename_prefix: "test".to_string(),
        audio_suffix_template: "_audio_track{track:01d}".to_string(),
        video_suffix_template: "_video_clip{clip:02d}".to_string(),
        set_start_from_ltc: false,
        embed_camera_metadata: true,
        trim_offsets_secs: vec![0.0, 0.0],
        timecode_meta_per_file: vec![None, None],
        camera_meta_per_file: vec![None; 2],
        device_name: None,
        concat_audio: false,
        resolved_hw_device: None,
    }
}

#[test]
fn test_conversion_resolves_and_reports_encoder() {
    let mut caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available");
        return;
    }
    if !caps.available_encoders.contains("libx264") {
        eprintln!("--- SKIPPED: libx264 encoder not available");
        return;
    }
    caps.available_encoders =
        std::collections::BTreeSet::from(["libx264".to_string(), "pcm_s24le".to_string()]);

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");
    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let settings = make_test_settings(dir.path(), vec![wav1, wav2]);

    let report = TestReport::new();
    let (encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        *report.completed.lock().unwrap(),
        "Expected Completed, got failed={}",
        *report.failed.lock().unwrap(),
    );
    assert!(
        encoder_used == Some("libx264".to_string()),
        "Expected encoder 'libx264', got {:?}",
        encoder_used,
    );
}

/// An unknown codec id yields a single dead candidate; the fallback runner
/// must exhaust it.
#[test]
fn test_conversion_fails_when_no_encoder_candidate_exists() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");
    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let mut settings = make_test_settings(dir.path(), vec![wav1, wav2]);
    settings.video_encoder = "definitely_not_a_codec".to_string();

    let report = TestReport::new();
    let (_encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        *report.failed.lock().unwrap(),
        "Expected Failed for unknown codec, got completed={}",
        *report.completed.lock().unwrap(),
    );
}

/// Create a short MP4 video with a pure-tone audio track using ffmpeg.
fn create_test_video_with_tone(
    path: &std::path::Path,
    duration_secs: f64,
    frequency: u32,
    sample_rate: u32,
) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=blue:s=320x240:r=25:duration={}", duration_secs),
            "-f",
            "lavfi",
            "-i",
            &format!(
                "sine=frequency={}:duration={}:sample_rate={}",
                frequency, duration_secs, sample_rate
            ),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "pcm_s16le",
            "-shortest",
            "-t",
            &format!("{}", duration_secs),
            &path.to_string_lossy(),
        ])
        .status()
        .expect("failed to spawn ffmpeg for test video");
    assert!(
        status.success(),
        "ffmpeg fixture creation failed for {}",
        path.display()
    );
}

#[test]
fn test_concat_audio_across_two_video_clips() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available");
        return;
    }
    if resolve_encoder_chain("h264", &caps).is_empty() && !caps.available_encoders.contains("mpeg4")
    {
        eprintln!("--- SKIPPED: no suitable video encoder");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let clip1 = dir.path().join("clip1.mp4");
    let clip2 = dir.path().join("clip2.mp4");

    create_test_video_with_tone(&clip1, 0.3, 440, 48000);
    create_test_video_with_tone(&clip2, 0.2, 880, 48000);

    let settings = ConverterSettings {
        pipeline: ConversionPipeline::VideoPassthrough,
        input_files: vec![clip1, clip2],
        recording_type: RecordingType::VideoClipSequence,
        ltc_track_channel_index: 0,
        channel_map: ChannelMap::identity(1),
        split_tracks: true,
        drop_ltc_track: false,
        ltc_video_source: None,
        container: "mkv".to_string(),
        copy_video: false,
        video_encoder: "h264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: dir.path().to_path_buf(),
        filename_prefix: "concat_test".to_string(),
        audio_suffix_template: "_audio_track{track:01d}".to_string(),
        video_suffix_template: "_video_clip{clip:02d}".to_string(),
        set_start_from_ltc: false,
        embed_camera_metadata: true,
        trim_offsets_secs: vec![0.0, 0.0],
        timecode_meta_per_file: vec![None, None],
        camera_meta_per_file: vec![None; 2],
        device_name: None,
        concat_audio: true,
        resolved_hw_device: None,
    };

    let report = TestReport::new();
    let (_encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        *report.completed.lock().unwrap(),
        "Expected Completed, got failed={}",
        *report.failed.lock().unwrap(),
    );

    // Per-step weight must be exactly 1.0 / steps.len() — ported from the
    // deleted runner-level concat test (WP-T6 §4.1).
    let sw: f32 = report.step_weight();
    assert!(sw > 0.0, "a step weight must have been published");
    let steps = 1.0 / sw;
    assert!(
        (steps - steps.round()).abs() < 0.06,
        "step weight must be 1.0/N (got {} → {} steps)",
        sw,
        steps
    );

    let concat_audio = dir.path().join("concat_test_audio_track1.wav");
    assert!(
        concat_audio.exists(),
        "Concatenated audio file not created: {} (dir: {:?})",
        concat_audio.display(),
        std::fs::read_dir(dir.path())
            .map(|e| e
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect::<Vec<_>>())
            .unwrap_or_default(),
    );

    if let Ok(reader) = hound::WavReader::open(&concat_audio) {
        let spec = reader.spec();
        let num_samples = reader.duration() as u64;
        let expected_samples = ((0.3 + 0.2) * spec.sample_rate as f64).round() as u64;
        let tolerance = (spec.sample_rate as f64 * 0.05) as u64;
        assert!(
            num_samples.abs_diff(expected_samples) <= tolerance,
            "Expected ~{} samples, got {} (tolerance: {})",
            expected_samples,
            num_samples,
            tolerance,
        );
        assert_eq!(spec.channels, 1, "concatenated audio should be mono");
    } else {
        panic!(
            "Could not open concatenated WAV: {}",
            concat_audio.display()
        );
    }
}

#[test]
fn test_metadata_only_concat_audio_across_two_video_clips() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let clip1 = dir.path().join("clip1.mp4");
    let clip2 = dir.path().join("clip2.mp4");

    create_test_video_with_tone(&clip1, 0.3, 440, 48000);
    create_test_video_with_tone(&clip2, 0.2, 880, 48000);

    let settings = ConverterSettings {
        pipeline: ConversionPipeline::MetadataOnly,
        input_files: vec![clip1.clone(), clip2.clone()],
        recording_type: RecordingType::VideoClipSequence,
        ltc_track_channel_index: 0,
        channel_map: ChannelMap::identity(1),
        split_tracks: true,
        drop_ltc_track: false,
        ltc_video_source: None,
        container: "mkv".to_string(),
        copy_video: false,
        video_encoder: "h264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: dir.path().to_path_buf(),
        filename_prefix: "{filename}".to_string(),
        audio_suffix_template: "_audio_track{track:01d}".to_string(),
        video_suffix_template: "_video_clip{clip:02d}".to_string(),
        set_start_from_ltc: false,
        embed_camera_metadata: false,
        trim_offsets_secs: vec![0.0, 0.0],
        timecode_meta_per_file: vec![None, None],
        camera_meta_per_file: vec![None; 2],
        device_name: None,
        concat_audio: true,
        resolved_hw_device: None,
    };

    let report = TestReport::new();
    let (_encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        *report.completed.lock().unwrap(),
        "Expected Completed, got failed={}",
        *report.failed.lock().unwrap(),
    );

    let audio_files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .map(|ext| ext == "wav")
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        audio_files.len(),
        1,
        "Expected exactly 1 concatenated audio file, got {} (files: {:?})",
        audio_files.len(),
        audio_files
            .iter()
            .map(|e| e.path().display().to_string())
            .collect::<Vec<_>>(),
    );

    let concat_path = audio_files[0].path();
    let reader = hound::WavReader::open(&concat_path).expect("Could not open concatenated WAV");
    let spec = reader.spec();
    let num_samples = reader.duration() as u64;
    let expected_samples = ((0.3 + 0.2) * spec.sample_rate as f64).round() as u64;
    let tolerance = (spec.sample_rate as f64 * 0.05) as u64;
    assert!(
        num_samples.abs_diff(expected_samples) <= tolerance,
        "Expected ~{} samples, got {} (tolerance: {})",
        expected_samples,
        num_samples,
        tolerance,
    );

    let renamed1 = dir.path().join("clip1_video_clip01.mp4");
    let renamed2 = dir.path().join("clip2_video_clip02.mp4");
    assert!(
        renamed1.exists(),
        "Renamed clip1 not found: {}",
        renamed1.display()
    );
    assert!(
        renamed2.exists(),
        "Renamed clip2 not found: {}",
        renamed2.display()
    );

    assert!(!clip1.exists(), "Original clip1 should have been renamed");
    assert!(!clip2.exists(), "Original clip2 should have been renamed");
}

// ── Stream-copy mode ("Leave Video Encoding Untouched") ─────────────────

/// Create an MP4 with MPEG-4 Part 2 video and a known keyframe interval.
fn create_test_video_with_gop(path: &std::path::Path, duration_secs: f64, gop: u32) {
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-v",
            "error",
            "-f",
            "lavfi",
            "-i",
            &format!("color=c=blue:s=320x240:r=25:duration={}", duration_secs),
            "-f",
            "lavfi",
            "-i",
            &format!(
                "sine=frequency=440:duration={}:sample_rate=48000",
                duration_secs
            ),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "mpeg4",
            "-g",
            &gop.to_string(),
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-shortest",
            "-t",
            &format!("{}", duration_secs),
            &path.to_string_lossy(),
        ])
        .status()
        .expect("failed to spawn ffmpeg for test video");
    assert!(
        status.success(),
        "ffmpeg fixture creation failed for {}",
        path.display()
    );
}

fn probe_duration_secs(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "csv=p=0",
            &path.to_string_lossy(),
        ])
        .output()
        .expect("failed to spawn ffprobe");
    assert!(
        out.status.success(),
        "ffprobe failed for {}",
        path.display()
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .expect("ffprobe duration not a number")
}

fn probe_stream_codecs(path: &Path) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type",
            "-of",
            "csv=p=0",
            &path.to_string_lossy(),
        ])
        .output()
        .expect("failed to spawn ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .flat_map(|l| {
            l.trim()
                .split(',')
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        })
        .filter(|l| !l.is_empty() && l != "unknown")
        .collect()
}

#[test]
fn test_copy_mode_streams_video_and_derives_container() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let clip = dir.path().join("C0001.MP4");
    create_test_video_with_gop(&clip, 4.0, 50);

    assert_eq!(
        gui_engine::ffprobe::snap_trim_to_keyframe(&clip, 2.5),
        2.0,
        "trim must snap to the last keyframe at-or-before the offset"
    );

    let settings = ConverterSettings {
        pipeline: ConversionPipeline::VideoPassthrough,
        input_files: vec![clip],
        recording_type: RecordingType::VideoClipSequence,
        ltc_track_channel_index: 0,
        channel_map: ChannelMap::identity(1),
        split_tracks: false,
        drop_ltc_track: false,
        ltc_video_source: None,
        container: "mkv".to_string(),
        copy_video: true,
        video_encoder: "weird-codec".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: dir.path().to_path_buf(),
        filename_prefix: "copytest".to_string(),
        audio_suffix_template: "_audio_track{track:01d}".to_string(),
        video_suffix_template: "_video_clip{clip:02d}".to_string(),
        set_start_from_ltc: true,
        embed_camera_metadata: true,
        trim_offsets_secs: vec![2.5],
        timecode_meta_per_file: vec![Some(gui_engine::converter::TimecodeMetadata {
            start: audio_core::Timecode {
                hours: 1,
                minutes: 0,
                seconds: 4,
                frames: 12,
            },
            fps: 25.0,
            drop_frame: false,
        })],
        camera_meta_per_file: vec![None; 1],
        device_name: None,
        concat_audio: false,
        resolved_hw_device: None,
    };

    let report = TestReport::new();
    let (_encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        *report.completed.lock().unwrap(),
        "Expected Completed, got failed={}",
        *report.failed.lock().unwrap(),
    );

    let output = dir.path().join("copytest_video_clip01.mp4");
    assert!(
        output.exists(),
        "stream-copy output not created (dir: {:?})",
        std::fs::read_dir(dir.path())
            .map(|e| e
                .filter_map(|e| e.ok().map(|e| e.path()))
                .collect::<Vec<_>>())
            .unwrap_or_default(),
    );

    let codecs = probe_stream_codecs(&output);
    assert!(
        codecs.iter().any(|c| c == "mpeg4"),
        "video stream must keep the source codec (copied), got: {:?}",
        codecs,
    );
    assert!(
        codecs.iter().any(|c| c == "aac"),
        "unfiltered audio must be stream-copied too, got: {:?}",
        codecs,
    );

    let duration = probe_duration_secs(&output);
    assert!(
        (duration - 2.0).abs() < 0.3,
        "expected ~2.0 s after keyframe-snapped trim, got {:.3}",
        duration,
    );
}

#[test]
fn test_progress_stays_below_100_until_all_steps_done() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("--- SKIPPED: ffmpeg not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let mut input_files = Vec::new();
    for i in 0..4 {
        let path = dir.path().join(format!("ch{}.wav", i));
        create_test_wav(&path, 48000, 60.0, 8000);
        input_files.push(path);
    }

    let settings = ConverterSettings {
        pipeline: ConversionPipeline::AudioOnly {
            generate_synthetic_video: false,
        },
        input_files,
        recording_type: RecordingType::MultiTrackAudio,
        ltc_track_channel_index: 0,
        channel_map: ChannelMap::identity(4),
        split_tracks: true,
        drop_ltc_track: false,
        ltc_video_source: None,
        container: "mkv".to_string(),
        copy_video: false,
        video_encoder: "h264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: dir.path().to_path_buf(),
        filename_prefix: "test".to_string(),
        audio_suffix_template: "_audio_track{track:01d}".to_string(),
        video_suffix_template: "_video_clip{clip:02d}".to_string(),
        set_start_from_ltc: false,
        embed_camera_metadata: true,
        trim_offsets_secs: vec![0.0; 4],
        timecode_meta_per_file: vec![None; 4],
        camera_meta_per_file: vec![None; 4],
        device_name: None,
        concat_audio: false,
        resolved_hw_device: None,
    };

    let report = TestReport::new();
    let (_encoder_used, _metadata_only) = run_conversion(&report, settings, Some(&caps));

    assert!(
        *report.completed.lock().unwrap(),
        "Expected Completed, got failed={}",
        *report.failed.lock().unwrap(),
    );

    let hist = report.progress_history();
    assert!(
        hist.len() >= 2,
        "conversion must report progress more than once"
    );
    assert!(
        hist.windows(2).all(|w| w[0] <= w[1]),
        "progress must be non-decreasing: {:?}",
        hist
    );
    assert!(
        hist[..hist.len() - 1].iter().all(|&p| p < 1.0),
        "progress must stay below 100% until the final step completes: {:?}",
        hist
    );
    assert!(
        *hist.last().unwrap() >= 0.99,
        "final progress must reach ~1.0"
    );
}
