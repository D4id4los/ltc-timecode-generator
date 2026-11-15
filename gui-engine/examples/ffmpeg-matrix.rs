//! ffmpeg Container × Video-Codec × Audio-Encoder support matrix tool.
//!
//! Enumerates every combination the converter registry *declares* support
//! for — containers × video codecs (`converter/formats.rs` +
//! `video_codecs.rs::VIDEO_CODECS`) × audio encoders (`formats.rs`) — at
//! runtime, resolves each codec's real encoder chain against the local
//! ffmpeg's capabilities, encodes a ~1 s `testsrc` + `sine` clip through the
//! chain (mirroring the production fallback: hardware candidates first,
//! software last), then ffprobes the result back and verifies the container
//! format, video codec and audio codec that actually landed.
//!
//! This is a *measurement* tool: exit 0 on a completed sweep regardless of
//! per-combination outcomes; non-zero only on operational errors (no ffmpeg,
//! unwritable JSON output). Local-only; never run in CI.
//!
//! Usage:
//! ```text
//! cargo run --release -p gui-engine --example ffmpeg-matrix -- \
//!     [--json <out-path>] [--ffmpeg <path>] [--container mov|mkv|mp4|mxf] \
//!     [--codec av1|h264|h265|prores|dnxhd]
//! ```

use std::collections::BTreeSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gui_engine::converter::{
    container_supports_audio_encoder, container_to_ffmpeg_format, extension_for_container,
    supported_audio_encoders, supported_containers, FfmpegCapabilities,
};
use gui_engine::ffprobe::run_ffprobe_json_with;
use gui_engine::hw_device::{build_test_encode_args, discover, validate_hw_encoders_with};
use gui_engine::subprocess::{
    no_window_command, run_ffmpeg_collect_stderr, run_output_with_timeout, stderr_tail,
    FfmpegRunError, SubprocessFailure, FFMPEG_STALL_TIMEOUT, PROBE_TIMEOUT,
};
use gui_engine::video_codecs::{
    candidate_args, codec_args, codec_supports_container, encoder_class, find_codec,
    resolve_encoder_chain, supported_video_codecs, EncoderClass, HwFramePath,
};

/// Per-combination encode wall-clock cap — a muxer/encoder that takes longer
/// than this on a 1 s clip is treated as a hang.
const ENCODE_TIMEOUT: Duration = Duration::from_secs(30);

const SINE: &str = "sine=frequency=1000:duration=1";

/// Test-clip resolution. DNxHD is resolution-locked (only fixed profiles like
/// 1920x1080 are accepted by the encoder), so the dnxhd chain must be fed a
/// legal frame size; everything else uses a small cheap clip. This measures
/// muxer×codec support, not per-resolution performance.
fn testsrc_for(codec: &str) -> &'static str {
    if codec == "dnxhd" {
        "testsrc=size=1920x1080:rate=25:duration=1"
    } else {
        "testsrc=size=640x360:rate=25:duration=1"
    }
}

/// One matrix row: a registry-declared combination and what happened when it
/// was actually produced and probed back.
struct ComboResult {
    container: &'static str,
    codec: &'static str,
    audio: &'static str,
    /// Concrete ffmpeg encoder that produced the file, if any.
    serving_encoder: Option<String>,
    /// Hardware / software classification of the serving encoder.
    serving_class: Option<&'static str>,
    /// How many chain candidates failed before the serving one.
    fallbacks: usize,
    verdict: Verdict,
    encode_ms: f64,
    probe: Option<ProbeBack>,
    /// Short failure detail (stderr tail / probe mismatch description).
    detail: Option<String>,
}

#[derive(PartialEq)]
enum Verdict {
    /// Encoded and ffprobe-verified: container format, video codec and audio
    /// codec all landed as declared.
    Pass,
    /// Registry says the pair is valid, but no chain encoder is available in
    /// this ffmpeg build.
    NoEncoder,
    /// Every chain candidate failed to encode.
    EncodeFailed,
    /// Encode succeeded but ffprobe verification mismatched or failed.
    VerifyFailed,
    /// The ffmpeg build does not list the container muxer at all.
    NoFormat,
}

