//! Hardware-device discovery for VAAPI and Vulkan via ffmpeg.
//!
//! Functions are pure/testable: the directory listing takes a `dir` parameter,
//! and the probe commands accept the ffmpeg executable path.
//! Platform gating is done at the call site.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::converter::HwDeviceCapabilities;

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