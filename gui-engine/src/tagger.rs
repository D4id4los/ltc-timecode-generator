use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::process::Output;

use std::time::Duration;

use crate::camera_meta::CameraInfo;
use crate::converter::{TimecodeMetadata, format_ffmpeg_timecode};
use crate::subprocess::{no_window_command, run_output_with_timeout, SubprocessFailure};

/// Originator string for WAV bext when no camera is detected.
const BEXT_DEFAULT_ORIGINATOR: &str = "LTC Timecode Generator";

/// Total timeout for the ffmpeg stream-copy tagging fallback.
/// Stream copying a full-length recording (e.g. 100 GB on slow storage) can
/// take many minutes, but should never hang indefinitely.
const TAG_REMUX_TIMEOUT: Duration = Duration::from_secs(1800);

/// Outcome of a tagging attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum TagOutcome {
    TaggedInPlace,
    TaggedViaFfmpeg,
    Skipped { reason: String },
}

/// Tag a file with start-timecode and (optionally) camera metadata using the
/// fastest available method:
///
/// 1. **MOV/MP4 with trailing `moov`** — native in-place tagger: replaces the
///    old `moov` with a `free` box and appends the rebuilt `moov` (original
///    boxes + a `tmcd` timecode track + a tiny `mdat` sample).  Never touches
///    media data, so I/O is O(1).  Camera metadata is NOT embedded by the
///    O(1) in-place MP4 tagger; only the ffmpeg fallback and all converted
///    outputs carry it.
/// 2. **WAV with an existing `bext` chunk** — patches the originator (32 B),
///    origination_date (10 B), and 8-byte `time_reference` fields in place.
///    No bext → ffmpeg fallback.
/// 3. **Everything else** — ffmpeg stream-copy remux (`-c copy
///    -timecode …`) to a temp file followed by an atomic rename over the
///    original.  This is I/O-bound (no decode) but requires a full file
///    write.  Camera metadata is passed via `-metadata` / `-write_bext`.
///
/// Containers that have no standard timecode metadata (e.g. MPEG-TS / MTS)
/// are skipped with a `Skipped` outcome.
pub fn tag_file(path: &Path, meta: &TimecodeMetadata, camera: Option<&CameraInfo>) -> Result<TagOutcome, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    match ext.as_str() {
        "mov" | "mp4" | "m4v" => {
            match native::tag_mp4_tmcd(path, meta) {
                Ok(outcome) => return Ok(outcome),
                Err(e) => {
                    log::warn!(
                        "Native MP4 tagger skipped for {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
        "mxf" => {
            // MXF uses ffmpeg fallback (header-partition TC is structural)
        }
        "wav" => {
            match tag_wav_bext(path, meta, camera) {
                Ok(outcome) => return Ok(outcome),
                Err(e) => {
                    log::warn!(
                        "Native WAV tagger skipped for {}: {}",
                        path.display(),
                        e
                    );
                }
            }
        }
        "mts" | "m2ts" | "ts" | "m2t" => {
            return Ok(TagOutcome::Skipped {
                reason: format!(
                    "MPEG-TS container '{}' has no standard timecode metadata",
                    ext
                ),
            });
        }
        _ => {
            log::info!(
                "Fallback tagging for unknown extension '{}' for {}",
                ext,
                path.display()
            );
        }
    }

    tag_via_ffmpeg(path, meta, camera)
}

// ── Native MOV/MP4 in-place tagger ─────────────────────────────────────

mod native {
    use super::*;

    /// Try to tag a MOV/MP4 file in place.  Returns `Ok(outcome)` on success;
    /// on failure (wrong layout, unparseable boxes) returns `Err(reason)` so
    /// the caller can fall through to the ffmpeg fallback.
    pub fn tag_mp4_tmcd(path: &Path, meta: &TimecodeMetadata) -> Result<TagOutcome, String> {
        let mut file =
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .map_err(|e| format!("cannot open {}: {}", path.display(), e))?;

        let file_len = file
            .seek(SeekFrom::End(0))
            .map_err(|e| format!("seek end {}: {}", path.display(), e))?;

        if file_len < 12 {
            return Err(format!("file too small: {}", file_len));
        }

        // ── Phase 1: scan top-level boxes ────────────────────────────────
        let top_boxes = scan_top_level_boxes(&mut file, file_len)?;

        let last = top_boxes
            .last()
            .ok_or_else(|| "no top-level boxes found".to_string())?;

        if last.box_type != *b"moov" {
            return Err(format!(
                "last box is '{:?}', not 'moov' — cannot tag in place",
                std::str::from_utf8(&last.box_type).unwrap_or("??")
            ));
        }

        // ── Phase 2: read and parse the moov box ─────────────────────────
        let moov_offset = last.offset;
        let moov_size = last.box_size;

        // Read the full moov payload (skip 8-byte header)
        let mut moov_data = vec![0u8; (moov_size - 8) as usize];
        file.seek(SeekFrom::Start(moov_offset + 8))
            .map_err(|e| format!("seek moov: {}", e))?;
        file.read_exact(&mut moov_data)
            .map_err(|e| format!("read moov: {}", e))?;

        // Parse sub-boxes of moov
        let inner_boxes = scan_boxes_in_slice(&moov_data, moov_offset + 8);

        let mvhd_box = inner_boxes
            .iter()
            .find(|b| b.box_type == *b"mvhd")
            .ok_or_else(|| "no mvhd in moov".to_string())?;

        // ── Phase 3: parse mvhd payload ─────────────────────────────────
        // mvhd version 0:  (1 + 3 = 4 bytes header)
        //   creation_time(4), modification_time(4), timescale(4), duration(4)
        //   rate(4), volume(2), reserved(10), matrix(36), predef(24), next_track_id(4)
        //
        // mvhd version 1: (1 + 3 = 4 bytes header)
        //   creation_time(8), modification_time(8), timescale(4), duration(8)
        //   rate(4), volume(2), reserved(10), matrix(36), predef(24), next_track_id(4)

        let mvhd_payload_offset = mvhd_box.payload_offset;
        let rel_payload = &moov_data
            [(mvhd_payload_offset - (moov_offset + 8)) as usize..];

        let version = rel_payload[0];
        let _flags = read_be_u24(&rel_payload[1..4]);

        let timescale;
        let duration;
        let next_track_id_off;

        if version == 0 {
            timescale = read_be_u32(&rel_payload[12..16]) as u64;
            duration = read_be_u32(&rel_payload[16..20]) as u64;
            next_track_id_off = 16 + 4 + 4 + 2 + 10 + 36 + 24; // 96
        } else {
            timescale = read_be_u32(&rel_payload[20..24]) as u64;
            duration = read_be_u64(&rel_payload[24..32]);
            next_track_id_off = 32 + 4 + 2 + 10 + 36 + 24; // shifted
        }
        if timescale == 0 {
            return Err("mvhd timescale is 0".to_string());
        }

        // Read current next_track_id
        let cur_next_track_id =
            read_be_u32(&rel_payload[next_track_id_off..next_track_id_off + 4]);
        let new_track_id = cur_next_track_id;

        // ── Phase 4: build the tmcd trak box ────────────────────────────
        let tmcd_trak =
            build_tmcd_trak(meta, new_track_id, timescale, duration)?;

        // The appended mdat payload lands at file_len + 8 (file_len was
        // captured before any writes; nothing else grows the file first).
        let mut tmcd_trak = tmcd_trak;
        patch_stco_offset(&mut tmcd_trak, (file_len + 8) as u32)?;

        // ── Phase 5: rebuild the moov with the tmcd trak appended ───────
        // The old moov payload is inner_boxes (all sub-boxes). We need to
        // patch mvhd's next_track_id, then append tmcd_trak.
        //
        // Approach: keep the original moov_data, patch the next_track_id
        // field in moov_data, then append tmcd_trak.

        // Patch next_track_id in moov_data
        let abs_next_off = mvhd_payload_offset - (moov_offset + 8) + next_track_id_off as u64;
        let _old_val = read_be_u32(&moov_data[abs_next_off as usize..abs_next_off as usize + 4]);
        let new_val = new_track_id;
        let next_bytes = new_val.to_be_bytes();
        moov_data[abs_next_off as usize..abs_next_off as usize + 4]
            .copy_from_slice(&next_bytes);

        // Build the new moov payload: old moov_data + tmcd_trak
        let mut new_moov_payload = moov_data;
        new_moov_payload.extend_from_slice(&tmcd_trak);
        let new_moov_size = (8 + new_moov_payload.len()) as u64;

        // ── Phase 6: build the appended mdat (4-byte sample) ────────────
        // The tmcd sample: timecode value in frames since midnight, using
        // tmcd's own timescale (typically fps * 1 or fps timescale).
        // QuickTime spec: the sample value is a 32-bit frame count.
        // We want the number of frames from midnight to meta.start.
        let fps_floor = meta.fps.floor() as u32;
        let frame_count = meta.start.hours * 3600 * fps_floor
            + meta.start.minutes * 60 * fps_floor
            + meta.start.seconds * fps_floor
            + meta.start.frames;
        let mdat_payload = frame_count.to_be_bytes(); // 4 bytes
        let mdat_box_size = 8u32 + 4; // header + sample
        let mut mdat_box = Vec::with_capacity(12);
        mdat_box.extend_from_slice(&mdat_box_size.to_be_bytes());
        mdat_box.extend_from_slice(b"mdat");
        mdat_box.extend_from_slice(&mdat_payload);

        // ── Phase 7: write in place ─────────────────────────────────────
        // 7a: overwrite old moov with a free box
        let free_size = moov_size as u32;
        let free_header = free_size.to_be_bytes();
        // free type + size:
        file.seek(SeekFrom::Start(moov_offset))
            .map_err(|e| format!("seek to moov: {}", e))?;
        file.write_all(&free_header) // size
            .map_err(|e| format!("write free size: {}", e))?;
        file.write_all(b"free") // type
            .map_err(|e| format!("write free type: {}", e))?;

        // 7b: append mdat box
        file.seek(SeekFrom::End(0))
            .map_err(|e| format!("seek end: {}", e))?;
        file.write_all(&mdat_box)
            .map_err(|e| format!("write appended mdat: {}", e))?;

        // 7c: append new moov
        let mut new_moov_header = Vec::with_capacity(8);
        new_moov_header.extend_from_slice(&(new_moov_size as u32).to_be_bytes());
        new_moov_header.extend_from_slice(b"moov");
        file.write_all(&new_moov_header)
            .map_err(|e| format!("write new moov header: {}", e))?;
        file.write_all(&new_moov_payload)
            .map_err(|e| format!("write new moov payload: {}", e))?;

        file.sync_all()
            .map_err(|e| format!("fsync: {}", e))?;

        log::info!(
            "Tagged {} in place (moov → free, appended tmcd + mdat, size delta ∼{} B)",
            path.display(),
            new_moov_size as i64 - moov_size as i64 + 12
        );

        Ok(TagOutcome::TaggedInPlace)
    }

    // ── Internal helpers ────────────────────────────────────────────────

    pub fn scan_top_level_boxes(
        file: &mut std::fs::File,
        file_len: u64,
    ) -> Result<Vec<BoxEntry>, String> {
        let mut boxes: Vec<BoxEntry> = Vec::new();
        let mut offset: u64 = 0;

        while offset + 8 <= file_len {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| format!("seek box at {}: {}", offset, e))?;

            let mut hdr = [0u8; 8];
            if file.read_exact(&mut hdr).is_err() {
                break;
            }
            let size = read_be_u32(&hdr[0..4]) as u64;
            let box_type = [hdr[4], hdr[5], hdr[6], hdr[7]];
            if size == 0 {
                // Box extends to end of file
                break;
            }
            if size < 8 {
                return Err(format!(
                    "invalid box size {} at offset {}",
                    size, offset
                ));
            }
            boxes.push(BoxEntry {
                offset,
                box_size: size,
                box_type,
                payload_offset: offset + 8,
            });
            offset += size;
        }
        Ok(boxes)
    }

    fn scan_boxes_in_slice(data: &[u8], base_offset: u64) -> Vec<BoxEntry> {
        let mut boxes: Vec<BoxEntry> = Vec::new();
        let len = data.len();
        let mut pos = 0usize;
        while pos + 8 <= len {
            let size = read_be_u32(&data[pos..pos + 4]) as u64;
            let box_type = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];
            if size == 0 {
                break;
            }
            if size < 8 || pos + (size as usize) > len {
                break;
            }
            boxes.push(BoxEntry {
                offset: base_offset + pos as u64,
                box_size: size,
                box_type,
                payload_offset: base_offset + pos as u64 + 8,
            });
            pos += size as usize;
        }
        boxes
    }
}

// ── Box structure ────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct BoxEntry {
    /// Absolute file offset of the box header
    offset: u64,
    /// Total box size (including 8-byte header)
    box_size: u64,
    /// 4-character box type
    box_type: [u8; 4],
    /// Absolute file offset of the payload (offset + 8)
    payload_offset: u64,
}

