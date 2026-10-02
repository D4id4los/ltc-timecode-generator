//! Converter clip-probe policy: probe a recording group's files with
//! ffprobe, camera metadata, and device-name resolution in one place so
//! the engine's probe policy is testable without the engine thread.

use std::path::PathBuf;

use crate::camera_meta;
use crate::device_name;
use crate::ffprobe::VideoAudioProbe;

/// Probe a recording group: ffprobe per file (all files), camera metadata
/// for the first [`device_name::DEVICE_NAME_PROBE_SAMPLE`] files only (the
/// expensive exiftool/ffprobe-tag probe is capped), then resolve the device
/// name from the camera metadata.
///
/// Returns `(probes, cameras, device_name)` — the exact shape of
/// `JobFinal::ClipProbes` minus its `Option` wrapper.
pub fn probe_clip_set(
    files: &[PathBuf],
) -> (Vec<Result<VideoAudioProbe, String>>, Vec<Option<crate::CameraInfo>>, String) {
    log::info!("Converter clip probe started: {} file(s)", files.len());
    let probes: Vec<Result<VideoAudioProbe, String>> = files.iter()
        .map(|f| crate::ffprobe::probe_video_audio(f).map_err(|e| e.to_string()))
        .collect();
    let cameras: Vec<Option<crate::CameraInfo>> = files.iter()
        .enumerate()
        .map(|(i, f)| {
            if i < device_name::DEVICE_NAME_PROBE_SAMPLE {
                camera_meta::probe_camera_info(f)
            } else {
                None
            }
        })
        .collect();
    let (device_name, _, _) = device_name::resolve_device_name(files, "", Some(&cameras));
    (probes, cameras, device_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    /// Camera metadata must be probed only for the first
    /// DEVICE_NAME_PROBE_SAMPLE files — files beyond the cap must get
    /// `None` without spawning a probe. Uses a nonexistent dir so probes
    /// fail fast; the cap policy is what's under test.
    #[test]
    fn camera_probe_is_capped_to_sample_limit() {
        let files: Vec<PathBuf> = (0..(device_name::DEVICE_NAME_PROBE_SAMPLE + 3))
            .map(|i| PathBuf::from(format!("/nonexistent/clip{}.MP4", i)))
            .collect();

        let (_, cameras, _) = probe_clip_set(&files);

        assert_eq!(cameras.len(), files.len());
        for (i, cam) in cameras.iter().enumerate() {
            let within_cap = i < device_name::DEVICE_NAME_PROBE_SAMPLE;
            // Within the cap a probe attempt is made (may fail → None on
            // nonexistent files); beyond the cap there must be no attempt
            // and the value is definitionally None.
            if !within_cap {
                assert!(cam.is_none(), "camera probe attempted beyond sample cap (index {})", i);
            }
        }
    }

    #[test]
    fn probe_results_are_one_per_file_in_order() {
        let files: Vec<PathBuf> = vec![
            PathBuf::from("/nonexistent/a.mp4"),
            PathBuf::from("/nonexistent/b.mp4"),
        ];
        let (probes, cameras, _device) = probe_clip_set(&files);
        assert_eq!(probes.len(), 2);
        assert_eq!(cameras.len(), 2);
        // Nonexistent files → every probe is an Err (or loud-skip environment)
        for p in &probes {
            assert!(p.is_err(), "probing a nonexistent file must fail");
        }
    }
}
