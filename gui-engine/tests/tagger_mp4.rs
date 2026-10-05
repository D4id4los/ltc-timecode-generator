//! Native MP4 tmcd in-place tagging — round-trip suite (WP-4 PR-6).
//!
//! Supersedes the skip-happy in-module `test_tag_mp4_tmcd_ffprobe_roundtrip`
//! in `tagger.rs`: it could silently skip both when ffmpeg/ffprobe were
//! absent *and* when the synthesized fixture "fell through" to the fallback
//! — i.e. exactly when the native path regressed. This suite uses a
//! committed fixture (`test-data/tmcd-roundtrip-trailing-moov.mp4`, trailing
//! moov — the native tagger's layout precondition) so:
//!
//! - the first two tests run everywhere (no ffmpeg needed) and **fail
//!   loudly** if the native in-place path is broken;
//! - only the ffprobe validation test is ffmpeg-gated, with a loud skip.
//!
//! Fixture regeneration (run from the repo root):
//! ```text
//! ffmpeg -y -f lavfi -i testsrc=duration=1:size=320x240:rate=25 \
//!        -c:v libx264 -g 25 -pix_fmt yuv420p \
//!        test-data/tmcd-roundtrip-trailing-moov.mp4
//! ```
//! (no `+faststart` — moov must be the last top-level box).

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use gui_engine::converter::TimecodeMetadata;
use gui_engine::tagger::{tag_file, TagOutcome};
use gui_engine::Timecode;

const FIXTURE: &str = "test-data/tmcd-roundtrip-trailing-moov.mp4";

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = gui-engine/; the fixture lives in the repo root.
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn fixture_path() -> PathBuf {
    repo_root().join(FIXTURE)
}

fn test_meta() -> TimecodeMetadata {
    TimecodeMetadata {
        start: Timecode {
            hours: 10,
            minutes: 0,
            seconds: 0,
            frames: 0,
        },
        fps: 25.0,
        drop_frame: false,
    }
}

/// Copy the pristine fixture to a temp path so tests never mutate it.
fn fresh_copy(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::TempDir::new().unwrap_or_else(|e| panic!("tempdir ({tag}): {e}"));
    let p = dir.path().join("input.mp4");
    std::fs::copy(fixture_path(), &p).unwrap_or_else(|e| panic!("copy fixture: {e}"));
    (dir, p)
}

/// Walk the top-level ISO-BMFF boxes of `path`; returns (type, offset, size).
fn top_level_boxes(path: &Path) -> Vec<(String, u64, u64)> {
    let mut file = std::fs::File::open(path).unwrap();
    let len = file.metadata().unwrap().len();
    let mut boxes = Vec::new();
    let mut off = 0u64;
    while off + 8 <= len {
        file.seek(SeekFrom::Start(off)).unwrap();
        let mut hdr = [0u8; 8];
        file.read_exact(&mut hdr).unwrap();
        let size = u32::from_be_bytes(hdr[0..4].try_into().unwrap()) as u64;
        let typ = String::from_utf8_lossy(&hdr[4..8]).to_string();
        if size == 0 {
            boxes.push((typ, off, len - off));
            break;
        }
        assert!(size >= 8, "invalid box size {size} at {off}");
        boxes.push((typ, off, size));
        off += size;
    }
    boxes
}

