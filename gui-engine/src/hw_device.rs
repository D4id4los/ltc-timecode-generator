//! Hardware-device discovery for VAAPI and Vulkan via ffmpeg.
//!
//! Functions are pure/testable: the directory listing takes a `dir` parameter,
//! and the probe commands accept the ffmpeg executable path.
//! Platform gating is done at the call site.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::converter::HwDeviceCapabilities;
use crate::video_codecs::{candidate_args, EncoderClass, HwFramePath, VIDEO_CODECS};

// ── VAAPI render-node enumeration ────────────────────────────────────────

/// List DRM render nodes under `dir` that look like `/dev/dri/renderD*`.
///
/// Returns entries sorted by device number (the numeric suffix after `renderD`).
/// On non-Linux platforms this typically returns an empty vec even if the
/// directory exists, but the caller should check `cfg!(target_os = "linux")`.
pub fn list_vaapi_render_nodes(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut nodes: Vec<PathBuf> = entries
        .filter_map(|e| {
            let path = e.ok()?.path();
            let name = path.file_name()?.to_str()?;
            if name.starts_with("renderD") {
                Some(path)
            } else {
                None
            }
        })
        .collect();

    // Sort by the numeric suffix so the first render node is stable.
    nodes.sort_by(|a, b| {
        let a_num = a
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.trim_start_matches("renderD").parse::<u32>().ok())
            .unwrap_or(0);
        let b_num = b
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.trim_start_matches("renderD").parse::<u32>().ok())
            .unwrap_or(0);
        a_num.cmp(&b_num)
    });

    nodes
}

// ── Probe helpers ────────────────────────────────────────────────────────

