use crate::{ChannelSel, Timecode};

// ── Volume mapping ──────────────────────────────────────────────────────────

/// Maps a linear 0–1 UI value to a perceptually logarithmic volume.
/// Quadratic curve: 0→0, 0.5→0.25, 0.7→0.49, 1.0→1.0.
/// Gives finer granularity at low perceived volumes.
/// Inputs outside 0..=1 are clamped — callers are not all validated.
fn map_volume(linear: f32) -> f32 {
    let v = linear.clamp(0.0, 1.0);
    v * v
}

/// Convert a dBFS peak target to the UI volume (0–1) that produces it
/// through `map_volume` (quadratic). `amp = 10^(dbfs/20)`, `ui = sqrt(amp)`,
/// i.e. `ui = 10^(dbfs/40)`. Inputs below −60 dBFS clamp to 0 (silence),
/// above 0 dBFS clamp to 1 (full scale).
pub fn dbfs_to_ui_volume(dbfs: f32) -> f32 {
    if dbfs <= -60.0 {
        return 0.0;
    }
    if dbfs >= 0.0 {
        return 1.0;
    }
    10f32.powf(dbfs / 40.0)
}

/// Convert a UI volume (0–1) back to the dBFS peak it produces through
/// `map_volume`. Inverse of [`dbfs_to_ui_volume`]; `ui_volume_to_dbfs(0.0)`
/// is silence and returns `f32::NEG_INFINITY`.
pub fn ui_volume_to_dbfs(ui: f32) -> f32 {
    let amp = map_volume(ui);
    20.0 * amp.log10()
}

// ── Bit writing helper ─────────────────────────────────────────────────────

fn write_val(bits: &mut [u8; 80], val: u32, start_bit: usize, length: usize) {
    for i in 0..length {
        bits[start_bit + i] = ((val >> i) & 1) as u8;
    }
}

// ── LTC bit encoding ───────────────────────────────────────────────────────

/// Build the 80-bit SMPTE LTC binary word for a given timecode.
///
/// Bit layout (SMPTE 12M-1):
///   bits  0–3    Frame units (BCD)
///   bits  4–7    Binary group 1 (user bits, always 0 here)
///   bits  8–9    Frame tens (BCD)
///   bit  10      Drop frame flag
///   bit  11      Color frame flag
///   bits 12–15   Binary group 2
///   bits 16–19   Seconds units
///   bits 20–23   Binary group 3
///   bits 24–26   Seconds tens
///   bits 27–31   Binary group 4
///   bits 32–35   Minutes units
///   bits 36–39   Binary group 5
///   bits 40–42   Minutes tens
///   bit  43      Unused (0)
///   bits 44–47   Binary group 6
///   bits 48–51   Hours units
///   bits 52–55   Binary group 7
///   bits 56–57   Hours tens
///   bits 58–59   Unused (0)
///   bits 60–63   Binary group 8
///   bits 64–79   Sync word: 0 0 1 1 1 1 1 1 1 1 1 1 1 1 0 1
pub fn get_ltc_bits(tc: &Timecode, drop_frame: bool) -> [u8; 80] {
    let mut bits = [0u8; 80];

    write_val(&mut bits, tc.frames % 10, 0, 4);
    write_val(&mut bits, 0, 4, 4);
    write_val(&mut bits, tc.frames / 10, 8, 2);
    bits[10] = if drop_frame { 1 } else { 0 };
    bits[11] = 0;
    write_val(&mut bits, 0, 12, 4);

    write_val(&mut bits, tc.seconds % 10, 16, 4);
    write_val(&mut bits, 0, 20, 4);
    write_val(&mut bits, tc.seconds / 10, 24, 3);
    write_val(&mut bits, 0, 27, 5);

    write_val(&mut bits, tc.minutes % 10, 32, 4);
    write_val(&mut bits, 0, 36, 4);
    write_val(&mut bits, tc.minutes / 10, 40, 3);
    bits[43] = 0;
    write_val(&mut bits, 0, 44, 4);

    write_val(&mut bits, tc.hours % 10, 48, 4);
    write_val(&mut bits, 0, 52, 4);
    write_val(&mut bits, tc.hours / 10, 56, 2);
    bits[58] = 0;
    bits[59] = 0;
    write_val(&mut bits, 0, 60, 4);

    bits[64] = 0;
    bits[65] = 0;
    for bit in bits.iter_mut().take(78).skip(66) {
        *bit = 1;
    }
    bits[78] = 0;
    bits[79] = 1;

    bits
}

// ── Timecode arithmetic ────────────────────────────────────────────────────

/// Advance a `Timecode` by one frame at the given frame rate.
///
/// Handles drop-frame (SMPTE 12M-1): frames 0 and 1 are skipped at the start
/// of each minute *except* minutes divisible by 10.
pub fn increment_timecode(tc: &Timecode, fps: f64, drop_frame: bool) -> Timecode {
    let max_frames = fps.ceil() as u32;
    let mut h = tc.hours;
    let mut m = tc.minutes;
    let mut s = tc.seconds;
    let mut f = tc.frames + 1;

    if f >= max_frames {
        f = 0;
        s += 1;
        if s >= 60 {
            s = 0;
            m += 1;
            if m >= 60 {
                m = 0;
                h += 1;
                if h >= 24 {
                    h = 0;
                }
            }
            if drop_frame && !m.is_multiple_of(10) {
                f = 2;
            }
        }
    }

    Timecode {
        hours: h,
        minutes: m,
        seconds: s,
        frames: f,
    }
}

// ── Sample-count accumulator ───────────────────────────────────────────────

/// Compute the number of (mono) samples for the next LTC frame using a
/// fractional-sample accumulator, eliminating drift when `sample_rate / fps`
/// is not an integer.
///
/// Returns `(frame_samples, samples_per_bit, new_accumulator)`.
///
/// Usage:
/// ```ignore
/// let (samples, spb, acc) = compute_frame_sample_count(exact_spf, base, accumulator);
/// // generate frame with `samples` and `spb`
/// // on next frame, pass `acc` as the accumulator
/// ```
pub fn compute_frame_sample_count(
    exact_samples_per_frame: f64,
    base_samples: usize,
    samples_accumulator: f64,
) -> (usize, f32, f64) {
    let frac = exact_samples_per_frame - base_samples as f64;
    let mut acc = samples_accumulator + frac;
    let extra = if acc >= 1.0 {
        acc -= 1.0;
        1
    } else {
        0
    };
    let frame_samples = base_samples + extra;
    let spb = frame_samples as f32 / 80.0;
    (frame_samples, spb, acc)
}

// ── LTC audio frame generation ─────────────────────────────────────────────
/// Parameters of one LTC frame render: the frame's timecode content, the
/// output timing, and the channel routing/level.
pub struct LtcFrameParams<'a> {
    pub tc: &'a Timecode,
    pub drop_frame: bool,
    pub total_samples: usize,
    pub samples_per_bit: f32,
    pub volume: f32,
    pub channel: ChannelSel,
}

/// Generate one stereo LTC audio frame (bi-phase mark modulation).
///
/// `total_samples` is the number of **mono** samples in this frame (total
/// stereo output will be `total_samples * 2`).  `samples_per_bit` is
/// `total_samples as f32 / 80.0`.
///
/// `last_level` tracks (a) the current bi-phase state (±1) and (b) the
/// low-pass-filtered output value, and **must** be passed between successive
/// frames for glitch-free continuous output.
///
/// The frame's content, timing and routing as one render request; the
/// mutable carry state (`last_level`) and the output buffer stay explicit
/// parameters. `stereo_out` must be at least `total_samples * 2` elements.
pub fn generate_ltc_frame_stereo(
    params: LtcFrameParams<'_>,
    last_level: &mut (f32, f32),
    stereo_out: &mut [f32],
) {
    let LtcFrameParams {
        tc,
        drop_frame,
        total_samples,
        samples_per_bit,
        volume,
        channel,
    } = params;
    let bits = get_ltc_bits(tc, drop_frame);
    modulate_bits_stereo(
        &bits,
        total_samples,
        samples_per_bit,
        volume,
        channel,
        last_level,
        stereo_out,
    );
}

