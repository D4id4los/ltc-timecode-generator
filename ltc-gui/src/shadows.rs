//! GUI-local shadow state for every interactive widget, bound to an engine
//! truth via [`gui_engine::edit_state::EditState`].
//!
//! Widgets render from these shadows (never directly from the snapshot) and
//! adopt the engine truth only when unfocused and no unconfirmed send is in
//! flight — see `widgets/bound.rs` for the wrapper functions that enforce
//! that contract. Engine-side side-effects (capability repairs, decode
//! auto-apply, recording-selection resets, probe resizes) propagate through
//! `EditState::sync` because the shadows are unfocused whenever the affected
//! widget is not being edited.

use std::collections::HashMap;
use std::path::PathBuf;

use gui_engine::edit_state::EditState;
use gui_engine::state::AppStateSnapshot;
use gui_engine::ChannelSel;

use gui_engine::converter::ChannelMap;

/// Shadow for every user-editable converter setting (mirrors
/// `ConverterUserSettings` field-for-field, minus the engine-internal
/// `output_folder_user_set` flag).
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
    pub channel_map: EditState<ChannelMap>,
    pub container: EditState<String>,
    pub video_encoder: EditState<String>,
    pub audio_encoder: EditState<String>,
    pub output_folder: EditState<PathBuf>,
    pub filename_prefix: EditState<String>,
    pub audio_suffix_template: EditState<String>,
    pub video_suffix_template: EditState<String>,
}

/// All GUI-local shadows, keyed by the widget/field they back.
#[derive(Debug)]
pub struct Shadows {
    // Clapper
    pub roll: EditState<String>,
    pub auto_increment: EditState<bool>,

    // Settings
    pub ltc_volume: EditState<f32>,
    pub beep_volume: EditState<f32>,
    pub beep_frequency: EditState<f32>,
    pub beep_duration: EditState<f32>,
    pub ltc_channel: EditState<ChannelSel>,
    pub beep_channel: EditState<ChannelSel>,
    pub sample_rate: EditState<u32>,
    pub selected_device: EditState<Option<String>>,

    // Decode (LTC verification section)
    pub decode_fps_index: EditState<usize>,
    pub decode_stream: EditState<usize>,
    pub decode_channel: EditState<usize>,

    // Converter
    pub conv: ConvShadows,

    // Offload
    pub parent_name: EditState<String>,
    /// Device-folder name buffers keyed by mount point (stable across
    /// rescans, unlike card list indices).
    pub device_names: HashMap<PathBuf, EditState<String>>,
    /// Per-file selection buffers keyed by `(mount, file path)`.
    pub file_selection: HashMap<(PathBuf, PathBuf), EditState<bool>>,
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
            channel_map: EditState::new(c.channel_map.clone()),
            container: EditState::new(c.container.clone()),
            video_encoder: EditState::new(c.video_encoder.clone()),
            audio_encoder: EditState::new(c.audio_encoder.clone()),
            output_folder: EditState::new(c.output_folder.clone()),
            filename_prefix: EditState::new(c.filename_prefix.clone()),
            audio_suffix_template: EditState::new(c.audio_suffix_template.clone()),
            video_suffix_template: EditState::new(c.video_suffix_template.clone()),
        }
    }
}

impl Shadows {
    pub fn new(s: &AppStateSnapshot) -> Self {
        Self {
            roll: EditState::new(s.clapper.roll.clone()),
            auto_increment: EditState::new(s.clapper.auto_increment_take),
            ltc_volume: EditState::new(s.ltc_volume),
            beep_volume: EditState::new(s.beep_volume),
            beep_frequency: EditState::new(s.beep_frequency),
            beep_duration: EditState::new(s.beep_duration),
            ltc_channel: EditState::new(s.ltc_channel),
            beep_channel: EditState::new(s.beep_channel),
            sample_rate: EditState::new(s.sample_rate),
            selected_device: EditState::new(s.selected_device.clone()),
            decode_fps_index: EditState::new(s.decode.fps_index),
            decode_stream: EditState::new(s.decode.selected_stream),
            decode_channel: EditState::new(s.decode.selected_channel),
            conv: ConvShadows::new(s),
            parent_name: EditState::new(s.offload.parent_name.clone()),
            device_names: HashMap::new(),
            file_selection: HashMap::new(),
        }
    }
}
