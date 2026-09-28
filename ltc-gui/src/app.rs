use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, FontId, RichText, Sense, Ui};
use gui_engine::command::{ConverterCommand, GuiCommand};
use gui_engine::config;
use gui_engine::state::{AppStateSnapshot, ConverterUserSettings};
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
    Offload,
}

impl Tab {
    pub fn label(self) -> &'static str {
        match self {
            Tab::Clapper => "Clapper Slate & Logs",
            Tab::Settings => "Signal & Audio Settings",
            Tab::Converter => "File Converter & Export",
            Tab::Offload => "Offload & Ingest",
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

    // ── Converter folder (GUI-local display) ──────────────────────────
    /// Last known converter selected folder — used to detect engine scan
    /// result arrival and for offload auto-switch.
    pub selected_folder: Option<PathBuf>,

    /// Local cache of converter settings, synced from the engine snapshot
    /// every frame.  egui's immediate-mode render functions need `&mut`
    /// access for checkboxes and TextEdits; this cache provides that while
    /// keeping the engine as the true source of truth.  All mutations send
    /// a `GuiCommand` to the engine; the cache is refreshed on the next
    /// `logic()` tick.
    pub local_settings: gui_engine::state::ConverterUserSettings,

    // Diagnostic: last group decode generation that was logged to avoid spam
    pub last_logged_group_decode_gen: u64,

    // Offload GUI-local state
    /// Last seen offload version — detects when a copy finishes so we can
    /// auto‑switch the converter folder.
    pub offload_last_version: u64,
}

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

        let selected_folder = cfg.last_input_folder.as_ref()
            .map(PathBuf::from)
            .filter(|p| p.exists());

        let result = Self {
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
            selected_folder: selected_folder.clone(),
            local_settings: gui_engine::state::ConverterUserSettings::initial(),
            last_logged_group_decode_gen: 0,
            offload_last_version: 0,
        };

        // Tell the engine to scan the restored input folder
        if let Some(ref folder) = selected_folder {
            let _ = result.cmd_tx.send(GuiCommand::Converter(
                gui_engine::command::ConverterCommand::SelectFolder(folder.clone()),
            ));
            log::info!(
                "Sent SelectFolder to engine on startup: {}",
                folder.display(),
            );
        }