impl Verdict {
    fn label(&self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::NoEncoder => "SKIP(no-encoder)",
            Verdict::EncodeFailed => "FAIL(encode)",
            Verdict::VerifyFailed => "FAIL(verify)",
            Verdict::NoFormat => "SKIP(no-format)",
        }
    }
}

struct ProbeBack {
    format_name: String,
    video_codec: Option<String>,
    audio_codec: Option<String>,
}

struct ToolConfig {
    ffmpeg: String,
    ffprobe: String,
    json_out: Option<PathBuf>,
    container_filter: Option<String>,
    codec_filter: Option<String>,
}

fn main() -> ExitCode {
    let cfg = match parse_args() {
        Ok(c) => c,
        Err(code) => return code,
    };

    let caps = match probe_capabilities(&cfg.ffmpeg) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: ffmpeg capability probe failed: {e}");
            return ExitCode::from(2);
        }
    };
    let version = caps.ffmpeg_version.clone().unwrap_or_default();
    println!("ffmpeg matrix: {} ({})", cfg.ffmpeg, version);

    let combos = enumerate_combos(&cfg);
    println!("{} registry combination(s) to exercise\n", combos.len());

    let mut results = Vec::new();
    for (container, codec, audio) in &combos {
        let r = run_combo(&cfg, &caps, container, codec, audio);
        println!(
            "  {:<5} {:<7} {:<10} -> {:<18} {}",
            container,
            codec,
            audio,
            r.verdict.label(),
            r.summary_line()
        );
        if let Some(d) = &r.detail {
            println!("      {d}");
        }
        results.push(r);
    }

    print_summary(&results);

    if let Some(json_path) = &cfg.json_out {
        if let Err(e) = write_json(json_path, &cfg, &version, &results) {
            eprintln!("error: cannot write JSON: {e}");
            return ExitCode::from(2);
        }
        println!("\nJSON written to {}", json_path.display());
    }

    ExitCode::SUCCESS
}

impl ComboResult {
    fn summary_line(&self) -> String {
        let enc = self
            .serving_encoder
            .clone()
            .unwrap_or_else(|| "-".to_string());
        let class = self.serving_class.unwrap_or("-").to_string();
        let fb = if self.fallbacks > 0 {
            format!(" ({} fallback(s))", self.fallbacks)
        } else {
            String::new()
        };
        format!("{:<22} {:<4} {:>7.0} ms", enc, class, self.encode_ms) + &fb
    }
}

fn parse_args() -> Result<ToolConfig, ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cfg = ToolConfig {
        ffmpeg: "ffmpeg".to_string(),
        ffprobe: "ffprobe".to_string(),
        json_out: None,
        container_filter: None,
        codec_filter: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => match it.next() {
                Some(p) => cfg.json_out = Some(PathBuf::from(p)),
                None => return Err(usage_error("--json requires a path")),
            },
            "--ffmpeg" => match it.next() {
                Some(p) => {
                    cfg.ffmpeg = p.clone();
                    // Assume a co-located ffprobe unless overridden implicitly
                    // by PATH; this matches typical ffmpeg release layouts.
                    if let Some(dir) = Path::new(p).parent() {
                        if !dir.as_os_str().is_empty() && dir.join("ffprobe").exists() {
                            cfg.ffprobe = dir.join("ffprobe").to_string_lossy().into_owned();
                        }
                    }
                }
                None => return Err(usage_error("--ffmpeg requires a path")),
            },
            "--container" => match it.next() {
                Some(c) => cfg.container_filter = Some(c.clone()),
                None => return Err(usage_error("--container requires a name")),
            },
            "--codec" => match it.next() {
                Some(c) => cfg.codec_filter = Some(c.clone()),
                None => return Err(usage_error("--codec requires an id")),
            },
            other => return Err(usage_error(&format!("unknown argument: {other}"))),
        }
    }
    Ok(cfg)
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!(
        "error: {msg}\n\nusage: ffmpeg-matrix [--json <out-path>] [--ffmpeg <path>] \
         [--container mov|mkv|mp4|mxf] [--codec av1|h264|h265|prores|dnxhd]"
    );
    ExitCode::from(2)
}

