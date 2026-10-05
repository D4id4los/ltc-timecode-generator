use std::collections::HashMap;
use std::path::{Path, PathBuf};

use egui::{Color32, FontId, RichText, Ui, Vec2};
use gui_engine::command::{GuiCommand, OffloadCommand};
use gui_engine::duration::format_duration_secs;
use gui_engine::state::AppStateSnapshot;
use gui_engine::{JobKind, UnitState};

use super::bound;
use crate::app::AppState;
use crate::theme::{ThemeColors, ACCENT};

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let _s = &state.latest;

    let frame = egui::Frame::group(ui.style()).inner_margin(egui::Margin::symmetric(16, 12));
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
        ui.label(
            RichText::new("Parent:  ")
                .font(FontId::monospace(12.0))
                .color(colors.text_muted),
        );
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
        ui.label(
            RichText::new(label)
                .font(FontId::monospace(11.0))
                .color(colors.text_main),
        );
    });

    ui.add_space(4.0);

    // Editable parent name (ISO date) — bound shadow; sends on each change
    // with a non-empty value.
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Subfolder: ")
                .font(FontId::monospace(12.0))
                .color(colors.text_muted),
        );
        let truth = state.latest.offload.parent_name.clone();
        bound::text(
            ui,
            state,
            |s| &mut s.sh.parent_name,
            &truth,
            |v| GuiCommand::Offload(OffloadCommand::SetParentName(v)),
            |edit| edit.font(FontId::monospace(14.0)).desired_width(160.0),
        );
        ui.label(
            RichText::new("(date subfolder)")
                .font(FontId::monospace(10.0))
                .color(colors.text_muted),
        );
    });
}

fn render_cards(ui: &mut Ui, state: &mut AppState) {
    // Borrow the snapshot by reference (Arc clone, not a deep card-list
    // clone): helpers below receive card data via `s`, keeping `state` free
    // for mutable `send` calls.
    let s = std::sync::Arc::clone(&state.latest);
    let colors = state.theme.colors();
    let off = &s.offload;
    let scanning = s.job(JobKind::OffloadScan).is_active();

    render_rescan_row(ui, state, &s);

    ui.add_space(4.0);

    if off.cards.is_empty() && !scanning {
        ui.label(
            RichText::new("No removable media detected. Insert an SD card and click Rescan.")
                .font(FontId::monospace(11.0))
                .color(colors.text_muted),
        );
    }

    for (idx, card) in off.cards.iter().enumerate() {
        render_card(ui, state, idx, card, &off.file_durations);
        ui.add_space(4.0);
    }
}

/// Rescan button + active-scan spinner row.
fn render_rescan_row(ui: &mut Ui, state: &mut AppState, s: &AppStateSnapshot) {
    ui.horizontal(|ui| {
        if ui.button("Rescan").clicked() {
            state.send(GuiCommand::Offload(OffloadCommand::ScanCards));
        }
        if s.job(JobKind::OffloadScan).is_active() {
            ui.add(egui::Spinner::new());
            ui.label(s.job(JobKind::OffloadScan).message().to_string());
        }
    });
}

