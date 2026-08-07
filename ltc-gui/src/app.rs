use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audio_core::{list_audio_devices, AudioCore, AudioDeviceInfo, AudioEvent, Timecode};
use egui::{Color32, FontId, RichText, Sense, Ui};
use log::{error, info, trace, warn};

use crate::log_buffer::LogBuffer;
use crate::theme::{Theme, ACCENT};
use crate::widgets;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUFFER_SIZE: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChannel {
    Left,
    Right,
    Both,
}

impl AudioChannel {
    pub fn as_str(self) -> &'static str {
        match self {
            AudioChannel::Left => "left",
            AudioChannel::Right => "right",
            AudioChannel::Both => "both",
        }
    }

    pub fn all() -> &'static [AudioChannel] {
        &[AudioChannel::Left, AudioChannel::Right, AudioChannel::Both]
    }

    pub fn label(self) -> &'static str {
        match self {
            AudioChannel::Left => "Left",
            AudioChannel::Right => "Right",
            AudioChannel::Both => "Both",
        }
    }
}

#[derive(Clone, Debug)]
pub struct FrameRateOption {
    pub name: &'static str,
    pub fps: f64,
    pub drop_frame: bool,
    pub description: &'static str,
}

pub const FRAME_RATE_OPTIONS: &[FrameRateOption] = &[
    FrameRateOption {
        name: "24 fps",
        fps: 24.0,
        drop_frame: false,
        description: "Standard cinema & film frame rate.",
    },
    FrameRateOption {
        name: "25 fps",
        fps: 25.0,
        drop_frame: false,
        description: "PAL standard (Europe, UK, Australia, Africa, Asia).",
    },
    FrameRateOption {
        name: "29.97 ND",
        fps: 29.97,
        drop_frame: false,
        description: "NTSC Non-Drop (broadcast video & web production).",
    },
    FrameRateOption {
        name: "29.97 DF",
        fps: 29.97,
        drop_frame: true,
        description: "NTSC Drop Frame (syncs clock drift to wall-time).",
    },
    FrameRateOption {
        name: "30 fps",
        fps: 30.0,
        drop_frame: false,
        description: "High-definition video rate / digital audio standard.",
    },
];

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct ClapLogItem {
    pub id: String,
    pub timestamp: String,
    pub timecode: String,
    pub milliseconds: String,
    pub note: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Clapper,
    Settings,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Clapper => "Clapper Slate & Logs",
            Tab::Settings => "Signal & Audio Settings",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationType {
    Error,
    Warning,
    Success,
    Info,
}

pub struct ToastNotification {
    pub id: u64,
    pub message: String,
    pub notification_type: NotificationType,
    pub created_at: Instant,
    pub duration: Duration,
}

/// Main application state, equivalent to the React `App` component.
pub struct AppState {
    pub theme: Theme,

    // Playback state
    pub is_playing: bool,
    pub is_locked: bool,
    pub start_timecode: Timecode,
    pub current_timecode: Timecode,
    pub fps_index: usize,

    // Audio settings
    pub sample_rate: u32,
    pub sample_rate_index: usize,
    pub ltc_channel: AudioChannel,
    pub beep_channel: AudioChannel,
    pub ltc_volume: f32,
    pub beep_volume: f32,
    pub beep_frequency: f32,
    pub beep_duration: f32,

    // Clapper state
    pub scene: u32,
    pub take: u32,
    pub roll: String,
    pub auto_increment_take: bool,
    pub logs: Vec<ClapLogItem>,
    pub clap_flash_alpha: f32,
    pub clap_arm_angle: f32,

    // Tab state
    pub active_tab: Tab,
    pub show_faq: bool,

    // Device state
    pub devices: Vec<AudioDeviceInfo>,
    pub selected_device: usize,
    pub previous_device: Option<usize>,
    pub audio_initialized: bool,
    pub recovery_attempt_count: u8,

    // Toast notifications
    pub notifications: Vec<ToastNotification>,
    next_notification_id: u64,

    // Status
    pub status_message: String,
    pub system_time: String,

    // Frame-rate independent animation
    last_frame_time: Option<Instant>,

    // Flag so we only maximise app once
    has_requested_maximize: bool,

    // Audio core
    pub audio_core: Mutex<AudioCore>,
    pub sample_format_name: String,

    // Debug log
    pub show_debug_log: bool,
    pub show_app_menu: bool,
    pub app_menu_pos: Option<egui::Pos2>,
    pub log_buffer: Arc<Mutex<LogBuffer>>,
}

impl AppState {
    pub fn new(log_buffer: Arc<Mutex<LogBuffer>>) -> Self {
        let start_tc = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let default_rate = audio_core::suggest_sample_rate();
        let rate_index = audio_core::SAMPLE_RATE_OPTIONS
            .iter()
            .position(|&r| r == default_rate)
            .unwrap_or(0);
        Self {
            theme: Theme::Light,
            is_playing: false,
            is_locked: false,
            start_timecode: start_tc,
            current_timecode: start_tc,
            fps_index: 1, // 25 fps PAL
            sample_rate: default_rate,
            sample_rate_index: rate_index,
            ltc_channel: AudioChannel::Left,
            beep_channel: AudioChannel::Right,
            ltc_volume: 0.25,
            beep_volume: 0.5,
            beep_frequency: 1000.0,
            beep_duration: 0.5,
            scene: 1,
            take: 1,
            roll: "A001".to_string(),
            auto_increment_take: true,
            logs: Vec::new(),
            clap_flash_alpha: 0.0,
            clap_arm_angle: -25.0,
            active_tab: Tab::Clapper,
            show_faq: false,
            devices: Vec::new(),
            selected_device: 0,
            previous_device: None,
            audio_initialized: false,
            recovery_attempt_count: 0,
            notifications: Vec::new(),
            next_notification_id: 0,
            status_message: "Ready".to_string(),
            system_time: String::new(),
            last_frame_time: None,
            has_requested_maximize: false,
            audio_core: Mutex::new(AudioCore::new()),
            sample_format_name: String::new(),
            show_debug_log: false,
            show_app_menu: false,
            app_menu_pos: None,
            log_buffer,
        }
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self::new(Arc::new(Mutex::new(LogBuffer::new(1000))))
    }
}

impl AppState {
    pub fn fps(&self) -> &FrameRateOption {
        &FRAME_RATE_OPTIONS[self.fps_index]
    }

    pub fn refresh_devices(&mut self) {
        match list_audio_devices() {
            Ok(devs) => {
                self.devices = devs;
                if self.selected_device >= self.devices.len() {
                    self.selected_device = 0;
                }
                info!("Refreshed devices: found {} audio devices", self.devices.len());
                self.status_message = format!("{} devices found", self.devices.len());
            }
            Err(e) => {
                error!("Failed to list audio devices: {}", e);
                self.status_message = format!("Device error: {}", e);
            }
        }
    }

    pub fn change_device(&mut self, index: usize) {
        if index >= self.devices.len() {
            warn!("change_device: index {} out of range ({} devices)", index, self.devices.len());
            return;
        }
        let device_name = self.devices[index].name.clone();
        let device_id = self.devices[index].id.clone();
        info!("Changing audio device to [{}] {} (id={})", index, device_name, device_id);

        let prev_selected = self.previous_device;
        let was_playing = self.is_playing;
        if was_playing {
            info!("Device change: stopping LTC stream first");
            self.stop_streaming();
        }

        if self.audio_initialized {
            info!("Device change: stopping existing audio output");
            let core = match self.audio_core.lock() {
                Ok(c) => c,
                Err(e) => {
                    error!("change_device: audio core mutex poisoned: {}", e);
                    self.status_message = "Device change failed: mutex poisoned".to_string();
                    return;
                }
            };
            if let Err(e) = core.stop_output() {
                error!("change_device: stop_output failed: {}", e);
            }
            drop(core);
            self.audio_initialized = false;
            std::thread::sleep(Duration::from_millis(50));
        }

        self.selected_device = index;
        self.recovery_attempt_count = 0;
        self.status_message = format!("Device changed to: {}", device_name);

        self.ensure_audio_init();

        if self.audio_initialized {
            self.previous_device = Some(self.selected_device);
            if was_playing {
                info!("Device change: restarting LTC stream");
                let tc = self.start_timecode;
                let fps = self.fps();
                let channel = self.ltc_channel.as_str().to_string();
                let volume = self.ltc_volume;
                let core = match self.audio_core.lock() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("change_device: audio core mutex poisoned on restart: {}", e);
                        self.status_message = "Device change: failed to restart stream".to_string();
                        return;
                    }
                };
                let restart_result = core.start_ltc(tc, fps.fps, fps.drop_frame, channel, volume);
                drop(core);
                match restart_result {
                    Ok(()) => {
                        self.is_playing = true;
                        self.status_message = format!("Streaming LTC on {}", device_name);
                        info!("LTC stream restarted on new device: {}", device_name);
                    }
                    Err(e) => {
                        error!("Failed to restart LTC on new device: {}", e);
                        self.status_message = format!("Restart failed after device change: {}", e);
                        self.add_notification(NotificationType::Error, format!("Failed to restart LTC stream: {}", e));
                    }
                }
            }
        } else {
            // Initialization failed — revert to previous device
            let fallback = prev_selected.unwrap_or(0);
            let fallback_name = if fallback < self.devices.len() {
                self.devices[fallback].name.clone()
            } else {
                "default".to_string()
            };
            self.selected_device = fallback;
            self.status_message = format!("Audio device '{}' failed. Reverted to '{}'", device_name, fallback_name);
            self.add_notification(
                NotificationType::Error,
                format!("Audio device '{}' failed to initialize. Reverted to '{}'.", device_name, fallback_name),
            );
        }
    }

    pub fn ensure_audio_init(&mut self) {
        if self.audio_initialized {
            return;
        }
        let device_id = if self.devices.is_empty() {
            "default".to_string()
        } else {
            self.devices[self.selected_device].id.clone()
        };

        let device_name = if self.devices.is_empty() {
            "default".to_string()
        } else {
            self.devices[self.selected_device].name.clone()
        };

        info!("Initializing audio on device: {} (id={})", device_name, device_id);

        let max_retries = 3;
        let mut delay_ms = 50u64;

        for attempt in 0..max_retries {
            let result = {
                let core = match self.audio_core.lock() {
                    Ok(c) => c,
                    Err(e) => {
                        error!("ensure_audio_init: audio core mutex poisoned: {}", e);
                        return;
                    }
                };
                core.init_output(&device_id, self.sample_rate, BUFFER_SIZE)
            };

            match result {
                Ok(actual_rate) => {
                    let fmt = {
                        let core = match self.audio_core.lock() {
                            Ok(c) => c,
                            Err(e) => {
                                error!("ensure_audio_init: audio core mutex poisoned after init: {}", e);
                                return;
                            }
                        };
                        let fmt = core.sample_format_name();
                        self.sample_format_name = fmt.clone();
                        fmt
                    };
                    self.audio_initialized = true;
                    if actual_rate != self.sample_rate {
                        warn!(
                            "Sample rate overridden: requested {} Hz, device uses {} Hz",
                            self.sample_rate, actual_rate
                        );
                        self.add_notification(
                            NotificationType::Warning,
                            format!(
                                "Sample rate overridden: {} Hz not supported by device, using {} Hz",
                                self.sample_rate, actual_rate
                            ),
                        );
                        self.sample_rate = actual_rate;
                        self.sample_rate_index = audio_core::SAMPLE_RATE_OPTIONS
                            .iter()
                            .position(|&r| r == actual_rate)
                            .unwrap_or(0);
                    }
                    self.status_message = format!("Audio initialized ({})", fmt);
                    info!("Audio initialized successfully on {} (format={}, sample_rate={})", device_name, fmt, actual_rate);
                    return;
                }
                Err(e) => {
                    let err_str = e.to_string();
                    let is_transient = audio_core::is_transient_audio_error(&err_str);
                    let is_permanent = audio_core::is_permanent_device_error(&err_str);

                    if is_permanent {
                        error!("ensure_audio_init: permanent error (no retry): {}", err_str);
                        self.status_message = format!("Audio init failed (permission): {}", err_str);
                        break;
                    }

                    if is_transient && attempt < max_retries - 1 {
                        warn!(
                            "Audio init transient error (attempt {}/{}), retrying in {}ms: {}",
                            attempt + 1,
                            max_retries,
                            delay_ms,
                            err_str
                        );
                        self.status_message = format!(
                            "Audio init busy, retrying... ({}/{})",
                            attempt + 1,
                            max_retries
                        );
                        std::thread::sleep(Duration::from_millis(delay_ms));
                        delay_ms *= 2;
                        continue;
                    }

                    error!(
                        "ensure_audio_init: init_output failed{}: {}",
                        if is_transient { " (retries exhausted)" } else { "" },
                        err_str
                    );
                    self.status_message = format!("Audio init failed: {}", err_str);
                    break;
                }
            }
        }
    }

    pub fn start_streaming(&mut self) {
        self.ensure_audio_init();
        if !self.audio_initialized {
            return;
        }

        self.recovery_attempt_count = 0;

        let tc = self.start_timecode;
        let fps = self.fps();
        let channel = self.ltc_channel.as_str().to_string();
        let volume = self.ltc_volume;

        info!("Starting LTC stream: tc={:02}:{:02}:{:02}:{:02}, fps={}, drop_frame={}, channel={}, volume={}",
            tc.hours, tc.minutes, tc.seconds, tc.frames, fps.fps, fps.drop_frame, channel, volume);

        let core = self.audio_core.lock().unwrap();
        match core.start_ltc(tc, fps.fps, fps.drop_frame, channel, volume) {
            Ok(()) => {
                self.is_playing = true;
                self.status_message = "Streaming LTC".to_string();
                info!("LTC stream started successfully");
            }
            Err(e) => {
                error!("Failed to start LTC stream: {}", e);
                self.status_message = format!("Start failed: {}", e);
            }
        }
    }

    pub fn stop_streaming(&mut self) {
        info!("Stopping LTC stream");

        let core = match self.audio_core.lock() {
            Ok(c) => c,
            Err(e) => {
                error!("stop_streaming: audio core mutex poisoned: {}", e);
                self.is_playing = false;
                return;
            }
        };
        if let Err(e) = core.stop_ltc() {
            error!("stop_streaming: stop_ltc failed: {}", e);
            self.status_message = format!("Stop failed: {}", e);
        }
        drop(core);
        self.is_playing = false;
        self.status_message = "Stopped".to_string();
        info!("LTC stream stopped");
    }

    pub fn attempt_recovery(&mut self) {
        self.recovery_attempt_count += 1;

        if self.recovery_attempt_count >= 3 {
            error!(
                "attempt_recovery: 3 recovery attempts exhausted — giving up"
            );
            self.is_playing = false;
            self.audio_initialized = false;
            self.status_message = "Recovery failed: device unreachable after 3 attempts".to_string();
            self.add_notification(
                NotificationType::Error,
                "Audio recovery failed after 3 attempts — device may be unavailable. Re-select or re-connect audio device.".to_string(),
            );
            return;
        }

        let was_playing = self.is_playing;
        if was_playing {
            info!("Recovery: stopping LTC stream");
            self.stop_streaming();
        }

        // Stop and re-init the audio output
        info!("Recovery: re-initializing audio output");
        let core = match self.audio_core.lock() {
            Ok(c) => c,
            Err(e) => {
                error!("attempt_recovery: audio core mutex poisoned: {}", e);
                self.status_message = "Recovery failed: mutex poisoned".to_string();
                return;
            }
        };
        let _ = core.stop_output();
        drop(core);
        self.audio_initialized = false;
        std::thread::sleep(Duration::from_millis(50));

        self.ensure_audio_init();

        if self.audio_initialized && was_playing {
            info!("Recovery: restarting LTC stream");
            let tc = self.start_timecode;
            let fps = self.fps();
            let channel = self.ltc_channel.as_str().to_string();
            let volume = self.ltc_volume;
            let core = match self.audio_core.lock() {
                Ok(c) => c,
                Err(e) => {
                    error!("attempt_recovery: audio core mutex poisoned on restart: {}", e);
                    self.status_message = "Recovery: failed to restart stream".to_string();
                    return;
                }
            };
            match core.start_ltc(tc, fps.fps, fps.drop_frame, channel, volume) {
                Ok(()) => {
                    self.recovery_attempt_count = 0;
                    self.is_playing = true;
                    self.status_message = "Recovery: stream restarted".to_string();
                    info!("Recovery: LTC stream restarted successfully");
                }
                Err(e) => {
                    error!("Recovery: failed to restart LTC stream: {}", e);
                    self.status_message = format!("Recovery failed: {}", e);
                }
            }
        }
    }

    pub fn handle_reset(&mut self) {
        let tc = self.start_timecode;
        info!("Resetting LTC to {:02}:{:02}:{:02}:{:02}", tc.hours, tc.minutes, tc.seconds, tc.frames);

        let core = match self.audio_core.lock() {
            Ok(c) => c,
            Err(e) => {
                error!("handle_reset: audio core mutex poisoned: {}", e);
                return;
            }
        };
        if let Err(e) = core.reset_ltc(tc) {
            error!("handle_reset: reset_ltc failed: {}", e);
        }
        drop(core);
        self.current_timecode = self.start_timecode;
        self.status_message = "Reset".to_string();
        info!("LTC reset complete");
    }

    pub fn trigger_clap(&mut self) {
        self.ensure_audio_init();
        if !self.audio_initialized {
            return;
        }

        info!("Clap triggered: scene={}, take={}, roll={}, freq={}Hz, volume={}, channel={}",
            self.scene, self.take, self.roll, self.beep_frequency, self.beep_volume, self.beep_channel.as_str());

        let freq = self.beep_frequency;
        let volume = self.beep_volume;
        let channel = self.beep_channel.as_str();

        let core = match self.audio_core.lock() {
            Ok(c) => c,
            Err(e) => {
                error!("trigger_clap: audio core mutex poisoned: {}", e);
                return;
            }
        };
        if let Err(e) = core.play_beep(self.sample_rate, freq, self.beep_duration, volume, channel) {
            error!("trigger_clap: play_beep failed: {}", e);
        }
        drop(core);

        // Trigger visual effects
        self.clap_flash_alpha = 1.0;
        self.clap_arm_angle = 0.0; // snap to closed position

        // Log the clap
        let fps = self.fps();
        let tc_str = timecode_to_string(self.current_timecode, fps.drop_frame);
        let ms_str = timecode_to_ms_string(self.current_timecode, fps.fps);
        let timestamp = chrono_now_string();
        let note = format!(
            "Scene {} / Take {} / Roll {}",
            self.scene,
            self.take,
            if self.roll.is_empty() { "—" } else { &self.roll }
        );

        let id = format!("clap-{}-{}", self.logs.len() + 1, timestamp);
        self.logs.push(ClapLogItem {
            id,
            timestamp,
            timecode: tc_str,
            milliseconds: ms_str,
            note,
        });

        if self.auto_increment_take {
            self.take += 1;
        }
    }

    pub fn update_clock(&mut self) {
        if !self.is_playing {
            return;
        }
        let core = match self.audio_core.lock() {
            Ok(c) => c,
            Err(e) => {
                warn!("update_clock: audio core mutex poisoned: {}", e);
                return;
            }
        };
        let prev = self.current_timecode;
        self.current_timecode = core.current_timecode();
        if self.current_timecode != prev {
            trace!("Clock updated: {:02}:{:02}:{:02}:{:02}",
                self.current_timecode.hours, self.current_timecode.minutes,
                self.current_timecode.seconds, self.current_timecode.frames);
        }
    }
}

