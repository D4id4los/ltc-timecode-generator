use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, FontId, RichText, Sense, Ui};
use crate::theme::ThemeColors;
use gui_engine::command::{ConverterCommand, GuiCommand};
use gui_engine::config;
use gui_engine::state::AppStateSnapshot;
use gui_engine::timecode::FPS_OPTIONS;
use gui_engine::{ArcSwap, AudioEvent, JobKind};

use crate::clap_anim::{self, ClapAnim};
use crate::shadows::Shadows;
use crate::theme::{Theme, ACCENT};
use crate::widgets;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

const OFFLOAD_PENDING_TIMEOUT: Duration = Duration::from_secs(3);

// ── GUI-only types ──────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    Clapper,
    Settings,
    Converter,
    Offload,
}

impl Tab {
    /// Navigation label: full descriptive form on wide layouts (row > 500 px),
    /// short form on narrow ones.
    pub fn nav_label(self, wide: bool) -> &'static str {
        match (self, wide) {
            (Tab::Clapper, true) => "Clapper Slate & Logs",
            (Tab::Settings, true) => "Signal & Audio Settings",
            (Tab::Converter, true) => "File Converter & Export",
            (Tab::Offload, true) => "Offload & Ingest",
            (Tab::Clapper, false) => "Clapper",
            (Tab::Settings, false) => "Settings",
            (Tab::Converter, false) => "Convert",
            (Tab::Offload, false) => "Offload",
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
    event_rx: Receiver<AudioEvent>,
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

    /// GUI-local shadow state for every interactive widget, synced against
    /// the engine snapshot each frame via `EditState` (see `shadows.rs`).
    pub sh: Shadows,
    /// Sender-assigned sequence for the next command — the engine acks via
    /// `AppStateSnapshot.applied_command_seq`, and `EditState::sync` treats
    /// a pending edit as applied once `applied_seq >= sent seq`.
    next_send_seq: u64,
    /// Latest engine ack counter (copied from the snapshot each frame).
    pub applied_seq: u64,
    /// Set by `send()` until the engine's ack counter has caught up — keeps
    /// fast repaints running so every command's echo is rendered promptly
    /// (egui would otherwise idle for up to 1 s before showing the result).
    awaiting_echo: bool,

    // Diagnostic: last group decode generation that was logged to avoid spam
    pub last_logged_group_decode_gen: u64,

    // Offload GUI-local state
    /// Last seen offload version — detects when a copy finishes so we can
    /// auto‑switch the converter folder.
    pub offload_last_version: u64,
    /// When the user clicks Start Offload, set to `Instant::now()` so the
    /// next frame forces a fast repaint until the engine publishes
    /// `job(JobKind::OffloadCopy).is_active()`. Resets once active is visible.
    offload_start_pending: Option<Instant>,

    /// GUI-local clap animation, started when `clapper.clap_seq` moves past
    /// [`Self::clap_last_seen`]. Owns the flash/arm decay so the values are
    /// sampled at the GUI's frame rate, not the engine's 25 fps tick.
    pub clap_anim: Option<ClapAnim>,
    /// Last `clapper.clap_seq` observed (seeded from the snapshot at startup
    /// so a mid-session GUI start doesn't animate for an old clap).
    clap_last_seen: u64,
}

impl AppState {
    /// Mark the offload start as pending so the next frame forces a fast repaint
    /// until the engine publishes `job(JobKind::OffloadCopy).is_active()`.
    pub fn mark_offload_start_pending(&mut self) {
        self.offload_start_pending = Some(Instant::now());
    }

    pub fn new(
        cmd_tx: Sender<GuiCommand>,
        event_rx: Receiver<AudioEvent>,
        engine_state: Arc<ArcSwap<AppStateSnapshot>>,
        log_buffer: Arc<Mutex<gui_engine::log_buffer::LogBuffer>>,
    ) -> Self {
        let cfg = config::load();
        Self::new_with_config(cmd_tx, event_rx, engine_state, log_buffer, cfg)
    }