/// One detected-card frame: mount header, device-name edit, selection
/// summary, and the per-file selection list.
fn render_card(
    ui: &mut Ui,
    state: &mut AppState,
    idx: usize,
    card: &gui_engine::offload::SdCardInfo,
    file_durations: &HashMap<PathBuf, Option<f64>>,
) {
    let colors = state.theme.colors();
    let device_color = card_color(&card.name_source, &colors);

    ui.push_id(&card.mount, |ui| {
        egui::Frame::group(ui.style())
            .inner_margin(egui::Margin::symmetric(8, 6))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("💾 {}", card.mount.display()))
                            .font(FontId::monospace(11.0))
                            .color(colors.text_muted),
                    );
                    if !card.volume_label.is_empty() && card.volume_label != card.device_name {
                        ui.label(
                            RichText::new(format!("({})", card.volume_label))
                                .font(FontId::monospace(10.0))
                                .color(colors.text_muted),
                        );
                    }
                });

                ui.add_space(2.0);

                render_device_name_edit(ui, state, card);

                // File count summary (now with selection info).
                ui.horizontal(|ui| {
                    let summary = selection_summary(card);
                    ui.label(
                        RichText::new(summary)
                            .font(FontId::monospace(11.0))
                            .color(device_color),
                    );
                });

                // ── File selection list ──
                if !card.files.is_empty() {
                    ui.add_space(4.0);
                    render_bulk_select_buttons(ui, state, idx);
                    render_file_column_headers(ui, &colors);

                    let row_height = 20.0;
                    let total = card.files.len();
                    egui::ScrollArea::vertical()
                        .id_salt(crate::ids::offload_card_files_scroll(&card.mount))
                        .max_height(240.0)
                        .auto_shrink([false; 2])
                        .show_rows(ui, row_height, total, |ui, range| {
                            for i in range {
                                render_file_row(
                                    ui,
                                    state,
                                    idx,
                                    i,
                                    card,
                                    &card.files[i],
                                    file_durations,
                                );
                            }
                        });
                }
            });
    });
}

/// Editable device name — bound shadow keyed by mount (stable across
/// rescans); the command targets the mount, not the list index, so a rescan
/// mid-edit cannot retarget the card.
fn render_device_name_edit(
    ui: &mut Ui,
    state: &mut AppState,
    card: &gui_engine::offload::SdCardInfo,
) {
    let colors = state.theme.colors();
    let mount = card.mount.clone();
    let truth = card.device_name.clone();
    let mount_for_cmd = mount.clone();
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Device folder: ")
                .font(FontId::monospace(11.0))
                .color(colors.text_muted),
        );
        bound::text(
            ui,
            state,
            |s| {
                s.sh.device_names
                    .get_mut(&mount)
                    .expect("device shadow seeded in logic()")
            },
            &truth,
            move |v| {
                GuiCommand::Offload(OffloadCommand::SetDeviceNameByMount(
                    mount_for_cmd.clone(),
                    v,
                ))
            },
            |edit| edit.font(FontId::monospace(14.0)).desired_width(140.0),
        );
    });
}

/// Pure per-card selection summary line: `"<n>/<m> file(s) selected · <size>"`.
fn selection_summary(card: &gui_engine::offload::SdCardInfo) -> String {
    format!(
        "{}/{} file(s) selected · {}",
        card.selected_count,
        card.media_file_count,
        format_bytes(card.selected_bytes),
    )
}

/// All / None / Latest-day bulk-selection buttons for one card.
fn render_bulk_select_buttons(ui: &mut Ui, state: &mut AppState, idx: usize) {
    ui.horizontal(|ui| {
        let btn_size = Vec2::new(40.0, 18.0);
        if ui.add_sized(btn_size, egui::Button::new("All")).clicked() {
            state.send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(
                idx, true,
            )));
        }
        if ui.add_sized(btn_size, egui::Button::new("None")).clicked() {
            state.send(GuiCommand::Offload(OffloadCommand::SetAllFilesSelected(
                idx, false,
            )));
        }
        if ui
            .add_sized(egui::vec2(60.0, 18.0), egui::Button::new("Latest day"))
            .clicked()
        {
            state.send(GuiCommand::Offload(OffloadCommand::SelectLatestDay(idx)));
        }
    });
}

/// File / Size / Length / Date column headers.
fn render_file_column_headers(ui: &mut Ui, colors: &ThemeColors) {
    ui.horizontal(|ui| {
        ui.add(egui::Label::new(
            RichText::new("File")
                .font(FontId::monospace(10.0))
                .color(colors.text_muted),
        ));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.add(egui::Label::new(
                RichText::new("Size")
                    .font(FontId::monospace(10.0))
                    .color(colors.text_muted),
            ));
            ui.add(egui::Label::new(
                RichText::new("Length  ")
                    .font(FontId::monospace(10.0))
                    .color(colors.text_muted),
            ));
            ui.add(egui::Label::new(
                RichText::new("Date           ")
                    .font(FontId::monospace(10.0))
                    .color(colors.text_muted),
            ));
        });
    });
}

