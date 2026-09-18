slint::include_modules!();

use std::f64::consts::PI;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gui_engine::command::GuiCommand;
use gui_engine::state::AppStateSnapshot;
use gui_engine::timecode::{self, FPS_OPTIONS};
use gui_engine::{ArcSwap, AudioEvent, SAMPLE_RATE_OPTIONS};
use log::info;
use slint::{ModelRc, SharedString, VecModel};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const POLL_INTERVAL_MS: u64 = 40;

// ── Toast management ────────────────────────────────────────────────────

#[derive(Clone)]
struct ToastItem {
    id: i32,
    message: String,
    toast_type: String,
}

fn update_toast_model(ui: &AppWindow, toasts: &[ToastItem]) {
    let toast_data: Vec<ToastData> = toasts
        .iter()
        .map(|t| ToastData {
            id: t.id,
            message: SharedString::from(&t.message),
            toast_type: SharedString::from(&t.toast_type),
            opacity: 1.0,
        })
        .collect();
    ui.set_toasts(ModelRc::new(VecModel::<ToastData>::from(toast_data)));
}

fn push_toast(
    toasts: &Arc<Mutex<Vec<ToastItem>>>,
    next_id: &Arc<Mutex<i32>>,
    ui: &AppWindow,
    message: &str,
    toast_type: &str,
) {
    let mut toasts_vec = toasts.lock().unwrap();
    let mut nid = next_id.lock().unwrap();
    *nid += 1;
    toasts_vec.push(ToastItem {
        id: *nid,
        message: message.to_string(),
        toast_type: toast_type.to_string(),
    });
    update_toast_model(ui, &toasts_vec);

    let dismiss_id = *nid;
    let toasts_clone = toasts.clone();
    let ui_weak = ui.as_weak();
    let dismiss_timer = slint::Timer::default();
    dismiss_timer.start(
        slint::TimerMode::SingleShot,
        Duration::from_millis(3000),
        move || {
            if let Some(fui) = ui_weak.upgrade() {
                let mut tv = toasts_clone.lock().unwrap();
                if let Some(pos) = tv.iter().position(|t| t.id == dismiss_id) {
                    tv.remove(pos);
                }
                update_toast_model(&fui, &tv);
            }
        },
    );
    Box::leak(Box::new(dismiss_timer));
}

// ── Timecode segment helpers ────────────────────────────────────────────

fn split_timecode_segments(tc_str: &str) -> [String; 7] {
    let parts: Vec<&str> = tc_str.split([':', ';']).collect();
    let sep = if tc_str.contains(';') { ';' } else { ':' };
    [
        parts[0].to_string(),
        ":".to_string(),
        parts[1].to_string(),
        ":".to_string(),
        parts[2].to_string(),
        sep.to_string(),
        parts[3].to_string(),
    ]
}

fn set_tc_segments(ui: &AppWindow, tc_str: &str) {
    let [hh, sep1, mm, sep2, ss, sep3, ff] = split_timecode_segments(tc_str);
    ui.set_tc_hh(SharedString::from(hh));
    ui.set_tc_sep1(SharedString::from(sep1));
    ui.set_tc_mm(SharedString::from(mm));
    ui.set_tc_sep2(SharedString::from(sep2));
    ui.set_tc_ss(SharedString::from(ss));
    ui.set_tc_sep3(SharedString::from(sep3));
    ui.set_tc_ff(SharedString::from(ff));
}

// ── Main ────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = gui_engine::cli::parse_args();

    match gui_engine::cli::process_cli(cli) {
        gui_engine::cli::CliOutcome::Done => Ok(()),
        gui_engine::cli::CliOutcome::RunGui { cmd_tx, state } => {
            _run_gui(cmd_tx, state)
        }
    }
}

