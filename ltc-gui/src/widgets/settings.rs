use crate::text::{text, TextStyle};
use egui::{Sense, Ui, Vec2};
use gui_engine::command::GuiCommand;
use gui_engine::{timecode::FPS_OPTIONS, ChannelSel};

use super::{bound, style};
use crate::app::AppState;
use crate::theme::ACCENT;

pub fn render(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let playing = state.latest.is_playing;

    style::content_area(ui, &colors, |ui| {
        // 1. Start Timecode
        ui.horizontal(|ui| {
            ui.label(text(ui, "SET STARTING TIMECODE").style(TextStyle::Heading, &colors));
            if playing {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    style::static_badge(
                        ui,
                        &colors,
                        style::BadgeTone::Warning,
                        "STOP STREAM TO EDIT",
                        TextStyle::Description.font_spec(),
                    );
                });
            }
        });
        ui.add_space(8.0);
        render_timecode_steppers(ui, state);
        ui.add_space(16.0);

        ui.label(text(ui, "SELECT FRAME RATE").style(TextStyle::Heading, &colors));
        ui.add_space(8.0);
        render_frame_rate(ui, state);
        ui.add_space(16.0);

        ui.label(text(ui, "SAMPLE RATE").style(TextStyle::Heading, &colors));
        ui.add_space(8.0);
        render_sample_rate(ui, state);
        ui.add_space(16.0);

        ui.label(text(ui, "OUTPUT AUDIO INTERFACE SELECTION").style(TextStyle::Heading, &colors));
        ui.add_space(8.0);
        render_audio_device(ui, state);
        ui.add_space(16.0);

        ui.label(text(ui, "AUDIO ROUTING & SETTINGS").style(TextStyle::Heading, &colors));
        ui.add_space(8.0);
        render_routing_and_volume(ui, state);
        ui.add_space(16.0);

        ui.label(text(ui, "TEXT SIZE").style(TextStyle::Heading, &colors));
        ui.add_space(8.0);
        render_text_scale(ui, state);
    });
}

/// Accessibility text-scale chips; selection derives from the engine state
/// (like the theme toggle), so a plain command send is enough.
fn render_text_scale(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let selected = state.latest.text_scale_percent;
    ui.horizontal(|ui| {
        for &percent in gui_engine::state::TEXT_SCALE_OPTIONS {
            let font = if percent == selected {
                crate::text::TextStyle::Control.font_spec().bold()
            } else {
                crate::text::TextStyle::Control.font_spec()
            };
            if style::option_chip(
                ui,
                &colors,
                &format!("{}%", percent),
                font,
                percent == selected,
                true,
                Vec2::ZERO,
            )
            .clicked()
            {
                state.send(GuiCommand::SetTextScale(percent));
            }
        }
    });
}

fn render_timecode_steppers(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let is_playing = state.latest.is_playing;
    let tc = state.latest.start_timecode;

    ui.add_enabled_ui(!is_playing, |ui| {
        ui.columns(4, |cols| {
            stepper_card_col(
                &mut cols[0],
                "HOURS",
                tc.hours,
                &colors,
                TcField::Hours,
                state,
            );
            stepper_card_col(
                &mut cols[1],
                "MINUTES",
                tc.minutes,
                &colors,
                TcField::Minutes,
                state,
            );
            stepper_card_col(
                &mut cols[2],
                "SECONDS",
                tc.seconds,
                &colors,
                TcField::Seconds,
                state,
            );
            stepper_card_col(
                &mut cols[3],
                "FRAMES",
                tc.frames,
                &colors,
                TcField::Frames,
                state,
            );
        });
    });
}

/// Which start-timecode segment a stepper card edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TcField {
    Hours,
    Minutes,
    Seconds,
    Frames,
}

/// Pure mapping from a timecode segment to its `(increment, decrement)`
/// engine commands.
fn tc_commands(field: TcField) -> (GuiCommand, GuiCommand) {
    match field {
        TcField::Hours => (GuiCommand::HourUp, GuiCommand::HourDown),
        TcField::Minutes => (GuiCommand::MinuteUp, GuiCommand::MinuteDown),
        TcField::Seconds => (GuiCommand::SecondUp, GuiCommand::SecondDown),
        TcField::Frames => (GuiCommand::FrameUp, GuiCommand::FrameDown),
    }
}

