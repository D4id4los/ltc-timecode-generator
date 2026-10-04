use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use std::path::{Path, PathBuf};

use audio_core::{
    generate_ltc_frame_stereo, increment_timecode, list_audio_devices, AudioCore, ChannelSel,
    Timecode,
};
use clap::Parser;
use log::{error, info, warn};

use crate::command::GuiCommand;
use crate::ffprobe;
use crate::state::AppStateSnapshot;

// ── Logger ──────────────────────────────────────────────────────────────

fn init_logger() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(
            crate::log_buffer::DEFAULT_LOG_FILTER,
        ),
    )
    .try_init();
}

// ── CLI struct ──────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "ltc-timecode-generator", about = "LTC Timecode Generator", version)]
pub struct Cli {
    /// List available audio devices and exit
    #[arg(long, short = 'l')]
    pub list_devices: bool,

    /// Probe ffmpeg capabilities, print a stage-timing report (`key=value` lines) and exit
    #[arg(long)]
    pub probe_caps: bool,

    /// Run in headless mode (no GUI). Implied by --output-to-file.
    #[arg(long, short = 'H')]
    pub headless: bool,

    /// Audio device name/ID (from --list-devices)
    #[arg(long)]
    pub device: Option<String>,

    /// Audio device index (0-based, from --list-devices)
    #[arg(long)]
    pub device_index: Option<usize>,

    /// Starting timecode (HH:MM:SS:FF)
    #[arg(long, default_value = "01:00:00:00")]
    pub start_timecode: String,

    /// Frame rate: 24, 25, 29.97, 30
    #[arg(long, default_value_t = 25.0)]
    pub fps: f64,

    /// Enable drop-frame (only meaningful for 29.97 fps)
    #[arg(long)]
    pub drop_frame: bool,

    /// LTC audio channel: left, right, both
    #[arg(long, default_value = "left")]
    pub channel: String,

    /// LTC volume (0.0 to 1.0)
    #[arg(long, default_value_t = 0.25)]
    pub volume: f32,

    /// Sample rate: 44100 or 48000 (default: auto-detect)
    #[arg(long)]
    pub sample_rate: Option<u32>,

    /// Duration in seconds (omit for indefinite playback)
    #[arg(long)]
    pub duration: Option<f64>,

    /// Write WAV file instead of playing (implies --headless)
    #[arg(long)]
    pub output_to_file: Option<String>,

    /// Print timecode progression (headless) or detailed quality report (decode)
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Enable debug log output to stderr
    #[arg(long, short = 'd')]
    pub debug: bool,

    /// Decode LTC from a WAV or video file and print results (implies headless).
    /// Video files (mp4/mov/mkv/mts/mxf) are auto-detected: audio is extracted via
    /// ffmpeg before decoding. Use --audio-stream and --audio-channel to select
    /// which channel to decode from.
    #[arg(long)]
    pub decode: Option<String>,

    /// Audio stream position among the file's audio streams (0-based, for --decode of video files)
    #[arg(long, default_value_t = 0)]
    pub audio_stream: usize,

    /// Channel index within the selected audio stream (0-based, for --decode of video files)
    #[arg(long, default_value_t = 0)]
    pub audio_channel: usize,

    /// Decoder implementation: "builtin" (default) or "libltc"
    #[arg(long, default_value = "builtin", value_parser = clap::builder::PossibleValuesParser::new(["builtin", "libltc"]))]
    pub decoder: String,

    /// Frame rate for LTC decoding (default: 25)
    #[arg(long, default_value_t = 25.0)]
    pub decode_fps: f64,

    /// Enable drop-frame for LTC decoding (only meaningful for 29.97 fps)
    #[arg(long)]
    pub decode_drop_frame: bool,

    /// Use single-pass (non-chunked) decode instead of chunked parallel decode.
    /// Preferred for small files or when memory is not a concern.
    #[arg(long)]
    pub single_pass: bool,

    /// Context window: number of frames to show before/after gaps and glitches in verbose quality report
    #[arg(long, default_value_t = 3)]
    pub context_frames: u32,

    /// Print all decoded timecodes from the LTC file
    #[arg(short = 't', long = "list-timecodes", help = "Print all decoded LTC timecodes")]
    pub list_timecodes: bool,

    /// Start LTC generation immediately when the GUI opens (GUI mode only)
    #[arg(long)]
    pub autostart: bool,
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn timecode_fmt(tc: &Timecode) -> String {
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        tc.hours, tc.minutes, tc.seconds, tc.frames
    )
}

/// Field of a [`Timecode`] that failed to parse or was out of range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TcField {
    Hours,
    Minutes,
    Seconds,
    Frames,
}

impl TcField {
    fn as_str(self) -> &'static str {
        match self {
            TcField::Hours => "hours",
            TcField::Minutes => "minutes",
            TcField::Seconds => "seconds",
            TcField::Frames => "frames",
        }
    }
}

/// Typed error for [`parse_timecode`]. `Display` renders the exact strings
/// the former `Result<_, String>` implementation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TimecodeParseError {
    /// Wrong number of `:`-separated segments (or empty input).
    BadFormat { input: String },
    /// A segment that is not a valid unsigned integer.
    InvalidComponent { field: TcField, input: String },
    /// A segment that parsed but exceeds its valid range.
    OutOfRange(TcField),
}

impl std::fmt::Display for TimecodeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TimecodeParseError::BadFormat { input } => {
                write!(f, "Invalid timecode format '{}' — expected HH:MM:SS:FF", input)
            }
            TimecodeParseError::InvalidComponent { field, input } => {
                write!(f, "Invalid {} in '{}'", field.as_str(), input)
            }
            TimecodeParseError::OutOfRange(field) => match field {
                TcField::Hours => write!(f, "Hours must be 0-23"),
                TcField::Minutes => write!(f, "Minutes must be 0-59"),
                TcField::Seconds => write!(f, "Seconds must be 0-59"),
                // Frames are not range-checked by parse_timecode today; the
                // variant exists for completeness but is currently
                // unreachable (matching prior `Result<_, String>` behavior).
                TcField::Frames => write!(f, "Invalid frames in input"),
            },
        }
    }
}

impl std::error::Error for TimecodeParseError {}

/// Typed error for [`resolve_device`]. `Display` renders the exact strings
/// the former `Result<_, String>` implementation produced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum ResolveDeviceError {
    /// Neither a default device nor any device exists.
    NoDevices,
    /// `--device <NAME>` matched no device (exact, id, or substring).
    NotFound { query: String },
    /// `--device-index <N>` beyond the number of devices found.
    IndexOutOfRange { index: usize, count: usize },
}

impl std::fmt::Display for ResolveDeviceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveDeviceError::NoDevices => {
                write!(f, "No audio devices available")
            }
            ResolveDeviceError::NotFound { query } => {
                write!(
                    f,
                    "Device '{}' not found. Use --list-devices to see available devices.",
                    query
                )
            }
            ResolveDeviceError::IndexOutOfRange { index, count } => {
                write!(
                    f,
                    "Device index {} out of range ({} devices). Use --list-devices to see available devices.",
                    index, count
                )
            }
        }
    }
}

