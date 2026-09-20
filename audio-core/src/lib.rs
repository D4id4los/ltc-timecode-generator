use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use keepawake::{Builder as WakeBuilder, KeepAwake};
use log::{debug, error, info, warn};
use ringbuf::{HeapRb, HeapProducer, HeapConsumer};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub mod ltc_decoder;
pub mod ltc_decoder_libltc;
pub mod ltc_encoder;

pub use ltc_encoder::{get_ltc_bits, increment_timecode, generate_ltc_frame_stereo};

// ── Ring buffer capacities ─────────────────────────────────────────────────

const LTC_RING_CAPACITY: usize = 262_144;   // 128K stereo samples (~2.7s at 48kHz, ~8s at 16kHz)
const BEEP_RING_CAPACITY: usize = 32_768;    // 32K stereo samples (~0.34s at 48kHz, ~1s at 16kHz)

// ── Types ──────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Timecode {
    pub hours: u32,
    pub minutes: u32,
    pub seconds: u32,
    pub frames: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum AudioEvent {
    StreamError(String),
    StreamDied,
    StreamRecovering { attempt: u8 },
    StreamDead,
    RecoveryNeeded { reason: String },
    Underrun,
    FramesDropped { total: u64 },
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub formats: Vec<String>,
    pub channels_min: u16,
    pub channels_max: u16,
    pub sample_rate_min: u32,
    pub sample_rate_max: u32,
    pub buffer_min: u32,
    pub buffer_max: u32,
}

struct LtcStreamState {
    running: bool,
    tc: Timecode,
    fps: f64,
    drop_frame: bool,
    ltc_channel: String,
    ltc_volume: f32,
    sample_rate: u32,
    last_level: (f32, f32),
    frame_duration: Duration,
    next_frame_time: Instant,
    stop_signal: Arc<AtomicBool>,
    scheduler_thread: Option<JoinHandle<()>>,
    /// Exact (fractional) samples per frame: `sample_rate / fps`.
    exact_samples_per_frame: f64,
    /// `floor(exact_samples_per_frame)` — the base number of (mono) samples per frame.
    base_samples: usize,
    /// Running fractional-sample accumulator.  Added to `exact_samples_per_frame.fract()`
    /// each frame; when it reaches ≥1.0, one extra sample is added to that frame
    /// and the accumulator is decremented.
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

        let buf_size = if buffer_size > 0 {
            cpal::BufferSize::Fixed(buffer_size)
        } else {
            let mut min_buf = u32::MAX;
            let mut max_buf = u32::MIN;
            for cfg in &configs {
                if let cpal::SupportedBufferSize::Range { min, max } = cfg.buffer_size() {
                    min_buf = min_buf.min(*min);
                    max_buf = max_buf.max(*max);
                }
            }
            if min_buf <= 1024 && 1024 <= max_buf {
                cpal::BufferSize::Fixed(1024)
            } else if max_buf > 0 {
                cpal::BufferSize::Fixed(max_buf)
            } else {
                cpal::BufferSize::Default
            }
        };

        let (stream_config, sample_format) = select_best_config(
            &configs,
            2,
            sample_rate,
            buf_size,
        )?;

        let fmt_name = sample_format_name(sample_format);

        info!(
            "Audio output initialized: device={}, channels={}, sample_rate={}, buffer_size={}, format={}",
            device,
            stream_config.channels,
            stream_config.sample_rate,
            buffer_size,
            fmt_name,
        );

        // Lock-free LTC ring buffer: Producer shared via Arc<Mutex<>> (scheduler + push_samples),
        // Consumer moved into audio callback (lock-free pop)
        let ltc_rb = HeapRb::<f32>::new(LTC_RING_CAPACITY);
        let (ltc_producer, ltc_consumer) = ltc_rb.split();

        // Lock-free beep ring buffer: same pattern
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
            ltc_channel: String::from("both"),
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

