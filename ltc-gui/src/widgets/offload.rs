use egui::{FontId, RichText, Ui, Color32, Vec2};
use gui_engine::command::{
    GuiCommand,
    OffloadCommand,
};
use gui_engine::duration::format_duration_secs;
use gui_engine::offload::OffloadDeviceState;

use crate::app::AppState;
use crate::theme::{ThemeColors, ACCENT};

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let _s = &state.latest;

    let frame = egui::Frame::group(ui.style())
        .inner_margin(egui::Margin::symmetric(16, 12));
    frame.show(ui, |ui| {
        ui.vertical(|ui| {
            step_header(ui, "1", "SELECT PARENT FOLDER", &colors);

            render_parent_selection(ui, state);

            ui.add_space(12.0);
            step_header(ui, "2", "DETECTED MEDIA CARDS", &colors);

            render_cards(ui, state);

            ui.add_space(12.0);
            step_header(ui, "3", "COPY FILES", &colors);

            render_offload_actions(ui, state);
            render_progress(ui, state);
        });
    });
}

fn step_header(ui: &mut Ui, _number: &str, label: &str, colors: &ThemeColors) {
    ui.horizontal(|ui| {
        let label_rich = RichText::new(label)
            .font(FontId::proportional(14.0))
            .color(colors.text_main);
        ui.label(label_rich);
    });
    ui.add_space(4.0);
}

fn render_parent_selection(ui: &mut Ui, state: &mut AppState) {
    let s = &state.latest;
    let colors = state.theme.colors();
    let folder = s.offload.parent_folder.clone();
    let folder_label = folder
        .as_ref()
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_string();
    let display = folder_label;

    ui.horizontal(|ui| {
        ui.label(RichText::new("Parent:  ").font(FontId::monospace(12.0)).color(colors.text_muted));
        // Show current path + Browse button
        let label = if display.is_empty() {
            "Select Folder…".to_string()
        } else {
            display.clone()
        };
        if ui.button("Browse…").clicked() {
            let mut dialog = rfd::FileDialog::new();
            if !display.is_empty() {
                dialog = dialog.set_directory(&display);
            }
            if let Some(path) = dialog.pick_folder() {
                state.send(GuiCommand::Offload(OffloadCommand::SetParentFolder(path)));
            }
        }
        ui.label(RichText::new(label).font(FontId::monospace(11.0)).color(colors.text_main));
    });

    ui.add_space(4.0);

    // Editable parent name (ISO date) — persistent buffer via parent_name_edit.
    // Use a local copy inside the horizontal closure to avoid borrow conflicts;
    // write back through state.parent_name_edit after the closure exits.
    let mut name = state.parent_name_edit.buffer().to_string();
    let mut changed = false;
    let mut focused = false;
    ui.horizontal(|ui| {
        ui.label(RichText::new("Subfolder: ").font(FontId::monospace(12.0)).color(colors.text_muted));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut name)
                .font(FontId::monospace(14.0))
                .desired_width(160.0)
        );
        focused = resp.has_focus();
        changed = resp.changed() && !name.is_empty();
        ui.label(RichText::new("(date subfolder)").font(FontId::monospace(10.0)).color(colors.text_muted));
    });
    // Write back the local copy to the persistent buffer BEFORE mark_edited,
    // so that mark_edited captures the latest value as the pending confirmation.
    let parent_value = name.clone();
    *state.parent_name_edit.buffer_mut() = name;
    if changed {
        state.parent_name_edit.mark_edited(std::time::Instant::now());
    }
    state.parent_name_edit.set_focused(focused);
    if changed {
        state.send(GuiCommand::Offload(OffloadCommand::SetParentName(parent_value)));
    }
}