impl eframe::App for AppState {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.has_requested_maximize {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            self.has_requested_maximize = true;
        }
        // Apply theme
        self.theme.apply(ctx);

        // Frame-rate independent delta time
        let now = Instant::now();
        let delta = self
            .last_frame_time
            .map(|t| (now - t).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.1);
        self.last_frame_time = Some(now);

        // Animate clap effects using time-based deltas
        // Flash overlay: decays at 2.0/s (0.08 per frame at 25fps = 2.0/s)
        if self.clap_flash_alpha > 0.0 {
            self.clap_flash_alpha = (self.clap_flash_alpha - 2.0 * delta).max(0.0);
        }
        // Clapper arm: snaps to 0 (closed) and smoothly opens back to -25 (open)
        // Exponential decay at 4.0/s (per-frame factor 0.15 at 25fps ≈ 4.06/s)
        if self.clap_arm_angle > -25.0 {
            let target = -25.0;
            let diff = target - self.clap_arm_angle;
            let decay_factor = 1.0 - (-4.0 * delta).exp();
            self.clap_arm_angle += diff * decay_factor;
            if self.clap_arm_angle > -24.9 {
                self.clap_arm_angle = -25.0;
            }
        }

        let has_active_animation = self.clap_flash_alpha > 0.0 || self.clap_arm_angle > -25.0;