        result
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

/// Compare two `ConverterUserSettings` snapshots and emit the
/// `ConverterCommand` variants needed to reconcile `new` into the engine.
///
/// This is the reverse-direction bridge: engine state flows into the GUI
/// via `ArcSwap`, but when the user edits a field in the local copy, that
/// change is detected here and forwarded as a command.
///
/// Channel-map differences use `SwapChannelMapCells` (the only channel-map
/// mutation primitive) by computing a minimal swap sequence.
fn diff_converter_commands(
    old: &ConverterUserSettings,
    new: &ConverterUserSettings,
) -> Vec<ConverterCommand> {
    let mut cmds = Vec::new();

    if old.metadata_only != new.metadata_only {
        cmds.push(ConverterCommand::SetMetadataOnly(new.metadata_only));
    }
    if old.generate_synthetic_video != new.generate_synthetic_video {
        cmds.push(ConverterCommand::SetGenerateSyntheticVideo(new.generate_synthetic_video));
    }
    if old.copy_video != new.copy_video {
        cmds.push(ConverterCommand::SetCopyVideo(new.copy_video));
    }
    if old.split_tracks != new.split_tracks {
        cmds.push(ConverterCommand::SetSplitTracks(new.split_tracks));
    }
    if old.drop_ltc_track != new.drop_ltc_track {
        cmds.push(ConverterCommand::SetDropLtcTrack(new.drop_ltc_track));
    }
    if old.concat_audio != new.concat_audio {
        cmds.push(ConverterCommand::SetConcatAudio(new.concat_audio));
    }
    if old.set_start_from_ltc != new.set_start_from_ltc {
        cmds.push(ConverterCommand::SetStartFromLtc(new.set_start_from_ltc));
    }
    if old.embed_camera_metadata != new.embed_camera_metadata {
        cmds.push(ConverterCommand::SetEmbedCameraMetadata(new.embed_camera_metadata));
    }
    if old.trim_enabled != new.trim_enabled {
        cmds.push(ConverterCommand::SetTrimEnabled(new.trim_enabled));
    }
    if old.ltc_file_idx != new.ltc_file_idx {
        cmds.push(ConverterCommand::SetLtcFileIndex(new.ltc_file_idx));
    }
    if old.naming_pattern != new.naming_pattern {
        cmds.push(ConverterCommand::SetNamingPattern(new.naming_pattern));
    }
    if old.container != new.container {
        cmds.push(ConverterCommand::SetContainer(new.container.clone()));
    }
    if old.video_encoder != new.video_encoder {
        cmds.push(ConverterCommand::SetVideoCodec(new.video_encoder.clone()));
    }
    if old.audio_encoder != new.audio_encoder {
        cmds.push(ConverterCommand::SetAudioEncoder(new.audio_encoder.clone()));
    }
    if old.output_folder != new.output_folder {
        cmds.push(ConverterCommand::SetOutputFolder(new.output_folder.clone()));
    }
    if old.filename_prefix != new.filename_prefix {
        cmds.push(ConverterCommand::SetFilenamePrefix(new.filename_prefix.clone()));
    }
    if old.audio_suffix_template != new.audio_suffix_template {
        cmds.push(ConverterCommand::SetAudioSuffixTemplate(new.audio_suffix_template.clone()));
    }
    if old.video_suffix_template != new.video_suffix_template {
        cmds.push(ConverterCommand::SetVideoSuffixTemplate(new.video_suffix_template.clone()));
    }

    // Channel-map: emit SwapChannelMapCells for each position where the
    // mapping changed.  Only meaningful when dimensions match (engine
    // resizes via identity on probe).
    if old.channel_map.num_channels() == new.channel_map.num_channels() {
        let old_map = old.channel_map.mapping();
        let new_map = new.channel_map.mapping();
        let mut working = old_map.to_vec();
        for i in 0..working.len() {
            if working[i] != new_map[i] {
                if let Some(j) = working.iter().position(|&v| v == new_map[i]) {
                    cmds.push(ConverterCommand::SwapChannelMapCells(i, j));
                    working.swap(i, j);
                }
            }
        }
    }

    cmds
}

// ── egui App ────────────────────────────────────────────────────────────

impl eframe::App for AppState {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Sync latest state from engine
        let snapshot = self.engine_state.load();
        self.latest = Arc::clone(&snapshot);

        // 2. Diff local converter settings against engine snapshot and send
        //    commands for fields that changed on the GUI side (user edits).
        let old = self.local_settings.clone();
        self.local_settings = self.latest.converter.settings.clone();
        for cmd in diff_converter_commands(&old, &self.latest.converter.settings) {
            let _ = self.cmd_tx.send(GuiCommand::Converter(cmd));
        }

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

        // 9. Auto-switch converter folder when an offload completes
        let off_version = self.latest.offload.last_offload_version;
        if off_version != self.offload_last_version {
            self.offload_last_version = off_version;
            if let Some(ref path) = self.latest.offload.last_offload_parent {
                let path = path.clone();
                log::info!("Offload completed — auto-switching converter folder to {:?}", path);
                self.selected_folder = Some(path.clone());
                self.ltc_file_idx = 1;
                let _ = self.cmd_tx.send(GuiCommand::Converter(
                    gui_engine::command::ConverterCommand::SelectFolder(path.clone()),
                ));
            }
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let bg = self.theme.colors().app_bg;
        let clip_rect = ui.clip_rect();
        ui.painter().rect_filled(clip_rect, 0.0, bg);

        egui::ScrollArea::both()
            .id_salt(crate::ids::root_scroll())
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
            for tab in &[Tab::Clapper, Tab::Settings, Tab::Converter, Tab::Offload] {
                let is_active = *tab == self.active_tab;
                let is_wide = ui.available_width() > 500.0;
                let label = if is_wide {
                    tab.label().to_string()
                } else {
                    match tab {
                        Tab::Clapper => "Clapper".to_string(),
                        Tab::Settings => "Settings".to_string(),
                        Tab::Converter => "Convert".to_string(),
                        Tab::Offload => "Offload".to_string(),
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
            Tab::Offload => widgets::offload::render(ui, self),
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
            ui.vertical(|ui| {
                egui::ScrollArea::vertical()
                    .id_salt(crate::ids::debug_log_scroll())
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
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("📋 Copy Log").clicked() {
                            let mut text = String::new();
                            if let Ok(buf) = self.log_buffer.lock() {
                                for entry in buf.entries.iter() {
                                    text.push_str(entry);
                                    text.push('\n');
                                }
                            }
                            ui.ctx().copy_text(text);
                        }
                    });
                });
            });
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

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

