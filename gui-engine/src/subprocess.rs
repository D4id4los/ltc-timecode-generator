use std::io;
use std::io::BufRead;
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Default timeout for ffprobe/exiftool probe subprocesses.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Stall (no-stderr-output) timeout for streaming ffmpeg subprocesses.
/// If no stderr line arrives within this window the child is killed.
pub const FFMPEG_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Outcome of [`watch_stderr_lines`] when the child had to be stopped.
#[derive(Debug, Clone, PartialEq)]
pub enum WatchdogStop {
    /// Cancel flag was observed — child killed and reaped.
    Cancelled,
    /// No stderr output for the stall timeout — child killed and reaped.
    Stalled,
    /// wait()/try_wait() failed.
    Wait(String),
}

/// Error type for subprocess operations.
#[derive(Debug, Clone, PartialEq)]
pub enum SubprocessFailure {
    /// The process could not be spawned (bad binary, no such file, etc).
    Io(String),
    /// The process did not exit within the allotted timeout.
    TimedOut,
    /// The process ran but exited with a non-zero status. Carries the
    /// trimmed stderr tail for diagnostics.
    NonZeroExit { stderr_tail: String },
    /// The process's output could not be parsed (e.g. invalid JSON).
    Parse(String),
}

/// Terminal outcome of a watchdog-driven ffmpeg run
/// ([`run_ffmpeg_collect_stderr`]).
#[derive(Debug)]
pub enum FfmpegRunError {
    /// The child could not be spawned, or its stderr pipe was unavailable.
    Spawn(io::Error),
    /// Cancel flag observed — child killed and reaped.
    Cancelled,
    /// No stderr output for the stall timeout — child killed and reaped.
    Stalled,
    /// wait()/try_wait() failed.
    Wait(String),
    /// Non-zero exit; carries the exit code string and the collected
    /// stderr tail (last ~400 chars, trimmed).
    Exit { code: String, stderr_tail: String },
}

/// Return the last `max_chars` characters of a string (trimmed), for
/// including the tail of a subprocess's stderr in error messages.
pub fn stderr_tail(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(len - max_chars).collect();
    format!("…{}", tail)
}

/// Spawn via `spawner`, drain stderr line-by-line through `on_line`,
/// enforce `stall` + `cancel`, and classify the terminal state.
/// Returns the full collected stderr on success.
pub fn run_ffmpeg_collect_stderr(
    spawner: &mut dyn FnMut(&[String]) -> std::io::Result<Child>,
    args: &[String],
    stall: Duration,
    cancel: Option<&AtomicBool>,
    on_line: &mut dyn FnMut(&str),
) -> Result<String, FfmpegRunError> {
    let mut child = spawner(args).map_err(FfmpegRunError::Spawn)?;
    let stderr = child.stderr.take().ok_or_else(|| {
        FfmpegRunError::Spawn(io::Error::other("failed to capture ffmpeg stderr"))
    })?;

    let mut collected = String::new();
    let result = watch_stderr_lines(&mut child, stderr, stall, cancel, &mut |line| {
        collected.push_str(line);
        collected.push('\n');
        on_line(line);
    });

    match result {
        Ok(status) if status.success() => Ok(collected),
        Ok(status) => {
            let code = status.code().map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
            Err(FfmpegRunError::Exit {
                code,
                stderr_tail: stderr_tail(&collected, 400),
            })
        }
        Err(WatchdogStop::Cancelled) => Err(FfmpegRunError::Cancelled),
        Err(WatchdogStop::Stalled) => Err(FfmpegRunError::Stalled),
        Err(WatchdogStop::Wait(e)) => Err(FfmpegRunError::Wait(e)),
    }
}


impl std::fmt::Display for SubprocessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubprocessFailure::Io(msg) => write!(f, "subprocess I/O error: {}", msg),
            SubprocessFailure::TimedOut => write!(f, "subprocess timed out"),
            SubprocessFailure::NonZeroExit { stderr_tail } => {
                write!(f, "subprocess failed: {}", stderr_tail)
            }
            SubprocessFailure::Parse(msg) => {
                write!(f, "failed to parse subprocess output: {}", msg)
            }
        }
    }
}

