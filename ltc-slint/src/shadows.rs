//! GUI-local shadow state for interactive widgets, bound to an engine truth
//! via [`gui_engine::edit_state::EditState`] — the same contract as the egui
//! frontend (`ltc-gui/src/shadows.rs`), adapted to Slint's retained mode:
//! widgets own their display, so the shadow gates *pushes* into Slint
//! properties instead of feeding values each frame. While an edit is
//! awaiting its engine ack, the push is skipped; once acked, one final push
//! renders the confirmed value. Engine-side side-effects (caps repair,
//! decode auto-apply, recording resets) propagate because `sync` adopts the
//! truth whenever the field is not pending.

use std::time::Instant;

use gui_engine::edit_state::EditState;
use gui_engine::state::AppStateSnapshot;
use gui_engine::SAMPLE_RATE_OPTIONS;

/// String shadow backing a two-way-bound Slint TextInput. Tracks the last
/// value pushed into the property so an unchanged value is never re-pushed
/// (pushing the text the user already typed is what makes carets jump).
#[derive(Debug)]
pub struct TextField {
    es: EditState<String>,
    last_pushed: Option<String>,
}

impl TextField {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            es: EditState::new(value.into()),
            last_pushed: None,
        }
    }

    /// Send bookkeeping: adopt `value` (the user's edit) into the shadow and
    /// mark it in-flight at sender sequence `seq`. The Slint property has
    /// already been updated by the two-way binding, so no push is needed.
    pub fn send_and_mark(&mut self, value: String, seq: u64, now: Instant) {
        self.es.send_and_mark(value, seq, now);
    }

    /// Sync against `truth` (focus-gated by `focused`) and return
    /// `Some(text)` when the Slint property should be pushed — only when no
    /// edit is awaiting its ack and the shadow value differs from the last
    /// pushed one. `None` means: leave the property alone this tick.
    pub fn sync_and_push(
        &mut self,
        truth: &str,
        focused: bool,
        applied_seq: u64,
        now: Instant,
    ) -> Option<String> {
        self.es.set_focused(focused);
        self.es.sync(&truth.to_string(), applied_seq, now);
        if self.es.is_pending() {
            return None;
        }
        if self.last_pushed.as_deref() == Some(self.es.value().as_str()) {
            return None;
        }
        self.last_pushed = Some(self.es.value().clone());
        Some(self.es.value().clone())
    }
}

/// Shadow for every user-editable converter setting (mirrors
/// `ConverterUserSettings` fields that the Slint UI edits).
#[derive(Debug)]
pub struct ConvShadows {
    pub metadata_only: EditState<bool>,
    pub generate_synthetic_video: EditState<bool>,
    pub copy_video: EditState<bool>,
    pub split_tracks: EditState<bool>,
    pub drop_ltc_track: EditState<bool>,
    pub concat_audio: EditState<bool>,
    pub set_start_from_ltc: EditState<bool>,
    pub embed_camera_metadata: EditState<bool>,
    pub ltc_file_idx: EditState<usize>,
    pub container: EditState<String>,
    pub video_encoder: EditState<String>,
    pub audio_encoder: EditState<String>,
    pub filename_prefix: TextField,
    pub audio_suffix_template: TextField,
    pub video_suffix_template: TextField,
}

/// All shadow-backed interactive fields of the Slint GUI, shared between
/// `main.rs` callbacks (send + mark) and the poll timer in `poll.rs`
/// (sync + gated push).
#[derive(Debug)]
pub struct Shadows {
    // Clapper
    pub roll: TextField,
    pub auto_increment: EditState<bool>,

    // Settings
    pub fps_index: EditState<usize>,
    /// Index into `SAMPLE_RATE_OPTIONS` (what the UI property displays).
    pub sample_rate: EditState<usize>,
    pub ltc_volume: EditState<f32>,
    pub beep_volume: EditState<f32>,
    pub beep_frequency: EditState<f32>,
    pub beep_duration: EditState<f32>,

    // Decode
    pub decode_fps_index: EditState<usize>,

    // Converter
    pub conv: ConvShadows,

    // Offload
    pub parent_name: TextField,
}

impl ConvShadows {
    pub fn new(s: &AppStateSnapshot) -> Self {
        let c = &s.converter.settings;
        Self {
            metadata_only: EditState::new(c.metadata_only),
            generate_synthetic_video: EditState::new(c.generate_synthetic_video),
            copy_video: EditState::new(c.copy_video),
            split_tracks: EditState::new(c.split_tracks),
            drop_ltc_track: EditState::new(c.drop_ltc_track),
            concat_audio: EditState::new(c.concat_audio),
            set_start_from_ltc: EditState::new(c.set_start_from_ltc),
            embed_camera_metadata: EditState::new(c.embed_camera_metadata),
            ltc_file_idx: EditState::new(c.ltc_file_idx),
            container: EditState::new(c.container.clone()),
            video_encoder: EditState::new(c.video_encoder.clone()),
            audio_encoder: EditState::new(c.audio_encoder.clone()),
            filename_prefix: TextField::new(c.filename_prefix.clone()),
            audio_suffix_template: TextField::new(c.audio_suffix_template.clone()),
            video_suffix_template: TextField::new(c.video_suffix_template.clone()),
        }
    }
}

impl Shadows {
    pub fn new(s: &AppStateSnapshot) -> Self {
        Self {
            roll: TextField::new(s.clapper.roll.clone()),
            auto_increment: EditState::new(s.clapper.auto_increment_take),
            fps_index: EditState::new(s.fps_index),
            sample_rate: EditState::new(
                SAMPLE_RATE_OPTIONS.iter().position(|&r| r == s.sample_rate).unwrap_or(0),
            ),
            ltc_volume: EditState::new(s.ltc_volume),
            beep_volume: EditState::new(s.beep_volume),
            beep_frequency: EditState::new(s.beep_frequency),
            beep_duration: EditState::new(s.beep_duration),
            decode_fps_index: EditState::new(s.decode.fps_index),
            conv: ConvShadows::new(s),
            parent_name: TextField::new(s.offload.parent_name.clone()),
        }
    }
}
