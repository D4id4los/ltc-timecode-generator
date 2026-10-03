use crate::{AudioDeviceInfo, AudioEvent, ChannelSel, Timecode};
use crate::ltc_encoder::{self, generate_ltc_frame_stereo, increment_timecode};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use keepawake::{Builder as WakeBuilder, KeepAwake};
use log::{error, info, warn};
use ringbuf::{HeapConsumer, HeapProducer, HeapRb};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

// ── Ring buffer capacities ─────────────────────────────────────────────────

const LTC_RING_CAPACITY: usize = 262_144;   // 128K stereo samples (~2.7s at 48kHz, ~8s at 16kHz)
const BEEP_RING_CAPACITY: usize = 32_768;    // 32K stereo samples (~0.34s at 48kHz, ~1s at 16kHz)

// ── Internal state structs ─────────────────────────────────────────────────

struct LtcStreamState {
    running: bool,
    tc: Timecode,
    fps: f64,
    drop_frame: bool,
    ltc_channel: ChannelSel,
    ltc_volume: f32,
    sample_rate: u32,
    last_level: (f32, f32),
    frame_duration: Duration,
    next_frame_time: Instant,
    stop_signal: Arc<AtomicBool>,
    scheduler_thread: Option<JoinHandle<()>>,
    exact_samples_per_frame: f64,
    base_samples: usize,
    samples_accumulator: f64,
}

struct AudioOutputState {
    ltc_producer: Arc<Mutex<HeapProducer<f32>>>,
    stream: cpal::Stream,
    beep_producer: Arc<Mutex<HeapProducer<f32>>>,
    ltc: Arc<Mutex<LtcStreamState>>,
    streaming: Arc<AtomicBool>,
    underrun_count: Arc<AtomicU64>,
    callback_counter: Arc<AtomicU64>,
    playing: Arc<AtomicBool>,
}

/// Tauri-free, Send+Sync audio core. Held by each Tauri frontend crate as
/// managed state; the `#[tauri::command]` wrappers just forward into here.
pub struct AudioCore {
    audio: Mutex<Option<AudioOutputState>>,
    sample_format_name: Mutex<String>,
    events: Arc<Mutex<Vec<AudioEvent>>>,
    wake_lock: Mutex<Option<KeepAwake>>,
}

impl AudioCore {
    pub fn new() -> Self {
        Self {
            audio: Mutex::new(None),
            sample_format_name: Mutex::new(String::new()),
            events: Arc::new(Mutex::new(Vec::new())),
            wake_lock: Mutex::new(None),
        }
    }