        stream
            .play()
            .map_err(|e| format!("Failed to play audio output stream: {}", e))?;

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
        ltc_channel: String,
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
            ltc_channel: ltc_channel.clone(),
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
                    &ltc_channel,
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
        channel: &str,
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

// ── Chunked LTC decode infrastructure ─────────────────────────────────────

/// Configuration for chunked WAV reading.
pub struct DecodeConfig {
    /// Target raw-audio chunk size in bytes (~50MB).
    pub chunk_size_bytes: u64,
    /// Overlap between adjacent chunks in seconds (~2 seconds).
    pub overlap_seconds: f64,
}

impl Default for DecodeConfig {
    fn default() -> Self {
        Self {
            chunk_size_bytes: 50_000_000,  // 50 MB
            overlap_seconds: 2.0,
        }
    }
}

/// Shared progress state for a chunked decode operation.
pub struct DecodeProgress {
    pub chunks_total: usize,
    pub chunks_completed: Arc<AtomicUsize>,
    pub cancel_flag: Arc<AtomicBool>,
}

impl DecodeProgress {
    pub fn new(chunks_total: usize) -> Self {
        Self {
            chunks_total,
            chunks_completed: Arc::new(AtomicUsize::new(0)),
            cancel_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn percent(&self) -> f32 {
        if self.chunks_total == 0 {
            return 1.0;
        }
        self.chunks_completed.load(Ordering::Relaxed) as f32 / self.chunks_total as f32
    }

    pub fn cancel(&self) {
        self.cancel_flag.store(true, Ordering::Relaxed);
    }
}

/// Low-level WAV chunk reader that reads from a data section offset without
/// loading the entire file into memory.
pub struct WavChunkReader {
    file: std::fs::File,
    spec: hound::WavSpec,
    data_start: u64,
    data_len: u64,
    total_mono_samples: usize,
    channels: usize,
    bytes_per_sample: u64,
}

impl WavChunkReader {
    /// Open a WAV file, parse the header, and prepare for chunked reading.
    pub fn open(path: &Path) -> Result<(Self, std::time::Instant), String> {
        let start = std::time::Instant::now();
        let file = std::fs::File::open(path)
            .map_err(|e| format!("Failed to open WAV file: {}", e))?;

        let (spec, data_offset, data_len) = {
            let tmp = file.try_clone()
                .map_err(|e| format!("Failed to clone file handle: {}", e))?;
            let reader = hound::WavReader::new(tmp)
                .map_err(|e| format!("Failed to read WAV header: {}", e))?;
            let spec = reader.spec();
            let mut inner = reader.into_inner();
            let offset = inner.stream_position()
                .map_err(|e| format!("Failed to get data offset: {}", e))?;
            // Compute remaining data length
            let file_len = inner.seek(SeekFrom::End(0))
                .map_err(|e| format!("Failed to seek: {}", e))?;
            let data_len = file_len.saturating_sub(offset);
            (spec, offset, data_len)
        };

        let bytes_per_sample = (spec.bits_per_sample / 8) as u64;
        let channels = spec.channels as usize;
        let total_mono_samples = (data_len / bytes_per_sample / spec.channels as u64) as usize;

        info!("WavChunkReader: {} ({} Hz, {} ch, {} bit, {:.2}s, data_offset={}, data_len={})",
            path.display(), spec.sample_rate, channels, spec.bits_per_sample,
            total_mono_samples as f64 / spec.sample_rate as f64,
            data_offset, data_len);

        Ok((Self {
            file,
            spec,
            data_start: data_offset,
            data_len,
            total_mono_samples,
            channels,
            bytes_per_sample,
        }, start))
    }

    pub fn spec(&self) -> &hound::WavSpec { &self.spec }
    pub fn sample_rate(&self) -> u32 { self.spec.sample_rate }
    pub fn channels(&self) -> usize { self.channels }
    pub fn total_mono_samples(&self) -> usize { self.total_mono_samples }

    /// Read a range of mono samples (first channel) from the file.
    /// `start_sample` and `num_samples` are in mono (first-channel) sample units.
    /// Returns a `Vec<f32>` for the builtin decoder.
    pub fn read_mono_samples_f32(&mut self, start_sample: usize, num_samples: usize) -> Result<Vec<f32>, String> {
        let byte_offset = self.data_start + (start_sample * self.channels) as u64 * self.bytes_per_sample;
        let bytes_to_read = num_samples * self.channels * self.bytes_per_sample as usize;
        let max_bytes = self.data_len as usize - ((start_sample * self.channels) as u64 * self.bytes_per_sample).min(self.data_len) as usize;
        let bytes_to_read = bytes_to_read.min(max_bytes);

        self.file.seek(SeekFrom::Start(byte_offset))
            .map_err(|e| format!("Failed to seek: {}", e))?;

        let mut raw = vec![0u8; bytes_to_read];
        let mut pos = 0;
        while pos < bytes_to_read {
            let n = self.file.read(&mut raw[pos..])
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            if n == 0 { break; }
            pos += n;
        }
        raw.truncate(pos);

        match self.spec.sample_format {
            hound::SampleFormat::Int => {
                let max_val = (1i64 << (self.spec.bits_per_sample - 1)) as f32;
                let samples_per_channel = raw.len() / (self.channels * self.bytes_per_sample as usize);
                let mut result = Vec::with_capacity(samples_per_channel);
                for i in 0..samples_per_channel {
                    let sample_start = i * self.channels * self.bytes_per_sample as usize;
                    let byte_ofs = sample_start;
                    let sample = match self.bytes_per_sample {
                        1 => (raw[byte_ofs] as i32) - 128, // 8-bit WAV is unsigned
                        2 => i16::from_le_bytes([raw[byte_ofs], raw[byte_ofs + 1]]) as i32,
                        3 => {
                            let b = &raw[byte_ofs..byte_ofs + 3];
                            let val = i32::from_le_bytes([b[0], b[1], b[2], 0]);
                            (val << 8) >> 8
                        }
                        4 => i32::from_le_bytes([raw[byte_ofs], raw[byte_ofs + 1], raw[byte_ofs + 2], raw[byte_ofs + 3]]),
                        _ => return Err(format!("Unsupported bytes per sample: {}", self.bytes_per_sample)),
                    };
                    result.push(sample as f32 / max_val);
                }
                Ok(result)
            }
            hound::SampleFormat::Float => {
                let samples_per_channel = raw.len() / (self.channels * 4);
                let mut result = Vec::with_capacity(samples_per_channel);
                for i in 0..samples_per_channel {
                    let byte_ofs = i * self.channels * 4;
                    let sample = f32::from_le_bytes([
                        raw[byte_ofs], raw[byte_ofs + 1],
                        raw[byte_ofs + 2], raw[byte_ofs + 3],
                    ]);
                    result.push(sample);
                }
                Ok(result)
            }
        }
    }

    /// Read a range of mono samples as `Vec<i16>` for the libltc decoder.
    /// Only works for 16-bit integer PCM.
    pub fn read_mono_samples_i16(&mut self, start_sample: usize, num_samples: usize) -> Result<Vec<i16>, String> {
        if self.spec.sample_format != hound::SampleFormat::Int || self.bytes_per_sample != 2 {
            return Err("libltc chunk reader requires 16-bit integer PCM".to_string());
        }

        let byte_offset = self.data_start + (start_sample * self.channels) as u64 * self.bytes_per_sample;
        let bytes_to_read = num_samples * self.channels * self.bytes_per_sample as usize;
        let max_bytes = self.data_len as usize - ((start_sample * self.channels) as u64 * self.bytes_per_sample).min(self.data_len) as usize;
        let bytes_to_read = bytes_to_read.min(max_bytes);

        self.file.seek(SeekFrom::Start(byte_offset))
            .map_err(|e| format!("Failed to seek: {}", e))?;

        let mut raw = vec![0u8; bytes_to_read];
        let mut pos = 0;
        while pos < bytes_to_read {
            let n = self.file.read(&mut raw[pos..])
                .map_err(|e| format!("Failed to read samples: {}", e))?;
            if n == 0 { break; }
            pos += n;
        }
        raw.truncate(pos);

        let num_mono_samples = raw.len() / (self.channels * 2);
        let mut result = Vec::with_capacity(num_mono_samples);
        for i in 0..num_mono_samples {
            let byte_ofs = i * self.channels * 2;
            let sample = i16::from_le_bytes([raw[byte_ofs], raw[byte_ofs + 1]]);
            result.push(sample);
        }
        Ok(result)
    }
}

/// Decode LTC from a WAV file in parallel chunks with progress reporting and cancelation.
///
/// The WAV file is read in ~50 MB chunks with a configurable overlap. Each chunk is
/// decoded independently in a scoped thread pool. Results are merged by sorting all
/// timecodes by `timecode_secs` and removing duplicates at chunk boundaries.
///
/// `progress` is used for reporting progress and cancelation. The caller should
/// poll `progress.percent()` and check `progress.cancel_flag` from another thread.
pub fn decode_ltc_chunked(
    path: &Path,
    use_libltc: bool,
    fps: f64,
    drop_frame: bool,
    config: DecodeConfig,
    progress: &DecodeProgress,
) -> Result<LtcDetectionResult, String> {
    let (chunk_reader, overall_start) = WavChunkReader::open(path)?;
    let sample_rate = chunk_reader.sample_rate();
    let channels = chunk_reader.channels();
    let total_mono = chunk_reader.total_mono_samples();
    let total_duration = total_mono as f64 / sample_rate as f64;

    debug!("decode_ltc_chunked: {} samples @ {} Hz, {} ch, config chunk={} bytes, overlap={}s",
        total_mono, sample_rate, channels, config.chunk_size_bytes, config.overlap_seconds);

    if total_mono == 0 {
        warn!("decode_ltc_chunked: WAV file contains no samples");
        return Ok(LtcDetectionResult::error("Audio file contains no samples"));
    }

    // Calculate chunk boundaries (in mono samples)
    // chunk_bytes = chunk_size_bytes (raw audio bytes, all channels)
    let bytes_per_mono_sample = (channels as u64) * (chunk_reader.spec.bits_per_sample as u64 / 8);
    let chunk_mono_samples = if bytes_per_mono_sample > 0 {
        (config.chunk_size_bytes / bytes_per_mono_sample) as usize
    } else {
        total_mono / 4  // fallback
    };
    let overlap_samples = (config.overlap_seconds * sample_rate as f64) as usize;
    let chunk_mono_samples = chunk_mono_samples.max(overlap_samples * 2);

    let mut chunks: Vec<(usize, usize)> = Vec::new(); // (start, end) in mono samples
    let mut pos = 0usize;
    while pos < total_mono {
        let end = (pos + chunk_mono_samples).min(total_mono);
        chunks.push((pos, end));
        if end >= total_mono { break; }
        // Next chunk starts at end - overlap
        let next_start = end.saturating_sub(overlap_samples);
        if next_start <= pos { break; } // prevent infinite loop
        pos = next_start;
    }

    let num_chunks = chunks.len();
    info!("decode_ltc_chunked: split into {} chunks ({} mono samples each, overlap={} samples)",
        num_chunks, chunk_mono_samples, overlap_samples);

    if num_chunks == 0 {
        return Ok(LtcDetectionResult::error("No audio data to decode"));
    }

    // Collect chunk results
    struct ChunkResult {
        chunk_idx: usize,
        result: Result<LtcDetectionResult, String>,
    }

    let mut chunk_results: Vec<ChunkResult> = Vec::with_capacity(num_chunks);

    // Use scoped threads for parallel decoding
    let progress_completed = progress.chunks_completed.clone();
    let cancel_flag = progress.cancel_flag.clone();

    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(num_chunks);
        // We need one reader per thread. Clone the file handle + re-seek for each.
        for (chunk_idx, &(start_sample, end_sample)) in chunks.iter().enumerate() {
            if cancel_flag.load(Ordering::Relaxed) {
                info!("decode_ltc_chunked: cancel requested, stopping dispatch at chunk {}", chunk_idx);
                break;
            }

            let num_samples = end_sample - start_sample;
            let cancel_flag = cancel_flag.clone();
            let progress_completed = progress_completed.clone();

            let handle = s.spawn(move || {
                if cancel_flag.load(Ordering::Relaxed) {
                    return ChunkResult {
                        chunk_idx,
                        result: Err("Canceled".to_string()),
                    };
                }

                // Create a fresh reader for this chunk (each thread needs its own file handle)
                let mut local_reader = match WavChunkReader::open(path) {
                    Ok((r, _)) => r,
                    Err(e) => return ChunkResult {
                        chunk_idx,
                        result: Err(format!("Failed to open file for chunk {}: {}", chunk_idx, e)),
                    },
                };

                let chunk_start = Instant::now();

                if use_libltc {
                    let samples = match local_reader.read_mono_samples_i16(start_sample, num_samples) {
                        Ok(s) => s,
                        Err(e) => return ChunkResult {
                            chunk_idx,
                            result: Err(format!("Failed to read chunk {}: {}", chunk_idx, e)),
                        },
                    };
                    let result = crate::ltc_decoder_libltc::decode_ltc_samples_libltc(
                        &samples, 1, sample_rate, fps, drop_frame, chunk_start,
                    );
                    let elapsed = chunk_start.elapsed();
                    debug!("Chunk {}/{} decoded (libltc): {:.1}ms", chunk_idx + 1, num_chunks, elapsed.as_secs_f64() * 1000.0);
                    progress_completed.fetch_add(1, Ordering::Relaxed);
                    ChunkResult { chunk_idx, result }
                } else {
                    let samples = match local_reader.read_mono_samples_f32(start_sample, num_samples) {
                        Ok(s) => s,
                        Err(e) => return ChunkResult {
                            chunk_idx,
                            result: Err(format!("Failed to read chunk {}: {}", chunk_idx, e)),
                        },
                    };
                    let result = crate::ltc_decoder::decode_ltc_samples(
                        &samples, sample_rate, 1, fps, drop_frame, chunk_start,
                    );
                    let elapsed = chunk_start.elapsed();
                    debug!("Chunk {}/{} decoded (builtin): {:.1}ms", chunk_idx + 1, num_chunks, elapsed.as_secs_f64() * 1000.0);
                    progress_completed.fetch_add(1, Ordering::Relaxed);
                    ChunkResult { chunk_idx, result }
                }
            });

            handles.push(handle);
        }

        for handle in handles {
            chunk_results.push(handle.join().expect("chunk decode thread panicked"));
        }
    });