/// Create a `Command` for a helper binary (ffmpeg/ffprobe) that will **not**
/// flash a console window on Windows. No-op on other platforms — returns a
/// plain `Command`.
///
/// Use this everywhere in `gui-engine` that spawns an external process so
/// that Windows GUI builds (which have no console of their own) avoid
/// allocating a new visible console for every child process.
pub fn no_window_command(program: &str) -> Command {
    #[cfg(windows)]
    {
        let mut cmd = Command::new(program);
        cmd.creation_flags(CREATE_NO_WINDOW);
        cmd
    }
    #[cfg(not(windows))]
    {
        Command::new(program)
    }
}

/// Spawn `cmd`, capture stdout/stderr via pipe, and wait for the child to
/// finish within `timeout`. If the deadline expires the child is killed first.
///
/// The stdout and stderr **must** already be configured as `Stdio::piped()`
/// before calling this function (otherwise the returned `Output` will contain
/// empty buffers).  Returns `SubprocessFailure::TimedOut` when the child does
/// not exit before the deadline.
pub fn run_output_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Output, SubprocessFailure> {
    let mut child = cmd.spawn().map_err(|e| SubprocessFailure::Io(format!("{}", e)))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let cancel = Arc::new(AtomicBool::new(false));

    // Reader threads — drain pipes concurrently so we never deadlock.
    let stdout_buf = drain_pipe(stdout, cancel.clone());
    let stderr_buf = drain_pipe(stderr, cancel.clone());

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                cancel.store(true, Ordering::Relaxed);
                let out = join_output(stdout_buf, stderr_buf);
                return Ok(Output { status, stdout: out.0, stderr: out.1 });
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    cancel.store(true, Ordering::Relaxed);
                    let _ = join_output(stdout_buf, stderr_buf);
                    return Err(SubprocessFailure::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                cancel.store(true, Ordering::Relaxed);
                let _ = join_output(stdout_buf, stderr_buf);
                return Err(SubprocessFailure::Io(format!("{}", e)));
            }
        }
    }
}

/// Run a command and wait within `timeout`.  Returns `true` if the child
/// exits successfully before the deadline, `false` otherwise (spawn failure,
/// non-zero exit, or timeout).  The child is killed on timeout.
pub fn run_with_timeout(child: &mut Child, timeout: Duration) -> Option<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(_) => return None,
        }
    }
}