        // Poll current timecode when playing
        if self.is_playing {
            self.update_clock();
            let interval = Duration::from_secs_f64(1.0 / self.fps().fps);
            ctx.request_repaint_after(interval);
        } else if has_active_animation {
            // Run at 60fps while animation is in progress
            ctx.request_repaint_after(Duration::from_secs_f64(1.0 / 60.0));
        } else {
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        // Update system time
        self.system_time = chrono_now_string();

        // Drain audio events and surface as toast notifications
        let events = self.audio_core.lock()
            .map(|c| c.drain_events())
            .unwrap_or_default();
        for evt in events {
            match evt {
                AudioEvent::StreamError(msg) => {
                    self.add_notification(NotificationType::Error, format!("Audio stream error: {}", msg));
                }
                AudioEvent::StreamDied => {
                    self.add_notification(NotificationType::Error, "Audio stream has died — re-initialize device".to_string());
                }
                AudioEvent::StreamRecovering { attempt } => {
                    self.add_notification(NotificationType::Warning, format!("Audio stream recovering (attempt {})", attempt));
                }
                AudioEvent::StreamDead => {
                    self.add_notification(NotificationType::Error, "Fatal: audio device unreachable — stop and re-select device".to_string());
                    self.is_playing = false;
                    self.audio_initialized = false;
                }
                AudioEvent::RecoveryNeeded { reason } => {
                    self.add_notification(NotificationType::Warning, format!("Audio recovery needed: {}", reason));
                    self.attempt_recovery();
                }
                AudioEvent::Underrun => {
                    self.add_notification(NotificationType::Warning, "Audio underrun — samples not keeping up".to_string());
                }
                AudioEvent::FramesDropped { total } => {
                    self.add_notification(NotificationType::Warning, format!("{} frame(s) dropped — audio buffer overloaded", total));
                }
            }
        }

        // Keyboard shortcuts (skip when a text field has focus)
        let any_focused = ctx.memory(|m| m.focused().is_some());
        let (toggle_play, do_clap, do_reset, do_lock, toggle_debug) = if !any_focused {
            ctx.input(|i| (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::L),
                i.modifiers.ctrl && i.key_pressed(egui::Key::D),
            ))
        } else {
            (false, false, false, false, false)
        };

