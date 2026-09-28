use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use crate::subprocess::{
    no_window_command, run_output_with_timeout, SubprocessFailure, WatchdogStop, PROBE_TIMEOUT,
};
use crate::subprocess::{watch_stderr_lines, FFMPEG_STALL_TIMEOUT};

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
    probe_video_audio_with(path, &mut |args: &[String]| {
        run_output_with_timeout(
            no_window_command("ffprobe")
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
            PROBE_TIMEOUT,
        )
    })
}

/// Injectable-runner variant of [`probe_video_audio`] for testability.
///
/// `runner` receives the full argument list (including the path) and must
/// return the subprocess [`Output`] (with timed-out/killed handled as
/// `Err(SubprocessFailure::TimedOut)`).
pub fn probe_video_audio_with(
    path: &Path,
    runner: &mut dyn FnMut(&[String]) -> Result<Output, SubprocessFailure>,
) -> Result<VideoAudioProbe, String> {
    let args: Vec<String> = vec![
        "-v".into(),
        "quiet".into(),
        "-print_format".into(),
        "json".into(),
        "-show_streams".into(),
        "-select_streams".into(),
        "a".into(),
        path.to_string_lossy().into(),
    ];

    let output = runner(&args).map_err(|e| {
        let msg = match e {
            SubprocessFailure::Io(_) => format!("ffprobe probe failed: {}", e),
            SubprocessFailure::TimedOut => {
                format!("ffprobe timed out after {:.0}s — file may be corrupt", PROBE_TIMEOUT.as_secs_f64())
            }
        };
        error!("{} for '{}'", msg, path.display());
        msg
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("ffprobe failed: {}", stderr.trim()));
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

/// Total timeout for audio extraction ffmpeg calls (CLI decode and similar).
/// Corrupt files can hang indefinitely, so a generous total-timeout bounds the
/// wait and produces a clear error instead of blocking the CLI forever.
pub const EXTRACT_TIMEOUT: Duration = Duration::from_secs(600);

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
    extract_audio_channel_with(
        path,
        absolute_stream_index,
        channel_index,
        output_wav,
        &mut |args: &[String]| {
            run_output_with_timeout(
                no_window_command("ffmpeg")
                    .args(args)
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped()),
                EXTRACT_TIMEOUT,
            )
        },
    )
}

/// Injectable-runner variant of [`extract_audio_channel`] for testability.
///
/// `runner` receives the full argument list (including the input path and
/// output WAV path) and must return the subprocess [`Output`] (with
/// timed-out/killed handled as `Err(SubprocessFailure::TimedOut)`).
pub fn extract_audio_channel_with(
    path: &Path,
    absolute_stream_index: usize,
    channel_index: usize,
    output_wav: &Path,
    runner: &mut dyn FnMut(&[String]) -> Result<Output, SubprocessFailure>,
) -> Result<(), String> {
    info!(
        "Extracting audio: stream={}, channel={} from '{}' → '{}'",
        absolute_stream_index,
        channel_index,
        path.display(),
        output_wav.display()
    );

    let args = build_extract_args(path, absolute_stream_index, channel_index, output_wav);

    let output = runner(&args).map_err(|e| {
        let _ = std::fs::remove_file(output_wav);
        match e {
            SubprocessFailure::Io(msg) => format!("Failed to run ffmpeg: {}", msg),
            SubprocessFailure::TimedOut => format!(
                "ffmpeg audio extraction timed out after {}s — file may be corrupt",
                EXTRACT_TIMEOUT.as_secs()
            ),
        }
    })?;

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

/// Parse an `out_time_us=<int>` progress line from ffmpeg's `-progress pipe:2` output.
/// Returns `Some(duration_seconds)` on a valid match, `None` for other lines
/// (including `N/A`, `out_time=`, `out_time_ms=`).
pub fn parse_out_time_us(line: &str) -> Option<f64> {
    let line = line.trim();
    let prefix = "out_time_us=";
    if let Some(val_str) = line.strip_prefix(prefix) {
        let usecs: f64 = val_str.parse().ok()?;
        Some(usecs / 1_000_000.0)
    } else {
        None
    }
}

/// Quickly probe the duration (in seconds) of a single stream inside a
/// container file by calling ffprobe with `-show_entries stream=duration`.
/// Falls back to the container-level `format.duration` when per-stream
/// duration is unavailable. Returns `None` on failure or timeout.
pub fn probe_stream_duration_secs(path: &Path, absolute_stream_index: usize) -> Option<f64> {
    probe_stream_duration_secs_with(path, absolute_stream_index, &mut |args: &[String]| {
        run_output_with_timeout(
            no_window_command("ffprobe")
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null()),
            PROBE_TIMEOUT,
        )
    })
}