/// Stream `stderr` line-by-line through `on_line` while the child runs.
///
/// - Cancel is checked every ~100 ms (not only between lines) and kills the
///   child immediately on detection.
/// - If no line arrives for `stall`, the child is killed and
///   `Err(WatchdogStop::Stalled)` returned.
/// - On normal exit, all buffered lines are delivered via `on_line`, then the
///   exit status is reaped and returned as `Ok(status)`.
pub fn watch_stderr_lines(
    child: &mut Child,
    stderr: impl io::Read + Send + 'static,
    stall: Duration,
    cancel: Option<&AtomicBool>,
    on_line: &mut dyn FnMut(&str),
) -> Result<std::process::ExitStatus, WatchdogStop> {
    let (tx, rx) = mpsc::channel::<String>();

    // Spawn reader thread (detached — handle is dropped on return).
    std::thread::Builder::new()
        .name("stderr-watchdog-reader".into())
        .spawn(move || {
            let reader = std::io::BufReader::new(stderr);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .expect("failed to spawn stderr reader thread");

    let mut last_activity = Instant::now();

    loop {
        // Cancel is checked on every poll iteration, not just per-line.
        if let Some(cancel) = cancel {
            if cancel.load(Ordering::Relaxed) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(WatchdogStop::Cancelled);
            }
        }

        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                last_activity = Instant::now();
                on_line(&line);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if last_activity.elapsed() >= stall {
                    match child.try_wait() {
                        Ok(None) => {
                            // Child is still running but silent → kill.
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(WatchdogStop::Stalled);
                        }
                        // Child already exited — keep draining below.
                        Ok(Some(_)) => {}
                        Err(e) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return Err(WatchdogStop::Wait(format!("{}", e)));
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Reader thread finished. Normally the child has also
                // exited (pipe close = child exit).  Safeguard: if the
                // child closed stderr while still alive (rare edge case),
                // bound the wait by the stall timeout.
                let disconnect_start = Instant::now();
                loop {
                    if disconnect_start.elapsed() >= stall {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(WatchdogStop::Stalled);
                    }
                    match child.try_wait() {
                        Ok(Some(status)) => return Ok(status),
                        Ok(None) => {
                            std::thread::sleep(Duration::from_millis(100));
                        }
                        Err(e) => {
                            return Err(WatchdogStop::Wait(format!("{}", e)));
                        }
                    }
                }
            }
        }
    }
}

/// Spawn a reader thread that reads all bytes from `reader` into a buffer.
/// Sets `cancel` to signal the thread to stop early (on timeout/kill).
fn drain_pipe(
    reader: Option<impl io::Read + Send + 'static>,
    cancel: Arc<AtomicBool>,
) -> Option<JoinHandle<Vec<u8>>> {
    reader.map(|r| {
        std::thread::Builder::new()
            .spawn(move || {
                let mut buf = Vec::new();
                let mut r = r;
                // Read in chunks, checking cancel periodically.
                let mut tmp = [0u8; 8192];
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        break;
                    }
                    // Short timeout on the read so we check cancel.
                    // On Linux we can use `read` which blocks; for simplicity
                    // we rely on the fact that once pipes are closed (child dead)
                    // read will return Ok(0). The cancel flag is only needed
                    // before the pipe closes.
                    match r.read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => buf.extend_from_slice(&tmp[..n]),
                        Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
                        Err(_) => break,
                    }
                }
                buf
            })
            .expect("failed to spawn pipe reader thread")
    })
}

/// Join the stdout/stderr reader threads and return the captured buffers.
fn join_output(
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
) -> (Vec<u8>, Vec<u8>) {
    let out = stdout.and_then(|h| h.join().ok()).unwrap_or_default();
    let err = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    (out, err)
}