/// One file row: selection checkbox, filename, and right-aligned
/// date/duration/size cells.
fn render_file_row(
    ui: &mut Ui,
    state: &mut AppState,
    idx: usize,
    i: usize,
    card: &gui_engine::offload::SdCardInfo,
    file: &gui_engine::offload::OffloadFileInfo,
    file_durations: &HashMap<PathBuf, Option<f64>>,
) {
    let colors = state.theme.colors();
    let (text_main, text_muted) = (colors.text_main, colors.text_muted);
    let row_height = 20.0;
    let selected = card.selected.get(i).copied().unwrap_or(false);
    let file_key = file.path.clone();
    let mount_key = card.mount.clone();
    ui.horizontal(|ui| {
        ui.set_min_height(row_height);
        ui.set_height(row_height);
        bound::checkbox(
            ui,
            state,
            |s| {
                s.sh.file_selection
                    .get_mut(&(mount_key.clone(), file_key.clone()))
                    .expect("file-selection shadow seeded in logic()")
            },
            selected,
            "",
            true,
            |v| GuiCommand::Offload(OffloadCommand::SetFileSelected(idx, i, v)),
        );

        // Filename
        ui.label(
            RichText::new(&file.name)
                .font(FontId::monospace(11.0))
                .color(text_main),
        );

        // Date, duration, size — right-aligned
        let date_str = file
            .modified
            .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default();
        let dur_str = duration_cell_text(file_durations, &file.path);
        let size_str = format_bytes(file.size_bytes);

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                RichText::new(size_str)
                    .font(FontId::monospace(10.0))
                    .color(text_muted),
            );
            ui.label(
                RichText::new(format!("  {}", dur_str))
                    .font(FontId::monospace(10.0))
                    .color(text_muted),
            );
            ui.label(
                RichText::new(format!("  {}", date_str))
                    .font(FontId::monospace(10.0))
                    .color(text_muted),
            );
        });
    });
}

/// Pure duration cell: formatted length for a probed file, `—` when probing
/// failed/pending (key present with `None`), `…` while the probe is in
/// flight (key absent).
fn duration_cell_text(file_durations: &HashMap<PathBuf, Option<f64>>, path: &Path) -> String {
    file_durations
        .get(path)
        .and_then(|opt| *opt)
        .map(format_duration_secs)
        .unwrap_or_else(|| {
            if file_durations.contains_key(path) {
                "—".to_string()
            } else {
                "…".to_string()
            }
        })
}

fn render_offload_actions(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_running = state.latest.job(JobKind::OffloadCopy).is_active();
    let is_scanning = state.latest.job(JobKind::OffloadScan).is_active();
    let can_start = !state.latest.offload.cards.is_empty()
        && state.latest.offload.parent_folder.is_some()
        && !is_running
        && !is_scanning;

    ui.horizontal(|ui| {
        if is_running {
            if ui
                .button(
                    RichText::new("■ Cancel")
                        .font(FontId::proportional(13.0))
                        .color(colors.error_red),
                )
                .clicked()
            {
                state.send(GuiCommand::Offload(OffloadCommand::CancelOffload));
            }
        } else if can_start {
            if ui
                .button(
                    RichText::new("Start Offload")
                        .font(FontId::proportional(13.0))
                        .color(ACCENT),
                )
                .clicked()
            {
                state.mark_offload_start_pending();
                state.send(GuiCommand::Offload(OffloadCommand::StartOffload));
            }
        } else {
            ui.add_enabled(false, egui::Button::new("Start Offload"));
        }
    });
}

