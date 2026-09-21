use std::path::Path;
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