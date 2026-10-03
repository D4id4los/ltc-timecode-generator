use std::path::Path;
use std::process::{Child, Stdio};
use std::time::Duration;

use log::{info, warn};

use crate::subprocess::{no_window_command, run_ffmpeg_collect_stderr, FFMPEG_STALL_TIMEOUT};

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

impl std::fmt::Display for StepFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepFailure::EncoderInit(d) => write!(f, "encoder init failed: {}", d),
            StepFailure::Fatal(d) => write!(f, "{}", d),
        }
    }
}

/// Minimum output file size (in bytes) that suggests ffmpeg actually produced
/// real encoded/copied content (not just a muxer header).
pub const MIN_PRODUCED_OUTPUT_BYTES: u64 = 4096;

/// Highest step fraction reported while the ffmpeg process is still running.
/// ffmpeg emits `progress=end` (and `out_time` can reach the container
/// duration) while it is still finalising the output file, so an in-flight
/// report must never claim the full step weight — only [`crate::converter::runner::ConversionReport::advance_step`],
/// called after the process exits successfully, may complete a step.
const MAX_IN_FLIGHT_STEP_FRACTION: f32 = 0.99;

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

/// Run ffmpeg for a conversion step, reporting progress through the
/// provided `ConversionReport`.
///
/// `args` is the ffmpeg argument list **without** the output path.
/// `output` is the output path.
pub fn run_ffmpeg_process<R: crate::converter::runner::ConversionReport>(
    args: &[String],
    output: &Path,
    report: &R,
    total_steps: usize,
    current_step: usize,
) -> Result<(), StepFailure> {
    let step_label = format!("[{}/{}]", current_step, total_steps);
    info!("{} Spawning ffmpeg with {} args → {}", step_label, args.len(), output.display());

    let full_args: Vec<String> = args.iter().cloned().chain(std::iter::once(output.to_string_lossy().to_string())).collect();

    run_ffmpeg_process_with(
        &full_args, output, report, total_steps, current_step,
        &mut |full_args: &[String]| {
            no_window_command("ffmpeg")
                .args(full_args)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
        },
        FFMPEG_STALL_TIMEOUT,
    )
}