fn stepper_card_col(
    ui: &mut Ui,
    label: &str,
    value: u32,
    colors: &crate::theme::ThemeColors,
    field: TcField,
    state: &mut AppState,
) {
    let (cmd_up, cmd_down) = tc_commands(field);
    let card = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::symmetric(10, 8));
    card.show(ui, |ui| {
        ui.vertical_centered(|ui| {
            if ui
                .button(
                    text(ui, "^")
                        .bold()
                        .color(ui.style().visuals.strong_text_color()),
                )
                .clicked()
            {
                state.send(cmd_up);
            }
            ui.add_space(1.0);
            ui.label(
                text(ui, format!("{:02}", value))
                    .style(TextStyle::BigValue, colors)
                    .color(colors.text_title),
            );
            ui.add_space(1.0);
            ui.label(text(ui, label).style(TextStyle::Caption, colors));
            ui.add_space(1.0);
            if ui
                .button(
                    text(ui, "v")
                        .bold()
                        .color(ui.style().visuals.strong_text_color()),
                )
                .clicked()
            {
                state.send(cmd_down);
            }
        });
    });
}

fn render_frame_rate(ui: &mut Ui, state: &mut AppState) {
    let width = ui.available_width();
    let is_playing = state.latest.is_playing;
    ui.add_enabled_ui(!is_playing, |ui| {
        if width > 520.0 {
            ui.columns(5, |cols| {
                for (i, opt) in FPS_OPTIONS.iter().enumerate() {
                    frame_rate_card(&mut cols[i], i, opt, state);
                }
            });
        } else {
            let usable = ((width - 32.0) / 2.0).max(1.0);
            if usable < 90.0 || width < 260.0 {
                for (i, opt) in FPS_OPTIONS.iter().enumerate() {
                    frame_rate_card(ui, i, opt, state);
                    ui.add_space(6.0);
                }
            } else {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::Vec2::new(8.0, 8.0);
                    for (i, opt) in FPS_OPTIONS.iter().enumerate() {
                        ui.scope(|ui| {
                            ui.set_max_width(usable);
                            frame_rate_card(ui, i, opt, state);
                        });
                    }
                });
            }
        }
    });
}

fn frame_rate_card(
    ui: &mut Ui,
    index: usize,
    opt: &gui_engine::timecode::FpsOption,
    state: &mut AppState,
) {
    let is_selected = index == state.latest.fps_index;
    let is_playing = state.latest.is_playing;
    let response = style::option_card(
        ui,
        &state.theme.colors(),
        opt.name,
        opt.description,
        is_selected,
        !is_playing,
    );
    if response.clicked() {
        state.send(GuiCommand::SetFpsIndex(index));
    }
}

fn render_sample_rate(ui: &mut Ui, state: &mut AppState) {
    let is_playing = state.latest.is_playing;
    let rates = gui_engine::SAMPLE_RATE_OPTIONS;
    let selected_rate = state.latest.sample_rate;
    bound::sync(state, |s| &mut s.sh.sample_rate, selected_rate);
    let selected_rate = *state.sh.sample_rate.value();
    ui.add_enabled_ui(!is_playing, |ui| {
        let width = ui.available_width();
        if width > 300.0 {
            ui.horizontal(|ui| {
                for &rate in rates {
                    sample_rate_button(ui, state, selected_rate, rate);
                }
            });
        } else {
            for &rate in rates {
                sample_rate_button(ui, state, selected_rate, rate);
                ui.add_space(6.0);
            }
        }
    });
}

/// One sample-rate option chip (shared by the wide and narrow layouts).
fn sample_rate_button(ui: &mut Ui, state: &mut AppState, selected_rate: u32, rate: u32) {
    let is_selected = rate == selected_rate;
    let colors = state.theme.colors();
    if style::option_chip(
        ui,
        &colors,
        &format!("{} Hz", rate),
        crate::text::TextStyle::Control.font_spec(),
        is_selected,
        true,
        Vec2::ZERO,
    )
    .clicked()
    {
        bound::select_value(
            state,
            |s| &mut s.sh.sample_rate,
            selected_rate,
            rate,
            GuiCommand::SetSampleRate,
        );
    }
}