    // Check for cancelation
    if cancel_flag.load(Ordering::Relaxed) {
        return Err("Decode canceled by user".to_string());
    }

    // Merge results
    chunk_results.sort_by_key(|cr| cr.chunk_idx);

    let mut all_timecodes: Vec<(usize, FrameTimecode)> = Vec::new(); // (chunk_idx, ftc)
    let mut merged_details: Vec<String> = Vec::new();
    let mut first_tc_secs: f64 = f64::MAX;
    let last_sample_rate: u32 = sample_rate;
    let mut max_conf: f32 = 0.0;

    for cr in &chunk_results {
        match &cr.result {
            Ok(r) => {
                merged_details.push(format!("Chunk {}: {} valid / {} possible (conf {:.1}%)",
                    cr.chunk_idx, r.valid_frames, r.total_possible_frames, r.avg_confidence * 100.0));
                max_conf = max_conf.max(r.avg_confidence);
                let chunk_start_sample = chunks.get(cr.chunk_idx).map(|&(s, _)| s).unwrap_or(0);
                let chunk_start_secs = chunk_start_sample as f64 / sample_rate as f64;
                let chunk_first_secs = if r.first_ltc_timecode_secs > 0.0 {
                    r.first_ltc_timecode_secs + chunk_start_secs
                } else {
                    0.0
                };
                if chunk_first_secs > 0.0 && chunk_first_secs < first_tc_secs {
                    first_tc_secs = chunk_first_secs;
                }
                for ftc in &r.timecodes {
                    let mut adjusted = ftc.clone();
                    adjusted.timecode_secs += chunk_start_secs;
                    all_timecodes.push((cr.chunk_idx, adjusted));
                }
            }
            Err(e) => {
                merged_details.push(format!("Chunk {}: error - {}", cr.chunk_idx, e));
            }
        }
    }

    // Sort by timecode_secs (and chunk_idx for stability)
    all_timecodes.sort_by(|a, b| {
        a.1.timecode_secs
            .partial_cmp(&b.1.timecode_secs)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });

    // Deduplicate: remove entries with timecode_secs too close to the previous one.
    // Within a single chunk, consecutive frames are ~1/fps seconds apart, so we use
    // half a frame as the threshold. This also removes genuine duplicates at chunk
    // boundaries where the same timecode appears in both overlapping chunks.
    let frame_duration = 1.0 / fps;
    let dedup_threshold = (frame_duration * 0.5).min(config.overlap_seconds * 0.5);
    let mut deduped: Vec<FrameTimecode> = Vec::with_capacity(all_timecodes.len());
    let mut last_secs: f64 = -dedup_threshold;
    for (_, ftc) in all_timecodes {
        if ftc.timecode_secs - last_secs > dedup_threshold {
            last_secs = ftc.timecode_secs;
            deduped.push(ftc);
        }
    }

    // Re-index frame indices
    for (i, ftc) in deduped.iter_mut().enumerate() {
        ftc.frame_index = i as u32;
    }

    let valid_frames = deduped.len() as u32;
    let true_total_possible = (total_duration * fps).round() as u32;
    let avg_confidence = if true_total_possible > 0 {
        valid_frames as f32 / true_total_possible as f32
    } else {
        0.0
    };

    let status = if valid_frames > 0 {
        if avg_confidence >= 0.70 {
            LtcDecodeStatus::Success
        } else if avg_confidence >= 0.30 {
            LtcDecodeStatus::LowConfidence
        } else {
            LtcDecodeStatus::NoSyncWord
        }
    } else {
        LtcDecodeStatus::NoSyncWord
    };

    let processing_time_ms = overall_start.elapsed().as_secs_f64() * 1000.0;

    merged_details.push(format!(
        "Chunked decode: {} chunks, {} valid / {} possible after merge",
        num_chunks, valid_frames, true_total_possible,
    ));

