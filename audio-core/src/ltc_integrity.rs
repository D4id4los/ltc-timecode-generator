//! Backend-neutral decoded-value integrity pass (WP-DR).
//!
//! Two pure post-passes over a decoded frame list — no samples, no bits,
//! no builtin internals — so the same logic could later be wired into the
//! libltc backend (further-analysis item, deliberately not wired here):
//!
//! - **DR1 `validate_bcd`** — LTC's BCD digit fields have unused states
//!   (seconds/minutes tens 6–7, hours tens 3+, frame values at or above the
//!   frame rate). A decoded value outside those ranges is *detectably*
//!   corrupt. Policy: reconstruct from coherent neighbours when the frame
//!   sits exactly between them (`prev+1 → suspect → prev+2`), else drop.
//! - **DR2 `repair_single_frame_outliers`** — a single frame whose value is
//!   wrong while both neighbours stay on the `prev+1 / prev+2` grid is
//!   reconstructed to `prev+1`. Segment boundaries and forward jumps (edit
//!   points) are never touched; two consecutive corrupt frames are left
//!   alone (no safe evidence).
//!
//! Confidence math is deliberately untouched: `valid_frames` /
//! `avg_confidence` keep counting sync-matched frames; these passes only
//! mutate the `timecodes` vector (Decision B of the WP-DR plan).

use serde::{Deserialize, Serialize};

use crate::ltc_decoder::{FrameTimecode, LtcDetectionResult};
use crate::{increment_timecode, Timecode};

/// Counts of value-integrity mutations applied to a decoded frame list.
/// Typed on purpose (no text pins): tests and callers assert the struct.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepairStats {
    /// Frames dropped because their value violates BCD ranges and no
    /// coherent-neighbour context allowed a safe reconstruction.
    pub bcd_dropped: u32,
    /// BCD-invalid frames reconstructed as `prev + 1` from coherent
    /// neighbours.
    pub bcd_repaired: u32,
    /// Value-valid frames whose single-frame outlier shape
    /// (`prev+1 → suspect → prev+2`) allowed a continuity repair.
    pub outliers_repaired: u32,
}

impl RepairStats {
    /// Whether any mutation was applied.
    pub fn any(self) -> bool {
        self != Self::default()
    }
}

/// BCD/range plausibility of a decoded composite timecode under the assumed
/// frame rate. Only range checks on the decoded values (Decision A) — no
/// bits needed. A units digit that overflowed past 9 is detectable exactly
/// when it pushes the composite out of the valid range (e.g. a seconds
/// digit pair `5, 12` decodes as 62 ≥ 60).
pub(crate) fn bcd_value_valid(tc: &Timecode, fps: f64) -> bool {
    if fps <= 0.0 {
        return true;
    }
    let max_frames = (fps.ceil() as u32).max(1);
    tc.frames < max_frames && tc.seconds < 60 && tc.minutes < 60 && tc.hours < 24
}

/// `prev` advanced two frames on the locked grid.
fn two_after(prev: &Timecode, fps: f64, drop_frame: bool) -> Timecode {
    increment_timecode(&increment_timecode(prev, fps, drop_frame), fps, drop_frame)
}

/// DR1 — drop or neighbour-repair frames whose decoded value violates BCD
/// ranges. Frames are renumbered contiguously after mutation (matching
/// `from_frame_starts` semantics).
pub(crate) fn validate_bcd(
    tcs: &mut Vec<FrameTimecode>,
    fps: f64,
    drop_frame: bool,
) -> RepairStats {
    let mut stats = RepairStats::default();
    let mut kept: Vec<FrameTimecode> = Vec::with_capacity(tcs.len());
    for pos in 0..tcs.len() {
        let suspect = tcs[pos].clone();
        if bcd_value_valid(&suspect.timecode, fps) {
            kept.push(suspect);
            continue;
        }
        // Repair from context when the suspect sits exactly between two
        // value-valid neighbours on the locked grid: prev+1 → suspect →
        // prev+2. Otherwise dropping is the only safe move.
        let next = tcs.get(pos + 1);
        let repaired = match (kept.last(), next) {
            (Some(prev), Some(next))
                if bcd_value_valid(&prev.timecode, fps)
                    && bcd_value_valid(&next.timecode, fps) =>
            {
                let expected = increment_timecode(&prev.timecode, fps, drop_frame);
                (next.timecode == two_after(&prev.timecode, fps, drop_frame)).then_some(expected)
            }
            _ => None,
        };
        match repaired {
            Some(tc) => {
                stats.bcd_repaired += 1;
                let mut ftc = suspect;
                ftc.timecode = tc;
                kept.push(ftc);
            }
            None => stats.bcd_dropped += 1,
        }
    }
    for (i, ftc) in kept.iter_mut().enumerate() {
        ftc.frame_index = i as u32;
    }
    *tcs = kept;
    stats
}

