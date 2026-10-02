use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use log::warn;

use audio_core::{FrameTimecode, Timecode};

#[derive(Clone, Debug)]
pub struct TimecodeMetadata {
    pub start: Timecode,
    pub fps: f64,
    pub drop_frame: bool,
}

/// Scan RIFF chunks (starting after the `RIFF`/`WAVE` header) for the `fmt `
/// chunk and return its sample rate.
///
/// Chunk sizes are little-endian per the RIFF spec; odd-sized chunks carry one
/// padding byte that must be skipped, so WAVs with `JUNK`/`LIST`/`bext` chunks
/// before `fmt ` (as written by many recorders) are handled correctly.
pub fn read_wav_sample_rate_from_file(file: &mut std::fs::File) -> Option<u32> {
    let file_len = file.seek(SeekFrom::End(0)).ok()?;
    file.seek(SeekFrom::Start(12)).ok()?;
    let mut offset: u64 = 12;
    while offset + 8 <= file_len {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut hdr = [0u8; 8];
        file.read_exact(&mut hdr).ok()?;
        let chunk_size = u32::from_le_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;
        if &hdr[0..4] == b"fmt " {
            if chunk_size < 16 {
                return None;
            }
            // fmt payload: format_tag(2) | channels(2) | sample_rate(4) | ...
            let mut fmt_data = [0u8; 8];
            file.read_exact(&mut fmt_data).ok()?;
            return Some(u32::from_le_bytes([
                fmt_data[4], fmt_data[5], fmt_data[6], fmt_data[7],
            ]));
        }
        offset += 8 + chunk_size;
        if chunk_size % 2 != 0 {
            offset += 1;
        }
    }
    None
}

pub fn read_wav_sample_rate(path: &Path) -> Option<u32> {
    let mut file = std::fs::File::open(path).ok()?;
    read_wav_sample_rate_from_file(&mut file)
}

pub fn format_ffmpeg_timecode(tc: &Timecode, drop_frame: bool) -> String {
    let frame_sep = if drop_frame { ";" } else { ":" };
    format!(
        "{:02}:{:02}:{:02}{}{:02}",
        tc.hours, tc.minutes, tc.seconds, frame_sep, tc.frames
    )
}

fn decrement_timecode_frame(tc: &Timecode, fps: f64, drop_frame: bool) -> Timecode {
    let max_frames = fps.ceil() as u32;
    let mut h = tc.hours;
    let mut m = tc.minutes;
    let mut s = tc.seconds;
    let mut f = tc.frames;

    if drop_frame && s == 0 && m % 10 != 0 && f <= 1 {
        if m > 0 {
            m -= 1;
        } else {
            m = 59;
            h = if h == 0 { 23 } else { h - 1 };
        }
        return Timecode { hours: h, minutes: m, seconds: 59, frames: max_frames - 1 };
    }

    if f > 0 {
        f -= 1;
    } else if s > 0 {
        s -= 1;
        f = max_frames - 1;
    } else {
        if m > 0 {
            m -= 1;
        } else {
            m = 59;
            h = if h == 0 { 23 } else { h - 1 };
        }
        s = 59;
        f = max_frames - 1;
    }

    Timecode { hours: h, minutes: m, seconds: s, frames: f }
}

pub fn shift_timecode_back(tc: &Timecode, delta_secs: f64, fps: f64, drop_frame: bool) -> Timecode {
    let mut out = *tc;
    if delta_secs <= 0.0 || fps <= 0.0 {
        return out;
    }
    let mut frames = (delta_secs * fps).round() as u64;
    while frames > 0 {
        out = decrement_timecode_frame(&out, fps, drop_frame);
        frames -= 1;
    }
    out
}

pub fn start_timecode_from_ltc(result: &audio_core::LtcDetectionResult) -> Option<TimecodeMetadata> {
    use audio_core::LtcDecodeStatus;
    if !matches!(result.status, LtcDecodeStatus::Success | LtcDecodeStatus::LowConfidence) {
        return None;
    }
    if result.timecodes.is_empty() || result.detected_fps <= 0.0 {
        return None;
    }
    let fps = result.detected_fps as f64;
    let first = &result.timecodes[0];
    let offset = if result.first_ltc_timecode_secs > 0.0 {
        result.first_ltc_timecode_secs
    } else {
        first.timecode_secs
    };
    let secure = audio_core::find_first_coherent_index(
        &result.timecodes, fps, result.drop_frame,
    ) == Some(0);
    if !secure {
        warn!(
            "start_timecode_from_ltc: first timecode at {:.3}s is not part of a secure coherent \
             run (>=2s); using best-effort",
            offset,
        );
    }
    let start = shift_timecode_back(&first.timecode, offset, fps, result.drop_frame);
    Some(TimecodeMetadata { start, fps, drop_frame: result.drop_frame })
}

