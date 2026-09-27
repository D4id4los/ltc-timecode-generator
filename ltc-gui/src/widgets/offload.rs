use std::path::PathBuf;

use egui::{FontId, RichText, Ui, Color32};
use gui_engine::command::{
    GuiCommand,
    OffloadCommand,
};
use gui_engine::offload::OffloadDeviceState;

use crate::app::AppState;
use crate::theme::{ThemeColors, ACCENT};

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let s = &state.latest;

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

    // Editable parent name (ISO date)
    let mut name = s.offload.parent_name.clone();
    ui.horizontal(|ui| {
        ui.label(RichText::new("Subfolder: ").font(FontId::monospace(12.0)).color(colors.text_muted));
        let resp = ui.add(
            egui::TextEdit::singleline(&mut name)
                .font(FontId::monospace(14.0))
                .desired_width(160.0)
        );
        if resp.changed() && !name.is_empty() {
            state.send(GuiCommand::Offload(OffloadCommand::SetParentName(name)));
        }
        ui.label(RichText::new("(date subfolder)").font(FontId::monospace(10.0)).color(colors.text_muted));
    });
}

fn render_cards(ui: &mut Ui, state: &mut AppState) {
    let s = &state.latest;
    let colors = state.theme.colors();
    let off = &s.offload;
    let cards = off.cards.clone();
    let scanning = off.scanning;

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

                // Editable device name.
                let mut dev_name = card.device_name.clone();
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Device folder: ")
                        .font(FontId::monospace(11.0))
                        .color(colors.text_muted));
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut dev_name)
                            .font(FontId::monospace(14.0))
                            .desired_width(140.0)
                    );
                    if resp.changed() {
                        state.send(GuiCommand::Offload(OffloadCommand::SetDeviceName(idx, dev_name)));
                    }
                });

                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{} file(s), {}", card.media_file_count, format_bytes(card.total_bytes)))
                        .font(FontId::monospace(11.0))
                        .color(device_color));
                });
            });
        ui.add_space(4.0);
    }
}

fn render_offload_actions(ui: &mut Ui, state: &mut AppState) {
    let s = &state.latest;
    let colors = state.theme.colors();
    let off = &s.offload;
    let has_cards = !off.cards.is_empty();
    let has_parent = off.parent_folder.is_some();
    let can_start = has_cards && has_parent && !off.running && !off.scanning;

    ui.horizontal(|ui| {
        if off.running {
            if ui.button(RichText::new("■ Cancel").font(FontId::proportional(13.0)).color(colors.error_red)).clicked() {
                state.send(GuiCommand::Offload(OffloadCommand::CancelOffload));
            }
        } else if can_start {
            if ui.button(RichText::new("Start Offload").font(FontId::proportional(13.0)).color(ACCENT)).clicked() {
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
        ui.add_space(4.0);

        // Per-device progress.
        for dev in &off.device_progress {
            let dev_pct = if dev.files_total > 0 {
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
                    ui.label(RichText::new(format!("{}/{} files", dev.files_done, dev.files_total))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_muted));
                });
            });
            if dev.files_total > 0 {
                ui.add(egui::ProgressBar::new(dev_pct).desired_width(ui.available_width()));
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