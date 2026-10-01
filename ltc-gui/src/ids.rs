/// ScrollArea id-salt constructors for the ltc-gui.
///
/// Every egui widget that keeps persistent state (scroll offsets, collapsing
/// open/close, …) needs a unique id‑salt to avoid clashing with sibling
/// widgets on the same parent Ui (see egui 0.35 ui.rs:249-257 — sibling
/// child‑Uis without explicit salt share the stable id `parent.with("child")`).
///
/// Salt for the group results ScrollArea in the Convert tab.
pub fn ltc_group_results_scroll() -> &'static str {
    "ltc_group_results_scroll"
}

/// Salt for the per‑clip timecode‑list ScrollArea inside a clip's body.
/// `id_salt` is the per‑clip salt passed to `render_ltc_result`.
pub fn clip_timecodes_scroll(id_salt: &str) -> String {
    format!("ltc_timecodes_scroll_{}", id_salt)
}

/// Salt for the ffmpeg log ScrollArea while a conversion is `Running`.
pub fn ffmpeg_log_running_scroll() -> &'static str {
    "ffmpeg_log_running_scroll"
}

/// Salt for the ffmpeg log ScrollArea when a conversion `Completed`.
pub fn ffmpeg_log_completed_scroll() -> &'static str {
    "ffmpeg_log_completed_scroll"
}

/// Salt for the ffmpeg log ScrollArea when a conversion `Failed`.
pub fn ffmpeg_log_failed_scroll() -> &'static str {
    "ffmpeg_log_failed_scroll"
}

/// Salt for the clap‑log ScrollArea in the Clapper tab.
pub fn clap_log_scroll() -> &'static str {
    "clap_log_scroll"
}

/// Salt for the top‑level ScrollArea that wraps the whole app content.
pub fn root_scroll() -> &'static str {
    "root_scroll"
}

/// Salt for the Debug‑Log window's ScrollArea.
pub fn debug_log_scroll() -> &'static str {
    "debug_log_scroll"
}

/// Salt for the per-card file-list ScrollArea in the Offload tab.
/// Keyed by the card mount path (same per-card key as the device-name
/// shadows in `shadows.rs`)
/// so scroll state follows the card across rescans.
pub fn offload_card_files_scroll(mount: &std::path::Path) -> String {
    format!("offload_card_files_scroll_{}", mount.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// All salts that are ever co‑visible in the same frame must be pairwise
    /// distinct.  Currently the worst‑case set is:
    ///   root_scroll + ltc_group_results_scroll + one ffmpeg‑log salt
    fn distinct_set() -> Vec<String> {
        let mut s: Vec<String> = vec![
            root_scroll().to_string(),
            ltc_group_results_scroll().to_string(),
            ffmpeg_log_running_scroll().to_string(),
            // Per‑clip salts are tested separately in the clip set test.
        ];
        s.sort();
        s.dedup();
        s
    }

    #[test]
    fn all_co_visible_salts_are_distinct() {
        let d = distinct_set();
        // We added 3 elements above.  If dedup shrinks the vec we have a clash.
        assert_eq!(d.len(), 3, "co‑visible salts must be pairwise distinct");
    }

    #[test]
    fn ffmpeg_log_salts_are_distinct_from_each_other() {
        let a = ffmpeg_log_running_scroll().to_string();
        let b = ffmpeg_log_completed_scroll().to_string();
        let c = ffmpeg_log_failed_scroll().to_string();
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
    }

    #[test]
    fn clip_timecodes_salts_differ_across_results() {
        let a = clip_timecodes_scroll("clip_0");
        let b = clip_timecodes_scroll("clip_1");
        let c = clip_timecodes_scroll("single");
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        // All must differ from the group-results scroll too.
        assert_ne!(a, ltc_group_results_scroll());
        assert_ne!(a, ffmpeg_log_running_scroll());
    }

    #[test]
    fn clap_log_salt_differs_from_root_and_group() {
        assert_ne!(clap_log_scroll(), root_scroll());
        assert_ne!(clap_log_scroll(), ltc_group_results_scroll());
    }

    #[test]
    fn debug_log_salt_is_unique() {
        assert_ne!(debug_log_scroll(), root_scroll());
        assert_ne!(debug_log_scroll(), clap_log_scroll());
    }

    #[test]
    fn offload_card_salts_differ_across_cards() {
        use std::path::Path;
        let a = offload_card_files_scroll(Path::new("/media/sda1"));
        let b = offload_card_files_scroll(Path::new("/media/sdb1"));
        let c = offload_card_files_scroll(Path::new("/media/other/sda1"));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        assert_ne!(a, root_scroll());
        assert_ne!(a, clap_log_scroll());
        assert_ne!(a, debug_log_scroll());
        assert_ne!(a, ltc_group_results_scroll());
    }
}