    pub fn new_with_config(
        cmd_tx: Sender<GuiCommand>,
        event_rx: Receiver<AudioEvent>,
        engine_state: Arc<ArcSwap<AppStateSnapshot>>,
        log_buffer: Arc<Mutex<gui_engine::log_buffer::LogBuffer>>,
        cfg: gui_engine::config::ConverterConfig,
    ) -> Self {
        let initial = AppStateSnapshot::initial();
        let is_dark = initial.is_dark_theme;

        let selected_folder = cfg.last_input_folder.as_ref()
            .map(PathBuf::from)
            .filter(|p| p.exists());

        let mut result = Self {
            cmd_tx,
            event_rx,
            latest: Arc::new(initial.clone()),
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
            sh: Shadows::new(&initial),
            next_send_seq: 0,
            applied_seq: initial.applied_command_seq,
            awaiting_echo: false,
            last_logged_group_decode_gen: 0,
            offload_last_version: 0,
            offload_start_pending: None,
            clap_anim: None,
            clap_last_seen: initial.clapper.clap_seq,
        };

        // Tell the engine to scan the restored input folder and auto-select
        // the first recording (deferred by the engine until scan completes).
        // Routed through `send` so the ack-counter sequence stays 1:1 with
        // the engine's `applied_command_seq`.
        if let Some(ref folder) = selected_folder {
            let folder = folder.clone();
            result.send(GuiCommand::Converter(
                gui_engine::command::ConverterCommand::SelectFolder(folder),
            ));
            result.send(GuiCommand::Converter(
                gui_engine::command::ConverterCommand::SelectRecording(0),
            ));
            log::info!(
                "Sent SelectFolder + SelectRecording(0) to engine on startup: {}",
                selected_folder.as_ref().map(|p| p.display().to_string()).unwrap_or_default(),
            );
        }

        result
    }

    /// Send a command to the engine and return the sender-assigned sequence
    /// number. All commands must go through here so the GUI's sequence stays
    /// 1:1 with the engine's `applied_command_seq` ack counter — shadow
    /// edits treat a send as confirmed once `applied_seq >= sent seq`.
    pub fn send(&mut self, cmd: GuiCommand) -> u64 {
        self.next_send_seq += 1;
        self.awaiting_echo = true;
        let _ = self.cmd_tx.send(cmd);
        self.next_send_seq
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

    let ltc_detecting = s.job(JobKind::LtcDecode).is_active();
    let offload_running = s.job(JobKind::OffloadCopy).is_active();

    let base = if s.is_playing || ltc_detecting {
        let interval = Duration::from_secs_f64(1.0 / s.fps().max(1.0));
        interval.min(Duration::from_millis(40))
    } else if offload_running {
        Duration::from_millis(100)
    } else {
        Duration::from_secs(1)
    };

    (base + predicted_dt).max(floor)
}

/// Combine the snapshot-driven repaint interval with the clap animator's
/// own request: clap during playback (or a decode/offload job) must take
/// the min of both delays so neither source stalls the other. `None` from
/// the animator (settled) leaves the snapshot interval untouched.
fn combined_repaint_interval(
    s: &AppStateSnapshot,
    clap_anim: Option<&ClapAnim>,
    now: Instant,
) -> Duration {
    let base = next_repaint_interval(s);
    match clap_anim.and_then(|a| clap_anim::animation_repaint_delay(a, now)) {
        Some(d) => base.min(d),
        None => base,
    }
}

// ── Pure logic helpers (unit-tested below) ───────────────────────────────

/// Maintain keyed offload shadows (value sync happens per-widget at draw
/// time inside the `bound::` wrappers): prune vanished mounts/files and
/// seed new entries from the engine snapshot.
fn sync_offload_shadows(sh: &mut Shadows, snap: &AppStateSnapshot) {
    let mounts: Vec<PathBuf> = snap.offload.cards.iter().map(|c| c.mount.clone()).collect();
    sh.device_names.retain(|mount, _| mounts.contains(mount));
    for card in &snap.offload.cards {
        sh.device_names
            .entry(card.mount.clone())
            .or_insert_with(|| gui_engine::edit_state::EditState::new(card.device_name.clone()));
    }
    let file_keys: Vec<(PathBuf, PathBuf)> = snap.offload.cards.iter()
        .flat_map(|c| c.files.iter().map(move |f| (c.mount.clone(), f.path.clone())))
        .collect();
    sh.file_selection.retain(|k, _| file_keys.contains(k));
    for card in &snap.offload.cards {
        for (i, file) in card.files.iter().enumerate() {
            let selected = card.selected.get(i).copied().unwrap_or(false);
            sh.file_selection
                .entry((card.mount.clone(), file.path.clone()))
                .or_insert_with(|| gui_engine::edit_state::EditState::new(selected));
        }
    }
}

/// Map a drained engine audio event to a toast notification, if the event
/// deserves one. Pure decision — the caller performs the mutation.
fn audio_event_to_notification(evt: &AudioEvent) -> Option<(NotificationType, String)> {
    match evt {
        AudioEvent::StreamError(msg) => {
            Some((NotificationType::Error, format!("Audio stream error: {}", msg)))
        }
        AudioEvent::StreamDied => {
            Some((NotificationType::Error, "Audio stream has died — re-initialize device".to_string()))
        }
        AudioEvent::StreamRecovering { attempt } => {
            Some((NotificationType::Warning, format!("Audio stream recovering (attempt {})", attempt)))
        }
        AudioEvent::StreamDead => {
            Some((NotificationType::Error, "Fatal: audio device unreachable".to_string()))
        }
        AudioEvent::RecoveryNeeded { reason } => {
            Some((NotificationType::Warning, format!("Audio recovery needed: {}", reason)))
        }
        AudioEvent::Underrun => {
            Some((NotificationType::Warning, "Audio underrun — samples not keeping up".to_string()))
        }
        AudioEvent::FramesDropped { total } => {
            Some((NotificationType::Warning, format!("{} frame(s) dropped", total)))
        }
    }
}

/// Repaint delay for the command-echo round-trip: `None` when the engine's
/// ack counter has caught up (echo confirmed), otherwise the fast-repaint
/// delay so the echo is rendered on the next frame instead of waiting out
/// the idle repaint interval (~40 ms engine tick + egui predicted_dt
/// compensation, floored like `next_repaint_interval`).
fn echo_repaint_delay(applied: u64, next: u64) -> Option<Duration> {
    if applied >= next {
        None
    } else {
        let floor = Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1);
        Some((Duration::from_millis(40) + Duration::from_secs_f64(1.0 / 60.0)).max(floor))
    }
}

/// Commands the current keyboard-shortcut press set maps to. Pure decision —
/// the caller performs the sends. Lock-independent guards (play/clap/reset
/// are ignored while locked) are applied here; `ToggleLock` is always sent.
fn shortcut_commands(
    toggle_play: bool,
    do_clap: bool,
    do_reset: bool,
    do_lock: bool,
    is_playing: bool,
    is_locked: bool,
) -> Vec<GuiCommand> {
    let mut cmds = Vec::new();
    if toggle_play && !is_locked {
        cmds.push(if is_playing { GuiCommand::StopLtc } else { GuiCommand::StartLtc });
    }
    if do_clap && !is_locked {
        cmds.push(GuiCommand::Clap);
    }
    if do_reset && !is_locked {
        cmds.push(GuiCommand::Reset);
    }
    if do_lock {
        cmds.push(GuiCommand::ToggleLock);
    }
    cmds
}

/// Offload-completion watch: `Some((new_version, parent))` when the engine's
/// offload version has moved past `last_version`, carrying the fresh parent
/// folder (if the engine published one) for the converter auto-switch.
fn offload_just_finished(
    snap: &AppStateSnapshot,
    last_version: u64,
) -> Option<(u64, Option<PathBuf>)> {
    let version = snap.offload.last_offload_version;
    if version == last_version {
        return None;
    }
    Some((version, snap.offload.last_offload_parent.clone()))
}

// ── egui App ────────────────────────────────────────────────────────────

impl eframe::App for AppState {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 1. Sync latest state from engine
        let snapshot = self.engine_state.load();
        self.latest = Arc::clone(&snapshot);
        self.applied_seq = snapshot.applied_command_seq;

