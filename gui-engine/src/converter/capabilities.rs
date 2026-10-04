use std::collections::BTreeSet;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde;

use crate::subprocess::{no_window_command, run_output_with_timeout, PROBE_TIMEOUT};

/// Hardware device info resolved for a concrete encoder at conversion time.
#[derive(Clone, Debug)]
pub enum ResolvedHwDevice {
    Vaapi { device_path: String },
    Vulkan,
}

impl ResolvedHwDevice {
    /// Prelude args: `-init_hw_device <type>[=name[:device]]` +
    /// `-filter_hw_device <name>` — must appear before `-i`.
    pub fn prelude_args(&self) -> Vec<String> {
        match self {
            ResolvedHwDevice::Vaapi { device_path } => vec![
                "-init_hw_device".to_string(),
                format!("vaapi=vaapi0:{}", device_path),
                "-filter_hw_device".to_string(),
                "vaapi0".to_string(),
            ],
            ResolvedHwDevice::Vulkan => vec![
                "-init_hw_device".to_string(),
                "vulkan=vulkan0".to_string(),
                "-filter_hw_device".to_string(),
                "vulkan0".to_string(),
            ],
        }
    }
}

/// Hardware-device availability context carried into the encoder fallback
/// loop.  Derived from [`FfmpegCapabilities::hw`] at spawn time.
#[derive(Clone, Debug, Default)]
pub struct HwDeviceContext {
    pub vaapi_device: Option<String>,
    pub vulkan_available: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct HwDeviceCapabilities {
    pub vaapi_device: Option<String>,
    pub vulkan_available: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct FfmpegCapabilities {
    pub has_ffmpeg: bool,
    pub available_encoders: BTreeSet<String>,
    pub available_formats: BTreeSet<String>,
    pub error_message: Option<String>,
    #[serde(default)]
    pub hw: HwDeviceCapabilities,
}

pub fn query_ffmpeg_capabilities() -> FfmpegCapabilities {
    match probe_ffmpeg_version() {
        FfmpegVersionProbe::Available => {}
        ref failure => {
            if matches!(failure, FfmpegVersionProbe::TimedOut) {
                log::warn!("ffmpeg -version probe timed out; treating ffmpeg as unavailable");
            }
            return caps_for_version_failure(failure);
        }
    }

    let encoders = run_ffmpeg_list(&["-encoders", "-hide_banner"], |flags| {
        let f = flags.as_bytes();
        !f.is_empty() && (f[0] == b'V' || f[0] == b'A')
    });
    let formats = run_ffmpeg_list(&["-formats", "-hide_banner"], |flags| {
        flags.contains('E')
    });

    let hw = crate::hw_device::discover("ffmpeg", &encoders);

    let mut caps = FfmpegCapabilities {
        has_ffmpeg: true,
        available_encoders: encoders,
        available_formats: formats,
        error_message: None,
        hw,
    };

    crate::hw_device::validate_hw_encoders(&mut caps.available_encoders, &caps.hw);

    caps
}

fn run_ffmpeg_list(args: &[&str], filter_fn: fn(&str) -> bool) -> BTreeSet<String> {
    run_ffmpeg_list_with(|| no_window_command("ffmpeg"), PROBE_TIMEOUT, args, filter_fn)
}

/// Testable seam for the encoder/format list probe: builds the command via
/// `make_cmd`, runs it under `timeout`, and parses the stdout with the
/// flag `filter_fn`. Any failure (spawn error, non-zero exit, timeout)
/// yields an empty set.
pub(crate) fn run_ffmpeg_list_with(
    make_cmd: impl Fn() -> Command,
    timeout: Duration,
    args: &[&str],
    filter_fn: fn(&str) -> bool,
) -> BTreeSet<String> {
    let _ = (make_cmd, timeout, args, filter_fn);
    BTreeSet::new()
}

/// Parse the stdout of `ffmpeg -encoders` / `ffmpeg -formats`: skip blank
/// lines, option lines (`-`/`--`) and section headers, split the name field
/// on commas, and keep only lines whose flag field passes `filter_fn`.
pub(crate) fn parse_ffmpeg_list_output(
    text: &str,
    filter_fn: fn(&str) -> bool,
) -> BTreeSet<String> {
    let _ = (text, filter_fn);
    BTreeSet::new()
}

/// Outcome of the `ffmpeg -version` availability probe.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FfmpegVersionProbe {
    Available,
    NonZeroExit,
    SpawnFailed(String),
    TimedOut,
}

/// Testable seam: run an already-built command under `timeout` and classify
/// the outcome as a version-probe result.
fn probe_version_with(_cmd: &mut Command, _timeout: Duration) -> FfmpegVersionProbe {
    FfmpegVersionProbe::SpawnFailed("stub".to_string())
}

/// Production wrapper: probe `ffmpeg -version` under [`PROBE_TIMEOUT`].
fn probe_ffmpeg_version() -> FfmpegVersionProbe {
    probe_version_with(&mut no_window_command("ffmpeg"), PROBE_TIMEOUT)
}

/// Map a failed version probe onto the "converter unavailable" capabilities
/// shape (empty encoder/format sets + an error message).
pub(crate) fn caps_for_version_failure(_probe: &FfmpegVersionProbe) -> FfmpegCapabilities {
    FfmpegCapabilities {
        has_ffmpeg: false,
        available_encoders: BTreeSet::new(),
        available_formats: BTreeSet::new(),
        error_message: None,
        hw: HwDeviceCapabilities::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subprocess::tests::{echo_args, echo_prog, sleep_args, sleep_prog, success_prog};

    fn any_flag(_flags: &str) -> bool {
        true
    }

    // ── probe_version_with ──────────────────────────────────────────────────

    #[test]
    fn test_probe_version_hung_command_times_out() {
        let mut cmd = Command::new(sleep_prog());
        cmd.args(sleep_args(30));
        let probe = probe_version_with(&mut cmd, Duration::from_millis(100));
        assert!(
            matches!(probe, FfmpegVersionProbe::TimedOut),
            "a hung command must classify as TimedOut, got {:?}",
            probe
        );
    }

    #[test]
    fn test_probe_version_nonzero_exit_returns_nonzero() {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "exit 1"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "exit 1"]);
            c
        };
        let probe = probe_version_with(&mut cmd, Duration::from_secs(5));
        assert!(matches!(probe, FfmpegVersionProbe::NonZeroExit), "got {:?}", probe);
    }

    #[test]
    fn test_probe_version_spawn_failure() {
        let mut cmd = Command::new("this-command-does-not-exist-99999");
        let probe = probe_version_with(&mut cmd, Duration::from_secs(5));
        assert!(
            matches!(probe, FfmpegVersionProbe::SpawnFailed(_)),
            "got {:?}",
            probe
        );
    }

    #[test]
    fn test_probe_version_success() {
        let mut cmd = Command::new(success_prog());
        let probe = probe_version_with(&mut cmd, Duration::from_secs(5));
        assert!(matches!(probe, FfmpegVersionProbe::Available), "got {:?}", probe);
    }

    // ── caps_for_version_failure ────────────────────────────────────────────

    #[test]
    fn test_caps_from_version_failure_variants_disable_ffmpeg() {
        let probes = [
            FfmpegVersionProbe::NonZeroExit,
            FfmpegVersionProbe::SpawnFailed("nope".to_string()),
            FfmpegVersionProbe::TimedOut,
        ];
        for probe in &probes {
            let caps = caps_for_version_failure(probe);
            assert!(!caps.has_ffmpeg, "{:?} must not enable ffmpeg", probe);
            assert!(
                caps.available_encoders.is_empty() && caps.available_formats.is_empty(),
                "{:?} must not report encoders/formats",
                probe
            );
            assert!(
                caps.error_message.is_some(),
                "{:?} must carry an error message",
                probe
            );
            assert_eq!(caps.hw, HwDeviceCapabilities::default());
        }
    }

    // ── parse_ffmpeg_list_output ────────────────────────────────────────────

    #[test]
    fn test_parse_ffmpeg_list_output_skips_headers_and_options() {
        let text = "\
ffmpeg version 6.1
Encoders:
 V....D libx264  libx264 H.264
 A....D aac  AAC encoder
Formats:
 --enable-something
File formats:
 DE mov  QuickTime format

";
        let got = parse_ffmpeg_list_output(text, any_flag);
        assert!(got.contains("libx264"), "got {:?}", got);
        assert!(got.contains("aac"), "got {:?}", got);
        assert!(got.contains("mov"), "got {:?}", got);
        // headers, option lines, version banner and blanks contribute nothing
        assert_eq!(got.len(), 3, "got {:?}", got);
    }

    #[test]
    fn test_parse_ffmpeg_list_output_splits_comma_names() {
        let text = " V.S... foo,bar,baz  description here\n";
        let got = parse_ffmpeg_list_output(text, any_flag);
        let expected: BTreeSet<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(got, expected);
    }

    #[test]
    fn test_parse_ffmpeg_list_output_applies_flag_filter() {
        let text = "\
 V....D vcodec  video encoder
 A....D acodec  audio encoder
";
        let video_only = parse_ffmpeg_list_output(text, |flags| flags.starts_with('V'));
        assert!(video_only.contains("vcodec"));
        assert!(!video_only.contains("acodec"), "got {:?}", video_only);
    }

    // ── run_ffmpeg_list_with (routing) ──────────────────────────────────────

    #[test]
    fn test_run_ffmpeg_list_respects_timeout() {
        let got = run_ffmpeg_list_with(
            || {
                let mut c = Command::new(sleep_prog());
                c.args(sleep_args(30));
                c
            },
            Duration::from_millis(100),
            &[],
            any_flag,
        );
        assert!(got.is_empty(), "a hung list probe must yield an empty set");
    }

    #[test]
    fn test_run_ffmpeg_list_parses_successful_output() {
        let got = run_ffmpeg_list_with(
            || {
                let mut c = Command::new(echo_prog());
                c.args(echo_args());
                c
            },
            Duration::from_secs(5),
            &[],
            any_flag,
        );
        assert!(got.contains("hello"), "got {:?}", got);
    }
}