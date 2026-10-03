//! Converter clip-probe policy: probe a recording group's files with
//! ffprobe, camera metadata, and device-name resolution in one place so
//! the engine's probe policy is testable without the engine thread.

use std::io;
use std::path::PathBuf;
use std::process::Output;

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
    probe_clip_set_with(files, &mut camera_meta::run_probe_program)
}

/// Injectable variant of [`probe_clip_set`]: `camera_runner` receives
/// `(program_name, &[arg_strings])` for every camera-metadata probe
/// attempt and must return `io::Result<Output>` matching what the real
/// program would produce.
pub fn probe_clip_set_with(
    files: &[PathBuf],
    camera_runner: &mut dyn FnMut(&str, &[String]) -> io::Result<Output>,
) -> (Vec<Result<VideoAudioProbe, String>>, Vec<Option<crate::CameraInfo>>, String) {
    log::info!("Converter clip probe started: {} file(s)", files.len());
    let probes: Vec<Result<VideoAudioProbe, String>> = files.iter()
        .map(|f| crate::ffprobe::probe_video_audio(f).map_err(|e| e.to_string()))
        .collect();
    let cameras: Vec<Option<crate::CameraInfo>> = files.iter()
        .enumerate()
        .map(|(i, f)| {
            if i < device_name::DEVICE_NAME_PROBE_SAMPLE {
                camera_meta::probe_camera_info_with(f, camera_runner)
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
    /// `None` without spawning a probe. Uses the injectable runner to
    /// count actual probe attempts (all of which fail fast).
    #[test]
    fn camera_probe_is_capped_to_sample_limit() {
        let files: Vec<PathBuf> = (0..(device_name::DEVICE_NAME_PROBE_SAMPLE + 3))
            .map(|i| PathBuf::from(format!("/nonexistent/clip{}.MP4", i)))
            .collect();

        // Each per-file probe may spawn several subprocesses (exiftool,
        // ffprobe fallback) — count the distinct files probed.
        let probed = std::cell::RefCell::new(std::collections::HashSet::new());
        let mut runner = |_prog: &str, args: &[String]| -> std::io::Result<Output> {
            if let Some(path) = args.last() {
                probed.borrow_mut().insert(path.clone());
            }
            Err(std::io::Error::other("no probe subprocess in test"))
        };

        let (_, cameras, _) = probe_clip_set_with(&files, &mut runner);

        let probed = probed.into_inner();
        assert_eq!(
            probed.len(),
            device_name::DEVICE_NAME_PROBE_SAMPLE,
            "camera probe must cover exactly DEVICE_NAME_PROBE_SAMPLE files, probed {:?}",
            probed
        );
        assert_eq!(cameras.len(), files.len());
        for (i, cam) in cameras.iter().enumerate() {
            if i >= device_name::DEVICE_NAME_PROBE_SAMPLE {
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
