#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::command::{ConverterCommand, GuiCommand, OffloadCommand};
use gui_engine::{AudioEvent, ChannelSel};
use gui_engine::config;
use gui_engine::state::AppStateSnapshot;
use gui_engine::timecode::{self, FPS_OPTIONS};
use gui_engine::{ArcSwap, JobKind, SAMPLE_RATE_OPTIONS};
use log::info;
use slint::{ModelRc, SharedString, VecModel};

use crate::poll::{setup_poll_timer, PollContext};
use crate::shadows::Shadows;
use crate::sink::CommandSink;
use crate::theme::set_theme_palette;
use crate::timecode_helpers::set_tc_segments;
use crate::toast::{push_toast, update_toast_model, ToastItem};

mod poll;
mod shadows;
mod sink;
mod theme;
mod timecode_helpers;
mod toast;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

const CHANNEL_CHOICES: [ChannelSel; 3] = [ChannelSel::Left, ChannelSel::Right, ChannelSel::Both];

// ── Pure helpers ─────────────────────────────────────────────────────────
// Extracted verbatim from the callbacks below so they can be unit-tested
// without a running Slint runtime.

/// fps×100 value sent by settings.slint → FPS_OPTIONS index.
fn fps_index_for(fps_x100: f64) -> Option<usize> {
    FPS_OPTIONS.iter().position(|o| (o.fps - fps_x100).abs() < 0.01)
}

/// Sample-rate option label ("48000 Hz") → (rate, option index).
fn sample_rate_index_for(val: &str) -> Option<(u32, usize)> {
    let rate_str = val.split_whitespace().next()?;
    let rate = rate_str.parse::<u32>().ok()?;
    let idx = SAMPLE_RATE_OPTIONS.iter().position(|&r| r == rate).unwrap_or(0);
    Some((rate, idx))
}

/// Flat channel-list index → (stream_index, channel_index); `None` when
/// there is no probe or the index is out of range.
fn resolve_flat_channel(
    probe: Option<&gui_engine::ffprobe::VideoAudioProbe>,
    flat_idx: usize,
) -> Option<(usize, usize)> {
    let probe = probe?;
    let mut flat = 0usize;
    for st in &probe.streams {
        for ch in 0..st.channels {
            if flat == flat_idx {
                return Some((st.stream_index, ch));
            }
            flat += 1;
        }
    }
    None
}

/// Clipboard report text for an LTC decode result (the slint-side report
/// builder; the CLI has its own).
fn build_ltc_report(result: &gui_engine::LtcDetectionResult) -> String {
    let drop_flag = if result.drop_frame { " DF" } else { "" };
    let fps_str = if result.detected_fps > 0.0 {
        format!("{:.2}{}", result.detected_fps, drop_flag)
    } else {
        "—".to_string()
    };
    let first = result.timecodes.first();
    let last = result.timecodes.last();
    let tc_range = match (first, last) {
        (Some(f), Some(l)) => {
            format!(
                "{} → {}",
                timecode::timecode_to_string(f.timecode, result.drop_frame),
                timecode::timecode_to_string(l.timecode, result.drop_frame),
            )
        }
        _ => "—".to_string(),
    };

    let mut report = String::new();
    report.push_str("LTC Decode Report\n");
    report.push_str("=================\n");
    report.push_str(&format!("Status:          {:?}\n", result.status));
    report.push_str(&format!("FPS:             {}\n", fps_str));
    report.push_str(&format!(
        "Valid frames:   {} / {} ({:.1}%)\n",
        result.valid_frames,
        result.total_possible_frames,
        result.avg_confidence * 100.0
    ));
    report.push_str(&format!("Timecode range:  {}\n", tc_range));
    report.push_str(&format!("Sample rate:     {} Hz\n", result.sample_rate));
    report.push_str(&format!("Audio duration:  {:.2}s\n", result.total_audio_duration_secs));
    report.push_str(&format!("Processing time: {:.1}ms\n", result.processing_time_ms));

    if let Some(ref q) = result.quality {
        report.push_str(&format!(
            "\nQuality Score:  {:.2} / 1.00 ({})\n",
            q.score, q.grade
        ));
        report.push_str(&format!(
            "  Usable:        {:.1}% ({} block(s), {} backward jump(s))\n",
            q.usable_coverage * 100.0, q.block_count, q.backward_jump_count
        ));
        report.push_str(&format!(
            "  Missing:       {} frames, {} gap(s), {} glitch(es), {} edit point(s)\n",
            q.missing_frames, q.gap_count, q.glitch_count, q.edit_count
        ));
        if q.max_drift_secs > 0.01 {
            report.push_str(&format!(
                "  Max drift:    {:.3}s ({:.2} frames)\n",
                q.max_drift_secs, q.worst_block_drift_frames
            ));
        }
    }

    if !result.timecodes.is_empty() {
        report.push_str(&format!(
            "\nTimecodes ({} total):\n",
            result.timecodes.len()
        ));
        for ft in &result.timecodes {
            let tc = timecode::timecode_to_string(ft.timecode, result.drop_frame);
            report.push_str(&format!(
                "  [{:4}] {}  ({:.3}s)\n",
                ft.frame_index, tc, ft.timecode_secs
            ));
        }
    }

    report
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = gui_engine::cli::parse_args();

    match gui_engine::cli::process_cli(cli) {
        gui_engine::cli::CliOutcome::Done => Ok(()),
        gui_engine::cli::CliOutcome::RunGui { cmd_tx, state, event_rx } => {
            _run_gui(cmd_tx, state, event_rx)
        }
    }
}