impl std::error::Error for ResolveDeviceError {}

fn parse_timecode(s: &str) -> Result<Timecode, TimecodeParseError> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 4 {
        return Err(TimecodeParseError::BadFormat {
            input: s.to_string(),
        });
    }
    let mut parsed = [0u32; 4];
    let fields = [
        TcField::Hours,
        TcField::Minutes,
        TcField::Seconds,
        TcField::Frames,
    ];
    for (i, field) in fields.iter().enumerate() {
        parsed[i] = parts[i].parse::<u32>().map_err(|_| {
            TimecodeParseError::InvalidComponent {
                field: *field,
                input: s.to_string(),
            }
        })?;
    }
    let [hours, minutes, seconds, frames] = parsed;
    if hours >= 24 {
        return Err(TimecodeParseError::OutOfRange(TcField::Hours));
    }
    if minutes >= 60 {
        return Err(TimecodeParseError::OutOfRange(TcField::Minutes));
    }
    if seconds >= 60 {
        return Err(TimecodeParseError::OutOfRange(TcField::Seconds));
    }
    Ok(Timecode {
        hours,
        minutes,
        seconds,
        frames,
    })
}

// ── Device listing ──────────────────────────────────────────────────────

/// Pure formatter for `--list-devices` output. Owns every output line,
/// including the header and the empty-list line. Infallible; unit-tested.
fn list_device_lines(devices: &[audio_core::AudioDeviceInfo]) -> Vec<String> {
    let mut lines = vec!["Available audio output devices:".to_string()];
    for (i, dev) in devices.iter().enumerate() {
        let default_mark = if dev.is_default { " (default)" } else { "" };
        lines.push(format!("  [{:2}] {}{}", i, dev.name, default_mark));
        lines.push(format!(
            "         channels: {}-{}, sample rate: {}-{} Hz, formats: {}",
            dev.channels_min,
            dev.channels_max,
            dev.sample_rate_min,
            dev.sample_rate_max,
            dev.formats.join(", ")
        ));
    }
    if devices.is_empty() {
        lines.push("  (no output devices found)".to_string());
    }
    lines
}

/// Boundary runner for `--list-devices`: logger init + real device
/// enumeration + printing. No process exit — success returns `Ok(())`
/// (the arm returns `Done` → exit 0 via `main`), enumeration failure
/// returns `Err` for `CliError::ListDevices` rendering.
fn run_list_devices() -> Result<(), String> {
    init_logger();
    let devices = list_audio_devices()?;
    for line in list_device_lines(&devices) {
        println!("{line}");
    }
    Ok(())
}

fn resolve_device(
    devices: &[audio_core::AudioDeviceInfo],
    cli: &Cli,
) -> Result<String, ResolveDeviceError> {
    if let Some(ref id) = cli.device {
        for dev in devices {
            if dev.name == *id || dev.id == *id {
                return Ok(dev.id.clone());
            }
        }
        for dev in devices {
            if dev.name.contains(id) || dev.id.contains(id) {
                return Ok(dev.id.clone());
            }
        }
        return Err(ResolveDeviceError::NotFound {
            query: id.clone(),
        });
    }

    if let Some(index) = cli.device_index {
        if index >= devices.len() {
            return Err(ResolveDeviceError::IndexOutOfRange {
                index,
                count: devices.len(),
            });
        }
        return Ok(devices[index].id.clone());
    }

    for dev in devices {
        if dev.is_default {
            return Ok(dev.id.clone());
        }
    }
    devices.first().map(|d| d.id.clone()).ok_or(ResolveDeviceError::NoDevices)
}

// ── Headless mode ───────────────────────────────────────────────────────

pub fn run_headless(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.debug {
        init_logger();
    }
    let start_tc = parse_timecode(&cli.start_timecode)?;
    let fps = cli.fps;
    let drop_frame = cli.drop_frame;
    let channel = ChannelSel::parse(&cli.channel).ok_or_else(|| {
        format!(
            "Unsupported channel: '{}'. Must be one of: left, right, both",
            cli.channel
        )
    })?;
    let volume = cli.volume;
    let verbose = cli.verbose;

    let valid_fps = [24.0, 25.0, 29.97, 30.0];
    if !valid_fps.iter().any(|f| (f - fps).abs() < 0.01) {
        return Err(
            format!("Unsupported fps: {}. Must be one of: 24, 25, 29.97, 30", fps).into(),
        );
    }

    if !(0.0..=1.0).contains(&volume) {
        return Err("Volume must be between 0.0 and 1.0".to_string().into());
    }

    info!(
        "Headless mode: tc={}, fps={}, drop_frame={}, channel={}, volume={}",
        timecode_fmt(&start_tc),
        fps,
        drop_frame,
        channel.as_str(),
        volume
    );

    let devices =
        list_audio_devices().map_err(|e| format!("Failed to list audio devices: {}", e))?;
    let device_id = resolve_device(&devices, &cli)?;
    info!("Using audio device: id={}", device_id);

    let sample_rate = cli.sample_rate.unwrap_or_else(audio_core::suggest_sample_rate);

    let core = AudioCore::new();

    core.init_output(&device_id, sample_rate, 0)
        .map_err(|e| format!("Failed to initialize audio output: {}", e))?;

    info!("Audio output initialized at {} Hz", sample_rate);

    core.start_ltc(start_tc, fps, drop_frame, channel, volume)
        .map_err(|e| format!("Failed to start LTC stream: {}", e))?;

    info!("LTC stream started");

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();
    ctrlc::set_handler(move || {
        info!("Ctrl+C received, stopping...");
        r.store(false, Ordering::SeqCst);
    })
    .map_err(|e| format!("Failed to set Ctrl+C handler: {}", e))?;

    let start_time = Instant::now();
    let mut last_tc = start_tc;

    if verbose {
        println!(
            "LTC {}  |  {} Hz  |  {} fps{}  |  {:?}",
            timecode_fmt(&start_tc),
            sample_rate,
            fps,
            if drop_frame { " DF" } else { "" },
            if let Some(d) = cli.duration {
                format!("duration {}s", d)
            } else {
                "indefinite".to_string()
            }
        );
        println!("{}", "-".repeat(60));
    }

    let poll_interval = Duration::from_millis(100);
    let max_duration = cli.duration.map(Duration::from_secs_f64);

    while running.load(Ordering::SeqCst) {
        if let Some(max_dur) = max_duration {
            if start_time.elapsed() >= max_dur {
                info!("Duration reached, stopping");
                break;
            }
        }

        if verbose {
            let current_tc = core.current_timecode();
            if current_tc != last_tc {
                println!("  {}  ({})", timecode_fmt(&current_tc), timecode_fmt(&start_tc));
                last_tc = current_tc;
            }
        }

        for event in core.drain_events() {
            match event {
                audio_core::AudioEvent::StreamError(msg) => {
                    error!("Audio stream error: {}", msg);
                }
                audio_core::AudioEvent::StreamDied => {
                    error!("Audio stream died");
                    break;
                }
                audio_core::AudioEvent::StreamDead => {
                    error!("Audio stream permanently dead");
                    break;
                }
                audio_core::AudioEvent::Underrun => {
                    warn!("Audio underrun detected");
                }
                audio_core::AudioEvent::FramesDropped { total } => {
                    warn!("{} frames dropped", total);
                }
                _ => {}
            }
        }

        std::thread::sleep(poll_interval);
    }

    info!("Stopping LTC stream");
    let _ = core.stop_ltc();
    let final_tc = core.current_timecode();
    let _ = core.stop_output();

    let elapsed = start_time.elapsed();
    let total_frames = (elapsed.as_secs_f64() * fps).round() as u64;

    if verbose {
        println!("{}", "-".repeat(60));
        println!(
            "Done.  Duration: {:.1}s  |  Frames: {}  |  Final TC: {}",
            elapsed.as_secs_f64(),
            total_frames,
            timecode_fmt(&final_tc),
        );
    }

    info!(
        "Headless mode complete: duration={:.2}s, frames={}",
        elapsed.as_secs_f64(),
        total_frames
    );
    Ok(())
}

