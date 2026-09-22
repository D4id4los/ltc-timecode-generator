use std::path::Path;
use std::process::Command;

use crate::converter::RecordingType;

/// Read the duration (seconds) of an audio/video file.
/// WAV files use a fast header-only parse via `hound`; other files use ffprobe.
pub fn file_duration_secs(path: &Path) -> Option<f64> {
    file_duration_secs_with(path, run_ffprobe_duration)
}

/// Like [`file_duration_secs`] but accepts an injectable ffprobe runner
/// for testing (avoiding a hard dependency on the `ffprobe` binary).
pub fn file_duration_secs_with<R>(path: &Path, ffprobe_runner: R) -> Option<f64>
where
    R: FnOnce(&Path) -> Option<f64>,
{
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext.eq_ignore_ascii_case("wav") {
        wav_duration_secs(path)
    } else {
        ffprobe_runner(path)
    }
}

/// Compute group duration from per-file durations.
/// MultiTrackAudio → take length (max); VideoClipSequence → total (sum).
/// Returns `None` when every file's duration is unknown.
pub fn group_duration_secs(recording_type: &RecordingType, durations: &[Option<f64>]) -> Option<f64> {
    match recording_type {
        RecordingType::MultiTrackAudio => durations.iter().filter_map(|&d| d).max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)),
        RecordingType::VideoClipSequence => {
            let sum: f64 = durations.iter().filter_map(|&d| d).sum();
            if sum == 0.0 { None } else { Some(sum) }
        }
    }
}

/// Format seconds as `H:MM:SS` (e.g. "1:02:03", "0:00:45").
pub fn format_duration_secs(secs: f64) -> String {
    let total = secs.round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    format!("{}:{:02}:{:02}", hours, minutes, seconds)
}

// ── WAV header duration (hound) ───────────────────────────────────────────

fn wav_duration_secs(path: &Path) -> Option<f64> {
    let reader = hound::WavReader::open(path).ok()?;
    let spec = reader.spec();
    if spec.sample_rate == 0 {
        return None;
    }
    let samples = reader.duration();
    Some(samples as f64 / spec.sample_rate as f64)
}

// ── ffprobe duration ──────────────────────────────────────────────────────