fn ffmpeg_tooling_available() -> bool {
    let run = |bin: &str| {
        Command::new(bin)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    run("ffmpeg") && run("ffprobe")
}

/// The fixture's structural precondition for the native tagger: the last
/// top-level box must be moov. If a fixture regeneration ever breaks this,
/// fail here instead of silently falling through to the ffmpeg fallback.
#[test]
fn fixture_has_trailing_moov() {
    let boxes = top_level_boxes(&fixture_path());
    assert!(!boxes.is_empty(), "fixture has no boxes");
    let (typ, _, _) = boxes.last().unwrap();
    assert_eq!(
        typ, "moov",
        "fixture's last top-level box must be moov, got {typ}"
    );
}

#[test]
fn test_tag_mp4_native_in_place_no_ffmpeg_fallback() {
    let (_dir, p) = fresh_copy("in-place");
    let len_before = std::fs::metadata(&p).unwrap().len();

    let outcome = tag_file(&p, &test_meta(), None).expect("tagging must succeed");

    // The native in-place path is the whole point — a fallback remux here
    // means the native tagger failed on the committed fixture's layout.
    assert!(
        matches!(outcome, TagOutcome::TaggedInPlace),
        "native in-place path must be taken, got {outcome:?}"
    );

    // The in-place tag grows the file by exactly: 12-byte appended mdat +
    // new moov (old moov payload + tmcd trak) − old moov size + 8-byte new
    // moov header, where the old moov slot is reused as a free box. Assert
    // structurally instead: growth is small (kilobytes) and the box walk
    // still ends in a valid moov. A ffmpeg fallback remux would produce a
    // materially different (typically much smaller/larger) size.
    let len_after = std::fs::metadata(&p).unwrap().len();
    let growth = len_after as i64 - len_before as i64;
    assert!(
        growth > 0 && growth < 8192,
        "in-place tag must grow the file modestly, got {growth} B"
    );

    let boxes = top_level_boxes(&p);
    let (typ, _, _) = boxes.last().unwrap();
    assert_eq!(typ, "moov", "tagged file must still end in a valid moov");
    // Old moov slot became a free box.
    assert!(
        boxes.iter().any(|(t, _, _)| t == "free"),
        "old moov slot must have been rewritten as a free box"
    );
    assert!(
        boxes.iter().any(|(t, _, _)| t == "mdat"),
        "appended tmcd sample must live in an mdat box"
    );
}

/// The §3.2 regression net: the tmcd track's stco entry must point at the
/// appended mdat payload (pre_tag_len + 8 — the file length captured before
/// any writes, plus the appended mdat's 8-byte header), and the bytes there
/// must be the 32-bit frame count of the tagged start timecode.
#[test]
fn test_tag_mp4_stco_offset_resolves_to_appended_payload() {
    let (_dir, p) = fresh_copy("stco");
    let len_before = std::fs::metadata(&p).unwrap().len();

    let outcome = tag_file(&p, &test_meta(), None).expect("tagging must succeed");
    assert!(matches!(outcome, TagOutcome::TaggedInPlace));

    let expected_stco = len_before + 8;
    let expected_frames: u32 = 10 * 3600 * 25; // 10:00:00:00 at 25 fps

    let mut data = Vec::new();
    std::fs::File::open(&p)
        .unwrap()
        .read_to_end(&mut data)
        .unwrap();

    // Walk the appended (last) moov, find trak with hdlr 'tmcd', read stco.
    let boxes = top_level_boxes(&p);
    let (moov_typ, moov_off, moov_size) = boxes.last().unwrap().clone();
    assert_eq!(moov_typ, "moov");

    let moov = &data[moov_off as usize..(moov_off + moov_size) as usize];
    let mut trak_stco: Option<u32> = None;
    // Scan sub-boxes recursively enough to reach trak > stco.
    // moov contains trak boxes; find the one whose (recursively located)
    // hdlr has component subtype 'tmcd', then read its stco entry.
    fn scan_all<'a>(buf: &'a [u8], expected: &str, out: &mut Vec<(u64, &'a [u8])>) {
        let mut pos = 0usize;
        while pos + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[pos..pos + 4].try_into().unwrap()) as usize;
            let typ = String::from_utf8_lossy(&buf[pos + 4..pos + 8]).to_string();
            if size < 8 || pos + size > buf.len() {
                break;
            }
            if typ == expected {
                out.push((pos as u64, &buf[pos..pos + size]));
            }
            if matches!(typ.as_str(), "moov" | "trak" | "mdia" | "minf" | "stbl") {
                scan_all(&buf[pos + 8..pos + size], expected, out);
            }
            pos += size;
        }
    }
    let mut pos = 8usize; // skip moov header
    while pos + 8 <= moov.len() {
        let size = u32::from_be_bytes(moov[pos..pos + 4].try_into().unwrap()) as usize;
        let typ = &moov[pos + 4..pos + 8];
        if size < 8 || pos + size > moov.len() {
            break;
        }
        if typ == b"trak" {
            let trak = &moov[pos..pos + size];
            let mut hdlrs = Vec::new();
            scan_all(trak, "hdlr", &mut hdlrs);
            let is_tmcd = hdlrs.iter().any(|(_, hdlr_buf)| {
                // hdlr box: 8-byte header, version/flags(4), pre_defined(4),
                // then the 4-byte handler subtype — read from the box's own
                // bytes (scan_all offsets are nested-relative).
                &hdlr_buf[16..20] == b"tmcd"
            });
            if is_tmcd {
                let mut stcos = Vec::new();
                scan_all(trak, "stco", &mut stcos);
                if let Some((_, stco_buf)) = stcos.first() {
                    // stco box: 8-byte header, then version/flags(4)
                    // entry_count(4) chunk_offset(4) — full box [16..20].
                    let entry = u32::from_be_bytes(stco_buf[16..20].try_into().unwrap());
                    trak_stco = Some(entry);
                }
            }
        }
        pos += size;
    }
    let stco = trak_stco.expect("tagged file must contain a tmcd trak with stco");
    assert_eq!(
        stco as u64, expected_stco,
        "stco must point at file_len_before + 8 (the appended mdat payload)"
    );
    let sample = u32::from_be_bytes(data[stco as usize..stco as usize + 4].try_into().unwrap());
    assert_eq!(
        sample, expected_frames,
        "mdat payload at the stco offset must be the tagged frame count"
    );
}

#[test]
fn test_tag_mp4_ffprobe_roundtrip() {
    if !ffmpeg_tooling_available() {
        eprintln!("--- SKIPPED: ffmpeg/ffprobe not available (tmcd ffprobe round-trip)");
        return;
    }
    let (_dir, p) = fresh_copy("ffprobe");
    let outcome = tag_file(&p, &test_meta(), None).expect("tagging must succeed");
    assert!(matches!(outcome, TagOutcome::TaggedInPlace));

    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-of", "json"])
        .arg(&p)
        .output()
        .expect("ffprobe should run");
    assert!(out.status.success(), "ffprobe must accept the tagged file");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("tmcd"),
        "tagged file must expose a tmcd stream"
    );
    // The embedded start timecode VALUE lives in the tmcd sample (the mdat
    // payload asserted by test_tag_mp4_stco_offset_resolves_to_appended_payload);
    // ffprobe surfaces the stream but not the sample bytes as a text tag —
    // same assertion the superseded in-module test made.
}