    // ── apply_group_selection tests ─────────────────────────────────────

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
    fn apply_group_selection_audio_returns_commands() {
        use gui_engine::file_pattern::MatchedGroup;
        use crate::widgets::converter::apply_group_selection;

        let mut app = app_with_no_decode_state();

        let group = MatchedGroup {
            prefix: "TEST".to_string(),
            rel_dir: String::new(),
            pattern_name: "TASCAM",
            recording_type: gui_engine::converter::RecordingType::MultiTrackAudio,
            files: vec![
                PathBuf::from("TEST_S01.wav"),
                PathBuf::from("TEST_S02.wav"),
            ],
        };
        let groups = vec![group];

        app.ltc_file_idx = 5;
        let cmds = apply_group_selection(&mut app, &groups, 0);

        assert_eq!(app.ltc_file_idx, 0);
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds[0], GuiCommand::ClearRecordingDecodeState));
        assert!(matches!(&cmds[1], GuiCommand::Converter(gui_engine::command::ConverterCommand::SelectRecording(0))));
    }

    #[test]
    fn apply_group_selection_video_returns_commands() {
        use gui_engine::file_pattern::MatchedGroup;
        use crate::widgets::converter::apply_group_selection;

        let mut app = app_with_no_decode_state();

        let group = MatchedGroup {
            prefix: "CLIP".to_string(),
            rel_dir: String::new(),
            pattern_name: "GoPro",
            recording_type: gui_engine::converter::RecordingType::VideoClipSequence,
            files: vec![
                PathBuf::from("GOPR0001.MP4"),
                PathBuf::from("GOPR0002.MP4"),
            ],
        };
        let groups = vec![group];

        let cmds = apply_group_selection(&mut app, &groups, 0);

        assert_eq!(app.ltc_file_idx, 0);
        assert_eq!(cmds.len(), 2, "video group should return 2 commands");
        assert!(matches!(cmds[0], GuiCommand::ClearRecordingDecodeState));
        assert!(matches!(&cmds[1], GuiCommand::Converter(gui_engine::command::ConverterCommand::SelectRecording(0))));
    }

    #[test]
    fn startup_restore_sends_select_folder_to_engine() {
        let dir = tempfile::TempDir::new().unwrap();
        let mp4_path = dir.path().join("C0001.MP4");
        std::fs::write(&mp4_path, b"dummy").unwrap();

        let cfg = gui_engine::config::ConverterConfig {
            last_input_folder: Some(dir.path().to_string_lossy().to_string()),
            ..Default::default()
        };

        let (tx, rx) = mpsc::channel();
        let _app = super::AppState::new_with_config(
            tx,
            dummy_state(),
            dummy_log_buffer(),
            cfg,
        );

        let cmds: Vec<GuiCommand> = std::iter::from_fn(|| rx.try_recv().ok()).collect();

        let first = cmds.first().expect("expected at least one command");
        assert!(
            matches!(first, GuiCommand::Converter(gui_engine::command::ConverterCommand::SelectFolder(p)) if p == dir.path()),
            "first command must be SelectFolder, got: {:?}", first
        );

        assert_eq!(cmds.len(), 1, "only SelectFolder should be sent at startup; SelectRecording is async");
    }
}