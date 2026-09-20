use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::converter::{
    query_ffmpeg_capabilities, spawn_conversion, ChannelMap, ConversionState, ConversionStatus,
    ConverterSettings,
};

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
    if !caps.available_encoders.contains("libx264") {
        eprintln!("Skipping: libx264 encoder not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");
    let output = dir.path().join("output.mkv");

    // Create two very short (0.25s) WAV files
    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let settings = ConverterSettings {
        input_files: vec![wav1, wav2],
        channel_map: ChannelMap::identity(2),
        container: "mkv".to_string(),
        video_encoder: "libx264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        output_path: output.clone(),
        trim_start_secs: 0.0,
    };

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel));

    let (final_status, max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));

    handle.join().expect("conversion thread panicked");

    assert!(
        max_progress > 0.0,
        "Expected progress > 0.0 during conversion, got {}. Log:\n{}",
        max_progress,
        log,
    );

    assert!(
        matches!(final_status, ConversionStatus::Completed),
        "Expected Completed, got {:?}. Log:\n{}",
        final_status,
        log,
    );

    assert!(
        output.exists(),
        "Output file was not created: {}",
        output.display()
    );
    assert!(
        output.metadata().map(|m| m.len() > 0).unwrap_or(false),
        "Output file is empty: {}",
        output.display()
    );
}

#[test]
fn test_conversion_cancellation() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }
    if !caps.available_encoders.contains("libx264") {
        eprintln!("Skipping: libx264 encoder not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");
    let output = dir.path().join("output.mkv");

    // Use a longer duration so we have time to cancel
    create_test_wav(&wav1, 48000, 5.0, 8000);
    create_test_wav(&wav2, 48000, 5.0, -8000);

    let settings = ConverterSettings {
        input_files: vec![wav1, wav2],
        channel_map: ChannelMap::identity(2),
        container: "mkv".to_string(),
        video_encoder: "libx264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        output_path: output.clone(),
        trim_start_secs: 0.0,
    };

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel));

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

#[test]
fn test_conversion_progress_reaches_100_percent() {
    let caps = query_ffmpeg_capabilities();
    if !caps.has_ffmpeg {
        eprintln!("Skipping: ffmpeg not available");
        return;
    }
    if !caps.available_encoders.contains("libx264") {
        eprintln!("Skipping: libx264 encoder not available");
        return;
    }

    let dir = tempfile::TempDir::new().unwrap();
    let wav1 = dir.path().join("ch1.wav");
    let wav2 = dir.path().join("ch2.wav");
    let output = dir.path().join("output.mkv");

    create_test_wav(&wav1, 48000, 0.25, 8000);
    create_test_wav(&wav2, 48000, 0.25, -8000);

    let settings = ConverterSettings {
        input_files: vec![wav1, wav2],
        channel_map: ChannelMap::identity(2),
        container: "mkv".to_string(),
        video_encoder: "libx264".to_string(),
        audio_encoder: "pcm_s24le".to_string(),
        output_path: output.clone(),
        trim_start_secs: 0.0,
    };

    let state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    let handle = spawn_conversion(settings, Arc::clone(&state), Arc::clone(&cancel));

    let (final_status, max_progress, log) =
        poll_conversion(&state, &cancel, Duration::from_secs(30));

    handle.join().expect("conversion thread panicked");

    assert!(
        matches!(final_status, ConversionStatus::Completed),
        "Expected Completed, got {:?}. Log:\n{}",
        final_status,
        log,
    );

    assert!(
        (max_progress - 1.0).abs() < 0.01,
        "Expected progress to reach ~1.0 on completion, got {}. Log:\n{}",
        max_progress,
        log,
    );
}