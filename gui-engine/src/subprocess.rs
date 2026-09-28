use std::io;
use std::process::{Child, Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Default timeout for ffprobe/exiftool probe subprocesses.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Error type for subprocess operations.
#[derive(Debug, Clone)]
pub enum SubprocessFailure {
    /// The process could not be spawned (bad binary, no such file, etc).
    Io(String),
    /// The process did not exit within the allotted timeout.
    TimedOut,
}

impl std::fmt::Display for SubprocessFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SubprocessFailure::Io(msg) => write!(f, "subprocess I/O error: {}", msg),
            SubprocessFailure::TimedOut => write!(f, "subprocess timed out"),
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
mod tests {
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

    // ── platform helpers ─────────────────────────────────────────────────────

    #[cfg(windows)]
    fn success_prog() -> &'static str { "cmd" }
    #[cfg(not(windows))]
    fn success_prog() -> &'static str { "true" }

    #[cfg(windows)]
    fn echo_prog() -> &'static str { "cmd" }
    #[cfg(not(windows))]
    fn echo_prog() -> &'static str { "printf" }

    #[cfg(windows)]
    fn echo_args() -> Vec<&'static str> { vec!["/C", "echo", "hello"] }
    #[cfg(not(windows))]
    fn echo_args() -> Vec<&'static str> { vec!["hello"] }

    #[cfg(windows)]
    fn sleep_prog() -> &'static str { "ping" }
    #[cfg(windows)]
    fn sleep_args(secs: u32) -> Vec<String> { vec!["-n".into(), (secs + 1).to_string(), "127.0.0.1".into()] }
    #[cfg(not(windows))]
    fn sleep_prog() -> &'static str { "sleep" }
    #[cfg(not(windows))]
    fn sleep_args(secs: u32) -> Vec<String> { vec![secs.to_string()] }
}