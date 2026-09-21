slint::include_modules!();

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gui_engine::command::GuiCommand;
use gui_engine::config;
use gui_engine::converter::{
    available_audio_encoders_for_container, available_containers,
    available_video_encoders_for_container, find_timecode_at_offset,
    query_ffmpeg_capabilities, select_best_combination, spawn_conversion,
    ChannelMap, ConversionPipeline, ConversionState, ConverterSettings,
    DEFAULT_AUDIO_SUFFIX, DEFAULT_VIDEO_SUFFIX, FfmpegCapabilities,
    RecordingType, TimecodeMetadata,
};
use gui_engine::file_pattern::{match_files_to_groups, wrap_user_selected_files, BUILTIN_PATTERNS};
use gui_engine::state::AppStateSnapshot;
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

/// Update the Slint dropdown models for container/video/audio options
/// based on ffmpeg capabilities and current container selection.
fn update_converter_options(
    ui: &AppWindow,
    caps: &FfmpegCapabilities,
    container: &str,
) {
    let container_options: Vec<SharedString> = available_containers(caps)
        .iter().map(|(k, _)| SharedString::from(*k)).collect();
    ui.set_conv_container_options(ModelRc::new(VecModel::<SharedString>::from(container_options)));

    let video_options: Vec<SharedString> = available_video_encoders_for_container(container, caps)
        .iter().map(|(k, _)| SharedString::from(*k)).collect();
    ui.set_conv_video_encoder_options(ModelRc::new(VecModel::<SharedString>::from(video_options)));

    let audio_options: Vec<SharedString> = available_audio_encoders_for_container(container, caps)
        .iter().map(|(k, _)| SharedString::from(*k)).collect();
    ui.set_conv_audio_encoder_options(ModelRc::new(VecModel::<SharedString>::from(audio_options)));
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

    // ── Converter state ─────────────────────────────────────────────────────
    let conv_selected_pattern: Arc<Mutex<i32>> = Arc::new(Mutex::new(0));
    let conv_selected_files: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
    let conv_selected_folder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let conv_file_groups: Arc<Mutex<BTreeMap<String, Vec<PathBuf>>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    let conv_selected_group_idx: Arc<Mutex<isize>> = Arc::new(Mutex::new(-1));
    let conv_channel_map: Arc<Mutex<ChannelMap>> = Arc::new(Mutex::new(ChannelMap::identity(0)));
    let conv_container: Arc<Mutex<String>> = Arc::new(Mutex::new("mkv".to_string()));
    let conv_video_encoder: Arc<Mutex<String>> = Arc::new(Mutex::new("libsvtav1".to_string()));
    let conv_audio_encoder: Arc<Mutex<String>> = Arc::new(Mutex::new("pcm_s24le".to_string()));
    let conv_output_path: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let conv_filename_prefix: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let conv_split_tracks: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let conv_drop_ltc_track: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let conv_generate_synthetic_video: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let conv_audio_suffix_template: Arc<Mutex<String>> = Arc::new(Mutex::new(DEFAULT_AUDIO_SUFFIX.to_string()));
    let conv_video_suffix_template: Arc<Mutex<String>> = Arc::new(Mutex::new(DEFAULT_VIDEO_SUFFIX.to_string()));
    let conv_state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let conv_cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let conv_handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let conv_ffmpeg_caps: Arc<Mutex<Option<FfmpegCapabilities>>> = Arc::new(Mutex::new(None));
    let conv_sanity_msg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let conv_trim_to_first_ltc: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let conv_trim_offset_secs: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));

    let conv_folder_for_group = conv_selected_folder.clone();
    let conv_ffmpeg_caps_for_select = conv_ffmpeg_caps.clone();

    // ── Restore last used converter folders from config ─────────────────────
    {
        let cfg = config::load();
        if let Some(ref folder) = cfg.last_input_folder {
            let path = std::path::Path::new(folder);
            if path.exists() {
                *conv_selected_folder.lock().unwrap() = folder.clone();
                let matched = match_files_to_groups(path, &BUILTIN_PATTERNS[0]);
                *conv_file_groups.lock().unwrap() = matched.clone();
                *conv_selected_group_idx.lock().unwrap() = -1;
                ui.set_conv_selected_folder(SharedString::from(folder.as_str()));
                let model: Vec<FileGroupInfo> = matched.iter().map(|(prefix, files)| {
                    FileGroupInfo {
                        prefix: SharedString::from(prefix),
                        files: ModelRc::new(VecModel::<SharedString>::from(
                            files.iter().map(|f| {
                                SharedString::from(f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                            }).collect::<Vec<_>>()
                        )),
                        channel_count: files.len() as i32,
                    }
                }).collect();
                ui.set_conv_file_groups(ModelRc::new(VecModel::<FileGroupInfo>::from(model)));
            }
        }
        if let Some(ref out_path) = cfg.last_output_folder {
            *conv_output_path.lock().unwrap() = out_path.clone();
            ui.set_conv_output_folder(SharedString::from(out_path.as_str()));
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
        let video_options: Vec<SharedString> = gui_engine::converter::supported_video_encoders()
            .iter().map(|(key, _)| SharedString::from(*key)).collect();
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
        let ui_weak = ui.as_weak();
        let pattern_arc = conv_selected_pattern.clone();
        let files_arc = conv_selected_files.clone();
        let folder = conv_selected_folder.clone();
        let groups = conv_file_groups.clone();
        let idx = conv_selected_group_idx.clone();
        let venc_for_folder = conv_video_encoder.clone();
        let aenc_for_folder = conv_audio_encoder.clone();
        let container_for_folder = conv_container.clone();
        ui.on_conv_select_folder(move || {
            let pat = *pattern_arc.lock().unwrap();
            if pat == 0 {
                // TASCAM: folder picker
                let mut dialog = rfd::FileDialog::new();
                {
                    let cur = folder.lock().unwrap();
                    if !cur.is_empty() {
                        dialog = dialog.set_directory(cur.as_str());
                    }
                }
                if let Some(path) = dialog.pick_folder() {
                    let path_str = path.to_string_lossy().to_string();
                    *folder.lock().unwrap() = path_str.clone();
                    *files_arc.lock().unwrap() = Vec::new();
                    config::save_input_folder(&path);
                    let pattern = &BUILTIN_PATTERNS[0];
                    let matched = match_files_to_groups(&path, pattern);
                    *groups.lock().unwrap() = matched.clone();
                    *idx.lock().unwrap() = -1;

                    if let Some(u) = ui_weak.upgrade() {
                        u.set_conv_selected_folder(SharedString::from(path_str));
                        let model: Vec<FileGroupInfo> = matched.iter().map(|(prefix, files)| {
                            FileGroupInfo {
                                prefix: SharedString::from(prefix),
                                files: ModelRc::new(VecModel::<SharedString>::from(
                                    files.iter().map(|f| {
                                        SharedString::from(f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                                    }).collect::<Vec<_>>()
                                )),
                                channel_count: files.len() as i32,
                            }
                        }).collect();
                        u.set_conv_file_groups(ModelRc::new(VecModel::<FileGroupInfo>::from(model)));
                    }
                }
            } else {
                // * (any): file picker
                let mut dialog = rfd::FileDialog::new()
                    .add_filter("Audio", &["*"]);
                {
                    let cur = folder.lock().unwrap();
                    if !cur.is_empty() {
                        dialog = dialog.set_directory(cur.as_str());
                    }
                }
                if let Some(paths) = dialog.pick_files()
                {
                    if !paths.is_empty() {
                        let parent = paths[0].parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                        *folder.lock().unwrap() = parent.clone();
                        *files_arc.lock().unwrap() = paths.clone();
                        let parent_path = paths[0].parent().unwrap_or(std::path::Path::new(""));
                        config::save_input_folder(parent_path);
                        let matched = wrap_user_selected_files(paths);
                        *groups.lock().unwrap() = matched.clone();
                        *idx.lock().unwrap() = -1;

                        if let Some(u) = ui_weak.upgrade() {
                            u.set_conv_selected_folder(SharedString::from(parent));
                            let model: Vec<FileGroupInfo> = matched.iter().map(|(prefix, files)| {
                                FileGroupInfo {
                                    prefix: SharedString::from(prefix),
                                    files: ModelRc::new(VecModel::<SharedString>::from(
                                        files.iter().map(|f| {
                                            SharedString::from(f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                                        }).collect::<Vec<_>>()
                                    )),
                                    channel_count: files.len() as i32,
                                }
                            }).collect();
                            u.set_conv_file_groups(ModelRc::new(VecModel::<FileGroupInfo>::from(model)));
                        }
                    }
                }
            }
            let caps = conv_ffmpeg_caps_for_select.clone();
            let ui_weak2 = ui_weak.clone();
            let container_for_update = container_for_folder.clone();
            let venc_for_update = venc_for_folder.clone();
            let aenc_for_update = aenc_for_folder.clone();
            std::thread::spawn(move || {
                let mut c = caps.lock().unwrap();
                if c.is_none() {
                    *c = Some(query_ffmpeg_capabilities());
                }
                if let Some(ref caps_data) = *c {
                    if let Some(u) = ui_weak2.upgrade() {
                        u.set_conv_has_ffmpeg(caps_data.has_ffmpeg);
                        if let Some(ref msg) = caps_data.error_message {
                            u.set_conv_ffmpeg_error(SharedString::from(msg));
                        }
                        let current_container = container_for_update.lock().unwrap().clone();
                        // Apply intelligent defaults
                        let (def_c, def_v, def_a) = select_best_combination(caps_data);
                        if current_container != def_c {
                            *container_for_update.lock().unwrap() = def_c.clone();
                            u.set_conv_container(SharedString::from(def_c.clone()));
                        }
                        if *venc_for_update.lock().unwrap() != def_v {
                            *venc_for_update.lock().unwrap() = def_v.clone();
                            u.set_conv_video_encoder(SharedString::from(def_v));
                        }
                        if *aenc_for_update.lock().unwrap() != def_a {
                            *aenc_for_update.lock().unwrap() = def_a.clone();
                            u.set_conv_audio_encoder(SharedString::from(def_a));
                        }
                        // Update dropdown models to show only available options
                        let cur_container = container_for_update.lock().unwrap().clone();
                        update_converter_options(&u, caps_data, &cur_container);
                    }
                }
            });
        });
    }
    // ── Pattern selection callback ──
    {
        let pattern_arc = conv_selected_pattern.clone();
        let files_arc = conv_selected_files.clone();
        let folder = conv_selected_folder.clone();
        let groups = conv_file_groups.clone();
        let idx = conv_selected_group_idx.clone();
        let out_path = conv_output_path.clone();
        let prefix_arc = conv_filename_prefix.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_select_pattern(move |new_pattern| {
            *pattern_arc.lock().unwrap() = new_pattern;
            *files_arc.lock().unwrap() = Vec::new();
            *folder.lock().unwrap() = String::new();
            *groups.lock().unwrap() = BTreeMap::new();
            *idx.lock().unwrap() = -1;
            *out_path.lock().unwrap() = String::new();
            *prefix_arc.lock().unwrap() = String::new();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_selected_pattern(new_pattern);
                u.set_conv_selected_folder(SharedString::from(""));
                u.set_conv_file_groups(ModelRc::new(VecModel::<FileGroupInfo>::from(vec![])));
                u.set_conv_selected_group_idx(-1);
                u.set_conv_num_channels(0);
                u.set_conv_output_folder(SharedString::from(""));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let groups = conv_file_groups.clone();
        let idx = conv_selected_group_idx.clone();
        let cmap = conv_channel_map.clone();
        let out_path = conv_output_path.clone();
        let prefix_arc = conv_filename_prefix.clone();
        let pattern_arc_sel = conv_selected_pattern.clone();
        let audio_suffix = conv_audio_suffix_template.clone();
        let video_suffix = conv_video_suffix_template.clone();
        let split_arc = conv_split_tracks.clone();
        let drop_arc = conv_drop_ltc_track.clone();
        let folder_for_group = conv_folder_for_group.clone();
        let cmd_group = cmd_tx.clone();
        ui.on_conv_select_group(move |group_idx| {
            let g = groups.lock().unwrap();
            let keys: Vec<String> = g.keys().cloned().collect();
            if group_idx >= 0 && (group_idx as usize) < keys.len() {
                let prefix = keys[group_idx as usize].clone();
                let files = g.get(&prefix).cloned().unwrap_or_default();
                let n = files.len();
                let is_video = *pattern_arc_sel.lock().unwrap() == 1 && n == 1;
                *cmap.lock().unwrap() = ChannelMap::identity(n);
                *idx.lock().unwrap() = group_idx as isize;
                *prefix_arc.lock().unwrap() = prefix.clone();
                *out_path.lock().unwrap() = String::new();
                *split_arc.lock().unwrap() = false;
                *drop_arc.lock().unwrap() = false;
                let folder = folder_for_group.lock().unwrap().clone();
                let ltc_file_names: Vec<SharedString> = files.iter().map(|f| {
                    SharedString::from(f.file_name().and_then(|s| s.to_str()).unwrap_or("?"))
                }).collect();
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_selected_group_idx(group_idx);
                    u.set_conv_num_channels(n as i32);
                    u.set_conv_is_video_recording(is_video);
                    u.set_ltc_selected_stream(0);
                    u.set_ltc_selected_channel(0);
                    u.set_ltc_channel_names(ModelRc::new(VecModel::<SharedString>::from(Vec::new())));
                    let map_vec: Vec<i32> = (0..n as i32).collect();
                    u.set_conv_channel_map(ModelRc::new(VecModel::from(map_vec)));
                    u.set_conv_output_folder(SharedString::from(folder.clone()));
                    u.set_conv_filename_prefix(SharedString::from(prefix.clone()));
                    u.set_conv_audio_suffix_template(SharedString::from(audio_suffix.lock().unwrap().clone()));
                    u.set_conv_video_suffix_template(SharedString::from(video_suffix.lock().unwrap().clone()));
                    u.set_ltc_file_idx(0);
                    u.set_ltc_file_names(ModelRc::new(VecModel::<SharedString>::from(ltc_file_names)));
                }

                // Auto-probe video files for audio streams
                if is_video && !files.is_empty() {
                    let folder_str = folder_for_group.lock().unwrap().clone();
                    if !folder_str.is_empty() {
                        let full_path = PathBuf::from(&folder_str).join(&files[0]);
                        let _ = cmd_group.send(GuiCommand::ProbeVideo(
                            full_path.to_string_lossy().to_string(),
                        ));
                    }
                }
            }
        });
    }
    {
        let cmap = conv_channel_map.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_map_cell_clicked(move |row, col| {
            let mut map = cmap.lock().unwrap();
            map.swap(row as usize, col as usize);
            let n = map.num_channels();
            let vec: Vec<i32> = (0..n).map(|i| map.get(i) as i32).collect();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_channel_map(ModelRc::new(VecModel::from(vec)));
            }
        });
    }
    {
        let state = conv_state.clone();
        let cancel = conv_cancel.clone();
        let handle = conv_handle.clone();
        let folder = conv_selected_folder.clone();
        let groups_data = conv_file_groups.clone();
        let idx = conv_selected_group_idx.clone();
        let cmap = conv_channel_map.clone();
        let container = conv_container.clone();
        let venc = conv_video_encoder.clone();
        let aenc = conv_audio_encoder.clone();
        let name_prefix_arc = conv_filename_prefix.clone();
        let trim_flag = conv_trim_to_first_ltc.clone();
        let trim_offset = conv_trim_offset_secs.clone();
        let eng_state = engine_state.clone();
        let gen_synth = conv_generate_synthetic_video.clone();
        let split_arc2 = conv_split_tracks.clone();
        let drop_arc2 = conv_drop_ltc_track.clone();
        let audio_suffix_arc = conv_audio_suffix_template.clone();
        let video_suffix_arc = conv_video_suffix_template.clone();
        let pattern_arc2 = conv_selected_pattern.clone();
        ui.on_conv_start(move || {
            let g = groups_data.lock().unwrap();
            let i = *idx.lock().unwrap();
            let keys: Vec<String> = g.keys().cloned().collect();
            if i < 0 || (i as usize) >= keys.len() { return; }
            let prefix = &keys[i as usize];
            let files = g.get(prefix).cloned().unwrap_or_default();
            let folder_path = folder.lock().unwrap().clone();
            let input_files: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(&folder_path).join(f)).collect();
            let num_files = input_files.len();
            let map = cmap.lock().unwrap().clone();
            let trim_flag_val = *trim_flag.lock().unwrap();
            let trim_secs = if trim_flag_val { *trim_offset.lock().unwrap() } else { 0.0 };
            let filename_prefix = name_prefix_arc.lock().unwrap().clone();
            let trim_offsets_secs: Vec<f64> = if trim_flag_val && trim_secs > 0.001 {
                vec![trim_secs; num_files]
            } else {
                vec![0.0; num_files]
            };
            let ltc_result = eng_state.load().ltc_decode_result.clone();
            let timecode_meta_per_file: Vec<Option<TimecodeMetadata>> = if trim_flag_val && trim_secs > 0.001 {
                (0..num_files).map(|_| {
                    ltc_result.as_ref().and_then(|r| {
                        use gui_engine::LtcDecodeStatus;
                        if !matches!(r.status, LtcDecodeStatus::Success | LtcDecodeStatus::LowConfidence) {
                            return None;
                        }
                        find_timecode_at_offset(&r.timecodes, trim_secs).map(|tc| TimecodeMetadata {
                            start: tc,
                            fps: r.detected_fps as f64,
                            drop_frame: r.drop_frame,
                        })
                    })
                }).collect()
            } else {
                vec![None; num_files]
            };
            let generate_video = *gen_synth.lock().unwrap();
            let split_val = *split_arc2.lock().unwrap();
            let drop_val = *drop_arc2.lock().unwrap();
            let audio_suffix_val = audio_suffix_arc.lock().unwrap().clone();
            let video_suffix_val = video_suffix_arc.lock().unwrap().clone();
            let ltc_idx = 1;
            let rec_type = if *pattern_arc2.lock().unwrap() == 1 && num_files == 1 {
                RecordingType::VideoClipSequence
            } else {
                RecordingType::MultiTrackAudio
            };
            let pipeline = match rec_type {
                RecordingType::MultiTrackAudio => ConversionPipeline::AudioOnly { generate_synthetic_video: generate_video },
                RecordingType::VideoClipSequence => ConversionPipeline::VideoPassthrough,
            };
            let ltc_video_source = match rec_type {
                RecordingType::VideoClipSequence => {
                    let s = eng_state.load();
                    Some((s.ltc_selected_stream, s.ltc_selected_channel))
                }
                RecordingType::MultiTrackAudio => None,
            };
            let settings = ConverterSettings {
                pipeline,
                input_files,
                recording_type: rec_type,
                ltc_track_channel_index: ltc_idx,
                channel_map: map,
                split_tracks: split_val,
                drop_ltc_track: drop_val,
                ltc_video_source,
                container: container.lock().unwrap().clone(),
                video_encoder: venc.lock().unwrap().clone(),
                audio_encoder: aenc.lock().unwrap().clone(),
                output_folder: PathBuf::from(&folder_path),
                filename_prefix,
                audio_suffix_template: audio_suffix_val,
                video_suffix_template: video_suffix_val,
                trim_to_first_ltc: trim_flag_val,
                trim_offsets_secs,
                timecode_meta_per_file,
            };
            *state.lock().unwrap() = ConversionState::idle();
            cancel.store(false, Ordering::Relaxed);
            let cs = state.clone();
            let cf = cancel.clone();
            let h = spawn_conversion(settings, cs, cf);
            *handle.lock().unwrap() = Some(h);
        });
    }
    {
        let cancel = conv_cancel.clone();
        ui.on_conv_cancel(move || {
            cancel.store(true, Ordering::Relaxed);
        });
    }
    {
        let trim_flag = conv_trim_to_first_ltc.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_trim_ltc(move || {
            let mut f = trim_flag.lock().unwrap();
            *f = !*f;
            if let Some(u) = ui_weak.upgrade() {
                u.set_trim_to_first_ltc(*f);
            }
        });
    }
    {
        let state = conv_state.clone();
        ui.on_conv_copy_log(move || {
            let s = state.lock().unwrap();
            let text = s.ffmpeg_output.clone();
            if let Ok(mut ctx) = arboard::Clipboard::new() {
                let _ = ctx.set_text(text);
            }
        });
    }
    {
        let state = conv_state.clone();
        let cmap = conv_channel_map.clone();
        let groups = conv_file_groups.clone();
        let idx = conv_selected_group_idx.clone();
        let out_path = conv_output_path.clone();
        let prefix_arc = conv_filename_prefix.clone();
        let split_arc3 = conv_split_tracks.clone();
        let drop_arc3 = conv_drop_ltc_track.clone();
        let gen_arc2 = conv_generate_synthetic_video.clone();
        let audio_suffix_arc2 = conv_audio_suffix_template.clone();
        let video_suffix_arc2 = conv_video_suffix_template.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_reset(move || {
            *state.lock().unwrap() = ConversionState::idle();
            *cmap.lock().unwrap() = ChannelMap::identity(0);
            *groups.lock().unwrap() = BTreeMap::new();
            *idx.lock().unwrap() = -1;
            *out_path.lock().unwrap() = String::new();
            *prefix_arc.lock().unwrap() = String::new();
            *split_arc3.lock().unwrap() = false;
            *drop_arc3.lock().unwrap() = false;
            *gen_arc2.lock().unwrap() = false;
            *audio_suffix_arc2.lock().unwrap() = DEFAULT_AUDIO_SUFFIX.to_string();
            *video_suffix_arc2.lock().unwrap() = DEFAULT_VIDEO_SUFFIX.to_string();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_status(SharedString::from("idle"));
                u.set_conv_progress(0.0);
                u.set_conv_log(SharedString::from(""));
                u.set_conv_selected_group_idx(-1);
                u.set_conv_num_channels(0);
                u.set_conv_channel_map(ModelRc::new(VecModel::<i32>::from(vec![])));
                u.set_conv_output_folder(SharedString::from(""));
                u.set_conv_filename_prefix(SharedString::from(""));
                u.set_conv_split_tracks(false);
                u.set_conv_drop_ltc_track(false);
                u.set_conv_generate_synthetic_video(false);
                u.set_conv_audio_suffix_template(SharedString::from(DEFAULT_AUDIO_SUFFIX));
                u.set_conv_video_suffix_template(SharedString::from(DEFAULT_VIDEO_SUFFIX));
                u.set_conv_is_video_recording(false);
                u.set_conv_sanity_msg(SharedString::from(""));
            }
        });
    }

    {
        let container = conv_container.clone();
        let ui_weak = ui.as_weak();
        let caps_arc = conv_ffmpeg_caps.clone();
        let venc_arc = conv_video_encoder.clone();
        let aenc_arc = conv_audio_encoder.clone();
        ui.on_conv_container_selected(move |idx| {
            let caps = caps_arc.lock().unwrap().clone();
            let options: Vec<(&str, &str)> = match caps {
                Some(ref c) if c.has_ffmpeg => available_containers(c),
                _ => gui_engine::converter::supported_containers(),
            };
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                *container.lock().unwrap() = key.clone();
                // Re-filter encoders for the new container
                if let Some(ref c) = caps {
                    if c.has_ffmpeg {
                        let vids: Vec<(&str, &str)> = available_video_encoders_for_container(&key, c);
                        let auds: Vec<(&str, &str)> = available_audio_encoders_for_container(&key, c);
                        if !vids.is_empty() {
                            *venc_arc.lock().unwrap() = vids[0].0.to_string();
                        }
                        if !auds.is_empty() {
                            *aenc_arc.lock().unwrap() = auds[0].0.to_string();
                        }
                    }
                }
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_container(SharedString::from(key));
                    u.set_conv_video_encoder(SharedString::from(venc_arc.lock().unwrap().clone()));
                    u.set_conv_audio_encoder(SharedString::from(aenc_arc.lock().unwrap().clone()));
                }
            }
        });
    }
    {
        let venc = conv_video_encoder.clone();
        let ui_weak = ui.as_weak();
        let caps_for_video = conv_ffmpeg_caps.clone();
        let container_for_video = conv_container.clone();
        ui.on_conv_video_selected(move |idx| {
            let caps = caps_for_video.lock().unwrap().clone();
            let container = container_for_video.lock().unwrap().clone();
            let options: Vec<&str> = match caps {
                Some(ref c) if c.has_ffmpeg => available_video_encoders_for_container(&container, c)
                    .iter().map(|(k, _)| *k).collect(),
                _ => gui_engine::converter::supported_video_encoders()
                    .iter().map(|(k, _)| *k).collect(),
            };
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].to_string();
                *venc.lock().unwrap() = key.clone();
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_video_encoder(SharedString::from(key));
                }
            }
        });
    }
    {
        let aenc = conv_audio_encoder.clone();
        let ui_weak = ui.as_weak();
        let caps_for_audio = conv_ffmpeg_caps.clone();
        let container_for_audio = conv_container.clone();
        ui.on_conv_audio_selected(move |idx| {
            let caps = caps_for_audio.lock().unwrap().clone();
            let container = container_for_audio.lock().unwrap().clone();
            let options: Vec<&str> = match caps {
                Some(ref c) if c.has_ffmpeg => available_audio_encoders_for_container(&container, c)
                    .iter().map(|(k, _)| *k).collect(),
                _ => gui_engine::converter::supported_audio_encoders()
                    .iter().map(|(k, _)| *k).collect(),
            };
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].to_string();
                *aenc.lock().unwrap() = key.clone();
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_audio_encoder(SharedString::from(key));
                }
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let out_folder = conv_folder_for_group.clone();
        ui.on_conv_select_output_folder(move || {
            let mut dialog = rfd::FileDialog::new();
            {
                let cur = out_folder.lock().unwrap();
                if !cur.is_empty() {
                    dialog = dialog.set_directory(cur.as_str());
                }
            }
            if let Some(path) = dialog.pick_folder() {
                let path_str = path.to_string_lossy().to_string();
                *out_folder.lock().unwrap() = path_str.clone();
                config::save_output_folder(&path);
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_output_folder(SharedString::from(path_str));
                }
            }
        });
    }
    {
        let prefix_arc = conv_filename_prefix.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_filename_prefix_changed(move |val| {
            *prefix_arc.lock().unwrap() = val.to_string();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_filename_prefix(val.clone());
            }
        });
    }
    {
        let audio_suffix = conv_audio_suffix_template.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_audio_suffix_changed(move |val| {
            *audio_suffix.lock().unwrap() = val.to_string();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_audio_suffix_template(val);
            }
        });
    }
    {
        let video_suffix = conv_video_suffix_template.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_video_suffix_changed(move |val| {
            *video_suffix.lock().unwrap() = val.to_string();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_video_suffix_template(val);
            }
        });
    }
    {
        let split_arc = conv_split_tracks.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_split_tracks(move || {
            let mut f = split_arc.lock().unwrap();
            *f = !*f;
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_split_tracks(*f);
            }
        });
    }
    {
        let drop_arc = conv_drop_ltc_track.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_drop_ltc_track(move || {
            let mut f = drop_arc.lock().unwrap();
            *f = !*f;
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_drop_ltc_track(*f);
            }
        });
    }
    {
        let gen_arc = conv_generate_synthetic_video.clone();
        let ui_weak = ui.as_weak();
        ui.on_toggle_synthetic_video(move || {
            let mut f = gen_arc.lock().unwrap();
            *f = !*f;
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_generate_synthetic_video(*f);
            }
        });
    }

    // ── LTC detection callback ─────────────────────────────────────────────
    {
        let ui_weak = ui.as_weak();
        let cmd = cmd_tx.clone();
        let groups = conv_file_groups.clone();
        let folder = conv_selected_folder.clone();
        let sel_idx = conv_selected_group_idx.clone();
        let detect_engine_state = engine_state.clone();
        ui.on_ltc_detect(move || {
            let g = groups.lock().unwrap();
            let f = folder.lock().unwrap();
            let idx = *sel_idx.lock().unwrap();
            if idx < 0 { return; }
            let keys: Vec<String> = g.keys().cloned().collect();
            if (idx as usize) >= keys.len() { return; }
            let prefix = &keys[idx as usize];
            let files = g.get(prefix).cloned().unwrap_or_default();
            let ltc_idx = 0;
            if ltc_idx >= files.len() { return; }
            let file_name = files[ltc_idx].file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let full_path = PathBuf::from(f.as_str()).join(&file_name);
            if let Some(u) = ui_weak.upgrade() {
                u.set_ltc_status(SharedString::from("detecting"));
                u.set_ltc_result_text(SharedString::from(""));
                u.set_ltc_error(SharedString::from(""));
            }
            let is_video = gui_engine::ffprobe::path_is_video(&full_path);
            if is_video {
                // Read probe from state to translate flat channel index to (stream, channel)
                let s = detect_engine_state.load();
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
                let _ = cmd.send(GuiCommand::ParseLtcVideo(
                    full_path.to_string_lossy().to_string(),
                    stream_idx,
                    channel_idx,
                ));
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
                        "  Missing:       {} frames, {} gap(s), {} glitch(es), {} edit point(s)\n",
                        q.missing_frames, q.gap_count, q.glitch_count, q.edit_count
                    ));
                    if q.max_drift_secs > 0.01 {
                        report.push_str(&format!("  Max drift:    {:.3}s\n", q.max_drift_secs));
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

    // ── Poll timer — state sync ────────────────────────────────────────────
    setup_poll_timer(
        &ui,
        engine_state,
        toasts,
        next_toast_id,
        log_buffer,
        last_debug_log_count,
        pulse_phase,
        conv_state,
        conv_ffmpeg_caps,
        conv_sanity_msg,
        conv_filename_prefix,
        conv_file_groups,
        conv_selected_group_idx,
        conv_container,
        conv_video_encoder,
        conv_audio_encoder,
        conv_selected_folder,
        conv_trim_offset_secs,
        conv_trim_to_first_ltc,
        conv_split_tracks.clone(),
        conv_drop_ltc_track.clone(),
    );

    info!("LTC Slint GUI initialized, showing window");
    ui.run()?;

    info!("LTC Slint GUI shutting down");
    Ok(())
}