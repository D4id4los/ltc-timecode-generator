use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use egui::{Color32, FontId, RichText, Sense, Ui};
use gui_engine::command::GuiCommand;
use gui_engine::config;
use gui_engine::converter::{ChannelMap, ConversionState, OutputNamingMode, RecordingType, SharedConversionState, CancelFlag, DEFAULT_AUDIO_SUFFIX, DEFAULT_VIDEO_SUFFIX};
use gui_engine::file_pattern::{match_files_all_patterns, MatchedGroup};
use gui_engine::state::AppStateSnapshot;
use gui_engine::timecode::FPS_OPTIONS;
use gui_engine::{ArcSwap, AudioEvent};

use crate::theme::{Theme, ACCENT};
use crate::widgets;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

// ── GUI-only types ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Clapper,
    Settings,
    Converter,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Clapper => "Clapper Slate & Logs",
            Tab::Settings => "Signal & Audio Settings",
            Tab::Converter => "File Converter & Export",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotificationType {
    Error,
    Warning,
}

pub struct ToastNotification {
    pub message: String,
    pub notification_type: NotificationType,
    pub created_at: Instant,
    pub duration: Duration,
}

// ── AppState ────────────────────────────────────────────────────────────

pub struct AppState {
    // Engine communication
    pub cmd_tx: Sender<GuiCommand>,
    engine_state: Arc<ArcSwap<AppStateSnapshot>>,
    pub latest: Arc<AppStateSnapshot>,

    // Theme (derived from engine state each frame)
    pub theme: Theme,

    // Tab state
    pub active_tab: Tab,
    pub show_faq: bool,

    // Toast notifications
    pub notifications: Vec<ToastNotification>,

    // Frame timing
    last_frame_time: Option<Instant>,
    has_requested_maximize: bool,

    // Debug log
    pub show_debug_log: bool,
    pub show_app_menu: bool,
    pub app_menu_pos: Option<egui::Pos2>,
    pub log_buffer: Arc<Mutex<gui_engine::log_buffer::LogBuffer>>,

    // ── LTC detection state (GUI-local) ──────────────────────────────
    pub ltc_file_idx: usize,

    // ── Converter state (GUI-local) ──────────────────────────────────
    pub _selected_pattern: usize,
    pub selected_folder: Option<PathBuf>,
    pub selected_files: Option<Vec<PathBuf>>,
    pub file_groups: Option<Vec<MatchedGroup>>,
    pub selected_group_idx: Option<usize>,
    pub channel_map: ChannelMap,
    pub recording_type: RecordingType,
    pub generate_synthetic_video: bool,
    pub split_tracks: bool,
    pub drop_ltc_track: bool,
    pub concat_audio: bool,
    pub container: String,
    pub leave_video_untouched: bool,
    pub video_encoder: String,
    pub audio_encoder: String,
    pub output_folder: PathBuf,
    pub filename_prefix: String,
    pub audio_suffix_template: String,
    pub video_suffix_template: String,
    pub naming_mode: OutputNamingMode,
    pub conversion_state: SharedConversionState,
    pub cancel_flag: CancelFlag,
    pub convert_handle: Option<JoinHandle<()>>,
    pub trim_ltc_start: bool,
    pub trim_offset_secs: f64,
    pub per_file_trim_offsets: Vec<f64>,

    // Latch: generation for which auto-settings (trim/split/drop) have been applied
    ltc_auto_applied_gen: u64,
}

type RestoredFolder = (Option<PathBuf>, Option<Vec<MatchedGroup>>, Option<usize>, ChannelMap, RecordingType, String, OutputNamingMode);

impl AppState {
    pub fn new(
        cmd_tx: Sender<GuiCommand>,
        engine_state: Arc<ArcSwap<AppStateSnapshot>>,
        log_buffer: Arc<Mutex<gui_engine::log_buffer::LogBuffer>>,
    ) -> Self {
        let cfg = config::load();
        Self::new_with_config(cmd_tx, engine_state, log_buffer, cfg)
    }