/// DR2 — repair single-frame value outliers inside audio-contiguous
/// segments. Never touches segment boundaries (dropouts) or edit points
/// (forward jumps): same conservatism as the quality scorer.
pub(crate) fn repair_single_frame_outliers(
    tcs: &mut Vec<FrameTimecode>,
    fps: f64,
    drop_frame: bool,
) -> RepairStats {
    let mut stats = RepairStats::default();
    if tcs.len() < 3 || fps <= 0.0 {
        return stats;
    }
    // Segment on audio positions: frames on both sides of a dropout (audio
    // gap) or an edit must never be pulled onto one grid.
    let audio_secs: Vec<f64> = tcs.iter().map(|t| t.timecode_secs).collect();
    for seg in crate::ltc_decoder::split_segments(&audio_secs, fps) {
        if seg.end - seg.start < 3 {
            continue;
        }
        for i in seg.start + 1..seg.end - 1 {
            let expected = increment_timecode(&tcs[i - 1].timecode, fps, drop_frame);
            let expected_next = increment_timecode(&expected, fps, drop_frame);
            // Shape 1: prev+1 → suspect → prev+2 (the classic single-frame
            // outlier). Shape 2: the neighbours are already consecutive
            // with *each other* (prev+1, X, prev+1) — X cannot sit on the
            // grid, so reconstruct it as prev+1 too (a duplicate, which
            // downstream consumers already tolerate at corruption
            // boundaries). Both leave segment boundaries and edit jumps
            // (where next is far from prev) untouched.
            if tcs[i].timecode != expected
                && (tcs[i + 1].timecode == expected_next || tcs[i + 1].timecode == expected)
            {
                tcs[i].timecode = expected;
                stats.outliers_repaired += 1;
            }
        }
    }
    // Trailing frames that break continuity with their predecessor and
    // have no successor to establish an edit are unverifiable — drop
    // them. A single frame after a jump is useless for sync anyway.
    // Counted under `bcd_dropped` (value-integrity drops).
    while tcs.len() >= 2 {
        let last = tcs.len() - 1;
        let expected = increment_timecode(&tcs[last - 1].timecode, fps, drop_frame);
        if tcs[last].timecode == expected {
            break;
        }
        tcs.pop();
        stats.bcd_dropped += 1;
    }
    stats
}

