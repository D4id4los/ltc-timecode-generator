use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use audio_core::{
    generate_ltc_frame_stereo, increment_timecode, list_audio_devices, AudioCore, Timecode,
};
use clap::Parser;
use log::{error, info, warn};

use crate::command::GuiCommand;
use crate::state::AppStateSnapshot;

// ── Logger ──────────────────────────────────────────────────────────────

fn init_logger() {
    let _ = env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or(
            "ltc_gui=trace,audio_core=trace,info",
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
    #[arg(long, default_value = "both")]
    pub channel: String,

    /// LTC volume (0.0 to 1.0)
    #[arg(long, default_value_t = 0.25)]
    pub volume: f32,

    /// Sample rate: 16000 or 48000 (default: auto-detect)
    #[arg(long)]
    pub sample_rate: Option<u32>,

    /// Duration in seconds (omit for indefinite playback)
    #[arg(long)]
    pub duration: Option<f64>,

    /// Write WAV file instead of playing (implies --headless)
    #[arg(long)]
    pub output_to_file: Option<String>,

    /// Print timecode progression to stdout
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Enable debug log output to stderr
    #[arg(long, short = 'd')]
    pub debug: bool,
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn timecode_fmt(tc: &Timecode) -> String {
    format!(
        "{:02}:{:02}:{:02}:{:02}",
        tc.hours, tc.minutes, tc.seconds, tc.frames
    )
}

fn parse_timecode(s: &str) -> Result<Timecode, String> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 4 {
        return Err(format!(
            "Invalid timecode format '{}' — expected HH:MM:SS:FF",
            s
        ));
    }
    let hours = parts[0]
        .parse::<u32>()
        .map_err(|_| format!("Invalid hours in '{}'", s))?;
    let minutes = parts[1]
        .parse::<u32>()
        .map_err(|_| format!("Invalid minutes in '{}'", s))?;
    let seconds = parts[2]
        .parse::<u32>()
        .map_err(|_| format!("Invalid seconds in '{}'", s))?;
    let frames = parts[3]
        .parse::<u32>()
        .map_err(|_| format!("Invalid frames in '{}'", s))?;
    if hours >= 24 {
        return Err("Hours must be 0-23".to_string());
    }
    if minutes >= 60 {
        return Err("Minutes must be 0-59".to_string());
    }
    if seconds >= 60 {
        return Err("Seconds must be 0-59".to_string());
    }
    Ok(Timecode {
        hours,
        minutes,
        seconds,
        frames,
    })
}

// ── Device listing ──────────────────────────────────────────────────────

pub fn list_devices_and_exit() -> ! {
    init_logger();
    match list_audio_devices() {
        Ok(devices) => {
            println!("Available audio output devices:");
            for (i, dev) in devices.iter().enumerate() {
                let default_mark = if dev.is_default { " (default)" } else { "" };
                println!("  [{:2}] {}{}", i, dev.name, default_mark);
                println!(
                    "         channels: {}-{}, sample rate: {}-{} Hz, formats: {}",
                    dev.channels_min,
                    dev.channels_max,
                    dev.sample_rate_min,
                    dev.sample_rate_max,
                    dev.formats.join(", ")
                );
            }
            if devices.is_empty() {
                println!("  (no output devices found)");
            }
        }
        Err(e) => {
            eprintln!("Error listing audio devices: {}", e);
            std::process::exit(1);
        }
    }
    std::process::exit(0);
}

fn resolve_device(
    devices: &[audio_core::AudioDeviceInfo],
    cli: &Cli,
) -> Result<String, String> {
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
        return Err(format!(
            "Device '{}' not found. Use --list-devices to see available devices.",
            id
        ));
    }

    if let Some(index) = cli.device_index {
        if index >= devices.len() {
            return Err(format!(
                "Device index {} out of range ({} devices). Use --list-devices to see available devices.",
                index,
                devices.len()
            ));
        }
        return Ok(devices[index].id.clone());
    }

    for dev in devices {
        if dev.is_default {
            return Ok(dev.id.clone());
        }
    }
    devices
        .first()
        .map(|d| d.id.clone())
        .ok_or_else(|| "No audio devices available".to_string())
}

// ── Headless mode ───────────────────────────────────────────────────────

pub fn run_headless(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if cli.debug {
        init_logger();
    }
    let start_tc = parse_timecode(&cli.start_timecode)?;
    let fps = cli.fps;
    let drop_frame = cli.drop_frame;
    let channel = cli.channel.clone();
    let volume = cli.volume;
    let verbose = cli.verbose;

    let valid_fps = [24.0, 25.0, 29.97, 30.0];
    if !valid_fps.iter().any(|f| (f - fps).abs() < 0.01) {
        return Err(
            format!("Unsupported fps: {}. Must be one of: 24, 25, 29.97, 30", fps).into(),
        );
    }

    let valid_channels = ["left", "right", "both"];
    if !valid_channels.iter().any(|c| *c == channel) {
        return Err(format!(
            "Unsupported channel: '{}'. Must be one of: left, right, both",
            channel
        )
        .into());
    }

    if !(0.0..=1.0).contains(&volume) {
        return Err("Volume must be between 0.0 and 1.0".to_string().into());
    }

    info!(
        "Headless mode: tc={}, fps={}, drop_frame={}, channel={}, volume={}",
        timecode_fmt(&start_tc),
        fps,
        drop_frame,
        channel,
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
    let channel = cli.channel.clone();
    let volume = cli.volume;

    let duration_secs = cli
        .duration
        .ok_or_else(|| "--duration is required for WAV file output")?;

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
            &channel,
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

// ── Outcome enum + dispatch ─────────────────────────────────────────────

pub enum CliOutcome {
    /// App should exit (headless, WAV, list-devices, or error)
    Done,
    /// App should start GUI with this engine handle
    RunGui {
        cmd_tx: std::sync::mpsc::Sender<GuiCommand>,
        state: Arc<arc_swap::ArcSwap<AppStateSnapshot>>,
    },
}

pub fn process_cli(cli: Cli) -> CliOutcome {
    if cli.list_devices {
        list_devices_and_exit();
    }

    if let Some(_path) = &cli.output_to_file {
        if let Err(e) = generate_wav(cli) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return CliOutcome::Done;
    }

    if cli.headless {
        if let Err(e) = run_headless(cli) {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        return CliOutcome::Done;
    }

    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel();
    let init_state = AppStateSnapshot::initial();
    let state = Arc::new(arc_swap::ArcSwap::new(Arc::new(init_state)));
    let state_clone = Arc::clone(&state);

    std::thread::Builder::new()
        .name("gui-engine".to_string())
        .spawn(move || {
            crate::engine::engine_main(cmd_rx, state_clone);
        })
        .expect("failed to spawn gui-engine thread");

    CliOutcome::RunGui {
        cmd_tx,
        state,
    }
}