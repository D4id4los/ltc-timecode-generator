export interface AudioDeviceInfo {
  id: string;
  name: string;
  is_default: boolean;
}

export interface TimecodeData {
  hours: number;
  minutes: number;
  seconds: number;
  frames: number;
}

function tauriInvoke(cmd: string, args?: Record<string, unknown>): Promise<any> {
  const w = window as any;
  // Tauri v2: low-level internal IPC bridge (always injected, no npm dep).
  if (w.__TAURI_INTERNALS__ && typeof w.__TAURI_INTERNALS__.invoke === 'function') {
    return w.__TAURI_INTERNALS__.invoke(cmd, args);
  }
  // Tauri v1: global API exposed when build.withGlobalTauri is true.
  if (w.__TAURI__ && typeof w.__TAURI__.invoke === 'function') {
    return w.__TAURI__.invoke(cmd, args);
  }
  return Promise.reject(new Error('Not running in Tauri'));
}

export function isTauri(): boolean {
  if (typeof window === 'undefined') return false;
  const w = window as any;
  return w.__TAURI_INTERNALS__ !== undefined || w.__TAURI__ !== undefined;
}

export type AudioBackendType = 'tauri' | 'web';

export function getAudioBackendType(): AudioBackendType {
  return isTauri() ? 'tauri' : 'web';
}

// ── Device enumeration ────────────────────────────────────────────────────

export async function getAudioDevices(): Promise<AudioDeviceInfo[]> {
  if (isTauri()) {
    const devices: AudioDeviceInfo[] = await tauriInvoke('get_audio_devices');
    return devices;
  }
  try {
    if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
      const allDevices = await navigator.mediaDevices.enumerateDevices();
      return allDevices
        .filter((d) => d.kind === 'audiooutput')
        .map((d) => ({
          id: d.deviceId,
          name: d.label || `Output Device (${d.deviceId.slice(0, 8)}...)`,
          is_default: d.deviceId === 'default',
        }));
    }
  } catch (err) {
    console.warn('enumerateDevices failed:', err);
  }
  return [];
}

export async function requestAudioPermission(): Promise<boolean> {
  if (isTauri()) {
    return true;
  }
  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    stream.getTracks().forEach((t) => t.stop());
    return true;
  } catch (err) {
    console.warn('Audio permission request failed:', err);
    return false;
  }
}

export async function audioOutputSinkSupported(): Promise<boolean> {
  if (isTauri()) {
    return true;
  }
  return typeof AudioContext !== 'undefined' &&
    typeof (AudioContext.prototype as any).setSinkId === 'function';
}

export async function selectAudioOutputNative(): Promise<string | null> {
  if (isTauri()) {
    return null;
  }
  if (
    navigator.mediaDevices &&
    typeof (navigator.mediaDevices as any).selectAudioOutput === 'function'
  ) {
    try {
      const device = await (navigator.mediaDevices as any).selectAudioOutput();
      return device ? device.deviceId : null;
    } catch {
      return null;
    }
  }
  return null;
}

// ── Audio output (Tauri backend) ──────────────────────────────────────────

export async function initAudioOutput(
  deviceId: string,
  sampleRate: number,
  bufferSize?: number
): Promise<void> {
  if (isTauri()) {
    await tauriInvoke('init_audio_output', {
      deviceId,
      sampleRate,
      bufferSize: bufferSize ?? 0,
    });
  }
}

export async function stopAudioOutput(): Promise<void> {
  if (isTauri()) {
    await tauriInvoke('stop_audio_output');
  }
}

export async function pushAudioSamples(samples: Float32Array): Promise<void> {
  if (isTauri()) {
    const arr: number[] = new Array(samples.length);
    for (let i = 0; i < samples.length; i++) {
      arr[i] = samples[i];
    }
    await tauriInvoke('push_audio_samples', { samples: arr });
  }
}

export async function playBeep(
  sampleRate: number,
  frequency: number,
  duration: number,
  volume: number,
  channel: string
): Promise<void> {
  if (isTauri()) {
    await tauriInvoke('play_beep', { sampleRate, frequency, duration, volume, channel });
  }
}

// ── LTC stream control (Tauri backend) ────────────────────────────────────

export async function startLtcStream(
  tc: TimecodeData,
  fps: number,
  dropFrame: boolean,
  ltcChannel: string,
  ltcVolume: number
): Promise<void> {
  if (isTauri()) {
    await tauriInvoke('start_ltc_stream', { tc, fps, dropFrame, ltcChannel, ltcVolume });
  }
}

export async function stopLtcStream(): Promise<void> {
  if (isTauri()) {
    await tauriInvoke('stop_ltc_stream');
  }
}

export async function resetLtcStream(tc: TimecodeData): Promise<void> {
  if (isTauri()) {
    await tauriInvoke('reset_ltc_stream', { tc });
  }
}

export async function getCurrentTimecode(): Promise<TimecodeData> {
  if (isTauri()) {
    return await tauriInvoke('get_current_timecode');
  }
  return { hours: 0, minutes: 0, seconds: 0, frames: 0 };
}