pub fn build_per_file_start_timecodes(
    results: &[Option<&audio_core::LtcDetectionResult>],
) -> Vec<Option<TimecodeMetadata>> {
    results.iter().map(|r| {
        r.and_then(start_timecode_from_ltc)
    }).collect()
}

pub fn build_per_file_trim_and_timecode(
    results: &[Option<&audio_core::LtcDetectionResult>],
) -> (Vec<f64>, Vec<Option<TimecodeMetadata>>) {
    let mut trims = Vec::with_capacity(results.len());
    let mut metas = Vec::with_capacity(results.len());

    for result_opt in results {
        match result_opt {
            Some(r) if matches!(r.status, audio_core::LtcDecodeStatus::Success | audio_core::LtcDecodeStatus::LowConfidence) => {
                let trim = r.first_ltc_timecode_secs;
                let meta = find_timecode_at_offset(&r.timecodes, trim).map(|tc| TimecodeMetadata {
                    start: tc,
                    fps: r.detected_fps as f64,
                    drop_frame: r.drop_frame,
                });
                trims.push(trim);
                metas.push(meta);
            }
            _ => {
                trims.push(0.0);
                metas.push(None);
            }
        }
    }

    (trims, metas)
}

/// Parse a native camera timecode string `HH:MM:SS:FF` into a `Timecode`.
/// Returns `None` for unparseable strings.
pub fn parse_native_timecode(s: &str) -> Option<Timecode> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 4 {
        return None;
    }
    let hours = parts[0].parse::<u8>().ok()?;
    let minutes = parts[1].parse::<u8>().ok()?;
    let seconds = parts[2].parse::<u8>().ok()?;
    let frames = parts[3].parse::<u8>().ok()?;
    if hours >= 24 || minutes >= 60 || seconds >= 60 || frames >= 60 {
        return None;
    }
    Some(Timecode { hours: hours as u32, minutes: minutes as u32, seconds: seconds as u32, frames: frames as u32 })
}

pub fn find_timecode_at_offset(
    timecodes: &[FrameTimecode],
    offset_secs: f64,
) -> Option<Timecode> {
    if timecodes.is_empty() {
        return None;
    }
    let idx = timecodes.binary_search_by(|ft| {
        ft.timecode_secs
            .partial_cmp(&offset_secs)
            .unwrap_or(std::cmp::Ordering::Greater)
    });
    let i = match idx {
        Ok(i) => i,
        Err(i) => i.min(timecodes.len() - 1),
    };
    Some(timecodes[i].timecode)
}

pub fn time_reference_samples(tc: &TimecodeMetadata, sample_rate: u32) -> u64 {
    let total_secs = tc.start.hours as f64 * 3600.0
        + tc.start.minutes as f64 * 60.0
        + tc.start.seconds as f64
        + tc.start.frames as f64 / tc.fps;
    (total_secs * sample_rate as f64).round() as u64
}

#[cfg(test)]
mod tests {
    use audio_core::{FrameTimecode, LtcDecodeStatus, Timecode};
    use super::*;

    fn make_test_result(timecodes: Vec<FrameTimecode>, fps: f32, first_secs: f64, status: audio_core::LtcDecodeStatus) -> audio_core::LtcDetectionResult {
        audio_core::LtcDetectionResult {
            status,
            detected_fps: fps,
            drop_frame: false,
            total_possible_frames: timecodes.len() as u32,
            valid_frames: timecodes.len() as u32,
            timecodes,
            avg_confidence: 1.0,
            details: vec![],
            total_audio_duration_secs: 10.0,
            sample_rate: 48000,
            processing_time_ms: 0.0,
            first_ltc_timecode_secs: first_secs,
            quality: None,
        }
    }

    #[test]
    fn test_format_ffmpeg_timecode_non_drop() {
        let tc = Timecode { hours: 1, minutes: 23, seconds: 45, frames: 16 };
        assert_eq!(format_ffmpeg_timecode(&tc, false), "01:23:45:16");
    }

    #[test]
    fn test_format_ffmpeg_timecode_drop() {
        let tc = Timecode { hours: 23, minutes: 59, seconds: 59, frames: 29 };
        assert_eq!(format_ffmpeg_timecode(&tc, true), "23:59:59;29");
    }

