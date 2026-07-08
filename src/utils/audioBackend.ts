export interface AudioDeviceInfo {
  id: string;
  name: string;
  is_default: boolean;
}

function tauriInvoke(cmd: string, args?: Record<string, unknown>): Promise<any> {
  const internals = (window as any).__TAURI_INTERNALS__;
  if (internals && typeof internals.invoke === 'function') {
    return internals.invoke(cmd, args);
  }
  return Promise.reject(new Error('Not running in Tauri'));
}

export function isTauri(): boolean {
  return (
    typeof window !== 'undefined' &&
    (window as any).__TAURI_INTERNALS__ !== undefined
  );
}

export type AudioBackendType = 'tauri' | 'web';

export function getAudioBackendType(): AudioBackendType {
  return isTauri() ? 'tauri' : 'web';
}

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

export async function pushAudioSamples(samples: Float32Array): Promise<void> {
  if (isTauri()) {
    const arr: number[] = new Array(samples.length);
    for (let i = 0; i < samples.length; i++) {
      arr[i] = samples[i];
    }
    await tauriInvoke('push_audio_samples', { samples: arr });
  }
}

export async function stopAudioOutput(): Promise<void> {
  if (isTauri()) {
    await tauriInvoke('stop_audio_output');
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