// ── Shared callback runtime handles ─────────────────────────────────────
// Built once in `_run_gui`; every `register_*` function clones the Arc
// handles it needs out of this struct.

struct GuiRuntime {
    sink: Arc<CommandSink>,
    shadows: Arc<Mutex<Shadows>>,
    engine_state: Arc<ArcSwap<AppStateSnapshot>>,
    toasts: Arc<Mutex<Vec<ToastItem>>>,
    next_toast_id: Arc<Mutex<i32>>,
    log_buffer: Arc<Mutex<gui_engine::log_buffer::LogBuffer>>,
    last_debug_log_count: Arc<Mutex<usize>>,
}

/// Config restore (last used converter folder) + static option models.
fn populate_static_models(ui: &AppWindow, rt: &GuiRuntime) {
    // ── Restore last used converter folders from config ─────────────────────
    {
        let sink = &rt.sink;
        let cfg = config::load();
        if let Some(ref folder) = cfg.last_input_folder {
            let path = std::path::Path::new(folder);
            if path.exists() {
                ui.set_conv_selected_folder(SharedString::from(folder.as_str()));
                sink.send(GuiCommand::Converter(ConverterCommand::SelectFolder(
                    path.to_path_buf(),
                )));
                // Auto-select first recording — deferred by the engine until
                // the async folder scan completes, then applied race-free.
                sink.send(GuiCommand::Converter(
                    ConverterCommand::SelectRecording(0),
                ));
            }
        }
    }

    // ── Populate FPS options model ──────────────────────────────────────────
    {
        let fps_model = ModelRc::new(VecModel::<FrameRateOption>::from(
            FPS_OPTIONS
                .iter()
                .map(|opt| FrameRateOption {
                    name: SharedString::from(opt.name),
                    fps: opt.fps as f32,
                    drop_frame: opt.drop_frame,
                    description: SharedString::from(opt.description),
                })
                .collect::<Vec<_>>(),
        ));
        ui.set_fps_options(fps_model);
    }

    // ── Populate decode FPS options model ────────────────────────────────────
    {
        let decode_fps_names: Vec<SharedString> = FPS_OPTIONS
            .iter()
            .map(|opt| SharedString::from(opt.name))
            .collect();
        ui.set_decode_fps_options(ModelRc::new(VecModel::<SharedString>::from(decode_fps_names)));
    }

    // ── Populate sample rate options ────────────────────────────────────────
    {
        let rate_model = ModelRc::new(VecModel::<SharedString>::from(
            SAMPLE_RATE_OPTIONS
                .iter()
                .map(|r| SharedString::from(format!("{} Hz", r)))
                .collect::<Vec<_>>(),
        ));
        ui.set_sample_rate_options(rate_model);
    }

    // ── Populate converter format options ────────────────────────────────────
    {
        let container_options: Vec<SharedString> = gui_engine::converter::supported_containers()
            .iter().map(|(key, _)| SharedString::from(*key)).collect();
        ui.set_conv_container_options(ModelRc::new(VecModel::<SharedString>::from(container_options)));
        let video_options: Vec<SharedString> = gui_engine::video_codecs::supported_video_codecs()
            .iter().map(|(key, desc)| SharedString::from(format!("{} — {}", key, desc))).collect();
        ui.set_conv_video_encoder_options(ModelRc::new(VecModel::<SharedString>::from(video_options)));
        let audio_options: Vec<SharedString> = gui_engine::converter::supported_audio_encoders()
            .iter().map(|(key, _)| SharedString::from(*key)).collect();
        ui.set_conv_audio_encoder_options(ModelRc::new(VecModel::<SharedString>::from(audio_options)));
    }
}

fn register_refresh_devices(ui: &AppWindow, rt: &GuiRuntime) {
    let state = rt.engine_state.clone();
    let ui_weak = ui.as_weak();
    let toasts_clone = rt.toasts.clone();
    let next_id = rt.next_toast_id.clone();
    let cmd = rt.sink.clone();

    let refresh = move || {
        let _ = cmd.send(GuiCommand::RefreshDevices);
        std::thread::sleep(Duration::from_millis(100));
        let s = state.load();
        let ui = match ui_weak.upgrade() {
            Some(u) => u,
            None => return,
        };
        let device_names: Vec<SharedString> = s.devices
            .iter()
            .map(|d| {
                if d.is_default {
                    SharedString::from(format!("{} (Default)", d.name))
                } else {
                    SharedString::from(d.name.clone())
                }
            })
            .collect();
        ui.set_device_names(ModelRc::new(VecModel::<SharedString>::from(device_names)));
        ui.set_device_count(s.devices.len() as i32);
        let dev_idx = s.selected_device.as_ref()
            .and_then(|id| s.devices.iter().position(|d| &d.id == id))
            .map(|i| i as i32)
            .unwrap_or(-1);
        ui.set_device_index(dev_idx);
        if !s.devices.is_empty() {
            push_toast(&toasts_clone, &next_id, &ui, &format!("{} devices found", s.devices.len()), "info");
        }
    };

    refresh();
    ui.on_refresh_devices(refresh);
}

