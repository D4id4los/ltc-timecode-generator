//! Bound-widget wrappers: egui widgets driven by an [`EditState`] shadow and
//! an engine truth, with per-change command dispatch and echo confirmation.
//!
//! Every wrapper follows the same four-step contract:
//! 1. `sync` the shadow against the engine truth (adopts only when the
//!    widget is unfocused and no unconfirmed send is in flight),
//! 2. draw from the shadow value,
//! 3. on user change: send the command through [`AppState::send`] (which
//!    assigns the ack sequence) and mark the shadow pending,
//! 4. track focus for the next frame's adoption gate.
//!
//! A short repaint nudge after each send keeps the echo round-trip (~40 ms
//! engine tick) visible on the very next frame instead of waiting out the
//! idle repaint interval.

use std::path::PathBuf;
use std::time::Duration;

use egui::{TextEdit, Ui};

use gui_engine::edit_state::EditState;

use crate::app::AppState;

/// How long after a send to force fast repaints so the engine echo is
/// rendered promptly (the engine publishes within one 40 ms tick).
const ECHO_REPAINT: Duration = Duration::from_millis(80);

fn now() -> std::time::Instant {
    std::time::Instant::now()
}

/// Sync a shadow against the engine truth without drawing anything.
/// Use for shadows whose widget draws itself (custom selectors) or whose
/// value is only read (combo display).
pub fn sync<T: PartialEq + Clone>(
    state: &mut AppState,
    shadow: impl Fn(&mut AppState) -> &mut EditState<T>,
    truth: T,
) {
    let seq = state.applied_seq;
    let t = now();
    shadow(state).sync(&truth, seq, t);
}

/// Programmatic write: send the command for `value` and adopt it into the
/// shadow immediately (e.g. Browse-dialog results, cascaded resets).
pub fn set_value<T: PartialEq + Clone>(
    state: &mut AppState,
    shadow: impl Fn(&mut AppState) -> &mut EditState<T>,
    value: T,
    make_cmd: impl Fn(T) -> gui_engine::command::GuiCommand,
) {
    let seq = state.send(make_cmd(value.clone()));
    let t = now();
    shadow(state).send_and_mark(value, seq, t);
}

/// Selector-style interaction: sync against `truth`, then send+mark
/// `new_value`. The shadow becomes the display source so a click is never
/// visually reverted by a stale snapshot.
pub fn select_value<T: PartialEq + Clone>(
    state: &mut AppState,
    shadow: impl Fn(&mut AppState) -> &mut EditState<T>,
    truth: T,
    new_value: T,
    make_cmd: impl Fn(T) -> gui_engine::command::GuiCommand,
) {
    let seq = state.applied_seq;
    let t = now();
    shadow(state).sync(&truth, seq, t);
    let seq = state.send(make_cmd(new_value.clone()));
    shadow(state).send_and_mark(new_value, seq, t);
}