        if toggle_play {
            if self.is_playing && !self.is_locked {
                self.stop_streaming();
            } else if !self.is_playing && !self.is_locked {
                self.start_streaming();
            }
        }
        if do_clap && !self.is_locked {
            self.trigger_clap();
        }
        if do_reset && !self.is_locked {
            self.handle_reset();
        }
        if do_lock {
            self.is_locked = !self.is_locked;
        }
        if toggle_debug {
            self.show_debug_log = !self.show_debug_log;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Apply theme background
        let bg = self.theme.colors().app_bg;
        let clip_rect = ui.clip_rect();
        ui.painter().rect_filled(clip_rect, 0.0, bg);

        // Use the full available rect, no forced scrolling at initial layout pass
        // ScrollOnly kicks in if content exceeds the window later
        let area = egui::ScrollArea::both()
            .auto_shrink([false, false]);

        area.show(ui, |ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 8.0);

            // Content container uses adaptive width (no fixed max)
            // but caps indentation so wide windows don't spread too much
            let max_content = ui.available_width().min(720.0);

            ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                ui.set_max_width(max_content);

                    // Header
                    self.render_header(ui);
                    ui.add_space(4.0);

                    // FAQ accordion panel
                    self.render_faq_panel(ui);
                    ui.add_space(4.0);

                    // Section 1: Master studio time clock
                    self.render_clock_section(ui);
                    ui.add_space(8.0);

                    // Section 2: Tabbed deck
                    self.render_tabbed_deck(ui);
                    ui.add_space(4.0);

                    // Footer status bar
                    self.render_status_bar(ui);
                });
            });

        // App icon context menu
        self.render_app_menu(ui);

        // Debug log window
        self.render_debug_log_window(ui);

        // Flash overlay (drawn on top of everything)
        if self.clap_flash_alpha > 0.01 {
            let ctx = ui.ctx();
            let screen = ctx.viewport_rect();
            let alpha = (self.clap_flash_alpha * 255.0) as u8;
            let color = Color32::from_rgba_unmultiplied(255, 255, 255, alpha);
            egui::Area::new(egui::Id::new("clap_flash"))
                .order(egui::Order::Foreground)
                .fixed_pos(screen.min)
                .show(ctx, |ui| {
                    ui.painter().rect_filled(screen, 0.0, color);
                });
        }

        // Toast notifications (on top of everything)
        self.render_toasts(ui);
    }
}