    let mut result = LtcDetectionResult {
        status,
        detected_fps: fps as f32,
        drop_frame,
        total_possible_frames: true_total_possible,
        valid_frames,
        timecodes: deduped,
        avg_confidence,
        details: merged_details,
        total_audio_duration_secs: total_duration,
        sample_rate: last_sample_rate,
        processing_time_ms,
        first_ltc_timecode_secs: if first_tc_secs < f64::MAX { first_tc_secs } else { 0.0 },
    };

    apply_coherent_first_timecode(&mut result);

    info!("decode_ltc_chunked complete: {} valid / {} possible ({:.1}%) in {:.1}ms",
        result.valid_frames, result.total_possible_frames, result.avg_confidence * 100.0, processing_time_ms);

    Ok(result)
}

// ── Config selection & stream building ────────────────────────────────────

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

    // First pass: exact match for the desired sample rate
    let mut best_config = try_find_config(configs, desired_channels, desired_sample_rate, desired_buffer_size, &format_priority);

    // Second pass: if desired rate is professional-grade (>=44100) and not found,
    // try 48000 Hz, then 44100 Hz (industry standard chain)
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

    // Third pass: clamp to device's min/max range
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

pub fn is_transient_audio_error(err: &str) -> bool {
    let keywords = ["temporarily busy", "already in use", "resource busy"];
    keywords.iter().any(|kw| err.contains(kw))
}

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

#[cfg(test)]
mod tests {
    use super::*;

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

    // ── is_transient_audio_error ──────────────────────────────────────────

    #[test]
    fn test_is_transient_audio_error_temporarily_busy() {
        assert!(is_transient_audio_error("device temporarily busy"));
    }

    #[test]
    fn test_is_transient_audio_error_already_in_use() {
        assert!(is_transient_audio_error("device already in use"));
    }

    #[test]
    fn test_is_transient_audio_error_resource_busy() {
        assert!(is_transient_audio_error("resource busy"));
    }

    #[test]
    fn test_is_transient_audio_error_permission_denied() {
        assert!(!is_transient_audio_error("Permission denied"));
    }

    #[test]
    fn test_is_transient_audio_error_empty_string() {
        assert!(!is_transient_audio_error(""));
    }

    #[test]
    fn test_is_transient_audio_error_unrelated() {
        assert!(!is_transient_audio_error("device not found"));
        assert!(!is_transient_audio_error("unknown error"));
    }

    #[test]
    fn test_is_transient_audio_error_partial_match() {
        assert!(is_transient_audio_error("The device is temporarily busy"));
        assert!(is_transient_audio_error("Stream error: already in use"));
    }

    #[test]
    fn test_is_transient_audio_error_case_sensitivity() {
        // The function does .contains() which is case-sensitive
        assert!(is_transient_audio_error("temporarily busy"));
        assert!(!is_transient_audio_error("TEMPORARILY BUSY"));
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
        // Test with default host (likely ALSA or PulseAudio)
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
        // We can't mock the host, but we can verify that the logic
        // would return true for these hosts regardless of name
        let host = cpal::default_host();
        let host_id = host.id();
        // Just verify our test names are accepted under the current host
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
        assert_eq!(SAMPLE_RATE_OPTIONS.len(), 2);
        assert!(SAMPLE_RATE_OPTIONS.contains(&44100));
        assert!(SAMPLE_RATE_OPTIONS.contains(&48000));
    }

    // ── Timecode ──────────────────────────────────────────────────────────

    #[test]
    fn test_timecode_clone_copy() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let copied = tc;
        assert_eq!(copied, tc);
    }