// ── WAV generation ──────────────────────────────────────────────────────

pub fn generate_wav(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.debug {
        init_logger();
    }
    let path = cli
        .output_to_file
        .as_ref()
        .ok_or("--output-to-file path required")?;
    let start_tc = parse_timecode(&cli.start_timecode)?;
    let fps = cli.fps;
    let drop_frame = cli.drop_frame;
    let channel = ChannelSel::parse(&cli.channel).ok_or_else(|| {
        format!(
            "Unsupported channel: '{}'. Must be one of: left, right, both",
            cli.channel
        )
    })?;
    let volume = cli.volume;

    let duration_secs = cli
        .duration
        .ok_or("--duration is required for WAV file output")?;

    if duration_secs <= 0.0 {
        return Err("Duration must be positive".to_string().into());
    }

    let sample_rate = cli.sample_rate.unwrap_or(48000);
    let total_frames = (duration_secs * fps).ceil() as u64;
    let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
    let samples_per_bit = samples_per_frame as f32 / 80.0;

    let num_channels = 2;

    info!(
        "Generating WAV: path={}, tc={}, fps={}, drop_frame={}, duration={}s, frames={}, rate={}Hz",
        path,
        timecode_fmt(&start_tc),
        fps,
        drop_frame,
        duration_secs,
        total_frames,
        sample_rate
    );

    let spec = hound::WavSpec {
        channels: num_channels as u16,
        sample_rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut writer = hound::WavWriter::create(path, spec)
        .map_err(|e| format!("Failed to create WAV file '{}': {}", path, e))?;

    let mut tc = start_tc;
    let mut last_level = (1.0f32, 1.0f32);
    let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

    let report_interval = if total_frames > 100 {
        total_frames / 100
    } else {
        1
    };

    for frame_num in 0..total_frames {
        frame_buf.fill(0.0);
        generate_ltc_frame_stereo(
            &tc,
            drop_frame,
            samples_per_frame,
            samples_per_bit,
            volume,
            channel,
            &mut last_level,
            &mut frame_buf[..samples_per_frame * 2],
        );

        for &sample in &frame_buf[..samples_per_frame * 2] {
            let clamped = sample.clamp(-1.0, 1.0);
            let int_sample = (clamped * i16::MAX as f32) as i16;
            writer
                .write_sample(int_sample)
                .map_err(|e| format!("WAV write error at frame {}: {}", frame_num, e))?;
        }

        tc = increment_timecode(&tc, fps, drop_frame);

        if cli.verbose && (frame_num % report_interval == 0 || frame_num == total_frames - 1) {
            let pct = (frame_num as f64 / total_frames as f64) * 100.0;
            print!("\r  Generating: {:.0}%  ({})", pct, timecode_fmt(&tc));
            std::io::stdout().flush().ok();
        }
    }

    writer
        .finalize()
        .map_err(|e| format!("Failed to finalize WAV file: {}", e))?;

    if cli.verbose {
        println!();
    }
    println!(
        "WAV written: {}  |  {} frames  |  {:.1}s  |  {} Hz  |  {} channels",
        path, total_frames, duration_secs, sample_rate, num_channels
    );

    info!("WAV generation complete: {}", path);
    Ok(())
}

// ── Convenience wrapper ──────────────────────────────────────────────────

/// Parse CLI args using clap. Calling crates don't need `Parser` in scope.
pub fn parse_args() -> Cli {
    Cli::parse()
}

// ── LTC Decode mode ─────────────────────────────────────────────────────

/// Run the actual decode on a WAV file (supports chunked or single-pass).
/// Wraps the shared [`crate::decode::decode_wav_core`] dispatch with the
/// CLI's stderr progress printer (chunked branch only, 200 ms poll,
/// 300 s deadline) — the CLI-only display lives here, decode.rs stays
/// display-free.
fn run_decode_on_wav(
    path: &Path,
    use_libltc: bool,
    single_pass: bool,
    fps: f64,
    drop_frame: bool,
) -> Result<audio_core::LtcDetectionResult, String> {
    // Pre-count so the printer thread knows the total (one extra
    // header-only open — same cost as before the dispatch-core extraction).
    let config = audio_core::DecodeConfig::default();
    let chunk_count = audio_core::count_chunks_in_wav(path, &config).unwrap_or(1);

    let progress = audio_core::DecodeProgress::new(chunk_count);
    let dispatch = crate::decode::wav_dispatch_decision(single_pass, Some(chunk_count), chunk_count);

    let progress_handle = if matches!(dispatch, crate::decode::WavDispatch::Chunked(_)) {
        let completed_ref = progress.chunks_completed.clone();
        let total_chunks = chunk_count;
        Some(std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            loop {
                let done = completed_ref.load(Ordering::Relaxed);
                let pct = (done.checked_mul(100))
                    .and_then(|v| v.checked_div(total_chunks))
                    .unwrap_or(100);
                eprint!("\rDecoding: {:3}%  (chunk {}/{})", pct.min(100), done.min(total_chunks), total_chunks);
                if done >= total_chunks || total_chunks == 0 || std::time::Instant::now() >= deadline { break; }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }))
    } else {
        None
    };

    let outcome = crate::decode::decode_wav_core(
        path,
        crate::decode::WavDecodeParams {
            use_libltc,
            single_pass,
            decode_fps: fps,
            decode_drop_frame: drop_frame,
        },
        Some(chunk_count),
        None,
        Some(&progress),
    )
    .map_err(|e| e.to_string())?;

    if let Some(progress_handle) = progress_handle {
        let _ = progress_handle.join();
        eprintln!("\rDecoding: 100%  (chunk {}/{})", chunk_count, chunk_count);
    }
    Ok(outcome.result)
}

