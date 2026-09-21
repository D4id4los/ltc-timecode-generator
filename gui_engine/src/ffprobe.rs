use std::path::Path;
use std::process::{Command, Stdio};

use log::info;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct AudioStreamInfo {
    pub stream_index: usize,
    pub channels: usize,
    pub codec_name: String,
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
        streams.push(AudioStreamInfo {
            stream_index: idx,
            channels: ch,
            codec_name: codec,
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

pub fn extract_audio_channel(
    path: &Path,
    stream_index: usize,
    channel_index: usize,
    output_wav: &Path,
) -> Result<(), String> {
    let channel_filter = format!("pan=mono|FC=c{}", channel_index);

    info!(
        "Extracting audio: stream={}, channel={} from '{}' → '{}'",
        stream_index,
        channel_index,
        path.display(),
        output_wav.display()
    );

    let args: Vec<String> = vec![
        "-y".into(),
        "-i".into(),
        path.to_string_lossy().to_string(),
        "-map".into(),
        format!("0:a:{}", stream_index),
        "-af".into(),
        channel_filter,
        "-c:a".into(),
        "pcm_s24le".into(),
        "-f".into(),
        "wav".into(),
        output_wav.to_string_lossy().to_string(),
    ];

    let output = Command::new("ffmpeg")
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("Failed to run ffmpeg: {}", e))?;

    if !output.status.success() {
        let _ = std::fs::remove_file(output_wav);
        return Err(format!(
            "ffmpeg audio extraction failed: stream {} channel {} in '{}'",
            stream_index,
            channel_index,
            path.display()
        ));
    }

    info!("Audio extraction successful: {}", output_wav.display());
    Ok(())
}