#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    fn test_no_window_command_exits_successfully() {
        let mut cmd = no_window_command(success_prog());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let output = cmd.output().expect("should spawn successfully");
        assert!(output.status.success());
    }

    #[test]
    fn test_no_window_command_nonexistent_returns_error() {
        let result = no_window_command("this-command-does-not-exist-99999").output();
        assert!(result.is_err());
    }

    #[test]
    fn test_no_window_command_stdout_captured() {
        let mut cmd = no_window_command(echo_prog());
        cmd.args(echo_args());
        cmd.stdout(Stdio::piped());
        let output = cmd.output().expect("should spawn");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"), "stdout should contain hello, got: {stdout:?}");
    }

    // ── run_output_with_timeout tests ────────────────────────────────────────

    #[test]
    fn test_run_output_with_timeout_fast_command_succeeds() {
        let mut cmd = no_window_command(success_prog());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let result = run_output_with_timeout(&mut cmd, Duration::from_secs(5));
        let output = result.expect("fast command should complete");
        assert!(output.status.success());
    }

    #[test]
    fn test_run_output_with_timeout_captures_stdout() {
        let mut cmd = no_window_command(echo_prog());
        cmd.args(echo_args());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let result = run_output_with_timeout(&mut cmd, Duration::from_secs(5));
        let output = result.expect("echo command should complete");
        assert!(output.status.success());
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.contains("hello"), "stdout should contain hello, got: {stdout:?}");
    }

    #[test]
    fn test_run_output_with_timeout_short_timeout_returns_timedout() {
        let mut cmd = no_window_command(sleep_prog());
        cmd.args(sleep_args(30)); // 30 s — way longer than our timeout
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        let start = Instant::now();
        let result = run_output_with_timeout(&mut cmd, Duration::from_millis(100));
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(SubprocessFailure::TimedOut)));
        // Should return well before 30 s, and with margin for scheduling
        assert!(elapsed < Duration::from_secs(5), "took {:?} — process not killed promptly?", elapsed);
    }

    #[test]
    fn test_run_output_with_timeout_nonexistent_returns_io_error() {
        let mut cmd = no_window_command("this-command-does-not-exist-99999");
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        let result = run_output_with_timeout(&mut cmd, Duration::from_secs(1));
        assert!(matches!(result, Err(SubprocessFailure::Io(_))));
    }

    // ── watch_stderr_lines tests ────────────────────────────────────────────

    #[test]
    fn test_watch_stderr_lines_delivers_lines_and_status() {
        let mut cmd = if cfg!(windows) {
            let mut c = no_window_command("cmd");
            c.args(["/C", "echo hello>&2 & echo world>&2"]);
            c
        } else {
            let mut c = no_window_command("sh");
            c.args(["-c", "echo hello >&2; echo world >&2"]);
            c
        };
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("should spawn");
        let stderr = child.stderr.take().unwrap();

        let mut lines = Vec::new();
        let status = watch_stderr_lines(
            &mut child,
            stderr,
            Duration::from_secs(5),
            None,
            &mut |l| lines.push(l.to_string()),
        )
        .expect("should complete successfully");
        assert!(status.success());
        assert_eq!(lines.len(), 2, "should have 2 lines: {:?}", lines);
    }

    #[test]
    fn test_watch_stderr_lines_cancel_while_silent() {
        let mut cmd = no_window_command(sleep_prog());
        cmd.args(sleep_args(30));
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn().expect("should spawn");
        // stderr was null → pass a dummy reader that reads nothing.
        let cancel = Arc::new(AtomicBool::new(true));
        let stderr_reader = std::io::Cursor::new(Vec::<u8>::new());

        let start = Instant::now();
        let result = watch_stderr_lines(
            &mut child,
            stderr_reader,
            Duration::from_secs(30),
            Some(&cancel),
            &mut |_| {},
        );
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(WatchdogStop::Cancelled)));
        assert!(
            elapsed < Duration::from_secs(5),
            "cancel took {:?} — should return promptly",
            elapsed
        );
    }

    #[test]
    fn test_watch_stderr_lines_stall_kills_silent_child() {
        let mut cmd = no_window_command(sleep_prog());
        cmd.args(sleep_args(30));
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());

        let mut child = cmd.spawn().expect("should spawn");
        let stderr_reader = std::io::Cursor::new(Vec::<u8>::new());
        let stall = Duration::from_millis(200);

        let start = Instant::now();
        let result = watch_stderr_lines(
            &mut child,
            stderr_reader,
            stall,
            None,
            &mut |_| {},
        );
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(WatchdogStop::Stalled)));
        assert!(
            elapsed < Duration::from_secs(5),
            "stall timeout took {:?} — should kill promptly",
            elapsed
        );
    }

    #[test]
    fn test_watch_stderr_lines_nonzero_status() {
        let mut cmd = if cfg!(windows) {
            let mut c = no_window_command("cmd");
            c.args(["/C", "exit /b 3"]);
            c
        } else {
            let mut c = no_window_command("sh");
            c.args(["-c", "exit 3"]);
            c
        };
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("should spawn");
        let stderr = child.stderr.take().unwrap();

        let status = watch_stderr_lines(
            &mut child,
            stderr,
            Duration::from_secs(5),
            None,
            &mut |_| {},
        )
        .expect("should complete");
        assert_eq!(status.code(), Some(3));
    }

    // ── platform helpers ─────────────────────────────────────────────────────

    #[cfg(windows)]
    pub(crate) fn success_prog() -> &'static str { "cmd" }
    #[cfg(not(windows))]
    pub(crate) fn success_prog() -> &'static str { "true" }

    #[cfg(windows)]
    pub(crate) fn echo_prog() -> &'static str { "cmd" }
    #[cfg(not(windows))]
    pub(crate) fn echo_prog() -> &'static str { "printf" }

    #[cfg(windows)]
    pub(crate) fn echo_args() -> Vec<&'static str> { vec!["/C", "echo hello world"] }
    #[cfg(not(windows))]
    pub(crate) fn echo_args() -> Vec<&'static str> { vec!["hello world"] }

    #[cfg(windows)]
    pub(crate) fn sleep_prog() -> &'static str { "ping" }
    #[cfg(windows)]
    pub(crate) fn sleep_args(secs: u32) -> Vec<String> { vec!["-n".into(), (secs + 1).to_string(), "127.0.0.1".into()] }
    #[cfg(not(windows))]
    pub(crate) fn sleep_prog() -> &'static str { "sleep" }
    #[cfg(not(windows))]
    pub(crate) fn sleep_args(secs: u32) -> Vec<String> { vec![secs.to_string()] }

    // ── run_ffmpeg_collect_stderr tests ─────────────────────────────────

    fn spawn_sh(script: &'static str) -> impl FnMut(&[String]) -> std::io::Result<Child> {
        move |_args: &[String]| {
            std::process::Command::new("sh")
                .args(["-c", script])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
        }
    }

    #[test]
    fn test_collect_stderr_success_collects_lines() {
        let mut spawner = spawn_sh("echo line1 >&2; echo line2 >&2");
        let mut seen = Vec::new();
        let result = run_ffmpeg_collect_stderr(
            &mut spawner, &[], Duration::from_secs(5), None, &mut |l| seen.push(l.to_string()),
        );
        let stderr = result.expect("should succeed");
        assert!(stderr.contains("line1") && stderr.contains("line2"));
        assert_eq!(seen.len(), 2);
    }

    #[test]
    fn test_collect_stderr_exit_code_with_tail() {
        let mut spawner = spawn_sh("echo boom >&2; exit 1");
        let result = run_ffmpeg_collect_stderr(
            &mut spawner, &[], Duration::from_secs(5), None, &mut |_| {},
        );
        match result {
            Err(FfmpegRunError::Exit { code, stderr_tail }) => {
                assert_eq!(code, "1");
                assert!(stderr_tail.contains("boom"), "tail: {}", stderr_tail);
            }
            other => panic!("expected Exit, got {:?}", other),
        }
    }

    #[test]
    fn test_collect_stderr_silent_child_stalls() {
        let mut spawner = spawn_sh("sleep 5");
        let start = std::time::Instant::now();
        let result = run_ffmpeg_collect_stderr(
            &mut spawner, &[], Duration::from_millis(200), None, &mut |_| {},
        );
        assert!(matches!(result, Err(FfmpegRunError::Stalled)), "got {:?}", result);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn test_collect_stderr_precancelled() {
        let cancel = AtomicBool::new(true);
        let mut spawner = spawn_sh("sleep 5");
        let result = run_ffmpeg_collect_stderr(
            &mut spawner, &[], Duration::from_secs(5), Some(&cancel), &mut |_| {},
        );
        assert!(matches!(result, Err(FfmpegRunError::Cancelled)), "got {:?}", result);
    }

    #[test]
    fn test_collect_stderr_spawn_failure() {
        let mut spawner = |_args: &[String]| {
            std::process::Command::new("no-such-binary-99999").spawn()
        };
        let result = run_ffmpeg_collect_stderr(
            &mut spawner, &[], Duration::from_secs(5), None, &mut |_| {},
        );
        assert!(matches!(result, Err(FfmpegRunError::Spawn(_))), "got {:?}", result);
    }

    #[test]
    fn test_stderr_tail_shared_helper() {
        assert_eq!(stderr_tail("  hello\n", 400), "hello");
        let long = "0123456789".repeat(100);
        assert_eq!(stderr_tail(&long, 10), "…0123456789");
    }
}