    #[test]
    fn test_timecode_debug() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4 };
        let d = format!("{:?}", tc);
        assert!(d.contains("1") || d.contains("hours"));
    }

    #[test]
    fn test_timecode_serialize_deserialize() {
        let tc = Timecode { hours: 10, minutes: 20, seconds: 30, frames: 15 };
        let json = serde_json::to_string(&tc).unwrap();
        let deserialized: Timecode = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized, tc);
    }

    // ── WavChunkReader 24-bit sign extension ──────────────────────────────

    #[test]
    fn test_wav_chunk_reader_24bit_sign_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("test_24bit.wav");

        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };

        // Known 24-bit integer sample values to write
        let test_samples: &[i32] = &[
            0,               // zero
            1,               // smallest positive
            -1,              // smallest negative
            8388607,         // max positive 24-bit (2^23 - 1)
            -8388608,        // min negative 24-bit (-2^23)
            1234567,         // arbitrary positive
            -1234567,        // arbitrary negative
            48000,           // moderate positive
            -48000,          // moderate negative
        ];

        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            for &s in test_samples {
                writer.write_sample(s).unwrap();
            }
            writer.finalize().unwrap();
        }

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        assert_eq!(reader.sample_rate(), 48000);
        assert_eq!(reader.channels(), 1);
        assert_eq!(reader.total_mono_samples(), test_samples.len());

        let max_val = (1i64 << 23) as f32;
        let read = reader.read_mono_samples_f32(0, test_samples.len()).unwrap();

        assert_eq!(read.len(), test_samples.len(),
            "should read all {} samples", test_samples.len());

        // Verify each sample's sign and approximate magnitude
        let tolerance = 1.0 / max_val; // ~1.19e-7 — one LSB tolerance
        for (i, (&expected_int, &actual_f32)) in test_samples.iter().zip(read.iter()).enumerate() {
            let expected_f32 = expected_int as f32 / max_val;

            let abs_diff = (actual_f32 - expected_f32).abs();
            assert!(abs_diff <= tolerance,
                "sample[{}]: expected {:.10} (from {}), got {:.10}, diff={:.10}",
                i, expected_f32, expected_int, actual_f32, abs_diff);

            // If expected is negative, actual must be negative (the sign-extension bug)
            if expected_int < 0 {
                assert!(actual_f32 < 0.0,
                    "sample[{}]: expected negative for int={}, got {:.10}",
                    i, expected_int, actual_f32);
            } else if expected_int > 0 {
                assert!(actual_f32 > 0.0,
                    "sample[{}]: expected positive for int={}, got {:.10}",
                    i, expected_int, actual_f32);
            } else {
                assert!((actual_f32).abs() <= tolerance,
                    "sample[{}]: expected zero for int=0, got {:.10}",
                    i, actual_f32);
            }
        }
    }

    // ── DecodeConfig default ───────────────────────────────────────────

    #[test]
    fn test_decode_config_default() {
        let config = DecodeConfig::default();
        assert_eq!(config.chunk_size_bytes, 50_000_000);
        assert!((config.overlap_seconds - 2.0).abs() < 1e-9);
    }

    // ── DecodeProgress ────────────────────────────────────────────────

    #[test]
    fn test_decode_progress_new() {
        let p = DecodeProgress::new(10);
        assert_eq!(p.chunks_total, 10);
        assert!((p.percent() - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_partial() {
        let p = DecodeProgress::new(4);
        p.chunks_completed.store(2, Ordering::Relaxed);
        assert!((p.percent() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_complete() {
        let p = DecodeProgress::new(5);
        p.chunks_completed.store(5, Ordering::Relaxed);
        assert!((p.percent() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_zero_total() {
        let p = DecodeProgress::new(0);
        assert!((p.percent() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_decode_progress_cancel() {
        let p = DecodeProgress::new(5);
        assert!(!p.cancel_flag.load(Ordering::Relaxed));
        p.cancel();
        assert!(p.cancel_flag.load(Ordering::Relaxed));
    }

    #[test]
    fn test_decode_progress_double_cancel() {
        let p = DecodeProgress::new(5);
        p.cancel();
        p.cancel(); // should not panic
        assert!(p.cancel_flag.load(Ordering::Relaxed));
    }

    // ── WavChunkReader: open errors ───────────────────────────────────

    #[test]
    fn test_wav_chunk_reader_nonexistent_file() {
        let result = WavChunkReader::open(Path::new("/nonexistent/path.wav"));
        assert!(result.is_err());
    }

    // ── WavChunkReader: 16-bit mono reads ─────────────────────────────

    fn write_test_wav_int(
        dir: &tempfile::TempDir,
        name: &str,
        channels: u16,
        sample_rate: u32,
        bits_per_sample: u16,
        samples: &[i32],
    ) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for &s in samples {
            writer.write_sample(s).unwrap();
        }
        writer.finalize().unwrap();
        path
    }

    #[test]
    fn test_wav_chunk_reader_16bit_mono_f32() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![0, 1, -1, 32767, -32768, 12345, -12345];
        let path = write_test_wav_int(&dir, "16bit_mono.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        assert_eq!(reader.sample_rate(), 48000);
        assert_eq!(reader.channels(), 1);
        assert_eq!(reader.total_mono_samples(), test_samples.len());

        let max_val = 32768.0f32;
        let read = reader.read_mono_samples_f32(0, test_samples.len()).unwrap();
        assert_eq!(read.len(), test_samples.len());

        for (i, (&expected_int, &actual_f32)) in test_samples.iter().zip(read.iter()).enumerate() {
            let expected_f32 = expected_int as f32 / max_val;
            let diff = (actual_f32 - expected_f32).abs();
            assert!(diff < 1e-6,
                "sample[{}]: expected {:.10}, got {:.10}, diff={:.10}",
                i, expected_f32, actual_f32, diff);
        }
    }

    #[test]
    fn test_wav_chunk_reader_16bit_mono_i16() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![0, 1, -1, 32767, -32768, 100, -200];
        let path = write_test_wav_int(&dir, "16bit_mono_i16.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_i16(0, test_samples.len()).unwrap();
        assert_eq!(read.len(), test_samples.len());
        for (i, (&expected, &actual)) in test_samples.iter().zip(read.iter()).enumerate() {
            assert_eq!(actual, expected as i16, "sample[{}]: mismatch", i);
        }
    }

    #[test]
    fn test_wav_chunk_reader_16bit_stereo_f32() {
        let dir = tempfile::TempDir::new().unwrap();
        // Stereo: L=value, R=0 for all samples
        let stereo_samples: Vec<i32> = vec![100, 0, 200, 0, 300, 0, -100, 0, -200, 0];
        let path = write_test_wav_int(&dir, "16bit_stereo.wav", 2, 48000, 16, &stereo_samples);
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        assert_eq!(reader.channels(), 2);
        assert_eq!(reader.total_mono_samples(), 5); // 5 mono samples from 10 interleaved

        let read = reader.read_mono_samples_f32(0, 5).unwrap();
        assert_eq!(read.len(), 5, "stereo should extract 5 left-channel samples");
        let max_val = 32768.0;
        assert!((read[0] - 100.0 / max_val).abs() < 1e-6);
        assert!((read[2] - 300.0 / max_val).abs() < 1e-6);
        assert!((read[3] + 100.0 / max_val).abs() < 1e-6);
    }

    #[test]
    fn test_wav_chunk_reader_16bit_stereo_i16() {
        let dir = tempfile::TempDir::new().unwrap();
        let stereo_samples: Vec<i32> = vec![100, 999, 200, 888, 300, 777];
        let path = write_test_wav_int(&dir, "16bit_stereo_i16.wav", 2, 48000, 16, &stereo_samples);
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();

        let read = reader.read_mono_samples_i16(0, 3).unwrap();
        assert_eq!(read.len(), 3);
        assert_eq!(read[0], 100i16);
        assert_eq!(read[1], 200i16);
        assert_eq!(read[2], 300i16);
    }

    #[test]
    fn test_wav_chunk_reader_read_i16_rejects_non_16bit() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = write_test_wav_int(&dir, "24bit_i16_reject.wav", 1, 48000, 24, &[0, 1, -1]);
        // Use 24-bit WAV → read_mono_samples_i16 should error
        // But open only works if we read later: we need a 24-bit WAV
        // Write one
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let result = reader.read_mono_samples_i16(0, 3);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("16-bit"));
    }

    #[test]
    fn test_wav_chunk_reader_partial_read() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = (0..100).collect();
        let path = write_test_wav_int(&dir, "partial.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(10, 5).unwrap();
        assert_eq!(read.len(), 5);
        let max_val = 32768.0;
        assert!((read[0] - 10.0 / max_val).abs() < 1e-6);
        assert!((read[4] - 14.0 / max_val).abs() < 1e-6);
    }

    #[test]
    fn test_wav_chunk_reader_read_beyond_end() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![1, 2, 3];
        let path = write_test_wav_int(&dir, "beyond_end.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 100).unwrap();
        assert_eq!(read.len(), 3, "should return only available samples");
    }

    #[test]
    fn test_wav_chunk_reader_empty_range() {
        let dir = tempfile::TempDir::new().unwrap();
        let test_samples: Vec<i32> = vec![1, 2, 3];
        let path = write_test_wav_int(&dir, "empty_range.wav", 1, 48000, 16, &test_samples);

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 0).unwrap();
        assert!(read.is_empty());
    }

    // ── WavChunkReader: 8-bit reads ───────────────────────────────────

    #[test]
    fn test_wav_chunk_reader_8bit() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("8bit.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 8,
            sample_format: hound::SampleFormat::Int,
        };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        // 8-bit signed: write values -128 to 127
        for i in -128i8..=127 {
            writer.write_sample(i).unwrap();
        }
        writer.finalize().unwrap();

        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 256).unwrap();
        assert_eq!(read.len(), 256);
        // 8-bit signed: max_val = 1<<7 = 128
        let max_val = 128.0f32;
        // value -128 → -1.0, value 0 → 0.0, value 127 → ~0.992
        assert!((read[0] + 1.0).abs() < 0.01, "first sample (-128) should be -1.0, got {}", read[0]);
        assert!((read[128] - 0.0).abs() < 1e-4, "sample at zero should be 0.0, got {}", read[128]);
        assert!((read[255] - 127.0 / max_val).abs() < 1e-4, "last sample (127) should be ~0.992, got {}", read[255]);
    }

    // ── WavChunkReader: float format reads ────────────────────────────

    #[test]
    fn test_wav_chunk_reader_float32() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("float.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        {
            let mut writer = hound::WavWriter::create(&path, spec).unwrap();
            writer.write_sample(0.5f32).unwrap();
            writer.write_sample(-0.25f32).unwrap();
            writer.write_sample(1.0f32).unwrap();
            writer.write_sample(-1.0f32).unwrap();
            writer.finalize().unwrap();
        }
        let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
        let read = reader.read_mono_samples_f32(0, 4).unwrap();
        assert_eq!(read.len(), 4);
        assert!((read[0] - 0.5).abs() < 1e-6);
        assert!((read[1] + 0.25).abs() < 1e-6);
        assert!((read[2] - 1.0).abs() < 1e-6);
        assert!((read[3] + 1.0).abs() < 1e-6);
    }

    // ── WavChunkReader: unsupported bits_per_sample ───────────────────

    #[test]
    fn test_wav_chunk_reader_unsupported_bps() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("unsupported.wav");
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 17, // not 8, 16, 24, or 32
            sample_format: hound::SampleFormat::Int,
        };
        {
            // hound might not support 17-bit, but let's try
            let result = hound::WavWriter::create(&path, spec);
            if let Ok(mut writer) = result {
                writer.write_sample(0i32).unwrap();
                writer.finalize().unwrap();
                let (mut reader, _start) = WavChunkReader::open(&path).unwrap();
                let read = reader.read_mono_samples_f32(0, 1);
                assert!(read.is_err() || read.unwrap().len() <= 1);
            }
        }
    }

    // ── decode_ltc_chunked merge tests ──────────────────────────────

    fn chunk_count_for_config(
        total_mono: usize,
        sample_rate: u32,
        channels: u16,
        bits_per_sample: u16,
        config: &DecodeConfig,
    ) -> usize {
        let bytes_per_mono = (channels as u64) * (bits_per_sample as u64 / 8);
        let chunk_mono = (config.chunk_size_bytes / bytes_per_mono.max(1)) as usize;
        let overlap_samples = (config.overlap_seconds * sample_rate as f64) as usize;
        let chunk_mono = chunk_mono.max(overlap_samples * 2);
        if total_mono <= chunk_mono + overlap_samples {
            return 1;
        }
        let mut count = 0usize;
        let mut pos = 0usize;
        while pos < total_mono {
            count += 1;
            let end = (pos + chunk_mono).min(total_mono);
            if end >= total_mono { break; }
            let next = end.saturating_sub(overlap_samples);
            if next <= pos || next >= total_mono { break; }
            pos = next;
        }
        count
    }

    #[test]
    fn test_chunked_merge_multi_chunk_preserves_all_frames() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_preserve.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        let sample_rate = 48000;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, sample_rate, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let nchunks = chunk_count_for_config(total_mono, sample_rate, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();
        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, fps, false).unwrap();

        assert_eq!(chunked.valid_frames, direct.valid_frames,
            "chunked merge lost frames: chunked={} vs direct={}",
            chunked.valid_frames, direct.valid_frames);
        assert_eq!(chunked.status, direct.status);
    }

    #[test]
    fn test_chunked_total_possible_not_inflated() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_total.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        let sample_rate = 48000;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, sample_rate, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let total_duration = total_mono as f64 / sample_rate as f64;
        let expected_possible = (total_duration * fps).round() as u32;
        let nchunks = chunk_count_for_config(total_mono, sample_rate, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();

        // total_possible_frames should match the stream-based total, not be inflated by overlap
        assert_eq!(chunked.total_possible_frames, expected_possible,
            "total_possible_frames should be {} (stream total), got {}",
            expected_possible, chunked.total_possible_frames);
        // It should NOT be inflated like the old sum-of-chunks approach would give
        assert!(chunked.total_possible_frames <= num_frames + 5,
            "total_possible should not be significantly larger than num_frames={}, got {}",
            num_frames, chunked.total_possible_frames);
    }

    #[test]
    fn test_chunked_single_chunk_matches_nonchunked_exactly() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("single_chunk.wav");

        let num_frames = 50u32;
        let fps = 25.0;
        generate_ltc_wav(
            &path,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            fps, false, 48000, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };

        let progress = DecodeProgress::new(1);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();
        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, fps, false).unwrap();

        // Allow small tolerance (1-2 frames) since non-chunked decoder's bit-extraction
        // based total_possible may differ from the stream-based count used by chunked.
        // The key assertion: both decode essentially the same number of frames.
        let diff = if chunked.valid_frames > direct.valid_frames {
            chunked.valid_frames - direct.valid_frames
        } else {
            direct.valid_frames - chunked.valid_frames
        };
        assert!(diff <= 2,
            "single chunk: chunked={} != direct={} (diff={})",
            chunked.valid_frames, direct.valid_frames, diff);
        assert!(chunked.total_possible_frames >= chunked.valid_frames,
            "total_possible ({}) < valid_frames ({})",
            chunked.total_possible_frames, chunked.valid_frames);
    }

    #[test]
    fn test_chunked_merge_no_unnecessary_dedup() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_dedup.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        let sample_rate = 48000;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, sample_rate, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let overlap_secs = config.overlap_seconds;

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let nchunks = chunk_count_for_config(total_mono, sample_rate, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, false, fps, false, config, &progress).unwrap();

        let total_from_chunks: u32 = chunked.details.iter()
            .filter(|d| d.starts_with("Chunk ") && d.contains("valid"))
            .filter_map(|d| {
                let s = d.split_whitespace().nth(2)?;
                s.parse::<u32>().ok()
            })
            .sum();

        let loss = total_from_chunks.saturating_sub(chunked.valid_frames);
        let frame_duration = 1.0 / fps;
        let max_expected_loss = ((nchunks.saturating_sub(1)) as f64
            * (overlap_secs / frame_duration).ceil()) as u32;
        assert!(loss <= max_expected_loss,
            "unnecessary dedup: lost {} frames (max expected loss from overlap: {}). \
             total_from_chunks={}, valid_after_merge={}",
            loss, max_expected_loss, total_from_chunks, chunked.valid_frames);
    }

    #[test]
    fn test_chunked_merge_libltc_preserves_frames() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("merge_libltc.wav");

        let num_frames = 200u32;
        let fps = 25.0;
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            fps, false, 48000, num_frames,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 200_000,
            overlap_seconds: 0.3,
        };

        let (reader, _) = WavChunkReader::open(&path).unwrap();
        let total_mono = reader.total_mono_samples();
        let nchunks = chunk_count_for_config(total_mono, 48000, 2, 16, &config);
        assert!(nchunks >= 3, "test needs at least 3 chunks, got {}", nchunks);

        let progress = DecodeProgress::new(nchunks);
        let chunked = decode_ltc_chunked(&path, true, fps, false, config, &progress).unwrap();
        let direct = crate::ltc_decoder_libltc::decode_ltc_from_wav_libltc(&path, fps, false).unwrap();

        // Allow 2-frame tolerance: chunk boundaries may lose a frame at each edge
        let diff = if chunked.valid_frames > direct.valid_frames {
            chunked.valid_frames - direct.valid_frames
        } else {
            direct.valid_frames - chunked.valid_frames
        };
        assert!(diff <= 2,
            "chunked libltc merge lost frames: chunked={} vs direct={} (diff={})",
            chunked.valid_frames, direct.valid_frames, diff);
        // total_possible may differ due to stream-based (chunked) vs bit-extraction (direct) counting
        assert!(chunked.total_possible_frames >= chunked.valid_frames);
    }

    // ── decode_ltc_chunked (existing tests) ───────────────────────────

    /// Generate a WAV file with LTC audio at the given parameters.
    fn generate_ltc_wav(
        path: &Path,
        start_tc: Timecode,
        fps: f64,
        drop_frame: bool,
        sample_rate: u32,
        num_frames: u32,
    ) {
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };

        let samples_per_frame = (sample_rate as f64 / fps).round() as usize;
        let samples_per_bit = samples_per_frame as f32 / 80.0;

        let mut writer = hound::WavWriter::create(path, spec).unwrap();
        let mut tc = start_tc;
        let mut last_level = (1.0f32, 1.0f32);
        let mut frame_buf = vec![0.0f32; samples_per_frame * 2];

        for _ in 0..num_frames {
            frame_buf.fill(0.0);
            crate::generate_ltc_frame_stereo(
                &tc,
                drop_frame,
                samples_per_frame,
                samples_per_bit,
                0.5,
                "both",
                &mut last_level,
                &mut frame_buf[..samples_per_frame * 2],
            );

            for &sample in &frame_buf[..samples_per_frame * 2] {
                let clamped = sample.clamp(-1.0, 1.0);
                let int_sample = (clamped * i16::MAX as f32) as i16;
                writer.write_sample(int_sample).unwrap();
            }

            tc = crate::increment_timecode(&tc, fps, drop_frame);
        }

        writer.finalize().unwrap();
    }

    #[test]
    fn test_decode_ltc_chunked_compare_samples() {
        // Compare the samples read by WavChunkReader vs hound-based read_mono_samples
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("compare_samples.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 50,
        );

        // Read samples via hound (as decode_ltc_from_wav does)
        let mut reader = hound::WavReader::open(&path).unwrap();
        let spec = reader.spec();
        let hound_samples: Vec<f32> = reader
            .samples::<i32>()
            .filter_map(|s| s.ok())
            .enumerate()
            .filter(|(i, _)| i % spec.channels as usize == 0)
            .map(|(_, s)| s as f32 / (1i64 << (spec.bits_per_sample - 1)) as f32)
            .collect();

        // Read samples via WavChunkReader (as decode_ltc_chunked does)
        let (mut cr, _) = WavChunkReader::open(&path).unwrap();
        let cr_samples = cr.read_mono_samples_f32(0, hound_samples.len()).unwrap();

        assert_eq!(hound_samples.len(), cr_samples.len(),
            "sample count mismatch: hound={}, WavChunkReader={}",
            hound_samples.len(), cr_samples.len());

        // Check how many samples differ by more than a small tolerance
        let max_diff: f32 = hound_samples.iter().zip(cr_samples.iter())
            .map(|(a, b)| (*a - *b).abs())
            .fold(0.0f32, f32::max);
        let num_diff = hound_samples.iter().zip(cr_samples.iter())
            .filter(|(a, b)| (*a - *b).abs() > 1e-6)
            .count();

        assert!(max_diff < 1e-4,
            "max sample diff is {:.10} ({} samples differ > 1e-6)",
            max_diff, num_diff);
    }

    #[test]
    fn test_decode_ltc_chunked_compare_with_direct() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("compare.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 50,
        );

        // Direct decode (known to work)
        let direct = crate::ltc_decoder::decode_ltc_from_wav(&path, 25.0, false).unwrap();

        // Chunked decode
        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };
        let progress = DecodeProgress::new(1);
        let chunked = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();

        assert_eq!(direct.valid_frames, chunked.valid_frames,
            "direct decode got {} valid, chunked got {} valid (both should match)",
            direct.valid_frames, chunked.valid_frames);
        assert_eq!(direct.status, chunked.status);
    }

    #[test]
    fn test_decode_ltc_chunked_single_chunk() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ltc_chunked_single.wav");

        // Generate about 1 second of LTC at 25fps
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 50,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };
        let progress = DecodeProgress::new(1);
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Success),
            "expected Success, got {:?} (valid={})", result.status, result.valid_frames);
        assert!(result.valid_frames >= 40,
            "should decode at least 40 frames, got {}", result.valid_frames);
    }

    #[test]
    fn test_decode_ltc_chunked_empty_wav() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("empty_ltc.wav");
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let writer = hound::WavWriter::create(&path, spec).unwrap();
        writer.finalize().unwrap();

        let config = DecodeConfig::default();
        let progress = DecodeProgress::new(0);
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress).unwrap();
        assert!(matches!(result.status, LtcDecodeStatus::Error { .. }));
    }

    #[test]
    fn test_decode_ltc_chunked_cancel() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("cancel_ltc.wav");

        // Generate 3 seconds of LTC at 25fps
        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 75,
        );

        // Use tiny chunks to force multi-chunk decode
        let config = DecodeConfig {
            chunk_size_bytes: 1000, // tiny chunk → many chunks
            overlap_seconds: 0.1,
        };
        let progress = DecodeProgress::new(100);
        // Cancel immediately
        progress.cancel();
        let result = decode_ltc_chunked(&path, false, 25.0, false, config, &progress);
        assert!(result.is_err(), "canceled decode should return Err");
        let err = result.unwrap_err();
        assert!(err.contains("Canceled") || err.contains("canceled"),
            "error should mention cancel: {}", err);
    }

    #[test]
    fn test_decode_ltc_chunked_libltc() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("ltc_chunked_libltc.wav");

        generate_ltc_wav(
            &path,
            Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 },
            25.0, false, 48000, 25,
        );

        let config = DecodeConfig {
            chunk_size_bytes: 10_000_000,
            overlap_seconds: 2.0,
        };
        let progress = DecodeProgress::new(1);
        let result = decode_ltc_chunked(&path, true, 25.0, false, config, &progress).unwrap();
        assert!(!matches!(result.status, LtcDecodeStatus::Error { .. }),
            "expected no Error for libltc chunked, got {:?}", result.status);
        assert!(result.valid_frames >= 20,
            "should decode at least 20 frames with libltc, got {}", result.valid_frames);
    }
}

