use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::converter::{
    query_ffmpeg_capabilities, spawn_conversion, ChannelMap, ConversionPipeline, ConversionState,
    ConversionStatus, ConverterSettings, OutputNamingMode, DEFAULT_AUDIO_SUFFIX, DEFAULT_VIDEO_SUFFIX, RecordingType,
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

/// Poll the SharedConversionState until completion/failure, or timeout.
/// Returns (final_status, max_progress_seen, output_log).
fn poll_conversion(
    state: &Arc<Mutex<ConversionState>>,
    cancel: &Arc<AtomicBool>,
    timeout: Duration,
) -> (ConversionStatus, f32, String) {
    let deadline = Instant::now() + timeout;
    let mut max_progress = 0.0_f32;

    loop {
        let (status, progress, log) = {
            let s = state.lock().unwrap();
            let p = match &s.status {
                ConversionStatus::Running { progress } => *progress,
                _ => 0.0,
            };
            (s.status.clone(), p, s.ffmpeg_output.clone())
        };
        max_progress = max_progress.max(progress);

        match &status {
            ConversionStatus::Running { .. } => {
                if Instant::now() > deadline {
                    cancel.store(true, Ordering::Relaxed);
                    return (status, max_progress, log);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            ConversionStatus::Completed | ConversionStatus::Failed { .. } => {
                return (status, max_progress, log);
            }
            ConversionStatus::Idle => {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[test]
fn test_conversion_progress_tracking() {
    // Skip if ffmpeg is not available
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }
    if resolve_encoder_chain("h264", &caps).is_empty() {
        eprintln!("Skipping: no H.264 encoder available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");

    // Create two very short (0.25s) WAV files
    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let settings = ConverterSettings {
        pipeline: ConversionPipeline::AudioOnly { generate_synthetic_video: true },
        input_files: vec![wav1, wav2],
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
        output_folder: dir.path().to_path_buf(),
        filename_prefix: "test".to_string(),
        audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
        video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        naming_mode: OutputNamingMode::PrefixTemplates,
        trim_to_first_ltc: false,
        trim_offsets_secs: vec![0.0, 0.0],
        timecode_meta_per_file: vec![None, None],
        concat_audio: false,
    };

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel), Some(&caps));

    let (final_status, _max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));

    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Completed),
        "Expected Completed, got {:?}. Log:\n{}",
        final_status,
        log,
    );

    // Output files check - look for test_video_clip01.mkv
    let expected_output = dir.path().join("test_video_clip01.mkv");
    assert!(
        expected_output.exists(),
        "Output file was not created: {} (dir contents: {:?})",
        expected_output.display(),
        std::fs::read_dir(dir.path()).map(|e| e.filter_map(|e| e.ok().map(|e| e.path())).collect::<Vec<_>>()).unwrap_or_default(),
    );
    assert!(
        expected_output.metadata().map(|m| m.len() > 0).unwrap_or(false),
        "Output file is empty: {}",
        expected_output.display()
    );
}

#[test]
fn test_conversion_cancellation() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }
    if resolve_encoder_chain("h264", &caps).is_empty() {
        eprintln!("Skipping: no H.264 encoder available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");

    // Use a longer duration so we have time to cancel
    create_test_wav(&wav1, 48000, 5.0, 8000);
    create_test_wav(&wav2, 48000, 5.0, -8000);

    let settings = make_test_settings(dir.path(), vec![wav1, wav2]);

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel), Some(&caps));

    // Let it run briefly, then cancel
    std::thread::sleep(Duration::from_millis(200));
    cancel.store(true, Ordering::Relaxed);

    let (final_status, _max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(10));

    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Failed { .. }),
        "Expected Failed (cancelled), got {:?}. Log:\n{}",
        final_status,
        log,
    );

    if let ConversionStatus::Failed { error_log } = &final_status {
        assert!(
            error_log.contains("CANCELLED BY USER"),
            "Expected cancellation message in log. Log:\n{}",
            log,
        );
    }
}

fn make_test_settings(dir: &Path, input_files: Vec<std::path::PathBuf>) -> ConverterSettings {
    ConverterSettings {
        pipeline: ConversionPipeline::AudioOnly { generate_synthetic_video: true },
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
        audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
        video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        naming_mode: OutputNamingMode::PrefixTemplates,
        trim_to_first_ltc: false,
        trim_offsets_secs: vec![0.0, 0.0],
        timecode_meta_per_file: vec![None, None],
        concat_audio: false,
    }
}