    #[test]
    fn test_format_ffmpeg_timecode_zero() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 };
        assert_eq!(format_ffmpeg_timecode(&tc, false), "00:00:00:00");
        assert_eq!(format_ffmpeg_timecode(&tc, true), "00:00:00;00");
    }

    #[test]
    fn test_find_timecode_at_offset_exact() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 };
        let tcs = vec![
            FrameTimecode { frame_index: 0, timecode: tc, timecode_secs: 0.0 },
            FrameTimecode { frame_index: 25, timecode: Timecode { hours: 1, minutes: 0, seconds: 1, frames: 0 }, timecode_secs: 1.0 },
            FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 2, frames: 0 }, timecode_secs: 2.0 },
        ];
        let found = find_timecode_at_offset(&tcs, 1.0).unwrap();
        assert_eq!(found, Timecode { hours: 1, minutes: 0, seconds: 1, frames: 0 });
    }

    #[test]
    fn test_find_timecode_at_offset_closest() {
        let tcs = vec![
            FrameTimecode { frame_index: 0, timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 0.0 },
            FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 2, frames: 0 }, timecode_secs: 2.0 },
        ];
        let found = find_timecode_at_offset(&tcs, 1.5).unwrap();
        assert_eq!(found, Timecode { hours: 1, minutes: 0, seconds: 2, frames: 0 });
    }

    #[test]
    fn test_find_timecode_at_offset_empty() {
        assert!(find_timecode_at_offset(&[], 1.0).is_none());
    }

    #[test]
    fn test_find_timecode_at_offset_before_first() {
        let tcs = vec![
            FrameTimecode { frame_index: 125, timecode: Timecode { hours: 1, minutes: 0, seconds: 5, frames: 0 }, timecode_secs: 5.0 },
        ];
        let found = find_timecode_at_offset(&tcs, 0.0).unwrap();
        assert_eq!(found, Timecode { hours: 1, minutes: 0, seconds: 5, frames: 0 });
    }

    #[test]
    fn test_build_per_file_trim_and_timecode_all_success() {
        let r = make_test_result(
            vec![FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 2.0 }],
            25.0, 2.0, LtcDecodeStatus::Success,
        );
        let (trims, metas) = build_per_file_trim_and_timecode(&[Some(&r)]);
        assert!((trims[0] - 2.0).abs() < 0.001);
        assert!(metas[0].is_some());
    }

    #[test]
    fn test_build_per_file_trim_and_timecode_with_failures() {
        let (trims, metas) = build_per_file_trim_and_timecode(&[None, None]);
        assert!((trims[0]).abs() < 0.001);
        assert!(metas[0].is_none());
        assert!((trims[1]).abs() < 0.001);
        assert!(metas[1].is_none());
    }

    #[test]
    fn test_build_per_file_trim_and_timecode_empty() {
        let (trims, metas) = build_per_file_trim_and_timecode(&[]);
        assert!(trims.is_empty());
        assert!(metas.is_empty());
    }

    #[test]
    fn test_shift_timecode_back_ndf() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 5, frames: 0 };
        let shifted = shift_timecode_back(&tc, 2.0, 25.0, false);
        assert_eq!(shifted, Timecode { hours: 1, minutes: 0, seconds: 3, frames: 0 });
    }

    #[test]
    fn test_shift_timecode_back_zero_is_identity() {
        let tc = Timecode { hours: 1, minutes: 0, seconds: 5, frames: 0 };
        let shifted = shift_timecode_back(&tc, 0.0, 25.0, false);
        assert_eq!(shifted, tc);
    }

    #[test]
    fn test_shift_timecode_back_drop_frame_skips_nonexistent() {
        let tc = Timecode { hours: 1, minutes: 1, seconds: 0, frames: 2 };
        let shifted = shift_timecode_back(&tc, 2.0 / 29.97, 29.97, true);
        assert_eq!(shifted, Timecode { hours: 1, minutes: 0, seconds: 59, frames: 29 });
    }

    #[test]
    fn test_shift_timecode_back_wraps_midnight() {
        let tc = Timecode { hours: 0, minutes: 0, seconds: 0, frames: 1 };
        let shifted = shift_timecode_back(&tc, 2.0 / 25.0, 25.0, false);
        assert_eq!(shifted, Timecode { hours: 23, minutes: 59, seconds: 59, frames: 24 });
    }

    #[test]
    fn test_shift_timecode_back_roundtrip_with_increment() {
        let start = Timecode { hours: 10, minutes: 30, seconds: 15, frames: 12 };
        let mut advanced = start;
        for _ in 0..5 {
            advanced = audio_core::increment_timecode(&advanced, 25.0, false);
        }
        let back = shift_timecode_back(&advanced, 5.0 / 25.0, 25.0, false);
        assert_eq!(back, start);
    }

    #[test]
    fn test_start_timecode_from_ltc_ndf() {
        let r = make_test_result(
            vec![FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 2.0 }],
            25.0, 2.0, LtcDecodeStatus::Success,
        );
        let meta = start_timecode_from_ltc(&r);
        assert!(meta.is_some());
        let meta = meta.unwrap();
        assert!((meta.fps - 25.0).abs() < 0.001);
        assert!(!meta.drop_frame);
    }

    #[test]
    fn test_start_timecode_from_ltc_wraps_midnight() {
        let r = make_test_result(
            vec![FrameTimecode { frame_index: 50, timecode: Timecode { hours: 0, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 2.0 }],
            25.0, 2.0, LtcDecodeStatus::Success,
        );
        assert!(start_timecode_from_ltc(&r).is_some());
    }

    #[test]
    fn test_start_timecode_from_ltc_none_failed_status() {
        let r = make_test_result(vec![], 0.0, 0.0, LtcDecodeStatus::Error { message: "no signal".into() });
        assert!(start_timecode_from_ltc(&r).is_none());
    }

    #[test]
    fn test_start_timecode_from_ltc_none_empty_timecodes() {
        let r = make_test_result(vec![], 25.0, 0.0, LtcDecodeStatus::Success);
        assert!(start_timecode_from_ltc(&r).is_none());
    }

    #[test]
    fn test_start_timecode_from_ltc_none_nosync() {
        let r = make_test_result(vec![], 0.0, 0.0, LtcDecodeStatus::NoSyncWord);
        assert!(start_timecode_from_ltc(&r).is_none());
    }

    #[test]
    fn test_start_timecode_from_ltc_low_confidence() {
        let mut r = make_test_result(
            vec![FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 2.0 }],
            25.0, 2.0, LtcDecodeStatus::LowConfidence,
        );
        r.avg_confidence = 0.3;
        let meta = start_timecode_from_ltc(&r);
        assert!(meta.is_some(), "LowConfidence should still produce metadata");
    }

    #[test]
    fn test_build_per_file_start_timecodes_all_success() {
        let r = make_test_result(
            vec![FrameTimecode { frame_index: 50, timecode: Timecode { hours: 1, minutes: 0, seconds: 0, frames: 0 }, timecode_secs: 2.0 }],
            25.0, 2.0, LtcDecodeStatus::Success,
        );
        let results = build_per_file_start_timecodes(&[Some(&r), Some(&r)]);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_some());
        assert!(results[1].is_some());
    }

    #[test]
    fn test_build_per_file_start_timecodes_with_failures() {
        let results = build_per_file_start_timecodes(&[None, None]);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_none());
        assert!(results[1].is_none());
    }

    #[test]
    fn test_build_per_file_start_timecodes_empty() {
        let results: [Option<&audio_core::LtcDetectionResult>; 0] = [];
        let metas = build_per_file_start_timecodes(&results);
        assert!(metas.is_empty());
    }

    // ── parse_native_timecode ─────────────────────────────────────────

    #[test]
    fn test_parse_native_timecode_valid() {
        let tc = parse_native_timecode("08:09:59:15").unwrap();
        assert_eq!(tc.hours, 8);
        assert_eq!(tc.minutes, 9);
        assert_eq!(tc.seconds, 59);
        assert_eq!(tc.frames, 15);
    }

    #[test]
    fn test_parse_native_timecode_midnight() {
        let tc = parse_native_timecode("00:00:00:00").unwrap();
        assert_eq!(tc.hours, 0);
        assert_eq!(tc.minutes, 0);
        assert_eq!(tc.seconds, 0);
        assert_eq!(tc.frames, 0);
    }

    #[test]
    fn test_parse_native_timecode_invalid_format() {
        assert!(parse_native_timecode("").is_none());
        assert!(parse_native_timecode("15:32").is_none());
        assert!(parse_native_timecode("15:32:14").is_none());
        assert!(parse_native_timecode("hello:world:foo:bar").is_none());
    }

    #[test]
    fn test_parse_native_timecode_invalid_bounds() {
        assert!(parse_native_timecode("24:00:00:00").is_none());
        assert!(parse_native_timecode("00:60:00:00").is_none());
        assert!(parse_native_timecode("00:00:61:00").is_none());
        assert!(parse_native_timecode("00:00:00:60").is_none());
    }

    #[test]
    fn test_parse_native_timecode_max_valid() {
        let tc = parse_native_timecode("23:59:59:59").unwrap();
        assert_eq!(tc.hours, 23);
        assert_eq!(tc.minutes, 59);
        assert_eq!(tc.seconds, 59);
        assert_eq!(tc.frames, 59);
    }
}