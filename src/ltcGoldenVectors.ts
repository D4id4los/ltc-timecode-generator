export interface GoldenVector {
  hours: number;
  minutes: number;
  seconds: number;
  frames: number;
  dropFrame: boolean;
  hex: string;
}

export const GOLDEN_VECTORS: GoldenVector[] = [
  { hours: 0, minutes: 0, seconds: 0, frames: 0, dropFrame: false, hex: "0000000000000000fcbf" },
  { hours: 1, minutes: 2, seconds: 3, frames: 4, dropFrame: false, hex: "0400030002000100fcbf" },
  { hours: 2, minutes: 1, seconds: 39, frames: 24, dropFrame: false, hex: "0402090301000200fcbf" },
  { hours: 2, minutes: 1, seconds: 40, frames: 0, dropFrame: false, hex: "0000000401000200fcbf" },
  { hours: 2, minutes: 1, seconds: 50, frames: 24, dropFrame: false, hex: "0402000501000200fcbf" },
  { hours: 2, minutes: 1, seconds: 59, frames: 24, dropFrame: false, hex: "0402090501000200fcbf" },
  { hours: 9, minutes: 59, seconds: 40, frames: 0, dropFrame: false, hex: "0000000409050900fcbf" },
  { hours: 12, minutes: 34, seconds: 56, frames: 18, dropFrame: false, hex: "0801060504030201fcbf" },
  { hours: 23, minutes: 59, seconds: 59, frames: 29, dropFrame: false, hex: "0902090509050302fcbf" },
  { hours: 0, minutes: 0, seconds: 0, frames: 0, dropFrame: true, hex: "0004000000000000fcbf" },
  { hours: 10, minutes: 15, seconds: 30, frames: 12, dropFrame: true, hex: "0205000305010001fcbf" },
];