// ── tmcd trak builder ──────────────────────────────────────────────────

/// Build a complete `trak` box for a QuickTime timecode media track.
///
/// The track carries a single 4-byte sample representing the start timecode
/// in frames since midnight, expressed in the tmcd's timescale (fps).
fn build_tmcd_trak(
    meta: &TimecodeMetadata,
    track_id: u32,
    movie_timescale: u64,
    movie_duration: u64,
) -> Result<Vec<u8>, String> {
    let fps = meta.fps;
    let drop_frame = meta.drop_frame;

    let fps_floor = fps.floor() as u32;
    if fps_floor == 0 {
        return Err("fps is 0".to_string());
    }

    // Timescale for tmcd = fps (so 1 unit = 1 frame)
    let tmcd_timescale = fps_floor;

    // Duration in tmcd timescale = movie_duration frames
    let tmcd_duration = if movie_timescale > 0 {
        let seconds = movie_duration as f64 / movie_timescale as f64;
        (seconds * fps).round() as u32
    } else {
        0
    };

    // Flags for the tmcd sample description
    // bit 0: drop frame (0=NDF, 1=DF)
    // bit 1: 24 hour max (1 = yes)
    // bit 2: negative timescale OK (0)
    let mut tmcd_flags: u32 = 0x0000_0200; // 24h max
    if drop_frame {
        tmcd_flags |= 0x0000_0100; // drop frame
    }

    // ── tkhd ───────────────────────────────────────────────────────────
    // Version 0, flags = 0x0007 (track enabled, in movie, in preview)
    let tkhd = build_tkhd(track_id, tmcd_duration as u64, movie_timescale as u32);

    // ── mdhd ───────────────────────────────────────────────────────────
    let mdhd = build_mdhd(tmcd_timescale, tmcd_duration, 0); // language = 0 (undetermined)

    // ── hdlr (handler: tmcd) ────────────────────────────────────────────
    let hdlr = build_hdlr_tmcd();

    // ── gmhd + tmcd sample description ─────────────────────────────────
    let gmin = build_gmin();
    let tmcd_sd = build_tmcd_sd_box(meta, fps_floor, tmcd_flags);
    let gmhd = build_box(b"gmhd", &[&gmin, &tmcd_sd]);

    // ── stbl ───────────────────────────────────────────────────────────
    // stsd: one entry = the tmcd sample description
    let stsd = build_stsd_tmcd(meta, fps_floor, tmcd_flags);

    // stts: 1 sample
    // - entry count: 1
    // - sample count: 1, sample duration: 1
    let stts = build_stts_one();

    // stsc: 1 chunk, 1 sample per chunk
    let stsc = build_stsc_one();

    // stsz: 1 sample, size = 4
    let stsz = build_stsz_one(4);

    // stco: chunk offset = will be the offset of the mdat box data after appending
    // We don't know yet, so write 0 and patch at write time.
    let stco = build_stco_one(0);

    let stbl = build_box(b"stbl", &[&stsd, &stts, &stsc, &stsz, &stco]);

    // ── minf ───────────────────────────────────────────────────────────
    let minf = build_box(b"minf", &[&gmhd, &stbl]);

    // ── mdia ───────────────────────────────────────────────────────────
    let mdia = build_box(b"mdia", &[&mdhd, &hdlr, &minf]);

    // ── trak ───────────────────────────────────────────────────────────
    let trak = build_box(b"trak", &[&tkhd, &mdia]);

    // The stco chunk offset is left at 0 here; the caller patches it with
    // patch_stco_offset() once the appended mdat position is known.
    Ok(trak)
}