/// Render the decode-results header block (everything up to and including
/// the `details` lines). Byte-for-byte what `print_decode_results` prints.
fn print_decode_summary(
    path: &Path,
    result: &audio_core::LtcDetectionResult,
    use_libltc: bool,
) -> String {
    let mut out = String::new();
    out.push_str("\n=== LTC Decode Results ===\n");
    out.push_str(&format!("  File:          {}\n", path.display()));
    out.push_str(&format!("  Decoder:       {}\n",
        if use_libltc { "libltc (C library)" } else { "builtin (Rust)" }));
    out.push_str(&format!("  Status:        {:?}\n", result.status));
    out.push_str(&format!("  Sample rate:   {} Hz\n", result.sample_rate));
    out.push_str(&format!("  Duration:      {:.3}s\n", result.total_audio_duration_secs));
    out.push_str(&format!("  FPS:           {:.2}{}\n", result.detected_fps,
        if result.drop_frame { " DF" } else { "" }));
    out.push_str(&format!("  Valid frames:  {} / {} ({:.1}%)\n",
        result.valid_frames, result.total_possible_frames, result.avg_confidence * 100.0));
    out.push_str(&format!("  First TC at:   {:.3}s\n", result.first_ltc_timecode_secs));
    out.push_str(&format!("  Processing:    {:.1}ms\n", result.processing_time_ms));

    if !result.timecodes.is_empty() {
        let first = &result.timecodes[0];
        let last = &result.timecodes[result.timecodes.len() - 1];
        out.push_str(&format!("  First TC:      {:02}:{:02}:{:02}:{:02}\n",
            first.timecode.hours, first.timecode.minutes,
            first.timecode.seconds, first.timecode.frames));
        out.push_str(&format!("  Last TC:       {:02}:{:02}:{:02}:{:02}\n",
            last.timecode.hours, last.timecode.minutes,
            last.timecode.seconds, last.timecode.frames));
    }

    for detail in &result.details {
        out.push_str(&format!("  {}\n", detail));
    }
    out
}

/// Render the `=== LTC Quality ===` block (without the preceding blank line).
fn print_quality_block(q: &audio_core::LtcQualityReport) -> String {
    let mut out = String::new();
    out.push_str("=== LTC Quality ===\n");
    out.push_str(&format!("  Score:        {:.2} / 1.00 ({})\n", q.score, q.grade));
    out.push_str(&format!("  Usable:       {:.1}% ({} block(s), {} backward jump(s))\n",
        q.usable_coverage * 100.0, q.block_count, q.backward_jump_count));
    out.push_str(&format!("  Frames:       {} missing, {} largest block\n",
        q.missing_frames, q.largest_block));
    out.push_str(&format!("  Contiguity:   {} gap(s), {} glitch(es), {} edit point(s)\n",
        q.gap_count, q.glitch_count, q.edit_count));
    out.push_str(&format!("  Sync drift:   max {:.3}s ({:.2} frames), rate {:.4} s/s\n",
        q.max_drift_secs, q.worst_block_drift_frames, q.drift_rate));
    out.push_str(&format!("  Summary:      {}\n", q.summary));
    out
}

/// Map a frame timecode to its audio-time seconds at the given fps
/// (the frames field contributes `frames / fps` seconds).
fn frame_timecode_to_secs(ft: &audio_core::FrameTimecode, fps: f64) -> f64 {
    ft.timecode.hours as f64 * 3600.0
        + ft.timecode.minutes as f64 * 60.0
        + ft.timecode.seconds as f64
        + ft.timecode.frames as f64 / fps
}

/// One `"[   12] 01:02:03:04  (3723.167s)"` frame-context line.
fn render_frame_context_line(ft: &audio_core::FrameTimecode) -> String {
    format!("[{:4}] {:02}:{:02}:{:02}:{:02}  ({:.3}s)", ft.frame_index,
        ft.timecode.hours, ft.timecode.minutes, ft.timecode.seconds, ft.timecode.frames, ft.timecode_secs)
}

/// Render the verbose gap sections (`--- Gap N ---` with pre/post context).
/// Empty when the quality report reports no gaps.
fn render_gap_report(
    q: &audio_core::LtcQualityReport,
    tc: &[audio_core::FrameTimecode],
    fps: f64,
    ctx: usize,
) -> String {
    let mut out = String::new();
    for (gi, &(prev_last, next_first)) in q.gap_edges.iter().enumerate() {
        out.push_str(&format!("\n--- Gap {} ---\n", gi + 1));
        let pre_start = prev_last.saturating_sub(ctx) + 1;
        for ft in &tc[pre_start..=prev_last] {
            out.push_str(&format!("  {}\n", render_frame_context_line(ft)));
        }
        let missing = ((frame_timecode_to_secs(&tc[next_first], fps)
            - frame_timecode_to_secs(&tc[prev_last], fps)
            - 1.0 / fps) / (1.0 / fps)).round() as u32;
        out.push_str(&format!("  ---- GAP ({} missing frame(s)) ----\n", missing));
        let post_end = (next_first + ctx).min(tc.len());
        for ft in &tc[next_first..post_end] {
            out.push_str(&format!("  {}\n", render_frame_context_line(ft)));
        }
    }
    out
}

/// Render the verbose glitch sections (`--- Glitch N ---` with pre/post
/// context and the `<<< GLITCH` marker). Empty when there are no glitches.
fn render_glitch_report(
    q: &audio_core::LtcQualityReport,
    tc: &[audio_core::FrameTimecode],
    ctx: usize,
) -> String {
    let mut out = String::new();
    for (gi, &idx) in q.glitch_indices.iter().enumerate() {
        out.push_str(&format!("\n--- Glitch {} ---\n", gi + 1));
        let pre_start = idx.saturating_sub(ctx);
        for ft in &tc[pre_start..idx] {
            out.push_str(&format!("  {}\n", render_frame_context_line(ft)));
        }
        let ft = &tc[idx];
        out.push_str(&format!("  {}  <<< GLITCH\n", render_frame_context_line(ft)));
        let post_end = (idx + 1 + ctx).min(tc.len());
        for ft in &tc[(idx + 1)..post_end] {
            out.push_str(&format!("  {}\n", render_frame_context_line(ft)));
        }
    }
    out
}

/// Render the verbose quality report (gaps + glitches). Empty when the
/// result has no quality report or reports no issues.
fn render_verbose_quality(
    result: &audio_core::LtcDetectionResult,
    context_frames: u32,
) -> String {
    let Some(ref q) = result.quality else { return String::new() };
    let has_issues = !q.gap_edges.is_empty() || !q.glitch_indices.is_empty();
    if !has_issues {
        return String::new();
    }
    let ctx = context_frames as usize;
    let tc = &result.timecodes;
    let fps = result.detected_fps as f64;
    let mut out = String::from("\n=== Verbose Quality Report ===\n");
    out.push_str(&render_gap_report(q, tc, fps, ctx));
    out.push_str(&render_glitch_report(q, tc, ctx));
    out
}