fn render_cards(ui: &mut Ui, state: &mut AppState) {
    let s = &state.latest;
    let colors = state.theme.colors();
    let off = &s.offload;
    let cards = off.cards.clone();
    let scanning = off.scanning;
    let file_durations = off.file_durations.clone();

    ui.horizontal(|ui| {
        if ui.button("Rescan").clicked() {
            state.send(GuiCommand::Offload(OffloadCommand::ScanCards));
        }
        if scanning {
            ui.add(egui::Spinner::new());
            ui.label("Scanning…");
        }
    });

    ui.add_space(4.0);

    if cards.is_empty() && !scanning {
        ui.label(RichText::new("No removable media detected. Insert an SD card and click Rescan.")
            .font(FontId::monospace(11.0))
            .color(colors.text_muted));
    }

    for (idx, card) in cards.iter().enumerate() {
        let device_color = card_color(&card.name_source, &colors);

        ui.push_id(&card.mount, |ui| {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("💾 {}", card.mount.display()))
                        .font(FontId::monospace(11.0))
                        .color(colors.text_muted));
                    if !card.volume_label.is_empty() && card.volume_label != card.device_name {
                        ui.label(RichText::new(format!("({})", card.volume_label))
                            .font(FontId::monospace(10.0))
                            .color(colors.text_muted));
                    }
                });

                ui.add_space(2.0);

                // Editable device name — persistent buffers in device_name_edits keyed by mount
                let mount = card.mount.clone();
                let copy = state.device_name_edits.get(&mount)
                    .map(|e| e.buffer().to_string())
                    .unwrap_or_else(|| card.device_name.clone());
                let mut dev_name = copy;
                let mut dev_changed = false;
                let mut dev_focused = false;
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Device folder: ")
                        .font(FontId::monospace(11.0))
                        .color(colors.text_muted));
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut dev_name)
                            .font(FontId::monospace(14.0))
                            .desired_width(140.0)
                    );
                    dev_focused = resp.has_focus();
                    dev_changed = resp.changed();
                });
                let dev_value = dev_name.clone();
                if let Some(edit) = state.device_name_edits.get_mut(&mount) {
                    *edit.buffer_mut() = dev_name;
                    if dev_changed {
                        edit.mark_edited(std::time::Instant::now());
                    }
                    edit.set_focused(dev_focused);
                }
                if dev_changed {
                    state.send(GuiCommand::Offload(OffloadCommand::SetDeviceName(idx, dev_value)));
                }

                // File count summary (now with selection info).
                ui.horizontal(|ui| {
                    let summary = format!(
                        "{}/{} file(s) selected · {}",
                        card.selected_count,
                        card.media_file_count,
                        format_bytes(card.selected_bytes),
                    );
                    ui.label(RichText::new(summary)
                        .font(FontId::monospace(11.0))
                        .color(device_color));
                });

                // ── File selection list ──
                if !card.files.is_empty() {
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        let btn_size = Vec2::new(40.0, 18.0);
                        if ui.add_sized(btn_size, egui::Button::new("All")).clicked() {
                            state.send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(idx, true)));
                        }
                        if ui.add_sized(btn_size, egui::Button::new("None")).clicked() {
                            state.send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(idx, false)));
                        }
                        if ui.add_sized(egui::vec2(60.0, 18.0), egui::Button::new("Latest day")).clicked() {
                            state.send(GuiCommand::Offload(OffloadCommand::SelectLatestDay(idx)));
                        }
                    });

                    // Column headers
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(
                            RichText::new("File").font(FontId::monospace(10.0)).color(colors.text_muted),
                        ));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add(egui::Label::new(
                                RichText::new("Size").font(FontId::monospace(10.0)).color(colors.text_muted),
                            ));
                            ui.add(egui::Label::new(
                                RichText::new("Length  ").font(FontId::monospace(10.0)).color(colors.text_muted),
                            ));
                            ui.add(egui::Label::new(
                                RichText::new("Date           ").font(FontId::monospace(10.0)).color(colors.text_muted),
                            ));
                        });
                    });

                    let row_height = 20.0;
                    let total = card.files.len();
                    egui::ScrollArea::vertical()
                        .id_salt(crate::ids::offload_card_files_scroll(&card.mount))
                        .max_height(240.0)
                        .auto_shrink([false; 2])
                        .show_rows(ui, row_height, total, |ui, range| {
                            for i in range {
                                let file = &card.files[i];
                                let selected = card.selected.get(i).copied().unwrap_or(false);
                                let mut checked = selected;
                                ui.horizontal(|ui| {
                                    ui.set_min_height(row_height);
                                    ui.set_height(row_height);
                                    let resp = ui.checkbox(&mut checked, "");
                                    if resp.changed() {
                                        state.send(GuiCommand::Offload(
                                            OffloadCommand::SetFileSelected(idx, i, checked),
                                        ));
                                    }

                                    // Filename
                                    ui.label(RichText::new(&file.name)
                                        .font(FontId::monospace(11.0))
                                        .color(colors.text_main));

                                    // Date, duration, size — right-aligned
                                    let date_str = file.modified
                                        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                                        .unwrap_or_default();
                                    let dur_str = file_durations.get(&file.path)
                                        .and_then(|opt| *opt)
                                        .map(format_duration_secs)
                                        .unwrap_or_else(|| {
                                            if file_durations.contains_key(&file.path) {
                                                "—".to_string()
                                            } else {
                                                "…".to_string()
                                            }
                                        });
                                    let size_str = format_bytes(file.size_bytes);

                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.label(RichText::new(size_str)
                                            .font(FontId::monospace(10.0))
                                            .color(colors.text_muted));
                                        ui.label(RichText::new(format!("  {}", dur_str))
                                            .font(FontId::monospace(10.0))
                                            .color(colors.text_muted));
                                        ui.label(RichText::new(format!("  {}", date_str))
                                            .font(FontId::monospace(10.0))
                                            .color(colors.text_muted));
                                    });
                                });
                            }
                        });
                }
            });
        });
        ui.add_space(4.0);
    }
}

