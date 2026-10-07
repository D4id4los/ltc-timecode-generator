use std::collections::BTreeSet;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
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
    /// First line of `ffmpeg -version` (cache-key ingredient for the HW
    /// validation cache). `None` when unknown (probe failure, test fakes).
    #[serde(default)]
    pub ffmpeg_version: Option<String>,
}

pub fn query_ffmpeg_capabilities() -> FfmpegCapabilities {
    let (caps, timings) = query_ffmpeg_capabilities_timed();
    log::info!(
        "ffmpeg capability probe timings: version={}ms encoders={}ms formats={}ms discover={}ms ({} VAAPI nodes) validate={}ms across {} candidate(s)",
        timings.version_ms,
        timings.encoders_ms,
        timings.formats_ms,
        timings.discover_ms,
        timings.discover_vaapi_nodes,
        timings.validate_ms,
        timings.validate_candidates.len(),
    );
    for (name, ms) in &timings.validate_candidates {
        log::info!("  hw encoder '{}' test encode: {}ms", name, ms);
    }
    caps
}

/// Per-stage wall-clock timings of one capability-probe run (diagnostics;
/// durations are logged for humans, never asserted in tests).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProbeTimings {
    pub version_ms: u64,
    pub encoders_ms: u64,
    pub formats_ms: u64,
    pub discover_ms: u64,
    pub discover_vaapi_nodes: usize,
    pub validate_ms: u64,
    /// `(encoder name, elapsed ms)` per test-encoded HW candidate, in probe order.
    pub validate_candidates: Vec<(String, u64)>,
}

fn elapsed_ms(start: Instant) -> u64 {
    start.elapsed().as_millis().min(u64::MAX as u128) as u64
}

/// [`query_ffmpeg_capabilities`] with per-stage timing instrumentation.
/// The functional result is identical to the plain wrapper; the timings are
/// additive diagnostics (surfaced via `--probe-caps` and the info log).
pub fn query_ffmpeg_capabilities_timed() -> (FfmpegCapabilities, ProbeTimings) {
    let mut timings = ProbeTimings::default();

    let start = Instant::now();
    let probe = probe_ffmpeg_version();
    timings.version_ms = elapsed_ms(start);
    let version = match probe {
        FfmpegVersionProbe::Available(version) => Some(version),
        ref failure => {
            if matches!(failure, FfmpegVersionProbe::TimedOut) {
                log::warn!("ffmpeg -version probe timed out; treating ffmpeg as unavailable");
            }
            return (caps_for_version_failure(failure), timings);
        }
    };

    let start = Instant::now();
    let encoders = run_ffmpeg_list(&["-encoders", "-hide_banner"], |flags| {
        let f = flags.as_bytes();
        !f.is_empty() && (f[0] == b'V' || f[0] == b'A')
    });
    timings.encoders_ms = elapsed_ms(start);

    let start = Instant::now();
    let formats = run_ffmpeg_list(&["-formats", "-hide_banner"], |flags| flags.contains('E'));
    timings.formats_ms = elapsed_ms(start);

    // Node count is a cheap directory read done for the measurement record;
    // `discover` itself re-lists the nodes when probing.
    timings.discover_vaapi_nodes =
        crate::hw_device::list_vaapi_render_nodes(Path::new("/dev/dri")).len();

    let start = Instant::now();
    let hw = crate::hw_device::discover("ffmpeg", &encoders);
    timings.discover_ms = elapsed_ms(start);

    let mut caps = FfmpegCapabilities {
        has_ffmpeg: true,
        available_encoders: encoders,
        available_formats: formats,
        error_message: None,
        hw,
        ffmpeg_version: version,
    };

    let start = Instant::now();
    let mut candidate_timings: Vec<(String, u64)> = Vec::new();
    {
        let mut timed_probe = |name: &str, hw_frames, vaapi_device: Option<&str>| {
            let candidate_start = Instant::now();
            let ok = crate::hw_device::test_encode("ffmpeg", name, hw_frames, vaapi_device);
            candidate_timings.push((name.to_string(), elapsed_ms(candidate_start)));
            ok
        };
        crate::hw_device::validate_hw_encoders_with(
            &mut caps.available_encoders,
            &caps.hw,
            &mut timed_probe,
        );
    }
    timings.validate_candidates = candidate_timings;
    timings.validate_ms = elapsed_ms(start);

    (caps, timings)
}