fn run_ffprobe_duration(path: &Path) -> Option<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "error",
            "-show_entries", "format=duration",
            "-of", "default=noprint_wrappers=1:nokey=1",
            path.as_os_str().to_str()?,
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed.parse::<f64>().ok()
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_test_wav(dir: &TempDir, name: &str, sample_rate: u32, channels: u16, bits_per_sample: u16, num_samples: u64) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for _ in 0..num_samples {
            for _ in 0..channels {
                writer.write_sample(0i16).unwrap();
            }
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn test_wav_duration_48k_mono_16bit() {
        let dir = TempDir::new().unwrap();
        let path = write_test_wav(&dir, "test.wav", 48000, 1, 16, 96000);
        let dur = file_duration_secs(&path).expect("should parse duration");
        assert!((dur - 2.0).abs() < 0.001, "expected 2.0s, got {}", dur);
    }

    #[test]
    fn test_wav_duration_44100_stereo_24bit() {
        let dir = TempDir::new().unwrap();
        let path = write_test_wav(&dir, "test.wav", 44100, 2, 24, 44100);
        let dur = file_duration_secs(&path).expect("should parse duration");
        assert!((dur - 1.0).abs() < 0.001, "expected 1.0s, got {}", dur);
    }

    #[test]
    fn test_wav_duration_8000_mono_8bit() {
        let dir = TempDir::new().unwrap();
        let path = write_test_wav(&dir, "test.wav", 8000, 1, 8, 4000);
        let dur = file_duration_secs(&path).expect("should parse duration");
        assert!((dur - 0.5).abs() < 0.001, "expected 0.5s, got {}", dur);
    }

    #[test]
    fn test_wav_duration_zero_samples() {
        let dir = TempDir::new().unwrap();
        let path = write_test_wav(&dir, "empty.wav", 48000, 1, 16, 0);
        let dur = file_duration_secs(&path).expect("should parse duration for empty wav");
        assert!((dur).abs() < 0.001, "expected 0.0s, got {}", dur);
    }

    #[test]
    fn test_missing_file_returns_none() {
        let result = file_duration_secs(Path::new("/nonexistent/file.wav"));
        assert!(result.is_none());
    }

    #[test]
    fn test_corrupt_wav_returns_none() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("corrupt.wav");
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(b"not a wav file at all").unwrap();
        drop(f);
        let result = file_duration_secs(&path);
        assert!(result.is_none());
    }

    #[test]
    fn test_non_wav_uses_ffprobe_runner() {
        // A non-.wav path — the ffprobe runner should be called.
        let path = Path::new("/some/video.mp4");
        let result = file_duration_secs_with(path, |_| Some(123.456));
        assert!((result.unwrap() - 123.456).abs() < 0.001);
    }

    #[test]
    fn test_non_wav_runner_returns_none() {
        let path = Path::new("/some/video.mp4");
        let result = file_duration_secs_with(path, |_| None);
        assert!(result.is_none());
    }

    #[test]
    fn test_wav_ignores_ffprobe_runner() {
        // .wav should go through hound, not the runner.
        let dir = TempDir::new().unwrap();
        let path = write_test_wav(&dir, "test.wav", 48000, 1, 16, 48000);
        let result = file_duration_secs_with(&path, |_| panic!("should not call ffprobe for WAV"));
        assert!((result.unwrap() - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_group_duration_audio_take_length() {
        // MultiTrackAudio → max
        let durs = [Some(10.0), Some(20.0), Some(15.0)];
        let result = group_duration_secs(&RecordingType::MultiTrackAudio, &durs);
        assert!((result.unwrap() - 20.0).abs() < 0.001);
    }

    #[test]
    fn test_group_duration_audio_all_none() {
        let durs = [None, None];
        let result = group_duration_secs(&RecordingType::MultiTrackAudio, &durs);
        assert!(result.is_none());
    }

    #[test]
    fn test_group_duration_audio_partial_none() {
        let durs = [None, Some(30.0), None];
        let result = group_duration_secs(&RecordingType::MultiTrackAudio, &durs);
        assert!((result.unwrap() - 30.0).abs() < 0.001);
    }

    #[test]
    fn test_group_duration_video_total() {
        let durs = [Some(60.0), Some(120.0), Some(30.0)];
        let result = group_duration_secs(&RecordingType::VideoClipSequence, &durs);
        assert!((result.unwrap() - 210.0).abs() < 0.001);
    }

    #[test]
    fn test_group_duration_video_single_clip() {
        let durs = [Some(45.0)];
        let result = group_duration_secs(&RecordingType::VideoClipSequence, &durs);
        assert!((result.unwrap() - 45.0).abs() < 0.001);
    }

    #[test]
    fn test_group_duration_video_all_none() {
        let durs = [None, None];
        let result = group_duration_secs(&RecordingType::VideoClipSequence, &durs);
        assert!(result.is_none());
    }

    #[test]
    fn test_group_duration_video_zero_from_none() {
        let durs = [None, Some(0.0)];
        let result = group_duration_secs(&RecordingType::VideoClipSequence, &durs);
        assert!(result.is_none(), "sum of zero should be None");
    }

    #[test]
    fn test_format_duration_typical() {
        assert_eq!(format_duration_secs(3723.0), "1:02:03");
    }

    #[test]
    fn test_format_duration_less_than_hour() {
        assert_eq!(format_duration_secs(45.0), "0:00:45");
    }

    #[test]
    fn test_format_duration_exact_hour() {
        assert_eq!(format_duration_secs(3600.0), "1:00:00");
    }

    #[test]
    fn test_format_duration_many_hours() {
        assert_eq!(format_duration_secs(90061.0), "25:01:01");
    }

    #[test]
    fn test_format_duration_rounds() {
        assert_eq!(format_duration_secs(61.7), "0:01:02");
        assert_eq!(format_duration_secs(59.4), "0:00:59");
    }

    #[test]
    fn test_format_duration_zero() {
        assert_eq!(format_duration_secs(0.0), "0:00:00");
    }
}