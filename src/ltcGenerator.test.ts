import { describe, expect, it } from "vitest";
import { getLTCBits, incrementTimecode, decrementTimecode, shiftTimecodeBack } from "./ltcGenerator";
import { GOLDEN_VECTORS } from "./ltcGoldenVectors";
import type { Timecode } from "./types";

function bitsToHex(bits: number[]): string {
  const bytes = new Array(10).fill(0);
  bits.forEach((b, i) => {
    bytes[Math.floor(i / 8)] |= b << (i % 8);
  });
  return bytes.map((b) => b.toString(16).padStart(2, "0")).join("");
}

function bitsToU8(slice: number[]): number {
  return slice.reduce((acc, b, i) => acc | (b << i), 0);
}

function decodeTimecode(bits: number[]): Timecode {
  const frames = bitsToU8(bits.slice(8, 10)) * 10 + bitsToU8(bits.slice(0, 4));
  const seconds = bitsToU8(bits.slice(24, 27)) * 10 + bitsToU8(bits.slice(16, 20));
  const minutes = bitsToU8(bits.slice(40, 43)) * 10 + bitsToU8(bits.slice(32, 36));
  const hours = bitsToU8(bits.slice(56, 58)) * 10 + bitsToU8(bits.slice(48, 52));
  return { hours, minutes, seconds, frames };
}

function toFrameNumber(tc: Timecode): number {
  return (tc.hours * 3600 + tc.minutes * 60 + tc.seconds) * 25 + tc.frames;
}

describe("getLTCBits golden vectors (Rust parity)", () => {
  it.each(GOLDEN_VECTORS)(
    "encodes $hours:$minutes:$seconds:$frames dropFrame=$dropFrame identically to audio-core",
    ({ hours, minutes, seconds, frames, dropFrame, hex }) => {
      const bits = getLTCBits(hours, minutes, seconds, frames, dropFrame);
      expect(bitsToHex(bits)).toBe(hex);
    }
  );
});

describe("getLTCBits seconds-tens field", () => {
  it("uses all 3 bits (24-26): bit 26 set for seconds 40-59", () => {
    for (let s = 40; s <= 59; s++) {
      const bits = getLTCBits(0, 0, s, 0, false);
      expect(bits[26]).toBe(1);
    }
  });

  it("round-trips every seconds value 0-59", () => {
    for (let s = 0; s <= 59; s++) {
      const bits = getLTCBits(2, 1, s, 12, false);
      expect(decodeTimecode(bits)).toEqual({ hours: 2, minutes: 1, seconds: s, frames: 12 });
    }
  });

  it("round-trips every minute 0-59 and hour 0-23", () => {
    for (let m = 0; m <= 59; m++) {
      const bits = getLTCBits(2, m, 39, 24, false);
      expect(decodeTimecode(bits)).toEqual({ hours: 2, minutes: m, seconds: 39, frames: 24 });
    }
    for (let h = 0; h <= 23; h++) {
      const bits = getLTCBits(h, 45, 39, 24, false);
      expect(decodeTimecode(bits)).toEqual({ hours: h, minutes: 45, seconds: 39, frames: 24 });
    }
  });
});

describe("encoded timecode sequence continuity", () => {
  it("advances exactly +1 frame across 2 minutes (no backward jumps at :39:24/:40:00)", () => {
    let tc: Timecode = { hours: 9, minutes: 59, seconds: 39, frames: 0 };
    let prev = toFrameNumber(tc);
    for (let i = 0; i < 2 * 60 * 25; i++) {
      tc = incrementTimecode(tc, 25, false);
      const bits = getLTCBits(tc.hours, tc.minutes, tc.seconds, tc.frames, false);
      const decoded = decodeTimecode(bits);
      expect(decoded).toEqual(tc);
      const n = toFrameNumber(decoded);
      expect(n).toBe(prev + 1);
      prev = n;
    }
  });
});

describe("decrementTimecode", () => {
  it("decrement is inverse of increment", () => {
    let tc: Timecode = { hours: 12, minutes: 34, seconds: 56, frames: 7 };
    for (let i = 0; i < 200; i++) {
      tc = incrementTimecode(tc, 25, false);
    }
    const back = shiftTimecodeBack(tc, 200 / 25, 25, false);
    expect(back).toEqual({ hours: 12, minutes: 34, seconds: 56, frames: 7 });
  });

  it("decrements NDF: shift back 5s = 125 frames", () => {
    const tc: Timecode = { hours: 2, minutes: 0, seconds: 0, frames: 0 };
    const out = shiftTimecodeBack(tc, 5.0, 25, false);
    expect(out.hours).toBe(1);
    expect(out.minutes).toBe(59);
    expect(out.seconds).toBe(55);
    expect(out.frames).toBe(0);
  });

  it("decrements DF: skips nonexistent frames at minute boundary", () => {
    // 29.97 DF: minute 1 starts at frame 2 — frames 0/1 don't exist.
    // Shifting back 2 frames from 01:01:00;02 must land on 01:00:59;29.
    const tc: Timecode = { hours: 1, minutes: 1, seconds: 0, frames: 2 };
    const out = shiftTimecodeBack(tc, 2 / 29.97, 29.97, true);
    expect(out.hours).toBe(1);
    expect(out.minutes).toBe(0);
    expect(out.seconds).toBe(59);
    expect(out.frames).toBe(29);
  });

  it("wraps midnight correctly", () => {
    const tc: Timecode = { hours: 0, minutes: 0, seconds: 0, frames: 0 };
    const out = shiftTimecodeBack(tc, 1 / 25, 25, false);
    expect(out.hours).toBe(23);
    expect(out.minutes).toBe(59);
    expect(out.seconds).toBe(59);
    expect(out.frames).toBe(24);
  });

  it("zero delta is identity", () => {
    const tc: Timecode = { hours: 1, minutes: 0, seconds: 0, frames: 12 };
    expect(shiftTimecodeBack(tc, 0, 25, false)).toEqual(tc);
    expect(shiftTimecodeBack(tc, -3, 25, false)).toEqual(tc);
  });
});