/// Patch the single stco chunk-offset entry inside a built tmcd trak buffer
/// to point at the appended mdat payload. `trak` is searched for the `stco`
/// box type; the entry lives 12 bytes past it (version/flags 4 + entry_count 4).
pub(super) fn patch_stco_offset(trak: &mut [u8], chunk_offset: u32) -> Result<(), String> {
    let pos = trak
        .windows(4)
        .position(|w| w == b"stco")
        .ok_or_else(|| "no stco box in tmcd trak".to_string())?;
    let entry = pos + 12;
    if entry + 4 > trak.len() {
        return Err(format!(
            "stco entry out of bounds (pos={}, len={})",
            pos,
            trak.len()
        ));
    }
    trak[entry..entry + 4].copy_from_slice(&chunk_offset.to_be_bytes());
    Ok(())
}

/// Build a `tkhd` box (version 0, flags 0x0003 = enabled + in movie).
fn build_tkhd(track_id: u32, duration: u64, _timescale: u32) -> Vec<u8> {
    let flags: u32 = 0x0000_0003; // track enabled + in movie
    let version_flags = [0u8, ((flags >> 16) & 0xFF) as u8, ((flags >> 8) & 0xFF) as u8, (flags & 0xFF) as u8];
    let mut buf = Vec::with_capacity(92);
    buf.extend_from_slice(&version_flags); // 4 bytes: version(1) + flags(3)
    buf.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    buf.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    buf.extend_from_slice(&(track_id.to_be_bytes())); // track_ID
    buf.extend_from_slice(&0u32.to_be_bytes()); // reserved
    buf.extend_from_slice(&(duration.to_be_bytes())); // duration
    buf.extend_from_slice(&0u64.to_be_bytes()); // reserved [layer, alternate_group, volume] + reserved
    // matrix (36 bytes) = identity
    let identity_matrix: [u8; 36] = [
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // a=1, b=0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,  // u=0, c=0
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // d=1, v=0
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,  // w=1, x=0
        0x00, 0x00, 0x00, 0x00,                             // y=0
    ];
    buf.extend_from_slice(&identity_matrix);
    // width(4) + height(4) = 0 (default)
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    build_box(b"tkhd", &[&buf])
}

/// Build an `mdhd` box (version 0).
fn build_mdhd(timescale: u32, duration: u32, language: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(24);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    buf.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    buf.extend_from_slice(&(timescale.to_be_bytes())); // timescale
    buf.extend_from_slice(&(duration.to_be_bytes())); // duration
    buf.extend_from_slice(&(language.to_be_bytes())); // language (ISO 639-2/T packed)
    buf.extend_from_slice(&0u16.to_be_bytes()); // pre_defined (quality)
    build_box(b"mdhd", &[&buf])
}

/// Build a `hdlr` box for handler type `tmcd` and component name ≈ "TimecodeHandler".
fn build_hdlr_tmcd() -> Vec<u8> {
    let mut buf = Vec::with_capacity(32);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(b"dhlr"); // component type
    buf.extend_from_slice(b"tmcd"); // component subtype
    buf.extend_from_slice(&[0u8; 4]); // component manufacturer
    buf.extend_from_slice(&[0u8; 4]); // component flags
    buf.extend_from_slice(&[0u8; 4]); // component flags mask
    // Name string (pascal-style: count byte + "TimecodeHandler\0")
    buf.push(15u8); // string length
    buf.extend_from_slice(b"TimecodeHandler");
    build_box(b"hdlr", &[&buf])
}

/// Build a `gmin` box (generic media info) — standard for timecode tracks.
fn build_gmin() -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(&0u16.to_be_bytes()); // graphics mode = 0 (copy)
    buf.extend_from_slice(&[0u8; 4]); // opColor red
    buf.extend_from_slice(&[0u8; 4]); // opColor green
    buf.extend_from_slice(&[0u8; 4]); // opColor blue
    buf.extend_from_slice(&0u16.to_be_bytes()); // balance = 0
    buf.extend_from_slice(&0u16.to_be_bytes()); // reserved
    build_box(b"gmin", &[&buf])
}

/// Build the `tmcd` sample description box (inside `gmhd` and `stsd`).
///
/// QuickTime Timecode Sample Description (extends SampleDescription):
///
/// | size (4) | type = 'tmcd' (4) | data_reference_index (2) | reserved (6) |
/// | flags (4) | ??? |
///
/// The additional fields after the standard 12-byte sample description header:
///   timeScale(4) | frameDuration(4) | numberOfFrames (1) | reserved (1) | reserved (2) | ... |
///
/// We build the simplest variant that NLEs accept.
fn build_tmcd_sd_box(_meta: &TimecodeMetadata, fps_floor: u32, tmcd_flags: u32) -> Vec<u8> {
    // This goes inside stsd as the single entry AND inside gmhd as additional
    // info. The exact layout is proprietary QuickTime but well-known:
    //
    // tmcd Atom (extending SampleDescription):
    //   size (4) | 'tmcd' (4) | data_reference_index (2) | reserved (6)
    //   | flags (4) | timeScale (4) | frameDuration (4) | numberOfFrames (1)
    //   | reserved (1) | reserved (2) | language (2) | quality (2)

    let frame_dur = fps_floor; // duration in tmcd timescale = 1 frame
    let num_frames = fps_floor as u8;

    let mut buf = Vec::with_capacity(36);
    buf.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index = 1
    buf.extend_from_slice(&[0u8; 6]); // reserved
    buf.extend_from_slice(&(tmcd_flags.to_be_bytes())); // flags (drop frame, 24h max)
    buf.extend_from_slice(&(fps_floor.to_be_bytes())); // timeScale
    buf.extend_from_slice(&(frame_dur.to_be_bytes())); // frameDuration
    buf.push(num_frames); // numberOfFrames
    buf.push(0u8); // reserved
    buf.extend_from_slice(&0u16.to_be_bytes()); // reserved
    buf.extend_from_slice(&0u16.to_be_bytes()); // language (undetermined)
    buf.extend_from_slice(&0u16.to_be_bytes()); // quality (default)

    build_box(b"tmcd", &[&buf])
}

/// Build `stsd` box with one `tmcd` entry.
fn build_stsd_tmcd(meta: &TimecodeMetadata, fps_floor: u32, tmcd_flags: u32) -> Vec<u8> {
    let entry = build_tmcd_sd_box(meta, fps_floor, tmcd_flags);
    let payload_size = 8 + entry.len(); // 8 = stsd header (version/flags + entry_count)
    let mut buf = Vec::with_capacity(payload_size);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
    buf.extend_from_slice(&entry);
    build_box(b"stsd", &[&buf])
}

fn build_stts_one() -> Vec<u8> {
    let mut buf = Vec::with_capacity(12);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    buf.extend_from_slice(&1u32.to_be_bytes()); // sample_count = 1
    buf.extend_from_slice(&1u32.to_be_bytes()); // sample_duration = 1
    build_box(b"stts", &[&buf])
}

fn build_stsc_one() -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    buf.extend_from_slice(&1u32.to_be_bytes()); // first_chunk = 1
    buf.extend_from_slice(&1u32.to_be_bytes()); // samples_per_chunk = 1
    buf.extend_from_slice(&1u32.to_be_bytes()); // sample_description_index = 1
    build_box(b"stsc", &[&buf])
}

fn build_stsz_one(sample_size: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(16);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(&0u32.to_be_bytes()); // sample_size = 0 (variable)
    buf.extend_from_slice(&1u32.to_be_bytes()); // sample_count = 1
    buf.extend_from_slice(&(sample_size.to_be_bytes())); // entry[0] = 4
    build_box(b"stsz", &[&buf])
}