fn register_theme_and_debug(ui: &AppWindow, rt: &GuiRuntime) {
    // ── Theme toggle ────────────────────────────────────────────────────────
    {
        let cmd = rt.sink.clone();
        ui.on_toggle_theme(move || {
            let _ = cmd.send(GuiCommand::ToggleTheme);
        });
    }

    // ── Debug log toggle ────────────────────────────────────────────────────
    {
        let log_buffer_clone = rt.log_buffer.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_debug_log(move || {
            if let Some(u) = ui_weak.upgrade() {
                let new_val = !u.get_show_debug_log();
                u.set_show_debug_log(new_val);
                if new_val {
                    let entries: Vec<SharedString> = log_buffer_clone
                        .lock().unwrap()
                        .entries.iter()
                        .map(|s| SharedString::from(s.as_str()))
                        .collect();
                    u.set_debug_log_entries(ModelRc::new(VecModel::<SharedString>::from(entries)));
                }
            }
        });
    }

    // ── Debug log clear ────────────────────────────────────────────────────
    {
        let log_buffer_clone = rt.log_buffer.clone();
        let last_count_clone = rt.last_debug_log_count.clone();
        let ui_weak = ui.as_weak();
        ui.on_clear_debug_log(move || {
            log_buffer_clone.lock().unwrap().entries.clear();
            *last_count_clone.lock().unwrap() = 0;
            if let Some(u) = ui_weak.upgrade() {
                u.set_debug_log_entries(ModelRc::new(VecModel::<SharedString>::default()));
            }
        });
    }
}

fn register_transport(ui: &AppWindow, rt: &GuiRuntime) {
    {
        let cmd = rt.sink.clone();
        ui.on_start_ltc(move || { let _ = cmd.send(GuiCommand::StartLtc); });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_stop_ltc(move || { let _ = cmd.send(GuiCommand::StopLtc); });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_reset_tc(move || { let _ = cmd.send(GuiCommand::Reset); });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_toggle_lock(move || { let _ = cmd.send(GuiCommand::ToggleLock); });
    }

    // ── Clapper ─────────────────────────────────────────────────────────────
    {
        let cmd = rt.sink.clone();
        ui.on_clap_beep(move || { let _ = cmd.send(GuiCommand::Clap); });
    }
}

fn register_clapper_metadata(ui: &AppWindow, rt: &GuiRuntime) {
    // ── Scene / Take / Roll ────────────────────────────────────────────────
    {
        let cmd = rt.sink.clone();
        ui.on_scene_up(move || { let _ = cmd.send(GuiCommand::SceneUp); });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_scene_down(move || { let _ = cmd.send(GuiCommand::SceneDown); });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_take_up(move || { let _ = cmd.send(GuiCommand::TakeUp); });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_take_down(move || { let _ = cmd.send(GuiCommand::TakeDown); });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        ui.on_roll_changed(move |val| {
            let seq = sink.send(GuiCommand::SetRoll(val.to_string()));
            shadows.lock().unwrap().roll.send_and_mark(val.to_string(), seq, Instant::now());
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_auto_increment_toggled(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.auto_increment.value();
            let seq = sink.send(GuiCommand::SetAutoIncrement(val));
            sh.auto_increment.send_and_mark(val, seq, Instant::now());
            u.set_auto_increment(val);
        });
    }

    // ── Logs ───────────────────────────────────────────────────────────────
    {
        let cmd = rt.sink.clone();
        ui.on_clear_logs(move || {
            let _ = cmd.send(GuiCommand::ClearLogs);
        });
    }

    {
        let state = rt.engine_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_copy_logs(move || {
            let s = state.load();
            let text = s.clapper.logs
                .iter()
                .map(|l| format!("[{}] LTC: {} | MS: {} | {}", l.timestamp, l.timecode, l.milliseconds, l.note))
                .collect::<Vec<_>>()
                .join("\n");
            if let Ok(mut ctx) = arboard::Clipboard::new() {
                let _ = ctx.set_text(text);
            }
            if let Some(u) = ui_weak.upgrade() {
                u.set_copy_confirmed(true);
                let ui_weak2 = u.as_weak();
                let reset_timer = slint::Timer::default();
                reset_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(2000),
                    move || {
                        if let Some(fui) = ui_weak2.upgrade() {
                            fui.set_copy_confirmed(false);
                        }
                    },
                );
                Box::leak(Box::new(reset_timer));
            }
        });
    }
}

fn register_toasts_and_tabs(ui: &AppWindow, rt: &GuiRuntime) {
    // ── Toast dismiss ──────────────────────────────────────────────────────
    {
        let toasts_clone = rt.toasts.clone();
        let ui_weak = ui.as_weak();
        ui.on_dismiss_toast(move |id| {
            let mut tv = toasts_clone.lock().unwrap();
            if let Some(pos) = tv.iter().position(|t| t.id == id) {
                tv.remove(pos);
            }
            if let Some(u) = ui_weak.upgrade() {
                update_toast_model(&u, &tv);
            }
        });
    }

    // ── Tab click ──────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_tab_clicked(move |index| {
            if let Some(u) = ui_weak.upgrade() {
                u.set_active_tab(index);
            }
        });
    }
}