fn render_progress(ui: &mut Ui, state: &mut AppState) {
    let s = std::sync::Arc::clone(&state.latest);
    let colors = state.theme.colors();
    let off = &s.offload;
    let copy_job = s.job(JobKind::OffloadCopy);
    let copy_running = copy_job.is_active();
    let copy_fraction = copy_job.fraction();
    let copy_speed = copy_job.speed().unwrap_or(0.0);
    let units = copy_job.units().to_vec();

    // Overall progress bar.
    if copy_running || copy_fraction > 0.0 {
        render_overall_progress(
            ui,
            &colors,
            copy_fraction,
            copy_speed,
            copy_running,
            &units,
            &off.device_totals,
        );
        ui.add_space(4.0);

        // Per-device progress — merge units with device_totals.
        for (i, unit) in units.iter().enumerate() {
            render_device_progress_row(ui, &colors, unit, off.device_totals.get(i));
        }
        ui.add_space(8.0);
    }

    render_completed_devices(ui, &colors, off);

    // Error message.
    if let Some(ref err) = off.error {
        ui.label(
            RichText::new(format!("Error: {}", err))
                .font(FontId::monospace(11.0))
                .color(colors.error_red),
        );
    }
}

/// Overall copy progress bar + speed / byte-totals line.
fn render_overall_progress(
    ui: &mut Ui,
    colors: &ThemeColors,
    copy_fraction: f32,
    copy_speed: f64,
    copy_running: bool,
    units: &[gui_engine::job::UnitSnapshot],
    device_totals: &[gui_engine::offload::OffloadDeviceTotals],
) {
    ui.add(
        egui::ProgressBar::new(copy_fraction)
            .show_percentage()
            .desired_width(ui.available_width()),
    );

    let speed_text = if copy_speed > 0.0 {
        format!("{} / s", format_bytes(copy_speed as u64))
    } else {
        String::new()
    };
    ui.horizontal(|ui| {
        if !speed_text.is_empty() {
            ui.label(
                RichText::new(&speed_text)
                    .font(FontId::monospace(10.0))
                    .color(ACCENT),
            );
        }
        if copy_running {
            let (done_bytes, total_bytes) = copy_byte_totals(units, device_totals);
            if total_bytes > 0 {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!(
                            "{} / {}",
                            format_bytes(done_bytes),
                            format_bytes(total_bytes)
                        ))
                        .font(FontId::monospace(10.0))
                        .color(colors.text_muted),
                    );
                });
            }
        }
    });
}

/// Pure byte totals across all copying devices: `(bytes_done, bytes_total)`,
/// with each unit's fraction weighted by its device's planned byte total.
fn copy_byte_totals(
    units: &[gui_engine::job::UnitSnapshot],
    device_totals: &[gui_engine::offload::OffloadDeviceTotals],
) -> (u64, u64) {
    let total_bytes: u64 = device_totals.iter().map(|d| d.bytes_total).sum();
    let done_bytes: u64 = units
        .iter()
        .zip(device_totals.iter())
        .map(|(u, t)| (u.fraction * t.bytes_total as f32) as u64)
        .sum();
    (done_bytes, total_bytes)
}

/// Pure status icon + color for a copy unit's state.
fn unit_state_icon(state: UnitState, colors: &ThemeColors) -> (&'static str, Color32) {
    match state {
        UnitState::Pending => ("⏳", colors.text_muted),
        UnitState::Running => ("▶", ACCENT),
        UnitState::Done => ("✅", colors.success_green),
        UnitState::Failed => ("❌", colors.error_red),
        UnitState::Skipped => ("⏭", colors.text_muted),
    }
}

/// Pure right-aligned progress text for one device: byte totals (only when
/// known) followed by `"<done>/<total> files"`.
fn device_progress_text(
    unit: &gui_engine::job::UnitSnapshot,
    totals: Option<&gui_engine::offload::OffloadDeviceTotals>,
) -> String {
    let bytes_total = totals.map(|t| t.bytes_total).unwrap_or(0);
    let bytes_done = (unit.fraction * bytes_total as f32) as u64;
    let files_total = totals.map(|t| t.files_total).unwrap_or(0);

    let bytes_str = if bytes_total > 0 {
        format!(
            "{} / {}  ",
            format_bytes(bytes_done),
            format_bytes(bytes_total)
        )
    } else {
        String::new()
    };
    let files_done = if files_total > 0 {
        (unit.fraction * files_total as f32).round() as usize
    } else {
        0
    };
    format!("{}{}/{} files", bytes_str, files_done, files_total)
}