fn _run_gui(
    cmd_tx: mpsc::Sender<GuiCommand>,
    engine_state: Arc<ArcSwap<AppStateSnapshot>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let log_buffer = gui_engine::log_buffer::init_logger(
        "ltc_gui=trace,audio_core=trace,info",
    )?;

    info!("LTC Slint GUI v{} starting...", APP_VERSION);
    let os_name = std::env::consts::OS.to_uppercase();
    info!("Operating system: {}", os_name);

    let ui = AppWindow::new()?;

    // ── Toast state ─────────────────────────────────────────────────────────
    let toasts: Arc<Mutex<Vec<ToastItem>>> = Arc::new(Mutex::new(Vec::new()));
    let next_toast_id: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
    let last_debug_log_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let pulse_phase: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));

    // ── Populate FPS options model ─────────────────────────────────────────
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

    // ── Populate sample rate options ───────────────────────────────────────
    {
        let rate_model = ModelRc::new(VecModel::<SharedString>::from(
            SAMPLE_RATE_OPTIONS
                .iter()
                .map(|r| SharedString::from(format!("{} Hz", r)))
                .collect::<Vec<_>>(),
        ));
        ui.set_sample_rate_options(rate_model);
    }

    // ── Set OS name and version ────────────────────────────────────────────
    ui.set_os_name(SharedString::from(os_name.clone()));
    ui.set_version(SharedString::from(format!("v{}", APP_VERSION)));
    ui.set_power_status(SharedString::from("AC"));

    // ── Initial state read ────────────────────────────────────────────────
    {
        let s = engine_state.load();
        let rate = s.sample_rate;
        let rate_khz = format!("{:.1}", rate as f32 / 1000.0);
        ui.set_sample_rate_khz(SharedString::from(rate_khz));
        let buffer_smp = (rate as f64 / s.fps).round() as i32;
        ui.set_buffer_size(buffer_smp);
        let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame);
        set_tc_segments(&ui, &tc_str);
        ui.set_ms_text(SharedString::from(timecode::timecode_to_ms_string(s.current_timecode, s.fps)));
        ui.set_fps_name(SharedString::from(FPS_OPTIONS[s.fps_index].name));
    }

    // ── Refresh devices ───────────────────────────────────────────────────
    {
        let state = engine_state.clone();
        let ui_weak = ui.as_weak();
        let toasts_clone = toasts.clone();
        let next_id = next_toast_id.clone();
        let cmd = cmd_tx.clone();

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
            ui.set_device_index(s.selected_device as i32);
            if !s.devices.is_empty() {
                push_toast(&toasts_clone, &next_id, &ui, &format!("{} devices found", s.devices.len()), "info");
            }
        };

        refresh();
        ui.on_refresh_devices(refresh);
    }

    // ── Theme toggle ───────────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_toggle_theme(move || {
            let _ = cmd.send(GuiCommand::ToggleTheme);
        });
    }

    // ── Debug log toggle ───────────────────────────────────────────────────
    {
        let log_buffer_clone = log_buffer.clone();
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
        let log_buffer_clone = log_buffer.clone();
        let last_count_clone = last_debug_log_count.clone();
        let ui_weak = ui.as_weak();
        ui.on_clear_debug_log(move || {
            log_buffer_clone.lock().unwrap().entries.clear();
            *last_count_clone.lock().unwrap() = 0;
            if let Some(u) = ui_weak.upgrade() {
                u.set_debug_log_entries(ModelRc::new(VecModel::<SharedString>::default()));
            }
        });
    }

    // ── Transport callbacks ───────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_start_ltc(move || { let _ = cmd.send(GuiCommand::StartLtc); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_stop_ltc(move || { let _ = cmd.send(GuiCommand::StopLtc); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_reset_tc(move || { let _ = cmd.send(GuiCommand::Reset); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_toggle_lock(move || { let _ = cmd.send(GuiCommand::ToggleLock); });
    }

    // ── Clapper callbacks ─────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_clap_beep(move || { let _ = cmd.send(GuiCommand::Clap); });
    }

    // ── Scene / Take / Roll ───────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_scene_up(move || { let _ = cmd.send(GuiCommand::SceneUp); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_scene_down(move || { let _ = cmd.send(GuiCommand::SceneDown); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_take_up(move || { let _ = cmd.send(GuiCommand::TakeUp); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_take_down(move || { let _ = cmd.send(GuiCommand::TakeDown); });
    }
    {
        let cmd = cmd_tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_roll_changed(move || {
            if let Some(u) = ui_weak.upgrade() {
                let _ = cmd.send(GuiCommand::SetRoll(u.get_roll().to_string()));
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_auto_increment_toggled(move || {
            if let Some(u) = ui_weak.upgrade() {
                let _ = cmd.send(GuiCommand::SetAutoIncrement(u.get_auto_increment()));
            }
        });
    }

    // ── Logs ──────────────────────────────────────────────────────────────
    // Clear logs: sends ClearLogs to engine
    {
        let cmd = cmd_tx.clone();
        ui.on_clear_logs(move || {
            let _ = cmd.send(GuiCommand::ClearLogs);
        });
    }

    // Copy logs: GUI-only, uses arboard
    {
        let state = engine_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_copy_logs(move || {
            let s = state.load();
            let text = s.logs
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

    // ── Toast dismiss ─────────────────────────────────────────────────────
    {
        let toasts_clone = toasts.clone();
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

    // ── Tab click ─────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_tab_clicked(move |index| {
            if let Some(u) = ui_weak.upgrade() {
                u.set_active_tab(index);
            }
        });
    }

    // ── Timecode steppers ─────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        let cmd2 = cmd_tx.clone();
        ui.on_hour_up(move || { let _ = cmd.send(GuiCommand::HourUp); });
        ui.on_hour_down(move || { let _ = cmd2.send(GuiCommand::HourDown); });
    }
    {
        let cmd = cmd_tx.clone();
        let cmd2 = cmd_tx.clone();
        ui.on_minute_up(move || { let _ = cmd.send(GuiCommand::MinuteUp); });
        ui.on_minute_down(move || { let _ = cmd2.send(GuiCommand::MinuteDown); });
    }
    {
        let cmd = cmd_tx.clone();
        let cmd2 = cmd_tx.clone();
        ui.on_second_up(move || { let _ = cmd.send(GuiCommand::SecondUp); });
        ui.on_second_down(move || { let _ = cmd2.send(GuiCommand::SecondDown); });
    }
    {
        let cmd = cmd_tx.clone();
        let cmd2 = cmd_tx.clone();
        ui.on_frame_up(move || { let _ = cmd.send(GuiCommand::FrameUp); });
        ui.on_frame_down(move || { let _ = cmd2.send(GuiCommand::FrameDown); });
    }

    // ── FPS selection ────────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_fps_selected(move |index| { let _ = cmd.send(GuiCommand::SetFpsIndex(index as usize)); });
    }

    // ── Sample rate selection ────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_sample_rate_tapped(move |_val| {
            if let Some(rate_str) = _val.split_whitespace().next() {
                if let Ok(rate) = rate_str.parse::<u32>() {
                    let _ = cmd.send(GuiCommand::SetSampleRate(rate));
                }
            }
        });
    }

    // ── Device selection ─────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_device_selected(move |index| { let _ = cmd.send(GuiCommand::SetDevice(index as usize)); });
    }

    // ── Routing ──────────────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_ltc_channel_selected(move |index| {
            let ch = ["left", "right", "both"][index as usize];
            let _ = cmd.send(GuiCommand::SetLtcChannel(ch.to_string()));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_beep_channel_selected(move |index| {
            let ch = ["left", "right", "both"][index as usize];
            let _ = cmd.send(GuiCommand::SetBeepChannel(ch.to_string()));
        });
    }

    // ── Volume / pitch / duration sliders ─────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_ltc_volume_changed(move |val| { let _ = cmd.send(GuiCommand::SetLtcVolume(val)); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_beep_volume_changed(move |val| { let _ = cmd.send(GuiCommand::SetBeepVolume(val)); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_beep_frequency_changed(move |val| { let _ = cmd.send(GuiCommand::SetBeepFrequency(val)); });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_beep_duration_changed(move |val| { let _ = cmd.send(GuiCommand::SetBeepDuration(val)); });
    }

    // ── Polling timer (25 fps) ─────────────────────────────────────────────
    {
        let state = engine_state.clone();
        let ui_weak = ui.as_weak();
        let toasts_clone = toasts.clone();
        let next_toast_id_clone = next_toast_id.clone();
        let log_buffer_clone = log_buffer.clone();
        let last_count_clone = last_debug_log_count.clone();
        let pulse_phase_clone = pulse_phase.clone();

        let poll_timer = slint::Timer::default();
        poll_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(POLL_INTERVAL_MS),
            move || {
                let poll_start = Instant::now();
                static POLL_COUNTER: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let tick = POLL_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

                let ui = match ui_weak.upgrade() {
                    Some(u) => u,
                    None => return,
                };

                // 1. Load latest state from engine
                let s = state.load();

                // 2. System time
                ui.set_system_time(SharedString::from(format!("{} UTC", timecode::chrono_now_string())));

                // 3. Theme sync
                AppColors::get(&ui).set_theme_dark(s.is_dark_theme);

                // 4. Pulse phase animation (GUI-only, 4Hz sine)
                let mut pp = pulse_phase_clone.lock().unwrap();
                *pp += 4.0 * 2.0 * PI * (POLL_INTERVAL_MS as f64 / 1000.0);
                if *pp > PI * 100.0 { *pp = 0.0; }
                ui.set_pulse_phase(*pp as f32);

                // 4. Timecode display
                let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame);
                let ms_str = timecode::timecode_to_ms_string(s.current_timecode, s.fps);
                set_tc_segments(&ui, &tc_str);
                ui.set_ms_text(SharedString::from(ms_str));

                // 5. Transport state
                ui.set_is_playing(s.is_playing);
                ui.set_is_locked(s.is_locked);
                ui.set_wake_lock_active(s.wake_lock_active);
                ui.set_status_message(SharedString::from(&s.status_message));

                // 6. FPS name
                ui.set_fps_name(SharedString::from(FPS_OPTIONS[s.fps_index].name));

                // 7. Routing pills
                ui.set_ltc_route(SharedString::from(s.ltc_channel.to_uppercase()));
                ui.set_beep_route(SharedString::from(s.beep_channel.to_uppercase()));

                // 8. Sample rate metadata
                let rate_khz = format!("{:.1}", s.sample_rate as f32 / 1000.0);
                ui.set_sample_rate_khz(SharedString::from(rate_khz));
                let buffer_smp = (s.sample_rate as f64 / s.fps).round() as i32;
                ui.set_buffer_size(buffer_smp);
                ui.set_sample_format(SharedString::from(s.sample_format_name.to_uppercase()));

                // 9. Clapper metadata
                ui.set_scene(s.scene as i32);
                ui.set_take(s.take as i32);
                ui.set_auto_increment(s.auto_increment_take);

                // 10. Arm angle and flash opacity (from engine)
                ui.set_arm_angle(s.clap_arm_angle);
                ui.set_flash_opacity(s.clap_flash_alpha);

                // 11. Device selection sync
                ui.set_device_index(s.selected_device as i32);

                // 12. Volume / pitch / duration
                ui.set_ltc_volume(s.ltc_volume);
                ui.set_beep_volume(s.beep_volume);
                ui.set_beep_frequency(s.beep_frequency);
                ui.set_beep_duration(s.beep_duration);

                // 13. Start timecode steppers
                ui.set_hour(s.start_timecode.hours as i32);
                ui.set_minute(s.start_timecode.minutes as i32);
                ui.set_second(s.start_timecode.seconds as i32);
                let max_frame = s.fps.round() as i32;
                ui.set_max_frame(if max_frame > 0 { max_frame - 1 } else { 0 });
                ui.set_frame(s.start_timecode.frames as i32);

                // 14. FPS and sample rate index
                ui.set_fps_index(s.fps_index as i32);
                let sr_index = SAMPLE_RATE_OPTIONS.iter()
                    .position(|&r| r == s.sample_rate)
                    .unwrap_or(0);
                ui.set_sample_rate_index(sr_index as i32);

                // 15. Log entries from engine
                {
                    let log_entries: Vec<LogEntry> = s.logs
                        .iter()
                        .map(|l| LogEntry {
                            timestamp: SharedString::from(&l.timestamp),
                            timecode: SharedString::from(&l.timecode),
                            milliseconds: SharedString::from(&l.milliseconds),
                            note: SharedString::from(&l.note),
                        })
                        .collect();
                    ui.set_logs(ModelRc::new(VecModel::<LogEntry>::from(log_entries)));
                }

                // 16. Device names
                {
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
                }

                // 17. Process events into toasts
                for event in &s.events {
                    let (msg, typ) = match event {
                        AudioEvent::StreamError(m) => (m.clone(), "error"),
                        AudioEvent::StreamDied => ("Audio stream died".to_string(), "error"),
                        AudioEvent::StreamRecovering { attempt } => {
                            (format!("Stream recovering (attempt {})", attempt), "warning")
                        }
                        AudioEvent::StreamDead => {
                            ("Audio device unreachable".to_string(), "error")
                        }
                        AudioEvent::RecoveryNeeded { reason } => {
                            (format!("Audio recovery needed: {}", reason), "warning")
                        }
                        AudioEvent::Underrun => ("Audio underrun".to_string(), "warning"),
                        AudioEvent::FramesDropped { total } => {
                            (format!("{} frames dropped", total), "warning")
                        }
                    };
                    push_toast(&toasts_clone, &next_toast_id_clone, &ui, &msg, typ);
                }

                // 18. Debug log sync
                if ui.get_show_debug_log() {
                    let entries = log_buffer_clone.lock().unwrap();
                    let current_count = entries.entries.len();
                    let mut last_count = last_count_clone.lock().unwrap();
                    if current_count != *last_count {
                        let model: Vec<SharedString> = entries.entries.iter()
                            .map(|s| SharedString::from(s.as_str()))
                            .collect();
                        drop(entries);
                        ui.set_debug_log_entries(ModelRc::new(VecModel::<SharedString>::from(model)));
                        *last_count = current_count;
                    }
                }

                // 19. Perf diagnostics
                let poll_elapsed = poll_start.elapsed();
                if tick % 250 == 0 {
                    info!("[PERF] Poll tick #{}: {}ms", tick, poll_elapsed.as_micros() as f64 / 1000.0);
                }
            },
        );

        Box::leak(Box::new(poll_timer));
    }

    // ── Run ──────────────────────────────────────────────────────────────
    info!("LTC Slint GUI initialized, showing window");
    ui.run()?;

    info!("LTC Slint GUI shutting down");
    Ok(())
}