fn render_audio_device(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let card = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(16));
    card.show(ui, |ui| {
        ui.vertical(|ui| {
            ui.horizontal(|ui| {
                render_device_combo(ui, state);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Refresh List").clicked() {
                        state.send(GuiCommand::RefreshDevices);
                    }
                });
            });
            ui.add_space(8.0);
            render_device_info_note(ui, &colors);
        });
    });
}

/// Pure display name for one device in the combo list: `"<name> (Default)"`
/// for the default output, plain name otherwise.
fn device_display_name(name: &str, is_default: bool) -> String {
    if is_default {
        format!("{} (Default)", name)
    } else {
        name.to_string()
    }
}

/// Pure combo-box label for the currently selected device id: the device's
/// display name, or `Default` when nothing (or an unknown id) is selected.
fn selected_device_text(
    selected: Option<&String>,
    devices: &[gui_engine::AudioDeviceInfo],
) -> String {
    selected
        .and_then(|id| devices.iter().find(|d| &d.id == id))
        .map(|d| device_display_name(&d.name, d.is_default))
        .unwrap_or_else(|| "Default".to_string())
}

/// Device selector: empty-state label or the bound combo box.
fn render_device_combo(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let device_names: Vec<String> = state
        .latest
        .devices
        .iter()
        .map(|d| device_display_name(&d.name, d.is_default))
        .collect();
    if device_names.is_empty() {
        ui.label(
            text(ui, "No devices found — using default output")
                .style(TextStyle::MonoLabel, &colors),
        );
    } else {
        let selected = state.sh.selected_device.value().clone();
        let selected_text = selected_device_text(selected.as_ref(), &state.latest.devices);
        ui.label(text(ui, "Interface:").style(TextStyle::Heading, &colors));
        egui::ComboBox::from_id_salt("settings_device_combo")
            .selected_text(&selected_text)
            .show_ui(ui, |ui| {
                for (i, dev) in state.latest.devices.clone().into_iter().enumerate() {
                    let active = selected.as_ref() == Some(&dev.id);
                    if ui.selectable_label(active, &device_names[i]).clicked() {
                        bound::select_value(
                            state,
                            |s| &mut s.sh.selected_device,
                            selected.clone(),
                            Some(dev.id.clone()),
                            |id| GuiCommand::SetDevice(id.unwrap_or_default()),
                        );
                    }
                }
            });
    }
}

/// Info note under the device selector: accent dot + description line.
fn render_device_info_note(ui: &mut Ui, colors: &crate::theme::ThemeColors) {
    let info_frame = egui::Frame::new()
        .fill(colors.card_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(6.0)
        .inner_margin(egui::Margin::symmetric(10, 6));
    info_frame.show(ui, |ui| {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::new(6.0, 6.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 3.0, ACCENT);
            ui.label(
                text(
                    ui,
                    "Sends SMPTE Linear Timecode audio to mixers, USB-DAC, or sync adapters.",
                )
                .style(TextStyle::Hint, colors),
            );
        });
    });
}

fn render_routing_and_volume(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let width = ui.available_width();
    let container = egui::Frame::new()
        .fill(colors.deep_bg)
        .stroke(egui::Stroke::new(1.0, colors.border_main))
        .corner_radius(10.0)
        .inner_margin(egui::Margin::same(8));
    container.show(ui, |ui| {
        if width > 500.0 {
            // Unequal split (egui `columns` is always equal-width): the
            // channel selectors need far less room than the volume sliders,
            // so the row is carved into explicit 25 % / 75 % panes.
            ui.horizontal(|ui| {
                let left_w = ui.available_width() * 0.25;
                ui.vertical(|ui| {
                    ui.set_min_width(left_w);
                    ui.set_max_width(left_w);
                    render_routing_buttons(ui, state);
                });
                ui.vertical(|ui| {
                    ui.set_min_width(ui.available_width());
                    render_sliders(ui, state);
                });
            });
        } else {
            render_routing_buttons(ui, state);
            ui.add_space(8.0);
            render_sliders(ui, state);
        }
    });
}