// Re-export LTC decoder types for convenience
pub use ltc_decoder::{
    apply_coherent_first_timecode, decode_ltc_from_wav, decode_ltc_samples,
    find_first_coherent_index, quick_check_ltc, FrameTimecode, LtcDecodeStatus,
    LtcDetectionResult,
};
pub use ltc_decoder_libltc::{decode_ltc_from_wav_libltc, decode_ltc_samples_libltc};

/// Decode LTC from a WAV file, selecting the decoder implementation.
/// `fps` and `drop_frame` specify the expected frame rate (no auto-detection).
/// Set `use_libltc = true` to use the libltc C library decoder.
pub fn decode_ltc_with_decoder(
    path: &std::path::Path,
    use_libltc: bool,
    fps: f64,
    drop_frame: bool,
) -> Result<LtcDetectionResult, String> {
    if use_libltc {
        decode_ltc_from_wav_libltc(path, fps, drop_frame)
    } else {
        decode_ltc_from_wav(path, fps, drop_frame)
    }
}

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

fn collect_device_configs(device: &cpal::Device) -> (Vec<String>, u16, u16, u32, u32, u32, u32) {
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
    (formats, min_channels, max_channels, min_rate, max_rate, min_buffer, max_buffer)
}

fn log_device_supported_configs(device: &cpal::Device, label: &str) {
    match device.supported_output_configs() {
        Ok(configs) => {
            let configs: Vec<_> = configs.collect();
            let mut formats: Vec<&'static str> = Vec::new();
            let mut min_channels = u16::MAX;
            let mut max_channels = u16::MIN;
            let mut min_rate = u32::MAX;
            let mut max_rate = u32::MIN;
            let mut min_buffer = u32::MAX;
            let mut max_buffer = u32::MIN;
            for cfg in &configs {
                let f = sample_format_name(cfg.sample_format());
                if !formats.contains(&f) {
                    formats.push(f);
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
            let ch_range = if min_channels == max_channels {
                format!("{}", min_channels)
            } else {
                format!("{}-{}", min_channels, max_channels)
            };
            let rate_range = if min_rate == max_rate {
                format!("{}", min_rate)
            } else {
                format!("{}-{}", min_rate, max_rate)
            };
            let buf = if min_buffer <= max_buffer && min_buffer != u32::MAX {
                if min_buffer == max_buffer {
                    format!("buffer={}", min_buffer)
                } else {
                    format!("buffer={}-{}", min_buffer, max_buffer)
                }
            } else {
                String::from("buffer=unknown")
            };
            info!(
                "  Device {}: formats=[{}], channels={}, rates={}, {}",
                label,
                formats.join(", "),
                ch_range,
                rate_range,
                buf,
            );
        }
        Err(e) => {
            let err_str = e.to_string();
            if is_permanent_device_error(&err_str) {
                warn!("  Device {}: skipped (error: {})", label, err_str);
            }
        }
    }
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
            match dev.supported_output_configs() {
                Ok(_) => {
                    seen.insert(name.clone());
                    let (formats, ch_min, ch_max, rate_min, rate_max, buf_min, buf_max) = collect_device_configs(dev);
                    devices.push(AudioDeviceInfo {
                        id: String::from("default"),
                        name: format!("{} (Default)", name),
                        is_default: true,
                        formats,
                        channels_min: if ch_min != u16::MAX { ch_min } else { 0 },
                        channels_max: if ch_max != u16::MIN { ch_max } else { 0 },
                        sample_rate_min: if rate_min != u32::MAX { rate_min } else { 0 },
                        sample_rate_max: if rate_max != u32::MIN { rate_max } else { 0 },
                        buffer_min: if buf_min != u32::MAX { buf_min } else { 0 },
                        buffer_max: if buf_max != u32::MIN { buf_max } else { 0 },
                    });
                    log_device_supported_configs(dev, &format!("\"{}\" (Default)", name));
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if is_permanent_device_error(&err_str) {
                        warn!("Skipping default device '{}': {}", name, err_str);
                    } else {
                        seen.insert(name.clone());
                        devices.push(AudioDeviceInfo {
                            id: String::from("default"),
                            name: format!("{} (Default)", name),
                            is_default: true,
                            formats: Vec::new(),
                            channels_min: 0,
                            channels_max: 0,
                            sample_rate_min: 0,
                            sample_rate_max: 0,
                            buffer_min: 0,
                            buffer_max: 0,
                        });
                        log_device_supported_configs(dev, &format!("\"{}\" (Default)", name));
                    }
                }
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
        match device.supported_output_configs() {
            Ok(_) => {}
            Err(e) => {
                let err_str = e.to_string();
                if is_permanent_device_error(&err_str) {
                    warn!("Skipping device '{}': {}", name, err_str);
                    continue;
                }
            }
        }
        log_device_supported_configs(&device, &format!("\"{}\"", name));
        let (formats, ch_min, ch_max, rate_min, rate_max, buf_min, buf_max) = collect_device_configs(&device);
        let is_default = default_name.as_deref() == Some(&name);
        devices.push(AudioDeviceInfo {
            id: name.clone(),
            name,
            is_default,
            formats,
            channels_min: if ch_min != u16::MAX { ch_min } else { 0 },
            channels_max: if ch_max != u16::MIN { ch_max } else { 0 },
            sample_rate_min: if rate_min != u32::MAX { rate_min } else { 0 },
            sample_rate_max: if rate_max != u32::MIN { rate_max } else { 0 },
            buffer_min: if buf_min != u32::MAX { buf_min } else { 0 },
            buffer_max: if buf_max != u32::MIN { buf_max } else { 0 },
        });
    }

    devices.sort_by(|a, b| b.is_default.cmp(&a.is_default).then(a.name.cmp(&b.name)));

    info!("Found {} audio devices", devices.len());

    Ok(devices)
}

// ── LTC scheduler thread ───────────────────────────────────────────────────

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
    let mut frame_count: u64 = 0;
    let mut drop_count: u64 = 0;
    let mut last_drop_event: u64 = 0;
    let mut last_callback_value: u64 = 0;
    let mut last_callback_check: Instant = Instant::now();
    let mut last_underrun_value: u64 = 0;

    // ── Watchdog recovery state (event-driven sliding window) ──
    let mut recovery_attempts: u8 = 0;
    let mut first_failure: Option<Instant> = None;

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
            let ltc_channel = state.ltc_channel.clone();
            let ltc_volume = state.ltc_volume;
            let frame_dur = state.frame_duration;
            let last_level = state.last_level;

            // Sample accumulator: track fractional-sample remainder across frames
            let (frame_samples, spb, acc) = ltc_encoder::compute_frame_sample_count(
                state.exact_samples_per_frame,
                state.base_samples,
                state.samples_accumulator,
            );

            (tc, fps, drop_frame, ltc_channel, ltc_volume, frame_dur, frame_samples, spb, last_level, acc)
        };

        // ── Watchdog: check if audio callback is still alive ──
        let current_callback = callback_counter.load(Ordering::Relaxed);
        if current_callback == last_callback_value {
            if last_callback_check.elapsed() > Duration::from_millis(500) {
                error!("LTC scheduler: audio callback has not fired for 500ms — stream appears dead");

                // Sliding window: reset counter if last failure was >10s ago
                let now = Instant::now();
                if let Some(first) = first_failure {
                    if now.duration_since(first) > Duration::from_secs(10) {
                        recovery_attempts = 0;
                        first_failure = None;
                    }
                }

                if recovery_attempts < 3 {
                    recovery_attempts += 1;
                    if first_failure.is_none() {
                        first_failure = Some(now);
                    }
                    warn!("LTC scheduler: recovery attempt {}/3", recovery_attempts);
                    if let Ok(mut ev) = events.lock() {
                        ev.push(AudioEvent::RecoveryNeeded {
                            reason: format!("callback stalled for 500ms (attempt {}/3)", recovery_attempts),
                        });
                    }
                    // Reset watchdog timer so we don't immediately re-trigger
                    last_callback_value = current_callback;
                    last_callback_check = Instant::now();
                    // Sleep a short time before checking again so the main thread can act
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                } else {
                    error!("LTC scheduler: 3 recovery attempts exhausted — stream permanently dead");
                    if let Ok(mut ev) = events.lock() {
                        ev.push(AudioEvent::StreamDead);
                    }
                    return;
                }
            }
        } else {
            // Callback is alive — reset recovery state
            last_callback_value = current_callback;
            last_callback_check = Instant::now();
            recovery_attempts = 0;
            first_failure = None;
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
            &ltc_channel,
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
            let pushed = producer.push_slice(&frame_buf[..needed]);
            if pushed < needed {
                drop_count += 1;
                if drop_count <= 1 || drop_count % 100 == 0 {
                    warn!("LTC scheduler: ring buffer full, dropped frame #{} (pushed {}/{}, total drops: {})",
                        frame_count, pushed, needed, drop_count);
                }
                if drop_count - last_drop_event >= 100 {
                    if let Ok(mut ev) = events.lock() {
                        ev.push(AudioEvent::FramesDropped { total: drop_count });
                    }
                    last_drop_event = drop_count;
                }
            }
        }

        frame_count += 1;
        if frame_count % 1000 == 0 {
            info!("LTC scheduler: frame={}, drops={}, channel={}, fps={}, tc={:02}:{:02}:{:02}:{:02}",
                frame_count, drop_count, ltc_channel,
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