fn register_timecode_steppers(ui: &AppWindow, rt: &GuiRuntime) {
    {
        let cmd = rt.sink.clone();
        let cmd2 = rt.sink.clone();
        ui.on_hour_up(move || { let _ = cmd.send(GuiCommand::HourUp); });
        ui.on_hour_down(move || { let _ = cmd2.send(GuiCommand::HourDown); });
    }
    {
        let cmd = rt.sink.clone();
        let cmd2 = rt.sink.clone();
        ui.on_minute_up(move || { let _ = cmd.send(GuiCommand::MinuteUp); });
        ui.on_minute_down(move || { let _ = cmd2.send(GuiCommand::MinuteDown); });
    }
    {
        let cmd = rt.sink.clone();
        let cmd2 = rt.sink.clone();
        ui.on_second_up(move || { let _ = cmd.send(GuiCommand::SecondUp); });
        ui.on_second_down(move || { let _ = cmd2.send(GuiCommand::SecondDown); });
    }
    {
        let cmd = rt.sink.clone();
        let cmd2 = rt.sink.clone();
        ui.on_frame_up(move || { let _ = cmd.send(GuiCommand::FrameUp); });
        ui.on_frame_down(move || { let _ = cmd2.send(GuiCommand::FrameDown); });
    }
}

fn register_settings_selections(ui: &AppWindow, rt: &GuiRuntime) {
    // ── FPS selection ──────────────────────────────────────────────────────
    // settings.slint sends the selected item's fps × 100; map it back to the
    // FPS_OPTIONS index before sending.
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_fps_selected(move |fps_x100| {
            let fps_val = fps_x100 as f64 / 100.0;
            let Some(idx) = fps_index_for(fps_val) else {
                return;
            };
            let seq = sink.send(GuiCommand::SetFpsIndex(idx));
            let mut sh = shadows.lock().unwrap();
            sh.fps_index.send_and_mark(idx, seq, Instant::now());
            if let Some(u) = ui_weak.upgrade() {
                u.set_fps_index(idx as i32);
            }
        });
    }

    // ── Sample rate selection ──────────────────────────────────────────────
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_sample_rate_tapped(move |_val| {
            if let Some((rate, idx)) = sample_rate_index_for(_val.as_str()) {
                let seq = sink.send(GuiCommand::SetSampleRate(rate));
                let mut sh = shadows.lock().unwrap();
                sh.sample_rate.send_and_mark(idx, seq, Instant::now());
                if let Some(u) = ui_weak.upgrade() {
                    u.set_sample_rate_index(idx as i32);
                }
            }
        });
    }

    // ── Device selection ──────────────────────────────────────────────────
    {
        let cmd = rt.sink.clone();
        let state_for_device = rt.engine_state.clone();
        ui.on_device_selected(move |index| {
            let s = state_for_device.load();
            if let Some(dev) = s.devices.get(index as usize) {
                let _ = cmd.send(GuiCommand::SetDevice(dev.id.clone()));
            }
        });
    }

    // ── Routing ───────────────────────────────────────────────────────────
    {
        let cmd = rt.sink.clone();
        ui.on_ltc_channel_selected(move |index| {
            let ch = CHANNEL_CHOICES[index as usize];
            let _ = cmd.send(GuiCommand::SetLtcChannel(ch));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_beep_channel_selected(move |index| {
            let ch = CHANNEL_CHOICES[index as usize];
            let _ = cmd.send(GuiCommand::SetBeepChannel(ch));
        });
    }

    // ── Volume / pitch / duration sliders ──────────────────────────────────
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_ltc_volume_changed(move |val| {
            let seq = sink.send(GuiCommand::SetLtcVolume(val));
            shadows.lock().unwrap().ltc_volume.send_and_mark(val, seq, Instant::now());
            if let Some(u) = ui_weak.upgrade() { u.set_ltc_volume(val); }
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_beep_volume_changed(move |val| {
            let seq = sink.send(GuiCommand::SetBeepVolume(val));
            shadows.lock().unwrap().beep_volume.send_and_mark(val, seq, Instant::now());
            if let Some(u) = ui_weak.upgrade() { u.set_beep_volume(val); }
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_beep_frequency_changed(move |val| {
            let seq = sink.send(GuiCommand::SetBeepFrequency(val));
            shadows.lock().unwrap().beep_frequency.send_and_mark(val, seq, Instant::now());
            if let Some(u) = ui_weak.upgrade() { u.set_beep_frequency(val); }
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_beep_duration_changed(move |val| {
            let seq = sink.send(GuiCommand::SetBeepDuration(val));
            shadows.lock().unwrap().beep_duration.send_and_mark(val, seq, Instant::now());
            if let Some(u) = ui_weak.upgrade() { u.set_beep_duration(val); }
        });
    }
}

fn register_converter_actions(ui: &AppWindow, rt: &GuiRuntime) {
    {
        let cmd = rt.sink.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_select_folder(move || {
            let dialog = rfd::FileDialog::new();
            if let Some(path) = dialog.pick_folder() {
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SelectFolder(
                    path.clone(),
                )));
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SelectRecording(0)));
                config::save_input_folder(&path);
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_selected_folder(SharedString::from(path.to_string_lossy().as_ref()));
                }
            }
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_conv_select_group(move |group_idx| {
            if group_idx >= 0 {
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SelectRecording(
                    group_idx as usize,
                )));
            }
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_conv_map_cell_clicked(move |row, col| {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SwapChannelMapCells(
                row as usize, col as usize,
            )));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_conv_start(move || {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::StartConversion));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_conv_cancel(move || {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::CancelConversion));
        });
    }
    {
        let state = rt.engine_state.clone();
        ui.on_conv_copy_log(move || {
            let s = state.load();
            let text = s.job(JobKind::Conversion).log().to_string();
            if let Ok(mut ctx) = arboard::Clipboard::new() {
                let _ = ctx.set_text(text);
            }
        });
    }
    {
        let cmd = rt.sink.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_select_output_folder(move || {
            let dialog = rfd::FileDialog::new();
            if let Some(path) = dialog.pick_folder() {
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetOutputFolder(
                    path.clone(),
                )));
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_output_folder(SharedString::from(path.to_string_lossy().as_ref()));
                }
            }
        });
    }
    // ── LTC file index changed callback ────────────────────────────────────
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_ltc_file_selected(move |idx| {
            let idx = idx as usize;
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetLtcFileIndex(idx)));
            shadows.lock().unwrap().conv.ltc_file_idx.send_and_mark(idx, seq, Instant::now());
            if let Some(u) = ui_weak.upgrade() {
                u.set_ltc_file_idx(idx as i32);
            }
        });
    }
    // ── Copy LTC report callback ──────────────────────────────────────────
    {
        let state = rt.engine_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_copy_ltc_report(move || {
            let s = state.load();
            if let Some(ref result) = s.decode.result {
                let report = build_ltc_report(result);
                if let Ok(mut ctx) = arboard::Clipboard::new() {
                    let _ = ctx.set_text(report);
                }
                if let Some(u) = ui_weak.upgrade() {
                    u.set_copy_confirmed(true);
                    let ui_weak2 = u.as_weak();
                    let reset_timer = slint::Timer::default();
                    reset_timer.start(
                        slint::TimerMode::SingleShot,
                        Duration::from_millis(2000),
                        move || {
                            if let Some(fui) = ui_weak2.upgrade() {
                                fui.set_copy_confirmed(false);
                            }
                        },
                    );
                    Box::leak(Box::new(reset_timer));
                }
            }
        });
    }
}