    pub fn new_with_config(
        cmd_tx: Sender<GuiCommand>,
        engine_state: Arc<ArcSwap<AppStateSnapshot>>,
        log_buffer: Arc<Mutex<gui_engine::log_buffer::LogBuffer>>,
        cfg: gui_engine::config::ConverterConfig,
    ) -> Self {
        let initial = AppStateSnapshot::initial();
        let is_dark = initial.is_dark_theme;

        // Restore last used converter folders from config
        let (selected_folder, file_groups, selected_group_idx, channel_map, recording_type, filename_prefix, naming_mode)
            = Self::restore_input_folder(&cfg);
        let output_folder = cfg.last_output_folder.map(PathBuf::from).unwrap_or_default();

        Self {
            cmd_tx,
            latest: Arc::new(initial),
            engine_state,
            theme: if is_dark { Theme::Dark } else { Theme::Light },
            active_tab: Tab::Clapper,
            show_faq: false,
            notifications: Vec::new(),
            last_frame_time: None,
            has_requested_maximize: false,
            show_debug_log: false,
            show_app_menu: false,
            app_menu_pos: None,
            log_buffer,
            ltc_file_idx: 0,
            _selected_pattern: 0,
            selected_folder,
            selected_files: None,
            file_groups,
            selected_group_idx,
            channel_map,
            recording_type,
            generate_synthetic_video: false,
            split_tracks: false,
            drop_ltc_track: false,
            concat_audio: false,
            container: "mkv".to_string(),
            leave_video_untouched: false,
            video_encoder: "av1".to_string(),
            audio_encoder: "pcm_s24le".to_string(),
            output_folder,
            filename_prefix,
            naming_mode,
            audio_suffix_template: DEFAULT_AUDIO_SUFFIX.to_string(),
            video_suffix_template: DEFAULT_VIDEO_SUFFIX.to_string(),
            conversion_state: Arc::new(Mutex::new(ConversionState::idle())),
            cancel_flag: Arc::new(AtomicBool::new(false)),
            convert_handle: None,
            trim_ltc_start: false,
            trim_offset_secs: 0.0,
            per_file_trim_offsets: Vec::new(),
            ltc_auto_applied_gen: 0,
        }
    }

    fn restore_input_folder(cfg: &config::ConverterConfig) -> RestoredFolder {
        let folder = match cfg.last_input_folder {
            Some(ref p) => PathBuf::from(p),
            None => return (None, None, None, ChannelMap::identity(0), RecordingType::MultiTrackAudio, String::new(), OutputNamingMode::PrefixTemplates),
        };
        if !folder.exists() {
            return (None, None, None, ChannelMap::identity(0), RecordingType::MultiTrackAudio, String::new(), OutputNamingMode::PrefixTemplates);
        }
        let groups = match_files_all_patterns(&folder);
        if groups.is_empty() {
            return (Some(folder), Some(groups), None, ChannelMap::identity(0), RecordingType::MultiTrackAudio, String::new(), OutputNamingMode::PrefixTemplates);
        }
        let num_files = groups[0].files.len();
        let rec_type = groups[0].recording_type.clone();
        let prefix = groups[0].prefix.clone();
        let naming_mode = match rec_type {
            RecordingType::VideoClipSequence => OutputNamingMode::SourceStems,
            RecordingType::MultiTrackAudio => OutputNamingMode::PrefixTemplates,
        };
        (Some(folder), Some(groups), Some(0), ChannelMap::identity(num_files), rec_type, prefix, naming_mode)
    }

    /// Applies auto-settings (trim/split/drop) from a successful decode result,
    /// but only once per `ltc_decode_generation` so user unticks survive.
    fn sync_ltc_decode_auto_settings(&mut self) {
        if let Some(ref result) = self.latest.ltc_decode_result {
            if matches!(result.status, gui_engine::LtcDecodeStatus::Success | gui_engine::LtcDecodeStatus::LowConfidence)
                && self.ltc_auto_applied_gen != self.latest.ltc_decode_generation
            {
                self.trim_offset_secs = result.first_ltc_timecode_secs;
                self.trim_ltc_start = true;
                self.split_tracks = true;
                self.drop_ltc_track = true;
                self.ltc_auto_applied_gen = self.latest.ltc_decode_generation;
            }
        }
    }

    pub fn send(&self, cmd: GuiCommand) {
        let _ = self.cmd_tx.send(cmd);
    }

}

// ── Repaint interval computation ──────────────────────────────────────────

/// egui 0.35 (ContextImpl::request_repaint_after, context.rs:148-151) subtracts
/// `predicted_dt` (fixed at 1/60 s — eframe/egui-winit never override it) from
/// every requested delay to avoid over-shooting.  We must compensate by adding
/// the same duration back, and floor the result to ensure a call with `≤ predicted_dt`
/// after compensation never becomes zero (which would cause an unbounded render
/// storm — see profiling analysis for context).
fn next_repaint_interval(s: &AppStateSnapshot) -> Duration {
    let predicted_dt = Duration::from_secs_f64(1.0 / 60.0);
    let floor = predicted_dt + Duration::from_millis(1);

    let base = if s.is_playing || s.ltc_is_detecting {
        let interval = Duration::from_secs_f64(1.0 / s.fps.max(1.0));
        interval.min(Duration::from_millis(40))
    } else if s.clap_animating {
        Duration::from_secs_f64(1.0 / 60.0)
    } else {
        Duration::from_secs(1)
    };

    (base + predicted_dt).max(floor)
}