fn render_routing_buttons(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();
    let ltc_truth = state.latest.ltc_channel;
    let beep_truth = state.latest.beep_channel;
    bound::sync(state, |s| &mut s.sh.ltc_channel, ltc_truth);
    bound::sync(state, |s| &mut s.sh.beep_channel, beep_truth);
    let ltc_ch = *state.sh.ltc_channel.value();
    let beep_ch = *state.sh.beep_channel.value();

    const CHANNEL_CHOICES: [(ChannelSel, &str); 3] = [
        (ChannelSel::Left, "Left"),
        (ChannelSel::Right, "Right"),
        (ChannelSel::Both, "Both"),
    ];

    ui.label(text(ui, "LTC OUTPUT").style(TextStyle::SequenceHeading, &colors));
    ui.add_space(4.0);
    // The chip rows live in the narrow (25 %) routing pane of the wide
    // layout, so they wrap rather than overflow the pane.
    ui.horizontal_wrapped(|ui| {
        for (val, lbl) in CHANNEL_CHOICES {
            channel_button(
                ui,
                state,
                |s| &mut s.sh.ltc_channel,
                ltc_ch,
                ltc_truth,
                val,
                lbl,
                GuiCommand::SetLtcChannel,
            );
        }
    });

    ui.add_space(8.0);
    ui.label(text(ui, "CLAPPER OUTPUT").style(TextStyle::SequenceHeading, &colors));
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for (val, lbl) in CHANNEL_CHOICES {
            channel_button(
                ui,
                state,
                |s| &mut s.sh.beep_channel,
                beep_ch,
                beep_truth,
                val,
                lbl,
                GuiCommand::SetBeepChannel,
            );
        }
    });
}

/// One channel option chip (shared by the LTC and clapper routing rows).
#[allow(clippy::too_many_arguments)]
fn channel_button(
    ui: &mut Ui,
    state: &mut AppState,
    shadow: fn(&mut AppState) -> &mut gui_engine::edit_state::EditState<ChannelSel>,
    selected: ChannelSel,
    truth: ChannelSel,
    val: ChannelSel,
    lbl: &str,
    make_cmd: fn(ChannelSel) -> GuiCommand,
) {
    let colors = state.theme.colors();
    if style::option_chip(
        ui,
        &colors,
        lbl,
        crate::text::TextStyle::Control.font_spec(),
        selected == val,
        true,
        Vec2::ZERO,
    )
    .clicked()
    {
        bound::select_value(state, shadow, truth, val, make_cmd);
    }
}