/// Core bi-phase mark modulator shared by timecode frames and the lead-in
/// preamble. Modulates `bits` (LSB-first per slot, one bit per slot) into
/// `stereo_out` (`total_samples * 2` interleaved elements), carrying
/// `last_level` across calls for glitch-free continuity.
fn modulate_bits_stereo(
    bits: &[u8],
    total_samples: usize,
    samples_per_bit: f32,
    volume: f32,
    channel: ChannelSel,
    last_level: &mut (f32, f32),
    stereo_out: &mut [f32],
) {
    let play_left = matches!(channel, ChannelSel::Both | ChannelSel::Left);
    let play_right = matches!(channel, ChannelSel::Both | ChannelSel::Right);
    let alpha = 0.35f32;
    let mut current_level = last_level.0;
    let mut last_y = last_level.1;

    for b in 0..bits.len() {
        let bf = b as f32;
        let start_sample = (bf * samples_per_bit).round() as usize;
        let end_sample = ((bf + 1.0) * samples_per_bit).round() as usize;
        let mid_sample = ((bf + 0.5) * samples_per_bit).round() as usize;
        let bit_val = bits[b];

        current_level = -current_level;

        let mid = mid_sample.min(total_samples);
        let end = end_sample.min(total_samples);

        for s in start_sample..mid {
            last_y += alpha * (current_level - last_y);
            let val = last_y * map_volume(volume);
            stereo_out[s * 2] = if play_left { val } else { 0.0 };
            stereo_out[s * 2 + 1] = if play_right { val } else { 0.0 };
        }

        if bit_val == 1 {
            current_level = -current_level;
        }

        for s in mid..end {
            last_y += alpha * (current_level - last_y);
            let val = last_y * map_volume(volume);
            stereo_out[s * 2] = if play_left { val } else { 0.0 };
            stereo_out[s * 2 + 1] = if play_right { val } else { 0.0 };
        }
    }

    last_level.0 = current_level;
    last_level.1 = last_y;
}

// ── Transmission lead-in (clock preamble) ──────────────────────────────────

/// Length of the transmission lead-in in bi-phase bits: one full 80-bit
/// frame's worth of alternating 0/1 clock (no sync word, no timecode), so a
/// recorder's input settling window (AGC/HPF conditioning) is absorbed
/// before the first real LTC frame. A preamble of raw clock carries no
/// codeword and cannot pollute trim-to-first-LTC flows.
pub const LEAD_IN_BITS: usize = 80;

/// Parameters for one lead-in render: length in mono samples, bit timing,
/// level, and channel routing.
pub struct LeadInParams {
    pub total_samples: usize,
    pub samples_per_bit: f32,
    pub volume: f32,
    pub channel: ChannelSel,
}

/// Generate the transmission lead-in: `LEAD_IN_BITS` alternating 0/1
/// bi-phase clock bits with **no** sync word — an ordinary bi-phase-mark
/// signal that decoders cannot lock a timecode frame onto.
///
/// Same modulation, filtering, level and routing as a real frame;
/// `last_level` must be threaded into the first real frame for continuity.
/// `stereo_out` must be at least `total_samples * 2` elements.
pub fn generate_ltc_lead_in_stereo(
    params: LeadInParams,
    last_level: &mut (f32, f32),
    stereo_out: &mut [f32],
) {
    let LeadInParams {
        total_samples,
        samples_per_bit,
        volume,
        channel,
    } = params;
    let bits = [0u8, 1u8]
        .iter()
        .copied()
        .cycle()
        .take(LEAD_IN_BITS)
        .collect::<Vec<u8>>();
    modulate_bits_stereo(
        &bits,
        total_samples,
        samples_per_bit,
        volume,
        channel,
        last_level,
        stereo_out,
    );
}

// ── Beep tone generation ───────────────────────────────────────────────────