/// Single-line text edit bound to an engine `String` truth. Sends on every
/// keystroke (per-change), protected by the pending/ack window.
pub fn text(
    ui: &mut Ui,
    state: &mut AppState,
    shadow: impl Fn(&mut AppState) -> &mut EditState<String>,
    truth: &str,
    make_cmd: impl Fn(String) -> gui_engine::command::GuiCommand,
    style: impl FnOnce(TextEdit<'_>) -> TextEdit<'_>,
) -> egui::Response {
    let seq = state.applied_seq;
    let t = now();
    shadow(state).sync(&truth.to_string(), seq, t);

    let mut draft = shadow(state).value().clone();
    let response = ui.add(style(TextEdit::singleline(&mut draft)));
    let changed = response.changed();
    let focused = response.has_focus();
    shadow(state).set_focused(focused);
    if changed {
        set_value(state, shadow, draft, make_cmd);
        ui.ctx().request_repaint_after(ECHO_REPAINT);
    }
    response
}

/// Single-line text edit bound to an engine `PathBuf` truth. The draft is
/// kept as a String for editing; commits convert back to a `PathBuf`.
pub fn path_text(
    ui: &mut Ui,
    state: &mut AppState,
    shadow: impl Fn(&mut AppState) -> &mut EditState<PathBuf>,
    truth: &std::path::Path,
    make_cmd: impl Fn(PathBuf) -> gui_engine::command::GuiCommand,
    style: impl FnOnce(TextEdit<'_>) -> TextEdit<'_>,
) -> egui::Response {
    let seq = state.applied_seq;
    let t = now();
    let truth_pb = PathBuf::from(truth);
    shadow(state).sync(&truth_pb, seq, t);

    let mut draft = shadow(state).value().to_string_lossy().to_string();
    let response = ui.add(style(TextEdit::singleline(&mut draft)));
    let changed = response.changed();
    let focused = response.has_focus();
    shadow(state).set_focused(focused);
    if changed {
        let path = PathBuf::from(&draft);
        set_value(state, shadow, path, make_cmd);
        ui.ctx().request_repaint_after(ECHO_REPAINT);
    }
    response
}

/// Checkbox bound to an engine `bool` truth.
pub fn checkbox(
    ui: &mut Ui,
    state: &mut AppState,
    shadow: impl Fn(&mut AppState) -> &mut EditState<bool>,
    truth: bool,
    label: &str,
    enabled: bool,
    make_cmd: impl Fn(bool) -> gui_engine::command::GuiCommand,
) -> egui::Response {
    let seq = state.applied_seq;
    let t = now();
    shadow(state).sync(&truth, seq, t);

    let mut v = *shadow(state).value();
    let cb = egui::Checkbox::new(&mut v, label);
    let response = if enabled {
        ui.add(cb)
    } else {
        ui.add_enabled(false, cb)
    };
    let changed = response.changed();
    let focused = response.has_focus();
    shadow(state).set_focused(focused);
    if changed {
        set_value(state, shadow, v, make_cmd);
        ui.ctx().request_repaint_after(ECHO_REPAINT);
    }
    response
}

/// Slider bound to an engine `f32` truth. Sends on every drag tick.
pub fn slider(
    ui: &mut Ui,
    state: &mut AppState,
    shadow: impl Fn(&mut AppState) -> &mut EditState<f32>,
    truth: f32,
    range: std::ops::RangeInclusive<f32>,
    step: Option<f64>,
    make_cmd: impl Fn(f32) -> gui_engine::command::GuiCommand,
) -> egui::Response {
    let seq = state.applied_seq;
    let t = now();
    shadow(state).sync(&truth, seq, t);

    let mut v = *shadow(state).value();
    let mut s = egui::Slider::new(&mut v, range).show_value(false);
    if let Some(step) = step {
        s = s.step_by(step);
    }
    let response = ui.add(s);
    let changed = response.changed();
    let focused = response.has_focus();
    shadow(state).set_focused(focused);
    if changed {
        set_value(state, shadow, v, make_cmd);
        ui.ctx().request_repaint_after(ECHO_REPAINT);
    }
    response
}

/// Sync the channel-map shadow. Structural rule: when the expected channel
/// count differs from the shadow's (probe resize), adopt the engine's map
/// unconditionally; otherwise sync normally against the engine truth (which
/// carries the user's applied swaps).
pub fn sync_channel_map(state: &mut AppState, expected_channels: usize) {
    let truth = state.latest.converter.settings.channel_map.clone();
    let t = now();
    let seq = state.applied_seq;
    if state.sh.conv.channel_map.value().num_channels() != expected_channels {
        state
            .sh
            .conv
            .channel_map
            .force_adopt(&gui_engine::converter::ChannelMap::identity(
                expected_channels,
            ));
    } else {
        state.sh.conv.channel_map.sync(&truth, seq, t);
    }
}