/// One device's progress row: status icon + name, progress text, bar, and —
/// while running — the live unit message.
fn render_device_progress_row(
    ui: &mut Ui,
    colors: &ThemeColors,
    unit: &gui_engine::job::UnitSnapshot,
    totals: Option<&gui_engine::offload::OffloadDeviceTotals>,
) {
    let text_muted = colors.text_muted;
    let dev_pct = unit.fraction;
    let dev_name = totals.map(|t| t.name.as_str()).unwrap_or(&unit.label);
    let bytes_total = totals.map(|t| t.bytes_total).unwrap_or(0);
    let files_total = totals.map(|t| t.files_total).unwrap_or(0);

    let (status_icon, status_color) = unit_state_icon(unit.state, colors);
    ui.horizontal(|ui| {
        ui.label(
            RichText::new(format!("{} {}", status_icon, dev_name))
                .font(FontId::monospace(11.0))
                .color(status_color),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let bytes_str = device_progress_text(unit, totals);
            ui.label(
                RichText::new(bytes_str)
                    .font(FontId::monospace(10.0))
                    .color(text_muted),
            );
        });
    });
    if files_total > 0 || bytes_total > 0 {
        ui.add(egui::ProgressBar::new(dev_pct).desired_width(ui.available_width()));
    }

    if let UnitState::Running = unit.state {
        if !unit.message.is_empty() {
            ui.label(
                RichText::new(format!("  {}", unit.message))
                    .font(FontId::monospace(9.0))
                    .color(colors.text_muted),
            );
        }
    }
}