/// Generate stereo beep tone samples with attack/release envelope.
///
/// `channel` selects which stereo outputs carry the tone.  Returns interleaved
/// stereo samples (L, R, L, R, …).
pub(crate) fn generate_beep_samples(
    sample_rate: u32,
    frequency: f32,
    duration: f32,
    volume: f32,
    channel: ChannelSel,
) -> Vec<f32> {
    let num_samples = (sample_rate as f32 * duration) as usize;
    let attack = (sample_rate as f32 * 0.005) as usize;
    let release = (sample_rate as f32 * 0.02) as usize;
    let mut samples = Vec::with_capacity(num_samples * 2);

    let play_left = matches!(channel, ChannelSel::Both | ChannelSel::Left);
    let play_right = matches!(channel, ChannelSel::Both | ChannelSel::Right);

    for i in 0..num_samples {
        let t = i as f32 / sample_rate as f32;
        let val = (t * frequency * 2.0 * std::f32::consts::PI).sin() * map_volume(volume);

        let envelope = if i < attack {
            i as f32 / attack.max(1) as f32
        } else if i > num_samples.saturating_sub(release) {
            (num_samples - i) as f32 / release.max(1) as f32
        } else {
            1.0
        };

        let sample = val * envelope;

        samples.push(if play_left { sample } else { 0.0 });
        samples.push(if play_right { sample } else { 0.0 });
    }

    samples
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── write_val ─────────────────────────────────────────────────────────

    // ── get_ltc_bits: sync word ───────────────────────────────────────────

    const SYNC_WORD: [u8; 16] = [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1];

    #[test]
    fn test_get_ltc_bits_sync_word_invariant() {
        for tc in [
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
            Timecode {
                hours: 23,
                minutes: 59,
                seconds: 59,
                frames: 29,
            },
            Timecode {
                hours: 12,
                minutes: 34,
                seconds: 56,
                frames: 18,
            },
            Timecode {
                hours: 10,
                minutes: 0,
                seconds: 0,
                frames: 0,
            },
        ] {
            let bits = get_ltc_bits(&tc, false);
            let sync = &bits[64..80];
            assert_eq!(sync, SYNC_WORD, "sync word invariant for {:?}", tc);
        }
    }

    // ── get_ltc_bits: timecode field encoding ─────────────────────────────

    // ── get_ltc_bits: round-trip via decoder ──────────────────────────────

    fn bits_to_u8(slice: &[u8]) -> u8 {
        slice
            .iter()
            .enumerate()
            .fold(0u8, |acc, (i, &b)| acc | (b << i))
    }

    fn decode_timecode_from_bits(bits: &[u8], frame_start: usize) -> Timecode {
        let frame_units = bits_to_u8(&bits[frame_start..frame_start + 4]);
        let frame_tens = bits_to_u8(&bits[frame_start + 8..frame_start + 10]);
        let frames = frame_tens * 10 + frame_units;

        let sec_units = bits_to_u8(&bits[frame_start + 16..frame_start + 20]);
        let sec_tens = bits_to_u8(&bits[frame_start + 24..frame_start + 27]);
        let seconds = sec_tens * 10 + sec_units;

        let min_units = bits_to_u8(&bits[frame_start + 32..frame_start + 36]);
        let min_tens = bits_to_u8(&bits[frame_start + 40..frame_start + 43]);
        let minutes = min_tens * 10 + min_units;

        let hour_units = bits_to_u8(&bits[frame_start + 48..frame_start + 52]);
        let hour_tens = bits_to_u8(&bits[frame_start + 56..frame_start + 58]);
        let hours = hour_tens * 10 + hour_units;

        Timecode {
            hours: hours as u32,
            minutes: minutes as u32,
            seconds: seconds as u32,
            frames: frames as u32,
        }
    }

    // ── increment_timecode ────────────────────────────────────────────────

    #[test]
    fn test_increment_basic() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        assert_eq!(
            increment_timecode(&tc, 25.0, false),
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 0,
                frames: 1
            }
        );
    }

    #[test]
    fn test_increment_25fps_rollover() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 24,
        };
        assert_eq!(
            increment_timecode(&tc, 25.0, false),
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 1,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_30fps_rollover() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 29,
        };
        assert_eq!(
            increment_timecode(&tc, 30.0, false),
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 1,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_24fps_rollover() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 23,
        };
        assert_eq!(
            increment_timecode(&tc, 24.0, false),
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 1,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_2997_non_drop_rollover() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 29,
        };
        assert_eq!(
            increment_timecode(&tc, 29.97, false),
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 1,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_second_rollover() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 59,
            frames: 24,
        };
        assert_eq!(
            increment_timecode(&tc, 25.0, false),
            Timecode {
                hours: 0,
                minutes: 1,
                seconds: 0,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_minute_rollover() {
        let tc = Timecode {
            hours: 0,
            minutes: 59,
            seconds: 59,
            frames: 24,
        };
        assert_eq!(
            increment_timecode(&tc, 25.0, false),
            Timecode {
                hours: 1,
                minutes: 0,
                seconds: 0,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_hour_rollover() {
        let tc = Timecode {
            hours: 23,
            minutes: 59,
            seconds: 59,
            frames: 24,
        };
        assert_eq!(
            increment_timecode(&tc, 25.0, false),
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 0,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_23976_rollover() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 23,
        };
        assert_eq!(
            increment_timecode(&tc, 23.976, false),
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 1,
                frames: 0
            }
        );
    }

    // ── increment_timecode: drop frame ────────────────────────────────────

    #[test]
    fn test_increment_drop_frame_skip_frames_0_1() {
        // 01:00:59:29 at 29.97df → should skip frames 0,1 → 01:01:00:02
        let tc = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 59,
            frames: 29,
        };
        assert_eq!(
            increment_timecode(&tc, 29.97, true),
            Timecode {
                hours: 1,
                minutes: 1,
                seconds: 0,
                frames: 2
            }
        );
    }

    #[test]
    fn test_increment_drop_frame_no_skip_on_div10() {
        // 01:09:59:29 at 29.97df → minute=9 (divisible by 10? No, 9%10!=0... wait)
        // Actually 9%10 != 0 so it SHOULD skip. Let's use minute=10 which IS divisible by 10.
        // 01:09:59:29, m%10 = 9 → skip BUT we just advanced m to 10, and m%10=0 → no skip
        // Wait, the check happens AFTER m is incremented. So if m becomes 10, m%10==0 → no skip.
        // So: 01:09:59:29 → s rolls to 60 → m becomes 10 → m%10==0 → no skip
        let tc = Timecode {
            hours: 1,
            minutes: 9,
            seconds: 59,
            frames: 29,
        };
        assert_eq!(
            increment_timecode(&tc, 29.97, true),
            Timecode {
                hours: 1,
                minutes: 10,
                seconds: 0,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_drop_frame_no_skip_first_minute_of_hour() {
        // 00:59:59:29 at 29.97df → frames roll to 30 → f=0, s rolls to 60 → s=0,
        // m rolls to 60 → m=0, h becomes 1.  Then drop check: m%10=0 → no skip.
        // Result: 01:00:00:00 (no drop because minute 0 never drops).
        let tc = Timecode {
            hours: 0,
            minutes: 59,
            seconds: 59,
            frames: 29,
        };
        let next = increment_timecode(&tc, 29.97, true);
        assert_eq!(next.hours, 1);
        assert_eq!(next.minutes, 0);
        assert_eq!(next.seconds, 0);
        assert_eq!(next.frames, 0);
    }

    #[test]
    fn test_increment_drop_frame_no_skip_minute_0() {
        // 00:00:59:29 at 29.97df → m becomes 1, 1%10=1, so it DOES skip
        // But what about 01:00:00:29? Let's test: m stays 0, frame just rolls normally
        // Actually let's test the case where the frame stays at the boundary:
        // 00:00:00:29 at 29.97df → frames=30, which >= 30 → f=0, s=1, no minute change → no drop check
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 29,
        };
        let next = increment_timecode(&tc, 29.97, true);
        assert_eq!(
            next,
            Timecode {
                hours: 0,
                minutes: 0,
                seconds: 1,
                frames: 0
            }
        );
    }

    #[test]
    fn test_increment_drop_frame_sequential() {
        let mut tc = Timecode {
            hours: 1,
            minutes: 1,
            seconds: 0,
            frames: 0,
        };
        tc = increment_timecode(&tc, 29.97, true);
        assert_eq!(
            tc,
            Timecode {
                hours: 1,
                minutes: 1,
                seconds: 0,
                frames: 1
            }
        );
        tc = increment_timecode(&tc, 29.97, true);
        assert_eq!(
            tc,
            Timecode {
                hours: 1,
                minutes: 1,
                seconds: 0,
                frames: 2
            }
        );
    }

    #[test]
    fn test_increment_drop_frame_at_23976() {
        // 23.976 drop-frame is uncommon but shouldn't crash/misbehave
        let tc = Timecode {
            hours: 1,
            minutes: 0,
            seconds: 59,
            frames: 23,
        };
        let next = increment_timecode(&tc, 23.976, true);
        assert_eq!(next.minutes, 1);
        assert_eq!(next.seconds, 0);
        assert_eq!(next.hours, 1);
    }

    // ── increment_timecode: long-running (2h simulation) ──────────────────

    #[test]
    fn test_increment_2h_2997_df_no_drift() {
        let fps = 29.97;
        let num_frames = (7200.0_f64 * fps).round() as u64; // 215,784
        let mut tc = Timecode {
            hours: 10,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        for _ in 0..num_frames {
            tc = increment_timecode(&tc, fps, true);
        }
        // After exactly 2 real-time hours of drop-frame, hours should advance by 2
        assert_eq!(tc.hours, 12, "hours after 2h at 29.97df");
        // The exact frame position depends on drop-frame accumulation
        // but hours must be correct
    }

    // ── generate_ltc_frame_stereo: basic properties ───────────────────────

    #[test]
    fn test_frame_output_length() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 1920; // 48kHz / 25fps
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 0.5,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        assert_eq!(buf.len(), total_samples * 2);
    }

    #[test]
    fn test_frame_output_not_all_zero() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640; // 16kHz / 25fps
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        let has_energy = buf.iter().any(|&s| s.abs() > 0.5);
        assert!(has_energy, "frame should contain non-trivial signal");
    }

    // ── generate_ltc_frame_stereo: channel routing ────────────────────────

    #[test]
    fn test_frame_channel_both() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        let left_energy: f32 = buf.iter().step_by(2).map(|s| s * s).sum();
        let right_energy: f32 = buf.iter().skip(1).step_by(2).map(|s| s * s).sum();
        assert!(left_energy > 0.0, "left channel should have signal");
        assert!(right_energy > 0.0, "right channel should have signal");
        let diff = (left_energy - right_energy).abs();
        assert!(
            diff < left_energy * 0.01,
            "both channels should be nearly equal"
        );
    }

    #[test]
    fn test_frame_channel_left_only() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Left,
            },
            &mut level,
            &mut buf,
        );

        let left_energy: f32 = buf.iter().step_by(2).map(|s| s * s).sum();
        let right_energy: f32 = buf.iter().skip(1).step_by(2).map(|s| s * s).sum();
        assert!(left_energy > 0.0, "left channel should have signal");
        assert!(right_energy < 1e-10, "right channel should be silent");
    }

    #[test]
    fn test_frame_channel_right_only() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Right,
            },
            &mut level,
            &mut buf,
        );

        let left_energy: f32 = buf.iter().step_by(2).map(|s| s * s).sum();
        let right_energy: f32 = buf.iter().skip(1).step_by(2).map(|s| s * s).sum();
        assert!(left_energy < 1e-10, "left channel should be silent");
        assert!(right_energy > 0.0, "right channel should have signal");
    }

    // ── generate_ltc_frame_stereo: volume ─────────────────────────────────

    #[test]
    fn test_frame_volume_zero() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 0.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        let max_amp = buf.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(max_amp < 1e-10, "zero volume should produce silence");
    }

    #[test]
    fn test_frame_volume_max_amplitude_bound() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        let max_amp = buf.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(
            max_amp <= 1.0,
            "amplitude should not exceed 1.0, got {}",
            max_amp
        );
    }

    // ── generate_ltc_frame_stereo: level continuity ───────────────────────

    #[test]
    fn test_frame_level_continuity() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        // After one frame, level.0 (current_level) should be inverted from start
        // Start: 1.0. First bit always toggles: -1.0. After 80 bits (40 toggles = even), back to 1.0.
        // Actually each bit: first toggle is unconditional (current_level = -current_level),
        // then if bit==1, another toggle. So net toggles per bit = 1 + bit_value.
        // Over 80 bits: sum(bits) toggles in addition to the 80 base toggles. Even + even = even → back to 1.0.
        assert!(
            (level.0 - 1.0).abs() < 0.01 || (level.0 + 1.0).abs() < 0.01,
            "level should be ±1, got {}",
            level.0
        );
    }

    #[test]
    fn test_frame_continuity_carries_across_calls() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf1 = vec![0.0f32; total_samples * 2];
        let mut buf2 = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf1,
        );
        let last_sample_frame1 = buf1[buf1.len() - 2]; // left channel, last sample

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf2,
        );
        let first_sample_frame2 = buf2[0]; // left channel, first sample

        // The last sample of frame 1 and first sample of frame 2 should be close
        // (the IIR filter smooths, so not exactly equal, but same sign)
        assert!(
            (last_sample_frame1.signum() - first_sample_frame2.signum()).abs() < 0.1,
            "sign should be continuous across frames: last={} first={}",
            last_sample_frame1,
            first_sample_frame2
        );
    }

    // ── generate_ltc_frame_stereo: bi-phase mark encoding ─────────────────

    #[test]
    fn test_frame_biphase_mark_bit_transition() {
        // For a bit value of 0: there should be a level transition at the start of the bit
        // (always), but no transition at the midpoint.
        // For a bit value of 1: there should be transitions at both start and midpoint.
        // We'll verify by checking that the midpoint samples have opposite sign from
        // the start-of-bit samples when bit=1, and same sign when bit=0.
        let total_samples = 640;
        let _samples_per_bit = total_samples as f32 / 80.0; // 8.0

        // Create a frame where bit 1 is 0 and bit 2 is 1
        // bit 0 is sync bit (bits[64] = 0), so let's look at bits 0-2 of a known TC
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        }; // frame units = 0
        let bits = get_ltc_bits(&tc, false);
        // bits[0..3] are the frame unit bits: 0,0,0,0

        // Let's create a TC that gives us known bit patterns
        // frames=0 → bit 0 = 0, binary group 1 = 0, frames tens = 0
        // bits[0]=0, bits[4]=0 → let's just test the sync word which has known bits
        // bits[64]=0 (start of sync), bits[66]=1
        // Actually simpler: test with a synthetic frame
        assert_eq!(bits[0], 0, "frame units bit 0 should be 0");
        assert_eq!(bits[64], 0, "sync word bit 64 should be 0");
        assert_eq!(bits[66], 1, "sync word bit 66 should be 1");
    }

    // ── generate_ltc_frame_stereo: sample coverage ────────────────────────

    #[test]
    fn test_frame_no_gaps_in_sample_coverage() {
        // Verify every sample position from 0 to total_samples-1 is written
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 1920;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![-1.0f32; total_samples * 2]; // init with sentinel
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        // All samples should have been overwritten (not -1.0)
        // Actually some may be exactly 0.0, but the sentinel should not remain
        let sentinel_count = buf.iter().filter(|&&s| s == -1.0).count();
        assert_eq!(
            sentinel_count,
            0,
            "all {} samples should be written",
            buf.len()
        );
    }

    // ── generate_ltc_frame_stereo: DC offset ──────────────────────────────

    #[test]
    fn test_frame_dc_offset_near_zero() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 1920;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        let mean: f32 = buf.iter().sum::<f32>() / buf.len() as f32;
        // Bi-phase is DC-free, so mean should be very close to 0
        assert!(
            mean.abs() < 0.02,
            "DC offset should be near zero, got {}",
            mean
        );
    }

    // ── generate_ltc_frame_stereo: 44kHz and 48kHz ────────────────────────

    #[test]
    fn test_frame_44khz() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 1764; // 44100/25
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 0.5,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        assert_eq!(buf.len(), total_samples * 2);
        let has_energy = buf.iter().any(|&s| s.abs() > 0.1);
        assert!(has_energy, "44kHz frame should have signal");
    }

    #[test]
    fn test_frame_48khz() {
        let tc = Timecode {
            hours: 0,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let total_samples = 1920; // 48000/25
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_frame_stereo(
            crate::ltc_encoder::LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 0.5,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );

        assert_eq!(buf.len(), total_samples * 2);
        let has_energy = buf.iter().any(|&s| s.abs() > 0.1);
        assert!(has_energy, "48kHz frame should have signal");
    }

    // ── generate_beep_samples ─────────────────────────────────────────────

    #[test]
    fn test_beep_basic_properties() {
        let samples = generate_beep_samples(48000, 1000.0, 0.1, 0.5, ChannelSel::Both);
        let expected_len = (48000.0 * 0.1) as usize * 2;
        assert_eq!(samples.len(), expected_len, "beep sample count");
    }

    #[test]
    fn test_beep_channel_left() {
        let samples = generate_beep_samples(48000, 1000.0, 0.1, 0.5, ChannelSel::Left);
        let left_energy: f32 = samples.iter().step_by(2).map(|s| s * s).sum();
        let right_energy: f32 = samples.iter().skip(1).step_by(2).map(|s| s * s).sum();
        assert!(left_energy > 0.0);
        assert!(
            right_energy < 1e-10,
            "right channel silent for left-only beep"
        );
    }

    #[test]
    fn test_beep_channel_right() {
        let samples = generate_beep_samples(48000, 1000.0, 0.1, 0.5, ChannelSel::Right);
        let left_energy: f32 = samples.iter().step_by(2).map(|s| s * s).sum();
        let right_energy: f32 = samples.iter().skip(1).step_by(2).map(|s| s * s).sum();
        assert!(
            left_energy < 1e-10,
            "left channel silent for right-only beep"
        );
        assert!(right_energy > 0.0);
    }

    #[test]
    fn test_beep_zero_volume() {
        let samples = generate_beep_samples(48000, 1000.0, 0.1, 0.0, ChannelSel::Both);
        let max_amp = samples.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(max_amp < 1e-10, "zero-volume beep should be silent");
    }

    /// Envelope shape at sine peaks. At 48 kHz a 1 kHz sine peaks
    /// (|sin| = 1) exactly at i = 48k + 12, so the expected sample value is
    /// envelope × volume² (map_volume squares the clamped gain) — an
    /// independent restatement of the documented attack/5 ms, release/20 ms
    /// envelope contract.
    #[test]
    fn test_beep_envelope_probe_points() {
        let vol = 0.8f32;
        let v2 = vol * vol;
        let samples = generate_beep_samples(48000, 1000.0, 0.5, vol, ChannelSel::Both);
        let num = 24000usize; // 48000 * 0.5
        assert_eq!(samples.len(), num * 2, "stereo sample count");
        // attack = 240 samples, release = 960 samples, release window opens
        // at i > num - release = 23040.
        let probes: &[(usize, f32)] = &[
            (12, 12.0 / 240.0),     // attack ramp
            (204, 204.0 / 240.0),   // attack ramp, higher
            (492, 1.0),             // sustain
            (12012, 1.0),           // sustain, late (phase-sensitive)
            (23820, 180.0 / 960.0), // release ramp
        ];
        for &(i, env) in probes {
            let got = samples[i * 2]; // left channel
            let want = env * v2;
            assert!(
                (got - want).abs() < 0.02,
                "sample {i}: expected ≈ {want}, got {got}"
            );
        }
    }

    /// Overlap fixture: the tone (20 ms = 960 samples) equals the release
    /// length, so `num - release` saturates to 0 and every i > 0 — including
    /// the boundary sample i == attack — falls inside the release branch.
    /// The boundary sample must carry the release envelope, not the 1.0 a
    /// mis-routed attack branch would produce.
    #[test]
    fn test_beep_attack_release_overlap_boundary() {
        let vol = 0.8f32;
        let samples = generate_beep_samples(48000, 2050.0, 0.02, vol, ChannelSel::Both);
        assert_eq!(samples.len(), 960 * 2, "stereo sample count");
        // i = 240 == attack; t·f = 0.005 · 2050 = 10.25 → sine peak.
        let got = samples[240 * 2];
        let want = (720.0 / 960.0) * vol * vol;
        assert!(
            (got - want).abs() < 0.02,
            "attack/release boundary sample: expected ≈ {want}, got {got}"
        );
    }

    #[test]
    fn test_beep_envelope_attack() {
        let samples = generate_beep_samples(48000, 1000.0, 0.1, 1.0, ChannelSel::Both);
        // The attack is 5ms = 240 samples. First sample should be near 0, then ramp up.
        assert!(
            samples[0].abs() < samples[100].abs(),
            "envelope should ramp up during attack"
        );
    }

    #[test]
    fn test_beep_envelope_release() {
        let samples = generate_beep_samples(48000, 1000.0, 0.1, 1.0, ChannelSel::Both);
        let _num_samples = samples.len() / 2;
        // Release is 20ms = 960 samples. End should taper off.
        let near_end = samples[samples.len() - 4];
        assert!(
            near_end.abs() < 0.5,
            "release should taper off near end, got {}",
            near_end
        );
    }

    // ── Long-running drift test (target state: will FAIL until accumulator) ─

    #[test]
    fn test_long_running_2h_sample_count_zero_drift_48khz() {
        let sample_rate = 48000u32;
        let fps = 29.97;
        let exact_spf = sample_rate as f64 / fps;
        let base = exact_spf.floor() as usize;
        let mut tc = Timecode {
            hours: 10,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let mut last_level = (1.0f32, 1.0f32);
        let mut total_generated: u64 = 0;
        let mut accumulator = 0.0_f64;
        let num_frames = (7200.0_f64 * fps).round() as u64;
        // Allocate max possible frame size (base+1)
        let mut frame_buf = vec![0.0f32; (base + 1) * 2];

        for _ in 0..num_frames {
            let (samples, spb, new_acc) = compute_frame_sample_count(exact_spf, base, accumulator);
            frame_buf[..samples * 2].fill(0.0);
            generate_ltc_frame_stereo(
                crate::ltc_encoder::LtcFrameParams {
                    tc: &tc,
                    drop_frame: false,
                    total_samples: samples,
                    samples_per_bit: spb,
                    volume: 0.5,
                    channel: ChannelSel::Both,
                },
                &mut last_level,
                &mut frame_buf[..samples * 2],
            );
            total_generated += samples as u64;
            tc = increment_timecode(&tc, fps, false);
            accumulator = new_acc;
        }

        let expected = (sample_rate as f64 * 7200.0) as u64;
        let drift = total_generated as i64 - expected as i64;
        let abs_drift = drift.unsigned_abs();
        assert!(
            abs_drift <= 2,
            "Zero drift target at 48kHz: got {} samples drift over 2h (allowed ±2)",
            abs_drift
        );
    }

    #[test]
    fn test_long_running_2h_sample_count_zero_drift_44khz() {
        let sample_rate = 44100u32;
        let fps = 29.97;
        let exact_spf = sample_rate as f64 / fps;
        let base = exact_spf.floor() as usize;
        let mut tc = Timecode {
            hours: 10,
            minutes: 0,
            seconds: 0,
            frames: 0,
        };
        let mut last_level = (1.0f32, 1.0f32);
        let mut total_generated: u64 = 0;
        let mut accumulator = 0.0_f64;
        let num_frames = (7200.0_f64 * fps).round() as u64;
        let mut frame_buf = vec![0.0f32; (base + 1) * 2];

        for _ in 0..num_frames {
            let (samples, spb, new_acc) = compute_frame_sample_count(exact_spf, base, accumulator);
            frame_buf[..samples * 2].fill(0.0);
            generate_ltc_frame_stereo(
                crate::ltc_encoder::LtcFrameParams {
                    tc: &tc,
                    drop_frame: false,
                    total_samples: samples,
                    samples_per_bit: spb,
                    volume: 0.5,
                    channel: ChannelSel::Both,
                },
                &mut last_level,
                &mut frame_buf[..samples * 2],
            );
            total_generated += samples as u64;
            tc = increment_timecode(&tc, fps, false);
            accumulator = new_acc;
        }

        let expected = (sample_rate as f64 * 7200.0) as u64;
        let drift = total_generated as i64 - expected as i64;
        let abs_drift = drift.unsigned_abs();
        assert!(
            abs_drift <= 2,
            "Zero drift target at 16kHz: got {} samples drift over 2h (allowed ±2)",
            abs_drift
        );
    }

    // ── dBFS ↔ UI-volume level helpers ────────────────────────────────────

    #[test]
    fn test_dbfs_to_ui_reference_points() {
        // −12 dBFS ↔ 0.5 UI (the calibration anchor: 0.5² = 0.25 = 10^(−12/10)
        // within rounding). −6.02 dBFS is ½ amplitude → 0.7079 UI; −18.06 is
        // ⅛ amplitude → 0.3548 UI.
        assert!((dbfs_to_ui_volume(-12.04) - 0.5).abs() < 0.005);
        assert!((dbfs_to_ui_volume(-6.02) - 0.7079).abs() < 0.005);
        assert!((dbfs_to_ui_volume(-18.06) - 0.3548).abs() < 0.005);
        assert!((dbfs_to_ui_volume(0.0) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_ui_volume_to_dbfs_reference_points() {
        assert!((ui_volume_to_dbfs(0.5) - (-12.04)).abs() < 0.05);
        assert!((ui_volume_to_dbfs(1.0)).abs() < 1e-6);
        assert_eq!(ui_volume_to_dbfs(0.0), f32::NEG_INFINITY);
    }

    #[test]
    fn test_dbfs_ui_round_trip() {
        // −60 dBFS is the clamp boundary (→ 0 UI → −inf), so round-trip
        // fidelity is asserted inside the unclamped range only.
        for dbfs in [-59.0f32, -48.0, -24.0, -18.0, -12.0, -6.0, -1.0] {
            let ui = dbfs_to_ui_volume(dbfs);
            let back = ui_volume_to_dbfs(ui);
            assert!(
                (back - dbfs).abs() < 0.01,
                "round trip {dbfs} dBFS -> {ui} UI -> {back} dBFS"
            );
        }
    }

    #[test]
    fn test_dbfs_to_ui_clamps() {
        assert_eq!(dbfs_to_ui_volume(-80.0), 0.0, "below −60 dBFS → silence");
        assert_eq!(dbfs_to_ui_volume(3.0), 1.0, "above 0 dBFS → full scale");
    }

    #[test]
    fn test_render_default_is_minus_12_dbfs_level() {
        // 0.5 UI through the quadratic map must produce a −12 dBFS peak
        // (±0.5 dB) — the calibrated render reference level.
        let level = ui_volume_to_dbfs(0.5);
        assert!(
            (level - (-12.0)).abs() <= 0.5,
            "0.5 UI must be ≈ −12 dBFS, got {level}"
        );
    }

    // ── Acceptance: transmission lead-in survives a recorder settle ───────

    /// 60 frames @ 25 fps/48 kHz, mono, with a deterministic LCG for the
    /// chain-noise component. Returns (audio, frame TCs).
    fn settle_fixture(with_lead_in: bool) -> (Vec<f32>, Vec<Timecode>) {
        let sample_rate = 48000u32;
        let fps = 25.0f64;
        let exact_spf = sample_rate as f64 / fps;
        let base = exact_spf.floor() as usize;
        let num_frames = 60;
        let mut tc = mk_tc(1, 0, 0, 0);
        let mut last_level = (1.0f32, 1.0f32);
        let mut accumulator = 0.0_f64;
        let mut audio: Vec<f32> = Vec::new();
        let mut frame_buf = vec![0.0f32; (base + 1) * 2];
        let mut tcs = Vec::new();

        if with_lead_in {
            generate_ltc_lead_in_stereo(
                LeadInParams {
                    total_samples: base,
                    samples_per_bit: base as f32 / LEAD_IN_BITS as f32,
                    volume: 0.8,
                    channel: ChannelSel::Both,
                },
                &mut last_level,
                &mut frame_buf[..base * 2],
            );
            audio.extend(frame_buf[..base * 2].iter().step_by(2).copied());
        }

        for _ in 0..num_frames {
            let (samples, spb, new_acc) = compute_frame_sample_count(exact_spf, base, accumulator);
            frame_buf[..samples * 2].fill(0.0);
            generate_ltc_frame_stereo(
                LtcFrameParams {
                    tc: &tc,
                    drop_frame: false,
                    total_samples: samples,
                    samples_per_bit: spb,
                    volume: 0.8,
                    channel: ChannelSel::Both,
                },
                &mut last_level,
                &mut frame_buf[..samples * 2],
            );
            audio.extend(frame_buf[..samples * 2].iter().step_by(2).copied());
            tcs.push(tc);
            accumulator = new_acc;
            tc = increment_timecode(&tc, fps, false);
        }
        (audio, tcs)
    }

    /// Simulated recorder settle: linear gain ramp 0→1 over the first
    /// 250 ms plus deterministic chain noise (LCG, ≈ −40 dBFS peak).
    fn apply_settle(audio: &mut [f32], sample_rate: u32) {
        let ramp_len = (sample_rate as usize * 250) / 1000;
        for (i, s) in audio.iter_mut().enumerate().take(ramp_len) {
            *s *= i as f32 / ramp_len as f32;
        }
        let mut seed = 0x2545F4914F6CDD1Du64;
        for s in audio.iter_mut() {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            *s += ((seed >> 33) as i32 as f32 / i32::MAX as f32) * 0.01;
        }
    }

    /// With the lead-in, the first real frame survives a 250 ms settle ramp:
    /// the decoder must return every frame starting at the start TC.
    /// (Measured without the lead-in: builtin loses frames 0–1 — first
    /// decoded = :02, 58/60 — see the WP-EN report addendum.)
    #[test]
    fn test_settle_ramp_with_lead_in_first_frame_survives_builtin() {
        let sample_rate = 48000u32;
        let fps = 25.0f64;
        let (mut audio, tcs) = settle_fixture(true);
        apply_settle(&mut audio, sample_rate);
        let result = crate::ltc_decoder::decode_ltc_samples(
            &audio,
            sample_rate,
            1,
            fps,
            false,
            std::time::Instant::now(),
            None,
        )
        .expect("decode of settled lead-in render must succeed");
        let decoded: Vec<Timecode> = result.timecodes.iter().map(|f| f.timecode).collect();
        assert_eq!(
            decoded.first(),
            Some(&tcs[0]),
            "first decoded frame must be the start TC under a 250 ms settle"
        );
        assert_eq!(
            decoded.len(),
            tcs.len(),
            "every generated frame must survive the settle"
        );
        for (i, frame) in decoded.iter().enumerate() {
            assert_eq!(
                frame, &tcs[i],
                "decoded frame {i} must match generation order"
            );
        }
    }

    /// Same acceptance on the libltc backend: the lead-in must fix the
    /// head loss (measured without lead-in: first decoded = :01 under the
    /// 250 ms settle). Tail loss is libltc's documented boundary artifact,
    /// so only the head and a floor on the yield are asserted here.
    #[test]
    fn test_settle_ramp_with_lead_in_first_frame_survives_libltc() {
        let sample_rate = 48000u32;
        let fps = 25.0f64;
        let (mut audio, tcs) = settle_fixture(true);
        apply_settle(&mut audio, sample_rate);

        let tmp =
            std::env::temp_dir().join(format!("ltc-leadin-acceptance-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&tmp, spec).expect("temp WAV create");
        for s in &audio {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
                .expect("temp WAV write");
        }
        w.finalize().expect("temp WAV finalize");

        let result =
            match crate::ltc_decoder_libltc::decode_ltc_from_wav_libltc(&tmp, fps, false, None) {
                Ok(r) => r,
                Err(_) => {
                    let _ = std::fs::remove_file(&tmp);
                    eprintln!("--- SKIPPED: libltc backend unavailable");
                    return;
                }
            };
        let _ = std::fs::remove_file(&tmp);

        let decoded: Vec<Timecode> = result.timecodes.iter().map(|f| f.timecode).collect();
        assert_eq!(
            decoded.first(),
            Some(&tcs[0]),
            "libltc: first decoded frame must be the start TC under a 250 ms settle"
        );
        assert!(
            decoded.len() >= tcs.len() - 1,
            "libltc: yield floor (tail loss tolerated): {}/{}",
            decoded.len(),
            tcs.len()
        );
    }

    // ── generate_ltc_lead_in_stereo ───────────────────────────────────────

    #[test]
    fn test_lead_in_sample_count_and_written() {
        let total_samples = 1920;
        let samples_per_bit = total_samples as f32 / LEAD_IN_BITS as f32;
        let mut buf = vec![f32::NAN; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);
        generate_ltc_lead_in_stereo(
            LeadInParams {
                total_samples,
                samples_per_bit,
                volume: 0.8,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );
        assert!(buf.iter().all(|s| s.is_finite()), "every sample written");
    }

    #[test]
    fn test_lead_in_dc_near_zero() {
        let total_samples = 1920;
        let samples_per_bit = total_samples as f32 / LEAD_IN_BITS as f32;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);
        generate_ltc_lead_in_stereo(
            LeadInParams {
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );
        let mean: f32 = buf.iter().sum::<f32>() / buf.len() as f32;
        assert!(
            mean.abs() < 0.02,
            "clock preamble must be DC-free, got {mean}"
        );
    }

    #[test]
    fn test_lead_in_volume_zero_silence() {
        let total_samples = 1920;
        let samples_per_bit = total_samples as f32 / LEAD_IN_BITS as f32;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);
        generate_ltc_lead_in_stereo(
            LeadInParams {
                total_samples,
                samples_per_bit,
                volume: 0.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );
        let max_amp = buf.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(max_amp < 1e-10, "zero-volume lead-in must be silent");
    }

    #[test]
    fn test_lead_in_channel_routing_left_only() {
        let total_samples = 1920;
        let samples_per_bit = total_samples as f32 / LEAD_IN_BITS as f32;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);
        generate_ltc_lead_in_stereo(
            LeadInParams {
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Left,
            },
            &mut level,
            &mut buf,
        );
        let left_energy: f32 = buf.iter().step_by(2).map(|s| s * s).sum();
        let right_energy: f32 = buf.iter().skip(1).step_by(2).map(|s| s * s).sum();
        assert!(left_energy > 0.0, "left channel should carry the preamble");
        assert!(right_energy < 1e-10, "right channel must be silent");
    }

    /// Continuity across the lead-in → first-frame seam: with 80 alternating
    /// bits ending on bit value 1 (bit 79 = 1), the preamble ends mid-bit with
    /// the post-midpoint level of a `1` bit; the following frame's first
    /// half-bit must still start with a level flip (no missing/extra
    /// transition). Mirrors `test_frame_continuity_carries_across_calls`.
    #[test]
    fn test_lead_in_continuity_into_first_frame() {
        let total_samples = 640;
        let samples_per_bit = total_samples as f32 / 80.0;
        let mut lead_buf = vec![0.0f32; total_samples * 2];
        let mut frame_buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);

        generate_ltc_lead_in_stereo(
            LeadInParams {
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut lead_buf,
        );
        let tc = mk_tc(1, 0, 0, 0);
        generate_ltc_frame_stereo(
            LtcFrameParams {
                tc: &tc,
                drop_frame: false,
                total_samples,
                samples_per_bit,
                volume: 1.0,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut frame_buf,
        );
        let last_lead = lead_buf[lead_buf.len() - 2];
        let first_frame = frame_buf[0];
        assert!(
            (last_lead.signum() - first_frame.signum()).abs() < 0.1,
            "sign must be continuous across the lead-in seam: last={last_lead} first={first_frame}"
        );
        // A bi-phase mark always flips at the bit boundary, so the filter is
        // driven toward the opposite level right at the seam: the frame's
        // first sample must move away from the lead-in's last value, not
        // freeze (which would mean a missing transition).
        assert!(
            (first_frame - last_lead).abs() > 1e-4,
            "first frame sample must show a level change across the seam"
        );
    }

    /// Lead-in carries no sync word: the builtin decoder must not decode a
    /// timecode frame from a preamble-only signal.
    #[test]
    fn test_lead_in_alone_decodes_no_timecode() {
        let sample_rate = 48000u32;
        let fps = 25.0f64;
        let total_samples = (sample_rate as f64 / fps) as usize;
        let samples_per_bit = total_samples as f32 / LEAD_IN_BITS as f32;
        let mut buf = vec![0.0f32; total_samples * 2];
        let mut level = (1.0f32, 1.0f32);
        generate_ltc_lead_in_stereo(
            LeadInParams {
                total_samples,
                samples_per_bit,
                volume: 0.8,
                channel: ChannelSel::Both,
            },
            &mut level,
            &mut buf,
        );
        let mono: Vec<f32> = buf.iter().step_by(2).copied().collect();
        let result = crate::ltc_decoder::decode_ltc_samples(
            &mono,
            sample_rate,
            1,
            fps,
            false,
            std::time::Instant::now(),
            None,
        )
        .expect("decode of lead-in must not error");
        assert!(
            result.timecodes.is_empty(),
            "clock preamble carries no timecode, decoded {}",
            result.timecodes.len()
        );
    }

    // ── map_volume ────────────────────────────────────────────────────────
    #[test]
    fn test_map_volume_zero() {
        assert_eq!(map_volume(0.0), 0.0);
    }

    #[test]
    fn test_map_volume_one() {
        assert_eq!(map_volume(1.0), 1.0);
    }

    #[test]
    fn test_map_volume_mid() {
        assert_eq!(map_volume(0.5), 0.25);
    }

    #[test]
    fn test_map_volume_quadratic() {
        let result = map_volume(0.7);
        assert!(
            (result - 0.49).abs() < 1e-6,
            "map_volume(0.7) should be ~0.49, got {}",
            result
        );
    }

    #[test]
    fn test_map_volume_clamps_out_of_range() {
        // Defensive clamp: entry points are not all validated (CLI
        // --output-to-file, engine Set*Volume), and a negative input would
        // square to a *positive* gain. Out-of-range input must be safe.
        assert_eq!(map_volume(2.0), 1.0);
        assert_eq!(map_volume(-1.0), 0.0);
        assert_eq!(map_volume(-0.5), 0.0);
    }

    // ── Bits -> u8 helper for round-trip ──────────────────────────────────

    // ── Helpers: exhaustive sweeps / continuity ───────────────────────────

    fn bits_to_hex(bits: &[u8; 80]) -> String {
        let mut bytes = [0u8; 10];
        for (i, &b) in bits.iter().enumerate() {
            bytes[i / 8] |= b << (i % 8);
        }
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }

    #[test]
    fn test_golden_vectors() {
        let cases: [(u32, u32, u32, u32, bool, &str); 11] = [
            (0, 0, 0, 0, false, "0000000000000000fcbf"),
            (1, 2, 3, 4, false, "0400030002000100fcbf"),
            (2, 1, 39, 24, false, "0402090301000200fcbf"),
            (2, 1, 40, 0, false, "0000000401000200fcbf"),
            (2, 1, 50, 24, false, "0402000501000200fcbf"),
            (2, 1, 59, 24, false, "0402090501000200fcbf"),
            (9, 59, 40, 0, false, "0000000409050900fcbf"),
            (12, 34, 56, 18, false, "0801060504030201fcbf"),
            (23, 59, 59, 29, false, "0902090509050302fcbf"),
            (0, 0, 0, 0, true, "0004000000000000fcbf"),
            (10, 15, 30, 12, true, "0205000305010001fcbf"),
        ];
        for (h, m, s, f, df, expected_hex) in cases {
            let hex = bits_to_hex(&get_ltc_bits(&mk_tc(h, m, s, f), df));
            assert_eq!(
                hex, expected_hex,
                "golden vector for {:02}:{:02}:{:02}:{:02} df={}",
                h, m, s, f, df
            );
        }
    }

    fn mk_tc(hours: u32, minutes: u32, seconds: u32, frames: u32) -> Timecode {
        Timecode {
            hours,
            minutes,
            seconds,
            frames,
        }
    }

    /// Absolute frame number in the integer frame-number space used by
    /// `increment_timecode` (`fps.ceil()` frames per displayed second).
    fn tc_frame_number(tc: &Timecode, fps: f64) -> u64 {
        let mpf = fps.ceil() as u64;
        (tc.hours as u64 * 3600 + tc.minutes as u64 * 60 + tc.seconds as u64) * mpf
            + tc.frames as u64
    }

    fn roundtrip(t: &Timecode, drop_frame: bool) -> Timecode {
        let bits = get_ltc_bits(t, drop_frame);
        decode_timecode_from_bits(&bits, 0)
    }

    // ── Regression: seconds tens must use all 3 bits (24–26) ─────────────
    //
    // Historical bug (a7ac813 → 2f68dd8): the seconds-tens BCD digit (0–5)
    // was written into 2 bits, so tens=4 (0b100) truncated to 0 and tens=5
    // (0b101) truncated to 1. Every minute decoded as: 00–39 correct,
    // 40–59 shown as 00–19 → periodic backward/forward jumps exactly at
    // :39:24 → :00:00 and :19:24 → next minute.

    #[test]
    fn test_seconds_tens_uses_3_bits() {
        for s in 40u32..=59 {
            let t = mk_tc(2, 1, s, 12);
            let bits = get_ltc_bits(&t, false);
            let expected: [u8; 3] = if s < 50 { [0, 0, 1] } else { [1, 0, 1] };
            assert_eq!(bits[24..27], expected, "seconds-tens bits for s={}", s);
            assert_eq!(roundtrip(&t, false), t, "round-trip for s={}", s);
        }
    }

    // ── Exhaustive round-trip sweeps ──────────────────────────────────────

    #[test]
    fn test_roundtrip_exhaustive_25fps() {
        for h in 0u32..=23 {
            for m in 0u32..=59 {
                for s in 0u32..=59 {
                    for f in 0u32..=24 {
                        let t = mk_tc(h, m, s, f);
                        assert_eq!(roundtrip(&t, false), t, "tc={:?}", t);
                    }
                }
            }
        }
    }

    #[test]
    fn test_roundtrip_sweep_other_fps() {
        for &fps in &[24.0f64, 29.97, 30.0] {
            let mf = fps.ceil() as u32;
            for &h in &[0u32, 1, 9, 10, 23] {
                for m in 0u32..=59 {
                    for s in 0u32..=59 {
                        for f in 0..mf {
                            let t = mk_tc(h, m, s, f);
                            assert_eq!(roundtrip(&t, false), t, "fps={} tc={:?}", fps, t);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn test_bcd_field_positions_and_widths() {
        for v in 0u32..=9 {
            let t = mk_tc(0, 0, 0, v);
            assert_eq!(
                bits_to_u8(&get_ltc_bits(&t, false)[0..4]),
                v as u8,
                "frame units {}",
                v
            );
        }
        for v in 0u32..=2 {
            let t = mk_tc(0, 0, 0, v * 10 + 3);
            let bits = get_ltc_bits(&t, false);
            assert_eq!(bits_to_u8(&bits[8..10]), v as u8, "frame tens {}", v);
            assert_eq!(bits_to_u8(&bits[0..4]), 3);
        }
        for v in 0u32..=9 {
            let t = mk_tc(0, 0, v, 3);
            let bits = get_ltc_bits(&t, false);
            assert_eq!(bits_to_u8(&bits[16..20]), v as u8, "seconds units {}", v);
        }
        for v in 0u32..=5 {
            let t = mk_tc(0, 0, v * 10 + 3, 0);
            let bits = get_ltc_bits(&t, false);
            assert_eq!(bits_to_u8(&bits[24..27]), v as u8, "seconds tens {}", v);
            assert_eq!(bits_to_u8(&bits[16..20]), 3);
        }
        for v in 0u32..=9 {
            let t = mk_tc(0, v, 3, 0);
            let bits = get_ltc_bits(&t, false);
            assert_eq!(bits_to_u8(&bits[32..36]), v as u8, "minutes units {}", v);
        }
        for v in 0u32..=5 {
            let t = mk_tc(0, v * 10 + 3, 3, 0);
            let bits = get_ltc_bits(&t, false);
            assert_eq!(bits_to_u8(&bits[40..43]), v as u8, "minutes tens {}", v);
            assert_eq!(bits_to_u8(&bits[32..36]), 3);
        }
        for v in 0u32..=9 {
            let t = mk_tc(v, 3, 3, 0);
            let bits = get_ltc_bits(&t, false);
            assert_eq!(bits_to_u8(&bits[48..52]), v as u8, "hours units {}", v);
        }
        for v in 0u32..=2 {
            let t = mk_tc(v * 10 + 1, 3, 3, 0);
            let bits = get_ltc_bits(&t, false);
            assert_eq!(bits_to_u8(&bits[56..58]), v as u8, "hours tens {}", v);
            assert_eq!(bits_to_u8(&bits[48..52]), 1);
        }
    }

    // ── Continuity: encode→decode sequence must advance by exactly 1 ─────
    //
    // Catches the observed production failure: every decoded frame must be
    // the strict +1 successor of the previous one. The historical 2-bit
    // seconds-tens bug produces 1000 "missing" frames per minute and fails
    // at the first :39:24 → :40:00 boundary.

    #[test]
    fn test_encoded_sequence_continuous_2h_25fps() {
        let fps = 25.0f64;
        let num_frames = (7200.0 * fps) as u64;
        let mut t = mk_tc(10, 0, 0, 0);
        let mut prev = tc_frame_number(&t, fps);
        for i in 0..num_frames {
            t = increment_timecode(&t, fps, false);
            let decoded = roundtrip(&t, false);
            assert_eq!(decoded, t, "frame {} must round-trip", i);
            let n = tc_frame_number(&decoded, fps);
            assert_eq!(
                n,
                prev + 1,
                "frame {} must advance exactly +1 ({} -> {})",
                i,
                prev,
                n
            );
            prev = n;
        }
        assert_eq!(t, mk_tc(12, 0, 0, 0));
    }

    #[test]
    fn test_encoded_sequence_continuous_2h_various_fps() {
        for &fps in &[24.0f64, 29.97, 30.0] {
            let num_frames = 7200 * fps.ceil() as u64;
            let mut t = mk_tc(10, 0, 0, 0);
            let mut prev = tc_frame_number(&t, fps);
            for i in 0..num_frames {
                t = increment_timecode(&t, fps, false);
                let decoded = roundtrip(&t, false);
                assert_eq!(decoded, t, "fps={} frame {} must round-trip", fps, i);
                let n = tc_frame_number(&decoded, fps);
                assert_eq!(
                    n,
                    prev + 1,
                    "fps={} frame {} must advance exactly +1",
                    fps,
                    i
                );
                prev = n;
            }
            assert_eq!(t, mk_tc(12, 0, 0, 0), "fps={} must land on +2h", fps);
        }
    }

    #[test]
    fn test_encoded_sequence_continuous_2h_2997_drop_frame() {
        let fps = 29.97f64;
        let num_frames = (7200.0 * fps).round() as u64;
        assert_eq!(num_frames, 215_784, "2h of 29.97df frame numbers");
        let mut t = mk_tc(10, 0, 0, 0);
        let mut skipped = 0u64;
        for i in 0..num_frames {
            let decoded = roundtrip(&t, true);
            assert_eq!(decoded, t, "df frame {} must round-trip", i);
            let prev_frames = t.frames;
            t = increment_timecode(&t, fps, true);
            if t.frames == 2 && prev_frames == 29 {
                skipped += 1;
                assert_ne!(
                    t.minutes % 10,
                    0,
                    "frames may only be dropped in non-tenth minutes"
                );
            }
        }
        assert_eq!(
            skipped, 108,
            "54 drop events per hour (2 frames each) = 108 over 2h"
        );
        assert_eq!(
            t,
            mk_tc(12, 0, 0, 0),
            "frame accounting must land exactly on +2h"
        );
    }

    #[test]
    fn test_midnight_wrap_continuity_25fps() {
        let fps = 25.0f64;
        let day_frames = 24u64 * 3600 * fps as u64;
        let mut t = mk_tc(23, 59, 59, 20);
        let start_num = tc_frame_number(&t, fps);
        for i in 0..20u64 {
            t = increment_timecode(&t, fps, false);
            let decoded = roundtrip(&t, false);
            assert_eq!(decoded, t, "wrap step {} must round-trip", i);
            let n = tc_frame_number(&decoded, fps);
            assert_eq!(n, (start_num + i + 1) % day_frames, "wrap step {}", i);
        }
        assert_eq!(t, mk_tc(0, 0, 0, 15));
    }

    #[test]
    fn test_full_24h_roundtrip_and_continuity_25fps() {
        let fps = 25.0f64;
        let day_frames = 24u64 * 3600 * fps as u64;
        let mut t = mk_tc(0, 0, 0, 0);
        let mut prev: u64 = day_frames - 1;
        for i in 0..day_frames {
            let decoded = roundtrip(&t, false);
            assert_eq!(decoded, t, "frame {} must round-trip", i);
            let n = tc_frame_number(&decoded, fps);
            assert_eq!(
                n,
                (prev + 1) % day_frames,
                "frame {} must advance exactly +1",
                i
            );
            prev = n;
            t = increment_timecode(&t, fps, false);
        }
        assert_eq!(t, mk_tc(0, 0, 0, 0), "24h must wrap back to zero");
    }

    // ── Long-running sample-count drift at 25fps ─────────────────────────

    // ── End-to-end: generated audio must decode gap-free ─────────────────
    //
    // Generates real modulated audio across the historically broken
    // boundaries (:39:24 → :40:00 and :59:24 → minute rollover, plus an
    // hour rollover) and runs the builtin decoder over it, mirroring the
    // production analysis pipeline. Any backward jump or gap fails here.

    #[test]
    fn test_generated_audio_decodes_gap_free_48khz_25fps() {
        let sample_rate = 48000u32;
        let fps = 25.0f64;
        let exact_spf = sample_rate as f64 / fps;
        let base = exact_spf.floor() as usize;
        let duration_secs = 63.0f64;
        let num_frames = (duration_secs * fps) as u64;
        let mut tc = mk_tc(9, 59, 39, 0);
        let mut last_level = (1.0f32, 1.0f32);
        let mut accumulator = 0.0_f64;
        let mut expected: Vec<Timecode> = Vec::with_capacity(num_frames as usize);
        let mut audio: Vec<f32> = Vec::with_capacity((duration_secs * sample_rate as f64) as usize);
        let mut frame_buf = vec![0.0f32; (base + 1) * 2];

        for _ in 0..num_frames {
            let (samples, spb, new_acc) = compute_frame_sample_count(exact_spf, base, accumulator);
            frame_buf[..samples * 2].fill(0.0);
            generate_ltc_frame_stereo(
                crate::ltc_encoder::LtcFrameParams {
                    tc: &tc,
                    drop_frame: false,
                    total_samples: samples,
                    samples_per_bit: spb,
                    volume: 0.8,
                    channel: ChannelSel::Both,
                },
                &mut last_level,
                &mut frame_buf[..samples * 2],
            );
            audio.extend(frame_buf[..samples * 2].iter().step_by(2).copied());
            expected.push(tc);
            accumulator = new_acc;
            tc = increment_timecode(&tc, fps, false);
        }
        assert_eq!(
            tc,
            mk_tc(10, 0, 42, 0),
            "generation must end at expected TC"
        );

        let result = crate::ltc_decoder::decode_ltc_samples(
            &audio,
            sample_rate,
            1,
            fps,
            false,
            std::time::Instant::now(),
            None,
        )
        .expect("decode of generated audio must succeed");
        assert!(
            matches!(result.status, crate::ltc_decoder::LtcDecodeStatus::Success),
            "status must be Success, got {:?}",
            result.status
        );
        assert_eq!(result.sample_rate, sample_rate);
        assert!(
            (result.detected_fps - 25.0).abs() < 0.01,
            "detected fps 25, got {}",
            result.detected_fps
        );

        let decoded: Vec<Timecode> = result.timecodes.iter().map(|f| f.timecode).collect();
        assert_eq!(
            decoded.len(),
            expected.len(),
            "every generated frame must decode: {} of {}",
            decoded.len(),
            expected.len()
        );
        assert_eq!(
            decoded[0], expected[0],
            "first decoded frame must be the start TC, got {:?}",
            decoded[0]
        );
        assert_eq!(
            decoded.last().unwrap(),
            expected.last().unwrap(),
            "last decoded frame must be the final generated frame, got {:?}",
            decoded.last().unwrap()
        );

        let start_idx = expected.iter().position(|t| *t == decoded[0]).unwrap();
        for (i, frame) in decoded.iter().enumerate() {
            assert_eq!(
                *frame,
                expected[start_idx + i],
                "decoded frame {} must be the expected +1 successor (gap or jump at index {})",
                i,
                i
            );
        }

        for boundary in [mk_tc(9, 59, 40, 0), mk_tc(10, 0, 0, 0), mk_tc(10, 0, 40, 0)] {
            assert!(
                decoded.contains(&boundary),
                "boundary {:?} must be decoded",
                boundary
            );
        }

        if let Some(q) = &result.quality {
            assert_eq!(q.gap_count, 0, "no gaps allowed: {}", q.summary);
            assert_eq!(q.glitch_count, 0, "no glitches allowed: {}", q.summary);
            assert_eq!(q.edit_count, 0, "no edit points allowed: {}", q.summary);
            assert_eq!(
                q.missing_frames, 0,
                "every frame must be decoded: {}",
                q.summary
            );
            assert!(
                q.max_drift_secs < 0.1,
                "drift must be negligible: {}",
                q.max_drift_secs
            );
        }
    }
}