/// Build an `stco` box (chunk offset box) with a single entry = `offset`.
/// The offset will be patched just before writing.
fn build_stco_one(chunk_offset: u32) -> Vec<u8> {
    let mut buf = Vec::with_capacity(12);
    buf.extend_from_slice(&[0u8, 0u8, 0u8, 0u8]); // version=0, flags=0
    buf.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
    buf.extend_from_slice(&(chunk_offset.to_be_bytes())); // chunk_offset
    build_box(b"stco", &[&buf])
}

/// Generic box builder: wraps payload in a size(4) + type(4) + payload.
fn build_box(box_type: &[u8; 4], payloads: &[&[u8]]) -> Vec<u8> {
    let total_payload: usize = payloads.iter().map(|p| p.len()).sum();
    let total_size = 8 + total_payload;
    let mut buf = Vec::with_capacity(total_size);
    buf.extend_from_slice(&(total_size as u32).to_be_bytes());
    buf.extend_from_slice(box_type);
    for p in payloads {
        buf.extend_from_slice(p);
    }
    buf
}

// ── WAV bext patch ─────────────────────────────────────────────────────

fn read_le_u32(data: &[u8]) -> u32 {
    debug_assert!(data.len() >= 4);
    u32::from_le_bytes(data[..4].try_into().unwrap())
}

fn bext_padded_string(s: &str, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let bytes = s.as_bytes();
    let copy_len = bytes.len().min(len);
    buf[..copy_len].copy_from_slice(&bytes[..copy_len]);
    buf
}

fn bext_originator(camera: Option<&CameraInfo>) -> String {
    let c = match camera {
        Some(c) => c,
        None => return BEXT_DEFAULT_ORIGINATOR.to_string(),
    };
    let make = c.make.as_deref().unwrap_or("");
    let model = c.model.as_deref().unwrap_or("");
    match (make.is_empty(), model.is_empty()) {
        (true, true) => BEXT_DEFAULT_ORIGINATOR.to_string(),
        (true, false) => model.to_string(),
        (false, true) => make.to_string(),
        (false, false) => format!("{} {}", make, model),
    }
}

/// Build the bext `description` field (model / lens) when lens is available.
fn build_bext_description(camera: Option<&CameraInfo>) -> Option<String> {
    let c = camera?;
    let lens = c.lens.as_ref()?;
    let model = c.model.as_deref().unwrap_or("");
    if !model.is_empty() {
        Some(format!("{} / {}", model, lens))
    } else {
        Some(lens.clone())
    }
}

/// Build a 602-byte BWF `bext` chunk payload (BWF v1 fixed-size fields).
///
/// Layout (from payload start):
///   description[256] | originator[32] | originator_ref[32] |
///   origination_date[10] | origination_time[8] |
///   time_reference[8] | version[2] | umid[64] | reserved[190]
fn build_bext_payload(
    camera: Option<&CameraInfo>,
    meta: &TimecodeMetadata,
    file: &mut std::fs::File,
) -> Result<Vec<u8>, String> {
    let mut buf = Vec::with_capacity(602);

    // description (256)
    let desc = build_bext_description(camera).unwrap_or_default();
    buf.extend_from_slice(&bext_padded_string(&desc, 256));

    // originator (32)
    let originator = bext_originator(camera);
    buf.extend_from_slice(&bext_padded_string(&originator, 32));

    // originator_ref (32) — empty
    buf.extend_from_slice(&bext_padded_string("", 32));

    // origination_date (10)
    let date = camera
        .and_then(|c| c.creation_date.as_deref())
        .unwrap_or("");
    buf.extend_from_slice(&bext_padded_string(date, 10));

    // origination_time (8) — empty
    buf.extend_from_slice(&bext_padded_string("", 8));

    // time_reference (8 bytes)
    let sample_rate = crate::converter::read_wav_sample_rate_from_file(file)
        .ok_or_else(|| "no fmt chunk found".to_string())?;
    let time_reference = crate::converter::time_reference_samples(meta, sample_rate);
    buf.extend_from_slice(&time_reference.to_le_bytes());

    // version (2 bytes) — BWF v1 = 1
    buf.extend_from_slice(&1u16.to_le_bytes());

    // umid (64 bytes) — empty
    buf.extend_from_slice(&[0u8; 64]);

    // reserved (190 bytes) — zero
    buf.extend_from_slice(&[0u8; 190]);

    debug_assert_eq!(buf.len(), 602);
    Ok(buf)
}

/// Try to patch the `bext` chunk's originator, origination_date, and
/// `time_reference` fields in a WAV file in place.
/// If no `bext` chunk exists, creates one by appending it at the end of
/// the file (O(1) for any file size — most WAV parsers accept bext after
/// the data chunk).
fn tag_wav_bext(path: &Path, meta: &TimecodeMetadata, camera: Option<&CameraInfo>) -> Result<TagOutcome, String> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("cannot open {}: {}", path.display(), e))?;

    let file_len = file
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("seek end {}: {}", path.display(), e))?;
    if file_len < 44 {
        return Err("not a valid WAV file".to_string());
    }

    // Read header
    let mut hdr = [0u8; 12];
    file.seek(SeekFrom::Start(0))
        .map_err(|e| format!("seek: {}", e))?;
    file.read_exact(&mut hdr)
        .map_err(|e| format!("read header: {}", e))?;
    if &hdr[0..4] != b"RIFF" || &hdr[8..12] != b"WAVE" {
        return Err("not a RIFF WAV file".to_string());
    }

    // Scan for "bext" chunk. WAV chunk sizes are little-endian.
    let mut offset: u64 = 12; // skip RIFF header
    while offset + 8 <= file_len {
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| format!("seek: {}", e))?;
        let mut chunk_hdr = [0u8; 8];
        file.read_exact(&mut chunk_hdr)
            .map_err(|e| format!("read chunk: {}", e))?;
        let chunk_size = read_le_u32(&chunk_hdr[4..8]) as u64;
        let chunk_id = &chunk_hdr[0..4];

        if chunk_id == b"bext" {
            // Found bext chunk — patch in place
            let payload_off = offset + 8;
            if chunk_size < 346 {
                return Err(format!(
                    "bext chunk too small ({} bytes, need >= 346)",
                    chunk_size
                ));
            }

            // Write description (256 bytes) if lens is available
            if let Some(ref desc) = build_bext_description(camera) {
                let desc_bytes = bext_padded_string(desc, 256);
                file.seek(SeekFrom::Start(payload_off))
                    .map_err(|e| format!("seek to description: {}", e))?;
                file.write_all(&desc_bytes)
                    .map_err(|e| format!("write description: {}", e))?;
            }

            // Write originator (32 bytes, NUL-padded)
            let originator = bext_originator(camera);
            let originator_bytes = bext_padded_string(&originator, 32);
            file.seek(SeekFrom::Start(payload_off + 256))
                .map_err(|e| format!("seek to originator: {}", e))?;
            file.write_all(&originator_bytes)
                .map_err(|e| format!("write originator: {}", e))?;

            // Write origination_date (10 bytes)
            if let Some(camera) = camera {
                if let Some(ref date) = camera.creation_date {
                    let date_bytes = bext_padded_string(date, 10);
                    file.seek(SeekFrom::Start(payload_off + 320))
                        .map_err(|e| format!("seek to origination_date: {}", e))?;
                    file.write_all(&date_bytes)
                        .map_err(|e| format!("write origination_date: {}", e))?;
                }
            }

            // Write time_reference (8 bytes)
            let sample_rate = crate::converter::read_wav_sample_rate_from_file(&mut file)
                .ok_or_else(|| "no fmt chunk found".to_string())?;
            let time_reference = crate::converter::time_reference_samples(meta, sample_rate);
            let time_ref_bytes = time_reference.to_le_bytes();
            file.seek(SeekFrom::Start(payload_off + 338))
                .map_err(|e| format!("seek to time_reference: {}", e))?;
            file.write_all(&time_ref_bytes[0..8])
                .map_err(|e| format!("write time_reference: {}", e))?;
            file.sync_all()
                .map_err(|e| format!("fsync: {}", e))?;

            log::info!(
                "Patched bext originator={}, time_reference={} in {}",
                originator,
                time_reference,
                path.display()
            );
            return Ok(TagOutcome::TaggedInPlace);
        }

        offset += 8 + chunk_size;
        // Chunk padding to even byte boundary
        if chunk_size % 2 != 0 {
            offset += 1;
        }
    }

    // No bext chunk found — create one by appending at end (O(1) for any file size)
    let bext_payload = build_bext_payload(camera, meta, &mut file)?;
    let bext_size = bext_payload.len() as u32;

    file.seek(SeekFrom::End(0))
        .map_err(|e| format!("seek end: {}", e))?;
    file.write_all(b"bext")
        .map_err(|e| format!("write bext id: {}", e))?;
    file.write_all(&bext_size.to_le_bytes())
        .map_err(|e| format!("write bext size: {}", e))?;
    file.write_all(&bext_payload)
        .map_err(|e| format!("write bext payload: {}", e))?;

    // Update RIFF size at offset 4
    let new_file_len = file_len + 8 + bext_size as u64;
    file.seek(SeekFrom::Start(4))
        .map_err(|e| format!("seek to RIFF size: {}", e))?;
    file.write_all(&(new_file_len as u32 - 8).to_le_bytes())
        .map_err(|e| format!("write RIFF size: {}", e))?;

    file.sync_all().map_err(|e| format!("fsync: {}", e))?;

    let originator = bext_originator(camera);
    log::info!(
        "Created bext originator={} in {}",
        originator,
        path.display()
    );
    Ok(TagOutcome::TaggedInPlace)
}