/// Centers a horizontal row of widgets perfectly using two-pass cross-frame memory.
pub fn centered_horizontal_row<R>(
    ui: &mut Ui,
    unique_id_str: &str,
    initial_guess: f32,
    add_contents: impl FnOnce(&mut Ui) -> R,
) -> R {
    let row_id = ui.id().with(unique_id_str);
    let row_width = ui.data_mut(|d| d.get_temp::<f32>(row_id).unwrap_or(initial_guess));
    let mut result = None;
    ui.horizontal(|ui| {
        let center_space = (ui.available_width() - row_width) / 2.0;
        ui.add_space(center_space.max(0.0));
        let inner_response = ui.scope(|ui| {
            result = Some(add_contents(ui));
        }).response;
        ui.data_mut(|d| d.insert_temp(row_id, inner_response.rect.width()));
    });
    result.expect("Inner contents closure must run exactly once")
}

impl AppState {
    fn render_header(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        ui.horizontal(|ui| {
            // Clapper logo (orange square with two black horizontal lines) — clickable for app menu
            let (rect, icon_response) = ui.allocate_exact_size(egui::Vec2::new(34.0, 34.0), Sense::click());
            ui.painter().rect_filled(rect, 4.0, ACCENT);
            let center_y = rect.center().y;
            let line_w = 18.0;
            let line_h = 2.0;
            let line1 = egui::Rect::from_center_size(egui::pos2(rect.center().x, center_y - 3.5), egui::vec2(line_w, line_h));
            let line2 = egui::Rect::from_center_size(egui::pos2(rect.center().x, center_y + 3.5), egui::vec2(line_w, line_h));
            ui.painter().rect_filled(line1, 0.0, Color32::BLACK);
            ui.painter().rect_filled(line2, 0.0, Color32::BLACK);
            if icon_response.clicked() || icon_response.secondary_clicked() {
                self.show_app_menu = true;
                self.app_menu_pos = Some(icon_response.rect.left_bottom());
            }

            ui.add_space(4.0);

            // Title and tagline
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("LTC ENGINE").font(FontId::proportional(16.0)).strong().color(colors.text_title));
                    ui.label(RichText::new(format!("v{}", APP_VERSION)).font(FontId::proportional(16.0)).strong().color(ACCENT));
                });
                ui.label(RichText::new("LINEAR TIMECODE HUB").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            });

            // Metadata indicators (responsive, shown if width > 500)
            if ui.available_width() > 300.0 {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    // Stats column 1: Interface
                    ui.vertical(|ui| {
                        ui.label(RichText::new("INTERFACE").font(FontId::proportional(8.0)).color(colors.text_muted).strong());
                        let status_text = if self.is_playing { "NATIVE ACTIVE" } else { "STANDBY" };
                        let status_color = if self.is_playing { Color32::from_rgb(0x22, 0xC5, 0x5E) } else { Color32::from_rgb(0xF5, 0x9E, 0x0B) };
                        ui.label(RichText::new(status_text).font(FontId::proportional(10.0)).strong().color(status_color));
                    });
                    ui.add_space(10.0);
                    // Stats column 2: Sample Rate
                    ui.vertical(|ui| {
                        ui.label(RichText::new("SAMPLE RATE").font(FontId::proportional(8.0)).color(colors.text_muted).strong());
                        let rate_khz = self.sample_rate as f32 / 1000.0;
                        ui.label(RichText::new(format!("{:.1} KHZ", rate_khz)).font(FontId::proportional(10.0)).strong().color(colors.text_title));
                    });
                    ui.add_space(10.0);
                    // Stats column 3: Buffer
                    ui.vertical(|ui| {
                        ui.label(RichText::new("BUFFER").font(FontId::proportional(8.0)).color(colors.text_muted).strong());
                        let buffer_smp = (self.sample_rate as f64 / self.fps().fps).round() as u32;
                        ui.label(RichText::new(format!("{} SMP", buffer_smp)).font(FontId::proportional(10.0)).strong().color(colors.text_title));
                    });
                });
            }

            // Right side buttons + system time
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // System clock
                let time_str = format!("{} UTC", self.system_time);
                if ui.available_width() > 180.0 {
                    let sys_frame = egui::Frame::new()
                        .fill(colors.nested_bg)
                        .stroke(egui::Stroke::new(1.0, colors.border_main))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 4));
                    sys_frame.show(ui, |ui| {
                        ui.label(
                            RichText::new(time_str)
                                .font(FontId::monospace(9.0))
                                .color(colors.text_muted),
                        );
                    });
                }

                // Help button
                let help_text = "?";
                let help_btn = egui::Button::new(RichText::new(help_text).font(FontId::proportional(12.0)).strong())
                    .fill(colors.nested_bg);
                if ui.add(help_btn).clicked() {
                    self.show_faq = !self.show_faq;
                }

                // Theme button
                let icon = if self.theme == Theme::Dark { "\u{2600}\u{FE0F}" } else { "\u{1F319}" };
                let theme_btn = egui::Button::new(RichText::new(icon).font(FontId::proportional(12.0)))
                    .fill(colors.nested_bg);
                if ui.add(theme_btn).clicked() {
                    self.theme = self.theme.toggle();
                }
            });
        });
    }

    fn render_faq_panel(&mut self, ui: &mut Ui) {
        if !self.show_faq {
            return;
        }

        let colors = self.theme.colors();
        let frame = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(16))
            .corner_radius(12.0)
            .fill(colors.card_bg)
            .stroke(egui::Stroke::new(1.5, colors.border_main));

        frame.show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(
                    RichText::new("LTC & MULTI-CAM SYNC - QUICK GUIDE")
                        .font(FontId::proportional(13.0))
                        .color(colors.text_title)
                        .strong(),
                );
                ui.add_space(8.0);

                let width = ui.available_width();
                if width > 500.0 {
                    ui.columns(3, |cols| {
                        cols[0].vertical(|ui| {
                            ui.label(RichText::new("📻 WHAT IS LINEAR TIMECODE?").font(FontId::proportional(10.0)).color(ACCENT).strong());
                            ui.add_space(4.0);
                            ui.label(RichText::new(
                                "Linear Timecode (LTC) is an analog audio signal encoding SMPTE timecode \
                                (Hours:Minutes:Seconds:Frames) using Bi-Phase Mark Modulation. Cameras and \
                                recorders listen to this audio signal to align footage in post."
                            ).font(FontId::proportional(10.5)).color(colors.text_muted));
                        });
                        cols[1].vertical(|ui| {
                            ui.label(RichText::new("🎥 CONNECTING CAMERAS").font(FontId::proportional(10.0)).color(ACCENT).strong());
                            ui.add_space(4.0);
                            ui.label(RichText::new(
                                "Connect your device's audio output (line/jack) \
                                directly to the mic input of your cameras, or dedicated sync boxes \
                                (Tentacle, Deity). Set camera audio gain manually to a medium level."
                            ).font(FontId::proportional(10.5)).color(colors.text_muted));
                        });
                        cols[2].vertical(|ui| {
                            ui.label(RichText::new("🔊 SYNCHRONIZING IN EDIT").font(FontId::proportional(10.0)).color(ACCENT).strong());
                            ui.add_space(4.0);
                            ui.label(RichText::new(
                                "Import all media files into DaVinci Resolve, Premiere, or Final Cut Pro. \
                                Right-click files and choose 'Update Timecode from Audio Track'. \
                                The software matches alignment instantly!"
                            ).font(FontId::proportional(10.5)).color(colors.text_muted));
                        });
                    });
                } else {
                    ui.vertical(|ui| {
                        ui.label(RichText::new("📻 WHAT IS LINEAR TIMECODE?").font(FontId::proportional(10.0)).color(ACCENT).strong());
                        ui.label(RichText::new(
                            "Linear Timecode (LTC) is an analog audio signal encoding SMPTE timecode. \
                            Cameras and recorders listen to this audio to align footage in post."
                        ).font(FontId::proportional(10.5)).color(colors.text_muted));
                        ui.add_space(6.0);

                        ui.label(RichText::new("🎥 CONNECTING CAMERAS").font(FontId::proportional(10.0)).color(ACCENT).strong());
                        ui.label(RichText::new(
                            "Connect audio output to camera mic input or sync boxes. \
                            Set gain manually to a medium level."
                        ).font(FontId::proportional(10.5)).color(colors.text_muted));
                        ui.add_space(6.0);

                        ui.label(RichText::new("🔊 SYNCHRONIZING IN EDIT").font(FontId::proportional(10.0)).color(ACCENT).strong());
                        ui.label(RichText::new(
                            "In DaVinci Resolve or Premiere, right-click files and select \
                            'Update Timecode from Audio Track' to auto-sync."
                        ).font(FontId::proportional(10.5)).color(colors.text_muted));
                    });
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Got it").clicked() {
                            self.show_faq = false;
                        }
                    });
                });
            });
        });
    }

    fn render_clock_section(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        let frame = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(20, 16))
            .fill(colors.card_bg)
            .stroke(egui::Stroke::new(1.5, colors.border_main))
            .corner_radius(16.0);

        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            
            // Timecode Stream label with pulsing dot
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    // Center the header
                    let text_w = 175.0;
                    ui.add_space(((ui.available_width() - text_w) / 2.0).max(0.0));
                    ui.label(
                        RichText::new("LINEAR TIMECODE STREAM")
                            .font(FontId::proportional(10.0))
                            .color(ACCENT)
                            .strong()
                            .extra_letter_spacing(1.5),
                    );
                    
                    // Pulsing dot
                    let time = ui.ctx().input(|i| i.time);
                    let alpha = if self.is_playing {
                        (time * 4.0).sin() * 0.3 + 0.7
                    } else {
                        1.0
                    };
                    let dot_color = if self.is_playing {
                        Color32::from_rgba_unmultiplied(0x22, 0xC5, 0x5E, (alpha * 255.0) as u8)
                    } else {
                        Color32::from_rgba_unmultiplied(0xF5, 0x9E, 0x0B, 255)
                    };
                    
                    let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(rect.center(), 4.0, dot_color);
                });
            });
            ui.add_space(4.0);

            // Clock rendering
            widgets::clock::render(ui, self);
            
            ui.add_space(8.0);

            // Pills Row
            let fps = self.fps();
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    let total_pill_w = 330.0;
                    ui.add_space(((ui.available_width() - total_pill_w) / 2.0).max(0.0));
                    
                    // Pill 1: FPS
                    let pill1_frame = egui::Frame::new()
                        .corner_radius(6.0)
                        .fill(colors.nested_bg)
                        .stroke(egui::Stroke::new(1.0, colors.border_main))
                        .inner_margin(egui::Margin::symmetric(10, 4));
                    pill1_frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("⚡").font(FontId::proportional(10.0)).color(ACCENT).strong());
                            ui.label(RichText::new(format!("FPS: {} ({} FPS)", fps.fps, fps.name)).font(FontId::proportional(10.0)).color(colors.text_muted).strong());
                        });
                    });
                    
                    ui.add_space(8.0);

                    // Pill 2: Routing
                    let pill2_frame = egui::Frame::new()
                        .corner_radius(6.0)
                        .fill(colors.nested_bg)
                        .stroke(egui::Stroke::new(1.0, colors.border_main))
                        .inner_margin(egui::Margin::symmetric(10, 4));
                    pill2_frame.show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("🔊").font(FontId::proportional(10.0)).color(ACCENT).strong());
                            ui.label(RichText::new(format!("ROUTE: LTC {} | CLAP {}", self.ltc_channel.label().to_uppercase(), self.beep_channel.label().to_uppercase())).font(FontId::proportional(10.0)).color(colors.text_muted).strong());
                        });
                    });
                });
            });

            ui.add_space(10.0);

            // Control buttons layout
            let width = ui.available_width();
            let spacing = 8.0;
            let is_locked = self.is_locked;
            
            if width > 420.0 {
                // Horizontal row layout
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing = egui::Vec2::new(spacing, 0.0);
                    
                    let primary_w = ((width - spacing * 3.0 - 40.0 * 2.0) / 2.0).max(100.0);
                    
                    // 1. Play/Stop Button
                    if self.is_playing {
                        let stop_btn = egui::Button::new(RichText::new("■ STOP").strong().color(Color32::WHITE))
                            .fill(Color32::from_rgb(0xDC, 0x26, 0x26)) // Bold Red
                            .min_size(egui::vec2(primary_w, 40.0));
                        let resp = ui.add_enabled(!is_locked, stop_btn);
                        if resp.clicked() {
                            self.stop_streaming();
                        }
                    } else {
                        let start_btn = egui::Button::new(RichText::new("▶ START").strong().color(colors.text_title))
                            .fill(colors.nested_bg)
                            .stroke(egui::Stroke::new(1.0, colors.border_main))
                            .min_size(egui::vec2(primary_w, 40.0));
                        let resp = ui.add_enabled(!is_locked, start_btn);
                        if resp.clicked() {
                            self.start_streaming();
                        }
                    }
                    
                    // 2. Clap & Beep Button (Prominent orange)
                    let clap_btn = egui::Button::new(RichText::new("🎥 CLAP & BEEP").strong().color(Color32::BLACK))
                        .fill(ACCENT)
                        .min_size(egui::vec2(primary_w, 40.0));
                    let resp = ui.add_enabled(!is_locked, clap_btn);
                    if resp.clicked() {
                        self.trigger_clap();
                    }
                    
                    // 3. Reset Button (square)
                    let reset_btn = egui::Button::new(RichText::new("⟲").font(FontId::proportional(16.0)).strong())
                        .fill(colors.nested_bg)
                        .stroke(egui::Stroke::new(1.0, colors.border_main))
                        .min_size(egui::vec2(40.0, 40.0));
                    let resp = ui.add_enabled(!is_locked, reset_btn);
                    if resp.clicked() {
                        self.handle_reset();
                    }
                    
                    // 4. Lock Button (square)
                    let lock_icon = if is_locked { "🔒" } else { "🔓" };
                    let lock_fill = if is_locked { ACCENT } else { colors.nested_bg };
                    let lock_text_color = if is_locked { Color32::BLACK } else { colors.text_muted };
                    let lock_btn = egui::Button::new(RichText::new(lock_icon).font(FontId::proportional(16.0)).strong().color(lock_text_color))
                        .fill(lock_fill)
                        .stroke(egui::Stroke::new(1.0, if is_locked { ACCENT } else { colors.border_main }))
                        .min_size(egui::vec2(40.0, 40.0));
                    if ui.add(lock_btn).clicked() {
                        self.is_locked = !self.is_locked;
                    }
                });
            } else {
                // Stacked layout for small screens
                ui.vertical(|ui| {
                    if self.is_playing {
                        let stop_btn = egui::Button::new(RichText::new("■ STOP").strong().color(Color32::WHITE))
                            .fill(Color32::from_rgb(0xDC, 0x26, 0x26))
                            .min_size(egui::vec2(width, 36.0));
                        if ui.add_enabled(!is_locked, stop_btn).clicked() {
                            self.stop_streaming();
                        }
                    } else {
                        let start_btn = egui::Button::new(RichText::new("▶ START").strong().color(colors.text_title))
                            .fill(colors.nested_bg)
                            .stroke(egui::Stroke::new(1.0, colors.border_main))
                            .min_size(egui::vec2(width, 36.0));
                        if ui.add_enabled(!is_locked, start_btn).clicked() {
                            self.start_streaming();
                        }
                    }
                    ui.add_space(4.0);

                    let clap_btn = egui::Button::new(RichText::new("🎥 CLAP & BEEP").strong().color(Color32::BLACK))
                        .fill(ACCENT)
                        .min_size(egui::vec2(width, 36.0));
                    if ui.add_enabled(!is_locked, clap_btn).clicked() {
                        self.trigger_clap();
                    }
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        let inner_w = (width - spacing) / 2.0;
                        let reset_btn = egui::Button::new(RichText::new("⟲ Reset").strong())
                            .fill(colors.nested_bg)
                            .stroke(egui::Stroke::new(1.0, colors.border_main))
                            .min_size(egui::vec2(inner_w, 36.0));
                        if ui.add_enabled(!is_locked, reset_btn).clicked() {
                            self.handle_reset();
                        }

                        let lock_icon = if is_locked { "🔒 Locked" } else { "🔓 Unlocked" };
                        let lock_fill = if is_locked { ACCENT } else { colors.nested_bg };
                        let lock_text_color = if is_locked { Color32::BLACK } else { colors.text_muted };
                        let lock_btn = egui::Button::new(RichText::new(lock_icon).strong().color(lock_text_color))
                            .fill(lock_fill)
                            .stroke(egui::Stroke::new(1.0, if is_locked { ACCENT } else { colors.border_main }))
                            .min_size(egui::vec2(inner_w, 36.0));
                        if ui.add(lock_btn).clicked() {
                            self.is_locked = !self.is_locked;
                        }
                    });
                });
            }
        });
    }

    fn render_tabbed_deck(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        ui.vertical(|ui| {
            // Tab bar
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(20.0, 0.0);
                for tab in &[Tab::Clapper, Tab::Settings] {
                    let selected = *tab == self.active_tab;
                    let text = if selected {
                        RichText::new(tab.label())
                            .font(FontId::proportional(12.0))
                            .strong()
                            .color(colors.text_title)
                    } else {
                        RichText::new(tab.label())
                            .font(FontId::proportional(12.0))
                            .color(colors.text_muted)
                    };
                    let resp = ui.add(egui::Button::new(text).frame(false).fill(Color32::TRANSPARENT));
                    if resp.clicked() {
                        self.active_tab = *tab;
                    }
                    if selected {
                        // Draw orange underline
                        let underline_y = resp.rect.bottom() + 4.0;
                        let start = egui::pos2(resp.rect.left(), underline_y);
                        let end = egui::pos2(resp.rect.right(), underline_y);
                        ui.painter().line_segment([start, end], egui::Stroke::new(2.0, ACCENT));
                    }
                }
            });

            ui.add_space(4.0);
            ui.separator();
            ui.add_space(8.0);

            // Tab content
            match self.active_tab {
                Tab::Clapper => widgets::clapper::render(ui, self),
                Tab::Settings => widgets::settings::render(ui, self),
            }
        });
    }

    fn render_toasts(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx();
        let now = Instant::now();

        self.notifications.retain(|n| now - n.created_at < n.duration);

        if self.notifications.is_empty() {
            return;
        }

        let screen = ctx.viewport_rect();
        let colors = self.theme.colors();
        let toast_width = 380.0;
        let toast_height = 46.0;
        let spacing = 8.0;
        let bottom_margin = 16.0;
        let base_x = screen.center().x - toast_width / 2.0;
        let slide_in_dur = 0.3;
        let fade_out_dur = 0.5;

        let mut to_remove = Vec::new();

        for (i, toast) in self.notifications.iter().enumerate() {
            let idx_from_bottom = self.notifications.len() - 1 - i;
            let age = now - toast.created_at;
            let age_secs = age.as_secs_f32();
            let remaining = toast.duration.checked_sub(age).unwrap_or(Duration::ZERO);
            let remaining_secs = remaining.as_secs_f32();

            let slide_progress = (age_secs / slide_in_dur).min(1.0);
            let slide_offset = (1.0 - slide_progress) * 20.0;

            let entry_alpha = (age_secs / slide_in_dur).min(1.0);
            let exit_alpha = (remaining_secs / fade_out_dur).min(1.0);
            let alpha = entry_alpha * exit_alpha;

            let y = screen.bottom()
                - bottom_margin
                - (idx_from_bottom as f32 * (toast_height + spacing))
                + slide_offset;

            let (strip_color, icon) = match toast.notification_type {
                NotificationType::Error => (colors.error_red, "❌"),
                NotificationType::Warning => (colors.warning_amber, "⚠"),
                NotificationType::Success => (colors.success_green, "✅"),
                NotificationType::Info => (colors.info_blue, "ℹ\u{FE0F}"),
            };

            let toast_id = toast.id;
            let close_clicked = std::cell::Cell::new(false);

            egui::Area::new(egui::Id::new(("toast", toast_id)))
                .order(egui::Order::Foreground)
                .fixed_pos(egui::pos2(base_x, y))
                .show(ctx, |ui| {
                    let bg = colors.card_bg;
                    let bg_alpha = Color32::from_rgba_premultiplied(
                        bg.r(),
                        bg.g(),
                        bg.b(),
                        (alpha * 255.0) as u8,
                    );
                    let border = colors.border_main;
                    let border_alpha = Color32::from_rgba_premultiplied(
                        border.r(),
                        border.g(),
                        border.b(),
                        (alpha * 255.0) as u8,
                    );

                    let frame = egui::Frame::new()
                        .fill(bg_alpha)
                        .stroke(egui::Stroke::new(1.0, border_alpha))
                        .corner_radius(8.0)
                        .inner_margin(egui::Margin::symmetric(8, 8));

                    frame.show(ui, |ui| {
                        let frame_rect = ui.max_rect();

                        ui.painter().rect_filled(
                            egui::Rect::from_min_size(
                                frame_rect.min,
                                egui::vec2(4.0, frame_rect.height()),
                            ),
                            egui::CornerRadius::same(4),
                            strip_color.linear_multiply(alpha),
                        );

                        ui.horizontal(|ui| {
                            ui.add_space(8.0);

                            ui.label(
                                RichText::new(icon)
                                    .color(strip_color.linear_multiply(alpha))
                                    .size(16.0)
                                    .strong(),
                            );
                            ui.add_space(4.0);

                            let text_color = Color32::from_rgba_premultiplied(
                                colors.text_main.r(),
                                colors.text_main.g(),
                                colors.text_main.b(),
                                (alpha * 255.0) as u8,
                            );
                            ui.label(
                                RichText::new(&toast.message)
                                    .color(text_color)
                                    .size(13.0),
                            );

                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                let btn = egui::Button::new(
                                    RichText::new("❌")
                                        .color(
                                            colors
                                                .text_muted
                                                .linear_multiply(alpha),
                                        )
                                        .size(14.0),
                                )
                                .frame(false);
                                if ui.add(btn).clicked() {
                                    close_clicked.set(true);
                                }
                            });
                        });
                    });
                });

            if close_clicked.get() {
                to_remove.push(toast_id);
            }
        }

        for id in to_remove {
            self.notifications.retain(|n| n.id != id);
        }
    }

    fn add_notification(&mut self, ntype: NotificationType, message: String) {
        let id = self.next_notification_id;
        self.next_notification_id += 1;
        let duration = match ntype {
            NotificationType::Error => Duration::from_secs(6),
            _ => Duration::from_secs(4),
        };
        self.notifications.push(ToastNotification {
            id,
            message,
            notification_type: ntype,
            created_at: Instant::now(),
            duration,
        });
    }

    fn render_status_bar(&mut self, ui: &mut Ui) {
        widgets::status::render(ui, self);
    }
}