/// Injectable-spawner variant of [`run_ffmpeg_process`] for testability.
///
/// `full_args` must include the output path as the last argument (already
/// appended by the caller). `spawner` returns the spawned child (with stderr
/// piped). `stall` is the no-output timeout; the public variant uses
/// [`FFMPEG_STALL_TIMEOUT`] (30 s).
pub fn run_ffmpeg_process_with<R: crate::converter::runner::ConversionReport>(
    full_args: &[String],
    output: &Path,
    report: &R,
    total_steps: usize,
    current_step: usize,
    spawner: &mut dyn FnMut(&[String]) -> std::io::Result<Child>,
    stall: Duration,
) -> Result<(), StepFailure> {
    let step_label = format!("[{}/{}]", current_step, total_steps);

    let args_str = format!("{} ffmpeg \\\n  {}", step_label, full_args.join(" \\\n  "));
    report.set_log(&args_str);

    if report.is_cancelled() {
        return Err(StepFailure::Fatal("cancelled by user".to_string()));
    }

    let mut local_log = String::new();
    let mut step_progress: f32 = 0.0;
    let mut produced_output = false;
    let duration_re = regex::Regex::new(r"Duration: (\d+):(\d+):(\d+)\.(\d+)").unwrap();
    let mut total_duration_secs: Option<f64> = None;

    let watchdog_result = run_ffmpeg_collect_stderr(
        spawner,
        full_args,
        stall,
        report.cancel_atomic(),
        &mut |line: &str| {
            local_log.push_str(line);
            local_log.push('\n');

            if total_duration_secs.is_none() {
                if let Some(caps) = duration_re.captures(line) {
                    let h: f64 = caps[1].parse().unwrap_or(0.0);
                    let m: f64 = caps[2].parse().unwrap_or(0.0);
                    let s: f64 = caps[3].parse().unwrap_or(0.0);
                    let frac: f64 = caps[4].parse().unwrap_or(0.0) / 100.0;
                    if h > 0.0 || m > 0.0 || s > 0.0 || frac > 0.0 {
                        total_duration_secs = Some(h * 3600.0 + m * 60.0 + s + frac);
                    }
                }
            }

            if let Some(current_secs) = parse_out_time(line) {
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

            report.report_step_fraction(step_progress.min(MAX_IN_FLIGHT_STEP_FRACTION), line);
        },
    );

    report.append_log(&local_log);

    match watchdog_result {
        Ok(_) => {
            report.advance_step();
            info!("{} Step completed: {}", step_label, output.display());
            Ok(())
        }
        Err(crate::subprocess::FfmpegRunError::Spawn(e)) => {
            let err_msg = format!("{} Failed to spawn ffmpeg: {}", step_label, e);
            warn!("{}", err_msg);
            report.append_log(&format!("\n\n--- {} ---", err_msg));
            Err(StepFailure::Fatal(err_msg))
        }
        Err(crate::subprocess::FfmpegRunError::Exit { code, .. }) => {
            warn!("{} ffmpeg exited with code {}: {}", step_label, code, output.display());
            report.append_log(&format!("\n\n--- FFMPEG EXITED WITH CODE {} ---", code));
            let classification = classify_step_failure(produced_output, output, &code);
            if matches!(classification, StepFailure::EncoderInit(_)) {
                let _ = std::fs::remove_file(output);
            }
            Err(classification)
        }
        Err(crate::subprocess::FfmpegRunError::Cancelled) => {
            report.append_log(&format!("{} --- CANCELLED ---\n", step_label));
            report.append_log("\n\n--- CANCELLED BY USER ---");
            Err(StepFailure::Fatal("cancelled by user".to_string()))
        }
        Err(crate::subprocess::FfmpegRunError::Stalled) => {
            let msg = format!(
                "ffmpeg stalled — no stderr output for {}s",
                stall.as_secs()
            );
            warn!("{} {}", step_label, msg);
            report.append_log(&format!("\n\n--- {} ---", msg));
            Err(StepFailure::Fatal(msg))
        }
        Err(crate::subprocess::FfmpegRunError::Wait(e)) => {
            let msg = format!("ffmpeg process wait error: {}", e);
            warn!("{} {}", step_label, msg);
            report.append_log(&format!("\n\n--- {} ---", msg));
            Err(StepFailure::Fatal(msg))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use super::*;
    use crate::converter::runner::{ConversionReport, TestReport};

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

    // ── run_ffmpeg_process_with tests ────────────────────────────────────

    /// Spawner that runs `sleep 30` (silent child). Ignores its args.
    fn silent_spawner(_full_args: &[String]) -> std::io::Result<Child> {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "ping", "-n", "30", "127.0.0.1", ">nul"]);
            c
        } else {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            c
        };
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::piped());
        cmd.spawn()
    }

    #[test]
    fn test_run_ffmpeg_cancel_while_silent() {
        let report = TestReport::new();
        report.cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
        let out = Path::new("/tmp/_test_ffmpeg_cancel.mp4");
        let start = std::time::Instant::now();

        let result = run_ffmpeg_process_with(
            &[], out, &report, 2, 1,
            &mut silent_spawner,
            Duration::from_secs(10),
        );
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(StepFailure::Fatal(ref m)) if m == "cancelled by user"),
            "expected cancel, got {:?}", result);
        assert!(elapsed < Duration::from_secs(5), "cancel took {:?}", elapsed);
    }

    #[test]
    fn test_run_ffmpeg_stall_timeout() {
        let report = TestReport::new();
        let out = Path::new("/tmp/_test_ffmpeg_stall.mp4");
        let start = std::time::Instant::now();

        let result = run_ffmpeg_process_with(
            &[], out, &report, 2, 1,
            &mut silent_spawner,
            Duration::from_millis(200),
        );
        let elapsed = start.elapsed();
        assert!(result.is_err(), "expected stall error, got {:?}", result);
        let log = report.log.lock().unwrap().clone();
        assert!(log.contains("stalled") || log.contains("stall"), "log: {}", log);
        assert!(elapsed < Duration::from_secs(5), "stall took {:?}", elapsed);
    }

    #[test]
    fn test_run_ffmpeg_parses_lines_and_reports_progress() {
        let report = TestReport::new();
        report.set_step_weight(0.5);
        let out = Path::new("/tmp/_test_ffmpeg_lines.mp4");

        let mut spawner = |_args: &[String]| {
            let mut cmd = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C",
                    "echo Duration: 00:00:10.00>&2 & echo out_time=00:00:05.000000>&2 & echo progress=end>&2"]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.args(["-c",
                    "echo 'Duration: 00:00:10.00' >&2; echo 'out_time=00:00:05.000000' >&2; echo 'progress=end' >&2"]);
                c
            };
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::piped());
            cmd.spawn()
        };

        let result = run_ffmpeg_process_with(
            &["-i".to_string(), "dummy".to_string()], out, &report, 2, 1,
            &mut spawner,
            Duration::from_secs(5),
        );
        // Child exits 0 → success
        assert!(result.is_ok(), "expected ok, got {:?}", result);
        // Progress should have advanced (step weight was 0.5)
        let progress = *report.progress.lock().unwrap();
        assert!(progress > 0.0, "progress should have advanced: {}", progress);
        assert!(progress <= 1.0, "progress should be <= 1.0: {}", progress);
    }

    /// An in-flight report must never claim the step is complete: ffmpeg
    /// emits `progress=end` (and `out_time` can reach the container
    /// duration) while still finalising the output, so the raw 1.0 must be
    /// capped below full. Only `advance_step` (successful process exit)
    /// completes a step.
    #[test]
    fn test_run_ffmpeg_in_flight_progress_never_completes_step() {
        let report = TestReport::new();
        report.set_step_weight(1.0);
        let out = Path::new("/tmp/_test_ffmpeg_inflight_cap.mp4");

        let mut spawner = |_args: &[String]| {
            let mut cmd = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C",
                    "echo Duration: 00:00:10.00>&2 & echo out_time=00:00:12.000000>&2 & echo progress=end>&2"]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.args(["-c",
                    "echo 'Duration: 00:00:10.00' >&2; echo 'out_time=00:00:12.000000' >&2; echo 'progress=end' >&2"]);
                c
            };
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::piped());
            cmd.spawn()
        };

        let result = run_ffmpeg_process_with(
            &["-i".to_string(), "dummy".to_string()], out, &report, 1, 1,
            &mut spawner,
            Duration::from_secs(5),
        );
        assert!(result.is_ok(), "expected ok, got {:?}", result);

        let hist = report.progress_history();
        assert!(hist.len() >= 2, "must have in-flight reports plus advance_step: {:?}", hist);
        assert!(
            hist[..hist.len() - 1].iter().all(|&p| p < 1.0),
            "in-flight progress must stay below the full step weight: {:?}",
            hist
        );
        let last = *hist.last().unwrap();
        assert!(last >= 0.99, "advance_step must complete the step: {}", last);
    }

    #[test]
    fn test_run_ffmpeg_exit_nonzero_no_output_encoder_init() {
        let report = TestReport::new();
        report.set_step_weight(0.5);
        // Tempdir-backed output path: a stale file >= 4096 B at a fixed
        // path would flip `classify_step_failure` from EncoderInit to Fatal.
        let dir = tempfile::TempDir::new().unwrap();
        let out = dir.path().join("_test_ffmpeg_fail.mp4");

        let mut spawner = |_args: &[String]| {
            let mut cmd = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C", "exit /b 1"]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.args(["-c", "exit 1"]);
                c
            };
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::piped());
            cmd.spawn()
        };

        let result = run_ffmpeg_process_with(
            &["-i".to_string(), "nonexistent".to_string()], &out, &report, 1, 1,
            &mut spawner,
            Duration::from_secs(5),
        );
        match result {
            Err(StepFailure::EncoderInit(_)) => {} // expected
            other => panic!("expected EncoderInit, got {:?}", other),
        }
    }
}