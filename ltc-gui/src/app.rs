use std::sync::Mutex;
use std::time::Duration;

use audio_core::{list_audio_devices, AudioCore, AudioDeviceInfo, Timecode};
use egui::{Color32, FontId, RichText, Sense, Ui};

use crate::theme::{Theme, ACCENT};
use crate::widgets;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const SAMPLE_RATE: u32 = 48000;
const BUFFER_SIZE: u32 = 256;

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
    pub ltc_channel: AudioChannel,
    pub beep_channel: AudioChannel,
    pub ltc_volume: f32,
    pub beep_volume: f32,
    pub beep_frequency: f32,

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

    // Device state
    pub devices: Vec<AudioDeviceInfo>,
    pub selected_device: usize,
    pub audio_initialized: bool,

    // Status
    pub status_message: String,
    pub system_time: String,
    pub battery_status: String,

    // Audio core
    pub audio_core: Mutex<AudioCore>,
}

impl Default for AppState {
    fn default() -> Self {
        let start_tc = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        Self {
            theme: Theme::Dark,
            is_playing: false,
            is_locked: false,
            start_timecode: start_tc,
            current_timecode: start_tc,
            fps_index: 1, // 25 fps PAL
            ltc_channel: AudioChannel::Right,
            beep_channel: AudioChannel::Left,
            ltc_volume: 0.7,
            beep_volume: 0.8,
            beep_frequency: 1000.0,
            scene: 1,
            take: 1,
            roll: String::new(),
            auto_increment_take: true,
            logs: Vec::new(),
            clap_flash_alpha: 0.0,
            clap_arm_angle: 0.0,
            active_tab: Tab::Clapper,
            devices: Vec::new(),
            selected_device: 0,
            audio_initialized: false,
            status_message: "Ready".to_string(),
            system_time: String::new(),
            battery_status: "Unknown".to_string(),
            audio_core: Mutex::new(AudioCore::new()),
        }
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
            }
            Err(e) => {
                self.status_message = format!("Device error: {}", e);
            }
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

        let core = self.audio_core.lock().unwrap();
        match core.init_output(&device_id, SAMPLE_RATE, BUFFER_SIZE) {
            Ok(()) => {
                self.audio_initialized = true;
                self.status_message = "Audio initialized".to_string();
            }
            Err(e) => {
                self.status_message = format!("Audio init failed: {}", e);
            }
        }
    }

    pub fn start_streaming(&mut self) {
        self.ensure_audio_init();
        if !self.audio_initialized {
            return;
        }

        let tc = self.start_timecode;
        let fps = self.fps();
        let channel = self.ltc_channel.as_str().to_string();
        let volume = self.ltc_volume;

        let core = self.audio_core.lock().unwrap();
        match core.start_ltc(tc, fps.fps, fps.drop_frame, channel, volume) {
            Ok(()) => {
                self.is_playing = true;
                self.status_message = "Streaming LTC".to_string();
            }
            Err(e) => {
                self.status_message = format!("Start failed: {}", e);
            }
        }
    }

    pub fn stop_streaming(&mut self) {
        let core = self.audio_core.lock().unwrap();
        if let Err(e) = core.stop_ltc() {
            self.status_message = format!("Stop failed: {}", e);
        }
        drop(core);
        self.is_playing = false;
    }

    pub fn handle_reset(&mut self) {
        let tc = self.start_timecode;
        let core = self.audio_core.lock().unwrap();
        let _ = core.reset_ltc(tc);
        drop(core);
        self.current_timecode = self.start_timecode;
        self.status_message = "Reset".to_string();
    }

    pub fn trigger_clap(&mut self) {
        self.ensure_audio_init();
        if !self.audio_initialized {
            return;
        }

        let freq = self.beep_frequency;
        let volume = self.beep_volume;
        let channel = self.beep_channel.as_str();

        let core = self.audio_core.lock().unwrap();
        let _ = core.play_beep(SAMPLE_RATE, freq, 0.15, volume, channel);
        drop(core);

        // Trigger visual effects
        self.clap_flash_alpha = 1.0;
        self.clap_arm_angle = -120.0; // snap open

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
        let core = self.audio_core.lock().unwrap();
        self.current_timecode = core.current_timecode();
    }
}

