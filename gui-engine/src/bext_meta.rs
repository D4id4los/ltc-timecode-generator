//! One home for the camera → WAV-bext field mapping (originator,
//! description, origination date) and the `-write_bext` ffmpeg argument
//! block shared by the converter arg builders and the tagger fallback.

use std::path::Path;

use crate::camera_meta::CameraInfo;

/// Originator string for WAV bext when no camera is detected.
pub const BEXT_DEFAULT_ORIGINATOR: &str = "LTC Timecode Generator";

/// Camera → bext originator: "make model", model, make, or the default.
pub fn bext_originator(camera: Option<&CameraInfo>) -> String {
    let c = match camera {
        Some(c) => c,
        None => return BEXT_DEFAULT_ORIGINATOR.to_string(),
    };
    let make = c.make.as_deref().unwrap_or("");
    let model = c.model.as_deref().unwrap_or("");
    match (make.is_empty(), model.is_empty()) {
        (true, true) => BEXT_DEFAULT_ORIGINATOR.to_string(),
        (true, false) => model.to_string(),
        (false, true) => make.to_string(),
        (false, false) => format!("{} {}", make, model),
    }
}

/// Bext description: "model / lens" (or lens alone) when a lens is known,
/// `None` otherwise (the model alone is already covered by the originator).
pub fn bext_description(camera: Option<&CameraInfo>) -> Option<String> {
    let c = camera?;
    let lens = c.lens.as_ref()?;
    let model = c.model.as_deref().unwrap_or("");
    if !model.is_empty() {
        Some(format!("{} / {}", model, lens))
    } else {
        Some(lens.clone())
    }
}

/// `creation_date`, falling back to the file's mtime formatted `%Y-%m-%d`
/// (chrono). `None` when neither source exists.
pub fn bext_origination_date(camera: Option<&CameraInfo>, file: Option<&Path>) -> Option<String> {
    camera
        .and_then(|c| c.creation_date.clone())
        .or_else(|| {
            let file = file?;
            let meta = std::fs::metadata(file).ok()?;
            let mtime = meta.modified().ok()?;
            let dt: chrono::DateTime<chrono::Local> = mtime.into();
            Some(dt.format("%Y-%m-%d").to_string())
        })
}

/// The `-write_bext 1` + `-metadata originator/origination_date/description`
/// argument block for WAV outputs (format checked by the caller).
pub fn push_wav_bext_args(args: &mut Vec<String>, camera: Option<&CameraInfo>, file: Option<&Path>) {
    args.push("-write_bext".to_string());
    args.push("1".to_string());
    args.push("-metadata".to_string());
    args.push(format!("originator={}", bext_originator(camera)));
    if let Some(ref date) = bext_origination_date(camera, file) {
        args.push("-metadata".to_string());
        args.push(format!("origination_date={}", date));
    }
    if let Some(ref desc) = bext_description(camera) {
        args.push("-metadata".to_string());
        args.push(format!("description={}", desc));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera_meta::CameraMetaSource;

    fn camera(make: Option<&str>, model: Option<&str>, lens: Option<&str>) -> CameraInfo {
        CameraInfo {
            make: make.map(str::to_string),
            model: model.map(str::to_string),
            source: CameraMetaSource::ExifTool,
            creation_date: None,
            lens: lens.map(str::to_string),
            serial: None,
            creation_time: None,
            gamma: None,
            native_timecode: None,
            exposure_summary: None,
        }
    }

    #[test]
    fn test_originator_variants() {
        assert_eq!(bext_originator(None), BEXT_DEFAULT_ORIGINATOR);
        assert_eq!(bext_originator(Some(&camera(Some("Sony"), Some("FS100"), None))), "Sony FS100");
        assert_eq!(bext_originator(Some(&camera(None, Some("GH6"), None))), "GH6");
        assert_eq!(bext_originator(Some(&camera(Some("Canon"), None, None))), "Canon");
        assert_eq!(
            bext_originator(Some(&camera(Some(""), Some(""), None))),
            BEXT_DEFAULT_ORIGINATOR
        );
    }

    #[test]
    fn test_description_variants() {
        assert_eq!(bext_description(None), None);
        assert_eq!(bext_description(Some(&camera(Some("Sony"), None, None))), None);
        assert_eq!(
            bext_description(Some(&camera(Some("Sony"), Some("FS100"), Some("18-105mm")))),
            Some("FS100 / 18-105mm".to_string())
        );
        assert_eq!(
            bext_description(Some(&camera(None, None, Some("18-105mm")))),
            Some("18-105mm".to_string())
        );
    }

    #[test]
    fn test_origination_date_from_camera_wins_over_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("clip.wav");
        std::fs::write(&path, b"x").unwrap();

        let mut c = camera(Some("Sony"), Some("FS100"), None);
        c.creation_date = Some("2026-01-02".to_string());
        assert_eq!(
            bext_origination_date(Some(&c), Some(&path)),
            Some("2026-01-02".to_string())
        );
    }

    #[test]
    fn test_origination_date_falls_back_to_mtime() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("clip.wav");
        std::fs::write(&path, b"x").unwrap();

        assert!(bext_origination_date(None, Some(&path)).is_some(),
            "mtime fallback produces a date");
        assert_eq!(bext_origination_date(None, Some(&dir.path().join("missing.wav"))), None);
        assert_eq!(bext_origination_date(None, None), None);
    }

    #[test]
    fn test_push_wav_bext_args_contains_all_keys() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("clip.wav");
        std::fs::write(&path, b"x").unwrap();
        let c = camera(Some("Sony"), Some("FS100"), Some("18-105mm"));

        let mut args = Vec::new();
        push_wav_bext_args(&mut args, Some(&c), Some(&path));
        let joined = args.join(" ");
        assert!(joined.contains("-write_bext 1"));
        assert!(joined.contains("originator=Sony FS100"));
        assert!(joined.contains("origination_date="));
        assert!(joined.contains("description=FS100 / 18-105mm"));
    }
}