fn register_converter_toggles(ui: &AppWindow, rt: &GuiRuntime) {
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_embed_camera_meta(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.embed_camera_metadata.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetEmbedCameraMetadata(val)));
            sh.conv.embed_camera_metadata.send_and_mark(val, seq, Instant::now());
            u.set_conv_embed_camera_meta(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_set_start_ltc(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.set_start_from_ltc.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetStartFromLtc(val)));
            sh.conv.set_start_from_ltc.send_and_mark(val, seq, Instant::now());
            u.set_set_start_from_ltc(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_split_tracks(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.split_tracks.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetSplitTracks(val)));
            sh.conv.split_tracks.send_and_mark(val, seq, Instant::now());
            u.set_conv_split_tracks(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_drop_ltc_track(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.drop_ltc_track.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetDropLtcTrack(val)));
            sh.conv.drop_ltc_track.send_and_mark(val, seq, Instant::now());
            u.set_conv_drop_ltc_track(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_concat_audio(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.concat_audio.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetConcatAudio(val)));
            sh.conv.concat_audio.send_and_mark(val, seq, Instant::now());
            u.set_conv_concat_audio(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_synthetic_video(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.generate_synthetic_video.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetGenerateSyntheticVideo(val)));
            sh.conv.generate_synthetic_video.send_and_mark(val, seq, Instant::now());
            u.set_conv_generate_synthetic_video(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_copy_video(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.copy_video.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetCopyVideo(val)));
            sh.conv.copy_video.send_and_mark(val, seq, Instant::now());
            u.set_conv_copy_video(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_metadata_only(move || {
            let Some(u) = ui_weak.upgrade() else { return };
            let mut sh = shadows.lock().unwrap();
            let val = !*sh.conv.metadata_only.value();
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetMetadataOnly(val)));
            sh.conv.metadata_only.send_and_mark(val, seq, Instant::now());
            u.set_conv_metadata_only(val);
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_reset(move || {
            let mut sh = shadows.lock().unwrap();
            let mark_bool = |es: &mut gui_engine::edit_state::EditState<bool>, cmd| {
                let seq = sink.send(cmd);
                es.send_and_mark(false, seq, Instant::now());
            };
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_metadata_only(false);
                u.set_conv_split_tracks(false);
                u.set_conv_drop_ltc_track(false);
                u.set_conv_concat_audio(false);
                u.set_conv_generate_synthetic_video(false);
                u.set_conv_copy_video(false);
            }
            mark_bool(&mut sh.conv.metadata_only,
                GuiCommand::Converter(ConverterCommand::SetMetadataOnly(false)));
            mark_bool(&mut sh.conv.split_tracks,
                GuiCommand::Converter(ConverterCommand::SetSplitTracks(false)));
            mark_bool(&mut sh.conv.drop_ltc_track,
                GuiCommand::Converter(ConverterCommand::SetDropLtcTrack(false)));
            mark_bool(&mut sh.conv.concat_audio,
                GuiCommand::Converter(ConverterCommand::SetConcatAudio(false)));
            mark_bool(&mut sh.conv.generate_synthetic_video,
                GuiCommand::Converter(ConverterCommand::SetGenerateSyntheticVideo(false)));
            mark_bool(&mut sh.conv.copy_video,
                GuiCommand::Converter(ConverterCommand::SetCopyVideo(false)));
            {
                let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetFilenamePrefix(String::new())));
                sh.conv.filename_prefix.send_and_mark(String::new(), seq, Instant::now());
            }
            {
                let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetAudioSuffixTemplate(
                    gui_engine::naming::DEFAULT_AUDIO_SUFFIX.to_string(),
                )));
                sh.conv.audio_suffix_template.send_and_mark(
                    gui_engine::naming::DEFAULT_AUDIO_SUFFIX.to_string(), seq, Instant::now(),
                );
            }
            {
                let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetVideoSuffixTemplate(
                    gui_engine::naming::DEFAULT_VIDEO_SUFFIX.to_string(),
                )));
                sh.conv.video_suffix_template.send_and_mark(
                    gui_engine::naming::DEFAULT_VIDEO_SUFFIX.to_string(), seq, Instant::now(),
                );
            }
        });
    }
}