// ── egui App ────────────────────────────────────────────────────────────

impl eframe::App for AppState {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Sync latest state from engine
        let snapshot = self.engine_state.load();
        self.latest = Arc::clone(&snapshot);

        // 2. Derive trim offset from LTC result, auto-check split/drop (once per generation)
        self.sync_ltc_decode_auto_settings();

        // 3. Maximize once
        if !self.has_requested_maximize {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
            self.has_requested_maximize = true;
        }

        // 4. Derive theme from engine state and apply
        self.theme = if self.latest.is_dark_theme { Theme::Dark } else { Theme::Light };
        self.theme.apply(ctx);

        // 5. Delta time
        let now = Instant::now();
        let _delta = self
            .last_frame_time
            .map(|t| (now - t).as_secs_f32())
            .unwrap_or(0.0)
            .min(0.1);
        self.last_frame_time = Some(now);

        // 5. Process engine events into toasts
        let events = self.latest.events.clone();
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
                    self.add_notification(NotificationType::Error, "Fatal: audio device unreachable".to_string());
                }
                AudioEvent::RecoveryNeeded { reason } => {
                    self.add_notification(NotificationType::Warning, format!("Audio recovery needed: {}", reason));
                }
                AudioEvent::Underrun => {
                    self.add_notification(NotificationType::Warning, "Audio underrun — samples not keeping up".to_string());
                }
                AudioEvent::FramesDropped { total } => {
                    self.add_notification(NotificationType::Warning, format!("{} frame(s) dropped", total));
                }
            }
        }

        // 7. Repaint scheduling
        ctx.request_repaint_after(next_repaint_interval(&self.latest));

        // 8. Keyboard shortcuts
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

        if toggle_play && !self.latest.is_locked {
            if self.latest.is_playing {
                self.send(GuiCommand::StopLtc);
            } else {
                self.send(GuiCommand::StartLtc);
            }
        }
        if do_clap && !self.latest.is_locked {
            self.send(GuiCommand::Clap);
        }
        if do_reset && !self.latest.is_locked {
            self.send(GuiCommand::Reset);
        }
        if do_lock {
            self.send(GuiCommand::ToggleLock);
        }
        if toggle_debug {
            self.show_debug_log = !self.show_debug_log;
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let bg = self.theme.colors().app_bg;
        let clip_rect = ui.clip_rect();
        ui.painter().rect_filled(clip_rect, 0.0, bg);

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 8.0);
                let max_content = ui.available_width().min(720.0);

                ui.with_layout(egui::Layout::top_down(egui::Align::Center), |ui| {
                    ui.set_max_width(max_content);
                    self.render_header(ui);
                    ui.add_space(4.0);
                    self.render_faq_panel(ui);
                    ui.add_space(4.0);
                    self.render_clock_section(ui);
                    ui.add_space(8.0);
                    self.render_tabbed_deck(ui);
                    ui.add_space(4.0);
                    self.render_status_bar(ui);
                });
            });

        self.render_app_menu(ui);
        self.render_debug_log_window(ui);

        if self.latest.clap_flash_alpha > 0.01 {
            let ctx = ui.ctx();
            let screen = ctx.viewport_rect();
            let alpha = (self.latest.clap_flash_alpha * 255.0) as u8;
            let color = Color32::from_rgba_unmultiplied(255, 255, 255, alpha);
            egui::Area::new(egui::Id::new("clap_flash"))
                .order(egui::Order::Foreground)
                .fixed_pos(screen.min)
                .show(ctx, |ui| {
                    ui.painter().rect_filled(screen, 0.0, color);
                });
        }

        self.render_toasts(ui);
    }
}

// ── Centering helper ────────────────────────────────────────────────────

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

// ── Render methods ──────────────────────────────────────────────────────