/// Render the `=== Decoded Timecodes ===` list (with leading blank line).
fn print_timecode_list(result: &audio_core::LtcDetectionResult) -> String {
    let mut out = String::from("\n=== Decoded Timecodes ===\n");
    for ft in &result.timecodes {
        let sep = if result.drop_frame { ";" } else { ":" };
        out.push_str(&format!("  [{:4}] {:02}{sep}{:02}{sep}{:02}{sep}{:02}  ({:.3}s)\n",
            ft.frame_index, ft.timecode.hours, ft.timecode.minutes,
            ft.timecode.seconds, ft.timecode.frames, ft.timecode_secs));
    }
    out
}

/// Print a rendered block verbatim (each renderer's output is a sequence of
/// full lines; `println!` re-adds the final newline).
fn print_block(block: &str) {
    if block.is_empty() {
        return;
    }
    print!("{}", block);
    if !block.ends_with('\n') {
        println!();
    }
}

fn print_decode_results(
    path: &Path,
    result: &audio_core::LtcDetectionResult,
    use_libltc: bool,
    verbose: bool,
    context_frames: u32,
    list_timecodes: bool,
) {
    print_block(&print_decode_summary(path, result, use_libltc));

    if let Some(ref q) = result.quality {
        println!();
        print_block(&print_quality_block(q));
    }

    if verbose {
        print_block(&render_verbose_quality(result, context_frames));
    }

    if list_timecodes {
        print_block(&print_timecode_list(result));
    }
    println!();
}

/// Run the WAV decode; returns the detection result for callers that want
/// to assert on it (printing happens in `process_cli_result`).
fn run_decode(cli: Cli) -> Result<audio_core::LtcDetectionResult, CliError> {
    let path = cli.decode.as_ref().ok_or_else(|| CliError::Decode("--decode path required".to_string()))?;
    let path = PathBuf::from(path);
    let use_libltc = cli.decoder == "libltc";

    if cli.debug { init_logger(); }

    info!("Decoding LTC from '{}' with {} decoder at {:.2} fps{}",
        path.display(), if use_libltc { "libltc" } else { "builtin" },
        cli.decode_fps, if cli.decode_drop_frame { " DF" } else { "" });

    run_decode_on_wav(&path, use_libltc, cli.single_pass, cli.decode_fps, cli.decode_drop_frame)
        .map_err(CliError::Decode)
}

// ── Outcome enum + dispatch ─────────────────────────────────────────────

pub enum CliOutcome {
    /// App should exit (headless, WAV, list-devices, or error)
    Done,
    /// App should start GUI with this engine handle
    RunGui {
        cmd_tx: std::sync::mpsc::Sender<GuiCommand>,
        state: Arc<arc_swap::ArcSwap<AppStateSnapshot>>,
        event_rx: std::sync::mpsc::Receiver<audio_core::AudioEvent>,
    },
}

/// Errors from the testable [`process_cli_result`] dispatch. The legacy
/// [`process_cli`] entry point renders these to stderr and exits(1) —
/// keeping its user-visible behavior byte-identical.
#[derive(Debug, Clone, PartialEq)]
pub enum CliError {
    /// `--decode <PATH>` failed (WAV or video branch).
    Decode(String),
    /// `--output-to-file` WAV generation failed.
    Generate(String),
    /// `--list-devices` audio-device enumeration failed.
    ///
    /// Display renders the legacy stderr prose `Error listing audio
    /// devices: {msg}`. Accepted deviation from the legacy path (Decision D3
    /// of the phase-toggles plan): the old `list_devices_and_exit` printed the
    /// bare prose; routing through [`process_cli`]'s unified renderer prefixes
    /// `Error: `. The line has no consumers asserting on it, so the
    /// byte-identical rule (R4) does not apply.
    ListDevices(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Decode(msg) => write!(f, "{msg}"),
            CliError::Generate(msg) => write!(f, "{msg}"),
            CliError::ListDevices(msg) => write!(f, "Error listing audio devices: {msg}"),
        }
    }
}

fn run_decode_video(cli: Cli) -> Result<audio_core::LtcDetectionResult, CliError> {
    let pipeline_start = Instant::now();

    let path = cli.decode.as_ref().ok_or_else(|| CliError::Decode("--decode path required".to_string()))?;
    let path = PathBuf::from(path);

    let use_libltc = cli.decoder == "libltc";

    if cli.debug {
        init_logger();
    }

    info!(
        "Decoding LTC from video '{}' (stream={}, channel={}) with {} decoder at {:.2} fps{}",
        path.display(),
        cli.audio_stream,
        cli.audio_channel,
        if use_libltc { "libltc" } else { "builtin" },
        cli.decode_fps,
        if cli.decode_drop_frame { " DF" } else { "" },
    );

    eprint!("Extracting audio from video...");
    let mut result = crate::decode::decode_video_file(
        &path,
        cli.audio_stream,
        cli.audio_channel,
        use_libltc,
        cli.decode_fps,
        cli.decode_drop_frame,
        None,
    )
    .map_err(|e| CliError::Decode(e.to_string()))?;
    eprintln!(" done.");

    result.processing_time_ms = pipeline_start.elapsed().as_secs_f64() * 1000.0;
    Ok(result)
}

/// Testable dispatch: identical decision order to [`process_cli`], but
/// decode/generate failures come back as `Err(CliError)` instead of killing
/// the process, and the decodable runners return their
/// [`audio_core::LtcDetectionResult`] (printing happens here, in the Done arm, so output
/// is unchanged). The `list_devices` arm is no longer process-bound: its
/// output comes from the pure [`list_device_lines`] formatter (unit-tested)
/// and errors return `Err(CliError::ListDevices)`; only the device
/// enumeration itself stays environment-bound. The `headless` arm remains
/// process-bound (real audio + infinite loop) — untestable by
/// design; the `RunGui` arm spawns the real engine and is covered
/// indirectly by the engine integration tests.
pub fn process_cli_result(cli: Cli) -> Result<CliOutcome, CliError> {
    if cli.list_devices {
        run_list_devices().map_err(CliError::ListDevices)?;
        return Ok(CliOutcome::Done);
    }

    if cli.probe_caps {
        let (caps, timings) = crate::converter::query_ffmpeg_capabilities_timed();
        for (key, value) in crate::converter::probe_caps_report(&caps, &timings) {
            println!("{}={}", key, value);
        }
        return Ok(CliOutcome::Done);
    }

    if let Some(path) = cli.decode.clone() {
        let path = PathBuf::from(path);
        // Copy the display flags out before `cli` is moved into a runner.
        let (verbose, context_frames, list_timecodes, use_libltc) =
            (cli.verbose, cli.context_frames, cli.list_timecodes, cli.decoder == "libltc");

        let result = if ffprobe::path_is_video(&path) {
            // Video file: use ffmpeg extraction
            run_decode_video(cli)?
        } else {
            // WAV file (or unknown): try WAV decode
            run_decode(cli)?
        };
        print_decode_results(
            &path, &result, use_libltc,
            verbose, context_frames, list_timecodes,
        );
        return Ok(CliOutcome::Done);
    }

    if let Some(_path) = &cli.output_to_file {
        generate_wav(cli).map_err(|e| CliError::Generate(e.to_string()))?;
        return Ok(CliOutcome::Done);
    }

    if cli.headless {
        // Process-bound by design (real audio + infinite loop).
        if let Err(e) = run_headless(cli) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return Ok(CliOutcome::Done);
    }

    Ok(spawn_gui_engine(cli))
}