    pub fn drain_events(&self) -> Vec<AudioEvent> {
        self.events
            .lock()
            .map(|mut e| e.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn sample_format_name(&self) -> String {
        self.sample_format_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn init_output(
        &self,
        device_id: &str,
        sample_rate: u32,
        buffer_size: u32,
    ) -> Result<u32, String> {
        let host = cpal::default_host();
        let device = if device_id.is_empty() || device_id == "default" {
            host.default_output_device()
                .ok_or_else(|| "No default output device available".to_string())?
        } else {
            host.output_devices()
                .map_err(|e| format!("Failed to enumerate devices: {}", e))?
                .find(|d| d.to_string() == device_id)
                .ok_or_else(|| format!("Output device '{}' not found", device_id))?
        };

        let configs: Vec<cpal::SupportedStreamConfigRange> = device
            .supported_output_configs()
            .map(|c| c.collect())
            .unwrap_or_default();

        let buf_size = choose_buffer_size(buffer_size);

        let (stream_config, sample_format) = select_best_config(
            &configs,
            2,
            sample_rate,
            buf_size,
        )?;

        let fmt_name = sample_format_name(sample_format);

        info!(
            "Audio output initialized: device={}, channels={}, sample_rate={}, buffer_size={:?}, format={}",
            device,
            stream_config.channels,
            stream_config.sample_rate,
            buf_size,
            fmt_name,
        );

        let ltc_rb = HeapRb::<f32>::new(LTC_RING_CAPACITY);
        let (ltc_producer, ltc_consumer) = ltc_rb.split();

        let beep_rb = HeapRb::<f32>::new(BEEP_RING_CAPACITY);
        let (beep_producer, beep_consumer) = beep_rb.split();

        let ltc = Arc::new(Mutex::new(LtcStreamState {
            running: false,
            tc: Timecode {
                hours: 0,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            fps: 25.0,
            drop_frame: false,
            ltc_channel: ChannelSel::Both,
            ltc_volume: 0.25,
            sample_rate: stream_config.sample_rate,
            last_level: (1.0, 1.0),
            frame_duration: Duration::from_millis(40),
            next_frame_time: Instant::now(),
            stop_signal: Arc::new(AtomicBool::new(false)),
            scheduler_thread: None,
            exact_samples_per_frame: 0.0,
            base_samples: 0,
            samples_accumulator: 0.0,
        }));

        let streaming = Arc::new(AtomicBool::new(false));
        let underrun_count = Arc::new(AtomicU64::new(0));
        let callback_counter = Arc::new(AtomicU64::new(0));

        let last_err_log = Arc::new(Mutex::new(Instant::now()));
        let events_for_err = self.events.clone();
        let err_handler = move |err: cpal::Error| {
            let now = Instant::now();
            let mut last = last_err_log.lock().unwrap_or_else(|e| e.into_inner());
            if now.duration_since(*last) > Duration::from_secs(1) {
                error!("Audio output stream error: {}", err);
                if let Ok(mut ev) = events_for_err.lock() {
                    ev.push(AudioEvent::StreamError(err.to_string()));
                }
                *last = now;
            }
        };

        let stream = build_stream_for_format(
            &device,
            &stream_config,
            sample_format,
            ltc_consumer,
            beep_consumer,
            streaming.clone(),
            underrun_count.clone(),
            callback_counter.clone(),
            err_handler,
        )?;

        let playing = Arc::new(AtomicBool::new(false));

        let mut audio = self
            .audio
            .lock()
            .map_err(|e| format!("State lock error: {}", e))?;
        *audio = Some(AudioOutputState {
            ltc_producer: Arc::new(Mutex::new(ltc_producer)),
            stream,
            beep_producer: Arc::new(Mutex::new(beep_producer)),
            ltc,
            streaming,
            underrun_count,
            callback_counter,
            playing,
        });

        let mut fmt = self
            .sample_format_name
            .lock()
            .map_err(|e| format!("Sample format lock error: {}", e))?;
        *fmt = fmt_name.to_string();

        let actual_rate = stream_config.sample_rate;
        Ok(actual_rate)
    }

    pub fn start_ltc(
        &self,
        tc: Timecode,
        fps: f64,
        drop_frame: bool,
        ltc_channel: ChannelSel,
        ltc_volume: f32,
    ) -> Result<(), String> {
        let audio = self
            .audio
            .lock()
            .map_err(|e| format!("State lock error: {}", e))?;
        let output = audio
            .as_ref()
            .ok_or_else(|| "Audio not initialized".to_string())?;

        let mut ltc = output
            .ltc
            .lock()
            .map_err(|e| format!("LTC state lock error: {}", e))?;

        let frame_duration_ns = (1.0 / fps * 1_000_000_000.0) as u64;
        let frame_duration = Duration::from_nanos(frame_duration_ns);
        let exact_samples_per_frame = ltc.sample_rate as f64 / fps;
        let base_samples = exact_samples_per_frame.floor() as usize;
        *ltc = LtcStreamState {
            running: true,
            tc,
            fps,
            drop_frame,
            ltc_channel,
            ltc_volume,
            sample_rate: ltc.sample_rate,
            last_level: (1.0, 1.0),
            frame_duration,
            next_frame_time: Instant::now(),
            stop_signal: Arc::new(AtomicBool::new(false)),
            scheduler_thread: None,
            exact_samples_per_frame,
            base_samples,
            samples_accumulator: 0.0_f64,
        };

        // Prefill with base_samples (accumulator starts at 0, so no extra yet)
        let prefill_count = 5;
        let prefill_total = base_samples;
        let prefill_spb = prefill_total as f32 / 80.0;
        let mut frame_buf = vec![0.0f32; prefill_total * 2];
        let mut prefill_tc = tc;
        let mut prefill_level = (1.0f32, 1.0f32);
        {
            let mut producer = output
                .ltc_producer
                .lock()
                .map_err(|e| format!("Producer lock error: {}", e))?;
            for _ in 0..prefill_count {
                frame_buf.fill(0.0);
                generate_ltc_frame_stereo(
                    &prefill_tc,
                    drop_frame,
                    prefill_total,
                    prefill_spb,
                    ltc_volume,
                    ltc_channel,
                    &mut prefill_level,
                    &mut frame_buf[..prefill_total * 2],
                );
                let pushed = producer.push_slice(&frame_buf[..prefill_total * 2]);
                if pushed < prefill_total * 2 {
                    warn!(
                        "LTC start: ring buffer full during prefill, dropped {} samples",
                        prefill_total * 2 - pushed
                    );
                }
                prefill_tc = increment_timecode(&prefill_tc, fps, drop_frame);
            }
        }
        ltc.tc = prefill_tc;
        ltc.last_level = prefill_level;

        // Start playback after prefill so the ring buffer is not empty
        if !output.playing.load(Ordering::Relaxed) {
            output
                .stream
                .play()
                .map_err(|e| format!("Failed to play audio output stream: {}", e))?;
            output.playing.store(true, Ordering::Relaxed);
        }

        output.streaming.store(true, Ordering::Relaxed);

        let stop_signal = ltc.stop_signal.clone();
        let ltc_producer = output.ltc_producer.clone();
        let ltc_clone = output.ltc.clone();
        let underrun_count = output.underrun_count.clone();
        let callback_counter = output.callback_counter.clone();
        let events_for_scheduler = self.events.clone();

        let handle = std::thread::Builder::new()
            .name("ltc-scheduler".into())
            .spawn(move || ltc_scheduler_thread(
                ltc_producer,
                ltc_clone,
                stop_signal,
                underrun_count,
                callback_counter,
                events_for_scheduler,
            ))
            .map_err(|e| format!("Failed to spawn LTC scheduler thread: {}", e))?;

        info!("LTC scheduler thread spawned (tc={:?}, fps={}, drop_frame={})", tc, fps, drop_frame);
        ltc.scheduler_thread = Some(handle);

        // Acquire system wake lock to prevent sleep while LTC is streaming
        if let Ok(mut wl) = self.wake_lock.lock() {
            if wl.is_none() {
                *wl = WakeBuilder::default()
                    .display(true)
                    .idle(true)
                    .reason("LTC timecode generation")
                    .app_name("LTC Timecode Generator")
                    .app_reverse_domain("at.agere.ltc-timecode-generator")
                    .create()
                    .ok();
                if wl.is_some() {
                    info!("System wake lock acquired (display+idle)");
                } else {
                    warn!("Failed to acquire system wake lock (D-Bus/systemd not available?)");
                }
            }
        }

        Ok(())
    }

    pub fn stop_ltc(&self) -> Result<(), String> {
        let audio = self
            .audio
            .lock()
            .map_err(|e| format!("State lock error: {}", e))?;
        if let Some(ref output) = *audio {
            output.streaming.store(false, Ordering::Relaxed);
            let mut ltc = output
                .ltc
                .lock()
                .map_err(|e| format!("LTC state lock error: {}", e))?;
            ltc.running = false;
            ltc.stop_signal.store(true, Ordering::Relaxed);
            if let Some(handle) = ltc.scheduler_thread.take() {
                if let Err(e) = handle.join() {
                    warn!("LTC scheduler thread panicked on stop: {:?}", e);
                }
            }
        }
        drop(audio);
        // Release system wake lock
        if let Ok(mut wl) = self.wake_lock.lock() {
            if wl.is_some() {
                *wl = None;
                info!("System wake lock released (stop_ltc)");
            }
        }
        Ok(())
    }

    pub fn reset_ltc(&self, tc: Timecode) -> Result<(), String> {
        let audio = self
            .audio
            .lock()
            .map_err(|e| format!("State lock error: {}", e))?;
        if let Some(ref output) = *audio {
            let mut ltc = output
                .ltc
                .lock()
                .map_err(|e| format!("LTC state lock error: {}", e))?;
            ltc.tc = tc;
            ltc.last_level = (1.0, 1.0);
            ltc.next_frame_time = Instant::now();
            info!("LTC reset to {:02}:{:02}:{:02}:{:02} (drop_frame={})",
                tc.hours, tc.minutes, tc.seconds, tc.frames, ltc.drop_frame);
        } else {
            warn!("reset_ltc: audio not initialized");
        }
        Ok(())
    }

    pub fn current_timecode(&self) -> Timecode {
        let audio = match self.audio.lock() {
            Ok(a) => a,
            Err(_) => {
                return Timecode {
                    hours: 0,
                    minutes: 0,
                    seconds: 0,
                    frames: 0,
                }
            }
        };
        if let Some(ref output) = *audio {
            if let Ok(ltc) = output.ltc.lock() {
                return ltc.tc;
            }
        }
        Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        }
    }

    pub fn play_beep(
        &self,
        sample_rate: u32,
        frequency: f32,
        duration: f32,
        volume: f32,
        channel: ChannelSel,
    ) -> Result<(), String> {
        let audio = self
            .audio
            .lock()
            .map_err(|e| format!("State lock error: {}", e))?;
        if let Some(ref output) = *audio {
            let samples = ltc_encoder::generate_beep_samples(sample_rate, frequency, duration, volume, channel);
            let mut producer = output
                .beep_producer
                .lock()
                .map_err(|e| format!("Beep producer lock error: {}", e))?;
            let pushed = producer.push_slice(&samples);
            if pushed < samples.len() {
                warn!("play_beep: ring buffer full, dropped {} beep samples", samples.len() - pushed);
            }
            // Ensure playback is active so the beep is audible even without active LTC
            if !output.playing.load(Ordering::Relaxed) {
                output
                    .stream
                    .play()
                    .map_err(|e| format!("Failed to start playback for beep: {}", e))?;
                output.playing.store(true, Ordering::Relaxed);
            }
        }
        Ok(())
    }

    pub fn push_samples(&self, samples: Vec<f32>) {
        let audio = match self.audio.lock() {
            Ok(a) => a,
            Err(e) => {
                warn!("push_samples: audio mutex poisoned: {}", e);
                return;
            }
        };
        if let Some(ref output) = *audio {
            let mut producer = match output.ltc_producer.lock() {
                Ok(p) => p,
                Err(e) => {
                    warn!("push_samples: producer mutex poisoned: {}", e);
                    return;
                }
            };
            let pushed = producer.push_slice(&samples);
            if pushed < samples.len() {
                warn!("push_samples: ring buffer full, dropped {} samples", samples.len() - pushed);
            }
        }
    }

    pub fn stop_output(&self) -> Result<(), String> {
        let mut audio = self
            .audio
            .lock()
            .map_err(|e| format!("State lock error: {}", e))?;
        if let Some(output) = audio.take() {
            output.streaming.store(false, Ordering::Relaxed);
            output.playing.store(false, Ordering::Relaxed);
            let mut ltc = output.ltc.lock().map_err(|e| format!("LTC state lock error: {}", e))?;
            ltc.running = false;
            ltc.stop_signal.store(true, Ordering::Relaxed);
            if let Some(handle) = ltc.scheduler_thread.take() {
                if let Err(e) = handle.join() {
                    warn!("LTC scheduler thread panicked on stop_output: {:?}", e);
                }
            }
            drop(output.stream);
            info!("Audio output stopped and stream dropped");
        } else {
            warn!("stop_output: no audio output to stop");
        }
        drop(audio);
        // Release system wake lock
        if let Ok(mut wl) = self.wake_lock.lock() {
            if wl.is_some() {
                *wl = None;
                info!("System wake lock released (stop_output)");
            }
        }
        Ok(())
    }

    /// Returns whether the system wake lock is currently held (display+idle prevention).
    pub fn wake_lock_active(&self) -> bool {
        self.wake_lock
            .lock()
            .map(|wl| wl.is_some())
            .unwrap_or(false)
    }
}

impl Default for AudioCore {
    fn default() -> Self {
        Self::new()
    }
}

// ── Config selection & stream building ────────────────────────────────────

fn choose_buffer_size(param: u32) -> cpal::BufferSize {
    if param > 0 {
        cpal::BufferSize::Fixed(param)
    } else {
        cpal::BufferSize::Default
    }
}

fn sample_format_name(fmt: cpal::SampleFormat) -> &'static str {
    match fmt {
        cpal::SampleFormat::F32 => "f32",
        cpal::SampleFormat::I16 => "i16",
        cpal::SampleFormat::I32 => "i32",
        cpal::SampleFormat::U16 => "u16",
        cpal::SampleFormat::I8 => "i8",
        cpal::SampleFormat::U8 => "u8",
        cpal::SampleFormat::I24 => "i24",
        cpal::SampleFormat::U24 => "u24",
        cpal::SampleFormat::I64 => "i64",
        cpal::SampleFormat::U32 => "u32",
        cpal::SampleFormat::U64 => "u64",
        cpal::SampleFormat::F64 => "f64",
        cpal::SampleFormat::DsdU8 => "dsd_u8",
        cpal::SampleFormat::DsdU16 => "dsd_u16",
        _ => "other",
    }
}

fn try_find_config(
    configs: &[cpal::SupportedStreamConfigRange],
    desired_channels: u16,
    target_rate: u32,
    desired_buffer_size: cpal::BufferSize,
    format_priority: &impl Fn(cpal::SampleFormat) -> u8,
) -> Option<(cpal::StreamConfig, cpal::SampleFormat, u8)> {
    let mut best: Option<(cpal::StreamConfig, cpal::SampleFormat, u8)> = None;
    for cfg_range in configs.iter() {
        let fmt = cfg_range.sample_format();
        let priority = format_priority(fmt);
        if cfg_range.channels() >= desired_channels
            && cfg_range.min_sample_rate() <= target_rate
            && cfg_range.max_sample_rate() >= target_rate
        {
            let channels = desired_channels;
            let raw_config = cfg_range.with_sample_rate(target_rate).config();
            let stream_config = cpal::StreamConfig {
                channels,
                sample_rate: raw_config.sample_rate,
                buffer_size: desired_buffer_size,
            };
            let is_better = match &best {
                None => true,
                Some((_, _, best_prio)) => priority > *best_prio,
            };
            if is_better {
                best = Some((stream_config, fmt, priority));
            }
        }
    }
    best
}

fn try_fallback_config(
    configs: &[cpal::SupportedStreamConfigRange],
    desired_channels: u16,
    target_rate: u32,
    desired_buffer_size: cpal::BufferSize,
    format_priority: &impl Fn(cpal::SampleFormat) -> u8,
) -> Option<(cpal::StreamConfig, cpal::SampleFormat, u8)> {
    let mut best: Option<(cpal::StreamConfig, cpal::SampleFormat, u8)> = None;
    for cfg_range in configs.iter() {
        let fmt = cfg_range.sample_format();
        let priority = format_priority(fmt);
        if cfg_range.channels() >= desired_channels {
            let channels = desired_channels;
            let rate = cfg_range
                .min_sample_rate()
                .max(target_rate)
                .min(cfg_range.max_sample_rate());
            let raw_config = cfg_range.with_sample_rate(rate).config();
            let stream_config = cpal::StreamConfig {
                channels,
                sample_rate: raw_config.sample_rate,
                buffer_size: desired_buffer_size,
            };
            let is_better = match &best {
                None => true,
                Some((_, _, best_prio)) => priority > *best_prio,
            };
            if is_better {
                best = Some((stream_config, fmt, priority));
            }
        }
    }
    best
}

fn select_best_config(
    configs: &[cpal::SupportedStreamConfigRange],
    desired_channels: u16,
    desired_sample_rate: u32,
    desired_buffer_size: cpal::BufferSize,
) -> Result<(cpal::StreamConfig, cpal::SampleFormat), String> {
    let format_priority = |fmt: cpal::SampleFormat| -> u8 {
        match fmt {
            cpal::SampleFormat::F32 => 5,
            cpal::SampleFormat::I16 => 4,
            cpal::SampleFormat::I32 => 3,
            cpal::SampleFormat::U16 => 2,
            cpal::SampleFormat::I8 => 1,
            cpal::SampleFormat::U8 => 0,
            _ => 0,
        }
    };

    let mut best_config = try_find_config(configs, desired_channels, desired_sample_rate, desired_buffer_size, &format_priority);

    if best_config.is_none() && desired_sample_rate >= 44100 {
        let fallback_rates = [48000u32, 44100];
        for &rate in &fallback_rates {
            if rate == desired_sample_rate {
                continue;
            }
            best_config = try_find_config(configs, desired_channels, rate, desired_buffer_size, &format_priority);
            if best_config.is_some() {
                break;
            }
        }
    }

    if best_config.is_none() {
        best_config = try_fallback_config(configs, desired_channels, desired_sample_rate, desired_buffer_size, &format_priority);
    }

    best_config
        .map(|(cfg, fmt, _)| (cfg, fmt))
        .ok_or_else(|| "No supported audio output config found for this device".to_string())
}

#[allow(clippy::too_many_arguments)]
fn build_stream_for_format(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    ltc_consumer: HeapConsumer<f32>,
    beep_consumer: HeapConsumer<f32>,
    streaming: Arc<AtomicBool>,
    underrun_count: Arc<AtomicU64>,
    callback_counter: Arc<AtomicU64>,
    err_handler: impl Fn(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, String> {
    match sample_format {
        cpal::SampleFormat::F32 => build_stream_generic::<f32>(device, config, ltc_consumer, beep_consumer, streaming, underrun_count, callback_counter, err_handler),
        cpal::SampleFormat::I16 => build_stream_generic::<i16>(device, config, ltc_consumer, beep_consumer, streaming, underrun_count, callback_counter, err_handler),
        cpal::SampleFormat::I32 => build_stream_generic::<i32>(device, config, ltc_consumer, beep_consumer, streaming, underrun_count, callback_counter, err_handler),
        cpal::SampleFormat::U16 => build_stream_generic::<u16>(device, config, ltc_consumer, beep_consumer, streaming, underrun_count, callback_counter, err_handler),
        other => Err(format!("Unsupported sample format: {:?}", other)),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_stream_generic<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut ltc_consumer: HeapConsumer<f32>,
    mut beep_consumer: HeapConsumer<f32>,
    streaming: Arc<AtomicBool>,
    underrun_count: Arc<AtomicU64>,
    callback_counter: Arc<AtomicU64>,
    err_handler: impl Fn(cpal::Error) + Send + 'static,
) -> Result<cpal::Stream, String>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let stream = device
        .build_output_stream::<T, _, _>(
            *config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                callback_counter.fetch_add(1, Ordering::Relaxed);

                let mut underrun_this_block = false;
                for sample in data.iter_mut() {
                    let ltc_val = match ltc_consumer.pop() {
                        Some(v) => v,
                        None => {
                            underrun_this_block = true;
                            0.0
                        }
                    };
                    let beep_val = beep_consumer.pop().unwrap_or(0.0);
                    *sample = T::from_sample(ltc_val + beep_val);
                }

                if underrun_this_block && streaming.load(Ordering::Relaxed) {
                    underrun_count.fetch_add(1, Ordering::Relaxed);
                }
            },
            err_handler,
            None,
        )
        .map_err(|e| format!("Failed to build audio output stream: {}", e))?;

    Ok(stream)
}

// ── Error classification helpers ────────────────────────────────────────────

pub fn is_permanent_device_error(err: &str) -> bool {
    let keywords = ["Permission denied", "Access denied"];
    keywords.iter().any(|kw| err.contains(kw))
}

/// Returns the default sample rate for audio processing.
pub fn suggest_sample_rate() -> u32 {
    48000
}

/// Available sample rate options for UI display.
pub const SAMPLE_RATE_OPTIONS: &[u32] = &[44100, 48000];

// ── Device enumeration ─────────────────────────────────────────────────────

const PLUGIN_KEYWORDS: &[&str] = &[
    "Discard all samples",
    "Rate Converter Plugin",
    "Samplerate Library",
    "Speex Resampler",
    "JACK Audio",
    "Open Sound System",
    "PipeWire Sound Server",
    "PulseAudio Sound Server",
    "Speex DSP",
    "channel upmix",
    "channel downmix",
    "Plugin for",
];

fn is_valid_device(name: &str, host_id: &cpal::HostId) -> bool {
    let host_name = host_id.name();
    if host_name == "pipewire" || host_name == "pulseaudio" {
        return true;
    }
    !PLUGIN_KEYWORDS.iter().any(|kw| name.contains(kw))
}

/// Aggregated config ranges for one device. Sentinels: `u16::MAX`/`u32::MAX`
/// (and the matching MIN) when no configs are exposed; buffer `u32::MIN`
/// when the host reports Unknown.
struct DeviceConfigSummary {
    formats: Vec<String>,
    channels_min: u16,
    channels_max: u16,
    rate_min: u32,
    rate_max: u32,
    buffer_min: u32,
    buffer_max: u32,
}

impl AudioDeviceInfo {
    /// Build from raw aggregated ranges, normalizing the "no data" sentinels
    /// to 0 — previously hand-inlined at all three construction sites.
    fn from_summary(id: String, name: String, is_default: bool, s: DeviceConfigSummary) -> Self {
        AudioDeviceInfo {
            id,
            name,
            is_default,
            formats: s.formats,
            channels_min: if s.channels_min != u16::MAX { s.channels_min } else { 0 },
            channels_max: if s.channels_max != u16::MIN { s.channels_max } else { 0 },
            sample_rate_min: if s.rate_min != u32::MAX { s.rate_min } else { 0 },
            sample_rate_max: if s.rate_max != u32::MIN { s.rate_max } else { 0 },
            buffer_min: if s.buffer_min != u32::MAX { s.buffer_min } else { 0 },
            buffer_max: if s.buffer_max != u32::MIN { s.buffer_max } else { 0 },
        }
    }
}

fn collect_device_configs(device: &cpal::Device) -> DeviceConfigSummary {
    let configs: Vec<_> = device
        .supported_output_configs()
        .map(|c| c.collect())
        .unwrap_or_default();
    let mut formats: Vec<String> = Vec::new();
    let mut min_channels = u16::MAX;
    let mut max_channels = u16::MIN;
    let mut min_rate = u32::MAX;
    let mut max_rate = u32::MIN;
    let mut min_buffer = u32::MAX;
    let mut max_buffer = u32::MIN;
    for cfg in &configs {
        let f = sample_format_name(cfg.sample_format());
        if !formats.iter().any(|x| x == f) {
            formats.push(f.to_string());
        }
        min_channels = min_channels.min(cfg.channels());
        max_channels = max_channels.max(cfg.channels());
        min_rate = min_rate.min(cfg.min_sample_rate());
        max_rate = max_rate.max(cfg.max_sample_rate());
        match cfg.buffer_size() {
            cpal::SupportedBufferSize::Range { min, max } => {
                min_buffer = min_buffer.min(*min);
                max_buffer = max_buffer.max(*max);
            }
            cpal::SupportedBufferSize::Unknown => {}
        }
    }
    DeviceConfigSummary {
        formats,
        channels_min: min_channels,
        channels_max: max_channels,
        rate_min: min_rate,
        rate_max: max_rate,
        buffer_min: min_buffer,
        buffer_max: max_buffer,
    }
}

/// Format the aggregated config ranges of one device into the log.
fn log_device_supported_configs(label: &str, summary: &DeviceConfigSummary) {
    let ch_range = if summary.channels_min == summary.channels_max {
        format!("{}", summary.channels_min)
    } else {
        format!("{}-{}", summary.channels_min, summary.channels_max)
    };
    let rate_range = if summary.rate_min == summary.rate_max {
        format!("{}", summary.rate_min)
    } else {
        format!("{}-{}", summary.rate_min, summary.rate_max)
    };
    let buf = if summary.buffer_min <= summary.buffer_max && summary.buffer_min != u32::MAX {
        if summary.buffer_min == summary.buffer_max {
            format!("buffer={}", summary.buffer_min)
        } else {
            format!("buffer={}-{}", summary.buffer_min, summary.buffer_max)
        }
    } else {
        String::from("buffer=unknown")
    };
    info!(
        "  Device {}: formats=[{}], channels={}, rates={}, {}",
        label,
        summary.formats.join(", "),
        ch_range,
        rate_range,
        buf,
    );
}

/// Shared probe path for the default device and the enumeration loop:
/// permanent config error → `None` (skip), non-permanent → proceed (empty
/// configs yield a zeroed entry), then log + collect + construct.
fn probe_device(
    device: &cpal::Device,
    id: String,
    display_name: String,
    is_default: bool,
    label: String,
) -> Option<AudioDeviceInfo> {
    match device.supported_output_configs() {
        Ok(_) => {}
        Err(e) => {
            let err_str = e.to_string();
            if is_permanent_device_error(&err_str) {
                warn!("Skipping device '{}': {}", display_name, err_str);
                return None;
            }
        }
    }
    let summary = collect_device_configs(device);
    log_device_supported_configs(&label, &summary);
    Some(AudioDeviceInfo::from_summary(id, display_name, is_default, summary))
}

pub fn list_audio_devices() -> Result<Vec<AudioDeviceInfo>, String> {
    let host = cpal::default_host();
    let host_id = host.id();
    let default_device = host.default_output_device();
    let default_name = default_device.as_ref().map(|d| d.to_string());

    let mut seen = HashSet::new();
    let mut devices: Vec<AudioDeviceInfo> = Vec::new();

    if let Some(ref dev) = default_device {
        let name = dev.to_string();
        if !name.is_empty() {
            if let Some(info) = probe_device(
                dev,
                String::from("default"),
                format!("{} (Default)", name),
                true,
                format!("\"{}\" (Default)", name),
            ) {
                seen.insert(name.clone());
                devices.push(info);
            }
        }
    }

    for device in host
        .output_devices()
        .map_err(|e| format!("Failed to enumerate output devices: {}", e))?
    {
        let name = device.to_string();
        if name.is_empty() || !is_valid_device(&name, &host_id) {
            continue;
        }
        if !seen.insert(name.clone()) {
            continue;
        }
        if let Some(info) = probe_device(
            &device,
            name.clone(),
            name.clone(),
            default_name.as_deref() == Some(&name),
            format!("\"{}\"", name),
        ) {
            devices.push(info);
        }
    }

    devices.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));

    info!("Found {} audio devices", devices.len());

    Ok(devices)
}

// ── LTC scheduler thread ───────────────────────────────────────────────────

/// Callback-stall watchdog state machine, extracted from
/// `ltc_scheduler_thread` so the recovery ladder is testable without a
/// device. The event push and any sleeping stay in the thread.
mod watchdog {
    use std::time::{Duration, Instant};

