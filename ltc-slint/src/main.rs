slint::include_modules!();

use audio_core::{
    list_audio_devices, suggest_sample_rate, AudioCore, AudioDeviceInfo, AudioEvent, Timecode,
    SAMPLE_RATE_OPTIONS,
};
use chrono::Local;
use log::{error, info, warn};
use slint::{ModelRc, SharedString, VecModel, Weak};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUFFER_SIZE: u32 = 0;
const POLL_INTERVAL_MS: u64 = 40; // 25 fps

struct ChannelName {
    name: &'static str,
}

impl ChannelName {
    const fn new(name: &'static str) -> Self {
        Self { name }
    }
}

const CHANNEL_NAMES: [ChannelName; 3] = [
    ChannelName::new("left"),
    ChannelName::new("right"),
    ChannelName::new("both"),
];

fn channel_to_str(index: usize) -> &'static str {
    CHANNEL_NAMES.get(index).map(|c| c.name).unwrap_or("both")
}



fn timecode_to_string(tc: Timecode, drop_frame: bool) -> String {
    let sep = if drop_frame { ';' } else { ':' };
    format!(
        "{:02}:{:02}:{:02}{}{:02}",
        tc.hours, tc.minutes, tc.seconds, sep, tc.frames
    )
}

fn timecode_to_ms_string(tc: Timecode, fps: f64) -> String {
    let ms = (tc.frames as f64 / fps * 1000.0).round() as u32;
    format!("{:02}:{:02}:{:02}.{:03}", tc.hours, tc.minutes, tc.seconds, ms)
}

fn chrono_now_string() -> String {
    Local::now().format("%H:%M:%S").to_string()
}

fn make_fps_name(fps: f64, drop_frame: bool) -> &'static str {
    if (fps - 24.0).abs() < 0.01 {
        "24 fps"
    } else if (fps - 25.0).abs() < 0.01 {
        "25 fps"
    } else if (fps - 29.97).abs() < 0.01 {
        if drop_frame {
            "29.97 DF"
        } else {
            "29.97 ND"
        }
    } else if (fps - 30.0).abs() < 0.01 {
        "30 fps"
    } else {
        "25 fps"
    }
}

struct FpsOption {
    name: &'static str,
    fps: f64,
    drop_frame: bool,
    description: &'static str,
}