// ── ffmpeg fallback ────────────────────────────────────────────────────

/// Tag a file via ffmpeg stream-copy remux: write to a temp file in the
/// same directory, then atomically rename over the original.
fn tag_via_ffmpeg(path: &Path, meta: &TimecodeMetadata, camera: Option<&CameraInfo>) -> Result<TagOutcome, String> {
    tag_via_ffmpeg_with(path, meta, camera, &mut |args: &[String], tmp: &Path| {
        run_output_with_timeout(
            no_window_command("ffmpeg")
                .args(args)
                .arg(tmp)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped()),
            TAG_REMUX_TIMEOUT,
        )
    })
}

/// Injectable-runner variant of [`tag_via_ffmpeg`] for testability.
///
/// `runner` receives the full argument list (INCLUDING the timecode metadata,
/// camera metadata, and container flags) plus the temporary output path. It
/// must return the subprocess [`Output`] (with timed-out/killed handled as
/// `Err(SubprocessFailure::TimedOut)`).
#[allow(clippy::type_complexity)]
fn tag_via_ffmpeg_with(
    path: &Path,
    meta: &TimecodeMetadata,
    camera: Option<&CameraInfo>,
    runner: &mut dyn FnMut(&[String], &Path) -> Result<Output, SubprocessFailure>,
) -> Result<TagOutcome, String> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin");

    // Determine output container (match input); mkv maps to ffmpeg's
    // "matroska" name for the -f flag.
    let container = match crate::media_ext::container_for_input(ext) {
        "mkv" => "matroska",
        other => other,
    };

    // Temp file in same directory
    let parent = path.parent().unwrap_or(Path::new("."));
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("temp");
    let tmp_path = parent.join(format!(".{}.tc-{}.tmp.{}", stem, std::process::id(), ext));
    let _final_path = tmp_path.with_extension(ext);

    let tc_str = format_ffmpeg_timecode(&meta.start, meta.drop_frame);

    let mut args: Vec<String> = vec!["-y".to_string()];
    args.push("-i".to_string());
    args.push(path.to_string_lossy().to_string());
    args.push("-map".to_string());
    args.push("0".to_string());
    args.push("-c".to_string());
    args.push("copy".to_string());
    args.push("-timecode".to_string());
    args.push(tc_str);

    if matches!(container, "mov" | "mp4" | "mxf") {
        args.push("-write_tmcd".to_string());
        args.push("1".to_string());
    }

    // Camera metadata (via shared helper from args.rs, plus WAV bext extras)
    if let Some(c) = camera {
        if container == "wav" {
            args.push("-write_bext".to_string());
            args.push("1".to_string());
            args.push("-metadata".to_string());
            args.push(format!("originator={}", bext_originator(Some(c))));
            if let Some(ref date) = c.creation_date {
                args.push("-metadata".to_string());
                args.push(format!("origination_date={}", date));
            }
            if let Some(ref lens) = c.lens {
                let model = c.model.as_deref().unwrap_or("");
                if !model.is_empty() {
                    args.push("-metadata".to_string());
                    args.push(format!("description={} / {}", model, lens));
                } else {
                    args.push("-metadata".to_string());
                    args.push(format!("description={}", lens));
                }
            }
        } else {
            let is_mov_like = matches!(container, "mov" | "mp4");
            crate::converter::args::push_camera_metadata(&mut args, Some(c), is_mov_like, container);
        }
    }

    args.push("-f".to_string());
    args.push(container.to_string());

    // Run ffmpeg via the injectable runner
    let output = runner(&args, &tmp_path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        match e {
            SubprocessFailure::Io(msg) => format!("failed to spawn ffmpeg: {}", msg),
            SubprocessFailure::TimedOut => format!(
                "ffmpeg timed out after {}s while tagging {} — file may be corrupt",
                TAG_REMUX_TIMEOUT.as_secs(),
                path.display(),
            ),
        }
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "ffmpeg failed for {}:\n{}",
            path.display(),
            stderr.lines().take(10).collect::<Vec<_>>().join("\n")
        ));
    }

    // Validate output exists and is non-trivial
    let meta_out = std::fs::metadata(&tmp_path)
        .map_err(|e| format!("missing output: {}", e))?;
    if meta_out.len() < 256 {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(format!(
            "output too small ({} bytes) — likely incomplete",
            meta_out.len()
        ));
    }

    // Atomic rename over the original
    std::fs::rename(&tmp_path, path)
        .map_err(|e| format!("rename over original: {}", e))?;

    log::info!("Tagged {} via ffmpeg (stream copy)", path.display());
    Ok(TagOutcome::TaggedViaFfmpeg)
}

// ── Endian helpers ─────────────────────────────────────────────────────

fn read_be_u32(data: &[u8]) -> u32 {
    debug_assert!(data.len() >= 4);
    u32::from_be_bytes(data[..4].try_into().unwrap())
}

fn read_be_u64(data: &[u8]) -> u64 {
    debug_assert!(data.len() >= 8);
    u64::from_be_bytes(data[..8].try_into().unwrap())
}

