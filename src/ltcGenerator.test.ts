import { describe, expect, it } from "vitest";
import { getLTCBits, incrementTimecode } from "./ltcGenerator";
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