impl eframe::App for AppState {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Apply theme
        self.theme.apply(ctx);

        // Animate clap effects
        if self.clap_flash_alpha > 0.0 {
            self.clap_flash_alpha = (self.clap_flash_alpha - 0.08).max(0.0);
        }
        if self.clap_arm_angle < 0.0 {
            self.clap_arm_angle = (self.clap_arm_angle + 6.0).min(0.0);
        }

        // Poll current timecode when playing
        if self.is_playing {
            self.update_clock();
            ctx.request_repaint_after(Duration::from_millis(16));
        }

        // Update system time
        self.system_time = chrono_now_string();
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Apply theme background
        let bg = self.theme.colors().app_bg;
        let clip_rect = ui.clip_rect();
        ui.painter().rect_filled(clip_rect, 0.0, bg);

        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin::same(12)))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 8.0);

                // ── Header ──
                self.render_header(ui);

                ui.add_space(4.0);

                // ── Section 1: Master Studio Time Clock ──
                self.render_clock_section(ui);

                ui.add_space(8.0);

                // ── Section 2: Tabbed deck ──
                self.render_tabbed_deck(ui);

                ui.add_space(4.0);

                // ── Footer status bar ──
                self.render_status_bar(ui);
            });

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
    }
}

impl AppState {
    fn render_header(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            // Logo dot
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(12.0, 12.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 6.0, ACCENT);

            ui.label(
                RichText::new(format!("LTC ENGINE v{}", APP_VERSION))
                    .font(FontId::proportional(16.0))
                    .strong(),
            );

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let icon = if self.theme == Theme::Dark { "☀" } else { "☾" };
                if ui.button(icon).clicked() {
                    self.theme = self.theme.toggle();
                }
            });
        });
    }

    fn render_clock_section(&mut self, ui: &mut Ui) {
        let frame = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(16))
            .corner_radius(8.0);
        frame.show(ui, |ui| {
            ui.vertical(|ui| {
                // Big timecode display
                widgets::clock::render(ui, self);

                ui.add_space(8.0);

                // Status pills
                ui.horizontal(|ui| {
                    let fps = self.fps();
                    widgets::pill(ui, &format!("{} FPS", fps.name), ACCENT);
                    ui.label(format!(
                        "LTC: {} | Beep: {}",
                        self.ltc_channel.label(),
                        self.beep_channel.label()
                    ));
                });

                ui.add_space(8.0);

                // Control buttons
                ui.horizontal(|ui| {
                    if !self.is_locked {
                        if self.is_playing {
                            if ui.button("◼ Stop").clicked() {
                                self.stop_streaming();
                            }
                        } else {
                            let btn = egui::Button::new("▶ Start")
                                .fill(ACCENT.linear_multiply(0.3));
                            if ui.add(btn).clicked() {
                                self.start_streaming();
                            }
                        }
                        if ui.button("🎬 Clap & Beep").clicked() {
                            self.trigger_clap();
                        }
                        if ui.button("↺ Reset").clicked() {
                            self.handle_reset();
                        }
                    }
                    let lock_label = if self.is_locked { "🔓 Unlock" } else { "🔒 Lock" };
                    if ui.button(lock_label).clicked() {
                        self.is_locked = !self.is_locked;
                    }
                });
            });
        });
    }

    fn render_tabbed_deck(&mut self, ui: &mut Ui) {
        let frame = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(12))
            .corner_radius(8.0);
        frame.show(ui, |ui| {
            ui.vertical(|ui| {
                // Tab bar
                ui.horizontal(|ui| {
                    for tab in &[Tab::Clapper, Tab::Settings] {
                        let selected = *tab == self.active_tab;
                        let btn = if selected {
                            egui::Button::new(tab.label())
                                .fill(ACCENT.linear_multiply(0.2))
                        } else {
                            egui::Button::new(tab.label())
                        };
                        if ui.add(btn).clicked() {
                            self.active_tab = *tab;
                        }
                    }
                });

                ui.separator();

                match self.active_tab {
                    Tab::Clapper => widgets::clapper::render(ui, self),
                    Tab::Settings => widgets::settings::render(ui, self),
                }
            });
        });
    }

    fn render_status_bar(&mut self, ui: &mut Ui) {
        widgets::status::render(ui, self);
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
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs();
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}