/// Build the `key=value` report rows for `--probe-caps`. Pure — the builder
/// is the unit-test surface; the CLI merely prints the rows.
pub fn probe_caps_report(
    caps: &FfmpegCapabilities,
    timings: &ProbeTimings,
) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = vec![
        ("has_ffmpeg".to_string(), caps.has_ffmpeg.to_string()),
        (
            "encoder_count".to_string(),
            caps.available_encoders.len().to_string(),
        ),
        (
            "format_count".to_string(),
            caps.available_formats.len().to_string(),
        ),
        (
            "hw_vaapi_device".to_string(),
            caps.hw
                .vaapi_device
                .clone()
                .unwrap_or_else(|| "none".to_string()),
        ),
        (
            "hw_vulkan".to_string(),
            caps.hw.vulkan_available.to_string(),
        ),
        (
            "error".to_string(),
            caps.error_message
                .clone()
                .unwrap_or_else(|| "none".to_string()),
        ),
        ("version_ms".to_string(), timings.version_ms.to_string()),
        ("encoders_ms".to_string(), timings.encoders_ms.to_string()),
        ("formats_ms".to_string(), timings.formats_ms.to_string()),
        ("discover_ms".to_string(), timings.discover_ms.to_string()),
        (
            "discover_vaapi_nodes".to_string(),
            timings.discover_vaapi_nodes.to_string(),
        ),
        ("validate_ms".to_string(), timings.validate_ms.to_string()),
        (
            "validate_candidates".to_string(),
            timings.validate_candidates.len().to_string(),
        ),
    ];
    for (name, ms) in &timings.validate_candidates {
        rows.push((format!("validate_{}", name), ms.to_string()));
    }
    rows
}

fn run_ffmpeg_list(args: &[&str], filter_fn: fn(&str) -> bool) -> BTreeSet<String> {
    run_ffmpeg_list_with(
        || no_window_command("ffmpeg"),
        PROBE_TIMEOUT,
        args,
        filter_fn,
    )
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
    let mut cmd = make_cmd();
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    match run_output_with_timeout(&mut cmd, timeout) {
        Ok(out) if out.status.success() => {
            parse_ffmpeg_list_output(&String::from_utf8_lossy(&out.stdout), filter_fn)
        }
        Ok(_) => {
            log::warn!(
                "ffmpeg list probe ({:?}) exited non-zero; treating as no entries",
                args
            );
            BTreeSet::new()
        }
        Err(e) => {
            log::warn!(
                "ffmpeg list probe ({:?}) failed: {} — treating as no entries",
                args,
                e
            );
            BTreeSet::new()
        }
    }
}

/// Parse the stdout of `ffmpeg -encoders` / `ffmpeg -formats`: skip blank
/// lines, option lines (`-`/`--`) and section headers, split the name field
/// on commas, and keep only lines whose flag field passes `filter_fn`.
pub(crate) fn parse_ffmpeg_list_output(
    text: &str,
    filter_fn: fn(&str) -> bool,
) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('-') {
                return None;
            }
            if trimmed.starts_with("Encoders:")
                || trimmed.starts_with("Formats:")
                || trimmed.starts_with("File formats:")
            {
                return None;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            if parts.len() >= 2 && filter_fn(parts[0]) {
                Some(
                    parts[1]
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .collect::<Vec<_>>(),
                )
            } else {
                None
            }
        })
        .flatten()
        .collect()
}

/// Outcome of the `ffmpeg -version` availability probe. `Available` carries
/// the version string (first stdout line) — the HW-validation cache key
/// ingredient.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum FfmpegVersionProbe {
    Available(String),
    NonZeroExit,
    SpawnFailed(String),
    TimedOut,
}

/// Testable seam: run an already-built command under `timeout` and classify
/// the outcome as a version-probe result.
fn probe_version_with(cmd: &mut Command, timeout: Duration) -> FfmpegVersionProbe {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    match run_output_with_timeout(cmd, timeout) {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let first_line = stdout
                .lines()
                .find(|l| !l.trim().is_empty())
                .map(|l| l.trim().to_string())
                .unwrap_or_default();
            FfmpegVersionProbe::Available(first_line)
        }
        Ok(_) => FfmpegVersionProbe::NonZeroExit,
        Err(crate::subprocess::SubprocessFailure::TimedOut) => FfmpegVersionProbe::TimedOut,
        Err(crate::subprocess::SubprocessFailure::Io(e)) => FfmpegVersionProbe::SpawnFailed(e),
        Err(other) => FfmpegVersionProbe::SpawnFailed(other.to_string()),
    }
}

/// Production wrapper: probe `ffmpeg -version` under [`PROBE_TIMEOUT`].
fn probe_ffmpeg_version() -> FfmpegVersionProbe {
    let mut cmd = no_window_command("ffmpeg");
    cmd.arg("-version");
    probe_version_with(&mut cmd, PROBE_TIMEOUT)
}