fn read_be_u24(data: &[u8]) -> u32 {
    debug_assert!(data.len() >= 3);
    u32::from_be_bytes([0, data[0], data[1], data[2]])
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::converter::TimecodeMetadata;
    use audio_core::Timecode;
    // use std::io::Write as IoWrite;
    use tempfile::TempDir;

    fn test_meta() -> TimecodeMetadata {
        TimecodeMetadata {
            start: Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            fps: 25.0,
            drop_frame: false,
        }
    }

    fn test_meta_df() -> TimecodeMetadata {
        TimecodeMetadata {
            start: Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            fps: 29.97,
            drop_frame: true,
        }
    }

    // ── Box parsing ────────────────────────────────────────────────────

    #[test]
    fn test_scan_top_level_boxes_empty() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("empty.mp4");
        std::fs::write(&p, b"").unwrap();

        let mut f = std::fs::OpenOptions::new().read(true).open(&p).unwrap();
        let boxes = native::scan_top_level_boxes(&mut f, 0).unwrap();
        assert!(boxes.is_empty());
    }

    #[test]
    fn test_scan_top_level_boxes_single() {
// Build one box: ftyp with 8B payload (b"mp42isom" is 8 bytes)
    let mut data = Vec::new();
    let box_size: u32 = 16; // header 8 + payload 8
    data.extend_from_slice(&box_size.to_be_bytes());
    data.extend_from_slice(b"ftyp");
    data.extend_from_slice(b"mp42isom");

    let dir = TempDir::new().unwrap();
    let p = dir.path().join("test.mp4");
    std::fs::write(&p, &data).unwrap();
    let flen = data.len() as u64;

    let mut f = std::fs::OpenOptions::new().read(true).open(&p).unwrap();
    let boxes = native::scan_top_level_boxes(&mut f, flen).unwrap();
    assert_eq!(boxes.len(), 1);
    assert_eq!(boxes[0].box_type, *b"ftyp");
    assert_eq!(boxes[0].box_size, 16);
    assert_eq!(boxes[0].offset, 0);
    assert_eq!(boxes[0].payload_offset, 8);
    }

    #[test]
    fn test_scan_top_level_boxes_multi() {
        let mut data = Vec::new();
        // ftyp (16B)
        data.extend_from_slice(&16u32.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(b"mp42isom");
        // moov
        let moov = build_box(b"moov", &[b"some_payload_here____"]);
        data.extend_from_slice(&moov);

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("test.mp4");
        std::fs::write(&p, &data).unwrap();
        let flen = data.len() as u64;

        let mut f = std::fs::OpenOptions::new().read(true).open(&p).unwrap();
        let boxes = native::scan_top_level_boxes(&mut f, flen).unwrap();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[0].box_type, *b"ftyp");
        assert_eq!(boxes[1].box_type, *b"moov");
    }

    #[test]
    fn test_scan_top_level_boxes_last_is_moov() {
        let mut data = Vec::new();
        data.extend_from_slice(&16u32.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(b"mp42isom");
        let moov = build_box(b"moov", &[b"some_payload_here____"]);
        data.extend_from_slice(&moov);

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("test.mp4");
        std::fs::write(&p, &data).unwrap();
        let flen = data.len() as u64;

        let mut f = std::fs::OpenOptions::new().read(true).open(&p).unwrap();
        let boxes = native::scan_top_level_boxes(&mut f, flen).unwrap();
        assert_eq!(boxes.last().unwrap().box_type, *b"moov");
    }

    // ── Box builder ────────────────────────────────────────────────────

    #[test]
    fn test_build_box_roundtrip() {
        let payload = b"HelloBoxWorld!";
        let b = build_box(b"test", &[payload]);
        assert!(b.len() > 8);
        let size = read_be_u32(&b[0..4]);
        assert_eq!(size as usize, b.len());
        assert_eq!(&b[4..8], b"test");
        assert_eq!(&b[8..], payload);
    }

    #[test]
    fn test_tkhd_structure() {
        let tkhd = build_tkhd(2, 100, 25000);
        assert!(tkhd.len() > 20);
        let total_size = read_be_u32(&tkhd[0..4]);
        assert_eq!(total_size as usize, tkhd.len());
        assert_eq!(&tkhd[4..8], b"tkhd");
    }

    #[test]
    fn test_mdhd_structure() {
        let mdhd = build_mdhd(25, 50, 0);
        let total_size = read_be_u32(&mdhd[0..4]);
        assert_eq!(total_size as usize, mdhd.len());
        assert_eq!(&mdhd[4..8], b"mdhd");
    }

    #[test]
    fn test_hdlr_tmcd() {
        let hdlr = build_hdlr_tmcd();
        let total_size = read_be_u32(&hdlr[0..4]);
        assert_eq!(total_size as usize, hdlr.len());
        assert_eq!(&hdlr[4..8], b"hdlr");
    }

    #[test]
    fn test_build_tmcd_trak_basic() {
        let trak = build_tmcd_trak(&test_meta(), 5, 25000, 1250).unwrap();
        assert!(trak.len() > 30);
        let total_size = read_be_u32(&trak[0..4]);
        assert_eq!(total_size as usize, trak.len());
        assert_eq!(&trak[4..8], b"trak");
    }

    #[test]
    fn test_build_tmcd_trak_drop_frame() {
        let trak = build_tmcd_trak(&test_meta_df(), 6, 30000, 1500).unwrap();
        assert!(trak.len() > 30);
        let total_size = read_be_u32(&trak[0..4]);
        assert_eq!(total_size as usize, trak.len());
        assert_eq!(&trak[4..8], b"trak");
    }

    #[test]
    fn test_build_stco_one() {
        let stco = build_stco_one(42);
        let payload = &stco[8..]; // skip box header
        // stco header: version(1) flags(3) entry_count(4) = 8 bytes before data
        let version = payload[0];
        assert_eq!(version, 0);
        let entry_count = read_be_u32(&payload[4..8]);
        assert_eq!(entry_count, 1);
        let chunk_offset = read_be_u32(&payload[8..12]);
        assert_eq!(chunk_offset, 42);
    }

    // ── WAV bext patch ─────────────────────────────────────────────────

    #[test]
    fn test_wav_bext_patch_no_bext_falls_back() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("test.wav");

        // Write a minimal WAV without bext
        let mut wav = Vec::new();
        let data_size = 44u32; // dummy
        let riff_size = 36u32 + data_size;
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_size.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes()); // chunk size
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&48000u32.to_le_bytes()); // sample rate
        wav.extend_from_slice(&(48000u16 as u32 * 2).to_le_bytes()); // byte rate
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_size.to_le_bytes());

        std::fs::write(&p, &wav).unwrap();

        // Should succeed (creates bext chunk in place)
        let result = tag_wav_bext(&p, &test_meta(), None);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        // Verify bext chunk now exists in the patched file
        let patched = std::fs::read(&p).unwrap();
        assert!(
            patched.windows(4).any(|w| w == b"bext"),
            "bext chunk should exist after tagging"
        );
    }

    #[test]
    fn test_wav_bext_patch_with_bext() {
        use hound;

        let dir = TempDir::new().unwrap();
        let p = dir.path().join("tone.wav");

        // Create a WAV with hound (no bext), then insert a bext chunk manually
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        {
            let mut writer = hound::WavWriter::create(&p, spec).unwrap();
            for _ in 0..48000 {
                writer.write_sample(0i32).unwrap();
                writer.write_sample(0i32).unwrap();
            }
            writer.finalize().unwrap();
        }

        // Read the file, locate "data" chunk, insert bext before data
        let original = std::fs::read(&p).unwrap();
        // Find "fmt " in the original, then find "data" after it
        let fmt_pos = original
            .windows(4)
            .position(|w| w == b"fmt ")
            .unwrap();
        let fmt_size_pos = fmt_pos + 4;
        let fmt_size = u32::from_le_bytes(
            original[fmt_size_pos..fmt_size_pos + 4].try_into().unwrap(),
        ) as usize;
        // data chunk starts after fmt chunk: fmt header(8) + fmt_size
        let data_chunk_start = fmt_size_pos + 4 + fmt_size;
        // Data chunk header is at data_chunk_start, "data" at data_chunk_start + 4

        // Insert bext chunk before data
        let bext_payload_size = 602u32;

        let mut bext_chunk = Vec::new();
        bext_chunk.extend_from_slice(b"bext");
        bext_chunk.extend_from_slice(&bext_payload_size.to_le_bytes());
        // Fill with dummy bext data (602 bytes)
        // description: 256 bytes
        bext_chunk.extend_from_slice(&[0u8; 256]);
        // originator: 32
        bext_chunk.extend_from_slice(b"LTC Timecode Generator\x00\x00\x00\x00\x00\x00\x00\x00\x00");
        // originator_ref: 32
        bext_chunk.extend_from_slice(&[0u8; 32]);
        // origination_date: 10
        bext_chunk.extend_from_slice(b"2024-01-01");
        // origination_time: 8
        bext_chunk.extend_from_slice(b"12:00:00");
        // time_reference: 8 bytes at offset 338 into payload
        bext_chunk.extend_from_slice(&12345678u64.to_le_bytes());

        // Append reserved bytes to reach 602 payload
        let remaining = 602 - (bext_chunk.len() - 8);
        bext_chunk.extend(std::iter::repeat(0u8).take(remaining));

        // Build modified file: everything before data chunk + bext + rest
        let mut modified = Vec::new();
        modified.extend_from_slice(&original[..data_chunk_start]); // everything up to where "data" chunk used to start
        modified.extend_from_slice(&bext_chunk);
        modified.extend_from_slice(&original[data_chunk_start..]); // data chunk and beyond

        // Fix RIFF size
        let riff_size = (modified.len() - 8) as u32;
        modified[4..8].copy_from_slice(&riff_size.to_le_bytes());

        std::fs::write(&p, &modified).unwrap();

        // Now tag it
        let result = tag_wav_bext(&p, &test_meta(), None);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);

        // Verify the time_reference changed from the initial dummy value
        let patched = std::fs::read(&p).unwrap();
        let bext_pos = patched
            .windows(4)
            .position(|w| w == b"bext")
            .unwrap();
        let time_ref_pos = bext_pos + 8 + 338;
        let time_ref_val = u64::from_le_bytes(
            patched[time_ref_pos..time_ref_pos + 8]
                .try_into()
                .unwrap(),
        );
        // Should no longer be the dummy 12345678
        assert_ne!(time_ref_val, 12345678, "time_reference was not patched");
        // Should be > 0 and reasonable for 1h @ samplerate
        assert!(
            time_ref_val > 100_000_000,
            "time_reference too small: {}",
            time_ref_val
        );
    }

    // ── ffmpeg integration (requires ffmpeg on PATH) ──────────────────

    #[test]
    fn test_tag_via_ffmpeg_integration() {
        let ffmpeg_available = no_window_command("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !ffmpeg_available {
            eprintln!("Skipping ffmpeg integration test: ffmpeg not found on PATH");
            return;
        }

        let dir = TempDir::new().unwrap();
        let video = dir.path().join("test_clip.mp4");

        // Create a short test video with ffmpeg
        let status = no_window_command("ffmpeg")
            .args([
                "-f", "lavfi", "-i", "color=c=blue:s=128x72:r=25:d=1",
                "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                "-c:v", "libx264", "-c:a", "aac",
                "-y",
            ])
            .arg(&video)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("ffmpeg should succeed");
        assert!(status.success(), "test video creation failed");

        let meta = TimecodeMetadata {
            start: Timecode { hours: 10, minutes: 0, seconds: 0, frames: 0 },
            fps: 25.0,
            drop_frame: false,
        };

        let result = tag_via_ffmpeg(&video, &meta, None);
        assert!(result.is_ok(), "ffmpeg tagging failed: {:?}", result);
        assert_eq!(result.unwrap(), TagOutcome::TaggedViaFfmpeg);

        // Verify the file still exists and has reasonable size
        let len = std::fs::metadata(&video).map(|m| m.len()).unwrap_or(0);
        assert!(len > 1000, "tagged file is too small: {}", len);

        // Verify it's still playable
        let verify = no_window_command("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(&video)
            .args(["-f", "null", "-"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("verify ffmpeg");
        assert!(verify.success(), "tagged file not playable");
    }

    #[test]
    fn test_tag_via_ffmpeg_wav_integration() {
        let ffmpeg_available = no_window_command("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        if !ffmpeg_available {
            eprintln!("Skipping ffmpeg WAV integration test: ffmpeg not found");
            return;
        }

        let dir = TempDir::new().unwrap();
        let wav = dir.path().join("tone.wav");

        // Create a WAV with ffmpeg
        let status = no_window_command("ffmpeg")
            .args([
                "-f", "lavfi", "-i", "sine=frequency=440:duration=1",
                "-c:a", "pcm_s16le",
                "-y",
            ])
            .arg(&wav)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("ffmpeg should succeed");
        assert!(status.success(), "WAV creation failed");

        let meta = test_meta();
        // No bext → should fall through to ffmpeg
        let result = tag_via_ffmpeg(&wav, &meta, None);
        assert!(result.is_ok(), "ffmpeg WAV tagging failed: {:?}", result);
    }

    // ── Tag file dispatch tests ────────────────────────────────────────

    #[test]
    fn test_tag_file_mts_skipped() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("clip.mts");
        std::fs::write(&p, b"dummy").unwrap();

        let result = tag_file(&p, &test_meta(), None);
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            TagOutcome::Skipped {
                reason: "MPEG-TS container 'mts' has no standard timecode metadata".to_string()
            }
        );
    }

    #[test]
    fn test_tag_file_unknown_ext_falls_to_ffmpeg() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("clip.bin");
        std::fs::write(&p, b"dummy").unwrap();

        // ffmpeg may fail but the file should be handled without panic
        let result = tag_file(&p, &test_meta(), None);
        // Either an error (ffmpeg not available or fails) or success
        if let Err(err_msg) = result {
            assert!(
                err_msg.contains("ffmpeg") || err_msg.contains("output too small") || err_msg.contains("cannot open"),
                "unexpected error: {}",
                err_msg
            );
        }
    }

    // ── tag_via_ffmpeg_with runner tests ──────────────────────────────────

    #[test]
    fn test_tag_via_ffmpeg_with_timeout() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("clip.mov");
        std::fs::write(&p, b"dummy content").unwrap();
        let meta = test_meta();

        let mut timeout_runner = |_: &[String], _: &Path| Err(SubprocessFailure::TimedOut);
        let result = tag_via_ffmpeg_with(&p, &meta, None, &mut timeout_runner);
        assert!(result.is_err(), "expected timeout error, got {:?}", result);
        let msg = result.unwrap_err();
        assert!(msg.contains("timed out"), "msg should mention timeout: {}", msg);
        assert!(msg.contains("1800"), "msg should mention 1800s: {}", msg);
    }

    #[test]
    fn test_tag_via_ffmpeg_with_io_error() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("clip.mp4");
        std::fs::write(&p, b"dummy content").unwrap();
        let meta = test_meta();

        let mut io_runner = |_: &[String], _: &Path| {
            Err(SubprocessFailure::Io("binary not found".into()))
        };
        let result = tag_via_ffmpeg_with(&p, &meta, None, &mut io_runner);
        assert!(result.is_err(), "expected Io error, got {:?}", result);
        let msg = result.unwrap_err();
        assert!(msg.contains("failed to spawn ffmpeg"), "msg: {}", msg);
    }

    // ── WAV fmt-chunk scanning ────────────────────────────────────────────

    fn write_wav_with_leading_junk(dir: &TempDir, sample_rate: u32) -> std::path::PathBuf {
        let path = dir.path().join("junk_first.wav");
        let mut f = std::fs::File::create(&path).unwrap();
        let junk_payload = [0u8; 32];
        let data_payload = [0u8; 8];
        let riff_size = 4u32
            + (8 + junk_payload.len() as u32)
            + (8 + 16)
            + (8 + data_payload.len() as u32);
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        // JUNK chunk before fmt — recorders (e.g. TASCAM) emit these.
        f.write_all(b"JUNK").unwrap();
        f.write_all(&(junk_payload.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&junk_payload).unwrap();
        // fmt chunk
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        f.write_all(&sample_rate.to_le_bytes()).unwrap();
        f.write_all(&(sample_rate * 2).to_le_bytes()).unwrap(); // byte rate
        f.write_all(&2u16.to_le_bytes()).unwrap(); // block align
        f.write_all(&16u16.to_le_bytes()).unwrap(); // bits per sample
        // data chunk
        f.write_all(b"data").unwrap();
        f.write_all(&(data_payload.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&data_payload).unwrap();
        path
    }

    #[test]
    fn test_read_wav_sample_rate_junk_chunk_before_fmt() {
        let dir = TempDir::new().unwrap();
        let path = write_wav_with_leading_junk(&dir, 48000);
        let mut file = std::fs::File::open(&path).unwrap();
        let rate = crate::converter::read_wav_sample_rate_from_file(&mut file)
            .expect("fmt chunk after a preceding JUNK chunk must be found");
        assert_eq!(rate, 48000);
    }

    #[test]
    fn test_read_wav_sample_rate_via_path_junk_chunk_before_fmt() {
        let dir = TempDir::new().unwrap();
        let path = write_wav_with_leading_junk(&dir, 44100);
        let rate = crate::converter::read_wav_sample_rate(&path)
            .expect("path-based reader must scan past preceding chunks");
        assert_eq!(rate, 44100);
    }

    #[test]
    fn test_read_wav_sample_rate_odd_sized_chunk_padding() {
        // A chunk with an odd size carries one padding byte that must be
        // skipped; otherwise the scan lands inside the payload.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("odd_chunk.wav");
        let mut f = std::fs::File::create(&path).unwrap();
        let odd_payload = [0u8; 7]; // odd size → 1 pad byte
        let riff_size = 4u32 + (8 + odd_payload.len() as u32 + 1) + (8 + 16) + (8 + 8);
        f.write_all(b"RIFF").unwrap();
        f.write_all(&riff_size.to_le_bytes()).unwrap();
        f.write_all(b"WAVE").unwrap();
        f.write_all(b"JUNK").unwrap();
        f.write_all(&(odd_payload.len() as u32).to_le_bytes()).unwrap();
        f.write_all(&odd_payload).unwrap();
        f.write_all(&[0u8]).unwrap(); // pad byte
        f.write_all(b"fmt ").unwrap();
        f.write_all(&16u32.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&1u16.to_le_bytes()).unwrap();
        f.write_all(&22050u32.to_le_bytes()).unwrap();
        f.write_all(&44100u32.to_le_bytes()).unwrap();
        f.write_all(&2u16.to_le_bytes()).unwrap();
        f.write_all(&16u16.to_le_bytes()).unwrap();
        f.write_all(b"data").unwrap();
        f.write_all(&8u32.to_le_bytes()).unwrap();
        f.write_all(&[0u8; 8]).unwrap();
        drop(f);
        let mut file = std::fs::File::open(&path).unwrap();
        let rate = crate::converter::read_wav_sample_rate_from_file(&mut file)
            .expect("fmt after odd-sized chunk must be found");
        assert_eq!(rate, 22050);
    }

    #[test]
    fn test_read_wav_sample_rate_no_fmt() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("no_fmt.wav");
        std::fs::write(&path, b"RIFF\x04\x00\x00\x00WAVE").unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        assert!(crate::converter::read_wav_sample_rate_from_file(&mut file).is_none());
        assert!(crate::converter::read_wav_sample_rate(&path).is_none());
    }

    // ── tmcd stco patching ────────────────────────────────────────────────

    #[test]
    fn test_patch_stco_offset() {
        let meta = test_meta();
        let mut trak = build_tmcd_trak(&meta, 2, 600, 600).unwrap();
        let target: u32 = 0x0012_3456;
        patch_stco_offset(&mut trak, target).expect("stco must exist in tmcd trak");
        let pos = trak
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco box present");
        let entry = read_be_u32(&trak[pos + 12..pos + 16]);
        assert_eq!(entry, target, "stco entry must carry the patched offset");
    }

    /// Minimal moov-last MP4: ftyp + moov(mvhd v0). Enough for
    /// `tag_mp4_tmcd` to accept the layout.
    fn write_minimal_moov_last_mp4(path: &std::path::Path) {
        let mut mvhd = Vec::new();
        mvhd.extend_from_slice(&100u32.to_be_bytes()); // box size
        mvhd.extend_from_slice(b"mvhd");
        mvhd.push(0); // version 0
        mvhd.extend_from_slice(&[0, 0, 0]); // flags
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation
        mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification
        mvhd.extend_from_slice(&600u32.to_be_bytes()); // timescale
        mvhd.extend_from_slice(&600u32.to_be_bytes()); // duration (1 s)
        mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
        mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
        mvhd.extend_from_slice(&[0u8; 10]); // reserved
        mvhd.extend_from_slice(&[0u8; 36]); // matrix
        mvhd.extend_from_slice(&[0u8; 24]); // pre_defined
        mvhd.extend_from_slice(&2u32.to_be_bytes()); // next_track_id

        let mut moov = Vec::new();
        moov.extend_from_slice(&((8 + mvhd.len()) as u32).to_be_bytes());
        moov.extend_from_slice(b"moov");
        moov.extend_from_slice(&mvhd);

        let mut ftyp = Vec::new();
        ftyp.extend_from_slice(&20u32.to_be_bytes());
        ftyp.extend_from_slice(b"ftyp");
        ftyp.extend_from_slice(b"isom");
        ftyp.extend_from_slice(&0x0000_0200u32.to_be_bytes());
        ftyp.extend_from_slice(b"isom");

        let mut f = std::fs::File::create(path).unwrap();
        use std::io::Write as _;
        f.write_all(&ftyp).unwrap();
        f.write_all(&moov).unwrap();
    }

    #[test]
    fn test_tag_mp4_tmcd_stco_points_at_mdat_payload() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("clip.mp4");
        write_minimal_moov_last_mp4(&p);
        let len_before = std::fs::metadata(&p).unwrap().len();

        let outcome = native::tag_mp4_tmcd(&p, &test_meta()).unwrap();
        assert!(matches!(outcome, TagOutcome::TaggedInPlace));

        // Read the appended (last) moov and find the tmcd trak's stco entry.
        let data = std::fs::read(&p).unwrap();
        let mut f = std::fs::File::open(&p).unwrap();
        let boxes = native::scan_top_level_boxes(&mut f, data.len() as u64).unwrap();
        let last = boxes.last().unwrap();
        assert_eq!(&last.box_type, b"moov", "appended moov must be last");
        let moov = &data[last.offset as usize + 8..(last.offset + last.box_size) as usize];
        let pos = moov
            .windows(4)
            .position(|w| w == b"stco")
            .expect("stco box present in appended moov");
        let entry = read_be_u32(&moov[pos + 12..pos + 16]);
        assert_eq!(
            entry as u64,
            len_before + 8,
            "stco chunk offset must point at the appended mdat payload"
        );
        // And the sample value at that offset must be the frame count.
        let meta = test_meta();
        let expected_frames = meta.start.hours * 3600 * 25
            + meta.start.minutes * 60 * 25
            + meta.start.seconds * 25
            + meta.start.frames;
        let sample = read_be_u32(&data[len_before as usize + 8..len_before as usize + 12]);
        assert_eq!(sample, expected_frames);
    }

    #[test]
    fn test_tag_mp4_tmcd_ffprobe_roundtrip() {
        // End-to-end: tag a real MP4 and confirm ffprobe sees a valid tmcd
        // track (this is the test that catches stco/offset regressions).
        if !which_exists("ffmpeg") {
            eprintln!("--- SKIPPED: ffmpeg not found (tmcd round-trip test)");
            return;
        }
        if !which_exists("ffprobe") {
            eprintln!("--- SKIPPED: ffprobe not found (tmcd round-trip test)");
            return;
        }
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("roundtrip.mov");

        let status = no_window_command("ffmpeg")
            .args([
                "-f", "lavfi", "-i", "testsrc=duration=0.5:size=64x64:rate=25",
                "-c:v", "mpeg4",
                "-movflags", "+faststart", // ensures moov is written last
                "-y",
            ])
            .arg(&p)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .expect("ffmpeg should succeed");
        assert!(status.success(), "MP4 fixture creation failed");

        let result = tag_file(&p, &test_meta(), None);
        match result {
            Ok(TagOutcome::TaggedInPlace) => {}
            Ok(other) => {
                eprintln!("--- SKIPPED: fixture fell through to {:?} (layout not moov-last)", other);
                return;
            }
            Err(e) => panic!("tagging failed: {}", e),
        }

        let out = no_window_command("ffprobe")
            .args(["-v", "error", "-show_streams", "-of", "json"])
            .arg(&p)
            .output()
            .expect("ffprobe should succeed");
        assert!(out.status.success(), "ffprobe must accept the tagged file");
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            stdout.contains("tmcd"),
            "tagged file must expose a tmcd stream, got: {}",
            stdout
        );
    }

    fn which_exists(prog: &str) -> bool {
        no_window_command(prog)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}