/// Injectable-runner variant of [`probe_stream_duration_secs`].
pub fn probe_stream_duration_secs_with(
    path: &Path,
    absolute_stream_index: usize,
    runner: &mut dyn FnMut(&[String]) -> Result<Output, SubprocessFailure>,
) -> Option<f64> {
    let args: Vec<String> = vec![
        "-v".into(),
        "quiet".into(),
        "-print_format".into(),
        "json".into(),
        "-show_entries".into(),
        "stream=duration:format=duration".into(),
        "-select_streams".into(),
        format!("{}", absolute_stream_index),
        path.to_string_lossy().into(),
    ];

    let output = match runner(&args) {
        Ok(o) if o.status.success() => o,
        Ok(_) => return None,
        Err(SubprocessFailure::TimedOut) => {
            warn!(
                "ffprobe duration probe timed out after {:.0}s for '{}'",
                PROBE_TIMEOUT.as_secs_f64(),
                path.display()
            );
            return None;
        }
        Err(SubprocessFailure::Io(_)) => return None,
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&stdout).ok()?;

    // Try per-stream duration first
    if let Some(streams) = parsed.get("streams").and_then(|v| v.as_array()) {
        for s in streams {
            if let Some(d) = s.get("duration").and_then(|v| v.as_str()) {
                if let Ok(secs) = d.parse::<f64>() {
                    if secs > 0.0 {
                        return Some(secs);
                    }
                }
            }
        }
    }

    // Fallback to format-level duration
    if let Some(d) = parsed.get("format").and_then(|f| f.get("duration")).and_then(|v| v.as_str()) {
        if let Ok(secs) = d.parse::<f64>() {
            if secs > 0.0 {
                return Some(secs);
            }
        }
    }

    None
}

/// Like [`extract_audio_channel`] but uses `-progress pipe:2` to report
/// extraction progress via `on_frac` (called with values 0.0..1.0) and
/// respects the `cancel` flag to kill ffmpeg mid-extraction.
///
/// `total_duration_secs` is used to convert ffmpeg's `out_time_us` into a
/// fraction. When `None`, the function still spawns and parses progress lines
/// but `on_frac` is only called with 0.0 and 1.0 (at start and end).
///
/// On cancel, ffmpeg is killed and `Err("Audio extraction canceled")` is
/// returned.  If ffmpeg hangs silently for 30 s, it is killed with a stall
/// error.
pub fn extract_audio_channel_with_progress(
    path: &Path,
    absolute_stream_index: usize,
    channel_index: usize,
    output_wav: &Path,
    total_duration_secs: Option<f64>,
    cancel: Option<&AtomicBool>,
    on_frac: &impl Fn(f32),
) -> Result<(), String> {
    extract_audio_channel_with_progress_with(
        path,
        absolute_stream_index,
        channel_index,
        output_wav,
        total_duration_secs,
        cancel,
        on_frac,
        &mut |args: &[String]| {
            no_window_command("ffmpeg")
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .spawn()
        },
        FFMPEG_STALL_TIMEOUT,
    )
}

