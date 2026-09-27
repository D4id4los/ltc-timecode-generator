#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gui_engine::command::{ConverterCommand, GuiCommand, OffloadCommand};
use gui_engine::config;
use gui_engine::state::{AppStateSnapshot, ConverterUserSettings};
use gui_engine::timecode::{self, FPS_OPTIONS};
use gui_engine::{ArcSwap, SAMPLE_RATE_OPTIONS};
use log::info;
use slint::{ModelRc, SharedString, VecModel};

use crate::poll::setup_poll_timer;
use crate::theme::set_theme_palette;
use crate::timecode_helpers::set_tc_segments;
use crate::toast::{push_toast, update_toast_model, ToastItem};

mod poll;
mod theme;
mod timecode_helpers;
mod toast;

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");

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

    set_theme_palette(&ui);

    // ── Toast state ─────────────────────────────────────────────────────────
    let toasts: Arc<Mutex<Vec<ToastItem>>> = Arc::new(Mutex::new(Vec::new()));
    let next_toast_id: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
    let last_debug_log_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let pulse_phase: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));

    // ── Converter state cache (mirrored from engine each tick by poll.rs) ──
    let conv_settings_cache: Arc<Mutex<ConverterUserSettings>> =
        Arc::new(Mutex::new(ConverterUserSettings::initial()));

    // ── Restore last used converter folders from config ─────────────────────
    {
        let cfg = config::load();
        if let Some(ref folder) = cfg.last_input_folder {
            let path = std::path::Path::new(folder);
            if path.exists() {
                ui.set_conv_selected_folder(SharedString::from(folder.as_str()));
                let _ = cmd_tx.send(GuiCommand::Converter(ConverterCommand::SelectFolder(
                    path.to_path_buf(),
                )));
                // Auto-select first group after engine publishes groups
                let cmd = cmd_tx.clone();
                let ui_weak = ui.as_weak();
                let select_timer = slint::Timer::default();
                select_timer.start(
                    slint::TimerMode::SingleShot,
                    Duration::from_millis(100),
                    move || {
                        let _ = cmd.send(GuiCommand::Converter(
                            ConverterCommand::SelectRecording(0),
                        ));
                        if let Some(u) = ui_weak.upgrade() {
                            u.set_conv_selected_group_idx(0);
                        }
                    },
                );
                Box::leak(Box::new(select_timer));
            }
        }
        if let Some(ref out_path) = cfg.last_output_folder {
            ui.set_conv_output_folder(SharedString::from(out_path.as_str()));
            let _ = cmd_tx.send(GuiCommand::Converter(ConverterCommand::SetOutputFolder(
                PathBuf::from(out_path),
            )));
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
        let buffer_smp = (rate as f64 / s.fps).round() as i32;
        ui.set_buffer_size(buffer_smp);
        let tc_str = timecode::timecode_to_string(s.current_timecode, s.drop_frame);
        set_tc_segments(&ui, &tc_str);
        ui.set_ms_text(SharedString::from(timecode::timecode_to_ms_string(s.current_timecode, s.fps)));
        ui.set_fps_name(SharedString::from(FPS_OPTIONS[s.fps_index].name));
        ui.set_decode_fps_index(s.decode_fps_index as i32);
    }

    // ── Refresh devices ────────────────────────────────────────────────────
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

    // ── Theme toggle ────────────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_toggle_theme(move || {
            let _ = cmd.send(GuiCommand::ToggleTheme);
        });
    }

    // ── Debug log toggle ────────────────────────────────────────────────────
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

    // ── Transport callbacks ────────────────────────────────────────────────
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

    // ── Clapper callbacks ──────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_clap_beep(move || { let _ = cmd.send(GuiCommand::Clap); });
    }

    // ── Scene / Take / Roll ────────────────────────────────────────────────
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

    // ── Logs ───────────────────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_clear_logs(move || {
            let _ = cmd.send(GuiCommand::ClearLogs);
        });
    }

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

    // ── Toast dismiss ──────────────────────────────────────────────────────
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

    // ── Tab click ──────────────────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        ui.on_tab_clicked(move |index| {
            if let Some(u) = ui_weak.upgrade() {
                u.set_active_tab(index);
            }
        });
    }

    // ── Timecode steppers ──────────────────────────────────────────────────
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

    // ── FPS selection ──────────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_fps_selected(move |index| { let _ = cmd.send(GuiCommand::SetFpsIndex(index as usize)); });
    }

    // ── Sample rate selection ──────────────────────────────────────────────
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

    // ── Device selection ──────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_device_selected(move |index| { let _ = cmd.send(GuiCommand::SetDevice(index as usize)); });
    }

    // ── Routing ───────────────────────────────────────────────────────────
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

    // ── Volume / pitch / duration sliders ──────────────────────────────────
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

    // ── Converter callbacks ──────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
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
        let cmd = cmd_tx.clone();
        ui.on_conv_select_pattern(move |new_pattern| {
            let val = if new_pattern >= 0 { Some(new_pattern as usize) } else { None };
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetNamingPattern(val)));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_conv_select_group(move |group_idx| {
            if group_idx >= 0 {
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SelectRecording(
                    group_idx as usize,
                )));
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_conv_map_cell_clicked(move |row, col| {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SwapChannelMapCells(
                row as usize, col as usize,
            )));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_conv_start(move || {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::StartConversion));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_conv_cancel(move || {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::CancelConversion));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_toggle_set_start_ltc(move || {
            let val = !cache.lock().unwrap().set_start_from_ltc;
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetStartFromLtc(val)));
        });
    }
    {
        let state = engine_state.clone();
        ui.on_conv_copy_log(move || {
            let s = state.load();
            let text = s.converter.conversion_state.ffmpeg_output.clone();
            if let Ok(mut ctx) = arboard::Clipboard::new() {
                let _ = ctx.set_text(text);
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_conv_container_selected(move |idx| {
            let settings = cache.lock().unwrap();
            let options = gui_engine::converter::supported_containers();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                drop(settings);
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetContainer(key)));
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_conv_video_selected(move |idx| {
            let settings = cache.lock().unwrap();
            let options = gui_engine::video_codecs::supported_video_codecs();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                drop(settings);
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetVideoCodec(key)));
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_conv_audio_selected(move |idx| {
            let settings = cache.lock().unwrap();
            let options = gui_engine::converter::supported_audio_encoders();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                drop(settings);
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetAudioEncoder(key)));
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_select_output_folder(move || {
            let dialog = rfd::FileDialog::new();
            if let Some(path) = dialog.pick_folder() {
                let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetOutputFolder(
                    path.clone(),
                )));
                config::save_output_folder(&path);
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_output_folder(SharedString::from(path.to_string_lossy().as_ref()));
                }
            }
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_conv_filename_prefix_changed(move |val| {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetFilenamePrefix(
                val.to_string(),
            )));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_conv_audio_suffix_changed(move |val| {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetAudioSuffixTemplate(
                val.to_string(),
            )));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_conv_video_suffix_changed(move |val| {
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetVideoSuffixTemplate(
                val.to_string(),
            )));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_toggle_split_tracks(move || {
            let val = !cache.lock().unwrap().split_tracks;
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetSplitTracks(val)));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_toggle_drop_ltc_track(move || {
            let val = !cache.lock().unwrap().drop_ltc_track;
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetDropLtcTrack(val)));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_toggle_concat_audio(move || {
            let val = !cache.lock().unwrap().concat_audio;
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetConcatAudio(val)));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_toggle_synthetic_video(move || {
            let val = !cache.lock().unwrap().generate_synthetic_video;
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetGenerateSyntheticVideo(val)));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_toggle_copy_video(move || {
            let val = !cache.lock().unwrap().copy_video;
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetCopyVideo(val)));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_toggle_metadata_only(move || {
            let val = !cache.lock().unwrap().metadata_only;
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetMetadataOnly(val)));
        });
    }
    {
        let cmd = cmd_tx.clone();
        let cache = conv_settings_cache.clone();
        ui.on_conv_reset(move || {
            *cache.lock().unwrap() = ConverterUserSettings::initial();
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetMetadataOnly(false)));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetSplitTracks(false)));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetDropLtcTrack(false)));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetConcatAudio(false)));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetGenerateSyntheticVideo(false)));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetCopyVideo(false)));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetFilenamePrefix(String::new())));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetAudioSuffixTemplate(
                gui_engine::naming::DEFAULT_AUDIO_SUFFIX.to_string(),
            )));
            let _ = cmd.send(GuiCommand::Converter(ConverterCommand::SetVideoSuffixTemplate(
                gui_engine::naming::DEFAULT_VIDEO_SUFFIX.to_string(),
            )));
        });
    }

    // ── LTC detection callback ─────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let cmd = cmd_tx.clone();
        let detect_engine_state = engine_state.clone();
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
            let ltc_idx = 0;
            if ltc_idx >= files.len() { return; }
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
                let (stream_idx, channel_idx) = s.ltc_probe.as_ref().map_or((0, 0), |probe| {
                    let mut flat = 0usize;
                    for st in &probe.streams {
                        for ch in 0..st.channels {
                            if flat == flat_idx {
                                return (st.stream_index, ch);
                            }
                            flat += 1;
                        }
                    }
                    (0, 0)
                });
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
        let cmd = cmd_tx.clone();
        let chan_engine_state = engine_state.clone();
        ui.on_channel_selected(move |flat_idx| {
            let s = chan_engine_state.load();
            if let Some(ref probe) = s.ltc_probe {
                let mut flat = 0usize;
                for st in &probe.streams {
                    for ch in 0..st.channels {
                        if flat == flat_idx as usize {
                            let _ = cmd.send(GuiCommand::SetLtcDecodeStream(st.stream_index));
                            let _ = cmd.send(GuiCommand::SetLtcDecodeChannel(ch));
                            return;
                        }
                        flat += 1;
                    }
                }
            }
        });
    }

    // ── Decode FPS selection callback ───────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_decode_fps_selected(move |index| {
            let _ = cmd.send(GuiCommand::SetDecodeFpsIndex(index as usize));
        });
    }

    // ── Cancel LTC decode callback ───────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_cancel_decode(move || {
            let _ = cmd.send(GuiCommand::CancelDecode);
        });
    }

    // ── Copy LTC report callback ──────────────────────────────────────────
    {
        let state = engine_state.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_copy_ltc_report(move || {
            let s = state.load();
            if let Some(ref result) = s.ltc_decode_result {
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

    // ── Offload callbacks ───────────────────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
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
        let cmd = cmd_tx.clone();
        ui.on_off_parent_name_changed(move |val| {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::SetParentName(
                val.to_string(),
            )));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_off_card_name_changed(move |idx, val| {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::SetDeviceName(
                idx as usize, val.to_string(),
            )));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_off_rescan(move || {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::ScanCards));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_off_start(move || {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::StartOffload));
        });
    }
    {
        let cmd = cmd_tx.clone();
        ui.on_off_cancel(move || {
            let _ = cmd.send(GuiCommand::Offload(OffloadCommand::CancelOffload));
        });
    }

    // ── Poll timer — state sync ────────────────────────────────────────────
    setup_poll_timer(
        &ui,
        engine_state,
        toasts,
        next_toast_id,
        log_buffer,
        last_debug_log_count,
        pulse_phase,
        conv_settings_cache,
    );

    info!("LTC Slint GUI initialized, showing window");
    ui.run()?;

    info!("LTC Slint GUI shutting down");
    Ok(())
}