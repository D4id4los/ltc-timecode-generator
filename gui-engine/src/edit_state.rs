//! GUI-side shadow state for interactive widgets bound to engine values.
//!
//! Immediate-mode widgets display whatever they are fed each frame, so the
//! fed value must come from somewhere stable while the user edits: re-seeding
//! from the (potentially stale) engine snapshot every frame makes egui revert
//! in-flight edits and jump the caret. This module provides a small pure
//! state machine that solves the problem uniformly for every widget type:
//!
//! 1. **Shadow value** — the widget renders from [`EditState::value_mut`],
//!    never directly from the snapshot.
//! 2. **Controlled synchronisation** — [`EditState::sync`] adopts the engine
//!    truth only when the widget is unfocused and no unconfirmed send is in
//!    flight.
//! 3. **Exact echo confirmation** — each send records the engine's
//!    `applied_command_seq` at send time; once the published snapshot's
//!    counter passes it, the edit is confirmed (engine echoed the value) or
//!    detected as overridden (engine changed it as a side-effect). A timeout
//!    covers anomalous channels where the ack never arrives.
//!
//! The type is GUI-framework-agnostic and lives in `gui-engine` so both
//! frontends can share it (mirrors the `theme.rs` precedent).

use std::time::{Duration, Instant};

/// How long to wait for the engine's ack of a sent edit before treating the
/// send as overridden. With a working `applied_command_seq` this only fires
/// when the ack channel is broken.
pub const EDIT_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug)]
struct Pending<T> {
    /// Sender-assigned sequence number; the edit is acked once the
    /// snapshot's `applied_command_seq >= seq`.
    seq: u64,
    since: Instant,
    _marker: std::marker::PhantomData<T>,
}

/// Shadow state for one interactive widget bound to an engine truth.
#[derive(Debug)]
pub struct EditState<T> {
    value: T,
    pending: Option<Pending<T>>,
    focused: bool,
}

impl<T: PartialEq + Clone> EditState<T> {
    pub fn new(initial: T) -> Self {
        Self {
            value: initial,
            pending: None,
            focused: false,
        }
    }

    /// Current shadow value — what the widget should render.
    pub fn value(&self) -> &T {
        &self.value
    }

    /// Mutable shadow value for `&mut`-binding widgets (egui TextEdit,
    /// Checkbox, Slider). After mutating, call [`EditState::send_and_mark`]
    /// with the new value so the in-flight window is protected.
    pub fn value_mut(&mut self) -> &mut T {
        &mut self.value
    }

    /// Record that the widget had focus on the last frame. The focus gate in
    /// [`EditState::sync`] only adopts engine values into unfocused widgets,
    /// so the GUI must call this every frame after drawing.
    pub fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// Send bookkeeping: adopt `value` into the shadow and mark it as
    /// in-flight at sender sequence `seq`. Call after (or instead of) sending
    /// the corresponding command; `seq` is the value returned by the
    /// GUI's command sink for that send.
    pub fn send_and_mark(&mut self, value: T, seq: u64, now: Instant) {
        self.pending = Some(Pending {
            seq,
            since: now,
            _marker: std::marker::PhantomData,
        });
        self.value = value;
    }

    /// Force the shadow to the engine truth, discarding any in-flight edit.
    /// For programmatic writes and structural changes (e.g. the channel map
    /// being resized by a probe) where the shadow must follow unconditionally.
    pub fn force_adopt(&mut self, truth: &T) {
        self.pending = None;
        self.value = truth.clone();
    }

    /// Per-frame synchronisation with the engine truth. Rules, in order:
    ///
    /// 1. If an edit is in flight and the snapshot's ack counter has passed
    ///    its send seq: the engine has applied everything up to that send.
    ///    Equal truth confirms the edit; different truth means the engine
    ///    changed it as a side-effect — either way the pending clears and
    ///    the focus gate decides adoption below.
    /// 2. If an edit is in flight but unacked and not yet timed out: keep
    ///    the user's value (early return, no adoption).
    /// 3. Adoption gate: an unfocused widget whose value differs from the
    ///    truth adopts the truth (engine-initiated changes propagate). A
    ///    focused widget never adopts — the user is editing.
    pub fn sync(&mut self, truth: &T, applied_seq: u64, now: Instant) {
        if let Some(pending) = &self.pending {
            if applied_seq >= pending.seq
                || now.duration_since(pending.since) > EDIT_CONFIRM_TIMEOUT
            {
                self.pending = None;
            } else {
                // Still awaiting the ack — keep the user's value.
                return;
            }
        }

        if !self.focused && self.value != *truth {
            self.value = truth.clone();
        }
    }

    /// True while an edit is awaiting the engine ack (diagnostics/tests).
    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> Instant {
        Instant::now() + Duration::from_secs(secs)
    }

    // ── Basic adoption ───────────────────────────────────────────────────

    #[test]
    fn unfocused_widget_adopts_engine_value() {
        let mut es = EditState::new("old".to_string());
        es.sync(&"new".to_string(), 0, t(0));
        assert_eq!(es.value(), "new");
        assert!(!es.is_pending());
    }