fn register_converter_dropdowns(ui: &AppWindow, rt: &GuiRuntime) {
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_container_selected(move |idx| {
            let options = gui_engine::converter::supported_containers();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetContainer(key.clone())));
                shadows.lock().unwrap().conv.container.send_and_mark(key.clone(), seq, Instant::now());
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_container(SharedString::from(key));
                }
            }
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_video_selected(move |idx| {
            let options = gui_engine::video_codecs::supported_video_codecs();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetVideoCodec(key.clone())));
                shadows.lock().unwrap().conv.video_encoder.send_and_mark(key.clone(), seq, Instant::now());
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_video_encoder(SharedString::from(key));
                }
            }
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_audio_selected(move |idx| {
            let options = gui_engine::converter::supported_audio_encoders();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetAudioEncoder(key.clone())));
                shadows.lock().unwrap().conv.audio_encoder.send_and_mark(key.clone(), seq, Instant::now());
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_audio_encoder(SharedString::from(key));
                }
            }
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        ui.on_conv_filename_prefix_changed(move |val| {
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetFilenamePrefix(
                val.to_string(),
            )));
            shadows.lock().unwrap().conv.filename_prefix.send_and_mark(val.to_string(), seq, Instant::now());
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        ui.on_conv_audio_suffix_changed(move |val| {
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetAudioSuffixTemplate(
                val.to_string(),
            )));
            shadows.lock().unwrap().conv.audio_suffix_template.send_and_mark(val.to_string(), seq, Instant::now());
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        ui.on_conv_video_suffix_changed(move |val| {
            let seq = sink.send(GuiCommand::Converter(ConverterCommand::SetVideoSuffixTemplate(
                val.to_string(),
            )));
            shadows.lock().unwrap().conv.video_suffix_template.send_and_mark(val.to_string(), seq, Instant::now());
        });
    }
}

fn register_decode(ui: &AppWindow, rt: &GuiRuntime) {
    // ── LTC detection callback ─────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let cmd = rt.sink.clone();
        let detect_engine_state = rt.engine_state.clone();
        ui.on_ltc_detect(move || {
            let s = detect_engine_state.load();
            let sel_idx = s.converter.selected_group_idx;
            if sel_idx.is_none() { return; }
            let idx = sel_idx.unwrap();
            let groups = &s.converter.groups;
            if idx >= groups.len() { return; }
            let group = &groups[idx];
            let folder_str = s.converter.groups_folder.clone()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let files = &group.files;
            if files.is_empty() { return; }
            // Use the UI-selected LTC file index (for audio groups)
            let ltc_idx = ui_weak.upgrade()
                .map(|u| u.get_ltc_file_idx() as usize)
                .unwrap_or(0)
                .min(files.len().saturating_sub(1));
            let file_name = files[ltc_idx].file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let full_path = std::path::PathBuf::from(&folder_str).join(&file_name);
            if let Some(u) = ui_weak.upgrade() {
                u.set_ltc_status(SharedString::from("detecting"));
                u.set_ltc_result_text(SharedString::from(""));
                u.set_ltc_error(SharedString::from(""));
            }
            let is_video = gui_engine::ffprobe::path_is_video(&full_path);
            if is_video {
                let flat_idx = ui_weak.upgrade()
                    .map(|u| u.get_ltc_selected_channel() as usize)
                    .unwrap_or(0);
                let (stream_idx, channel_idx) = resolve_flat_channel(s.decode.probe.as_ref(), flat_idx)
                    .unwrap_or((0, 0));
                let paths: Vec<String> = files.iter()
                    .map(|filename| std::path::PathBuf::from(&folder_str).join(filename).to_string_lossy().to_string())
                    .collect();
                let _ = cmd.send(GuiCommand::DecodeLtcVideoGroup {
                    paths,
                    stream_index: stream_idx,
                    channel_index: channel_idx,
                });
            } else {
                let _ = cmd.send(GuiCommand::ParseLtcWavFile(
                    full_path.to_string_lossy().to_string(),
                ));
            }
        });
    }

    // ── Channel selection callback (for video probe) ──────────────────────
    {
        let cmd = rt.sink.clone();
        let chan_engine_state = rt.engine_state.clone();
        ui.on_channel_selected(move |flat_idx| {
            let s = chan_engine_state.load();
            if let Some((stream_idx, channel_idx)) =
                resolve_flat_channel(s.decode.probe.as_ref(), flat_idx as usize)
            {
                let _ = cmd.send(GuiCommand::SetLtcDecodeStream(stream_idx));
                let _ = cmd.send(GuiCommand::SetLtcDecodeChannel(channel_idx));
            }
        });
    }

    // ── Decode FPS selection callback ───────────────────────────────────────
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        let ui_weak = ui.as_weak();
        ui.on_decode_fps_selected(move |index| {
            let idx = index as usize;
            let seq = sink.send(GuiCommand::SetDecodeFpsIndex(idx));
            shadows.lock().unwrap().decode_fps_index.send_and_mark(idx, seq, Instant::now());
            if let Some(u) = ui_weak.upgrade() {
                u.set_decode_fps_index(idx as i32);
            }
        });
    }

    // ── Cancel LTC decode callback ───────────────────────────────────────
    {
        let cmd = rt.sink.clone();
        ui.on_cancel_decode(move || {
            let _ = cmd.send(GuiCommand::CancelDecode);
        });
    }
}