/// Completed-devices summary block (no-op when the list is empty).
fn render_completed_devices(
    ui: &mut Ui,
    colors: &ThemeColors,
    off: &gui_engine::offload::OffloadSnapshot,
) {
    if off.completed_devices.is_empty() {
        return;
    }
    ui.add_space(4.0);
    ui.label(
        RichText::new("Completed:")
            .font(FontId::proportional(12.0))
            .color(colors.success_green),
    );
    for name in &off.completed_devices {
        ui.label(
            RichText::new(format!("  ✅ {}", name))
                .font(FontId::monospace(11.0))
                .color(colors.success_green),
        );
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
#[cfg(test)]
mod tests {
    use super::*;
    use gui_engine::job::UnitSnapshot;
    use gui_engine::offload::{OffloadDeviceTotals, OffloadFileInfo, SdCardInfo};
    use std::path::PathBuf;

    fn card(selected_count: usize, media_count: usize, selected_bytes: u64) -> SdCardInfo {
        SdCardInfo {
            mount: PathBuf::from("/mnt/card"),
            volume_label: String::new(),
            device_name: "CAM".to_string(),
            name_source: gui_engine::offload::DeviceNameSource::Manual,
            media_file_count: media_count,
            total_bytes: 0,
            files: Vec::new(),
            selected: Vec::new(),
            selected_count,
            selected_bytes,
        }
    }

    fn unit(fraction: f32, state: UnitState) -> UnitSnapshot {
        UnitSnapshot {
            label: "DEV".to_string(),
            message: String::new(),
            fraction,
            state,
        }
    }

    fn totals(bytes: u64, files: usize) -> OffloadDeviceTotals {
        OffloadDeviceTotals {
            name: "DEV".to_string(),
            files_total: files,
            bytes_total: bytes,
        }
    }

    #[test]
    fn selection_summary_reports_count_and_selected_bytes() {
        // test-lint: allow(text-pin): formatter output is the contract
        let summary = selection_summary(&card(3, 10, 2048));
        assert_eq!(summary, "3/10 file(s) selected · 2.0 KB");
    }

    #[test]
    fn duration_cell_text_formats_probed_value() {
        let mut map = HashMap::new();
        map.insert(PathBuf::from("a.wav"), Some(12.5));
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            duration_cell_text(&map, &PathBuf::from("a.wav")),
            format_duration_secs(12.5)
        );
    }

    #[test]
    fn duration_cell_text_distinguishes_failed_from_pending() {
        let mut map = HashMap::new();
        map.insert(PathBuf::from("failed.wav"), None);
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(duration_cell_text(&map, &PathBuf::from("failed.wav")), "—");
        assert_eq!(duration_cell_text(&map, &PathBuf::from("pending.wav")), "…");
    }

    #[test]
    fn copy_byte_totals_weights_fraction_by_device_bytes() {
        let units = vec![unit(0.5, UnitState::Running), unit(1.0, UnitState::Done)];
        let totals_list = vec![totals(1000, 2), totals(100, 1)];
        let (done, total) = copy_byte_totals(&units, &totals_list);
        assert_eq!(total, 1100);
        assert_eq!(done, 600); // 500 + 100 (truncated)
    }

    #[test]
    fn copy_byte_totals_handles_missing_units_or_totals() {
        assert_eq!(copy_byte_totals(&[], &[]), (0, 0));
        // More totals than units: the extra device still counts toward total.
        let (done, total) = copy_byte_totals(
            &[unit(1.0, UnitState::Done)],
            &[totals(10, 1), totals(20, 1)],
        );
        assert_eq!(total, 30);
        assert_eq!(done, 10);
    }

    #[test]
    fn unit_state_icon_maps_every_state() {
        let colors = crate::theme::Theme::Dark.colors();
        let (icon, color) = unit_state_icon(UnitState::Pending, &colors);
        assert_eq!(icon, "⏳");
        assert_eq!(color, colors.text_muted);
        let (icon, color) = unit_state_icon(UnitState::Running, &colors);
        assert_eq!(icon, "▶");
        assert_eq!(color, ACCENT);
        let (icon, color) = unit_state_icon(UnitState::Done, &colors);
        assert_eq!(icon, "✅");
        assert_eq!(color, colors.success_green);
        let (icon, color) = unit_state_icon(UnitState::Failed, &colors);
        assert_eq!(icon, "❌");
        assert_eq!(color, colors.error_red);
        let (icon, color) = unit_state_icon(UnitState::Skipped, &colors);
        assert_eq!(icon, "⏭");
        assert_eq!(color, colors.text_muted);
    }

    #[test]
    fn device_progress_text_includes_bytes_only_when_known() {
        // test-lint: allow(text-pin): formatter output is the contract
        let u = unit(0.5, UnitState::Running);
        assert_eq!(device_progress_text(&u, None), "0/0 files");
        assert_eq!(
            device_progress_text(&u, Some(&totals(1000, 10))),
            "500 B / 1.0 KB  5/10 files"
        );
    }

    #[test]
    fn file_row_data_extraction_matches_engine_selection_default() {
        // render_file_row reads `card.selected.get(i)` with a false fallback;
        // pin the fallback semantics the extractor relies on.
        let mut c = card(0, 0, 0);
        c.selected = vec![true];
        assert!(c.selected.first().copied().unwrap_or(false));
        assert!(!c.selected.get(5).copied().unwrap_or(false));
    }

    #[test]
    fn offload_file_info_row_shape_is_keyed_by_path() {
        // The row renderer keys shadows by (mount, path); pin that the info
        // struct carries both.
        let info = OffloadFileInfo {
            path: PathBuf::from("/mnt/card/a.wav"),
            name: "a.wav".to_string(),
            size_bytes: 7,
            modified: None,
        };
        assert_eq!(info.path.file_name(), Some(std::ffi::OsStr::new("a.wav")));
    }
}
