use std::sync::Mutex;
use std::time::Duration;

use audio_core::{list_audio_devices, AudioCore, AudioDeviceInfo, Timecode};
use egui::{Color32, FontId, RichText, Sense, Ui};

use crate::theme::{Theme, ACCENT};
use crate::widgets;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const SAMPLE_RATE: u32 = 16000;
const BUFFER_SIZE: u32 = 512;

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
    pub show_faq: bool,

    // Device state
    pub devices: Vec<AudioDeviceInfo>,
    pub selected_device: usize,
    pub audio_initialized: bool,

    // Status
    pub status_message: String,
    pub system_time: String,

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
            ltc_channel: AudioChannel::Left,
            beep_channel: AudioChannel::Right,
            ltc_volume: 0.7,
            beep_volume: 0.8,
            beep_frequency: 1000.0,
            scene: 1,
            take: 1,
            roll: "A001".to_string(), // match webapp default Roll
            auto_increment_take: true,
            logs: Vec::new(),
            clap_flash_alpha: 0.0,
            clap_arm_angle: -25.0, // default open state
            active_tab: Tab::Clapper,
            show_faq: false,
            devices: Vec::new(),
            selected_device: 0,
            audio_initialized: false,
            status_message: "Ready".to_string(),
            system_time: String::new(),
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
        // Clapper arm: snaps to 0 (closed) and smoothly opens back to -25 (open)
        if self.clap_arm_angle > -25.0 {
            let target = -25.0;
            let diff = target - self.clap_arm_angle;
            self.clap_arm_angle += diff * 0.15;
            if self.clap_arm_angle <= -24.8 {
                self.clap_arm_angle = -25.0;
            }
        }

        // Poll current timecode when playing
        if self.is_playing {
            self.update_clock();
            ctx.request_repaint_after(Duration::from_millis(16)); // ~60fps for smooth clock
        } else {
            // Slow repaint for system time clock (once per second)
            ctx.request_repaint_after(Duration::from_secs(1));
        }

        // Update system time
        self.system_time = chrono_now_string();

        // Keyboard shortcuts (skip when a text field has focus)
        let any_focused = ctx.memory(|m| m.focused().is_some());
        let (toggle_play, do_clap, do_reset, do_lock) = if !any_focused {
            ctx.input(|i| (
                i.key_pressed(egui::Key::Space),
                i.key_pressed(egui::Key::C),
                i.key_pressed(egui::Key::R),
                i.key_pressed(egui::Key::L),
            ))
        } else {
            (false, false, false, false)
        };

        if toggle_play {
            if self.is_playing {
                self.stop_streaming();
            } else if !self.is_locked {
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
            let avail_w = ui.available_width();
            let max_content = avail_w.min(720.0);
            let indent = ((avail_w - max_content) / 2.0).max(0.0);

            ui.horizontal(|ui| {
                ui.allocate_space(egui::vec2(indent, 0.0));
                ui.vertical(|ui| {
                    ui.set_min_width((avail_w - indent).min(max_content));

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
            // Clapper logo (orange square with two black horizontal lines)
            let (rect, _) = ui.allocate_exact_size(egui::Vec2::new(34.0, 34.0), Sense::hover());
            ui.painter().rect_filled(rect, 4.0, ACCENT);
            let center_y = rect.center().y;
            let line_w = 18.0;
            let line_h = 2.0;
            let line1 = egui::Rect::from_center_size(egui::pos2(rect.center().x, center_y - 3.5), egui::vec2(line_w, line_h));
            let line2 = egui::Rect::from_center_size(egui::pos2(rect.center().x, center_y + 3.5), egui::vec2(line_w, line_h));
            ui.painter().rect_filled(line1, 0.0, Color32::BLACK);
            ui.painter().rect_filled(line2, 0.0, Color32::BLACK);

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
                        ui.label(RichText::new("16.0 KHZ").font(FontId::proportional(10.0)).strong().color(colors.text_title));
                    });
                    ui.add_space(10.0);
                    // Stats column 3: Buffer
                    ui.vertical(|ui| {
                        ui.label(RichText::new("BUFFER").font(FontId::proportional(8.0)).color(colors.text_muted).strong());
                        let buffer_smp = (16000.0 / self.fps().fps).round() as u32;
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
                let icon = if self.theme == Theme::Dark { "\u{2600}" } else { "\u{263E}" };
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
    use chrono::Local;
    Local::now().format("%H:%M:%S").to_string()
}