fn register_offload(ui: &AppWindow, rt: &GuiRuntime) {
    {
        let cmd = rt.sink.clone();
        let ui_weak = ui.as_weak();
        ui.on_off_select_parent_folder(move || {
            let mut dialog = rfd::FileDialog::new();
            if let Some(u) = ui_weak.upgrade() {
                let current = u.get_off_parent_folder();
                if !current.is_empty() {
                    dialog = dialog.set_directory(current.as_str());
                }
            }
            if let Some(path) = dialog.pick_folder() {
                let _ = cmd.send(GuiCommand::Offload(OffloadCommand::SetParentFolder(path)));
                // Poll will sync the parent folder from engine state
            }
        });
    }
    {
        let sink = rt.sink.clone();
        let shadows = rt.shadows.clone();
        ui.on_off_parent_name_changed(move |val| {
            let seq = sink.send(GuiCommand::Offload(OffloadCommand::SetParentName(
                val.to_string(),
            )));
            shadows.lock().unwrap().parent_name.send_and_mark(val.to_string(), seq, Instant::now());
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_off_card_name_changed(move |idx, val| {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::SetDeviceName(
                idx as usize, val.to_string(),
            )));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_off_rescan(move || {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::ScanCards));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_off_start(move || {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::StartOffload));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_off_cancel(move || {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::CancelOffload));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_off_file_toggled(move |card_idx, file_idx, sel| {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::SetFileSelected(
                card_idx as usize, file_idx as usize, sel,
            )));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_off_select_all_files(move |card_idx, sel| {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(
                card_idx as usize, sel,
            )));
        });
    }
    {
        let cmd = rt.sink.clone();
        ui.on_off_select_latest_day(move |card_idx| {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::SelectLatestDay(
                card_idx as usize,
            )));
        });
    }
}