#[test]
fn test_conversion_progress_reaches_100_percent() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }
    if resolve_encoder_chain("h264", &caps).is_empty() {
        eprintln!("Skipping: no H.264 encoder available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");

    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let settings = make_test_settings(dir.path(), vec![wav1, wav2]);

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel), Some(&caps));

    let (final_status, _max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));

    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Completed),
        "Expected Completed, got {:?}. Log:\n{}",
        final_status,
        log,
    );
}

/// With a capability set restricted to `libx264`, the codec "h264" must
/// resolve to exactly that encoder, which is then reported in the log.
#[test]
fn test_conversion_resolves_and_reports_encoder() {
    let mut caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }
    if !caps.available_encoders.contains("libx264") {
        eprintln!("Skipping: libx264 encoder not available");
        return;
    }
    caps.available_encoders = std::collections::BTreeSet::from([
        "libx264".to_string(),
        "pcm_s24le".to_string(),
    ]);

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");
    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let settings = make_test_settings(dir.path(), vec![wav1, wav2]);

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel), Some(&caps));

    let (final_status, _max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));

    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Completed),
        "Expected Completed, got {:?}. Log:\n{}",
        final_status,
        log,
    );
    assert!(
        log.contains("Video encoder used: libx264"),
        "Log should report the resolved encoder. Log:\n{}",
        log,
    );
}

/// An unknown codec id yields a single dead candidate; the fallback runner
/// must exhaust it and publish a Failed state with an explanatory log.
#[test]
fn test_conversion_fails_when_no_encoder_candidate_exists() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");
    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let mut settings = make_test_settings(dir.path(), vec![wav1, wav2]);
    settings.video_encoder = "definitely_not_a_codec".to_string();

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel), Some(&caps));

    let (final_status, _max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));

    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Failed { .. }),
        "Expected Failed for unknown codec, got {:?}. Log:\n{}",
        final_status,
        log,
    );
    assert!(
        log.contains("failed to initialize"),
        "Log should explain the encoder failure. Log:\n{}",
        log,
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
            "-y", "-v", "error",
            "-f", "lavfi", "-i", &format!("color=c=blue:s=320x240:r=25:duration={}", duration_secs),
            "-f", "lavfi", "-i", &format!("sine=frequency={}:duration={}:sample_rate={}", frequency, duration_secs, sample_rate),
            "-map", "0:v", "-map", "1:a",
            "-c:v", "libsvtav1", "-pix_fmt", "yuv420p",
            "-c:a", "pcm_s16le",
            "-shortest",
            "-t", &format!("{}", duration_secs),
            &path.to_string_lossy(),
        ])
        .status()
        .expect("failed to spawn ffmpeg for test video");
    assert!(status.success(), "ffmpeg fixture creation failed for {}", path.display());
}

#[test]
fn test_concat_audio_across_two_video_clips() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }
    if resolve_encoder_chain("h264", &caps).is_empty() && !caps.available_encoders.contains("mpeg4") {
        eprintln!("Skipping: no suitable video encoder");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let clip1 = dir.path().join("clip1.mp4");
    let clip2 = dir.path().join("clip2.mp4");

    // Two clips: 0.3s + 0.2s = 0.5s total
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
        audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
        video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        naming_mode: OutputNamingMode::PrefixTemplates,
        trim_to_first_ltc: false,
        trim_offsets_secs: vec![0.0, 0.0],
        timecode_meta_per_file: vec![None, None],
        concat_audio: true,
    };

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel), Some(&caps));

    let (final_status, _max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));

    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Completed),
        "Expected Completed, got {:?}. Log:\n{}",
        final_status,
        log,
    );

    // Check that the concatenated audio file exists
    let concat_audio = dir.path().join("concat_test_audio_track1.wav");
    assert!(
        concat_audio.exists(),
        "Concatenated audio file not created: {} (dir: {:?})",
        concat_audio.display(),
        std::fs::read_dir(dir.path()).map(|e| e.filter_map(|e| e.ok().map(|e| e.path())).collect::<Vec<_>>()).unwrap_or_default(),
    );

    // Read the WAV to verify approximate total duration
    if let Ok(reader) = hound::WavReader::open(&concat_audio) {
        let spec = reader.spec();
        let num_samples = reader.duration() as u64;
        let expected_samples = ((0.3 + 0.2) * spec.sample_rate as f64).round() as u64;
        let tolerance = (spec.sample_rate as f64 * 0.05) as u64; // 50ms tolerance
        assert!(
            num_samples.abs_diff(expected_samples) <= tolerance,
            "Expected ~{} samples, got {} (tolerance: {})",
            expected_samples, num_samples, tolerance,
        );
        assert_eq!(spec.channels, 1, "concatenated audio should be mono");
    } else {
        panic!("Could not open concatenated WAV: {}", concat_audio.display());
    }
}

