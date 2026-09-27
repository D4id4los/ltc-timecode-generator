use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;

use log::{info, warn};

use crate::converter::progress::{CancelFlag, ConversionStatus, SharedConversionState};
use crate::subprocess::no_window_command;
use std::io::BufRead;

#[derive(Clone, Debug, PartialEq)]
pub enum StepFailure {
    /// ffmpeg exited before producing any output — typically an encoder
    /// initialization failure (the encoder is listed by `ffmpeg -encoders`
    /// but the hardware/driver is missing). Safe to retry with the next
    /// candidate in the chain.
    EncoderInit(String),
    /// Failure after output was produced, a spawn error, or user
    /// cancellation. Not retryable with a different encoder.
    Fatal(String),
}

/// Minimum output file size (in bytes) that suggests ffmpeg actually produced
/// real encoded/copied content (not just a muxer header).
pub const MIN_PRODUCED_OUTPUT_BYTES: u64 = 4096;

/// Parse an `out_time=` progress line from `-progress pipe:2` output.
/// Returns `Some(duration_seconds)` when the line contains a valid
/// `out_time=HH:MM:SS.ssssss` value (including 0.0), and `None` for
/// `N/A`, suffix keys (`out_time_us`, `out_time_ms`), or unrelated lines.
pub fn parse_out_time(line: &str) -> Option<f64> {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| regex::Regex::new(r"out_time=(\d+):(\d+):(\d+)\.(\d+)").unwrap());
    let caps = re.captures(line)?;
    let raw = caps.get(0).map(|m| m.as_str()).unwrap_or("");
    let tail = &raw["out_time".len()..];
    if !tail.starts_with('=') {
        return None;
    }
    let h: f64 = caps[1].parse().unwrap_or(0.0);
    let m: f64 = caps[2].parse().unwrap_or(0.0);
    let s: f64 = caps[3].parse().unwrap_or(0.0);
    let frac: f64 = caps[4].parse().unwrap_or(0.0) / 1_000_000.0;
    Some(h * 3600.0 + m * 60.0 + s + frac)
}

/// Classify an ffmpeg step failure as retryable (`EncoderInit`) or
/// terminal (`Fatal`).  Considers both `produced_output` (from log
/// parsing) and a file-size sanity check so encoder-init failures that
/// leave a header-only (or zero-length) file are correctly retried.
pub fn classify_step_failure(produced_output: bool, output: &Path, code: &str) -> StepFailure {
    let file_output = std::fs::metadata(output)
        .map(|m| m.len())
        .unwrap_or(0)
        >= MIN_PRODUCED_OUTPUT_BYTES;
    if file_output && !produced_output {
        info!(
            "classify_step_failure: produced_output=false but output file is {} bytes — treating as Fatal",
            std::fs::metadata(output).map(|m| m.len()).unwrap_or(0)
        );
    }
    if produced_output || file_output {
        StepFailure::Fatal(format!("ffmpeg exited with code {}", code))
    } else {
        StepFailure::EncoderInit(format!(
            "ffmpeg exited with code {} before producing output",
            code
        ))
    }
}

pub fn mark_conversion_failed(state: &SharedConversionState, overall_log: &str) {
    let mut s = state.lock().unwrap();
    s.status = ConversionStatus::Failed {
        error_log: overall_log.to_string(),
    };
    s.ffmpeg_output = overall_log.to_string();
}