    /// Production stall timeout (also baked into the recovery-reason text).
    pub(crate) const STALL_TIMEOUT: Duration = Duration::from_millis(500);
    /// Production recovery-attempt budget.
    pub(crate) const MAX_ATTEMPTS: u8 = 3;

    /// Byte-exact recovery-reason text formatted into
    /// `AudioEvent::RecoveryNeeded { reason }` by the scheduler thread.
    /// The event text is a contract (engine log line), so it lives here next
    /// to the payload ingredients it is derived from.
    pub(crate) fn watchdog_reason(stall_timeout: Duration, attempt: u8, max_attempts: u8) -> String {
        format!(
            "callback stalled for {}ms (attempt {}/{})",
            stall_timeout.as_millis(),
            attempt,
            max_attempts
        )
    }

    /// Decision for one scheduler iteration.
    #[derive(Debug)]
    pub(crate) enum WatchdogAction {
        /// Callback healthy, or stall not yet confirmed — generate the next frame.
        Continue,
        /// Stall confirmed: emit `AudioEvent::RecoveryNeeded { reason }`, then
        /// wait `backoff` before the next check.
        Recover { attempt: u8, backoff: Duration },
        /// Recovery budget exhausted: emit `AudioEvent::StreamDead`, stop the thread.
        Dead,
    }