fn render_sliders(ui: &mut Ui, state: &mut AppState) {
    let colors = state.theme.colors();

    let truth = state.latest.ltc_volume;
    ui.horizontal(|ui| {
        ui.label(text(ui, "LTC VOL").style(TextStyle::MonoValue, &colors));
        bound::slider(
            ui,
            state,
            |s| &mut s.sh.ltc_volume,
            truth,
            0.0..=1.0,
            Some(0.01),
            GuiCommand::SetLtcVolume,
        );
        let shown = *state.sh.ltc_volume.value();
        ui.label(
            text(ui, format!("{}%", (shown * 100.0).round()))
                .style(TextStyle::StatValue, &colors)
                .mono(),
        );
        // Live dBFS readout: the peak level the shown UI volume produces.
        let dbfs = gui_engine::ui_volume_to_dbfs(shown);
        let readout = if dbfs.is_finite() {
            format!("≈ {dbfs:.1} dBFS")
        } else {
            "silence".to_string()
        };
        ui.label(text(ui, readout).style(TextStyle::MonoValue, &colors));
        // Calibrated level presets (safe-hot reference levels for camera
        // feeds); each sends the equivalent UI volume through SetLtcVolume.
        for &(label, dbfs) in &[("-18", -18.0f32), ("-12", -12.0), ("-6", -6.0)] {
            if ui
                .small_button(text(ui, label).style(TextStyle::MonoReadout, &colors))
                .clicked()
            {
                let v = gui_engine::dbfs_to_ui_volume(dbfs);
                bound::set_value(state, |s| &mut s.sh.ltc_volume, v, GuiCommand::SetLtcVolume);
            }
        }
    });

    let truth = state.latest.beep_volume;
    ui.horizontal(|ui| {
        ui.label(text(ui, "BEEP VOL").style(TextStyle::MonoValue, &colors));
        bound::slider(
            ui,
            state,
            |s| &mut s.sh.beep_volume,
            truth,
            0.0..=1.0,
            Some(0.01),
            GuiCommand::SetBeepVolume,
        );
        let shown = *state.sh.beep_volume.value();
        ui.label(
            text(ui, format!("{}%", (shown * 100.0).round()))
                .style(TextStyle::StatValue, &colors)
                .mono(),
        );
    });

    let truth = state.latest.beep_frequency;
    ui.horizontal(|ui| {
        ui.label(text(ui, "PITCH").style(TextStyle::MonoValue, &colors));
        bound::slider(
            ui,
            state,
            |s| &mut s.sh.beep_frequency,
            truth,
            400.0..=2000.0,
            None,
            GuiCommand::SetBeepFrequency,
        );
        let shown = *state.sh.beep_frequency.value();
        ui.label(
            text(ui, format!("{} Hz", shown.round()))
                .style(TextStyle::StatValue, &colors)
                .mono(),
        );
    });

    let truth = state.latest.beep_duration;
    ui.horizontal(|ui| {
        ui.label(text(ui, "DUR").style(TextStyle::MonoValue, &colors));
        bound::slider(
            ui,
            state,
            |s| &mut s.sh.beep_duration,
            truth,
            0.05..=2.0,
            Some(0.05),
            GuiCommand::SetBeepDuration,
        );
        let shown = *state.sh.beep_duration.value();
        ui.label(
            text(ui, format!("{:.0} ms", shown * 1000.0))
                .style(TextStyle::StatValue, &colors)
                .mono(),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(id: &str, name: &str, is_default: bool) -> gui_engine::AudioDeviceInfo {
        gui_engine::AudioDeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            is_default,
            formats: Vec::new(),
            channels_min: 0,
            channels_max: 0,
            sample_rate_min: 0,
            sample_rate_max: 0,
            buffer_min: 0,
            buffer_max: 0,
        }
    }

    #[test]
    fn device_display_name_marks_the_default_output() {
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            device_display_name("PulseAudio", true),
            "PulseAudio (Default)"
        );
        assert_eq!(device_display_name("USB DAC", false), "USB DAC");
    }

    #[test]
    fn selected_device_text_resolves_the_selected_id() {
        let devices = vec![device("a", "Built-in", false), device("b", "USB DAC", true)];
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(
            selected_device_text(Some(&"b".to_string()), &devices),
            "USB DAC (Default)"
        );
        assert_eq!(
            selected_device_text(Some(&"a".to_string()), &devices),
            "Built-in"
        );
    }

    #[test]
    fn selected_device_text_falls_back_to_default() {
        // test-lint: allow(text-pin): formatter output is the contract
        assert_eq!(selected_device_text(None, &[]), "Default");
        // A stale id (device unplugged) must not panic or show a wrong name.
        assert_eq!(
            selected_device_text(Some(&"gone".to_string()), &[]),
            "Default"
        );
    }

    #[test]
    fn tc_commands_maps_each_segment_to_its_up_down_pair() {
        let (up, down) = tc_commands(TcField::Hours);
        assert!(matches!(up, GuiCommand::HourUp) && matches!(down, GuiCommand::HourDown));
        let (up, down) = tc_commands(TcField::Minutes);
        assert!(matches!(up, GuiCommand::MinuteUp) && matches!(down, GuiCommand::MinuteDown));
        let (up, down) = tc_commands(TcField::Seconds);
        assert!(matches!(up, GuiCommand::SecondUp) && matches!(down, GuiCommand::SecondDown));
        let (up, down) = tc_commands(TcField::Frames);
        assert!(matches!(up, GuiCommand::FrameUp) && matches!(down, GuiCommand::FrameDown));
    }
}