/// Map a failed version probe onto the "converter unavailable" capabilities
/// shape (empty encoder/format sets + an error message).
pub(crate) fn caps_for_version_failure(probe: &FfmpegVersionProbe) -> FfmpegCapabilities {
    let msg = match probe {
        FfmpegVersionProbe::Available(version) => {
            return FfmpegCapabilities {
                has_ffmpeg: true,
                available_encoders: BTreeSet::new(),
                available_formats: BTreeSet::new(),
                error_message: None,
                hw: HwDeviceCapabilities::default(),
                ffmpeg_version: Some(version.clone()),
            }
        }
        FfmpegVersionProbe::NonZeroExit => {
            "ffmpeg found but returned a non-zero exit status".to_string()
        }
        FfmpegVersionProbe::SpawnFailed(e) => format!(
            "ffmpeg not found in PATH. Please install ffmpeg to use the converter. Error: {}",
            e
        ),
        FfmpegVersionProbe::TimedOut => format!(
            "ffmpeg did not respond within {}s — treating it as unavailable",
            PROBE_TIMEOUT.as_secs()
        ),
    };
    if !matches!(probe, FfmpegVersionProbe::SpawnFailed(_)) {
        log::warn!("{}", msg);
    }
    FfmpegCapabilities {
        has_ffmpeg: false,
        available_encoders: BTreeSet::new(),
        available_formats: BTreeSet::new(),
        error_message: Some(msg),
        hw: HwDeviceCapabilities::default(),
        ffmpeg_version: None,
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
        assert!(
            matches!(probe, FfmpegVersionProbe::NonZeroExit),
            "got {:?}",
            probe
        );
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
        assert!(
            matches!(probe, FfmpegVersionProbe::Available(_)),
            "got {:?}",
            probe
        );
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

    // ── timed probe + probe_caps_report ─────────────────────────────────────

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .arg("-version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn test_timed_probe_returns_capabilities_unchanged() {
        if !ffmpeg_available() {
            eprintln!("--- SKIPPED: ffmpeg not available on this machine");
            return;
        }
        let (timed_caps, _timings) = query_ffmpeg_capabilities_timed();
        let plain_caps = query_ffmpeg_capabilities();
        assert_eq!(
            timed_caps, plain_caps,
            "the timed path must return identical capabilities"
        );
    }

    #[test]
    fn test_probe_caps_report_structural_facts() {
        let caps = FfmpegCapabilities {
            has_ffmpeg: true,
            available_encoders: BTreeSet::from(["libx264".to_string(), "h264_vaapi".to_string()]),
            available_formats: BTreeSet::from(["matroska".to_string()]),
            error_message: None,
            hw: HwDeviceCapabilities {
                vaapi_device: Some("/dev/dri/renderD128".to_string()),
                vulkan_available: false,
            },
            ffmpeg_version: None,
        };
        let timings = ProbeTimings {
            version_ms: 11,
            encoders_ms: 22,
            formats_ms: 33,
            discover_ms: 44,
            discover_vaapi_nodes: 1,
            validate_ms: 120,
            validate_candidates: vec![("h264_vaapi".to_string(), 120)],
        };

        let rows = probe_caps_report(&caps, &timings);
        let get = |key: &str| rows.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());

        assert_eq!(
            get("has_ffmpeg").as_deref(),
            Some("true"),
            "report must carry has_ffmpeg matching the caps"
        );
        assert_eq!(get("encoder_count").as_deref(), Some("2"));
        // one timing entry per stage field
        for stage in [
            "version_ms",
            "encoders_ms",
            "formats_ms",
            "discover_ms",
            "validate_ms",
        ] {
            assert!(
                get(stage).is_some(),
                "report must carry a {} timing entry",
                stage
            );
        }
        assert_eq!(get("discover_vaapi_nodes").as_deref(), Some("1"));
        // one per-candidate entry per candidate
        assert_eq!(get("validate_h264_vaapi").as_deref(), Some("120"));
    }

    // ── parse_ffmpeg_list_output ────────────────────────────────────────────

    #[test]
    fn test_parse_ffmpeg_list_output_skips_headers_and_options() {
        let text = "\
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
        assert!(got.contains("world"), "got {:?}", got);
    }

    // ── ResolvedHwDevice::prelude_args (WP-B) ─────────────────────────────

    /// ffmpeg arg vectors are legitimate string contracts (AGENTS.md).
    #[test]
    fn resolved_hw_device_prelude_args_arms() {
        let vaapi = ResolvedHwDevice::Vaapi {
            device_path: "/dev/dri/renderD129".to_string(),
        };
        assert_eq!(
            vaapi.prelude_args(),
            vec![
                "-init_hw_device".to_string(),
                "vaapi=vaapi0:/dev/dri/renderD129".to_string(),
                "-filter_hw_device".to_string(),
                "vaapi0".to_string(),
            ]
        );

        assert_eq!(
            ResolvedHwDevice::Vulkan.prelude_args(),
            vec![
                "-init_hw_device".to_string(),
                "vulkan=vulkan0".to_string(),
                "-filter_hw_device".to_string(),
                "vulkan0".to_string(),
            ]
        );
    }
}
