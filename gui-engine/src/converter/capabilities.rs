use std::collections::BTreeSet;
use std::process::Stdio;

use log::info;
use serde;

use crate::subprocess::no_window_command;

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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct HwDeviceCapabilities {
    pub vaapi_device: Option<String>,
    pub vulkan_available: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct FfmpegCapabilities {
    pub has_ffmpeg: bool,
    pub available_encoders: BTreeSet<String>,
    pub available_formats: BTreeSet<String>,
    pub error_message: Option<String>,
    #[serde(default)]
    pub hw: HwDeviceCapabilities,
}

pub fn query_ffmpeg_capabilities() -> FfmpegCapabilities {
    let version_ok = no_window_command("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    match version_ok {
        Ok(status) if status.success() => {}
        Ok(_) => {
            return FfmpegCapabilities {
                has_ffmpeg: false,
                available_encoders: BTreeSet::new(),
                available_formats: BTreeSet::new(),
                error_message: Some(
                    "ffmpeg found but returned a non-zero exit status".to_string(),
                ),
                hw: HwDeviceCapabilities::default(),
            };
        }
        Err(e) => {
            let msg = format!(
                "ffmpeg not found in PATH. Please install ffmpeg to use the converter. \
                 Error: {}",
                e
            );
            log::warn!("{}", msg);
            return FfmpegCapabilities {
                has_ffmpeg: false,
                available_encoders: BTreeSet::new(),
                available_formats: BTreeSet::new(),
                error_message: Some(msg),
                hw: HwDeviceCapabilities::default(),
            };
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
    let output = no_window_command("ffmpeg")
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let text = String::from_utf8_lossy(&out.stdout);
            text.lines()
                .flat_map(|line| {
                    let trimmed = line.trim();
                    if trimmed.is_empty() || trimmed.starts_with('-') || trimmed.starts_with("--") {
                        return Vec::new().into_iter();
                    }
                    if trimmed.starts_with("Encoders:")
                        || trimmed.starts_with("Formats:")
                        || trimmed.starts_with("File formats:")
                    {
                        return Vec::new().into_iter();
                    }
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    if parts.len() >= 2 && filter_fn(parts[0]) {
                        parts[1]
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .collect::<Vec<_>>()
                            .into_iter()
                    } else {
                        Vec::new().into_iter()
                    }
                })
                .collect()
        }
        _ => BTreeSet::new(),
    }
}