        // 2. Maintain keyed offload shadows
        sync_offload_shadows(&mut self.sh, &self.latest);

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

        // 5. Process engine events into toasts — drained from the
        //    audio-event channel, not the snapshot (one-shot mailbox).
        while let Ok(evt) = self.event_rx.try_recv() {
            if let Some((nt, msg)) = audio_event_to_notification(&evt) {
                self.add_notification(nt, msg);
            }
        }

        // 5a. Clap-seq watch: the engine bumps `clapper.clap_seq` once per
        //     clap (event-like); a change starts the GUI-local animator.
        //     Settled animators clear themselves so idle ticks cost nothing.
        if self.latest.clapper.clap_seq != self.clap_last_seen {
            self.clap_last_seen = self.latest.clapper.clap_seq;
            self.clap_anim = Some(ClapAnim::new(self.latest.clapper.clap_seq, now));
        }
        if let Some(anim) = self.clap_anim {
            if anim.is_settled(now) {
                self.clap_anim = None;
            }
        }

        // 7. Repaint scheduling
        ctx.request_repaint_after(combined_repaint_interval(&self.latest, self.clap_anim.as_ref(), now));

        // 7a. While commands await the engine ack, keep fast repaints so the
        //     echo round-trip (~40 ms engine tick) is rendered on the next
        //     frame instead of waiting out the idle repaint interval.
        if self.awaiting_echo {
            match echo_repaint_delay(self.applied_seq, self.next_send_seq) {
                None => self.awaiting_echo = false,
                Some(delay) => ctx.request_repaint_after(delay),
            }
        }

        // 7b. Offload start-pending latch: force fast repaints until the engine
        //     publishes is_active() (or timeout) so the Start→Cancel button
        //     switch appears on the very next frame after engine publish.
        if let Some(pending_since) = self.offload_start_pending {
            let offload_running = self.latest.job(JobKind::OffloadCopy).is_active();
            if offload_running || now.duration_since(pending_since) >= OFFLOAD_PENDING_TIMEOUT {
                self.offload_start_pending = None;
            } else {
                ctx.request_repaint_after(Duration::from_millis(40));
            }
        }

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

        for cmd in shortcut_commands(
            toggle_play, do_clap, do_reset, do_lock,
            self.latest.is_playing, self.latest.is_locked,
        ) {
            self.send(cmd);
        }
        if toggle_debug {
            self.show_debug_log = !self.show_debug_log;
        }

