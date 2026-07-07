/**
 * @license
 * SPDX-License-Identifier: Apache-2.0
 */

export interface Timecode {
  hours: number;
  minutes: number;
  seconds: number;
  frames: number;
}

export interface FrameRateOption {
  id: string;
  name: string;
  fps: number;
  dropFrame: boolean;
  description: string;
}

export interface ClapLogItem {
  id: string;
  timestamp: string;
  timecode: string;
  milliseconds: string;
  notes?: string;
}

export type AudioChannel = "left" | "right" | "both";

export interface AudioSettings {
  ltcChannel: AudioChannel;
  beepChannel: AudioChannel;
  ltcVolume: number;
  beepVolume: number;
  beepFrequency: number;
}