/// Injectable-spawner variant of [`extract_audio_channel_with_progress`] for
/// testability.
///
/// `spawner` receives the full argument list (including the input path and
/// output WAV path, plus `-progress` / `pipe:2` flags) and must return the
/// spawned child with stderr piped. `stall` is the no-output timeout; the
/// public variant uses [`FFMPEG_STALL_TIMEOUT`] (30 s).
pub fn extract_audio_channel_with_progress_with(
    path: &Path,
    absolute_stream_index: usize,
    channel_index: usize,
    output_wav: &Path,
    total_duration_secs: Option<f64>,
    cancel: Option<&AtomicBool>,
    on_frac: &impl Fn(f32),
    spawner: &mut dyn FnMut(&[String]) -> std::io::Result<Child>,
    stall: Duration,
) -> Result<(), String> {
    let mut args = build_extract_args(path, absolute_stream_index, channel_index, output_wav);
    args.push("-progress".to_string());
    args.push("pipe:2".to_string());

    let mut child = spawner(&args).map_err(|e| format!("Failed to spawn ffmpeg: {}", e))?;

    let stderr = child.stderr.take().ok_or("Failed to capture ffmpeg stderr")?;
    let mut stderr_lines: Vec<String> = Vec::new();

    let result = watch_stderr_lines(
        &mut child,
        stderr,
        stall,
        cancel,
        &mut |line| {
            stderr_lines.push(line.to_string());
            if let Some(secs) = parse_out_time_us(line) {
                if let Some(duration) = total_duration_secs {
                    if duration > 0.0 {
                        let frac = (secs / duration).min(1.0) as f32;
                        on_frac(frac);
                    }
                }
            }
        },
    );

    match result {
        Ok(status) if status.success() => {
            info!("Audio extraction (with progress) successful: {}", output_wav.display());
            Ok(())
        }
        Ok(_status) => {
            let _ = std::fs::remove_file(output_wav);
            let stderr = stderr_lines.join("\n");
            let tail = stderr_tail(&stderr, 400);
            error!(
                "ffmpeg audio extraction failed for '{}' (stream {} channel {}): {}",
                path.display(),
                absolute_stream_index,
                channel_index,
                tail
            );
            Err(format!(
                "ffmpeg audio extraction failed: stream {} channel {} in '{}': {}",
                absolute_stream_index,
                channel_index,
                path.display(),
                tail
            ))
        }
        Err(WatchdogStop::Cancelled) => {
            let _ = std::fs::remove_file(output_wav);
            Err("Audio extraction canceled".to_string())
        }
        Err(WatchdogStop::Stalled) => {
            let _ = std::fs::remove_file(output_wav);
            Err(format!(
                "Audio extraction stalled: ffmpeg produced no output for {}s — input may be corrupt",
                stall.as_secs()
            ))
        }
        Err(WatchdogStop::Wait(e)) => {
            let _ = std::fs::remove_file(output_wav);
            Err(format!("Audio extraction wait error: {}", e))
        }
    }
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

    let args: Vec<String> = vec![
        "-v".into(),
        "error".into(),
        "-select_streams".into(),
        "v:0".into(),
        "-show_entries".into(),
        "packet=pts_time,flags".into(),
        "-read_intervals".into(),
        interval,
        "-of".into(),
        "json".into(),
        path.to_string_lossy().into(),
    ];

    let output = match run_output_with_timeout(
        no_window_command("ffprobe")
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        PROBE_TIMEOUT,
    )
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
        Err(SubprocessFailure::TimedOut) => {
            warn!(
                "Keyframe probe timed out after {:.0}s for '{}' (offset {:.3}s)",
                PROBE_TIMEOUT.as_secs_f64(),
                path.display(),
                offset_secs,
            );
            return offset_secs;
        }
        Err(SubprocessFailure::Io(e)) => {
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

    // ── parse_out_time_us ──────────────────────────────────────────────────

    #[test]
    fn test_parse_out_time_us_valid() {
        assert!((parse_out_time_us("out_time_us=1234567").unwrap() - 1.234567).abs() < 1e-9);
        assert!((parse_out_time_us("out_time_us=0").unwrap()).abs() < 1e-9);
        assert!((parse_out_time_us("out_time_us=1000000").unwrap() - 1.0).abs() < 1e-9);
        assert!((parse_out_time_us("out_time_us=999999999").unwrap() - 999.999999).abs() < 1e-9);
    }

    #[test]
    fn test_parse_out_time_us_negative_larger() {
        // Negative microsecond values (ffmpeg shouldn't produce them, but robust)
        let result = parse_out_time_us("out_time_us=-1000000");
        assert!(result.is_some());
        assert!((result.unwrap() + 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_parse_out_time_us_ignores_out_time() {
        assert_eq!(parse_out_time_us("out_time=01:02:03.456789"), None);
    }

    #[test]
    fn test_parse_out_time_us_ignores_out_time_ms() {
        assert_eq!(parse_out_time_us("out_time_ms=1234567"), None);
    }

    #[test]
    fn test_parse_out_time_us_n_a() {
        assert_eq!(parse_out_time_us("out_time_us=N/A"), None);
    }

    #[test]
    fn test_parse_out_time_us_garbage() {
        assert_eq!(parse_out_time_us("not a progress line"), None);
        assert_eq!(parse_out_time_us(""), None);
        assert_eq!(parse_out_time_us("out_time_us="), None);
    }

    #[test]
    fn test_parse_out_time_us_trailing_text() {
        // ffmpeg can emit extra whitespace; trim handles it
        assert!((parse_out_time_us("  out_time_us=5000000  ").unwrap() - 5.0).abs() < 1e-9);
    }

    // ── probe_video_audio_with timeout ──────────────────────────────────────

    fn probe_success_runner(_args: &[String]) -> Result<Output, SubprocessFailure> {
        let json = br#"{"streams":[{"index":1,"codec_name":"aac","channels":2,"sample_rate":"48000"}]}"#;
        Ok(Output {
            status: std::process::ExitStatus::default(),
            stdout: json.to_vec(),
            stderr: Vec::new(),
        })
    }

    fn probe_timeout_runner(_args: &[String]) -> Result<Output, SubprocessFailure> {
        Err(SubprocessFailure::TimedOut)
    }

    fn probe_io_error_runner(_args: &[String]) -> Result<Output, SubprocessFailure> {
        Err(SubprocessFailure::Io("ffprobe not found".into()))
    }

    #[test]
    fn test_probe_video_audio_with_success() {
        let path = Path::new("test.mp4");
        let result = probe_video_audio_with(path, &mut probe_success_runner);
        assert!(result.is_ok());
        let probe = result.unwrap();
        assert_eq!(probe.total_audio_channels, 2);
        assert!(probe.is_video_file);
        assert_eq!(probe.streams.len(), 1);
        assert_eq!(probe.streams[0].codec_name, "aac");
    }

    #[test]
    fn test_probe_video_audio_with_timed_out_returns_error() {
        let path = Path::new("corrupt.mp4");
        let result = probe_video_audio_with(path, &mut probe_timeout_runner);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("timed out"), "error should mention timeout: {}", msg);
        assert!(msg.contains("corrupt"), "error should mention file may be corrupt: {}", msg);
    }

    #[test]
    fn test_probe_video_audio_with_io_error() {
        let path = Path::new("unreadable.mp4");
        let result = probe_video_audio_with(path, &mut probe_io_error_runner);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("ffprobe"), "error should mention ffprobe: {}", msg);
    }

    #[test]
    fn test_probe_video_audio_with_empty_streams_returns_error() {
        // ffprobe returned valid JSON but no streams → error path
        let mut runner = |_: &[String]| {
            let json = r#"{"streams":[]}"#;
            Ok(Output {
                status: std::process::ExitStatus::default(),
                stdout: json.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        };
        let path = Path::new("silent.mp4");
        let result = probe_video_audio_with(path, &mut runner);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("No audio streams"), "error should mention no streams: {}", msg);
    }

    #[test]
    fn test_probe_video_audio_with_invalid_json_returns_error() {
        let mut runner = |_: &[String]| {
            Ok(Output {
                status: std::process::ExitStatus::default(),
                stdout: b"not json".to_vec(),
                stderr: Vec::new(),
            })
        };
        let path = Path::new("garbage.mp4");
        let result = probe_video_audio_with(path, &mut runner);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("Failed to parse"), "error should mention parse failure: {}", msg);
    }

    // ── probe_stream_duration_secs_with timeout ─────────────────────────────

    fn duration_success_runner(_args: &[String]) -> Result<Output, SubprocessFailure> {
        let json = r#"{"streams":[{"duration":"123.456"}]}"#;
        Ok(Output {
            status: std::process::ExitStatus::default(),
            stdout: json.as_bytes().to_vec(),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn test_probe_stream_duration_secs_with_success() {
        let path = Path::new("clip.mp4");
        let result = probe_stream_duration_secs_with(path, 0, &mut duration_success_runner);
        assert!((result.unwrap() - 123.456).abs() < 0.001);
    }

    #[test]
    fn test_probe_stream_duration_secs_with_timed_out_returns_none() {
        let path = Path::new("corrupt.mp4");
        let result = probe_stream_duration_secs_with(path, 0, &mut probe_timeout_runner);
        assert!(result.is_none(), "timeout should return None, got {:?}", result);
    }

    #[test]
    fn test_probe_stream_duration_secs_with_empty_json_returns_none() {
        // ffprobe returned success but JSON has no stream or format duration
        let mut runner = |_: &[String]| {
            let json = r#"{"streams":[{"index":1}]}"#;
            Ok(Output {
                status: std::process::ExitStatus::default(),
                stdout: json.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        };
        let path = Path::new("bad.mp4");
        let result = probe_stream_duration_secs_with(path, 0, &mut runner);
        assert!(result.is_none());
    }

    #[test]
    fn test_probe_stream_duration_secs_with_format_fallback() {
        let mut runner = |_: &[String]| {
            let json = r#"{"format":{"duration":"42.0"}}"#;
            Ok(Output {
                status: std::process::ExitStatus::default(),
                stdout: json.as_bytes().to_vec(),
                stderr: Vec::new(),
            })
        };
        let path = Path::new("clip.mp4");
        let result = probe_stream_duration_secs_with(path, 0, &mut runner);
        assert!((result.unwrap() - 42.0).abs() < 0.001);
    }

    // ── extract_audio_channel_with (runner-based) ─────────────────────────

    fn extract_success_runner(_args: &[String]) -> Result<Output, SubprocessFailure> {
        Ok(Output {
            status: std::process::ExitStatus::default(),
            stdout: Vec::new(),
            stderr: Vec::new(),
        })
    }

    #[test]
    fn test_extract_audio_channel_with_success() {
        let path = Path::new("test.mp4");
        let out = Path::new("/tmp/_unused_test_extract.wav");
        let result = extract_audio_channel_with(path, 1, 0, out, &mut extract_success_runner);
        assert!(result.is_ok());
    }

    #[test]
    fn test_extract_audio_channel_with_timeout() {
        let mut runner = |_: &[String]| Err(SubprocessFailure::TimedOut);
        let path = Path::new("corrupt.mp4");
        let out = Path::new("/tmp/_test_extract_timeout.wav");
        let result = extract_audio_channel_with(path, 1, 0, out, &mut runner);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("timed out"), "msg should mention timeout: {}", msg);
    }

    #[test]
    fn test_extract_audio_channel_with_io_error() {
        let mut runner = |_: &[String]| Err(SubprocessFailure::Io("binary not found".into()));
        let path = Path::new("missing.mp4");
        let out = Path::new("/tmp/_test_extract_io.wav");
        let result = extract_audio_channel_with(path, 1, 0, out, &mut runner);
        assert!(result.is_err());
        let msg = result.unwrap_err();
        assert!(msg.contains("Failed to run ffmpeg"), "msg: {}", msg);
    }

    // ── extract_audio_channel_with_progress_with (spawner-based) ────────

    fn spawn_silent_child(_args: &[String]) -> std::io::Result<std::process::Child> {
        let mut cmd = if cfg!(windows) {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "ping", "-n", "30", "127.0.0.1", ">nul"]);
            c
        } else {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            c
        };
        cmd.stdout(std::process::Stdio::null());
        cmd.stderr(std::process::Stdio::piped());
        cmd.spawn()
    }

    #[test]
    fn test_extract_with_progress_cancel_while_silent() {
        let cancel = std::sync::Arc::new(AtomicBool::new(true));
        let out = Path::new("/tmp/_test_extract_cancel.wav");
        let fracs = std::sync::Mutex::new(Vec::new());
        let start = std::time::Instant::now();

        let result = extract_audio_channel_with_progress_with(
            Path::new("dummy.mp4"),
            1,
            0,
            out,
            None,
            Some(&cancel),
            &|f| {
                let mut p = fracs.lock().unwrap();
                p.push(f);
            },
            &mut spawn_silent_child,
            Duration::from_secs(30),
        );
        let elapsed = start.elapsed();
        assert!(matches!(result, Err(ref e) if e == "Audio extraction canceled"),
            "expected cancel error, got {:?}", result);
        assert!(elapsed < Duration::from_secs(5),
            "cancel took {:?}", elapsed);
    }

    #[test]
    fn test_extract_with_progress_stall_timeout() {
        let out = Path::new("/tmp/_test_extract_stall.wav");
        let start = std::time::Instant::now();

        let result = extract_audio_channel_with_progress_with(
            Path::new("dummy.mp4"),
            1,
            0,
            out,
            None,
            None,
            &|_| {},
            &mut spawn_silent_child,
            Duration::from_millis(200),
        );
        let elapsed = start.elapsed();
        assert!(result.is_err(), "expected stall error, got {:?}", result);
        let msg = result.unwrap_err();
        assert!(msg.contains("stalled") || msg.contains("stall"), "msg: {}", msg);
        assert!(elapsed < Duration::from_secs(5),
            "stall detection took {:?}", elapsed);
    }

    #[test]
    fn test_extract_with_progress_parses_lines() {
        let fracs = std::sync::Mutex::new(Vec::new());

        let mut spawner = |_args: &[String]| {
            let mut cmd = if cfg!(windows) {
                let mut c = std::process::Command::new("cmd");
                c.args(["/C", "echo out_time_us=5000000>&2 & echo out_time_us=10000000>&2"]);
                c
            } else {
                let mut c = std::process::Command::new("sh");
                c.args(["-c", "echo out_time_us=5000000 >&2; echo out_time_us=10000000 >&2"]);
                c
            };
            cmd.stdout(std::process::Stdio::null());
            cmd.stderr(std::process::Stdio::piped());
            cmd.spawn()
        };

        let out = Path::new("/tmp/_test_extract_lines.wav");

        let result = extract_audio_channel_with_progress_with(
            Path::new("dummy.mp4"),
            1,
            0,
            out,
            Some(10.0),
            None,
            &|f| {
                let mut p = fracs.lock().unwrap();
                p.push(f);
            },
            &mut spawner,
            Duration::from_secs(5),
        );

        // Child exits 0 → success
        assert!(result.is_ok(), "expected ok, got {:?}", result);
        let p = fracs.lock().unwrap();
        // Should have called on_frac with at least one value
        assert!(!p.is_empty(), "at least one progress callback should fire, got {:?}", p);
    }
}