/// Probe the configured ffmpeg binary: version line, encoder/format lists,
/// hw-device discovery, and the 1-frame test-encode validation pass
/// (production stage-2 parity — non-working HW candidates are pruned from
/// the chain before the matrix runs).
fn probe_capabilities(ffmpeg: &str) -> Result<FfmpegCapabilities, String> {
    let version_args = vec!["-version".to_string()];
    let version =
        run_capture(ffmpeg, &version_args, PROBE_TIMEOUT).map_err(|e| format!("-version: {e}"))?;
    let version_line = version
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();

    let encoders_args = vec!["-encoders".to_string(), "-hide_banner".to_string()];
    let encoders_raw = run_capture(ffmpeg, &encoders_args, PROBE_TIMEOUT)
        .map_err(|e| format!("-encoders: {e}"))?;
    let formats_args = vec!["-formats".to_string(), "-hide_banner".to_string()];
    let formats_raw =
        run_capture(ffmpeg, &formats_args, PROBE_TIMEOUT).map_err(|e| format!("-formats: {e}"))?;

    let mut available_encoders = parse_ffmpeg_list(&encoders_raw, |flags| {
        flags.starts_with('V') || flags.starts_with('A')
    });
    let available_formats = parse_ffmpeg_list(&formats_raw, |flags| flags.contains('E'));

    let hw = discover(ffmpeg, &available_encoders);
    validate_hw_encoders_with(
        &mut available_encoders,
        &hw,
        &mut |name, hw_frames, vaapi_dev| test_encode_ok(ffmpeg, name, hw_frames, vaapi_dev),
    );

    Ok(FfmpegCapabilities {
        has_ffmpeg: true,
        available_encoders,
        available_formats,
        error_message: None,
        hw,
        ffmpeg_version: Some(version_line),
    })
}