impl AppState {
    fn render_app_menu(&mut self, ui: &mut Ui) {
        if !self.show_app_menu {
            return;
        }
        let colors = self.theme.colors();
        let pos = match self.app_menu_pos {
            Some(p) => p,
            None => {
                self.show_app_menu = false;
                return;
            }
        };

        egui::Area::new("app_menu".into())
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                let frame = egui::Frame::new()
                    .fill(colors.card_bg)
                    .stroke(egui::Stroke::new(1.0, colors.border_main))
                    .corner_radius(6.0)
                    .inner_margin(egui::Margin::symmetric(4, 4));
                frame.show(ui, |ui| {
                    ui.set_min_width(140.0);
                    let debug_label = if self.show_debug_log {
                        "✓ Debug Log"
                    } else {
                        "Debug Log"
                    };
                    if ui
                        .add(egui::Button::new(RichText::new(debug_label).font(FontId::proportional(12.0)).color(colors.text_main)))
                        .clicked()
                    {
                        self.show_debug_log = !self.show_debug_log;
                        self.show_app_menu = false;
                    }
                });
            });

        // Close menu when clicking outside
        let screen = ui.ctx().viewport_rect();
        if ui.ctx().input(|i| i.pointer.any_click()) {
            let click_pos = ui.ctx().pointer_interact_pos();
            if let Some(click) = click_pos {
                if !screen.contains(click) {
                    self.show_app_menu = false;
                }
            }
        }
    }

    fn render_debug_log_window(&mut self, ui: &mut Ui) {
        if !self.show_debug_log {
            return;
        }

        let ctx = ui.ctx().clone();

        egui::Window::new("Debug Log")
            .id("debug_log_window".into())
            .default_size([600.0, 400.0])
            .resizable(true)
            .collapsible(false)
            .show(&ctx, |ui| {
                let buffer = self.log_buffer.lock().unwrap();
                // Pre-allocate memory to avoid multiple reallocations
                let estimated_size: usize = buffer.entries.iter().map(|s| s.len() + 1).sum();
                let mut log_text = String::with_capacity(estimated_size);
                for (i, entry) in buffer.entries.iter().enumerate() {
                    if i > 0 {
                        log_text.push('\n');
                    }
                    log_text.push_str(entry);
                }
                egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    ui.add_sized(
                        ui.available_size(),
                        egui::TextEdit::multiline(&mut log_text)
                            .font(egui::TextStyle::Monospace)
                            .interactive(true),
                    );
                });
            });
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────

pub fn timecode_to_string(tc: Timecode, drop_frame: bool) -> String {
    let sep = if drop_frame { ';' } else { ':' };
    format!(
        "{:02}:{:02}:{:02}{}{:02}",
        tc.hours, tc.minutes, tc.seconds, sep, tc.frames
    )
}

pub fn timecode_to_ms_string(tc: Timecode, fps: f64) -> String {
    let ms = (tc.frames as f64 / fps * 1000.0).round() as u32;
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        tc.hours, tc.minutes, tc.seconds, ms
    )
}

pub fn chrono_now_string() -> String {
    use chrono::Local;
    Local::now().format("%H:%M:%S").to_string()
}
