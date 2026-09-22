use std::path::Path;
use std::process::{Command, Stdio};

use log::{error, info, warn};

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AudioStreamInfo {
    pub stream_index: usize,
    pub channels: usize,
    pub codec_name: String,
    pub sample_rate: u32,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct VideoAudioProbe {
    pub streams: Vec<AudioStreamInfo>,
    pub total_audio_channels: usize,
    pub is_video_file: bool,
}

const VIDEO_EXTENSIONS: &[&str] = &["mp4", "mov", "mkv", "mts", "m2ts", "mxf", "avi", "webm"];

pub fn path_is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .is_some_and(|e| VIDEO_EXTENSIONS.contains(&e.as_str()))
}

pub fn probe_video_audio(path: &Path) -> Result<VideoAudioProbe, String> {
    let output = Command::new("ffprobe")
        .args([
            "-v", "quiet",
            "-print_format", "json",
            "-show_streams",
            "-select_streams", "a",
            &path.to_string_lossy(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffprobe: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffprobe failed: {}", stderr));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).map_err(|e| format!("Failed to parse ffprobe JSON: {}", e))?;

    let streams_val = parsed
        .get("streams")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "ffprobe returned no streams array".to_string())?;

    let mut streams = Vec::new();
    for s in streams_val {
        let idx = s.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let ch = s.get("channels").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
        let codec = s
            .get("codec_name")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let sample_rate = s
            .get("sample_rate")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(48000);
        streams.push(AudioStreamInfo {
            stream_index: idx,
            channels: ch,
            codec_name: codec,
            sample_rate,
        });
    }

    if streams.is_empty() {
        return Err(format!(
            "No audio streams found in '{}'",
            path.display()
        ));
    }

    let total_audio_channels: usize = streams.iter().map(|s| s.channels).sum();

    info!(
        "Probed '{}': {} audio stream(s), {} total channel(s)",
        path.display(),
        streams.len(),
        total_audio_channels
    );

    Ok(VideoAudioProbe {
        streams,
        total_audio_channels,
        is_video_file: true,
    })
}

/// Build the ffmpeg argument vector used to extract one audio channel from a
/// container file into a mono 24-bit PCM WAV.
///
/// `absolute_stream_index` is the **absolute** stream index within the
/// container — the same numbering as ffprobe's `index` field (e.g. `1` for the
/// only audio track of a typical video+audio MP4). It is mapped via
/// `-map 0:{n}`, as opposed to `-map 0:a:{n}` which would select the n-th
/// *audio* stream.
fn build_extract_args(
    path: &Path,
    absolute_stream_index: usize,
    channel_index: usize,
    output_wav: &Path,
) -> Vec<String> {
    let channel_filter = format!("pan=mono|FC=c{}", channel_index);

    vec![
        "-y".into(),
        "-i".into(),
        path.to_string_lossy().to_string(),
        "-map".into(),
        format!("0:{}", absolute_stream_index),
        "-af".into(),
        channel_filter,
        "-c:a".into(),
        "pcm_s24le".into(),
        "-f".into(),
        "wav".into(),
        output_wav.to_string_lossy().to_string(),
    ]
}

/// Extract a single channel of one audio stream from a container file into a
/// mono 24-bit PCM WAV.
///
/// `absolute_stream_index` is the absolute stream index inside the container
/// (ffprobe's `index` field, as stored in [`AudioStreamInfo::stream_index`]).
pub fn extract_audio_channel(
    path: &Path,
    absolute_stream_index: usize,
    channel_index: usize,
    output_wav: &Path,
) -> Result<(), String> {
    info!(
        "Extracting audio: stream={}, channel={} from '{}' → '{}'",
        absolute_stream_index,
        channel_index,
        path.display(),
        output_wav.display()
    );

    let args = build_extract_args(path, absolute_stream_index, channel_index, output_wav);

    let output = Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {}", e))?;

    if !output.status.success() {
        let _ = std::fs::remove_file(output_wav);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr_tail(&stderr, 400);
        error!(
            "ffmpeg audio extraction failed for '{}' (stream {} channel {}): {}",
            path.display(),
            absolute_stream_index,
            channel_index,
            tail
        );
        return Err(format!(
            "ffmpeg audio extraction failed: stream {} channel {} in '{}': {}",
            absolute_stream_index,
            channel_index,
            path.display(),
            tail
        ));
    }

    info!("Audio extraction successful: {}", output_wav.display());
    Ok(())
}

/// Return the last `max_chars` characters of a string (trimmed), for
/// including the tail of a subprocess's stderr in error messages.
fn stderr_tail(s: &str, max_chars: usize) -> String {
    let s = s.trim();
    let len = s.chars().count();
    if len <= max_chars {
        return s.to_string();
    }
    let tail: String = s.chars().skip(len - max_chars).collect();
    format!("…{}", tail)
}

// ── Keyframe lookup (stream-copy trim snapping) ──────────────────────────

/// Parse ffprobe packet JSON (`-show_entries packet=pts_time,flags`) and
/// return the timestamp of the **last video keyframe at-or-before**
/// `offset_secs`.
///
/// Packets may appear out of order (B-frame reordering), so all entries are
/// scanned and the maximum qualifying keyframe timestamp wins. Returns
/// `None` when no keyframe qualifies.
pub fn parse_last_keyframe(json: &str, offset_secs: f64) -> Option<f64> {
    let parsed: serde_json::Value = serde_json::from_str(json).ok()?;
    let packets = parsed.get("packets")?.as_array()?;
    let mut best: Option<f64> = None;
    for p in packets {
        let flags = p.get("flags").and_then(|v| v.as_str()).unwrap_or("");
        if !flags.contains('K') {
            continue;
        }
        let pts = p
            .get("pts_time")
            .and_then(|v| v.as_str())
            .and_then(|v| v.parse::<f64>().ok())
            .or_else(|| p.get("pts_time").and_then(|v| v.as_f64()));
        if let Some(pts) = pts {
            if pts <= offset_secs + 0.001 && best.map_or(true, |b| pts > b) {
                best = Some(pts);
            }
        }
    }
    best
}

/// Find the timestamp of the last video keyframe at-or-before `offset_secs`
/// in `path`, for snapping stream-copy trims to keyframe boundaries.
///
/// Returns `offset_secs` unchanged when probing fails (degrades to an
/// unsnapped cut) and `0.0` when the file has no keyframe at-or-before the
/// offset within the scan window (cut from the start of the file).
pub fn snap_trim_to_keyframe(path: &Path, offset_secs: f64) -> f64 {
    if offset_secs <= 0.05 {
        return 0.0;
    }

    // Generous lookback window; camera GOPs are typically ≤ 2 s.
    let window_start = (offset_secs - 15.0).max(0.0);
    let interval = format!("{:.3}%{:.3}", window_start, offset_secs + 0.05);

    let output = match Command::new("ffprobe")
        .args([
            "-v", "error",
            "-select_streams", "v:0",
            "-show_entries", "packet=pts_time,flags",
            "-read_intervals", &interval,
            "-of", "json",
            &path.to_string_lossy(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            warn!(
                "Keyframe probe failed for '{}' (offset {:.3}s): {}",
                path.display(),
                offset_secs,
                stderr_tail(stderr.trim(), 200)
            );
            return offset_secs;
        }
        Err(e) => {
            warn!("Failed to run ffprobe for keyframe lookup: {}", e);
            return offset_secs;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    match parse_last_keyframe(&stdout, offset_secs) {
        Some(kf) => {
            if (kf - offset_secs).abs() > 0.001 {
                info!(
                    "Trim {:.3}s snaps to keyframe at {:.3}s in '{}'",
                    offset_secs,
                    kf,
                    path.display()
                );
            }
            kf
        }
        None => {
            warn!(
                "No keyframe found at-or-before {:.3}s in '{}' — trimming from file start",
                offset_secs,
                path.display()
            );
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn test_path_is_video_extensions() {
        assert!(path_is_video(&p("clip.mp4")));
        assert!(path_is_video(&p("clip.MOV")));
        assert!(path_is_video(&p("clip.Mkv")));
        assert!(path_is_video(&p("clip.mxf")));
        assert!(!path_is_video(&p("tone.wav")));
        assert!(!path_is_video(&p("notes.txt")));
        assert!(!path_is_video(&p("noext")));
    }

    #[test]
    fn test_build_extract_args_maps_absolute_stream_index() {
        let args = build_extract_args(&p("/in/c0003.mp4"), 1, 0, &p("/tmp/out.wav"));
        let map_pos = args.iter().position(|a| a == "-map").expect("-map present");
        assert_eq!(
            args[map_pos + 1],
            "0:1",
            "-map must use the absolute stream index (ffprobe `index` numbering), \
             not the n-th-audio-stream form; args: {:?}",
            args
        );
    }

    #[test]
    fn test_build_extract_args_channel_filter_and_pcm() {
        let args = build_extract_args(&p("/in/c0003.mp4"), 2, 1, &p("/tmp/out.wav"));
        let af_pos = args.iter().position(|a| a == "-af").expect("-af present");
        assert_eq!(args[af_pos + 1], "pan=mono|FC=c1");
        let codec_pos = args.iter().position(|a| a == "-c:a").expect("-c:a present");
        assert_eq!(args[codec_pos + 1], "pcm_s24le");
        assert_eq!(args.last().unwrap(), "/tmp/out.wav");
        assert_eq!(args.first().unwrap(), "-y");
    }

    #[test]
    fn test_build_extract_args_stream_zero() {
        let args = build_extract_args(&p("/in/a.mkv"), 0, 3, &p("/tmp/o.wav"));
        let map_pos = args.iter().position(|a| a == "-map").unwrap();
        assert_eq!(args[map_pos + 1], "0:0");
        let af_pos = args.iter().position(|a| a == "-af").unwrap();
        assert_eq!(args[af_pos + 1], "pan=mono|FC=c3");
    }

    #[test]
    fn test_parse_sample_rate_from_ffprobe_json() {
        let json = r#"{"streams":[{"index":1,"codec_name":"aac","channels":2,"sample_rate":"48000"}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let s = &parsed["streams"][0];
        let sr = s["sample_rate"].as_str().and_then(|v| v.parse::<u32>().ok()).unwrap_or(48000);
        assert_eq!(sr, 48000);
    }

    #[test]
    fn test_parse_sample_rate_fallback_on_missing() {
        let json = r#"{"streams":[{"index":1,"codec_name":"pcm_s16le","channels":1}]}"#;
        let parsed: serde_json::Value = serde_json::from_str(json).unwrap();
        let s = &parsed["streams"][0];
        let sr = s["sample_rate"].as_str().and_then(|v| v.parse::<u32>().ok()).unwrap_or(48000);
        assert_eq!(sr, 48000);
    }

    #[test]
    fn test_stderr_tail_short_input() {
        assert_eq!(stderr_tail("  hello\n", 400), "hello");
    }

    #[test]
    fn test_stderr_tail_truncates_from_the_end() {
        let long = "0123456789".repeat(100); // 1000 chars
        let tail = stderr_tail(&long, 10);
        assert_eq!(tail, "…0123456789");
        assert_eq!(tail.chars().count(), 11);
    }

    // ── parse_last_keyframe ──────────────────────────────────────────────

    /// Real ffprobe output shape (timestamps absolute, packets unordered
    /// due to B-frame reordering) from a 25 fps H.264 clip with GOP 50.
    const KEYFRAME_JSON: &str = r#"{
        "packets": [
            { "pts_time": "1.080000", "flags": "___" },
            { "pts_time": "0.000000", "flags": "K__" },
            { "pts_time": "1.040000", "flags": "___" },
            { "pts_time": "0.960000", "flags": "___" },
            { "pts_time": "2.000000", "flags": "K__" },
            { "pts_time": "2.160000", "flags": "___" },
            { "pts_time": "2.040000", "flags": "___" }
        ]
    }"#;

    #[test]
    fn test_parse_last_keyframe_picks_max_qualifying() {
        // Offset mid-GOP: last keyframe at or before 3.0 is 2.0
        assert_eq!(parse_last_keyframe(KEYFRAME_JSON, 3.0), Some(2.0));
        assert_eq!(parse_last_keyframe(KEYFRAME_JSON, 2.0), Some(2.0));
        assert_eq!(parse_last_keyframe(KEYFRAME_JSON, 2.0001), Some(2.0));
    }

    #[test]
    fn test_parse_last_keyframe_first_gop() {
        assert_eq!(parse_last_keyframe(KEYFRAME_JSON, 0.5), Some(0.0));
        assert_eq!(parse_last_keyframe(KEYFRAME_JSON, 0.0), Some(0.0));
    }

    #[test]
    fn test_parse_last_keyframe_none_before_any_keyframe() {
        // Negative offsets have no keyframe
        assert_eq!(parse_last_keyframe(KEYFRAME_JSON, -1.0), None);
    }

    #[test]
    fn test_parse_last_keyframe_empty_or_garbage() {
        assert_eq!(parse_last_keyframe(r#"{"packets": []}"#, 5.0), None);
        assert_eq!(parse_last_keyframe("not json", 5.0), None);
        assert_eq!(parse_last_keyframe(r#"{"other": 1}"#, 5.0), None);
    }

    #[test]
    fn test_parse_last_keyframe_flags_without_k_ignored() {
        let json = r#"{"packets": [
            { "pts_time": "1.0", "flags": "__" },
            { "pts_time": "2.0", "flags": "___" }
        ]}"#;
        assert_eq!(parse_last_keyframe(json, 5.0), None);
    }

    #[test]
    fn test_parse_last_keyframe_mixed_k_flags() {
        // Discard/corrupt flag combos like "K_" or "-K-" still count
        let json = r#"{"packets": [
            { "pts_time": "1.5", "flags": "K_" }
        ]}"#;
        assert_eq!(parse_last_keyframe(json, 5.0), Some(1.5));
    }
}