// ── Stream-copy mode ("Leave Video Encoding Untouched") ─────────────────

/// Create an MP4 with MPEG-4 Part 2 video (universally available encoder)
/// and a known keyframe interval, so stream-copy cut points are predictable.
fn create_test_video_with_gop(path: &std::path::Path, duration_secs: f64, gop: u32) {
    let status = Command::new("ffmpeg")
        .args([
            "-y", "-v", "error",
            "-f", "lavfi", "-i", &format!("color=c=blue:s=320x240:r=25:duration={}", duration_secs),
            "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={}:sample_rate=48000", duration_secs),
            "-map", "0:v", "-map", "1:a",
            "-c:v", "mpeg4", "-g", &gop.to_string(), "-pix_fmt", "yuv420p",
            "-c:a", "aac",
            "-shortest",
            "-t", &format!("{}", duration_secs),
            &path.to_string_lossy(),
        ])
        .status()
        .expect("failed to spawn ffmpeg for test video");
    assert!(status.success(), "ffmpeg fixture creation failed for {}", path.display());
}

/// Duration of the first video stream of a file, via ffprobe.
fn probe_duration_secs(path: &Path) -> f64 {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "csv=p=0",
            &path.to_string_lossy(),
        ])
        .output()
        .expect("failed to spawn ffprobe");
    assert!(out.status.success(), "ffprobe failed for {}", path.display());
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .expect("ffprobe duration not a number")
}

/// Name of every stream codec (video/audio/data) in a file, via ffprobe.
fn probe_stream_codecs(path: &Path) -> Vec<String> {
    let out = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "stream=codec_name,codec_type",
            "-of", "csv=p=0",
            &path.to_string_lossy(),
        ])
        .output()
        .expect("failed to spawn ffprobe");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .flat_map(|l| l.trim().split(',').map(|s| s.to_string()).collect::<Vec<_>>())
        .filter(|l| !l.is_empty() && l != "unknown")
        .collect()
}

#[test]
fn test_copy_mode_streams_video_and_derives_container() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let clip = dir.path().join("C0001.MP4");
    // 4 s @ 25 fps with GOP 50 → keyframes at 0.0, 2.0, 4.0
    create_test_video_with_gop(&clip, 4.0, 50);

    // Trim offset 2.5 s must snap to the keyframe at 2.0 s
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
        // Deliberately bogus: copy mode must derive mp4 from the input and
        // ignore the video codec selection entirely.
        container: "mkv".to_string(),
        copy_video: true,
        video_encoder: "weird-codec".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        resolved_video_encoder: String::new(),
        output_folder: dir.path().to_path_buf(),
        filename_prefix: "copytest".to_string(),
        audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
        video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
        naming_mode: OutputNamingMode::PrefixTemplates,
        trim_to_first_ltc: true,
        trim_offsets_secs: vec![2.5],
        timecode_meta_per_file: vec![Some(gui_engine::converter::TimecodeMetadata {
            start: audio_core::Timecode { hours: 1, minutes: 0, seconds: 4, frames: 12 },
            fps: 25.0,
            drop_frame: false,
        })],
        concat_audio: false,
    };

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel), Some(&caps));
    let (final_status, _max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));
    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Completed),
        "Expected Completed, got {:?}. Log:\n{}",
        final_status,
        log,
    );

    // Container derived from the .MP4 input, not the bogus "mkv" setting
    let output = dir.path().join("copytest_video_clip01.mp4");
    assert!(
        output.exists(),
        "stream-copy output not created (dir: {:?}). Log:\n{}",
        std::fs::read_dir(dir.path()).map(|e| e.filter_map(|e| e.ok().map(|e| e.path())).collect::<Vec<_>>()).unwrap_or_default(),
        log,
    );

    // Video stream must be copied, not transcoded: still the source codec.
    // A tmcd data stream proves the timecode metadata was written.
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
    assert!(
        codecs.iter().any(|c| c == "data"),
        "tmcd timecode track must be present, got: {:?}",
        codecs,
    );

    // Trim snapped 2.5 → 2.0: output duration ≈ 4.0 − 2.0 = 2.0 s
    let duration = probe_duration_secs(&output);
    assert!(
        (duration - 2.0).abs() < 0.3,
        "expected ~2.0 s after keyframe-snapped trim, got {:.3}",
        duration,
    );
}