        // 9. Auto-switch converter folder when an offload completes
        if let Some((_version, parent)) = offload_just_finished(&self.latest, self.offload_last_version) {
            self.offload_last_version = self.latest.offload.last_offload_version;
            if let Some(path) = parent {
                log::info!("Offload completed — auto-switching converter folder to {:?}", path);
                // Preset the LTC source to the second track, mirroring the
                // previous merge-buffer behavior, then switch the folder.
                widgets::bound::set_value(
                    self,
                    |s| &mut s.sh.conv.ltc_file_idx,
                    1,
                    |v| GuiCommand::Converter(ConverterCommand::SetLtcFileIndex(v)),
                );
                self.send(GuiCommand::Converter(
                    gui_engine::command::ConverterCommand::SelectFolder(path.clone()),
                ));
                self.send(GuiCommand::Converter(
                    gui_engine::command::ConverterCommand::SelectRecording(0),
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

        let flash_alpha = self.clap_anim
            .map(|a| clap_anim::flash_alpha_at(a.elapsed(Instant::now())))
            .unwrap_or(0.0);
        if flash_alpha > 0.01 {
            let ctx = ui.ctx();
            let screen = ctx.viewport_rect();
            let alpha = (flash_alpha * 255.0) as u8;
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

// ── Clock-section helpers ───────────────────────────────────────────────

/// Routing pill text: `LTC: LEFT | CLAP: RIGHT` (uppercase channel names).
fn route_label(ltc: gui_engine::ChannelSel, beep: gui_engine::ChannelSel) -> String {
    format!("LTC: {} | CLAP: {}", ltc.as_str().to_uppercase(), beep.as_str().to_uppercase())
}

/// One transport button with the shared size/fill shape; returns whether it
/// was clicked. Collapses the four near-identical button blocks.
fn transport_button(
    ui: &mut Ui,
    width: f32,
    label: RichText,
    fill: Color32,
) -> bool {
    let btn = egui::Button::new(label)
        .fill(fill)
        .min_size(egui::vec2(width, 32.0));
    ui.add(btn).clicked()
}

/// Start/Stop button: green while stopped, red while playing. Gated by the
/// lock guard like every transport action.
fn render_start_stop(ui: &mut Ui, state: &mut AppState, s: &AppStateSnapshot, btn_w: f32) {
    if s.is_playing {
        if transport_button(ui, btn_w, RichText::new("■ STOP").strong().color(Color32::WHITE), Color32::from_rgb(0xDC, 0x26, 0x26))
            && !s.is_locked
        {
            state.send(GuiCommand::StopLtc);
        }
    } else {
        if transport_button(ui, btn_w, RichText::new("▶ START").strong().color(Color32::BLACK), Color32::from_rgb(0x22, 0xC5, 0x5E))
            && !s.is_locked
        {
            state.send(GuiCommand::StartLtc);
        }
    }
}

/// Clap & beep button (lock-guarded).
fn render_clap_button(ui: &mut Ui, state: &mut AppState, s: &AppStateSnapshot, btn_w: f32) {
    if transport_button(ui, btn_w, RichText::new("CLAP & BEEP").strong().color(Color32::BLACK), ACCENT) && !s.is_locked {
        state.send(GuiCommand::Clap);
    }
}

/// Reset button (lock-guarded).
fn render_reset_button(ui: &mut Ui, state: &mut AppState, colors: ThemeColors, s: &AppStateSnapshot, btn_w: f32) {
    if transport_button(ui, btn_w, RichText::new("↺").strong().color(colors.text_title), colors.nested_bg) && !s.is_locked {
        state.send(GuiCommand::Reset);
    }
}

/// Lock toggle button (always clickable — it *is* the lock).
fn render_lock_button(ui: &mut Ui, state: &mut AppState, colors: ThemeColors, s: &AppStateSnapshot, btn_w: f32) {
    let lock_icon = if s.is_locked { "🔒" } else { "🔓" };
    let lock_color = if s.is_locked { ACCENT } else { colors.text_muted };
    if transport_button(ui, btn_w, RichText::new(lock_icon).color(lock_color), colors.nested_bg) {
        state.send(GuiCommand::ToggleLock);
    }
}

/// The Start/Stop · Clap · Reset · Lock row (egui-bound; sends commands).
fn render_transport_buttons(ui: &mut Ui, state: &mut AppState, s: &AppStateSnapshot, btn_w: f32) {
    let colors = state.theme.colors();
    centered_horizontal_row(ui, "transport_row", 400.0, |ui| {
        ui.horizontal(|ui| {
            render_start_stop(ui, state, s, btn_w);
            render_clap_button(ui, state, s, btn_w);
            render_reset_button(ui, state, colors, s, btn_w);
            render_lock_button(ui, state, colors, s, btn_w);
        });
    });
}

// ── Render methods ──────────────────────────────────────────────────────

impl AppState {
    fn render_header(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        let s = std::sync::Arc::clone(&self.latest);
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
                        let buffer_smp = (s.sample_rate as f64 / s.fps()).round() as u32;
                        ui.label(RichText::new(format!("{} SMP", buffer_smp)).font(FontId::proportional(10.0)).strong().color(colors.text_title));
                    });
                });
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.available_width() > 180.0 {
                    let time_str = format!("{} UTC", gui_engine::timecode::chrono_now_string());
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
                    self.send(GuiCommand::ToggleTheme);
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
        let s = std::sync::Arc::clone(&self.latest);
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
                            let route = route_label(s.ltc_channel, s.beep_channel);
                            ui.label(RichText::new(route).font(FontId::monospace(8.0)).color(colors.text_muted));
                        });
                    });
                });

                ui.add_space(10.0);

                // Transport buttons
                let btn_w = (ui.available_width() - 24.0) / 4.0;
                render_transport_buttons(ui, self, &s, btn_w);
            });
        });
    }

    fn render_tabbed_deck(&mut self, ui: &mut Ui) {
        let colors = self.theme.colors();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing = egui::Vec2::new(0.0, 0.0);
            for tab in &[Tab::Clapper, Tab::Settings, Tab::Converter, Tab::Offload] {
                let is_active = *tab == self.active_tab;
                let label = tab.nav_label(ui.available_width() > 500.0).to_string();
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
    use gui_engine::job::{JobStatus, ProgressSnapshot};
    use gui_engine::JobPhase;

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

    fn with_ltc_detecting(mut s: AppStateSnapshot) -> AppStateSnapshot {
        s.jobs.insert(JobKind::LtcDecode, JobStatus { progress: ProgressSnapshot { phase: JobPhase::Running, fraction: 0.0, message: String::new(), speed: None, units: Vec::new(), log: String::new() }, error: None });
        s
    }

    fn with_offload_running(mut s: AppStateSnapshot) -> AppStateSnapshot {
        s.jobs.insert(JobKind::OffloadCopy, JobStatus { progress: ProgressSnapshot { phase: JobPhase::Running, fraction: 0.0, message: String::new(), speed: None, units: Vec::new(), log: String::new() }, error: None });
        s
    }

    #[test]
    fn repaint_interval_idle_returns_approx_1s() {
        let s = make_snapshot();
        // Idle: not playing, not detecting.
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
        s.fps_index = 0;
        let dur = next_repaint_interval(&s);
        // base = min(41.67ms, 40ms) = 40ms → request = 56.67ms
        assert!(dur > Duration::from_millis(50) && dur < Duration::from_millis(65));
    }

    #[test]
    fn repaint_interval_playing_25fps_returns_approx_57ms() {
        let mut s = make_snapshot();
        s.is_playing = true;
        s.fps_index = 1;
        let dur = next_repaint_interval(&s);
        // base = min(40ms, 40ms) = 40ms → request = 56.67ms
        assert!(dur > Duration::from_millis(50) && dur < Duration::from_millis(65));
    }

    #[test]
    fn repaint_interval_playing_30fps_returns_approx_50ms() {
        let mut s = make_snapshot();
        s.is_playing = true;
        s.fps_index = 4;
        let dur = next_repaint_interval(&s);
        // base = min(33.33ms, 40ms) = 33.33ms → request = 50ms
        assert!(dur > Duration::from_millis(44) && dur < Duration::from_millis(56));
    }

    #[test]
    fn repaint_interval_detecting_same_as_playing() {
        let s = with_ltc_detecting(make_snapshot());
        // s.fps() derives from fps_index, but we need 24 fps for the test.
        // We adjust the index directly.
        let mut s_with_fps = s.clone();
        s_with_fps.fps_index = 0;
        let dur_detect = next_repaint_interval(&s_with_fps);
        let mut s2 = make_snapshot();
        s2.is_playing = true;
        s2.fps_index = 0;
        let dur_play = next_repaint_interval(&s2);
        assert!((dur_detect.as_secs_f64() - dur_play.as_secs_f64()).abs() < 1e-9);
    }

    #[test]
    fn repaint_interval_combined_with_running_animator_takes_the_min() {
        let s = make_snapshot();
        let now = Instant::now();
        let anim = crate::clap_anim::ClapAnim::new(1, now);
        // Idle base ~1.0167 s; a running clap animator must shorten it to
        // the animator's ~33 ms request (clap while idle).
        let dur = combined_repaint_interval(&s, Some(&anim), now);
        assert!(dur <= Duration::from_millis(35), "animator delay must win over idle, got {:?}", dur);

        // Clap during playback: the animator delay must not stall the
        // faster playing interval, and vice versa.
        let mut playing = make_snapshot();
        playing.is_playing = true;
        playing.fps_index = 4; // 30 fps → ~50 ms request
        let dur = combined_repaint_interval(&playing, Some(&anim), now);
        assert!(dur <= Duration::from_millis(35), "min(animator, playing) must hold, got {:?}", dur);
    }

    #[test]
    fn repaint_interval_combined_with_settled_animator_uses_snapshot_only() {
        let s = make_snapshot();
        let now = Instant::now();
        let anim = crate::clap_anim::ClapAnim::new(1, now - Duration::from_secs(10));
        assert!(crate::clap_anim::animation_repaint_delay(&anim, now).is_none(),
            "settled animator must not request repaints (render-storm guard)");
        let with_anim = combined_repaint_interval(&s, Some(&anim), now);
        let without = combined_repaint_interval(&s, None, now);
        assert_eq!(with_anim, without, "a settled animator must not alter the snapshot interval");
    }

    #[test]
    fn repaint_interval_all_non_idle_never_below_floor() {
        let floor = Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1);
        // playing
        let mut s = make_snapshot();
        s.is_playing = true;
        s.fps_index = 4;
        assert!(next_repaint_interval(&s) >= floor);
        // detecting
        let s2 = with_ltc_detecting(make_snapshot());
        // s2.fps defaults to 25.0 from initial(), which is fine for floor test
        assert!(next_repaint_interval(&s2) >= floor);
        // clap animator running (combined rule, floor applied inside both)
        let now = Instant::now();
        let anim = crate::clap_anim::ClapAnim::new(1, now);
        assert!(combined_repaint_interval(&make_snapshot(), Some(&anim), now) >= floor);
        // offload running
        let s4 = with_offload_running(make_snapshot());
        assert!(next_repaint_interval(&s4) >= floor);
    }

    #[test]
    fn repaint_interval_offload_running_returns_approx_100ms() {
        let s = with_offload_running(make_snapshot());
        let dur = next_repaint_interval(&s);
        // base = 100ms, plus predicted_dt 16.67ms = 116.67ms
        assert!(dur > Duration::from_millis(110) && dur < Duration::from_millis(130));
    }

    // ── apply_group_selection tests ─────────────────────────────────────

    fn app_with_decode_state() -> (super::AppState, std::sync::mpsc::Receiver<GuiCommand>) {
        let (tx, rx) = mpsc::channel();
        let (_event_tx, event_rx) = mpsc::channel();
        let app = super::AppState::new_with_config(
            tx,
            event_rx,
            dummy_state(),
            dummy_log_buffer(),
            gui_engine::config::ConverterConfig::default(),
        );
        (app, rx)
    }

    #[test]
    fn apply_group_selection_audio_sends_and_resets_shadow() {
        use gui_engine::file_pattern::MatchedGroup;
        use crate::widgets::converter::apply_group_selection;

        let (mut app, rx) = app_with_decode_state();

        let group = MatchedGroup {
            prefix: "TEST".to_string(),
            rel_dir: String::new(),
            recording_type: gui_engine::converter::RecordingType::MultiTrackAudio,
            files: vec![
                PathBuf::from("TEST_S01.wav"),
                PathBuf::from("TEST_S02.wav"),
            ],
        };
        let groups = vec![group];

        app.sh.conv.ltc_file_idx.force_adopt(&5);
        apply_group_selection(&mut app, &groups, 0);

        assert_eq!(*app.sh.conv.ltc_file_idx.value(), 0,
            "apply_group_selection must reset the ltc_file_idx shadow");

        let mut cmds: Vec<GuiCommand> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(cmds.len(), 2);
        assert!(matches!(cmds.remove(0), GuiCommand::ClearRecordingDecodeState));
        assert!(matches!(cmds.remove(0), GuiCommand::Converter(gui_engine::command::ConverterCommand::SelectRecording(0))));
    }

    #[test]
    fn apply_group_selection_video_sends_and_resets_shadow() {
        use gui_engine::file_pattern::MatchedGroup;
        use crate::widgets::converter::apply_group_selection;

        let (mut app, rx) = app_with_decode_state();

        let group = MatchedGroup {
            prefix: "CLIP".to_string(),
            rel_dir: String::new(),
            recording_type: gui_engine::converter::RecordingType::VideoClipSequence,
            files: vec![
                PathBuf::from("GOPR0001.MP4"),
                PathBuf::from("GOPR0002.MP4"),
            ],
        };
        let groups = vec![group];

        apply_group_selection(&mut app, &groups, 0);

        assert_eq!(*app.sh.conv.ltc_file_idx.value(), 0,
            "apply_group_selection must reset the ltc_file_idx shadow");

        let mut cmds: Vec<GuiCommand> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(cmds.len(), 2, "video group should send 2 commands");
        assert!(matches!(cmds.remove(0), GuiCommand::ClearRecordingDecodeState));
        assert!(matches!(cmds.remove(0), GuiCommand::Converter(gui_engine::command::ConverterCommand::SelectRecording(0))));
    }

    #[test]
    fn send_assigns_monotonic_sequences() {
        let (mut app, rx) = app_with_decode_state();
        assert_eq!(app.send(GuiCommand::Clap), 1);
        assert_eq!(app.send(GuiCommand::Reset), 2);
        assert_eq!(app.next_send_seq, 2);
        assert_eq!(rx.iter().take(2).count(), 2);
    }

    // ── logic() helper extractions ───────────────────────────────────────

    fn card_with_files(mount: &str, paths: &[&str]) -> gui_engine::offload::SdCardInfo {
        let mut card = gui_engine::offload::SdCardInfo {
            mount: PathBuf::from(mount),
            volume_label: String::new(),
            device_name: "CAM".to_string(),
            name_source: gui_engine::offload::DeviceNameSource::Manual,
            media_file_count: paths.len(),
            total_bytes: 0,
            files: Vec::new(),
            selected: Vec::new(),
            selected_count: 0,
            selected_bytes: 0,
        };
        for p in paths {
            card.files.push(gui_engine::offload::OffloadFileInfo {
                path: PathBuf::from(p),
                name: p.to_string(),
                size_bytes: 0,
                modified: None,
            });
            card.selected.push(false);
        }
        card
    }

    fn snapshot_with_card(card: gui_engine::offload::SdCardInfo) -> AppStateSnapshot {
        let mut s = AppStateSnapshot::initial();
        s.offload.cards.push(card);
        s
    }

    #[test]
    fn sync_offload_shadows_seeds_device_names_per_mount() {
        let snap = snapshot_with_card(card_with_files("/mnt/a", &[]));
        let mut sh = Shadows::new(&AppStateSnapshot::initial());
        assert!(sh.device_names.is_empty());

        sync_offload_shadows(&mut sh, &snap);

        assert_eq!(sh.device_names.len(), 1);
        assert_eq!(sh.device_names[&PathBuf::from("/mnt/a")].value(), "CAM");
    }

    #[test]
    fn sync_offload_shadows_prunes_vanished_mounts_and_files() {
        let mut sh = Shadows::new(&AppStateSnapshot::initial());
        sh.device_names.insert(PathBuf::from("/mnt/old"), gui_engine::edit_state::EditState::new("X".to_string()));
        sh.file_selection.insert((PathBuf::from("/mnt/old"), PathBuf::from("f.wav")), gui_engine::edit_state::EditState::new(true));

        let snap = snapshot_with_card(card_with_files("/mnt/new", &["f.wav"]));
        sync_offload_shadows(&mut sh, &snap);

        assert!(!sh.device_names.contains_key(&PathBuf::from("/mnt/old")));
        assert!(!sh.file_selection.contains_key(&(PathBuf::from("/mnt/old"), PathBuf::from("f.wav"))));
        assert!(sh.device_names.contains_key(&PathBuf::from("/mnt/new")));
        assert!(sh.file_selection.contains_key(&(PathBuf::from("/mnt/new"), PathBuf::from("f.wav"))));
    }

    #[test]
    fn sync_offload_shadows_seeds_file_selection_from_engine_default() {
        let mut card = card_with_files("/mnt/a", &["f1.wav", "f2.wav"]);
        card.selected = vec![false, true];
        let snap = snapshot_with_card(card);

        let mut sh = Shadows::new(&AppStateSnapshot::initial());
        sync_offload_shadows(&mut sh, &snap);

        assert!(!*sh.file_selection[&(PathBuf::from("/mnt/a"), PathBuf::from("f1.wav"))].value());
        assert!(*sh.file_selection[&(PathBuf::from("/mnt/a"), PathBuf::from("f2.wav"))].value());
    }

    #[test]
    fn audio_event_to_notification_maps_all_variants() {
        let (nt, _) = audio_event_to_notification(&AudioEvent::StreamError("boom".into())).unwrap();
        assert_eq!(nt, NotificationType::Error);
        let (nt, _) = audio_event_to_notification(&AudioEvent::StreamDied).unwrap();
        assert_eq!(nt, NotificationType::Error);
        let (nt, _) = audio_event_to_notification(&AudioEvent::StreamRecovering { attempt: 2 }).unwrap();
        assert_eq!(nt, NotificationType::Warning);
        let (nt, _) = audio_event_to_notification(&AudioEvent::StreamDead).unwrap();
        assert_eq!(nt, NotificationType::Error);
        let (nt, _) = audio_event_to_notification(&AudioEvent::RecoveryNeeded { reason: "x".into() }).unwrap();
        assert_eq!(nt, NotificationType::Warning);
        let (nt, _) = audio_event_to_notification(&AudioEvent::Underrun).unwrap();
        assert_eq!(nt, NotificationType::Warning);
        let (nt, msg) = audio_event_to_notification(&AudioEvent::FramesDropped { total: 7 }).unwrap();
        assert_eq!(nt, NotificationType::Warning);
        assert!(msg.contains('7'), "dropped-frame count must appear in the message");
    }

    #[test]
    fn echo_repaint_delay_none_once_ack_caught_up() {
        assert!(echo_repaint_delay(5, 5).is_none());
        assert!(echo_repaint_delay(6, 5).is_none());
    }

    #[test]
    fn echo_repaint_delay_some_while_pending_above_floor() {
        let delay = echo_repaint_delay(5, 6).expect("pending echo must request a repaint");
        let floor = Duration::from_secs_f64(1.0 / 60.0) + Duration::from_millis(1);
        assert!(delay >= floor);
    }

    #[test]
    fn shortcut_commands_toggle_play_chooses_start_or_stop() {
        let cmds = shortcut_commands(true, false, false, false, false, false);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], GuiCommand::StartLtc));

        let cmds = shortcut_commands(true, false, false, false, true, false);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], GuiCommand::StopLtc));
    }

    #[test]
    fn shortcut_commands_suppressed_while_locked_except_lock_toggle() {
        let cmds = shortcut_commands(true, true, true, false, true, true);
        assert!(cmds.is_empty(), "play/clap/reset must be suppressed while locked");

        let cmds = shortcut_commands(true, true, true, true, true, true);
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], GuiCommand::ToggleLock));
    }

    #[test]
    fn shortcut_commands_full_unlocked_set() {
        let cmds = shortcut_commands(true, true, true, false, false, false);
        assert_eq!(cmds.len(), 3);
        assert!(matches!(cmds[0], GuiCommand::StartLtc));
        assert!(matches!(cmds[1], GuiCommand::Clap));
        assert!(matches!(cmds[2], GuiCommand::Reset));
    }

    #[test]
    fn offload_just_finished_none_while_version_unchanged() {
        let s = AppStateSnapshot::initial();
        assert!(offload_just_finished(&s, 0).is_none());
    }

    #[test]
    fn offload_just_finished_carries_new_version_and_parent() {
        let mut s = AppStateSnapshot::initial();
        s.offload.last_offload_version = 3;
        s.offload.last_offload_parent = Some(PathBuf::from("/out"));

        let (version, parent) = offload_just_finished(&s, 2).expect("version moved");
        assert_eq!(version, 3);
        assert_eq!(parent, Some(PathBuf::from("/out")));
    }

    #[test]
    fn offload_just_finished_allows_missing_parent() {
        let mut s = AppStateSnapshot::initial();
        s.offload.last_offload_version = 1;
        let (_, parent) = offload_just_finished(&s, 0).expect("version moved");
        assert_eq!(parent, None);
    }

    #[test]
    fn route_label_uppercases_both_channels() {
        // test-lint: allow(text-pin): formatter output is the contract
        let label = route_label(gui_engine::ChannelSel::Left, gui_engine::ChannelSel::Right);
        assert_eq!(label, "LTC: LEFT | CLAP: RIGHT");
    }

    #[test]
    fn shadows_seed_from_initial_snapshot() {
        let (mut app, _rx) = app_with_decode_state();
        app.sh.conv.ltc_file_idx.force_adopt(&2);
        assert_eq!(*app.sh.conv.ltc_file_idx.value(), 2);
        // Adoption via sync when unfocused and no pending edit.
        app.sh.conv.ltc_file_idx.sync(&0, 0, Instant::now());
        assert_eq!(*app.sh.conv.ltc_file_idx.value(), 0);
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
        let (_event_tx, event_rx) = mpsc::channel();
        let _app = super::AppState::new_with_config(
            tx,
            event_rx,
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

        assert_eq!(cmds.len(), 2, "startup must send SelectFolder + SelectRecording(0)");
        assert!(
            matches!(&cmds[1], GuiCommand::Converter(gui_engine::command::ConverterCommand::SelectRecording(0))),
            "second command must be SelectRecording(0), got: {:?}", cmds[1]
        );
    }

    // test-lint: allow(text-pin): label mapping is the contract
    #[test]
    fn nav_label_wide_uses_full_names() {
        assert_eq!(Tab::Clapper.nav_label(true), "Clapper Slate & Logs");
        assert_eq!(Tab::Settings.nav_label(true), "Signal & Audio Settings");
        assert_eq!(Tab::Converter.nav_label(true), "File Converter & Export");
        assert_eq!(Tab::Offload.nav_label(true), "Offload & Ingest");
    }

    // test-lint: allow(text-pin): label mapping is the contract
    #[test]
    fn nav_label_narrow_uses_short_names() {
        assert_eq!(Tab::Clapper.nav_label(false), "Clapper");
        assert_eq!(Tab::Settings.nav_label(false), "Settings");
        assert_eq!(Tab::Converter.nav_label(false), "Convert");
        assert_eq!(Tab::Offload.nav_label(false), "Offload");
    }

}
