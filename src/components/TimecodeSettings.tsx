/**
 * @license
 * SPDX-License-Identifier: Apache-2.0
 */

import React from "react";
import { Timecode, FrameRateOption, AudioSettings, AudioChannel } from "../types";
import { Sliders, Volume2, Music, ToggleLeft, Activity, Info, Speaker, Laptop } from "lucide-react";

interface TimecodeSettingsProps {
  startTimecode: Timecode;
  setStartTimecode: (tc: Timecode) => void;
  selectedFps: FrameRateOption;
  setSelectedFps: (fps: FrameRateOption) => void;
  audioSettings: AudioSettings;
  setAudioSettings: (settings: AudioSettings) => void;
  frameRateOptions: FrameRateOption[];
  isPlaying: boolean;
  selectedSinkId: string;
  onSinkIdChange: (sinkId: string) => void;
}

function TimecodeSettings({
  startTimecode,
  setStartTimecode,
  selectedFps,
  setSelectedFps,
  audioSettings,
  setAudioSettings,
  frameRateOptions,
  isPlaying,
  selectedSinkId,
  onSinkIdChange,
}: TimecodeSettingsProps) {
  const handleTimecodeChange = (field: keyof Timecode, val: number) => {
    if (isPlaying) return; // Prevent changing start time while streaming

    let newVal = val;
    const maxFrames = Math.ceil(selectedFps.fps);

    if (field === "hours") {
      newVal = (newVal + 24) % 24;
    } else if (field === "minutes" || field === "seconds") {
      newVal = (newVal + 60) % 60;
    } else if (field === "frames") {
      newVal = (newVal + maxFrames) % maxFrames;
    }

    setStartTimecode({
      ...startTimecode,
      [field]: newVal,
    });
  };

  const handleAudioSettingChange = <K extends keyof AudioSettings>(
    key: K,
    value: AudioSettings[K]
  ) => {
    setAudioSettings({
      ...audioSettings,
      [key]: value,
    });
  };

  const [devices, setDevices] = React.useState<MediaDeviceInfo[]>([]);
  const [permissionGranted, setPermissionGranted] = React.useState<boolean>(false);
  const [isRequestingPermission, setIsRequestingPermission] = React.useState<boolean>(false);

  React.useEffect(() => {
    let active = true;
    const getDevices = async () => {
      try {
        if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
          const allDevices = await navigator.mediaDevices.enumerateDevices();
          const outputs = allDevices.filter((d) => d.kind === "audiooutput");
          if (active) {
            setDevices(outputs);
            const hasLabels = outputs.some((d) => d.label);
            setPermissionGranted(hasLabels);
          }
        }
      } catch (err) {
        console.warn("enumerateDevices failed:", err);
      }
    };

    getDevices();

    if (navigator.mediaDevices && navigator.mediaDevices.addEventListener) {
      navigator.mediaDevices.addEventListener("devicechange", getDevices);
      return () => {
        active = false;
        navigator.mediaDevices.removeEventListener("devicechange", getDevices);
      };
    }
  }, []);

  const requestAudioPermission = async () => {
    setIsRequestingPermission(true);
    try {
      const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
      stream.getTracks().forEach((track) => track.stop());
      
      if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
        const allDevices = await navigator.mediaDevices.enumerateDevices();
        const outputs = allDevices.filter((d) => d.kind === "audiooutput");
        setDevices(outputs);
        const hasLabels = outputs.some((d) => d.label);
        setPermissionGranted(hasLabels);
      }
    } catch (err) {
      console.warn("Audio permission request failed:", err);
    } finally {
      setIsRequestingPermission(false);
    }
  };

  const handleNativePicker = async () => {
    if (navigator.mediaDevices && (navigator.mediaDevices as any).selectAudioOutput) {
      try {
        const device = await (navigator.mediaDevices as any).selectAudioOutput();
        if (device) {
          onSinkIdChange(device.deviceId);
        }
      } catch (err) {
        console.log("Native output picker canceled or failed:", err);
      }
    }
  };

  const pad = (num: number) => num.toString().padStart(2, "0");

  return (
    <div id="tc-settings-panel" className="bg-card-bg border-2 border-border-main rounded-2xl p-6 shadow-2xl space-y-8">
      {/* 1. START TIMECODE STEPPERS */}
      <div className="space-y-4">
        <div className="flex items-center justify-between">
          <h3 className="text-xs font-bold tracking-wider text-text-muted uppercase flex items-center gap-2">
            <Sliders className="w-4 h-4 text-[#FF5F1F]" />
            Set Starting Timecode
          </h3>
          {isPlaying && (
            <span className="text-xs bg-amber-500/10 text-amber-400 px-2.5 py-1 rounded-full border border-amber-500/20 uppercase font-mono tracking-wider">
              Stop stream to edit starting time
            </span>
          )}
        </div>

        <div className={`grid grid-cols-4 gap-3 md:gap-4 max-w-lg ${isPlaying ? "opacity-30 pointer-events-none" : ""}`}>
          {(["hours", "minutes", "seconds", "frames"] as const).map((field) => {
            const val = startTimecode[field];
            return (
              <div key={field} className="flex flex-col items-center bg-deep-bg rounded-xl p-2 border border-border-main">
                <button
                  id={`btn-up-${field}`}
                  onClick={() => handleTimecodeChange(field, val + 1)}
                  className="w-full py-2.5 flex justify-center text-text-muted hover:text-[#FF5F1F] active:bg-card-bg hover:bg-nested-hover rounded-lg transition-all text-xl font-bold touch-manipulation cursor-pointer"
                  title={`Increase ${field}`}
                >
                  ▲
                </button>
                <div className="my-1 text-center">
                  <div className="text-3xl md:text-4xl font-mono font-bold text-text-title select-none">
                    {pad(val)}
                  </div>
                  <span className="text-[10px] font-mono text-text-muted uppercase tracking-widest mt-1 block font-semibold">
                    {field}
                  </span>
                </div>
                <button
                  id={`btn-down-${field}`}
                  onClick={() => handleTimecodeChange(field, val - 1)}
                  className="w-full py-2.5 flex justify-center text-text-muted hover:text-[#FF5F1F] active:bg-card-bg hover:bg-nested-hover rounded-lg transition-all text-xl font-bold touch-manipulation cursor-pointer"
                  title={`Decrease ${field}`}
                >
                  ▼
                </button>
              </div>
            );
          })}
        </div>
      </div>

      {/* 2. FRAMERATE SELECTION */}
      <div className="space-y-4">
        <h3 className="text-xs font-bold tracking-wider text-text-muted uppercase flex items-center gap-2">
          <Activity className="w-4 h-4 text-[#FF5F1F]" />
          Select Frame Rate
        </h3>
        <div className="grid grid-cols-1 sm:grid-cols-2 md:grid-cols-5 gap-3">
          {frameRateOptions.map((opt) => {
            const isSelected = selectedFps.id === opt.id;
            return (
              <button
                key={opt.id}
                id={`btn-fps-${opt.id}`}
                onClick={() => !isPlaying && setSelectedFps(opt)}
                disabled={isPlaying}
                className={`flex flex-col items-start p-4 rounded-xl border-2 text-left transition-all touch-manipulation ${
                  isSelected
                    ? "bg-[#FF5F1F]/10 border-[#FF5F1F] text-[#FF5F1F] font-semibold shadow-md shadow-[#FF5F1F]/10"
                    : "bg-deep-bg border-border-main text-text-muted hover:border-text-muted hover:text-text-title"
                } ${isPlaying ? "opacity-50 cursor-not-allowed" : "cursor-pointer"}`}
              >
                <span className={`text-lg font-mono font-bold ${isSelected ? "text-text-title" : ""}`}>{opt.name}</span>
                <span className="text-[11px] text-text-muted mt-1 line-clamp-2 leading-relaxed">
                  {opt.description}
                </span>
              </button>
            );
          })}
        </div>
      </div>

      {/* 3. SELECT OUTPUT AUDIO INTERFACE */}
      <div className="space-y-4">
        <div className="flex flex-col sm:flex-row sm:items-center justify-between gap-3">
          <h3 className="text-xs font-bold tracking-wider text-text-muted uppercase flex items-center gap-2">
            <Speaker className="w-4 h-4 text-[#FF5F1F]" />
            Output Audio Interface Selection
          </h3>
          <div className="flex flex-wrap gap-2">
            {navigator.mediaDevices && (navigator.mediaDevices as any).selectAudioOutput && (
              <button
                id="btn-native-picker"
                type="button"
                onClick={handleNativePicker}
                className="px-3 py-1 bg-deep-bg hover:bg-nested-hover border border-border-main text-[#FF5F1F] font-bold text-[10px] tracking-wider rounded-lg uppercase transition-all flex items-center gap-1.5 cursor-pointer touch-manipulation shadow-sm"
              >
                <Laptop className="w-3.5 h-3.5" /> Browser System Picker
              </button>
            )}
            {!permissionGranted && (
              <button
                id="btn-request-audio-names"
                type="button"
                onClick={requestAudioPermission}
                disabled={isRequestingPermission}
                className="px-3 py-1 bg-[#FF5F1F]/10 hover:bg-[#FF5F1F]/20 border border-[#FF5F1F]/20 text-[#FF5F1F] font-bold text-[10px] tracking-wider rounded-lg uppercase transition-all flex items-center gap-1.5 cursor-pointer disabled:opacity-50 touch-manipulation shadow-sm"
              >
                {isRequestingPermission ? "Authorizing..." : "Reveal Device Names"}
              </button>
            )}
          </div>
        </div>

        <div className="bg-deep-bg p-5 rounded-2xl border border-border-main space-y-4">
          <div className="space-y-2">
            <label htmlFor="select-audio-device" className="text-xs font-semibold text-text-muted block uppercase tracking-wider">
              Selected Audio Interface
            </label>
            <div className="relative">
              <select
                id="select-audio-device"
                value={selectedSinkId}
                onChange={(e) => onSinkIdChange(e.target.value)}
                className="w-full bg-card-bg border-2 border-border-main focus:border-[#FF5F1F] text-text-title rounded-xl px-4 py-3 text-sm font-mono tracking-wide appearance-none cursor-pointer focus:outline-none transition-all"
              >
                <option value="default">Default System Audio Output</option>
                {devices.map((device, idx) => (
                  <option key={device.deviceId || idx} value={device.deviceId}>
                    {device.label || `Physical Output Device ${idx + 1} (Unrevealed)`}
                  </option>
                ))}
              </select>
              <div className="absolute inset-y-0 right-4 flex items-center pointer-events-none text-zinc-500">
                ▼
              </div>
            </div>
          </div>

          <div className="text-[11px] text-text-muted leading-relaxed flex items-start gap-2 bg-card-bg/50 p-3.5 rounded-xl border border-border-main font-mono">
            <div className="w-1.5 h-1.5 rounded-full bg-[#FF5F1F] shrink-0 mt-1.5"></div>
            <div>
              <span className="text-text-title font-semibold">Active Mode:</span> Send linear timecode audio and clapper slates to specialized multi-channel mixers, USB-DAC sound cards, headphones, or sync adapters.
              {!permissionGranted && (
                <p className="mt-1 text-amber-500 font-sans">
                  ⚠️ Physical hardware device names are currently masked by browser security. Click "Reveal Device Names" to list.
                </p>
              )}
            </div>
          </div>
        </div>
      </div>

      {/* 4. AUDIO ROUTING & CHANNELS */}
      <div className="space-y-4">
        <h3 className="text-xs font-bold tracking-wider text-text-muted uppercase flex items-center gap-2">
          <ToggleLeft className="w-4 h-4 text-[#FF5F1F]" />
          Audio Routing & Settings (Dual-Channel Splits)
        </h3>

        <div className="grid grid-cols-1 lg:grid-cols-2 gap-6 bg-deep-bg p-5 rounded-2xl border border-border-main">
          {/* Channel Selectors */}
          <div className="space-y-5">
            {/* LTC Target Routing */}
            <div className="space-y-2">
              <label className="text-xs font-semibold text-text-muted block uppercase tracking-wider">
                LTC Audio Output Route
              </label>
              <div className="grid grid-cols-3 gap-2">
                {(["left", "right", "both"] as AudioChannel[]).map((ch) => {
                  const isActive = audioSettings.ltcChannel === ch;
                  return (
                    <button
                      key={ch}
                      id={`btn-ltc-ch-${ch}`}
                      onClick={() => handleAudioSettingChange("ltcChannel", ch)}
                      className={`py-2 text-xs font-bold font-mono rounded-lg border-2 capitalize transition-all touch-manipulation cursor-pointer ${
                        isActive
                          ? "bg-[#FF5F1F] text-black border-[#FF5F1F]"
                          : "bg-card-bg text-text-muted border-border-main hover:border-text-muted hover:text-text-title"
                      }`}
                    >
                      {ch}
                    </button>
                  );
                })}
              </div>
            </div>

            {/* Clapper Beep Target Routing */}
            <div className="space-y-2">
              <label className="text-xs font-semibold text-text-muted block uppercase tracking-wider">
                Digital Clapper Route
              </label>
              <div className="grid grid-cols-3 gap-2">
                {(["left", "right", "both"] as AudioChannel[]).map((ch) => {
                  const isActive = audioSettings.beepChannel === ch;
                  return (
                    <button
                      key={ch}
                      id={`btn-beep-ch-${ch}`}
                      onClick={() => handleAudioSettingChange("beepChannel", ch)}
                      className={`py-2 text-xs font-bold font-mono rounded-lg border-2 capitalize transition-all touch-manipulation cursor-pointer ${
                        isActive
                          ? "bg-[#FF5F1F] text-black border-[#FF5F1F]"
                          : "bg-card-bg text-text-muted border-border-main hover:border-text-muted hover:text-text-title"
                      }`}
                    >
                      {ch}
                    </button>
                  );
                })}
              </div>
            </div>
          </div>

          {/* Volume Adjusters */}
          <div className="space-y-5">
            {/* LTC Volume */}
            <div className="space-y-2">
              <div className="flex justify-between items-center text-xs">
                <span className="font-bold text-text-muted uppercase tracking-wider flex items-center gap-1.5">
                  <Volume2 className="w-3.5 h-3.5 text-[#FF5F1F]" />
                  LTC Volume Level
                </span>
                <span className="font-mono text-text-title font-bold">
                  {Math.round(audioSettings.ltcVolume * 100)}%
                </span>
              </div>
              <input
                id="slider-ltc-vol"
                type="range"
                min="0"
                max="1.0"
                step="0.05"
                value={audioSettings.ltcVolume}
                onChange={(e) => handleAudioSettingChange("ltcVolume", parseFloat(e.target.value))}
                className="w-full h-1.5 bg-card-bg rounded-lg appearance-none cursor-pointer accent-[#FF5F1F] touch-manipulation"
              />
            </div>

            {/* Beep Volume & Pitch */}
            <div className="space-y-4">
              <span className="font-bold text-text-muted uppercase tracking-wider flex items-center gap-1.5 text-xs">
                <Music className="w-3.5 h-3.5 text-[#FF5F1F]" />
                Clapper Beep Settings
              </span>
              
              <div className="space-y-3 bg-card-bg/30 p-3.5 rounded-xl border border-border-main">
                {/* Level (Volume) slider */}
                <div className="space-y-1.5">
                  <div className="flex justify-between items-center text-[11px]">
                    <span className="text-text-muted uppercase tracking-wider font-semibold flex items-center gap-1">
                      <Volume2 className="w-3.5 h-3.5 text-[#FF5F1F]" /> Beep Level (Volume)
                    </span>
                    <span className="font-mono text-text-title font-bold text-xs bg-card-bg px-2 py-0.5 rounded border border-border-main">
                      {Math.round(audioSettings.beepVolume * 100)}%
                    </span>
                  </div>
                  <input
                    id="slider-beep-vol"
                    type="range"
                    min="0"
                    max="1.0"
                    step="0.01"
                    value={audioSettings.beepVolume}
                    onChange={(e) => handleAudioSettingChange("beepVolume", parseFloat(e.target.value))}
                    className="w-full h-1.5 bg-card-bg rounded-lg appearance-none cursor-pointer accent-[#FF5F1F] touch-manipulation"
                  />
                </div>

                {/* Pitch (Frequency) slider */}
                <div className="space-y-1.5">
                  <div className="flex justify-between items-center text-[11px]">
                    <span className="text-text-muted uppercase tracking-wider font-semibold flex items-center gap-1">
                      <Activity className="w-3.5 h-3.5 text-[#FF5F1F]" /> Beep Pitch (Frequency)
                    </span>
                    <span className="font-mono text-text-title font-bold text-xs bg-card-bg px-2 py-0.5 rounded border border-border-main">
                      {audioSettings.beepFrequency} Hz
                    </span>
                  </div>
                  <input
                    id="slider-beep-pitch"
                    type="range"
                    min="400"
                    max="2000"
                    step="50"
                    value={audioSettings.beepFrequency}
                    onChange={(e) => handleAudioSettingChange("beepFrequency", parseInt(e.target.value))}
                    className="w-full h-1.5 bg-card-bg rounded-lg appearance-none cursor-pointer accent-[#FF5F1F] touch-manipulation"
                  />
                </div>
              </div>
            </div>
          </div>
        </div>

        {/* Informative split setup note */}
        <div className="bg-deep-bg/80 rounded-xl p-4 border border-border-main flex gap-3 text-xs text-text-muted leading-relaxed items-start">
          <Info className="w-4 h-4 text-[#FF5F1F] shrink-0 mt-0.5" />
          <div>
            <strong className="text-text-title">Pro-Tip for Indie Shoots:</strong> Map <strong className="text-text-title">LTC to Left</strong> and <strong className="text-[#FF5F1F]">Clapper to Right</strong>. Route the stereo output of your tablet into a DSLR or audio recorder. It writes LTC on Channel 1 (for auto-sync in DaVinci Resolve/Premiere) and a clear audible beep clapper on Channel 2 (for traditional scratch matching)!
          </div>
        </div>
      </div>
    </div>
  );
}

export default React.memo(TimecodeSettings);