/// Spawn the engine thread and return the GUI handle (the `RunGui` arm).
fn spawn_gui_engine(cli: Cli) -> CliOutcome {
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let use_libltc = cli.decoder == "libltc";
    let init_state = AppStateSnapshot::initial();
    let state = Arc::new(arc_swap::ArcSwap::new(Arc::new(init_state)));
    let state_clone = Arc::clone(&state);
    let (event_tx, event_rx) = std::sync::mpsc::channel::<audio_core::AudioEvent>();

    std::thread::Builder::new()
        .name("gui-engine".to_string())
        .spawn(move || {
            crate::engine::engine_main(cmd_rx, state_clone, use_libltc, event_tx);
        })
        .expect("failed to spawn gui-engine thread");

    if cli.autostart {
        let _ = cmd_tx.send(GuiCommand::StartLtc);
    }

    CliOutcome::RunGui {
        cmd_tx,
        state,
        event_rx,
    }
}

pub fn process_cli(cli: Cli) -> CliOutcome {
    match process_cli_result(cli) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audio_core::AudioDeviceInfo;

    // ── parse_timecode ────────────────────────────────────────────────────

    #[test]
    fn test_parse_timecode_valid() {
        let tc = parse_timecode("01:02:03:04").unwrap();
        assert_eq!(tc.hours, 1);
        assert_eq!(tc.minutes, 2);
        assert_eq!(tc.seconds, 3);
        assert_eq!(tc.frames, 4);
    }

    #[test]
    fn test_parse_timecode_zero() {
        let tc = parse_timecode("00:00:00:00").unwrap();
        assert_eq!(tc.hours, 0);
        assert_eq!(tc.minutes, 0);
        assert_eq!(tc.seconds, 0);
        assert_eq!(tc.frames, 0);
    }

    #[test]
    fn test_parse_timecode_max_valid() {
        let tc = parse_timecode("23:59:59:29").unwrap();
        assert_eq!(tc.hours, 23);
        assert_eq!(tc.minutes, 59);
        assert_eq!(tc.seconds, 59);
        assert_eq!(tc.frames, 29);
    }

    #[test]
    fn test_parse_timecode_wrong_segment_count() {
        let err = parse_timecode("01:02:03").unwrap_err();
        assert!(
            matches!(err, TimecodeParseError::BadFormat { .. }),
            "expected BadFormat, got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_too_many_segments() {
        let err = parse_timecode("01:02:03:04:05").unwrap_err();
        assert!(
            matches!(err, TimecodeParseError::BadFormat { .. }),
            "expected BadFormat, got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_non_numeric_hours() {
        let err = parse_timecode("ab:00:00:00").unwrap_err();
        assert!(
            matches!(
                err,
                TimecodeParseError::InvalidComponent { field: TcField::Hours, .. }
            ),
            "expected InvalidComponent(Hours), got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_non_numeric_frames() {
        let err = parse_timecode("00:00:00:xx").unwrap_err();
        assert!(
            matches!(
                err,
                TimecodeParseError::InvalidComponent { field: TcField::Frames, .. }
            ),
            "expected InvalidComponent(Frames), got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_hours_overflow() {
        let err = parse_timecode("24:00:00:00").unwrap_err();
        assert!(
            matches!(err, TimecodeParseError::OutOfRange(TcField::Hours)),
            "expected OutOfRange(Hours), got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_minutes_overflow() {
        let err = parse_timecode("00:60:00:00").unwrap_err();
        assert!(
            matches!(err, TimecodeParseError::OutOfRange(TcField::Minutes)),
            "expected OutOfRange(Minutes), got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_seconds_overflow() {
        let err = parse_timecode("00:00:60:00").unwrap_err();
        assert!(
            matches!(err, TimecodeParseError::OutOfRange(TcField::Seconds)),
            "expected OutOfRange(Seconds), got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_empty_string() {
        let err = parse_timecode("").unwrap_err();
        assert!(
            matches!(err, TimecodeParseError::BadFormat { .. }),
            "expected BadFormat, got {:?}",
            err
        );
    }

    #[test]
    fn test_parse_timecode_negative_hours() {
        let err = parse_timecode("-1:00:00:00").unwrap_err();
        assert!(
            matches!(
                err,
                TimecodeParseError::InvalidComponent { field: TcField::Hours, .. }
            ),
            "expected InvalidComponent(Hours), got {:?}",
            err
        );
    }

    // ── timecode_fmt ──────────────────────────────────────────────────────

    #[test]
    fn test_timecode_fmt_zero() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        assert_eq!(timecode_fmt(&tc), "00:00:00:00");
    }

    #[test]
    fn test_timecode_fmt_typical() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        assert_eq!(timecode_fmt(&tc), "01:02:03:04");
    }

    #[test]
    fn test_timecode_fmt_max() {
        let tc = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 29 };
        assert_eq!(timecode_fmt(&tc), "23:59:59:29");
    }

    #[test]
    fn test_timecode_fmt_width() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        assert_eq!(timecode_fmt(&tc).len(), 11);
    }

    // ── autostart ─────────────────────────────────────────────────────────

    #[test]
    fn test_autostart_flag_parsed() {
        let cli = Cli::try_parse_from(["test", "--autostart"]).unwrap();
        assert!(cli.autostart, "--autostart should set autostart=true");
    }

    #[test]
    fn test_autostart_default_false() {
        let cli = Cli::try_parse_from(["test"]).unwrap();
        assert!(!cli.autostart, "default autostart should be false");
    }

    // ── resolve_device ────────────────────────────────────────────────────

    fn make_devices() -> Vec<AudioDeviceInfo> {
        vec![
            AudioDeviceInfo {
                id: "alsa_output.pci-0000_00_1f.3.analog-stereo".into(),
                name: "Built-in Audio (Default)".into(),
                is_default: true,
                formats: vec!["f32".into(), "i16".into()],
                channels_min: 2,
                channels_max: 2,
                sample_rate_min: 44100,
                sample_rate_max: 192000,
                buffer_min: 256,
                buffer_max: 8192,
            },
            AudioDeviceInfo {
                id: "alsa_output.usb-Behringer_UMC204HD-00.analog-stereo".into(),
                name: "UMC204HD".into(),
                is_default: false,
                formats: vec!["f32".into(), "i16".into()],
                channels_min: 2,
                channels_max: 2,
                sample_rate_min: 44100,
                sample_rate_max: 96000,
                buffer_min: 64,
                buffer_max: 2048,
            },
        ]
    }

    fn default_resolve_cli() -> Cli {
        Cli {
            device: None, device_index: None,
            list_devices: false, headless: false,
            start_timecode: "01:00:00:00".into(),
            fps: 25.0, drop_frame: false,
            channel: "left".into(), volume: 0.25,
            sample_rate: None, duration: None,
            output_to_file: None, verbose: false, debug: false,
            decode: None, audio_stream: 0, audio_channel: 0,
            decoder: "builtin".into(),
            decode_fps: 25.0, decode_drop_frame: false,
            single_pass: false,
            context_frames: 3,
            list_timecodes: false,
            autostart: false,
        probe_caps: false,
        }
    }

    #[test]
    fn test_resolve_device_default_fallback() {
        let devs = make_devices();
        let cli = default_resolve_cli();
        let id = resolve_device(&devs, &cli).unwrap();
        assert_eq!(id, "alsa_output.pci-0000_00_1f.3.analog-stereo");
    }

    #[test]
    fn test_resolve_device_by_index() {
        let devs = make_devices();
        let mut cli = default_resolve_cli();
        cli.device_index = Some(1);
        let id = resolve_device(&devs, &cli).unwrap();
        assert_eq!(id, "alsa_output.usb-Behringer_UMC204HD-00.analog-stereo");
    }

    #[test]
    fn test_resolve_device_by_name_exact() {
        let devs = make_devices();
        let mut cli = default_resolve_cli();
        cli.device = Some("UMC204HD".into());
        let id = resolve_device(&devs, &cli).unwrap();
        assert_eq!(id, "alsa_output.usb-Behringer_UMC204HD-00.analog-stereo");
    }

    #[test]
    fn test_resolve_device_by_id_exact() {
        let devs = make_devices();
        let id_str = "alsa_output.pci-0000_00_1f.3.analog-stereo";
        let mut cli = default_resolve_cli();
        cli.device = Some(id_str.into());
        let id = resolve_device(&devs, &cli).unwrap();
        assert_eq!(id, id_str);
    }

    #[test]
    fn test_resolve_device_by_substring() {
        let devs = make_devices();
        let mut cli = default_resolve_cli();
        cli.device = Some("UMC204".into());
        let id = resolve_device(&devs, &cli).unwrap();
        assert!(id.contains("UMC204HD"), "expected UMC204HD, got {}", id);
    }

    #[test]
    fn test_resolve_device_index_out_of_range() {
        let devs = make_devices();
        let mut cli = default_resolve_cli();
        cli.device_index = Some(99);
        let err = resolve_device(&devs, &cli).unwrap_err();
        assert!(
            matches!(
                err,
                ResolveDeviceError::IndexOutOfRange { index: 99, count: 2 }
            ),
            "expected IndexOutOfRange {{ index: 99, count: 2 }}, got {:?}",
            err
        );
    }

    #[test]
    fn test_resolve_device_name_not_found() {
        let devs = make_devices();
        let mut cli = default_resolve_cli();
        cli.device = Some("NonExistentDevice".into());
        let err = resolve_device(&devs, &cli).unwrap_err();
        assert!(
            matches!(err, ResolveDeviceError::NotFound { .. }),
            "expected NotFound, got {:?}",
            err
        );
    }

    // ── list_device_lines ─────────────────────────────────────────────────

    #[test]
    fn test_list_device_lines_header_and_entries() {
        let lines = list_device_lines(&make_devices());
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[0], "Available audio output devices:");
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[1], "  [ 0] Built-in Audio (Default) (default)");
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[3], "  [ 1] UMC204HD");
    }

    #[test]
    fn test_list_device_lines_detail_line() {
        let lines = list_device_lines(&make_devices());
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            lines[2],
            "         channels: 2-2, sample rate: 44100-192000 Hz, formats: f32, i16"
        );
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            lines[4],
            "         channels: 2-2, sample rate: 44100-96000 Hz, formats: f32, i16"
        );
    }

    #[test]
    fn test_list_device_lines_index_width() {
        let mut devs = make_devices();
        for i in 2..11 {
            devs.push(audio_core::AudioDeviceInfo {
                id: format!("dev{i}"),
                name: format!("Device {i}"),
                is_default: false,
                formats: vec!["f32".into()],
                channels_min: 2,
                channels_max: 2,
                sample_rate_min: 44100,
                sample_rate_max: 48000,
                buffer_min: 64,
                buffer_max: 2048,
            });
        }
        let lines = list_device_lines(&devs);
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[21], "  [10] Device 10");
    }

    #[test]
    fn test_list_device_lines_empty_list() {
        let lines = list_device_lines(&[]);
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            lines,
            vec![
                "Available audio output devices:".to_string(),
                "  (no output devices found)".to_string(),
            ]
        );
    }

    #[test]
    fn test_resolve_device_empty_device_list() {
        let devs: Vec<AudioDeviceInfo> = vec![];
        let cli = default_resolve_cli();
        let err = resolve_device(&devs, &cli).unwrap_err();
        assert!(
            matches!(err, ResolveDeviceError::NoDevices),
            "expected NoDevices, got {:?}",
            err
        );
    }

    #[test]
    fn test_resolve_device_first_when_no_default() {
        let devs = vec![
            AudioDeviceInfo {
                id: "dev1".into(), name: "Device 1".into(), is_default: false,
                formats: vec![], channels_min: 0, channels_max: 0,
                sample_rate_min: 0, sample_rate_max: 0, buffer_min: 0, buffer_max: 0,
            },
        ];
        let cli = default_resolve_cli();
        let id = resolve_device(&devs, &cli).unwrap();
        assert_eq!(id, "dev1");
    }

    // ── decode-report renderers (characterization) ────────────────────────

    fn frame(idx: u32, h: u32, m: u32, s: u32, f: u32, secs: f64) -> audio_core::FrameTimecode {
        audio_core::FrameTimecode {
            frame_index: idx,
            timecode: audio_core::Timecode { hours: h, minutes: m, seconds: s, frames: f },
            timecode_secs: secs,
        }
    }

    fn quality(gap_edges: Vec<(usize, usize)>, glitch_indices: Vec<usize>) -> audio_core::LtcQualityReport {
        audio_core::LtcQualityReport {
            score: 0.87,
            grade: audio_core::ltc_decoder::QualityGrade::Good,
            missing_frames: 3,
            gap_count: 1,
            glitch_count: 1,
            edit_count: 0,
            max_drift_secs: 0.012,
            drift_rate: 0.0004,
            largest_block: 50,
            usable_coverage: 0.95,
            block_count: 2,
            worst_block_drift_frames: 0.4,
            backward_jump_count: 0,
            gap_edges,
            glitch_indices,
            summary: "decent".to_string(),
        }
    }

    fn detection(tc: Vec<audio_core::FrameTimecode>, q: Option<audio_core::LtcQualityReport>) -> audio_core::LtcDetectionResult {
        audio_core::LtcDetectionResult {
            status: audio_core::LtcDecodeStatus::Success,
            detected_fps: 25.0,
            drop_frame: false,
            total_possible_frames: 100,
            valid_frames: 97,
            timecodes: tc,
            avg_confidence: 0.97,
            details: vec!["chunk 0: 97/100 frames".to_string()],
            total_audio_duration_secs: 4.0,
            sample_rate: 48_000,
            processing_time_ms: 12.34,
            first_ltc_timecode_secs: 0.04,
            quality: q,
            chunk_summaries: Vec::new(),
        }
    }

    #[test]
    fn test_frame_timecode_to_secs_frames_contribute_fraction() {
        // 01:00:02:12 at 25 fps → 3600 + 2 + 12/25
        let ft = frame(0, 1, 0, 2, 12, 3602.48);
        let secs = frame_timecode_to_secs(&ft, 25.0);
        assert!((secs - 3602.48).abs() < 1e-9, "got {}", secs);
    }

    // test-lint: allow(text-pin): the rendered report lines are the formatter
    // output — their exact text is the contract under characterization.
    #[test]
    fn test_render_frame_context_line_format() {
        let ft = frame(12, 1, 2, 3, 4, 3723.16);
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(render_frame_context_line(&ft), "[  12] 01:02:03:04  (3723.160s)");
    }

    #[test]
    fn test_print_decode_summary_block() {
        let result = detection(vec![
            frame(0, 1, 0, 0, 0, 0.0),
            frame(1, 1, 0, 0, 1, 0.04),
        ], None);
        let out = print_decode_summary(Path::new("/tmp/x.wav"), &result, true);
        let lines: Vec<&str> = out.lines().collect();
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "=== LTC Decode Results ===");
        assert_eq!(lines[2], "  File:          /tmp/x.wav");
        assert_eq!(lines[3], "  Decoder:       libltc (C library)");
        assert!(lines.contains(&"  Status:        Success"));
        assert!(lines.contains(&"  Sample rate:   48000 Hz"));
        assert!(lines.contains(&"  Duration:      4.000s"));
        assert!(lines.contains(&"  FPS:           25.00"));
        assert!(lines.contains(&"  Valid frames:  97 / 100 (97.0%)"));
        assert!(lines.contains(&"  First TC:      01:00:00:00"));
        assert!(lines.contains(&"  Last TC:       01:00:00:01"));
        assert!(lines.contains(&"  chunk 0: 97/100 frames"));
        // detail line is indented
        assert_eq!(lines.last().copied(), Some("  chunk 0: 97/100 frames"));
    }

    #[test]
    fn test_print_decode_summary_builtin_and_df_marker() {
        let mut result = detection(vec![], None);
        result.drop_frame = true;
        let out = print_decode_summary(Path::new("y.wav"), &result, false);
        // test-lint: allow(text-pin): formatter output is the contract
        assert!(out.contains("  Decoder:       builtin (Rust)"));
        assert!(out.contains("  FPS:           25.00 DF"));
        // empty timecode list → no First/Last TC lines
        assert!(!out.contains("First TC:"));
    }

    #[test]
    fn test_print_quality_block_lines() {
        let q = quality(vec![], vec![]);
        let out = print_quality_block(&q);
        let lines: Vec<&str> = out.lines().collect();
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[0], "=== LTC Quality ===");
        assert!(lines.contains(&"  Score:        0.87 / 1.00 (Good)"));
        assert!(lines.contains(&"  Usable:       95.0% (2 block(s), 0 backward jump(s))"));
        assert!(lines.contains(&"  Frames:       3 missing, 50 largest block"));
        assert!(lines.contains(&"  Contiguity:   1 gap(s), 1 glitch(es), 0 edit point(s)"));
        assert!(lines.contains(&"  Sync drift:   max 0.012s (0.40 frames), rate 0.0004 s/s"));
        assert!(lines.contains(&"  Summary:      decent"));
    }

    #[test]
    fn test_render_gap_report_context_and_missing_count() {
        // 4 frames at 25 fps; gap between index 1 (last of block) and 2..
        // Build: frames 0,1 then a one-frame hole represented by jumping
        // timecode; gap edge (1, 2) with 2 missing frames computed from secs.
        let tc = vec![
            frame(0, 0, 0, 0, 0, 0.0),
            frame(1, 0, 0, 0, 1, 0.04),
            frame(2, 0, 0, 0, 4, 0.16),
            frame(3, 0, 0, 0, 5, 0.20),
        ];
        let q = quality(vec![(1, 2)], vec![]);
        let out = render_gap_report(&q, &tc, 25.0, 1);
        let lines: Vec<&str> = out.lines().collect();
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "--- Gap 1 ---");
        // pre-context: only frame 1 (ctx=1, pre_start = 1-1+1 = 1)
        assert_eq!(lines[2], "  [   1] 00:00:00:01  (0.040s)");
        // missing = round((0.16 - 0.04 - 0.04) / 0.04) = 2
        assert_eq!(lines[3], "  ---- GAP (2 missing frame(s)) ----");
        // post-context: frame 2 only (ctx=1)
        assert_eq!(lines[4], "  [   2] 00:00:00:04  (0.160s)");
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn test_render_glitch_report_marks_glitch_frame() {
        let tc = vec![
            frame(0, 0, 0, 0, 0, 0.0),
            frame(1, 0, 0, 0, 9, 0.36),
            frame(2, 0, 0, 0, 2, 0.08),
        ];
        let q = quality(vec![], vec![1]);
        let out = render_glitch_report(&q, &tc, 1);
        let lines: Vec<&str> = out.lines().collect();
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "--- Glitch 1 ---");
        assert_eq!(lines[2], "  [   0] 00:00:00:00  (0.000s)");
        assert_eq!(lines[3], "  [   1] 00:00:00:09  (0.360s)  <<< GLITCH");
        assert_eq!(lines[4], "  [   2] 00:00:00:02  (0.080s)");
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn test_render_verbose_quality_empty_without_issues() {
        let result = detection(vec![], Some(quality(vec![], vec![])));
        assert!(render_verbose_quality(&result, 2).is_empty());
        let no_quality = detection(vec![], None);
        assert!(render_verbose_quality(&no_quality, 2).is_empty());
    }

    #[test]
    fn test_render_verbose_quality_has_header_then_sections() {
        let tc = vec![
            frame(0, 0, 0, 0, 0, 0.0),
            frame(1, 0, 0, 0, 9, 0.36),
            frame(2, 0, 0, 0, 2, 0.08),
        ];
        let result = detection(tc, Some(quality(vec![], vec![1])));
        let out = render_verbose_quality(&result, 2);
        let lines: Vec<&str> = out.lines().collect();
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "=== Verbose Quality Report ===");
        assert!(out.contains("--- Glitch 1 ---"));
    }

    #[test]
    fn test_print_timecode_list_colon_and_semicolon_separators() {
        let mut result = detection(vec![
            frame(0, 1, 0, 0, 0, 0.0),
            frame(1, 1, 0, 0, 1, 0.04),
        ], None);
        let out = print_timecode_list(&result);
        let lines: Vec<&str> = out.lines().collect();
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(lines[0], "");
        assert_eq!(lines[1], "=== Decoded Timecodes ===");
        assert_eq!(lines[2], "  [   0] 01:00:00:00  (0.000s)");
        assert_eq!(lines[3], "  [   1] 01:00:00:01  (0.040s)");

        result.drop_frame = true;
        let out_df = print_timecode_list(&result);
        // test-lint: allow(text-pin): formatter output is the contract
        assert!(out_df.contains("01;00;00;00"));
        assert!(!out_df.contains("01:00:00:00"));
    }
}