pub fn run_ffmpeg_process(
    args: &[String],
    output: &Path,
    state: &SharedConversionState,
    cancel: &CancelFlag,
    step_progress_weight: f32,
    overall_progress: &mut f32,
    overall_log: &mut String,
    total_steps: usize,
    current_step: usize,
) -> Result<(), StepFailure> {
    let step_label = format!("[{}/{}]", current_step, total_steps);
    info!("{} Spawning ffmpeg with {} args → {}", step_label, args.len(), output.display());

    let full_args: Vec<String> = args.iter().cloned().chain(std::iter::once(output.to_string_lossy().to_string())).collect();
    let args_str = format!("{} ffmpeg \\\n  {}", step_label, full_args.join(" \\\n  "));
    {
        let mut s = state.lock().unwrap();
        s.ffmpeg_output = args_str.clone();
        s.current_line = args_str;
    }

    let mut child = match no_window_command("ffmpeg")
        .args(&full_args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            let err_msg = format!("{} Failed to spawn ffmpeg: {}", step_label, e);
            warn!("{}", err_msg);
            overall_log.push_str(&format!("\n\n--- {} ---", err_msg));
            return Err(StepFailure::Fatal(err_msg));
        }
    };

    let stderr = child.stderr.take().unwrap();
    let reader = std::io::BufReader::new(stderr);
    let mut local_log = String::new();
    let mut step_progress: f32 = 0.0;
    let mut produced_output = false;
    let duration_re = regex::Regex::new(r"Duration: (\d+):(\d+):(\d+)\.(\d+)").unwrap();
    let mut total_duration_secs: Option<f64> = None;

    for line in reader.lines() {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            overall_log.push_str(&format!("{} --- CANCELLED ---\n", step_label));
            overall_log.push_str("\n\n--- CANCELLED BY USER ---");
            return Err(StepFailure::Fatal("cancelled by user".to_string()));
        }

        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        local_log.push_str(&line);
        local_log.push('\n');

        if total_duration_secs.is_none() {
            if let Some(caps) = duration_re.captures(&line) {
                let h: f64 = caps[1].parse().unwrap_or(0.0);
                let m: f64 = caps[2].parse().unwrap_or(0.0);
                let s: f64 = caps[3].parse().unwrap_or(0.0);
                let frac: f64 = caps[4].parse().unwrap_or(0.0) / 100.0;
                if h > 0.0 || m > 0.0 || s > 0.0 || frac > 0.0 {
                    total_duration_secs = Some(h * 3600.0 + m * 60.0 + s + frac);
                }
            }
        }

        if let Some(current_secs) = parse_out_time(&line) {
            if current_secs > 0.0 {
                produced_output = true;
            }

            if let Some(total) = total_duration_secs {
                if total > 0.0 {
                    step_progress = (current_secs / total).min(1.0) as f32;
                }
            } else if current_secs > 0.0 {
                let heuristic = current_secs * 100.0;
                step_progress = (current_secs / heuristic).min(1.0) as f32;
            }
        }

        if line.trim() == "progress=end" {
            step_progress = 1.0;
        }

        let combined = *overall_progress + step_progress * step_progress_weight;
        {
            let mut s = state.lock().unwrap();
            s.status = ConversionStatus::Running { progress: combined.min(1.0) };
            s.current_line = line.clone();
        }
    }

    let exit_status = child.wait();
    overall_log.push_str(&local_log);

    match exit_status {
        Ok(status) if status.success() => {
            *overall_progress += step_progress_weight;
            info!("{} Step completed: {}", step_label, output.display());
            Ok(())
        }
        Ok(status) => {
            let code = status.code().map(|c| c.to_string()).unwrap_or("unknown".into());
            warn!("{} ffmpeg exited with code {}: {}", step_label, code, output.display());
            overall_log.push_str(&format!("\n\n--- FFMPEG EXITED WITH CODE {} ---", code));
            let classification = classify_step_failure(produced_output, output, &code);
            if matches!(classification, StepFailure::EncoderInit(_)) {
                let _ = std::fs::remove_file(output);
            }
            Err(classification)
        }
        Err(e) => {
            warn!("{} ffmpeg error: {}", step_label, e);
            overall_log.push_str(&format!("\n\n--- FFMPEG ERROR: {} ---", e));
            Err(StepFailure::Fatal(e.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use super::*;

    #[test]
    fn test_parse_out_time_happy_path() {
        let line = "out_time=01:23:45.678901";
        let t = parse_out_time(line);
        assert!(t.is_some());
        let expected = 1.0 * 3600.0 + 23.0 * 60.0 + 45.0 + 0.678901;
        assert!((t.unwrap() - expected).abs() < 0.000_001);
    }

    #[test]
    fn test_parse_out_time_na() {
        assert!(parse_out_time("out_time=N/A").is_none());
    }

    #[test]
    fn test_parse_out_time_key_suffixes() {
        assert!(parse_out_time("out_time_us=12345").is_none(), "out_time_us must not match");
        assert!(parse_out_time("out_time_ms=1234").is_none(), "out_time_ms must not match");
    }

    #[test]
    fn test_parse_out_time_progress_lines() {
        let line = "out_time=00:00:10.000000";
        let t = parse_out_time(line);
        assert!((t.unwrap() - 10.0).abs() < 0.001);
    }

    #[test]
    fn test_parse_out_time_stderr_time_not_mistaken() {
        let line = "  Stream #0:0(und): Audio: pcm_s24le, 48000 Hz, ...";
        assert!(parse_out_time(line).is_none());
    }

    #[test]
    fn test_parse_out_time_empty() {
        assert!(parse_out_time("").is_none());
    }

    #[test]
    fn test_classify_step_failure_no_output_file_missing() {
        let p = Path::new("/nonexistent/file.wav");
        let sf = classify_step_failure(false, p, "1");
        assert!(matches!(sf, StepFailure::EncoderInit(_)));
    }

    #[test]
    fn test_classify_step_failure_no_output_zero_byte() {
        let dir = std::env::temp_dir();
        let p = dir.join("test_classify_zero.wav");
        let _ = std::fs::write(&p, []);
        let sf = classify_step_failure(false, &p, "1");
        assert!(matches!(sf, StepFailure::EncoderInit(_)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_classify_step_failure_no_output_header_only() {
        let dir = std::env::temp_dir();
        let p = dir.join("test_classify_header.wav");
        let _ = std::fs::write(&p, b"RIFF");
        let sf = classify_step_failure(false, &p, "1");
        assert!(matches!(sf, StepFailure::EncoderInit(_)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_classify_step_failure_real_output_file() {
        let dir = std::env::temp_dir();
        let p = dir.join("test_classify_real.wav");
        let buf = vec![0u8; 5000];
        let _ = std::fs::write(&p, &buf);
        let sf = classify_step_failure(false, &p, "1");
        assert!(matches!(sf, StepFailure::Fatal(_)));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn test_classify_step_failure_produced_output_trumps_no_file() {
        let p = Path::new("/nonexistent/output.wav");
        let sf = classify_step_failure(true, p, "1");
        assert!(matches!(sf, StepFailure::Fatal(_)));
    }
}