/// Run `ffmpeg -v error -init_hw_device vaapi=<device> -h` and check exit
/// status. Returns `true` when the device initialises successfully.
pub fn probe_vaapi(ffmpeg: &str, device: &Path) -> bool {
    let dev_str = device.to_string_lossy();
    Command::new(ffmpeg)
        .args(["-v", "error", "-init_hw_device", &format!("vaapi={}", dev_str), "-h"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `ffmpeg -v error -init_hw_device vulkan -h` and check exit status.
/// Returns `true` when a Vulkan device initialises successfully.
pub fn probe_vulkan(ffmpeg: &str) -> bool {
    Command::new(ffmpeg)
        .args(["-v", "error", "-init_hw_device", "vulkan", "-h"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run all applicable hardware probes and return discovered capabilities.
///
/// Probing is skipped when:
/// - The platform is not Linux (return all-`None`/`false`).
/// - The `encoders` set contains no `*_vaapi` or `*_vulkan` encoder (no point
///   probing hardware that won't be used).
pub fn discover(ffmpeg: &str, encoders: &BTreeSet<String>) -> HwDeviceCapabilities {
    if !cfg!(target_os = "linux") {
        return HwDeviceCapabilities::default();
    }

    let has_vaapi_encoder = encoders.iter().any(|e| e.ends_with("_vaapi"));
    let has_vulkan_encoder = encoders.iter().any(|e| e.ends_with("_vulkan"));

    let vaapi_device = if has_vaapi_encoder {
        // Enumerate /dev/dri and probe until one works.
        let nodes = list_vaapi_render_nodes(Path::new("/dev/dri"));
        nodes
            .iter()
            .find(|node| probe_vaapi(ffmpeg, node))
            .map(|p| p.to_string_lossy().to_string())
    } else {
        None
    };

    let vulkan_available = has_vulkan_encoder && probe_vulkan(ffmpeg);

    HwDeviceCapabilities {
        vaapi_device,
        vulkan_available,
    }
}

// ── HW encoder test-encode validation ───────────────────────────────────
//
// Runs a 1-frame null encode for each hardware encoder candidate to verify
// that the encoder actually works (driver, device, GPU all present).

/// Timeout for a single test-encode invocation (10 seconds).
const TEST_ENCODE_TIMEOUT: Duration = Duration::from_secs(10);

/// Build a 1-frame test-encode command for a hardware encoder.
pub fn build_test_encode_args(
    _ffmpeg: &str,
    encoder: &str,
    hw_frames: Option<HwFramePath>,
    vaapi_device: Option<&str>,
) -> Vec<String> {
    let mut args = Vec::new();
    args.push("-v".to_string());
    args.push("error".to_string());
    args.push("-nostdin".to_string());

    // Prelude for VAAPI / Vulkan (must appear before -i)
    match hw_frames {
        Some(HwFramePath::Vaapi) => {
            let dev = vaapi_device.unwrap_or("/dev/dri/renderD128");
            args.push("-init_hw_device".to_string());
            args.push(format!("vaapi=vaapi0:{}", dev));
            args.push("-filter_hw_device".to_string());
            args.push("vaapi0".to_string());
        }
        Some(HwFramePath::Vulkan) => {
            args.push("-init_hw_device".to_string());
            args.push("vulkan=vulkan0".to_string());
            args.push("-filter_hw_device".to_string());
            args.push("vulkan0".to_string());
        }
        None => {}
    }

    args.push("-f".to_string());
    args.push("lavfi".to_string());
    args.push("-i".to_string());
    args.push("testsrc=size=160x120:rate=25:duration=0.08".to_string());

    if hw_frames.is_some() {
        args.push("-vf".to_string());
        args.push("format=nv12,hwupload".to_string());
    } else {
        args.push("-vf".to_string());
        args.push("format=yuv420p".to_string());
    }

    args.push("-c:v".to_string());
    args.push(encoder.to_string());

    for (key, value) in candidate_args(encoder) {
        args.push(format!("-{}", key));
        args.push((*value).to_string());
    }

    args.push("-f".to_string());
    args.push("null".to_string());
    args.push("-".to_string());

    args
}

/// Run a child process and wait for it to finish within `timeout`, killing
/// it on expiry. Returns `None` on timeout/spawn failure, `Some(exit_success)`
/// otherwise.
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

/// Test-encode a single hardware encoder: run a 1-frame null encode and
/// return `true` if it succeeds within the timeout.
pub fn test_encode(ffmpeg: &str, encoder: &str, hw_frames: Option<HwFramePath>, vaapi_device: Option<&str>) -> bool {
    let args = build_test_encode_args(ffmpeg, encoder, hw_frames, vaapi_device);
    let mut child = match Command::new(ffmpeg)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    run_with_timeout(&mut child, TEST_ENCODE_TIMEOUT).unwrap_or(false)
}

/// Injectable variant of [`test_encode`] for unit testing — accepts a
/// runner closure that receives the argument list and returns success.
pub fn test_encode_with(
    runner: &mut dyn FnMut(&[String]) -> bool,
    encoder: &str,
    hw_frames: Option<HwFramePath>,
    vaapi_device: Option<&str>,
) -> bool {
    let args = build_test_encode_args("ffmpeg", encoder, hw_frames, vaapi_device);
    runner(&args)
}

/// Validate all registered hardware encoder candidates by running a
/// 1-frame test encode for each one that is listed in `available_encoders`
/// and passes its hw-frame gating. Non-functional encoders are removed
/// from `available_encoders`.
pub fn validate_hw_encoders(encoders: &mut BTreeSet<String>, hw: &HwDeviceCapabilities) {
    validate_hw_encoders_with(encoders, hw, &mut |encoder, hw_frames, vaapi_device| {
        test_encode("ffmpeg", encoder, hw_frames, vaapi_device)
    });
}

/// Injectable variant of [`validate_hw_encoders`] for testing.
/// `probe` is called as `probe(encoder_name, hw_frames, vaapi_device_opt)`.
#[allow(clippy::type_complexity)]
pub fn validate_hw_encoders_with(
    encoders: &mut BTreeSet<String>,
    hw: &HwDeviceCapabilities,
    probe: &mut dyn FnMut(&str, Option<HwFramePath>, Option<&str>) -> bool,
) {
    for codec in VIDEO_CODECS {
        for candidate in codec.candidates {
            if candidate.class != EncoderClass::Hardware {
                continue;
            }
            if !encoders.contains(candidate.name) {
                continue;
            }

            // Check hw-frame gating: skip candidates whose hw device isn't
            // available (they're already filtered by resolve_encoder_chain,
            // but still in available_encoders — don't waste time probing).
            let vaapi_device = match candidate.hw_frames {
                Some(HwFramePath::Vaapi) => {
                    match hw.vaapi_device {
                        Some(ref dev) => Some(dev.as_str()),
                        None => continue, // no VAAPI device -> skip probe
                    }
                }
                Some(HwFramePath::Vulkan) => {
                    if !hw.vulkan_available {
                        continue; // no Vulkan device -> skip probe
                    }
                    None
                }
                None => None,
            };

            let ok = probe(candidate.name, candidate.hw_frames, vaapi_device);
            if !ok {
                log::warn!(
                    "HW encoder '{}' failed test encode — removing from available encoders",
                    candidate.name
                );
                encoders.remove(candidate.name);
            } else {
                log::info!("HW encoder '{}' passed test encode", candidate.name);
            }
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_render_nodes_sorts_by_suffix() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path();

        // Create render nodes out of order
        std::fs::write(dir.join("renderD128"), b"").unwrap();
        std::fs::write(dir.join("renderD129"), b"").unwrap();
        std::fs::write(dir.join("renderD127"), b"").unwrap();

        // Non-matching entries
        std::fs::write(dir.join("card0"), b"").unwrap();
        std::fs::write(dir.join("other"), b"").unwrap();

        let nodes = list_vaapi_render_nodes(dir);
        let names: Vec<String> = nodes
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["renderD127", "renderD128", "renderD129"]);
    }

    #[test]
    fn test_list_render_nodes_empty_dir() {
        let tmp = tempfile::TempDir::new().unwrap();
        assert!(list_vaapi_render_nodes(tmp.path()).is_empty());
    }

    #[test]
    fn test_list_render_nodes_nonexistent_dir() {
        assert!(list_vaapi_render_nodes(Path::new("/nonexistent/path")).is_empty());
    }

    #[test]
    fn test_discover_skips_non_linux() {
        // On any platform, when we run the function (cfg gates the probing),
        // the result should be Default on non-Linux.
        let encoders = BTreeSet::from(["av1_vaapi".to_string()]);
        let caps = discover("ffmpeg", &encoders);
        // On non-Linux: default. On Linux: probing may or may not find a device.
        // Just check the function doesn't panic and returns something valid.
        assert!(caps.vaapi_device.is_none() || cfg!(target_os = "linux"));
        assert!(!caps.vulkan_available || cfg!(target_os = "linux"));
    }

    #[test]
    fn test_discover_skips_when_no_relevant_encoder() {
        // Even on Linux, when no *_vaapi or *_vulkan encoder is listed,
        // probing should be skipped entirely.
        let encoders = BTreeSet::from(["libx264".to_string(), "pcm_s24le".to_string()]);
        let caps = discover("ffmpeg", &encoders);
        assert!(caps.vaapi_device.is_none());
        assert!(!caps.vulkan_available);
    }

    #[test]
    fn test_discover_vaapi_no_render_nodes() {
        // When the platform is Linux but /dev/dri has no render nodes, discover
        // should return None for vaapi_device (without panicking).
        if cfg!(target_os = "linux") {
            // Use an empty temp dir as a fake /dev/dri-less environment.
            let tmp = tempfile::TempDir::new().unwrap();
            // We can't easily mock /dev/dri, but we can verify the function
            // doesn't panic when the dir has no render nodes.
            let nodes = list_vaapi_render_nodes(tmp.path());
            assert!(nodes.is_empty());
        }
    }
}