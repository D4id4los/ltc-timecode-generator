use std::process::Command;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

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

    // platform helpers
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
}