/// Local replica of the production `parse_ffmpeg_list_output` (crate-private
/// there): parse `ffmpeg -encoders` / `-formats` rows into names, keeping
/// lines whose flag field passes the filter.
fn parse_ffmpeg_list(text: &str, filter: fn(&str) -> bool) -> BTreeSet<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('-')
                || trimmed.starts_with("Encoders:")
                || trimmed.starts_with("Formats:")
                || trimmed.starts_with("File formats:")
            {
                return None;
            }
            let parts: Vec<&str> = trimmed.split_whitespace().collect();
            (parts.len() >= 2 && filter(parts[0])).then(|| {
                parts[1]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect()
}

fn test_encode_ok(
    ffmpeg: &str,
    encoder: &str,
    hw_frames: Option<HwFramePath>,
    vaapi_device: Option<&str>,
) -> bool {
    let args = build_test_encode_args(ffmpeg, encoder, hw_frames, vaapi_device);
    let mut child = match no_window_command(ffmpeg)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    gui_engine::subprocess::run_with_timeout(&mut child, ENCODE_TIMEOUT).unwrap_or(false)
}

/// Run a helper binary capturing stdout, with timeout — thin wrapper over
/// the shared subprocess helpers.
fn run_capture(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> Result<String, SubprocessFailure> {
    let output = run_output_with_timeout(
        no_window_command(program)
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
        timeout,
    )?;
    if !output.status.success() {
        return Err(SubprocessFailure::NonZeroExit {
            stderr_tail: stderr_tail(&String::from_utf8_lossy(&output.stderr), 200),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Enumerate the full registry-declared matrix: containers × codecs that
/// declare the container × audio encoders the container accepts. Filters
/// apply on the registry ids. This is the runtime enumeration — a new
/// container/codec/audio encoder added to the registry is picked up
/// automatically.
fn enumerate_combos(cfg: &ToolConfig) -> Vec<(&'static str, &'static str, &'static str)> {
    let mut out = Vec::new();
    for (container, _) in supported_containers() {
        if let Some(f) = &cfg.container_filter {
            if container != f {
                continue;
            }
        }
        for (codec, _) in supported_video_codecs() {
            if let Some(f) = &cfg.codec_filter {
                if codec != f {
                    continue;
                }
            }
            if !codec_supports_container(codec, container) {
                continue;
            }
            for (audio, _) in supported_audio_encoders() {
                if container_supports_audio_encoder(container, audio) {
                    out.push((container, codec, audio));
                }
            }
        }
    }
    out
}

/// Encode a 1 s testsrc+sine clip through the codec's resolved chain with
/// production-order fallback, then ffprobe it back.
fn run_combo(
    cfg: &ToolConfig,
    caps: &FfmpegCapabilities,
    container: &'static str,
    codec: &'static str,
    audio: &'static str,
) -> ComboResult {
    let mut r = ComboResult {
        container,
        codec,
        audio,
        serving_encoder: None,
        serving_class: None,
        fallbacks: 0,
        verdict: Verdict::EncodeFailed,
        encode_ms: 0.0,
        probe: None,
        detail: None,
    };

    let ffmpeg_format = container_to_ffmpeg_format(container);
    if !caps.available_formats.contains(ffmpeg_format) {
        r.verdict = Verdict::NoFormat;
        r.detail = Some(format!("muxer '{ffmpeg_format}' not listed by this ffmpeg"));
        return r;
    }

    let chain = resolve_encoder_chain(codec, caps);
    if chain.is_empty() {
        r.verdict = Verdict::NoEncoder;
        r.detail = Some(format!(
            "no available encoder (needs one of: {})",
            find_codec(codec)
                .map(|c| c
                    .candidates
                    .iter()
                    .map(|c| c.name)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ));
        return r;
    }

    let tmp = match tempfile::Builder::new()
        .prefix(&format!("ffmpeg-matrix-{container}-{codec}-{audio}-"))
        .suffix(&format!(".{}", extension_for_container(container)))
        .tempfile()
    {
        Ok(t) => t,
        Err(e) => {
            r.detail = Some(format!("tempfile creation failed: {e}"));
            return r;
        }
    };
    let out_path = tmp.path().to_path_buf();

    let start = Instant::now();
    let mut failures: Vec<String> = Vec::new();
    for (i, encoder) in chain.iter().enumerate() {
        r.fallbacks = i;
        match encode_one(cfg, caps, encoder, codec, audio, ffmpeg_format, &out_path) {
            Ok(()) => {
                r.serving_encoder = Some(encoder.clone());
                r.serving_class = Some(match encoder_class(encoder) {
                    Some(EncoderClass::Hardware) => "hw",
                    _ => "sw",
                });
                break;
            }
            Err(e) => failures.push(format!("{encoder}: {e}")),
        }
    }
    r.encode_ms = start.elapsed().as_secs_f64() * 1000.0;

    if r.serving_encoder.is_none() {
        r.verdict = Verdict::EncodeFailed;
        r.detail = Some(failures.join(" | "));
        return r;
    }

    // ffprobe it back.
    let args: Vec<String> = [
        "-v".to_string(),
        "quiet".to_string(),
        "-print_format".to_string(),
        "json".to_string(),
        "-show_format".to_string(),
        "-show_streams".to_string(),
        out_path.to_string_lossy().into_owned(),
    ]
    .to_vec();
    let parsed: serde_json::Value = match run_ffprobe_json_with(&args, PROBE_TIMEOUT, &mut |a| {
        run_output_with_timeout(
            no_window_command(&cfg.ffprobe)
                .args(a)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
            PROBE_TIMEOUT,
        )
    }) {
        Ok(v) => v,
        Err(e) => {
            r.verdict = Verdict::VerifyFailed;
            r.detail = Some(format!("ffprobe failed: {e}"));
            return r;
        }
    };

    let format_name = parsed
        .pointer("/format/format_name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let mut video_codec = None;
    let mut audio_codec = None;
    if let Some(streams) = parsed.get("streams").and_then(|v| v.as_array()) {
        for s in streams {
            let name = s
                .get("codec_name")
                .and_then(|v| v.as_str())
                .map(String::from);
            match s.get("codec_type").and_then(|v| v.as_str()) {
                Some("video") => video_codec = name,
                Some("audio") => audio_codec = name,
                _ => {}
            }
        }
    }

    let probe = ProbeBack {
        format_name: format_name.clone(),
        video_codec: video_codec.clone(),
        audio_codec: audio_codec.clone(),
    };

    let mut mismatches: Vec<String> = Vec::new();
    let format_tokens: Vec<&str> = format_name.split(',').map(str::trim).collect();
    if !format_tokens.contains(&ffmpeg_format) {
        mismatches.push(format!(
            "container: expected '{ffmpeg_format}', got '{format_name}'"
        ));
    }
    let expected_video = expected_video_codec_name(codec);
    if video_codec.as_deref() != Some(expected_video.as_str()) {
        mismatches.push(format!(
            "video: expected '{expected_video}', got '{}'",
            video_codec.as_deref().unwrap_or("(none)")
        ));
    }
    let expected_audio = expected_audio_codec_name(audio);
    if audio_codec.as_deref() != Some(expected_audio.as_str()) {
        mismatches.push(format!(
            "audio: expected '{expected_audio}', got '{}'",
            audio_codec.as_deref().unwrap_or("(none)")
        ));
    }

    if mismatches.is_empty() {
        r.verdict = Verdict::Pass;
    } else {
        r.verdict = Verdict::VerifyFailed;
        r.detail = Some(mismatches.join("; "));
    }
    r.probe = Some(probe);
    r
}

/// The ffmpeg encoder for `codec` + its hw-device prelude, args assembled the
/// same way the converter's `push_video_encoder` does (codec args, candidate
/// args, hwupload filter for hw-frame paths).
fn encode_one(
    cfg: &ToolConfig,
    caps: &FfmpegCapabilities,
    encoder: &str,
    codec: &str,
    audio: &str,
    ffmpeg_format: &str,
    out_path: &Path,
) -> Result<(), String> {
    let mut args: Vec<String> = vec!["-y".to_string(), "-v".to_string(), "error".to_string()];

    // HW prelude before -i, mirroring ResolvedHwDevice::prelude_args.
    match gui_engine::video_codecs::hw_frames_for(encoder) {
        Some(HwFramePath::Vaapi) => {
            let dev = caps
                .hw
                .vaapi_device
                .clone()
                .unwrap_or_else(|| "/dev/dri/renderD128".to_string());
            args.push("-init_hw_device".to_string());
            args.push(format!("vaapi=vaapi0:{dev}"));
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
    args.push(testsrc_for(codec).to_string());
    args.push("-f".to_string());
    args.push("lavfi".to_string());
    args.push("-i".to_string());
    args.push(SINE.to_string());
    args.push("-map".to_string());
    args.push("0:v".to_string());
    args.push("-map".to_string());
    args.push("1:a".to_string());

    args.push("-c:v".to_string());
    args.push(encoder.to_string());
    for (k, v) in codec_args(codec) {
        args.push(format!("-{k}"));
        args.push((*v).to_string());
    }
    for (k, v) in candidate_args(encoder) {
        args.push(format!("-{k}"));
        args.push((*v).to_string());
    }
    if gui_engine::video_codecs::hw_frames_for(encoder).is_some() {
        args.push("-vf".to_string());
        args.push("format=nv12,hwupload".to_string());
    }

    args.push("-c:a".to_string());
    args.push(audio.to_string());
    args.push("-ar".to_string());
    args.push("48000".to_string());
    args.push("-t".to_string());
    args.push("1".to_string());

    args.push("-f".to_string());
    args.push(ffmpeg_format.to_string());
    args.push(out_path.to_string_lossy().into_owned());

    let ffmpeg = cfg.ffmpeg.clone();
    let mut spawner = move |a: &[String]| {
        no_window_command(&ffmpeg)
            .args(a)
            .stderr(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
    };
    run_ffmpeg_collect_stderr(&mut spawner, &args, FFMPEG_STALL_TIMEOUT, None, &mut |_| {})
        .map(|_| ())
        .map_err(|e| match e {
            FfmpegRunError::Exit { stderr_tail, .. } => {
                // `-v error` output: the first line carries the root cause,
                // later lines are cascading cleanup noise.
                stderr_tail
                    .split('\n')
                    .next()
                    .unwrap_or_default()
                    .to_string()
            }
            FfmpegRunError::Spawn(e) => e.to_string(),
            FfmpegRunError::Cancelled => "cancelled".to_string(),
            FfmpegRunError::Stalled => "stalled".to_string(),
            FfmpegRunError::Wait(e) => e,
        })
}

/// ffprobe `codec_name` expected for a video codec id (vendor prefixes are
/// dropped: libx264 → h264, hevc_nvenc → hevc, prores_ks → prores).
fn expected_video_codec_name(codec: &str) -> String {
    match codec {
        "h265" => "hevc".to_string(),
        other => other.to_string(),
    }
}

/// ffprobe `codec_name` expected for an audio encoder id.
fn expected_audio_codec_name(encoder: &str) -> String {
    match encoder {
        "libopus" => "opus".to_string(),
        other => other.to_string(),
    }
}

fn print_summary(results: &[ComboResult]) {
    println!("\n=== Summary ===");
    let mut counts: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for r in results {
        *counts.entry(r.verdict.label()).or_default() += 1;
    }
    println!("  combinations: {} total", results.len());
    for (verdict, n) in &counts {
        println!("  {:<18} {n}", verdict);
    }
}

// --- JSON output (hand-rolled: stdlib-only, controlled escaping) ---

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn json_str(s: &str) -> String {
    format!("\"{}\"", json_escape(s))
}

fn json_opt(s: Option<&str>) -> String {
    match s {
        Some(v) => json_str(v),
        None => "null".to_string(),
    }
}

fn write_json(
    path: &Path,
    cfg: &ToolConfig,
    version: &str,
    results: &[ComboResult],
) -> std::io::Result<()> {
    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut j = String::with_capacity(32 * 1024);
    j.push_str("{\n");
    j.push_str("  \"schema\": \"ffmpeg-matrix/v1\",\n");
    j.push_str(&format!(
        "  \"binary_version\": {},\n",
        json_str(env!("CARGO_PKG_VERSION"))
    ));
    j.push_str(&format!("  \"ffmpeg\": {},\n", json_str(&cfg.ffmpeg)));
    j.push_str(&format!("  \"ffmpeg_version\": {},\n", json_str(version)));
    j.push_str(&format!("  \"generated_at\": {generated_at},\n"));

    j.push_str("  \"combinations\": [\n");
    for (i, r) in results.iter().enumerate() {
        let probe_part = match &r.probe {
            Some(p) => format!(
                "{{\"format_name\": {}, \"video_codec\": {}, \"audio_codec\": {}}}",
                json_str(&p.format_name),
                json_opt(p.video_codec.as_deref()),
                json_opt(p.audio_codec.as_deref()),
            ),
            None => "null".to_string(),
        };
        let detail_part = match &r.detail {
            Some(d) => format!(", \"detail\": {}", json_str(d)),
            None => String::new(),
        };
        j.push_str(&format!(
            "    {{\"container\": {}, \"codec\": {}, \"audio\": {}, \
             \"serving_encoder\": {}, \"class\": {}, \"fallbacks\": {}, \
             \"verdict\": {}, \"encode_ms\": {:.0}, \"probe\": {}{}}}{}\n",
            json_str(r.container),
            json_str(r.codec),
            json_str(r.audio),
            json_opt(r.serving_encoder.as_deref()),
            json_str(r.serving_class.unwrap_or("-")),
            r.fallbacks,
            json_str(r.verdict.label()),
            r.encode_ms,
            probe_part,
            detail_part,
            if i + 1 < results.len() { "," } else { "" },
        ));
    }
    j.push_str("  ]\n}\n");

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::File::create(path)?;
    f.write_all(j.as_bytes())
}
