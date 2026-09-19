slint::include_modules!();

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use gui_engine::command::GuiCommand;
use gui_engine::converter::{
    query_ffmpeg_capabilities, spawn_conversion, ChannelMap, ConversionState,
    FfmpegCapabilities,
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
    let conv_state: Arc<Mutex<ConversionState>> = Arc::new(Mutex::new(ConversionState::idle()));
    let conv_cancel: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    let conv_handle: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> = Arc::new(Mutex::new(None));
    let conv_ffmpeg_caps: Arc<Mutex<Option<FfmpegCapabilities>>> = Arc::new(Mutex::new(None));
    let conv_sanity_msg: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    let conv_trim_to_first_ltc: Arc<Mutex<bool>> = Arc::new(Mutex::new(false));
    let conv_trim_offset_secs: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));

    let conv_folder_for_group = conv_selected_folder.clone();
    let conv_ffmpeg_caps_for_select = conv_ffmpeg_caps.clone();

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
        ui.on_conv_select_folder(move || {
            let pat = *pattern_arc.lock().unwrap();
            if pat == 0 {
                // TASCAM: folder picker
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    let path_str = path.to_string_lossy().to_string();
                    *folder.lock().unwrap() = path_str.clone();
                    *files_arc.lock().unwrap() = Vec::new();
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
                if let Some(paths) = rfd::FileDialog::new()
                    .add_filter("Audio", &["*"])
                    .pick_files()
                {
                    if !paths.is_empty() {
                        let parent = paths[0].parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_default();
                        *folder.lock().unwrap() = parent.clone();
                        *files_arc.lock().unwrap() = paths.clone();
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
        let ui_weak = ui.as_weak();
        ui.on_conv_select_pattern(move |new_pattern| {
            *pattern_arc.lock().unwrap() = new_pattern;
            *files_arc.lock().unwrap() = Vec::new();
            *folder.lock().unwrap() = String::new();
            *groups.lock().unwrap() = BTreeMap::new();
            *idx.lock().unwrap() = -1;
            *out_path.lock().unwrap() = String::new();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_selected_pattern(new_pattern);
                u.set_conv_selected_folder(SharedString::from(""));
                u.set_conv_file_groups(ModelRc::new(VecModel::<FileGroupInfo>::from(vec![])));
                u.set_conv_selected_group_idx(-1);
                u.set_conv_num_channels(0);
                u.set_conv_output_path(SharedString::from(""));
            }
        });
    }
    {
        let ui_weak = ui.as_weak();
        let groups = conv_file_groups.clone();
        let idx = conv_selected_group_idx.clone();
        let cmap = conv_channel_map.clone();
        let out_path = conv_output_path.clone();
        let container = conv_container.clone();
        ui.on_conv_select_group(move |group_idx| {
            let g = groups.lock().unwrap();
            let keys: Vec<String> = g.keys().cloned().collect();
            if group_idx >= 0 && (group_idx as usize) < keys.len() {
                let prefix = keys[group_idx as usize].clone();
                let files = g.get(&prefix).cloned().unwrap_or_default();
                let n = files.len();
                *cmap.lock().unwrap() = ChannelMap::identity(n);
                *idx.lock().unwrap() = group_idx as isize;
                let folder = conv_folder_for_group.lock().unwrap().clone();
                let container_str = container.lock().unwrap().clone();
                let default_name = format!("{}-multi-audio-vid.{}", prefix, container_str);
                let full_path = if folder.is_empty() {
                    default_name
                } else {
                    format!("{}/{}", folder, default_name)
                };
                *out_path.lock().unwrap() = full_path.clone();
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_selected_group_idx(group_idx);
                    u.set_conv_num_channels(n as i32);
                    let map_vec: Vec<i32> = (0..n as i32).collect();
                    u.set_conv_channel_map(ModelRc::new(VecModel::from(map_vec)));
                    u.set_conv_output_path(SharedString::from(full_path));
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
        let out_path = conv_output_path.clone();
        let trim_flag = conv_trim_to_first_ltc.clone();
        let trim_offset = conv_trim_offset_secs.clone();
        ui.on_conv_start(move || {
            let g = groups_data.lock().unwrap();
            let i = *idx.lock().unwrap();
            let keys: Vec<String> = g.keys().cloned().collect();
            if i < 0 || (i as usize) >= keys.len() { return; }
            let prefix = &keys[i as usize];
            let files = g.get(prefix).cloned().unwrap_or_default();
            let folder_path = folder.lock().unwrap().clone();
            let input_files: Vec<PathBuf> = files.iter().map(|f| PathBuf::from(&folder_path).join(f)).collect();
            let map = cmap.lock().unwrap().clone();
            let output = PathBuf::from(out_path.lock().unwrap().clone());
            let settings = gui_engine::converter::ConverterSettings {
                input_files,
                channel_map: map,
                container: container.lock().unwrap().clone(),
                video_encoder: venc.lock().unwrap().clone(),
                audio_encoder: aenc.lock().unwrap().clone(),
                output_path: output,
                trim_start_secs: if *trim_flag.lock().unwrap() { *trim_offset.lock().unwrap() } else { 0.0 },
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
        let ui_weak = ui.as_weak();
        ui.on_conv_reset(move || {
            *state.lock().unwrap() = ConversionState::idle();
            *cmap.lock().unwrap() = ChannelMap::identity(0);
            *groups.lock().unwrap() = BTreeMap::new();
            *idx.lock().unwrap() = -1;
            *out_path.lock().unwrap() = String::new();
            if let Some(u) = ui_weak.upgrade() {
                u.set_conv_status(SharedString::from("idle"));
                u.set_conv_progress(0.0);
                u.set_conv_log(SharedString::from(""));
                u.set_conv_selected_group_idx(-1);
                u.set_conv_num_channels(0);
                u.set_conv_channel_map(ModelRc::new(VecModel::<i32>::from(vec![])));
                u.set_conv_output_path(SharedString::from(""));
                u.set_conv_sanity_msg(SharedString::from(""));
            }
        });
    }

    {
        let container = conv_container.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_container_selected(move |idx| {
            let options = gui_engine::converter::supported_containers();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                *container.lock().unwrap() = key.clone();
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_container(SharedString::from(key));
                }
            }
        });
    }
    {
        let venc = conv_video_encoder.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_video_selected(move |idx| {
            let options = gui_engine::converter::supported_video_encoders();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
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
        ui.on_conv_audio_selected(move |idx| {
            let options = gui_engine::converter::supported_audio_encoders();
            if idx >= 0 && (idx as usize) < options.len() {
                let key = options[idx as usize].0.to_string();
                *aenc.lock().unwrap() = key.clone();
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_audio_encoder(SharedString::from(key));
                }
            }
        });
    }
    {
        let out_path = conv_output_path.clone();
        let ui_weak = ui.as_weak();
        ui.on_conv_select_output_path(move || {
            if let Some(path) = rfd::FileDialog::new()
                .set_file_name("output.mkv")
                .save_file()
            {
                let path_str = path.to_string_lossy().to_string();
                *out_path.lock().unwrap() = path_str.clone();
                if let Some(u) = ui_weak.upgrade() {
                    u.set_conv_output_path(SharedString::from(path_str));
                }
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
        ui.on_ltc_detect(move || {
            let g = groups.lock().unwrap();
            let f = folder.lock().unwrap();
            let idx = *sel_idx.lock().unwrap();
            if idx < 0 { return; }
            let keys: Vec<String> = g.keys().cloned().collect();
            if (idx as usize) >= keys.len() { return; }
            let prefix = &keys[idx as usize];
            let files = g.get(prefix).cloned().unwrap_or_default();
            let ltc_idx = 0; // Use first file as default
            if ltc_idx >= files.len() { return; }
            // files contains filenames only; the callback in Slint was set up
            // with conv_file_groups = BTreeMap<String, Vec<PathBuf>> where values
            // are just filenames, so we join with folder to get full path
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
            let _ = cmd.send(GuiCommand::ParseLtcFile(
                full_path.to_string_lossy().to_string(),
            ));
        });
    }

    // ── Decode FPS selection callback ───────────────────────────────────────
    {
        let cmd = cmd_tx.clone();
        ui.on_decode_fps_selected(move |index| {
            let _ = cmd.send(GuiCommand::SetDecodeFpsIndex(index as usize));
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
        conv_output_path,
        conv_file_groups,
        conv_selected_group_idx,
        conv_container,
        conv_video_encoder,
        conv_audio_encoder,
        conv_selected_folder,
        conv_trim_offset_secs,
        conv_trim_to_first_ltc,
    );

    info!("LTC Slint GUI initialized, showing window");
    ui.run()?;

    info!("LTC Slint GUI shutting down");
    Ok(())
}