    #[test]
    fn focused_widget_keeps_user_value() {
        let mut es = EditState::new("engine".to_string());
        es.set_focused(true);
        es.value_mut().push_str("-edited");
        es.sync(&"engine".to_string(), 0, t(0));
        assert_eq!(es.value(), "engine-edited");
    }

    #[test]
    fn adoption_happens_after_focus_is_lost() {
        let mut es = EditState::new("engine".to_string());
        es.set_focused(true);
        es.value_mut().push_str("-edited");
        es.sync(&"engine".to_string(), 0, t(0));
        // User tabs away; next frame the engine value is adopted.
        es.set_focused(false);
        es.sync(&"engine-changed-by-side-effect".to_string(), 0, t(0));
        assert_eq!(es.value(), "engine-changed-by-side-effect");
    }

    #[test]
    fn equal_values_are_noop() {
        let mut es = EditState::new("same".to_string());
        es.sync(&"same".to_string(), 0, t(0));
        assert_eq!(es.value(), "same");
        assert!(!es.is_pending());
    }

    // ── Pending / ack lifecycle ──────────────────────────────────────────

    #[test]
    fn pending_edit_is_kept_until_acked() {
        let mut es = EditState::new("engine".to_string());
        es.send_and_mark("user".to_string(), 5, t(0));
        // Ack counter at 4 — not yet applied. Engine still holds the old
        // value, and the user's edit must survive the sync.
        es.sync(&"engine".to_string(), 4, t(0));
        assert_eq!(es.value(), "user");
        assert!(es.is_pending());
    }

    #[test]
    fn ack_with_matching_truth_confirms_edit() {
        let mut es = EditState::new("engine".to_string());
        es.send_and_mark("user".to_string(), 5, t(0));
        // Engine echoed the value back at seq 6.
        es.sync(&"user".to_string(), 6, t(0));
        assert_eq!(es.value(), "user");
        assert!(!es.is_pending());
    }

    #[test]
    fn ack_with_different_truth_resolves_as_override() {
        let mut es = EditState::new("engine".to_string());
        es.set_focused(true);
        es.send_and_mark("user".to_string(), 5, t(0));
        // Engine applied the command but a side-effect replaced the value.
        es.sync(&"engine-override".to_string(), 6, t(0));
        // Focused: user's text still wins for now…
        assert_eq!(es.value(), "user");
        assert!(!es.is_pending());
        // …and is dropped once the widget is unfocused.
        es.set_focused(false);
        es.sync(&"engine-override".to_string(), 6, t(0));
        assert_eq!(es.value(), "engine-override");
    }

    #[test]
    fn idempotent_write_confirms_on_seq_pass_without_echo_change() {
        // The publish gate skips stores for no-op writes, but the seq bump
        // still publishes; the ack confirms the pending even though the
        // truth never equalled a *changed* value.
        let mut es = EditState::new("0.25".to_string());
        es.send_and_mark("0.25".to_string(), 2, t(0));
        es.sync(&"0.25".to_string(), 2, t(0));
        assert!(!es.is_pending());
        assert_eq!(es.value(), "0.25");
    }

    #[test]
    fn unacked_edit_times_out_and_adopts_engine() {
        let mut es = EditState::new("engine".to_string());
        es.send_and_mark("user".to_string(), 5, t(0));
        // Sent at t(0); sync at t(0)+3s with the ack never arriving.
        let late = t(0) + Duration::from_secs(3);
        es.sync(&"engine".to_string(), 4, late);
        assert!(!es.is_pending());
        assert_eq!(es.value(), "engine");
    }

    #[test]
    fn subsequent_send_replaces_pending() {
        let mut es = EditState::new("engine".to_string());
        es.send_and_mark("first".to_string(), 5, t(0));
        es.send_and_mark("second".to_string(), 6, t(0));
        // Only the latest send's seq matters.
        es.sync(&"engine".to_string(), 5, t(0));
        assert_eq!(es.value(), "second");
        assert!(es.is_pending());
        es.sync(&"second".to_string(), 6, t(0));
        assert!(!es.is_pending());
    }

    // ── force_adopt ──────────────────────────────────────────────────────

    #[test]
    fn force_adopt_discards_pending_and_user_value() {
        let mut es = EditState::new("engine".to_string());
        es.set_focused(true);
        es.send_and_mark("user".to_string(), 5, t(0));
        es.force_adopt(&"restructured".to_string());
        assert_eq!(es.value(), "restructured");
        assert!(!es.is_pending());
    }

    // ── Non-string types ─────────────────────────────────────────────────

    #[test]
    fn works_with_bool_and_f32() {
        let mut b = EditState::new(false);
        b.send_and_mark(true, 1, t(0));
        b.sync(&true, 1, t(0));
        assert!(*b.value());

        let mut f = EditState::new(0.25f32);
        f.send_and_mark(0.5, 2, t(0));
        f.sync(&0.25, 1, t(0)); // unacked — keep user value
        assert_eq!(*f.value(), 0.5);
    }
}