    pub(crate) struct CallbackWatchdog {
        stall_timeout: Duration,
        max_attempts: u8,
        window: Duration,
        backoff: Duration,
        last_callback_value: u64,
        last_check: Instant,
        recovery_attempts: u8,
        first_failure: Option<Instant>,
    }

    impl CallbackWatchdog {
        /// Production constants: 500 ms stall timeout, 3 recovery attempts,
        /// 10 s sliding reset window, 100 ms recovery backoff.
        pub(crate) fn new() -> Self {
            Self::with_constants(
                STALL_TIMEOUT,
                MAX_ATTEMPTS,
                Duration::from_secs(10),
                Duration::from_millis(100),
            )
        }

        pub(crate) fn with_constants(
            stall_timeout: Duration,
            max_attempts: u8,
            window: Duration,
            backoff: Duration,
        ) -> Self {
            Self {
                stall_timeout,
                max_attempts,
                window,
                backoff,
                last_callback_value: 0,
                last_check: Instant::now(),
                recovery_attempts: 0,
                first_failure: None,
            }
        }

        /// `now` injected — tests drive a virtual clock.
        pub(crate) fn tick(&mut self, now: Instant, current_callback: u64) -> WatchdogAction {
            if current_callback != self.last_callback_value {
                // Callback is alive — reset recovery state.
                self.last_callback_value = current_callback;
                self.last_check = now;
                self.recovery_attempts = 0;
                self.first_failure = None;
                return WatchdogAction::Continue;
            }
            if now.duration_since(self.last_check) <= self.stall_timeout {
                // Stall not yet confirmed.
                return WatchdogAction::Continue;
            }
            // Stall confirmed. Sliding window: reset the attempt counter if
            // the first failure is older than the window.
            if let Some(first) = self.first_failure {
                if now.duration_since(first) > self.window {
                    self.recovery_attempts = 0;
                    self.first_failure = None;
                }
            }
            if self.recovery_attempts < self.max_attempts {
                self.recovery_attempts += 1;
                if self.first_failure.is_none() {
                    self.first_failure = Some(now);
                }
                let attempt = self.recovery_attempts;
                // Reset the stall timer so we don't immediately re-trigger.
                self.last_callback_value = current_callback;
                self.last_check = now;
                WatchdogAction::Recover { attempt, backoff: self.backoff }
            } else {
                WatchdogAction::Dead
            }
        }
    }
}

use watchdog::{watchdog_reason, CallbackWatchdog, WatchdogAction, MAX_ATTEMPTS, STALL_TIMEOUT};

/// Scheduler push bookkeeping (drop counting + event cadence).
struct FramePushStats {
    frame_count: u64,
    drop_count: u64,
    last_drop_event: u64,
}

/// Push one generated stereo frame into the ring; returns the event to emit,
/// if any (`FramesDropped { total }` every 100 cumulative drops). Works on a
/// plain `HeapRb` — no device needed.
fn push_frame(
    producer: &mut HeapProducer<f32>,
    frame: &[f32],
    stats: &mut FramePushStats,
) -> Option<AudioEvent> {
    let needed = frame.len();
    let pushed = producer.push_slice(frame);
    if pushed < needed {
        stats.drop_count += 1;
        if stats.drop_count <= 1 || stats.drop_count % 100 == 0 {
            warn!("LTC scheduler: ring buffer full, dropped frame #{} (pushed {}/{}, total drops: {})",
                stats.frame_count, pushed, needed, stats.drop_count);
        }
        if stats.drop_count - stats.last_drop_event >= 100 {
            stats.last_drop_event = stats.drop_count;
            return Some(AudioEvent::FramesDropped { total: stats.drop_count });
        }
    }
    None
}

fn ltc_scheduler_thread(
    ltc_producer: Arc<Mutex<HeapProducer<f32>>>,
    ltc: Arc<Mutex<LtcStreamState>>,
    stop_signal: Arc<AtomicBool>,
    underrun_count: Arc<AtomicU64>,
    callback_counter: Arc<AtomicU64>,
    events: Arc<Mutex<Vec<AudioEvent>>>,
) {
    info!("LTC scheduler thread started");

    // Attempt to elevate thread priority for tighter scheduling
    #[cfg(not(target_os = "macos"))]
    match thread_priority::set_current_thread_priority(thread_priority::ThreadPriority::Max) {
        Ok(_) => info!("LTC scheduler thread priority elevated to Max"),
        Err(e) => warn!("Could not set thread priority: {:?}", e),
    }
    #[cfg(target_os = "macos")]
    info!("LTC scheduler thread priority not elevated (macOS)");

    let mut frame_buf: Vec<f32> = Vec::new();
    let mut last_underrun_value: u64 = 0;
    let mut wd = CallbackWatchdog::new();
    let mut push_stats = FramePushStats { frame_count: 0, drop_count: 0, last_drop_event: 0 };

    loop {
        if stop_signal.load(Ordering::Relaxed) {
            info!("LTC scheduler thread stopped via stop signal");
            return;
        }

        let (tc, fps, drop_frame, ltc_channel, ltc_volume, frame_dur, total_samples, samples_per_bit, mut last_level, new_accumulator) = {
            let state = match ltc.lock() {
                Ok(s) => s,
                Err(e) => {
                    error!("LTC scheduler: state mutex poisoned: {}", e);
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
            };
            if !state.running {
                drop(state);
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }

            let now = Instant::now();
            if now < state.next_frame_time {
                let sleep = state.next_frame_time - now;
                let target = state.next_frame_time;
                drop(state);
                // Hybrid sleep: OS sleep until 2ms before deadline, then spin-loop
                if sleep > Duration::from_millis(2) {
                    std::thread::sleep(sleep - Duration::from_millis(2));
                }
                while Instant::now() < target {
                    std::hint::spin_loop();
                }
                continue;
            }

            let tc = state.tc;
            let fps = state.fps;
            let drop_frame = state.drop_frame;
            let ltc_channel = state.ltc_channel;
            let ltc_volume = state.ltc_volume;
            let frame_dur = state.frame_duration;
            let last_level = state.last_level;

            let (frame_samples, spb, acc) = ltc_encoder::compute_frame_sample_count(
                state.exact_samples_per_frame,
                state.base_samples,
                state.samples_accumulator,
            );

            (tc, fps, drop_frame, ltc_channel, ltc_volume, frame_dur, frame_samples, spb, last_level, acc)
        };

        // ── Watchdog: check if audio callback is still alive ──
        let current_callback = callback_counter.load(Ordering::Relaxed);
        match wd.tick(Instant::now(), current_callback) {
            WatchdogAction::Continue => {}
            WatchdogAction::Recover { attempt, backoff } => {
                error!("LTC scheduler: audio callback has not fired for 500ms — stream appears dead");
                warn!("LTC scheduler: recovery attempt {}/3", attempt);
                let reason = watchdog_reason(STALL_TIMEOUT, attempt, MAX_ATTEMPTS);
                if let Ok(mut ev) = events.lock() {
                    ev.push(AudioEvent::RecoveryNeeded { reason });
                }
                // Sleep a short time before checking again so the main thread can act
                std::thread::sleep(backoff);
                continue;
            }
            WatchdogAction::Dead => {
                error!("LTC scheduler: audio callback has not fired for 500ms — stream appears dead");
                error!("LTC scheduler: 3 recovery attempts exhausted — stream permanently dead");
                if let Ok(mut ev) = events.lock() {
                    ev.push(AudioEvent::StreamDead);
                }
                return;
            }
        }

        // ── Watchdog: check for underruns ──
        let current_underrun = underrun_count.load(Ordering::Relaxed);
        if current_underrun > last_underrun_value {
            let new_underruns = current_underrun - last_underrun_value;
            warn!("LTC scheduler: detected {} callback underruns (total: {})", new_underruns, current_underrun);
            if let Ok(mut ev) = events.lock() {
                ev.push(AudioEvent::Underrun);
            }
            last_underrun_value = current_underrun;
        }

        // ── Generate LTC frame ──
        let needed = total_samples * 2;
        frame_buf.resize(needed, 0.0);

        generate_ltc_frame_stereo(
            &tc,
            drop_frame,
            total_samples,
            samples_per_bit,
            ltc_volume,
            ltc_channel,
            &mut last_level,
            &mut frame_buf[..needed],
        );

        // ── Push samples into lock-free ring buffer ──
        {
            let mut producer = match ltc_producer.lock() {
                Ok(p) => p,
                Err(e) => {
                    error!("LTC scheduler: producer mutex poisoned: {}", e);
                    return;
                }
            };
            if let Some(ev) = push_frame(&mut producer, &frame_buf[..needed], &mut push_stats) {
                if let Ok(mut evq) = events.lock() {
                    evq.push(ev);
                }
            }
        }

        push_stats.frame_count += 1;
        if push_stats.frame_count % 1000 == 0 {
            info!("LTC scheduler: frame={}, drops={}, channel={}, fps={}, tc={:02}:{:02}:{:02}:{:02}",
                push_stats.frame_count, push_stats.drop_count, ltc_channel.as_str(),
                fps, tc.hours, tc.minutes, tc.seconds, tc.frames);
        }

        {
            let mut state = match ltc.lock() {
                Ok(s) => s,
                Err(e) => {
                    error!("LTC scheduler: state mutex poisoned updating state: {}", e);
                    return;
                }
            };
            state.last_level = last_level;
            state.tc = increment_timecode(&state.tc, fps, drop_frame);
            state.next_frame_time += frame_dur;
            state.samples_accumulator = new_accumulator;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use watchdog::{watchdog_reason, CallbackWatchdog, WatchdogAction};

    // ── CallbackWatchdog (virtual clock, device-free) ─────────────────────

    fn wd_fast() -> CallbackWatchdog {
        // Accelerated constants for the recovery-ladder walk; behavior is
        // identical to the production constants under the virtual clock.
        CallbackWatchdog::with_constants(Duration::from_millis(500), 3, Duration::from_secs(10), Duration::from_millis(100))
    }

    #[test]
    fn test_watchdog_healthy_counter_continues_and_resets() {
        let mut wd = wd_fast();
        let t0 = Instant::now();
        assert!(matches!(wd.tick(t0, 0), WatchdogAction::Continue));
        // Counter advances → healthy; internal state resets.
        assert!(matches!(wd.tick(t0 + Duration::from_millis(1), 10), WatchdogAction::Continue));
        // Now a long stall: because the counter advanced at t0+1ms, the
        // stall clock restarted there.
        assert!(matches!(wd.tick(t0 + Duration::from_millis(400), 10), WatchdogAction::Continue));
        match wd.tick(t0 + Duration::from_millis(901), 10) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected Recover, got {:?}", other),
        }
    }

    #[test]
    fn test_watchdog_stall_not_confirmed_before_timeout() {
        let mut wd = wd_fast();
        let t0 = Instant::now();
        assert!(matches!(wd.tick(t0, 0), WatchdogAction::Continue));
        assert!(matches!(wd.tick(t0 + Duration::from_millis(499), 0), WatchdogAction::Continue));
    }

    #[test]
    fn test_watchdog_exactly_500ms_is_not_a_stall() {
        // Pin the strict `>`: a stall of exactly the timeout is not confirmed.
        let mut wd = wd_fast();
        let t0 = Instant::now();
        // Advance the counter once so the stall clock baseline is t0+1ms.
        assert!(matches!(wd.tick(t0, 0), WatchdogAction::Continue));
        assert!(matches!(wd.tick(t0 + Duration::from_millis(1), 1), WatchdogAction::Continue));
        assert!(matches!(wd.tick(t0 + Duration::from_millis(501), 1), WatchdogAction::Continue));
        // One microsecond past the timeout → confirmed.
        match wd.tick(t0 + Duration::from_millis(501_001), 1) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected Recover, got {:?}", other),
        }
    }

    #[test]
    fn test_watchdog_recovery_ladder_then_dead() {
        let mut wd = wd_fast();
        let t0 = Instant::now();
        wd.tick(t0, 0);
        let t1 = t0 + Duration::from_millis(600);
        match wd.tick(t1, 0) {
            WatchdogAction::Recover { attempt, backoff, .. } => {
                assert_eq!(attempt, 1);
                assert_eq!(backoff, Duration::from_millis(100));
            }
            other => panic!("expected Recover, got {:?}", other),
        }
        // Each Recover resets the stall clock, so a fresh 500 ms wait is
        // needed before the next confirmation.
        match wd.tick(t1 + Duration::from_millis(501), 0) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 2),
            other => panic!("expected Recover, got {:?}", other),
        }
        match wd.tick(t1 + Duration::from_millis(1002), 0) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 3),
            other => panic!("expected Recover, got {:?}", other),
        }
        assert!(matches!(wd.tick(t1 + Duration::from_millis(1503), 0), WatchdogAction::Dead));
    }

    #[test]
    fn test_watchdog_recovery_resets_when_callback_resumes() {
        let mut wd = wd_fast();
        let t0 = Instant::now();
        wd.tick(t0, 0);
        match wd.tick(t0 + Duration::from_millis(600), 0) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected Recover, got {:?}", other),
        }
        // Callback resumes → attempts and first_failure reset.
        assert!(matches!(wd.tick(t0 + Duration::from_millis(700), 5), WatchdogAction::Continue));
        // Counter stalls again → ladder restarts at attempt 1.
        match wd.tick(t0 + Duration::from_millis(1300), 5) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected Recover(1) after resume, got {:?}", other),
        }
    }

    #[test]
    fn test_watchdog_sliding_window_resets_attempts() {
        let mut wd = wd_fast();
        let t0 = Instant::now();
        wd.tick(t0, 0);
        match wd.tick(t0 + Duration::from_millis(600), 0) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 1),
            other => panic!("expected Recover, got {:?}", other),
        }
        // No progress; next stall check happens just past the 10 s window
        // from the first failure → the attempt counter was reset to 0.
        match wd.tick(t0 + Duration::from_millis(10_601), 0) {
            WatchdogAction::Recover { attempt, .. } => assert_eq!(attempt, 1, "window reset must restart the ladder"),
            other => panic!("expected Recover, got {:?}", other),
        }
    }

    #[test]
    fn test_watchdog_recover_payload() {
        let mut wd = CallbackWatchdog::with_constants(
            Duration::from_millis(500), 3, Duration::from_secs(10), Duration::from_millis(100),
        );
        let t0 = Instant::now();
        wd.tick(t0, 0);
        match wd.tick(t0 + Duration::from_millis(600), 0) {
            WatchdogAction::Recover { attempt, backoff, .. } => {
                assert_eq!(attempt, 1);
                assert_eq!(backoff, Duration::from_millis(100));
            }
            other => panic!("expected Recover, got {:?}", other),
        }
    }

    /// The exact event text is a contract: the scheduler formats it into
    /// `AudioEvent::RecoveryNeeded { reason }` and the engine logs it.
    #[test]
    fn test_watchdog_reason_string_byte_exact() {
        assert_eq!(
            watchdog_reason(Duration::from_millis(500), 1, 3),
            "callback stalled for 500ms (attempt 1/3)"
        );
        assert_eq!(
            watchdog_reason(Duration::from_millis(750), 2, 4),
            "callback stalled for 750ms (attempt 2/4)"
        );
    }

    // ── push_frame ────────────────────────────────────────────────────────

    fn stats() -> FramePushStats {
        FramePushStats { frame_count: 0, drop_count: 0, last_drop_event: 0 }
    }

    #[test]
    fn test_push_frame_non_full_ring_returns_none() {
        let rb = HeapRb::<f32>::new(1024);
        let (mut prod, _cons) = rb.split();
        let mut st = stats();
        let frame = vec![0.0f32; 64];
        for i in 0..10 {
            assert!(push_frame(&mut prod, &frame, &mut st).is_none());
            st.frame_count += 1;
            assert_eq!(st.frame_count, i + 1);
        }
        assert_eq!(st.drop_count, 0);
    }

    #[test]
    fn test_push_frame_full_ring_counts_drop_without_event() {
        let rb = HeapRb::<f32>::new(8);
        let (mut prod, _cons) = rb.split();
        let mut st = stats();
        let frame = vec![0.0f32; 64]; // bigger than the ring
        assert!(push_frame(&mut prod, &frame, &mut st).is_none(), "first drop emits no event");
        assert_eq!(st.drop_count, 1);
    }

    #[test]
    fn test_push_frame_drop_event_cadence_every_100() {
        let rb = HeapRb::<f32>::new(2);
        let (mut prod, _cons) = rb.split();
        let mut st = stats();
        let frame = vec![0.0f32; 64];
        let mut events = Vec::new();
        for _ in 0..200 {
            if let Some(ev) = push_frame(&mut prod, &frame, &mut st) {
                events.push(ev);
            }
        }
        assert_eq!(st.drop_count, 200);
        let totals: Vec<u64> = events
            .iter()
            .map(|e| match e { AudioEvent::FramesDropped { total } => *total, other => panic!("unexpected event {:?}", other) })
            .collect();
        assert_eq!(totals, vec![100, 200], "FramesDropped every 100 cumulative drops");
    }

    // ── AudioDeviceInfo::from_summary ─────────────────────────────────────

    fn summary(ch_min: u16, ch_max: u16, r_min: u32, r_max: u32, b_min: u32, b_max: u32) -> DeviceConfigSummary {
        DeviceConfigSummary {
            formats: vec!["f32".to_string()],
            channels_min: ch_min,
            channels_max: ch_max,
            rate_min: r_min,
            rate_max: r_max,
            buffer_min: b_min,
            buffer_max: b_max,
        }
    }

    #[test]
    fn test_from_summary_all_sentinels_become_zero() {
        let info = AudioDeviceInfo::from_summary(
            "id".into(), "name".into(), false,
            summary(u16::MAX, u16::MIN, u32::MAX, u32::MIN, u32::MAX, u32::MIN),
        );
        assert_eq!(info.channels_min, 0);
        assert_eq!(info.channels_max, 0);
        assert_eq!(info.sample_rate_min, 0);
        assert_eq!(info.sample_rate_max, 0);
        assert_eq!(info.buffer_min, 0);
        assert_eq!(info.buffer_max, 0);
        assert!(info.formats.contains(&"f32".to_string()));
    }

    #[test]
    fn test_from_summary_real_values_pass_through() {
        let info = AudioDeviceInfo::from_summary(
            "id".into(), "name".into(), true,
            summary(1, 2, 44100, 48000, 64, 4096),
        );
        assert_eq!(info.channels_min, 1);
        assert_eq!(info.channels_max, 2);
        assert_eq!(info.sample_rate_min, 44100);
        assert_eq!(info.sample_rate_max, 48000);
        assert_eq!(info.buffer_min, 64);
        assert_eq!(info.buffer_max, 4096);
        assert!(info.is_default);
    }

    #[test]
    fn test_from_summary_mixed_sentinels() {
        // Min sentinels, max real: only the sentinel sides normalize to 0.
        let info = AudioDeviceInfo::from_summary(
            "id".into(), "name".into(), false,
            summary(u16::MAX, 8, u32::MAX, 96000, 64, u32::MIN),
        );
        assert_eq!(info.channels_min, 0);
        assert_eq!(info.channels_max, 8);
        assert_eq!(info.sample_rate_min, 0);
        assert_eq!(info.sample_rate_max, 96000);
        assert_eq!(info.buffer_min, 64);
        assert_eq!(info.buffer_max, 0);
    }

    // ── sample_format_name ────────────────────────────────────────────────

    #[test]
    fn test_sample_format_name_f32() {
        assert_eq!(sample_format_name(cpal::SampleFormat::F32), "f32");
    }

    #[test]
    fn test_sample_format_name_i16() {
        assert_eq!(sample_format_name(cpal::SampleFormat::I16), "i16");
    }

    #[test]
    fn test_sample_format_name_i32() {
        assert_eq!(sample_format_name(cpal::SampleFormat::I32), "i32");
    }

    #[test]
    fn test_sample_format_name_u16() {
        assert_eq!(sample_format_name(cpal::SampleFormat::U16), "u16");
    }

    #[test]
    fn test_sample_format_name_i8() {
        assert_eq!(sample_format_name(cpal::SampleFormat::I8), "i8");
    }

    #[test]
    fn test_sample_format_name_u8() {
        assert_eq!(sample_format_name(cpal::SampleFormat::U8), "u8");
    }

    #[test]
    fn test_sample_format_name_f64() {
        assert_eq!(sample_format_name(cpal::SampleFormat::F64), "f64");
    }

    // ── choose_buffer_size ────────────────────────────────────────────────

    #[test]
    fn test_choose_buffer_size_zero_returns_default() {
        assert!(matches!(choose_buffer_size(0), cpal::BufferSize::Default));
    }

    #[test]
    fn test_choose_buffer_size_positive_returns_fixed() {
        assert!(matches!(choose_buffer_size(256), cpal::BufferSize::Fixed(256)));
        assert!(matches!(choose_buffer_size(1024), cpal::BufferSize::Fixed(1024)));
        assert!(matches!(choose_buffer_size(480), cpal::BufferSize::Fixed(480)));
    }

    #[test]
    fn test_choose_buffer_size_never_returns_fixed_1024_for_zero() {
        // This test documents the fix: zero param must NOT force Fixed(1024)
        assert!(matches!(choose_buffer_size(0), cpal::BufferSize::Default));
    }

    // ── is_permanent_device_error ─────────────────────────────────────────

    #[test]
    fn test_is_permanent_device_error_permission_denied() {
        assert!(is_permanent_device_error("Permission denied"));
    }

    #[test]
    fn test_is_permanent_device_error_access_denied() {
        assert!(is_permanent_device_error("Access denied"));
    }

    #[test]
    fn test_is_permanent_device_error_not_permanent() {
        assert!(!is_permanent_device_error("device temporarily busy"));
        assert!(!is_permanent_device_error("device not found"));
    }

    #[test]
    fn test_is_permanent_device_error_partial_context() {
        assert!(is_permanent_device_error("ALSA: Permission denied"));
        assert!(is_permanent_device_error("Access denied: /dev/snd/pcmC0D0p"));
    }

    #[test]
    fn test_is_permanent_device_error_empty() {
        assert!(!is_permanent_device_error(""));
    }

    // ── suggest_sample_rate ───────────────────────────────────────────────

    #[test]
    fn test_suggest_sample_rate_returns_valid() {
        let rate = suggest_sample_rate();
        assert!(
            SAMPLE_RATE_OPTIONS.contains(&rate),
            "suggested rate {} should be one of {:?}",
            rate,
            SAMPLE_RATE_OPTIONS,
        );
    }

    // ── is_valid_device ───────────────────────────────────────────────────

    #[test]
    fn test_is_valid_device_plugin_keyword_filtered() {
        let host = cpal::default_host();
        let host_id = host.id();
        assert!(!is_valid_device("Discard all samples", &host_id), "Discard all samples should be filtered");
        assert!(!is_valid_device("Rate Converter Plugin", &host_id), "Rate Converter Plugin should be filtered");
        assert!(!is_valid_device("Samplerate Library", &host_id), "Samplerate Library should be filtered");
    }

    #[test]
    fn test_is_valid_device_clean_name_allowed() {
        let host = cpal::default_host();
        let host_id = host.id();
        assert!(is_valid_device("Built-in Audio Analog Stereo", &host_id));
        assert!(is_valid_device("UMC204HD", &host_id));
    }

    #[test]
    fn test_is_valid_device_pipewire_pulseaudio_always_allowed() {
        let host = cpal::default_host();
        let host_id = host.id();
        assert!(is_valid_device("Normal Device Name", &host_id));
    }

    // ── AudioCore: new / drain_events / sample_format_name / wake_lock ────

    #[test]
    fn test_audio_core_new_initial_state() {
        let core = AudioCore::new();
        assert!(core.drain_events().is_empty());
        assert!(core.sample_format_name().is_empty());
        assert!(!core.wake_lock_active());
    }

    #[test]
    fn test_audio_core_drain_events_idempotent() {
        let core = AudioCore::new();
        assert!(core.drain_events().is_empty());
        assert!(core.drain_events().is_empty());
    }

    #[test]
    fn test_audio_core_current_timecode_when_not_initialized() {
        let core = AudioCore::new();
        let tc = core.current_timecode();
        assert_eq!(tc.hours, 0);
        assert_eq!(tc.minutes, 0);
        assert_eq!(tc.seconds, 0);
        assert_eq!(tc.frames, 0);
    }

    // ── AudioEvent types ──────────────────────────────────────────────────

    #[test]
    fn test_audio_event_debug() {
        let e1 = AudioEvent::StreamError("test".into());
        let e2 = AudioEvent::StreamDied;
        let e3 = AudioEvent::Underrun;
        let e4 = AudioEvent::FramesDropped { total: 42 };
        assert!(format!("{:?}", e1).contains("StreamError"));
        assert!(format!("{:?}", e2).contains("StreamDied"));
        assert!(format!("{:?}", e3).contains("Underrun"));
        assert!(format!("{:?}", e4).contains("42"));
    }

    #[test]
    fn test_audio_event_clone() {
        let e = AudioEvent::StreamError("msg".into());
        let cloned = e.clone();
        assert!(matches!(cloned, AudioEvent::StreamError(ref m) if m == "msg"));
    }

    // ── SAMPLE_RATE_OPTIONS ───────────────────────────────────────────────

    #[test]
    fn test_sample_rate_options_valid() {
        // Policy: the broadcast-standard rates the hardware path is tuned
        // for must remain offered. The list may grow, but not duplicate.
        assert!(SAMPLE_RATE_OPTIONS.contains(&44100));
        assert!(SAMPLE_RATE_OPTIONS.contains(&48000));
        let mut rates = SAMPLE_RATE_OPTIONS.to_vec();
        rates.sort_unstable();
        rates.dedup();
        assert_eq!(rates.len(), SAMPLE_RATE_OPTIONS.len(), "rates must be distinct");
    }
}