fn render_offload_actions(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_running = state.latest.offload.running;
    let can_start = !state.latest.offload.cards.is_empty()
        && state.latest.offload.parent_folder.is_some()
        && !is_running
        && !state.latest.offload.scanning;

    ui.horizontal(|ui| {
        if is_running {
            if ui.button(RichText::new("■ Cancel").font(FontId::proportional(13.0)).color(colors.error_red)).clicked() {
                state.send(GuiCommand::Offload(OffloadCommand::CancelOffload));
            }
        } else if can_start {
            if ui.button(RichText::new("Start Offload").font(FontId::proportional(13.0)).color(ACCENT)).clicked() {
                state.mark_offload_start_pending();
                state.send(GuiCommand::Offload(OffloadCommand::StartOffload));
            }
        } else {
            ui.add_enabled(false, egui::Button::new("Start Offload"));
        }
    });
}

fn render_progress(ui: &mut Ui, state: &mut AppState) {
    let s = &state.latest;
    let colors = state.theme.colors();
    let off = &s.offload;

    // Overall progress bar.
    if off.running || off.overall_progress > 0.0 {
        let progress_f32 = off.overall_progress;
        ui.add(egui::ProgressBar::new(progress_f32)
            .show_percentage()
            .desired_width(ui.available_width()));

        // Speed and total bytes next to the bar.
        let speed_text = if off.speed_bytes_per_sec > 0.0 {
            format!("{} / s", format_bytes(off.speed_bytes_per_sec as u64))
        } else {
            String::new()
        };
        ui.horizontal(|ui| {
            if !speed_text.is_empty() {
                ui.label(RichText::new(&speed_text)
                    .font(FontId::monospace(10.0))
                    .color(ACCENT));
            }
            if off.running {
                // Compute total bytes from per-device totals.
                let total_bytes: u64 = off.device_progress.iter().map(|d| d.bytes_total).sum();
                let done_bytes: u64 = off.device_progress.iter().map(|d| d.bytes_done).sum();
                if total_bytes > 0 {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(RichText::new(format!("{} / {}", format_bytes(done_bytes), format_bytes(total_bytes)))
                            .font(FontId::monospace(10.0))
                            .color(colors.text_muted));
                    });
                }
            }
        });
        ui.add_space(4.0);

        // Per-device progress.
        for dev in &off.device_progress {
            let dev_pct = if dev.bytes_total > 0 {
                (dev.bytes_done as f32) / (dev.bytes_total as f32)
            } else if dev.files_total > 0 {
                dev.files_done as f32 / dev.files_total as f32
            } else {
                0.0
            };
            let (status_icon, status_color) = match &dev.state {
                OffloadDeviceState::Pending => ("⏳", colors.text_muted),
                OffloadDeviceState::Copying => ("▶", ACCENT),
                OffloadDeviceState::Done => ("✅", colors.success_green),
                OffloadDeviceState::Failed(_) => ("❌", colors.error_red),
                OffloadDeviceState::Skipped => ("⏭", colors.text_muted),
            };
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} {}", status_icon, dev.device_name))
                    .font(FontId::monospace(11.0))
                    .color(status_color));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    // Show byte progress + file count.
                    let bytes_str = if dev.bytes_total > 0 {
                        format!("{} / {}  ", format_bytes(dev.bytes_done), format_bytes(dev.bytes_total))
                    } else {
                        String::new()
                    };
                    ui.label(RichText::new(format!("{}{}/{} files", bytes_str, dev.files_done, dev.files_total))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_muted));
                });
            });
            if dev.files_total > 0 || dev.bytes_total > 0 {
                ui.add(egui::ProgressBar::new(dev_pct).desired_width(ui.available_width()));
            }

            // Show current file when copying.
            if let OffloadDeviceState::Copying = &dev.state {
                if !dev.current_file.is_empty() {
                    ui.label(RichText::new(format!("  {}", dev.current_file))
                        .font(FontId::monospace(9.0))
                        .color(colors.text_muted));
                }
            }

            // Show error if failed.
            if let OffloadDeviceState::Failed(ref err) = &dev.state {
                ui.label(RichText::new(err)
                    .font(FontId::monospace(10.0))
                    .color(colors.error_red));
            }
        }
        ui.add_space(8.0);
    }

    // Completed devices.
    if !off.completed_devices.is_empty() {
        ui.add_space(4.0);
        ui.label(RichText::new("Completed:").font(FontId::proportional(12.0)).color(colors.success_green));
        for name in &off.completed_devices {
            ui.label(RichText::new(format!("  ✅ {}", name))
                .font(FontId::monospace(11.0))
                .color(colors.success_green));
        }
    }

    // Error message.
    if let Some(ref err) = off.error {
        ui.label(RichText::new(format!("Error: {}", err))
            .font(FontId::monospace(11.0))
            .color(colors.error_red));
    }
}

fn card_color(source: &gui_engine::offload::DeviceNameSource, colors: &ThemeColors) -> Color32 {
    use gui_engine::offload::DeviceNameSource;
    match source {
        DeviceNameSource::Metadata => colors.success_green,
        DeviceNameSource::Pattern => ACCENT,
        DeviceNameSource::VolumeLabel => colors.text_main,
        DeviceNameSource::Manual | DeviceNameSource::Unknown => colors.text_muted,
    }
}

fn format_bytes(bytes: u64) -> String {
    if bytes >= 1_000_000_000 {
        format!("{:.1} GB", bytes as f64 / 1_000_000_000.0)
    } else if bytes >= 1_000_000 {
        format!("{:.1} MB", bytes as f64 / 1_000_000.0)
    } else if bytes >= 1_000 {
        format!("{:.1} KB", bytes as f64 / 1_000.0)
    } else {
        format!("{} B", bytes)
    }
}