impl AppState {
    fn render_header(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        let s = &self.latest;
        ui.horizontal(|ui| {
            let (rect, icon_response) = ui.allocate_exact_size(egui::Vec2::new(34.0, 34.0), Sense::click());
            ui.painter().rect_filled(rect, 4.0, ACCENT);
            let center_y = rect.center().y;
            let line_w = 18.0;
            let line_h = 2.0;
            let l1 = egui::Rect::from_center_size(egui::pos2(rect.center().x, center_y - 3.5), egui::vec2(line_w, line_h));
            let l2 = egui::Rect::from_center_size(egui::pos2(rect.center().x, center_y + 3.5), egui::vec2(line_w, line_h));
            ui.painter().rect_filled(l1, 0.0, Color32::BLACK);
            ui.painter().rect_filled(l2, 0.0, Color32::BLACK);
            if icon_response.clicked() || icon_response.secondary_clicked() {
                self.show_app_menu = true;
                self.app_menu_pos = Some(icon_response.rect.left_bottom());
            }

            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("LTC ENGINE").font(FontId::proportional(16.0)).strong().color(colors.text_title));
                    ui.label(RichText::new(format!("v{}", APP_VERSION)).font(FontId::proportional(16.0)).strong().color(ACCENT));
                });
                ui.label(RichText::new("LINEAR TIMECODE HUB").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
            });

            if ui.available_width() > 300.0 {
                ui.horizontal(|ui| {
                    ui.add_space(20.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("INTERFACE").font(FontId::proportional(8.0)).color(colors.text_muted).strong());
                        let status_text = if s.is_playing { "NATIVE ACTIVE" } else { "STANDBY" };
                        let status_color = if s.is_playing { Color32::from_rgb(0x22, 0xC5, 0x5E) } else { Color32::from_rgb(0xF5, 0x9E, 0x0B) };
                        ui.label(RichText::new(status_text).font(FontId::proportional(10.0)).strong().color(status_color));
                    });
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("SAMPLE RATE").font(FontId::proportional(8.0)).color(colors.text_muted).strong());
                        let rate_khz = s.sample_rate as f32 / 1000.0;
                        ui.label(RichText::new(format!("{:.1} KHZ", rate_khz)).font(FontId::proportional(10.0)).strong().color(colors.text_title));
                    });
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("BUFFER").font(FontId::proportional(8.0)).color(colors.text_muted).strong());
                        let buffer_smp = (s.sample_rate as f64 / s.fps).round() as u32;
                        ui.label(RichText::new(format!("{} SMP", buffer_smp)).font(FontId::proportional(10.0)).strong().color(colors.text_title));
                    });
                });
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.available_width() > 180.0 {
                    let time_str = format!("{} UTC", s.system_time);
                    let frame = egui::Frame::new()
                        .fill(colors.nested_bg)
                        .stroke(egui::Stroke::new(1.0, colors.border_main))
                        .corner_radius(6.0)
                        .inner_margin(egui::Margin::symmetric(8, 4));
                    frame.show(ui, |ui| {
                        ui.label(RichText::new(time_str).font(FontId::monospace(9.0)).color(colors.text_muted));
                    });
                }
                let help_btn = egui::Button::new(RichText::new("?").font(FontId::proportional(12.0)).strong()).fill(colors.nested_bg);
                if ui.add(help_btn).clicked() {
                    self.show_faq = !self.show_faq;
                }
                let icon = if self.theme == Theme::Dark { "\u{2600}\u{FE0F}" } else { "\u{1F319}" };
                let theme_btn = egui::Button::new(RichText::new(icon).font(FontId::proportional(12.0))).fill(colors.nested_bg);
                if ui.add(theme_btn).clicked() {
                    let _ = self.cmd_tx.send(GuiCommand::ToggleTheme);
                }
            });
        });
    }

    fn render_faq_panel(&mut self, ui: &mut Ui) {
        if !self.show_faq { return; }
        let colors = self.theme.colors();
        let frame = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::same(16))
            .corner_radius(12.0)
            .fill(colors.card_bg)
            .stroke(egui::Stroke::new(1.5, colors.border_main));
        frame.show(ui, |ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new("LTC & MULTI-CAM SYNC - QUICK GUIDE").font(FontId::proportional(13.0)).color(colors.text_title).strong());
                ui.add_space(8.0);
                let width = ui.available_width();
                let sections = [
                    ("What is LTC?", "Linear Timecode (LTC) is an analog audio signal encoding SMPTE timecode using Bi-Phase Mark Modulation. Cameras and recorders listen to this audio to align footage in post."),
                    ("Connecting Cameras", "Connect audio output to camera mic input or sync boxes. Set gain manually to a medium level."),
                    ("Synchronizing in Edit", "In DaVinci Resolve or Premiere, right-click files and select 'Update Timecode from Audio Track' to auto-sync."),
                ];
                if width > 500.0 {
                    ui.columns(3, |cols| {
                        for (i, (title, body)) in sections.iter().enumerate() {
                            cols[i].vertical(|ui| {
                                ui.label(RichText::new(*title).font(FontId::proportional(10.0)).color(ACCENT).strong());
                                ui.add_space(4.0);
                                ui.label(RichText::new(*body).font(FontId::proportional(10.5)).color(colors.text_muted));
                            });
                        }
                    });
                } else {
                    for (title, body) in &sections {
                        ui.label(RichText::new(*title).font(FontId::proportional(10.0)).color(ACCENT).strong());
                        ui.label(RichText::new(*body).font(FontId::proportional(10.5)).color(colors.text_muted));
                        ui.add_space(6.0);
                    }
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Got it").clicked() { self.show_faq = false; }
                    });
                });
            });
        });
    }

    fn render_clock_section(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        let s = &self.latest;
        let frame = egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(20, 16))
            .fill(colors.card_bg)
            .stroke(egui::Stroke::new(1.5, colors.border_main))
            .corner_radius(16.0);
        frame.show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.vertical(|ui| {
                // Header: "LINEAR TIMECODE STREAM" + pulsing dot
                ui.horizontal(|ui| {
                    ui.label(RichText::new("LINEAR TIMECODE STREAM").font(FontId::proportional(9.0)).color(colors.text_muted).strong());
                    let dot_color = if s.is_playing { Color32::from_rgb(0x22, 0xC5, 0x5E) } else { Color32::from_rgb(0xF5, 0x9E, 0x0B) };
                    ui.add_space(6.0);
                    let (dot_rect, _) = ui.allocate_exact_size(egui::Vec2::new(8.0, 8.0), Sense::hover());
                    ui.painter().circle_filled(dot_rect.center(), 3.5, dot_color);
                });
                ui.add_space(4.0);

                // Clock display
                widgets::clock::render(ui, self);

                // FPS and routing pills
                ui.add_space(4.0);
                centered_horizontal_row(ui, "fps_route_row", 300.0, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 0.0);
                        let fps_name = FPS_OPTIONS[s.fps_index].name;
                        let pill = egui::Frame::new()
                            .fill(colors.nested_bg)
                            .corner_radius(6.0)
                            .stroke(egui::Stroke::new(1.0, ACCENT))
                            .inner_margin(egui::Margin::symmetric(8, 3));
                        pill.show(ui, |ui| {
                            ui.label(RichText::new(fps_name).font(FontId::monospace(9.0)).color(ACCENT).strong());
                        });
                        let route_pill = egui::Frame::new()
                            .fill(colors.nested_bg)
                            .corner_radius(6.0)
                            .stroke(egui::Stroke::new(1.0, colors.border_main))
                            .inner_margin(egui::Margin::symmetric(8, 3));
                        route_pill.show(ui, |ui| {
                            let route = format!("LTC: {} | CLAP: {}", s.ltc_channel.to_uppercase(), s.beep_channel.to_uppercase());
                            ui.label(RichText::new(route).font(FontId::monospace(8.0)).color(colors.text_muted));
                        });
                    });
                });

                ui.add_space(10.0);

                // Transport buttons
                let btn_w = (ui.available_width() - 24.0) / 4.0;
                centered_horizontal_row(ui, "transport_row", 400.0, |ui| {
                    ui.horizontal(|ui| {
                        // Start / Stop
                        if s.is_playing {
                            let stop_btn = egui::Button::new(RichText::new("■ STOP").strong().color(Color32::WHITE))
                                .fill(Color32::from_rgb(0xDC, 0x26, 0x26))
                                .min_size(egui::vec2(btn_w, 32.0));
                            if ui.add(stop_btn).clicked() && !s.is_locked {
                                self.send(GuiCommand::StopLtc);
                            }
                        } else {
                            let start_btn = egui::Button::new(RichText::new("▶ START").strong().color(Color32::BLACK))
                                .fill(Color32::from_rgb(0x22, 0xC5, 0x5E))
                                .min_size(egui::vec2(btn_w, 32.0));
                            if ui.add(start_btn).clicked() && !s.is_locked {
                                self.send(GuiCommand::StartLtc);
                            }
                        }
                        // Clap
                        let clap_btn = egui::Button::new(RichText::new("CLAP & BEEP").strong().color(Color32::BLACK))
                            .fill(ACCENT)
                            .min_size(egui::vec2(btn_w, 32.0));
                        if ui.add(clap_btn).clicked() && !s.is_locked {
                            self.send(GuiCommand::Clap);
                        }
                        // Reset
                        let reset_btn = egui::Button::new(RichText::new("↺").strong().color(colors.text_title))
                            .fill(colors.nested_bg)
                            .min_size(egui::vec2(btn_w, 32.0));
                        if ui.add(reset_btn).clicked() && !s.is_locked {
                            self.send(GuiCommand::Reset);
                        }
                        // Lock
                        let lock_icon = if s.is_locked { "🔒" } else { "🔓" };
                        let lock_color = if s.is_locked { ACCENT } else { colors.text_muted };
                        let lock_btn = egui::Button::new(RichText::new(lock_icon).color(lock_color))
                            .fill(colors.nested_bg)
                            .min_size(egui::vec2(btn_w, 32.0));
                        if ui.add(lock_btn).clicked() {
                            self.send(GuiCommand::ToggleLock);
                        }
                    });
                });
            });
        });
    }

    fn render_tabbed_deck(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::new(0.0, 0.0);
            for tab in &[Tab::Clapper, Tab::Settings, Tab::Converter] {
                let is_active = *tab == self.active_tab;
                let is_wide = ui.available_width() > 500.0;
                let label = if is_wide {
                    tab.label().to_string()
                } else {
                    match tab {
                        Tab::Clapper => "Clapper".to_string(),
                        Tab::Settings => "Settings".to_string(),
                        Tab::Converter => "Convert".to_string(),
                    }
                };
                let btn = egui::Button::new(
                    RichText::new(label)
                        .font(FontId::proportional(12.0))
                        .color(if is_active { ACCENT } else { colors.text_muted })
                        .strong(),
                )
                .fill(if is_active { colors.card_bg } else { colors.app_bg })
                .corner_radius(8)
                .stroke(if is_active {
                    egui::Stroke::new(2.0, ACCENT)
                } else {
                    egui::Stroke::new(0.0, Color32::TRANSPARENT)
                });
                if ui.add(btn).clicked() {
                    self.active_tab = *tab;
                }
            }
        });

        ui.add_space(4.0);
        match self.active_tab {
            Tab::Clapper => widgets::clapper::render(ui, self),
            Tab::Settings => widgets::settings::render(ui, self),
            Tab::Converter => widgets::converter::render(ui, self),
        }
    }

    fn render_toasts(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx();
        let screen = ctx.viewport_rect();
        let _colors = self.theme.colors();
        let now = Instant::now();

        self.notifications.retain(|t| now - t.created_at < t.duration);

        if !self.notifications.is_empty() {
            let mut y_offset = screen.bottom() - 80.0;
            for toast in self.notifications.iter().rev() {
                let elapsed = now - toast.created_at;
                let fade = (1.0 - (elapsed.as_secs_f32() / toast.duration.as_secs_f32())).max(0.0);
                let alpha = (fade * 255.0) as u8;

                let (r, g, b) = match toast.notification_type {
                    NotificationType::Error => (0xEF, 0x44, 0x44),
                    NotificationType::Warning => (0xF5, 0x9E, 0x0B),
                };

                let bg = Color32::from_rgba_unmultiplied(0x1A, 0x1A, 0x1E, alpha);
                let border = Color32::from_rgba_unmultiplied(r, g, b, alpha);
                let text_c = Color32::from_rgba_unmultiplied(0xFF, 0xFF, 0xFF, alpha);

                let rect = egui::Rect::from_min_size(
                    egui::pos2(screen.center().x - 140.0, y_offset),
                    egui::vec2(280.0, 36.0),
                );
                ui.painter().rect_filled(rect, 8.0, bg);
                ui.painter().rect_stroke(rect, 8.0, egui::Stroke::new(1.0, border), egui::StrokeKind::Inside);
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(egui::pos2(rect.left(), rect.top()), egui::vec2(4.0, rect.height())),
                    0.0,
                    border,
                );
                let text_pos = egui::pos2(rect.left() + 12.0, rect.top() + 10.0);
                ui.painter().text(text_pos, egui::Align2::LEFT_TOP, &toast.message, egui::FontId::proportional(14.0), text_c);

                y_offset -= 44.0;
            }
        }
    }

    fn add_notification(&mut self, nt: NotificationType, message: String) {
        let duration = match nt {
            NotificationType::Error => Duration::from_secs(6),
            _ => Duration::from_secs(4),
        };
        self.notifications.push(ToastNotification {
            message,
            notification_type: nt,
            created_at: Instant::now(),
            duration,
        });
        if self.notifications.len() > 5 {
            self.notifications.remove(0);
        }
    }

    fn render_status_bar(&mut self, ui: &mut Ui) {
        widgets::status::render(ui, self);
    }

    fn render_app_menu(&mut self, ui: &mut Ui) {
        if !self.show_app_menu { return; }
        let pos = self.app_menu_pos.unwrap_or(egui::Pos2::new(0.0, 34.0));
        let colors = self.theme.colors();

        egui::Area::new(egui::Id::new("app_menu"))
            .fixed_pos(pos)
            .order(egui::Order::Foreground)
            .show(ui.ctx(), |ui| {
                let frame = egui::Frame::new()
                    .fill(colors.card_bg)
                    .stroke(egui::Stroke::new(1.0, colors.border_main))
                    .corner_radius(8.0)
                    .inner_margin(egui::Margin::same(8));
                frame.show(ui, |ui| {
                    ui.vertical(|ui| {
                        if ui.selectable_label(false, "Debug Log").clicked() {
                            self.show_debug_log = !self.show_debug_log;
                            self.show_app_menu = false;
                        }
                    });
                });
                if ui.input(|i| i.pointer.any_click()) && !ui.rect_contains_pointer(ui.min_rect()) {
                    self.show_app_menu = false;
                }
            });
    }

    fn render_debug_log_window(&mut self, ui: &mut Ui) {
        if !self.show_debug_log { return; }
        let colors = self.theme.colors();

        let window = egui::Window::new("Debug Log")
            .id(egui::Id::new("debug_log_window"))
            .default_size([400.0, 300.0])
            .resizable(true)
            .collapsible(true);
        window.show(ui.ctx(), |ui| {
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if let Ok(buf) = self.log_buffer.lock() {
                        for entry in buf.entries.iter() {
                            ui.label(
                                RichText::new(entry)
                                    .font(FontId::monospace(10.0))
                                    .color(colors.text_muted),
                            );
                        }
                    }
                });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gui_engine::LtcDecodeStatus;
    use gui_engine::LtcDetectionResult;
    use gui_engine::LtcQualityReport;
    use std::sync::mpsc;

    fn make_successful_result(first_tc_secs: f64) -> LtcDetectionResult {
        LtcDetectionResult {
            status: LtcDecodeStatus::Success,
            detected_fps: 25.0,
            drop_frame: false,
            total_possible_frames: 100,
            valid_frames: 100,
            timecodes: vec![],
            avg_confidence: 0.95,
            details: vec![],
            total_audio_duration_secs: 4.0,
            sample_rate: 48000,
            processing_time_ms: 10.0,
            first_ltc_timecode_secs: first_tc_secs,
            quality: Some(LtcQualityReport {
                score: 0.95,
                grade: "Excellent".to_string(),
                missing_frames: 0,
                gap_count: 0,
                glitch_count: 0,
                edit_count: 0,
                max_drift_secs: 0.001,
                drift_rate: 0.0,
                largest_block: 100,
                summary: "No issues".to_string(),
                gap_edges: vec![],
                glitch_indices: vec![],
            }),
        }
    }

    fn snapshot_with_decode(
        generation: u64,
        result: Option<LtcDetectionResult>,
    ) -> AppStateSnapshot {
        let mut s = AppStateSnapshot::initial();
        s.ltc_decode_generation = generation;
        s.ltc_decode_result = result;
        s
    }

    fn dummy_state() -> Arc<ArcSwap<AppStateSnapshot>> {
        Arc::new(ArcSwap::new(Arc::new(AppStateSnapshot::initial())))
    }

    fn dummy_log_buffer() -> Arc<Mutex<gui_engine::log_buffer::LogBuffer>> {
        Arc::new(Mutex::new(gui_engine::log_buffer::LogBuffer::new(10)))
    }

    // ── next_repaint_interval ──────────────────────────────────────────────

    fn make_snapshot() -> AppStateSnapshot {
        AppStateSnapshot::initial()
    }

    #[test]
    fn repaint_interval_idle_returns_approx_1s() {
        let s = make_snapshot();
        // Idle: not playing, not detecting, not animating.
        let dur = next_repaint_interval(&s);
        assert!(dur > Duration::from_secs(1));
        assert!(dur < Duration::from_secs_f64(1.1)); // 1.0167s is well under 1.1s
    }

    #[test]
    fn repaint_interval_idle_never_below_floor() {
        let s = make_snapshot();
        let dur = next_repaint_interval(&s);
        let floor = Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1);
        assert!(dur >= floor);
    }

    #[test]
    fn repaint_interval_playing_24fps_returns_approx_58ms() {
        let mut s = make_snapshot();
        s.is_playing = true;
        s.fps = 24.0;
        let dur = next_repaint_interval(&s);
        // base = min(41.67ms, 40ms) = 40ms → request = 56.67ms
        assert!(dur > Duration::from_millis(50) && dur < Duration::from_millis(65));
    }

    #[test]
    fn repaint_interval_playing_25fps_returns_approx_57ms() {
        let mut s = make_snapshot();
        s.is_playing = true;
        s.fps = 25.0;
        let dur = next_repaint_interval(&s);
        // base = min(40ms, 40ms) = 40ms → request = 56.67ms
        assert!(dur > Duration::from_millis(50) && dur < Duration::from_millis(65));
    }

    #[test]
    fn repaint_interval_playing_30fps_returns_approx_50ms() {
        let mut s = make_snapshot();
        s.is_playing = true;
        s.fps = 30.0;
        let dur = next_repaint_interval(&s);
        // base = min(33.33ms, 40ms) = 33.33ms → request = 50ms
        assert!(dur > Duration::from_millis(44) && dur < Duration::from_millis(56));
    }

    #[test]
    fn repaint_interval_detecting_same_as_playing() {
        let mut s = make_snapshot();
        s.ltc_is_detecting = true;
        s.fps = 24.0;
        let dur_detect = next_repaint_interval(&s);
        let mut s2 = make_snapshot();
        s2.is_playing = true;
        s2.fps = 24.0;
        let dur_play = next_repaint_interval(&s2);
        assert!((dur_detect.as_secs_f64() - dur_play.as_secs_f64()).abs() < 1e-9);
    }

    #[test]
    fn repaint_interval_clap_animating_returns_approx_33ms() {
        let mut s = make_snapshot();
        s.clap_animating = true;
        let dur = next_repaint_interval(&s);
        // base = 16.67ms → request = 33.33ms
        assert!(dur > Duration::from_millis(28) && dur < Duration::from_millis(38));
    }

    #[test]
    fn repaint_interval_all_non_idle_never_below_floor() {
        let floor = Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1);
        // playing
        let mut s = make_snapshot();
        s.is_playing = true;
        s.fps = 30.0;
        assert!(next_repaint_interval(&s) >= floor);
        // detecting
        let mut s2 = make_snapshot();
        s2.ltc_is_detecting = true;
        s2.fps = 24.0;
        assert!(next_repaint_interval(&s2) >= floor);
        // animating
        let mut s3 = make_snapshot();
        s3.clap_animating = true;
        assert!(next_repaint_interval(&s3) >= floor);
    }

    // ── sync_ltc_decode_auto_settings tests ──────────────────────────────

    fn app_with_no_decode_state() -> super::AppState {
        let (tx, _) = mpsc::channel();
        super::AppState::new_with_config(
            tx,
            dummy_state(),
            dummy_log_buffer(),
            gui_engine::config::ConverterConfig::default(),
        )
    }

    #[test]
    fn auto_set_applies_once_per_decode_generation() {
        let mut app = app_with_no_decode_state();
        app.latest = Arc::new(snapshot_with_decode(1, Some(make_successful_result(10.5))));

        app.sync_ltc_decode_auto_settings();
        assert!(app.trim_ltc_start, "trim_ltc_start should be set");
        assert!(app.split_tracks, "split_tracks should be set");
        assert!(app.drop_ltc_track, "drop_ltc_track should be set");
        assert!((app.trim_offset_secs - 10.5).abs() < 1e-9);

        // Untick all — should stay unticked on subsequent calls
        app.trim_ltc_start = false;
        app.split_tracks = false;
        app.drop_ltc_track = false;
        app.trim_offset_secs = 0.0;

        app.sync_ltc_decode_auto_settings();
        assert!(!app.trim_ltc_start, "should remain unticked after user override");
        assert!(!app.split_tracks, "should remain unticked after user override");
        assert!(!app.drop_ltc_track, "should remain unticked after user override");
        assert!((app.trim_offset_secs).abs() < 1e-9, "offset should not be re-applied");
    }

    #[test]
    fn auto_set_reapplies_on_new_decode_generation() {
        let mut app = app_with_no_decode_state();
        // First decode — applied once
        app.latest = Arc::new(snapshot_with_decode(1, Some(make_successful_result(10.0))));
        app.sync_ltc_decode_auto_settings();
        assert!(app.split_tracks);

        // Untick
        app.split_tracks = false;

        // Simulate a new decode on a different file: generation bumps up, new result
        app.latest = Arc::new(snapshot_with_decode(2, Some(make_successful_result(20.0))));
        app.sync_ltc_decode_auto_settings();
        assert!(app.split_tracks, "should re-apply on new generation");
        assert!((app.trim_offset_secs - 20.0).abs() < 1e-9, "offset should be from latest decode");
    }

    #[test]
    fn auto_set_skips_unsuccessful_status() {
        let mut app = app_with_no_decode_state();
        // NoSyncWord
        app.latest = Arc::new(snapshot_with_decode(1, Some(LtcDetectionResult {
            status: LtcDecodeStatus::NoSyncWord,
            ..make_successful_result(5.0)
        })));
        app.sync_ltc_decode_auto_settings();
        assert!(!app.split_tracks, "should not set for NoSyncWord");

        // Error
        app.latest = Arc::new(snapshot_with_decode(2, Some(LtcDetectionResult {
            status: LtcDecodeStatus::Error { message: "test error".into() },
            ..make_successful_result(5.0)
        })));
        app.sync_ltc_decode_auto_settings();
        assert!(!app.split_tracks, "should not set for Error");
    }

    #[test]
    fn auto_set_noop_without_result() {
        let mut app = app_with_no_decode_state();
        app.latest = Arc::new(snapshot_with_decode(1, None));
        // Should not panic and leave state unchanged
        app.sync_ltc_decode_auto_settings();
        assert!(!app.split_tracks);
        assert!(!app.drop_ltc_track);
        assert!(!app.trim_ltc_start);
        assert!((app.trim_offset_secs).abs() < 1e-9);
    }
}