const FPS_OPTIONS: [FpsOption; 5] = [
    FpsOption { name: "24 fps", fps: 24.0, drop_frame: false, description: "Standard cinema & film frame rate." },
    FpsOption { name: "25 fps", fps: 25.0, drop_frame: false, description: "PAL standard (Europe, UK, Australia, Africa, Asia)." },
    FpsOption { name: "29.97 ND", fps: 29.97, drop_frame: false, description: "NTSC Non-Drop (broadcast video & web production)." },
    FpsOption { name: "29.97 DF", fps: 29.97, drop_frame: true, description: "NTSC Drop Frame (syncs clock drift to wall-time)." },
    FpsOption { name: "30 fps", fps: 30.0, drop_frame: false, description: "High-definition video rate / digital audio standard." },
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    info!("LTC Slint GUI v{} starting...", APP_VERSION);
    let os_name = std::env::consts::OS.to_uppercase();
    info!("Operating system: {}", os_name);
    info!("Default sample rate: {} Hz", suggest_sample_rate());

    let ui = AppWindow::new()?;
    let audio_core = Arc::new(Mutex::new(AudioCore::new()));

    // ── State ──────────────────────────────────────────────────────────────────
    let fps_index: Arc<Mutex<usize>> = Arc::new(Mutex::new(1)); // 25 fps default
    let sample_rate: Arc<Mutex<u32>> = Arc::new(Mutex::new(suggest_sample_rate()));
    let devices: Arc<Mutex<Vec<AudioDeviceInfo>>> = Arc::new(Mutex::new(Vec::new()));
    let logs: Arc<Mutex<Vec<LogEntry>>> = Arc::new(Mutex::new(Vec::new()));
    let flash_timer: Arc<Mutex<Option<slint::Timer>>> = Arc::new(Mutex::new(None));
    let is_playing: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let ltc_channel_index: Arc<Mutex<usize>> = Arc::new(Mutex::new(0)); // Left
    let beep_channel_index: Arc<Mutex<usize>> = Arc::new(Mutex::new(1)); // Right
    let ltc_volume: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.25));
    let beep_volume: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.5));
    let beep_frequency: Arc<Mutex<f32>> = Arc::new(Mutex::new(1000.0));
    let beep_duration: Arc<Mutex<f32>> = Arc::new(Mutex::new(0.5));
    let scene: Arc<Mutex<u32>> = Arc::new(Mutex::new(1));
    let take: Arc<Mutex<u32>> = Arc::new(Mutex::new(1));
    let roll: Arc<Mutex<String>> = Arc::new(Mutex::new("A001".to_string()));
    let auto_increment: Arc<Mutex<bool>> = Arc::new(Mutex::new(true));
    let start_timecode: Arc<Mutex<Timecode>> = Arc::new(Mutex::new(Timecode {
        hours: 1,
        minutes: 0,
        seconds: 0,
        frames: 0,
    }));
    let device_index: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));

    // ── Populate FPS options model ─────────────────────────────────────────────
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

    // ── Populate sample rate options ───────────────────────────────────────────
    {
        let rate_model = ModelRc::new(VecModel::<SharedString>::from(
            SAMPLE_RATE_OPTIONS
                .iter()
                .map(|r| SharedString::from(format!("{} Hz", r)))
                .collect::<Vec<_>>(),
        ));
        ui.set_sample_rate_options(rate_model);
        let rate_index = SAMPLE_RATE_OPTIONS
            .iter()
            .position(|&r| r == suggest_sample_rate())
            .unwrap_or(0);
        ui.set_sample_rate_index(rate_index as i32);
    }

    // ── Set OS name ────────────────────────────────────────────────────────────
    ui.set_os_name(SharedString::from(os_name.clone()));

    // ── Refresh devices ────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let audio_core_clone = audio_core.clone();
        let devices_clone = devices.clone();
        let device_index_clone = device_index.clone();
        let sample_rate_clone = sample_rate.clone();

        let refresh = move || {
            let ui = match ui_weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let mut devs = devices_clone.lock().unwrap();
            match list_audio_devices() {
                Ok(device_list) => {
                    *devs = device_list;
                    let count = devs.len();
                    info!("Found {} audio devices", count);
                    let device_names: Vec<SharedString> = devs
                        .iter()
                        .map(|d| {
                            if d.is_default {
                                SharedString::from(format!("{} (Default)", d.name))
                            } else {
                                SharedString::from(d.name.clone())
                            }
                        })
                        .collect();
                    let name_model = ModelRc::new(VecModel::<SharedString>::from(device_names));
                    ui.set_device_names(name_model);
                    ui.set_device_count(count as i32);

                    // Auto-initialize audio on first device
                    if count > 0 {
                        let mut idx = device_index_clone.lock().unwrap();
                        if *idx >= count {
                            *idx = 0;
                        }
                        ui.set_device_index(*idx as i32);

                        // Initialize audio output
                        let device_id = if devs[*idx].id == "default" {
                            String::new()
                        } else {
                            devs[*idx].id.clone()
                        };
                        let rate = *sample_rate_clone.lock().unwrap();
                        info!(
                            "Initializing audio on device: {} (id={}, rate={})",
                            devs[*idx].name, device_id, rate
                        );
                        let core = audio_core_clone.lock().unwrap();
                        match core.init_output(&device_id, rate, BUFFER_SIZE) {
                            Ok(actual_rate) => {
                                let fmt = core.sample_format_name();
                                info!(
                                    "Audio initialized: rate={}, format={}",
                                    actual_rate, fmt
                                );
                                ui.set_sample_format(SharedString::from(fmt.to_uppercase()));
                                ui.set_status_message(SharedString::from(format!(
                                    "Audio initialized ({})",
                                    fmt
                                )));
                                let mut sr = sample_rate_clone.lock().unwrap();
                                if actual_rate != *sr {
                                    warn!(
                                        "Sample rate overridden: {} -> {}",
                                        *sr, actual_rate
                                    );
                                    *sr = actual_rate;
                                    let rate_idx = SAMPLE_RATE_OPTIONS
                                        .iter()
                                        .position(|&r| r == actual_rate)
                                        .unwrap_or(0);
                                    ui.set_sample_rate_index(rate_idx as i32);
                                }
                            }
                            Err(e) => {
                                error!("Failed to initialize audio: {}", e);
                                ui.set_status_message(SharedString::from(format!(
                                    "Audio init failed: {}",
                                    e
                                )));
                            }
                        }
                    } else {
                        warn!("No audio devices found");
                        ui.set_status_message(SharedString::from("No audio devices found"));
                    }
                }
                Err(e) => {
                    error!("Failed to list audio devices: {}", e);
                    ui.set_status_message(SharedString::from(format!("Device error: {}", e)));
                }
            }
        };

        refresh();
        ui.on_refresh_devices(refresh);
    }

    // ── Clock callbacks ────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let audio_core_clone = audio_core.clone();
        let fps_index_clone = fps_index.clone();
        let ltc_channel_index_clone = ltc_channel_index.clone();
        let ltc_volume_clone = ltc_volume.clone();
        let is_playing_clone = is_playing.clone();
        let start_timecode_clone = start_timecode.clone();

        ui.on_start_ltc(move || {
            let ui = match ui_weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            let fi = *fps_index_clone.lock().unwrap();
            let opt = &FPS_OPTIONS[fi];
            let tc = *start_timecode_clone.lock().unwrap();
            let channel = channel_to_str(*ltc_channel_index_clone.lock().unwrap());
            let vol = *ltc_volume_clone.lock().unwrap();

            info!(
                "Starting LTC: tc={:02}:{:02}:{:02}:{:02}, fps={}, drop_frame={}, channel={}, volume={}",
                tc.hours, tc.minutes, tc.seconds, tc.frames, opt.fps, opt.drop_frame, channel, vol
            );
            let core = audio_core_clone.lock().unwrap();
            match core.start_ltc(tc, opt.fps, opt.drop_frame, channel.to_string(), vol) {
                Ok(()) => {
                    *is_playing_clone.lock().unwrap() = true;
                    ui.set_is_playing(true);
                    ui.set_status_message(SharedString::from("Streaming LTC"));
                    info!("LTC stream started");
                }
                Err(e) => {
                    error!("Failed to start LTC: {}", e);
                    ui.set_status_message(SharedString::from(format!("Start failed: {}", e)));
                }
            }
        });
    }

    {
        let ui_weak = ui.as_weak();
        let audio_core_clone = audio_core.clone();
        let is_playing_clone = is_playing.clone();

        ui.on_stop_ltc(move || {
            info!("Stopping LTC stream");
            let core = audio_core_clone.lock().unwrap();
            if let Err(e) = core.stop_ltc() {
                error!("Failed to stop LTC: {}", e);
            }
            *is_playing_clone.lock().unwrap() = false;
            let ui = match ui_weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            ui.set_is_playing(false);
            ui.set_wake_lock_active(false);
            ui.set_status_message(SharedString::from("Stopped"));
            info!("LTC stream stopped");
        });
    }

    {
        let ui_weak = ui.as_weak();
        let audio_core_clone = audio_core.clone();
        let start_timecode_clone = start_timecode.clone();

        ui.on_reset_tc(move || {
            let tc = *start_timecode_clone.lock().unwrap();
            info!("Resetting LTC to {:02}:{:02}:{:02}:{:02}", tc.hours, tc.minutes, tc.seconds, tc.frames);
            let core = audio_core_clone.lock().unwrap();
            if let Err(e) = core.reset_ltc(tc) {
                error!("Failed to reset LTC: {}", e);
            }
            let ui = match ui_weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            ui.set_status_message(SharedString::from("Reset"));
        });
    }

    {
        let is_locked = Arc::new(Mutex::new(false));
        let ui_weak = ui.as_weak();
        let is_locked_clone = is_locked.clone();

        ui.on_toggle_lock(move || {
            let mut locked = is_locked_clone.lock().unwrap();
            *locked = !*locked;
            let ui = match ui_weak.upgrade() {
                Some(u) => u,
                None => return,
            };
            ui.set_is_locked(*locked);
            info!("Controls {}", if *locked { "locked" } else { "unlocked" });
        });
    }

    // ── Clapper callbacks ──────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let audio_core_clone = audio_core.clone();
        let sample_rate_clone = sample_rate.clone();
        let beep_volume_clone = beep_volume.clone();
        let beep_frequency_clone = beep_frequency.clone();
        let beep_duration_clone = beep_duration.clone();
        let beep_channel_index_clone = beep_channel_index.clone();
        let logs_clone = logs.clone();
        let fps_index_clone = fps_index.clone();
        let flash_timer_clone = flash_timer.clone();
        let scene_clone = scene.clone();
        let take_clone = take.clone();
        let roll_clone = roll.clone();
        let auto_increment_clone = auto_increment.clone();

        ui.on_clap_beep(move || {
            let sr = *sample_rate_clone.lock().unwrap();
            let vol = *beep_volume_clone.lock().unwrap();
            let freq = *beep_frequency_clone.lock().unwrap();
            let dur = *beep_duration_clone.lock().unwrap();
            let ch = channel_to_str(*beep_channel_index_clone.lock().unwrap());

            info!(
                "Clap triggered: freq={}Hz, volume={}, duration={}s, channel={}",
                freq, vol, dur, ch
            );

            {
                let core = audio_core_clone.lock().unwrap();
                if let Err(e) = core.play_beep(sr, freq, dur, vol, ch) {
                    error!("Failed to play beep: {}", e);
                }
            }

            let ui = match ui_weak.upgrade() {
                Some(u) => u,
                None => return,
            };

            // Flash effect
            ui.set_flash_visible(true);
            let flash_weak = ui.as_weak();
            let ft = slint::Timer::default();
            ft.start(
                slint::TimerMode::SingleShot,
                Duration::from_millis(200),
                move || {
                    if let Some(fui) = flash_weak.upgrade() {
                        fui.set_flash_visible(false);
                    }
                },
            );
            *flash_timer_clone.lock().unwrap() = Some(ft);

            // Log the clap
            let fi = *fps_index_clone.lock().unwrap();
            let opt = &FPS_OPTIONS[fi];
            let tc = audio_core_clone.lock().unwrap().current_timecode();
            let tc_str = timecode_to_string(tc, opt.drop_frame);
            let ms_str = timecode_to_ms_string(tc, opt.fps);
            let timestamp = chrono_now_string();
            let s = *scene_clone.lock().unwrap();
            let t = *take_clone.lock().unwrap();
            let r = roll_clone.lock().unwrap().clone();
            let note = format!(
                "Scene {} / Take {} / Roll {}",
                s,
                t,
                if r.is_empty() { "—" } else { &r }
            );
            info!("Clap logged: {}", note);
            let entry = LogEntry {
                timestamp: SharedString::from(timestamp),
                timecode: SharedString::from(tc_str),
                milliseconds: SharedString::from(ms_str),
                note: SharedString::from(note),
            };
            let mut log_vec = logs_clone.lock().unwrap();
            log_vec.push(entry);

            // Update the model
            let log_model = ModelRc::new(VecModel::<LogEntry>::from(log_vec.clone()));
            ui.set_logs(log_model);

            if *auto_increment_clone.lock().unwrap() {
                let mut t = take_clone.lock().unwrap();
                *t += 1;
                ui.set_take(*t as i32);
            }
        });
    }

    // ── Scene/Take/Roll callbacks ─────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let scene_clone = scene.clone();
        ui.on_scene_up(move || {
            let mut s = scene_clone.lock().unwrap();
            *s = s.saturating_add(1);
            if let Some(u) = ui_weak.upgrade() {
                u.set_scene(*s as i32);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let scene_clone = scene.clone();
        ui.on_scene_down(move || {
            let mut s = scene_clone.lock().unwrap();
            *s = s.saturating_sub(1);
            if let Some(u) = ui_weak.upgrade() {
                u.set_scene(*s as i32);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let take_clone = take.clone();
        ui.on_take_up(move || {
            let mut t = take_clone.lock().unwrap();
            *t = t.saturating_add(1);
            if let Some(u) = ui_weak.upgrade() {
                u.set_take(*t as i32);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let take_clone = take.clone();
        ui.on_take_down(move || {
            let mut t = take_clone.lock().unwrap();
            *t = t.saturating_sub(1);
            if let Some(u) = ui_weak.upgrade() {
                u.set_take(*t as i32);
            }
        });
    }
    {
        let roll_clone = roll.clone();
        let ui_weak = ui.as_weak();
        ui.on_roll_changed(move || {
            if let Some(u) = ui_weak.upgrade() {
                let new_text = u.get_roll().to_string();
                *roll_clone.lock().unwrap() = new_text.clone();
                info!("Roll changed to: {}", new_text);
            }
        });
    }
    {
        let auto_increment_clone = auto_increment.clone();
        let ui_weak = ui.as_weak();
        ui.on_auto_increment_toggled(move || {
            let mut ai = auto_increment_clone.lock().unwrap();
            *ai = !*ai;
            if let Some(u) = ui_weak.upgrade() {
                u.set_auto_increment(*ai);
            }
            info!("Auto-increment take: {}", if *ai { "on" } else { "off" });
        });
    }

    // ── Log management ─────────────────────────────────────────────────────────
    {
        let logs_clone = logs.clone();
        let ui_weak = ui.as_weak();
        ui.on_clear_logs(move || {
            let mut log_vec = logs_clone.lock().unwrap();
            log_vec.clear();
            let empty_model: ModelRc<LogEntry> = ModelRc::new(VecModel::default());
            if let Some(u) = ui_weak.upgrade() {
                u.set_logs(empty_model);
            }
            info!("Logs cleared");
        });
    }
    {
        let logs_clone = logs.clone();
        ui.on_copy_logs(move || {
            let log_vec = logs_clone.lock().unwrap();
            let text: String = log_vec
                .iter()
                .map(|l| {
                    format!(
                        "[{}] LTC: {} | MS: {} | {}",
                        l.timestamp, l.timecode, l.milliseconds, l.note
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                info!("Copied log text:\n{}", text);
            }
        });
    }

    // ── Settings callbacks ─────────────────────────────────────────────────────
fn stepper_handlers(
    ui_weak: &Weak<AppWindow>,
    tc: &Arc<Mutex<Timecode>>,
    field: fn(&mut Timecode) -> &mut u32,
    max: u32,
    direction: i32,
    label: &str,
) -> impl FnMut() {
    let ui_weak = ui_weak.clone();
    let tc_clone = tc.clone();
    let label_str = label.to_string();
    move || {
        let mut t = tc_clone.lock().unwrap();
        let val = field(&mut t);
        if direction > 0 {
            *val = (*val + 1) % max;
        } else {
            *val = if *val == 0 { max - 1 } else { *val - 1 };
        }
        info!("Starting timecode {} changed to {:02}", label_str, *val);
        let ui = match ui_weak.upgrade() {
            Some(u) => u,
            None => return,
        };
        ui.set_hour(t.hours as i32);
        ui.set_minute(t.minutes as i32);
        ui.set_second(t.seconds as i32);
        ui.set_frame(t.frames as i32);
    }
}

    let st = start_timecode.clone();
    let uiw = ui.as_weak();
    ui.on_hour_up(stepper_handlers(&uiw, &st, |tc| &mut tc.hours, 24, 1, "hours"));
    let st = start_timecode.clone();
    let uiw = ui.as_weak();
    ui.on_hour_down(stepper_handlers(&uiw, &st, |tc| &mut tc.hours, 24, -1, "hours"));
    let st = start_timecode.clone();
    let uiw = ui.as_weak();
    ui.on_minute_up(stepper_handlers(&uiw, &st, |tc| &mut tc.minutes, 60, 1, "minutes"));
    let st = start_timecode.clone();
    let uiw = ui.as_weak();
    ui.on_minute_down(stepper_handlers(&uiw, &st, |tc| &mut tc.minutes, 60, -1, "minutes"));
    let st = start_timecode.clone();
    let uiw = ui.as_weak();
    ui.on_second_up(stepper_handlers(&uiw, &st, |tc| &mut tc.seconds, 60, 1, "seconds"));
    let st = start_timecode.clone();
    let uiw = ui.as_weak();
    ui.on_second_down(stepper_handlers(&uiw, &st, |tc| &mut tc.seconds, 60, -1, "seconds"));
    {
        let ui_weak = ui.as_weak();
        let tc_clone = start_timecode.clone();
        let fps_idx = fps_index.clone();
        ui.on_frame_up(move || {
            let mut t = tc_clone.lock().unwrap();
            let fi = *fps_idx.lock().unwrap();
            let opt = &FPS_OPTIONS[fi];
            let max = opt.fps.ceil() as u32;
            t.frames = (t.frames + 1) % max;
            info!("Starting timecode frames changed to {:02}", t.frames);
            if let Some(u) = ui_weak.upgrade() {
                u.set_hour(t.hours as i32);
                u.set_minute(t.minutes as i32);
                u.set_second(t.seconds as i32);
                u.set_frame(t.frames as i32);
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let tc_clone = start_timecode.clone();
        let fps_idx = fps_index.clone();
        ui.on_frame_down(move || {
            let mut t = tc_clone.lock().unwrap();
            let fi = *fps_idx.lock().unwrap();
            let opt = &FPS_OPTIONS[fi];
            let max = opt.fps.ceil() as u32;
            t.frames = if t.frames == 0 { max - 1 } else { t.frames - 1 };
            info!("Starting timecode frames changed to {:02}", t.frames);
            if let Some(u) = ui_weak.upgrade() {
                u.set_hour(t.hours as i32);
                u.set_minute(t.minutes as i32);
                u.set_second(t.seconds as i32);
                u.set_frame(t.frames as i32);
            }
        });
    }

    // ── FPS selection ──────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let fps_index_clone = fps_index.clone();
        let start_tc_clone = start_timecode.clone();
        ui.on_fps_selected(move |fps_value_x100| {
            let target_fps = fps_value_x100 as f64 / 100.0;
            let mut fi = fps_index_clone.lock().unwrap();
            if let Some(idx) = FPS_OPTIONS.iter().position(|opt| (opt.fps - target_fps).abs() < 0.01) {
                *fi = idx;
                let opt = &FPS_OPTIONS[idx];
                info!(
                    "Frame rate changed to: {} ({} fps, drop_frame={})",
                    opt.name, opt.fps, opt.drop_frame
                );
                if let Some(u) = ui_weak.upgrade() {
                    u.set_fps_index(idx as i32);
                    u.set_fps_name(SharedString::from(make_fps_name(opt.fps, opt.drop_frame)));
                    let max_frames = opt.fps.ceil() as i32;
                    u.set_max_frame(max_frames);

                    // Sync start_timecode Mutex from UI values
                    let tc = Timecode {
                        hours: u.get_hour() as u32,
                        minutes: u.get_minute() as u32,
                        seconds: u.get_second() as u32,
                        frames: u.get_frame() as u32,
                    };
                    *start_tc_clone.lock().unwrap() = tc;

                    let tc_str = timecode_to_string(tc, opt.drop_frame);
                    let ms_str = timecode_to_ms_string(tc, opt.fps);
                    u.set_timecode_text(SharedString::from(tc_str));
                    u.set_ms_text(SharedString::from(ms_str));
                }
            }
        });
    }

    // ── Sample rate selection ──────────────────────────────────────────────────
    {
        let sample_rate_clone = sample_rate.clone();
        let ui_weak = ui.as_weak();
        ui.on_sample_rate_tapped(move |rate_text| {
            let rate_str = rate_text.trim_end_matches(" Hz");
            if let Ok(rate) = rate_str.parse::<u32>() {
                info!("Sample rate changed to: {} Hz", rate);
                *sample_rate_clone.lock().unwrap() = rate;
                if let Some(u) = ui_weak.upgrade() {
                    if let Some(idx) = SAMPLE_RATE_OPTIONS.iter().position(|&r| r == rate) {
                        u.set_sample_rate_index(idx as i32);
                    }
                }
            }
        });
    }

    // ── Device selection ───────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let audio_core_clone = audio_core.clone();
        let devices_clone = devices.clone();
        let device_index_clone = device_index.clone();
        let sample_rate_clone = sample_rate.clone();
        let is_playing_clone = is_playing.clone();
        let fps_index_clone = fps_index.clone();
        let ltc_channel_index_clone = ltc_channel_index.clone();
        let ltc_volume_clone = ltc_volume.clone();

        ui.on_device_selected(move |index| {
            let idx = index as usize;
            let devs = devices_clone.lock().unwrap();
            if idx >= devs.len() {
                warn!("Device index {} out of range", idx);
                return;
            }
            let device = &devs[idx];
            info!("Device selected: {} (index {})", device.name, idx);

            let was_playing = *is_playing_clone.lock().unwrap();
            if was_playing {
                let core = audio_core_clone.lock().unwrap();
                let _ = core.stop_ltc();
                drop(core);
            }

            // Re-init audio on new device
            let core = audio_core_clone.lock().unwrap();
            let _ = core.stop_output();
            drop(core);

            *device_index_clone.lock().unwrap() = idx;
            let device_id = if device.id == "default" {
                String::new()
            } else {
                device.id.clone()
            };
            let rate = *sample_rate_clone.lock().unwrap();
            let core = audio_core_clone.lock().unwrap();
            match core.init_output(&device_id, rate, BUFFER_SIZE) {
                Ok(actual_rate) => {
                    let fmt = core.sample_format_name();
                    info!("Audio re-initialized on {}: rate={}, format={}", device.name, actual_rate, fmt);
                    if let Some(u) = ui_weak.upgrade() {
                        u.set_device_index(idx as i32);
                        u.set_sample_format(SharedString::from(fmt.to_uppercase()));
                        u.set_status_message(SharedString::from(format!("Audio: {}", device.name)));
                    }
                    if actual_rate != rate {
                        *sample_rate_clone.lock().unwrap() = actual_rate;
                        let rate_idx = SAMPLE_RATE_OPTIONS.iter().position(|&r| r == actual_rate).unwrap_or(0);
                        if let Some(u) = ui_weak.upgrade() {
                            u.set_sample_rate_index(rate_idx as i32);
                        }
                    }
                    if was_playing {
                        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
                        let fi = *fps_index_clone.lock().unwrap();
                        let opt = &FPS_OPTIONS[fi];
                        let ch = channel_to_str(*ltc_channel_index_clone.lock().unwrap());
                        let vol = *ltc_volume_clone.lock().unwrap();
                        match core.start_ltc(tc, opt.fps, opt.drop_frame, ch.to_string(), vol) {
                            Ok(()) => {
                                *is_playing_clone.lock().unwrap() = true;
                                if let Some(u) = ui_weak.upgrade() {
                                    u.set_is_playing(true);
                                }
                                info!("LTC restarted on new device");
                            }
                            Err(e) => error!("Failed to restart LTC on new device: {}", e),
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to init audio on new device: {}", e);
                    if let Some(u) = ui_weak.upgrade() {
                        u.set_status_message(SharedString::from(format!("Device init failed: {}", e)));
                    }
                }
            }
        });
    }

    // ── Channel routing callbacks ──────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let ltc_channel_index_clone = ltc_channel_index.clone();
        ui.on_ltc_channel_selected(move |index| {
            let idx = index as usize;
            *ltc_channel_index_clone.lock().unwrap() = idx;
            let name = channel_to_str(idx).to_uppercase();
            info!("LTC channel changed to: {}", name);
            if let Some(u) = ui_weak.upgrade() {
                u.set_ltc_route(SharedString::from(name));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let beep_channel_index_clone = beep_channel_index.clone();
        ui.on_beep_channel_selected(move |index| {
            let idx = index as usize;
            *beep_channel_index_clone.lock().unwrap() = idx;
            let name = channel_to_str(idx).to_uppercase();
            info!("Beep channel changed to: {}", name);
            if let Some(u) = ui_weak.upgrade() {
                u.set_beep_route(SharedString::from(name));
            }
        });
    }

    // ── Volume/frequency/duration sliders ──────────────────────────────────────
    {
        let ltc_volume_clone = ltc_volume.clone();
        ui.on_ltc_volume_changed(move |val| {
            *ltc_volume_clone.lock().unwrap() = val;
        });
    }
    {
        let beep_volume_clone = beep_volume.clone();
        ui.on_beep_volume_changed(move |val| {
            *beep_volume_clone.lock().unwrap() = val;
        });
    }
    {
        let beep_frequency_clone = beep_frequency.clone();
        ui.on_beep_frequency_changed(move |val| {
            *beep_frequency_clone.lock().unwrap() = val;
        });
    }
    {
        let beep_duration_clone = beep_duration.clone();
        ui.on_beep_duration_changed(move |val| {
            *beep_duration_clone.lock().unwrap() = val;
        });
    }

    // ── Polling timer (25 fps) ─────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let audio_core_clone = audio_core.clone();
        let fps_index_clone = fps_index.clone();
        let is_playing_clone = is_playing.clone();

        let poll_timer = slint::Timer::default();
        poll_timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(POLL_INTERVAL_MS),
            move || {
                let ui = match ui_weak.upgrade() {
                    Some(u) => u,
                    None => return,
                };

                // Update system time in header
                ui.set_system_time(SharedString::from(format!("{} UTC", chrono_now_string())));

                // Poll timecode if playing
                if *is_playing_clone.lock().unwrap() {
                    let core = audio_core_clone.lock().unwrap();
                    let tc = core.current_timecode();
                    let fi = *fps_index_clone.lock().unwrap();
                    let opt = &FPS_OPTIONS[fi];
                    let tc_str = timecode_to_string(tc, opt.drop_frame);
                    let ms_str = timecode_to_ms_string(tc, opt.fps);
                    let wake = core.wake_lock_active();
                    drop(core);

                    ui.set_timecode_text(SharedString::from(tc_str));
                    ui.set_ms_text(SharedString::from(ms_str));
                    ui.set_wake_lock_active(wake);
                }

                // Drain audio events
                let core = audio_core_clone.lock().unwrap();
                let events = core.drain_events();
                for evt in events {
                    match evt {
                        AudioEvent::StreamError(msg) => {
                            error!("Audio stream error: {}", msg);
                        }
                        AudioEvent::StreamDied => {
                            warn!("Audio stream has died");
                        }
                        AudioEvent::StreamRecovering { attempt } => {
                            info!("Audio stream recovering (attempt {})", attempt);
                        }
                        AudioEvent::StreamDead => {
                            error!("Fatal: audio device unreachable");
                            *is_playing_clone.lock().unwrap() = false;
                            ui.set_is_playing(false);
                        }
                        AudioEvent::RecoveryNeeded { reason } => {
                            warn!("Audio recovery needed: {}", reason);
                        }
                        AudioEvent::Underrun => {
                            warn!("Audio underrun detected");
                        }
                        AudioEvent::FramesDropped { total } => {
                            warn!("{} frame(s) dropped", total);
                        }
                    }
                }
            },
        );

        // Leak the timer so it lives for the entire program lifetime
        Box::leak(Box::new(poll_timer));
    }

    // ── Run the app ───────────────────────────────────────────────────────────
    info!("LTC Slint GUI initialized, showing window");
    ui.run()?;

    // Cleanup
    let core = audio_core.lock().unwrap();
    let _ = core.stop_output();
    info!("LTC Slint GUI shutting down");
    drop(core);

    Ok(())
}