/// DR1 + DR2 combined, with the human-rendered details line appended to
/// `result.details` exactly once when anything was repaired or dropped.
pub(crate) fn apply_value_integrity(result: &mut LtcDetectionResult) {
    if result.timecodes.is_empty() || result.detected_fps <= 0.0 {
        return;
    }
    let fps = result.detected_fps as f64;
    let mut stats = validate_bcd(&mut result.timecodes, fps, result.drop_frame);
    stats.outliers_repaired +=
        repair_single_frame_outliers(&mut result.timecodes, fps, result.drop_frame)
            .outliers_repaired;
    if stats.any() {
        result.details.push(format!(
            "Integrity: {} BCD-invalid frame(s) dropped, {} repaired from neighbours, {} continuity outlier(s) repaired",
            stats.bcd_dropped, stats.bcd_repaired, stats.outliers_repaired
        ));
    }
    // If every sync-matched frame failed BCD validation, there are no
    // usable values left — the decode must not claim Success on an empty
    // value set (periodic non-LTC content can sync-match without decoding).
    if result.timecodes.is_empty()
        && !matches!(result.status, crate::ltc_decoder::LtcDecodeStatus::Error { .. })
    {
        result.status = crate::ltc_decoder::LtcDecodeStatus::NoSyncWord;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: f64 = 25.0;

    fn ftc(frame_index: u32, tc: Timecode, secs: f64) -> FrameTimecode {
        FrameTimecode { frame_index, timecode: tc, timecode_secs: secs }
    }

    fn tc(h: u32, m: u32, s: u32, f: u32) -> Timecode {
        Timecode { hours: h, minutes: m, seconds: s, frames: f }
    }

    /// A clean ascending run starting at 01:00:00:00, one frame per
    /// `1/FPS` of audio.
    fn clean_run(n: u32) -> Vec<FrameTimecode> {
        let mut tc = tc(1, 0, 0, 0);
        (0..n)
            .map(|i| {
                let f = ftc(i, tc, i as f64 / FPS);
                tc = increment_timecode(&tc, FPS, false);
                f
            })
            .collect()
    }

    // ── bcd_value_valid ──────────────────────────────────────────────

    #[test]
    fn test_bcd_rejects_units_above_nine() {
        // A units digit that overflowed past 9 is detectable when it pushes
        // the composite out of range: tens=5 + overflowed units (10–15)
        // lands at 60–75 seconds/minutes; tens=2 hours lands at 30–39;
        // frame tens=2 + overflowed units lands at ≥ 30 ≥ fps.
        assert!(!bcd_value_valid(&tc(0, 0, 62, 0), FPS));
        assert!(!bcd_value_valid(&tc(0, 64, 0, 0), FPS));
        assert!(!bcd_value_valid(&tc(33, 0, 0, 0), FPS));
        assert!(!bcd_value_valid(&tc(0, 0, 0, 33), FPS));
    }

    #[test]
    fn test_bcd_rejects_tens_overruns() {
        // Sec/min tens digits 6–7 and hour tens 3+ are unused BCD states.
        for s in [60u32, 69, 70, 79] {
            assert!(!bcd_value_valid(&tc(0, 0, s, 0), FPS), "seconds {s}");
        }
        for m in [60u32, 77] {
            assert!(!bcd_value_valid(&tc(0, m, 0, 0), FPS), "minutes {m}");
        }
        for h in [24u32, 30, 99] {
            assert!(!bcd_value_valid(&tc(h, 0, 0, 0), FPS), "hours {h}");
        }
    }

    #[test]
    fn test_bcd_rejects_frames_at_or_above_fps() {
        assert!(!bcd_value_valid(&tc(0, 0, 0, 25), FPS));
        assert!(!bcd_value_valid(&tc(0, 0, 0, 30), FPS));
        assert!(bcd_value_valid(&tc(0, 0, 0, 24), FPS));
        // 29.97 DF runs to frame 29 under a 29.97 assumption (ceil = 30).
        assert!(bcd_value_valid(&tc(0, 0, 0, 29), 29.97));
        assert!(!bcd_value_valid(&tc(0, 0, 0, 30), 29.97));
    }

    #[test]
    fn test_bcd_accepts_all_valid_values() {
        // Floor sweep over the valid boundaries, not an enumeration pin.
        for f in [0u32, 11, 24] {
            assert!(bcd_value_valid(&tc(0, 0, 0, f), FPS));
        }
        for s in [0u32, 19, 59] {
            assert!(bcd_value_valid(&tc(0, 0, s, 0), FPS));
        }
        for m in [0u32, 45, 59] {
            assert!(bcd_value_valid(&tc(0, m, 0, 0), FPS));
        }
        for h in [0u32, 12, 23] {
            assert!(bcd_value_valid(&tc(h, 0, 0, 0), FPS));
        }
    }

    // ── validate_bcd ─────────────────────────────────────────────────

    #[test]
    fn test_validate_bcd_drops_garbage_without_neighbours() {
        let mut tcs = clean_run(5);
        // Garbage at the tail: no next neighbour, so no safe context.
        tcs[4].timecode = tc(1, 0, 0, 30); // frames ≥ fps
        let stats = validate_bcd(&mut tcs, FPS, false);
        assert_eq!(stats.bcd_dropped, 1);
        assert_eq!(stats.bcd_repaired, 0);
        assert_eq!(tcs.len(), 4);
        // Renumbered contiguously.
        assert_eq!(tcs[3].frame_index, 3);
        // Surviving values untouched.
        assert_eq!(tcs[2].timecode, tc(1, 0, 0, 2));

        // A mid-run frame with a *non-coherent* next neighbour is dropped
        // too: reconstruction needs the exact prev+1 → suspect → prev+2
        // shape, otherwise the "repair" would be guesswork.
        let mut tcs = clean_run(6);
        tcs[2].timecode = tc(1, 0, 0, 30);
        tcs[3].timecode = tc(1, 0, 70, 0); // also invalid → no context
        let stats = validate_bcd(&mut tcs, FPS, false);
        assert_eq!(stats.bcd_dropped, 2);
        assert_eq!(tcs.len(), 4);
    }

    #[test]
    fn test_validate_bcd_repairs_between_coherent_neighbours() {
        let mut tcs = clean_run(5);
        // A garbage value (frame tens overflow: 2 → 25+) between coherent
        // neighbours reconstructs as prev + 1.
        tcs[2].timecode = tc(1, 0, 0, 30);
        let stats = validate_bcd(&mut tcs, FPS, false);
        assert_eq!(stats.bcd_repaired, 1);
        assert_eq!(stats.bcd_dropped, 0);
        assert_eq!(tcs.len(), 5);
        assert_eq!(tcs[2].timecode, tc(1, 0, 0, 2));
        // Frame indices contiguous after repair (nothing dropped).
        assert_eq!(tcs[4].frame_index, 4);
    }

    #[test]
    fn test_validate_bcd_keeps_clean_input_untouched() {
        let mut tcs = clean_run(50);
        let before = tcs.clone();
        let stats = validate_bcd(&mut tcs, FPS, false);
        assert_eq!(stats, RepairStats::default());
        assert_eq!(tcs, before);
    }

    // ── repair_single_frame_outliers ─────────────────────────────────

    fn assert_stats(stats: RepairStats, outliers: u32) {
        assert_eq!(stats.outliers_repaired, outliers, "outliers repaired");
        assert_eq!(stats.bcd_dropped, 0);
        assert_eq!(stats.bcd_repaired, 0);
    }

    #[test]
    fn test_outlier_between_coherent_neighbours_is_repaired() {
        let mut tcs = clean_run(10);
        // …:04, GARBAGE (2 frames off, still BCD-valid), :06 … → middle
        // becomes :05.
        tcs[5].timecode = tc(1, 0, 0, 7);
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_stats(stats, 1);
        assert_eq!(tcs[5].timecode, tc(1, 0, 0, 5));
    }

    #[test]
    fn test_trailing_corrupt_frame_dropped() {
        let mut tcs = clean_run(10);
        // Garbage on the final frame: no successor can vouch for an edit,
        // so the unverifiable value is dropped instead of reported.
        tcs[9].timecode = tc(0, 0, 0, 24);
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_eq!(stats.bcd_dropped, 1, "trailing unverifiable frame dropped");
        assert_eq!(tcs.len(), 9);
        // A consistent tail is never touched.
        let mut tcs = clean_run(10);
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_eq!(stats, RepairStats::default());
        assert_eq!(tcs.len(), 10);
    }

    #[test]
    fn test_outlier_at_segment_boundary_not_touched() {
        let mut tcs = clean_run(10);
        // The first frame has no left neighbour, so no repair context.
        // (The last frame is covered by the trailing-drop rule — see
        // test_trailing_corrupt_frame_dropped.)
        tcs[0].timecode = tc(1, 0, 0, 4);
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_stats(stats, 0);
        assert_eq!(tcs[0].timecode, tc(1, 0, 0, 4));
        assert_eq!(tcs.len(), 10);
    }

    #[test]
    fn test_outlier_with_consecutive_neighbours_repaired_to_duplicate() {
        let mut tcs = clean_run(10);
        // 4, GARBAGE (24), 5: the neighbours are already consecutive with
        // each other, so the suspect cannot be on the grid — reconstructed
        // as prev+1 (a tolerated duplicate). The repair cascades one frame
        // further because the next neighbour pair then has the classic
        // outlier shape.
        tcs[5].timecode = tc(1, 0, 0, 24);
        tcs[6].timecode = tc(1, 0, 0, 5);
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_stats(stats, 2);
        assert_eq!(tcs[5].timecode, tc(1, 0, 0, 5));
        assert_eq!(tcs[6].timecode, tc(1, 0, 0, 6));
    }

    #[test]
    fn test_two_consecutive_corrupt_frames_left_alone() {
        let mut tcs = clean_run(10);
        // Two frames off the grid: no single safe reconstruction exists.
        tcs[4].timecode = tc(1, 0, 0, 10);
        tcs[5].timecode = tc(1, 0, 0, 11);
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_stats(stats, 0);
        assert_eq!(tcs[4].timecode, tc(1, 0, 0, 10));
        assert_eq!(tcs[5].timecode, tc(1, 0, 0, 11));
    }

    #[test]
    fn test_forward_edit_jump_not_repaired() {
        let mut tcs = clean_run(10);
        // Re-jam: +100 frames is a legitimate edit point; the frames on
        // both sides of the jump are individually correct.
        let mut cursor = tc(1, 0, 4, 4); // clean_run(4) + a 100-frame re-jam
        for (i, t) in tcs.iter_mut().enumerate().skip(5) {
            cursor = increment_timecode(&cursor, FPS, false);
            t.timecode = cursor;
            t.frame_index = i as u32;
        }
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_stats(stats, 0);
        assert_eq!(tcs[5].timecode, tc(1, 0, 4, 5));
    }

    #[test]
    fn test_backward_jump_not_repaired() {
        let mut tcs = clean_run(10);
        // TC reset at index 5 (backward jump): nothing inside the run has
        // the outlier shape.
        let mut cursor = tc(0, 59, 59, 20);
        for (i, t) in tcs.iter_mut().enumerate().skip(5) {
            cursor = increment_timecode(&cursor, FPS, false);
            t.timecode = cursor;
            t.frame_index = i as u32;
        }
        let stats = repair_single_frame_outliers(&mut tcs, FPS, false);
        assert_stats(stats, 0);
        assert_eq!(tcs[5].timecode, tc(0, 59, 59, 21));
    }

    // ── apply_value_integrity ────────────────────────────────────────

    fn detection_result(tcs: Vec<FrameTimecode>) -> LtcDetectionResult {
        LtcDetectionResult {
            status: crate::ltc_decoder::LtcDecodeStatus::Success,
            detected_fps: FPS as f32,
            drop_frame: false,
            total_possible_frames: tcs.len() as u32,
            valid_frames: tcs.len() as u32,
            timecodes: tcs,
            avg_confidence: 1.0,
            details: Vec::new(),
            total_audio_duration_secs: 2.0,
            sample_rate: 48000,
            processing_time_ms: 0.0,
            first_ltc_timecode_secs: 1.0,
            quality: None,
            chunk_summaries: Vec::new(),
        }
    }

    #[test]
    fn test_apply_value_integrity_repairs_and_logs_once() {
        let mut r = detection_result(clean_run(10));
        r.timecodes[3].timecode = tc(1, 0, 0, 40); // BCD-invalid, repaired
        r.timecodes[7].timecode = tc(1, 0, 0, 15); // value-valid outlier
        apply_value_integrity(&mut r);
        assert_eq!(r.timecodes[3].timecode, tc(1, 0, 0, 3));
        assert_eq!(r.timecodes[7].timecode, tc(1, 0, 0, 7));
        assert_eq!(r.details.len(), 1, "one details line for the pass");
    }

    #[test]
    fn test_apply_value_integrity_clean_result_untouched() {
        let mut r = detection_result(clean_run(10));
        let before_details = 0usize;
        apply_value_integrity(&mut r);
        assert_eq!(r.details.len(), before_details);
        assert_eq!(r.timecodes.len(), 10);
    }
}