fn _run_gui(
    cmd_tx: mpsc::Sender<GuiCommand>,
    engine_state: Arc<ArcSwap<AppStateSnapshot>>,
    event_rx: std::sync::mpsc::Receiver<AudioEvent>,
) -> Result<(), Box<dyn std::error::Error>> {
    let log_buffer = gui_engine::log_buffer::init_logger(
        gui_engine::log_buffer::DEFAULT_LOG_FILTER,
    )?;

    info!("LTC Slint GUI v{} starting...", APP_VERSION);
    let os_name = std::env::consts::OS.to_uppercase();
    info!("Operating system: {}", os_name);

    let ui = AppWindow::new()?;

    set_theme_palette(&ui);

    // ── Toast state ─────────────────────────────────────────────────────────
    let toasts: Arc<Mutex<Vec<ToastItem>>> = Arc::new(Mutex::new(Vec::new()));
    let next_toast_id: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
    let last_debug_log_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let pulse_phase: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));

    // ── Command sink + widget shadows ────────────────────────────────────────
    // Every command goes through the sink so the sender-assigned sequence
    // stays 1:1 with the engine's `applied_command_seq` ack counter; the
    // shadows use it to confirm edits and gate poll pushes (see shadows.rs).
    let sink = Arc::new(CommandSink::new(cmd_tx));
    let shadows = Arc::new(Mutex::new(Shadows::new(&engine_state.load())));

    let rt = GuiRuntime {
        sink,
        shadows,
        engine_state: engine_state.clone(),
        toasts: toasts.clone(),
        next_toast_id: next_toast_id.clone(),
        log_buffer: log_buffer.clone(),
        last_debug_log_count: last_debug_log_count.clone(),
    };

    populate_static_models(&ui, &rt);

    // ── Set OS name and version ─────────────────────────────────────────────
    ui.set_os_name(SharedString::from(os_name.clone()));
    ui.set_version(SharedString::from(format!("v{}", APP_VERSION)));
    ui.set_power_status(SharedString::from("AC"));

    // ── Initial state read ──────────────────────────────────────────────────
    {
        let s = engine_state.load();
        let rate = s.sample_rate;
        let rate_khz = format!("{:.1}", rate as f32 / 1000.0);
        ui.set_sample_rate_khz(SharedString::from(rate_khz));
        let buffer_smp = (rate as f64 / s.fps()).round() as i32;
        ui.set_buffer_size(buffer_smp);
        let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame());
        set_tc_segments(&ui, &tc_str);
        ui.set_ms_text(SharedString::from(timecode::timecode_to_ms_string(s.current_timecode, s.fps())));
        ui.set_fps_name(SharedString::from(FPS_OPTIONS[s.fps_index].name));
        ui.set_decode_fps_index(s.decode.fps_index as i32);
    }

    register_refresh_devices(&ui, &rt);
    register_theme_and_debug(&ui, &rt);
    register_transport(&ui, &rt);
    register_clapper_metadata(&ui, &rt);
    register_toasts_and_tabs(&ui, &rt);
    register_timecode_steppers(&ui, &rt);
    register_settings_selections(&ui, &rt);
    register_converter_actions(&ui, &rt);
    register_converter_toggles(&ui, &rt);
    register_converter_dropdowns(&ui, &rt);
    register_decode(&ui, &rt);
    register_offload(&ui, &rt);

    // ── Poll timer — state sync ────────────────────────────────────────────
    let initial_clap_seq = engine_state.load().clapper.clap_seq;
    setup_poll_timer(
        &ui,
        PollContext {
            engine_state,
            event_rx,
            toasts,
            next_toast_id,
            log_buffer,
            last_debug_log_count,
            pulse_phase,
            shadows: rt.shadows.clone(),
            last_log_count: Arc::new(Mutex::new(0)),
            last_device_key: Arc::new(Mutex::new((0, String::new()))),
            // Seeded from the snapshot so a mid-session GUI start does not
            // fire for an old clap.
            last_clap_seq: Arc::new(Mutex::new(initial_clap_seq)),
        },
    );

    info!("LTC Slint GUI initialized, showing window");
    ui.run()?;

    info!("LTC Slint GUI shutting down");
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps_index_matches_exact_and_rejects_unknown() {
        assert_eq!(fps_index_for(24.0), Some(0));
        assert_eq!(fps_index_for(25.0), Some(1));
        assert_eq!(fps_index_for(29.97), Some(2));
        assert_eq!(fps_index_for(30.0), Some(4));
        assert_eq!(fps_index_for(23.976), None);
    }

    #[test]
    fn sample_rate_index_parses_label_and_defaults_known_rates() {
        assert_eq!(sample_rate_index_for("44100 Hz"), Some((44100, 0)));
        assert_eq!(sample_rate_index_for("48000 Hz"), Some((48000, 1)));
        // rates not in the option list resolve to index 0 (existing behavior)
        assert_eq!(sample_rate_index_for("96000 Hz"), Some((96000, 0)));
        assert_eq!(sample_rate_index_for("garbage"), None);
        assert_eq!(sample_rate_index_for(""), None);
    }

    fn stream(i: usize, channels: usize) -> gui_engine::ffprobe::AudioStreamInfo {
        gui_engine::ffprobe::AudioStreamInfo {
            stream_index: i,
            channels,
            codec_name: "pcm".into(),
            sample_rate: 48_000,
        }
    }

    #[test]
    fn resolve_flat_channel_walks_streams_then_channels() {
        let probe = gui_engine::ffprobe::VideoAudioProbe {
            streams: vec![stream(0, 2), stream(1, 2)],
            total_audio_channels: 4,
            is_video_file: true,
        };
        assert_eq!(resolve_flat_channel(Some(&probe), 0), Some((0, 0)));
        assert_eq!(resolve_flat_channel(Some(&probe), 1), Some((0, 1)));
        assert_eq!(resolve_flat_channel(Some(&probe), 2), Some((1, 0)));
        assert_eq!(resolve_flat_channel(Some(&probe), 3), Some((1, 1)));
    }

    #[test]
    fn resolve_flat_channel_none_cases() {
        let probe = gui_engine::ffprobe::VideoAudioProbe {
            streams: vec![stream(0, 2)],
            total_audio_channels: 2,
            is_video_file: true,
        };
        assert_eq!(resolve_flat_channel(None, 0), None);
        assert_eq!(resolve_flat_channel(Some(&probe), 2), None);
    }

    fn decode_result() -> gui_engine::LtcDetectionResult {
        let mut r = gui_engine::LtcDetectionResult::error("unused");
        r.status = gui_engine::LtcDecodeStatus::Success;
        r.detected_fps = 25.0;
        r.total_possible_frames = 100;
        r.valid_frames = 90;
        r.avg_confidence = 0.9;
        r.sample_rate = 48_000;
        r.total_audio_duration_secs = 4.0;
        r.processing_time_ms = 10.0;
        r.timecodes.push(gui_engine::FrameTimecode {
            frame_index: 0,
            timecode: gui_engine::Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 },
            timecode_secs: 0.0,
        });
        r
    }

    // The report body is formatter output — the text is the contract
    // (clipboard payload pasted into editor notes), so the assertions here
    // pin the exact lines deliberately. test-lint: allow(text-pin): formatter
    // output is the contract for a clipboard report builder.

    #[test]
    fn ltc_report_has_header_and_summary_lines() {
        let report = build_ltc_report(&decode_result());
        assert!(report.starts_with("LTC Decode Report\n=================\n"));
        assert!(report.contains("Status:          Success\n"));
        assert!(report.contains("FPS:             25.00\n"));
        assert!(report.contains("Valid frames:   90 / 100 (90.0%)\n"));
        assert!(report.contains("Sample rate:     48000 Hz\n"));
    }

    #[test]
    fn ltc_report_lists_timecodes_and_range() {
        let report = build_ltc_report(&decode_result());
        assert!(report.contains("Timecode range:  01:00:00:00 → 01:00:00:00\n"));
        assert!(report.contains("Timecodes (1 total):\n"));
        assert!(report.contains("  [   0] 01:00:00:00  (0.000s)\n"));
    }

    #[test]
    fn ltc_report_empty_result_has_no_timecode_block() {
        let report = build_ltc_report(&gui_engine::LtcDetectionResult::error("x"));
        assert!(!report.contains("Timecodes ("));
        assert!(report.contains("Timecode range:  —\n"));
    }
}
