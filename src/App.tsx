/**
 * @license
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState, useEffect, useRef, useCallback } from "react";
import { Timecode, FrameRateOption, AudioSettings, ClapLogItem } from "./types";
import {
  incrementTimecode,
  timecodeToString,
  timecodeToMillisecondsString,
  generateLTCFrameBuffer,
  playClapperBeep,
} from "./ltcGenerator";
import TimecodeSettings from "./components/TimecodeSettings";
import ClapperSlate from "./components/ClapperSlate";
import FooterStatusBar from "./components/FooterStatusBar";
import {
  isTauri as isTauriApp,
  getAudioBackendType,
  initAudioOutput,
  stopAudioOutput as tauriStopAudio,
  playBeep as tauriPlayBeep,
  startLtcStream,
  stopLtcStream,
  resetLtcStream,
} from "./utils/audioBackend";
import { motion } from "motion/react";
import {
  Play,
  Square,
  RotateCcw,
  Lock,
  Unlock,
  Radio,
  Tv,
  HelpCircle,
  HelpCircle as QuestionIcon,
  Volume2,
  Info,
  Activity,
  Sun,
  Moon,
  Video,
} from "lucide-react";

const FRAME_RATE_OPTIONS: FrameRateOption[] = [
  { id: "24", name: "24 fps", fps: 24, dropFrame: false, description: "Standard cinema & film frame rate." },
  { id: "25", name: "25 fps", fps: 25, dropFrame: false, description: "PAL standard (Europe, UK, Australia, Africa, Asia)." },
  { id: "29.97nd", name: "29.97 ND", fps: 29.97, dropFrame: false, description: "NTSC Non-Drop (broadcast video & web production)." },
  { id: "29.97df", name: "29.97 DF", fps: 29.97, dropFrame: true, description: "NTSC Drop Frame (syncs clock drift to wall-time)." },
  { id: "30", name: "30 fps", fps: 30, dropFrame: false, description: "High-definition video rate / digital audio standard." },
];

export default function App() {
  // Theme state
  const [theme, setTheme] = useState<"dark" | "light">(() => {
    return (localStorage.getItem("theme") as "dark" | "light") || "dark";
  });

  useEffect(() => {
    const root = window.document.documentElement;
    if (theme === "dark") {
      root.classList.remove("light");
      root.classList.add("dark");
    } else {
      root.classList.remove("dark");
      root.classList.add("light");
    }
    localStorage.setItem("theme", theme);
  }, [theme]);

  // App states
  const [isPlaying, setIsPlaying] = useState<boolean>(false);
  const [isLocked, setIsLocked] = useState<boolean>(false);
  const [startTimecode, setStartTimecode] = useState<Timecode>({
    hours: 1,
    minutes: 0,
    seconds: 0,
    frames: 0,
  });
  const [currentTimecode, setCurrentTimecode] = useState<Timecode>({
    hours: 1,
    minutes: 0,
    seconds: 0,
    frames: 0,
  });
  const [selectedFps, setSelectedFps] = useState<FrameRateOption>(FRAME_RATE_OPTIONS[1]); // 25 fps PAL by default
  const [audioSettings, setAudioSettings] = useState<AudioSettings>({
    ltcChannel: "right",
    beepChannel: "left",
    ltcVolume: 0.7,
    beepVolume: 0.8,
    beepFrequency: 1000,
  });
  const [logs, setLogs] = useState<ClapLogItem[]>([]);
  const [activeTab, setActiveTab] = useState<"clapper" | "settings">("clapper");
  const [clapTriggerCount, setClapTriggerCount] = useState<number>(0);
  const [showFaq, setShowFaq] = useState<boolean>(false);
  const [isWakeLockActive, setIsWakeLockActive] = useState<boolean>(false);
  const [selectedSinkId, setSelectedSinkId] = useState<string>("default");

  const isTauriMode = getAudioBackendType() === "tauri";
  const tauriStartTimeRef = useRef<number>(0);
  const tauriAudioInitRef = useRef<boolean>(false);

  // Helper to apply Web Audio Sink / Output Interface selection
  const applyAudioSink = async (audioCtx: AudioContext, sinkId: string) => {
    if (audioCtx && typeof (audioCtx as any).setSinkId === "function") {
      try {
        const targetSinkId = sinkId === "default" ? "" : sinkId;
        await (audioCtx as any).setSinkId(targetSinkId);
        console.log(`Audio output sink set to: ${sinkId}`);
      } catch (err) {
        console.warn("Failed to set audio sink ID on AudioContext:", err);
      }
    }
  };

  // Re-apply sink selection when it changes on an active context
  useEffect(() => {
    if (isTauriMode) {
      if (audioCtxRef.current) {
        applyAudioSink(audioCtxRef.current, selectedSinkId);
      }
    } else {
      if (audioCtxRef.current) {
        applyAudioSink(audioCtxRef.current, selectedSinkId);
      }
    }
  }, [selectedSinkId, isTauriMode]);

  // Dynamic system and hardware state
  const [activeSampleRate, setActiveSampleRate] = useState<number>(48000);
  const [webAudioStatus, setWebAudioStatus] = useState<string>("WEBAUDIO: STANDBY");

  // Screen Wake Lock API ref
  const wakeLockRef = useRef<any>(null);

  const acquireWakeLock = async () => {
    if ("wakeLock" in navigator) {
      try {
        // Release existing one first just in case
        if (wakeLockRef.current) {
          await wakeLockRef.current.release();
        }
        const lock = await (navigator as any).wakeLock.request("screen");
        wakeLockRef.current = lock;
        setIsWakeLockActive(true);

        lock.addEventListener("release", () => {
          setIsWakeLockActive(false);
          wakeLockRef.current = null;
        });
        console.log("Screen Wake Lock acquired successfully.");
      } catch (err) {
        console.warn("Failed to acquire Screen Wake Lock:", err);
      }
    }
  };

  const releaseWakeLock = async () => {
    if (wakeLockRef.current) {
      try {
        await wakeLockRef.current.release();
        wakeLockRef.current = null;
        setIsWakeLockActive(false);
        console.log("Screen Wake Lock released successfully.");
      } catch (err) {
        console.error("Failed to release Screen Wake Lock:", err);
      }
    }
  };

  // Manage Screen Wake Lock automatically based on play state
  useEffect(() => {
    if (isPlaying) {
      acquireWakeLock();
    } else {
      releaseWakeLock();
    }

    return () => {
      if (wakeLockRef.current) {
        wakeLockRef.current.release().catch(() => {});
      }
    };
  }, [isPlaying]);

  // Handle page visibility changes (re-acquire lock if tab was minimized/hidden and restored)
  useEffect(() => {
    const handleVisibilityChange = async () => {
      if (document.visibilityState === "visible" && isPlaying) {
        await acquireWakeLock();
      }
    };

    document.addEventListener("visibilitychange", handleVisibilityChange);
    return () => {
      document.removeEventListener("visibilitychange", handleVisibilityChange);
    };
  }, [isPlaying]);

  // Web Audio refs and persistent mixer node routing
  const audioCtxRef = useRef<AudioContext | null>(null);
  const ltcGainNodeRef = useRef<GainNode | null>(null);
  const beepGainNodeRef = useRef<GainNode | null>(null);
  const mixerMergerNodeRef = useRef<ChannelMergerNode | null>(null);
  const schedulerTimerRef = useRef<number | null>(null);
  const nextFrameTimeRef = useRef<number>(0);
  const currentStreamingTcRef = useRef<Timecode>({ hours: 1, minutes: 0, seconds: 0, frames: 0 });
  const lastLevelRef = useRef<{ raw: number; filtered: number }>({ raw: 1, filtered: 1.0 });
  const scheduledFramesRef = useRef<Array<{ playTime: number; duration: number; tc: Timecode }>>([]);
  const activeSourcesRef = useRef<Array<{ source: AudioBufferSourceNode; stopTime: number }>>([]);

  // Set current timecode equal to start timecode when the start timecode configuration is modified
  const prevStartTimecodeRef = useRef(startTimecode);
  useEffect(() => {
    const isDifferent =
      prevStartTimecodeRef.current.hours !== startTimecode.hours ||
      prevStartTimecodeRef.current.minutes !== startTimecode.minutes ||
      prevStartTimecodeRef.current.seconds !== startTimecode.seconds ||
      prevStartTimecodeRef.current.frames !== startTimecode.frames;

    if (isDifferent && !isPlaying) {
      setCurrentTimecode({ ...startTimecode });
    }
    prevStartTimecodeRef.current = startTimecode;
  }, [startTimecode, isPlaying]);

  // Synchronize visual clock in real time with the audio playhead
  useEffect(() => {
    let animId: number;
    const updateVisualClock = () => {
      if (isPlaying) {
        const pad = (num: number) => num.toString().padStart(2, "0");
        const separator = selectedFps.dropFrame ? ";" : ":";

        if (isTauriMode) {
          const tc = currentStreamingTcRef.current;
          const hoursStr = pad(tc.hours);
          const minutesStr = pad(tc.minutes);
          const secondsStr = pad(tc.seconds);
          const framesStr = pad(tc.frames);
          const ms = Math.floor((tc.frames / selectedFps.fps) * 1000);
          const msStr = `${hoursStr}:${minutesStr}:${secondsStr}.${ms.toString().padStart(3, "0")}`;

          const elHours = document.getElementById("clock-hours");
          const elMinutes = document.getElementById("clock-minutes");
          const elSeconds = document.getElementById("clock-seconds");
          const elFrames = document.getElementById("clock-frames");
          const elSep1 = document.getElementById("clock-sep1");
          const elSep2 = document.getElementById("clock-sep2");
          const elSep3 = document.getElementById("clock-sep3");
          const elMs = document.getElementById("clock-ms");

          if (elHours) elHours.textContent = hoursStr;
          if (elMinutes) elMinutes.textContent = minutesStr;
          if (elSeconds) elSeconds.textContent = secondsStr;
          if (elFrames) elFrames.textContent = framesStr;
          if (elSep1) elSep1.textContent = separator;
          if (elSep2) elSep2.textContent = separator;
          if (elSep3) elSep3.textContent = separator;
          if (elMs) elMs.textContent = msStr;
        } else {
          const now = audioCtxRef.current?.currentTime ?? 0;
          const frames = scheduledFramesRef.current;

          let currentActiveFrame = null;
          for (let i = 0; i < frames.length; i++) {
            const f = frames[i];
            if (now >= f.playTime && now < f.playTime + f.duration) {
              currentActiveFrame = f;
              break;
            }
          }

          if (currentActiveFrame) {
            const hoursStr = pad(currentActiveFrame.tc.hours);
            const minutesStr = pad(currentActiveFrame.tc.minutes);
            const secondsStr = pad(currentActiveFrame.tc.seconds);
            const framesStr = pad(currentActiveFrame.tc.frames);
            const ms = Math.floor((currentActiveFrame.tc.frames / selectedFps.fps) * 1000);
            const msStr = `${hoursStr}:${minutesStr}:${secondsStr}.${ms.toString().padStart(3, "0")}`;

            const elHours = document.getElementById("clock-hours");
            const elMinutes = document.getElementById("clock-minutes");
            const elSeconds = document.getElementById("clock-seconds");
            const elFrames = document.getElementById("clock-frames");
            const elSep1 = document.getElementById("clock-sep1");
            const elSep2 = document.getElementById("clock-sep2");
            const elSep3 = document.getElementById("clock-sep3");
            const elMs = document.getElementById("clock-ms");

            if (elHours) elHours.textContent = hoursStr;
            if (elMinutes) elMinutes.textContent = minutesStr;
            if (elSeconds) elSeconds.textContent = secondsStr;
            if (elFrames) elFrames.textContent = framesStr;
            if (elSep1) elSep1.textContent = separator;
            if (elSep2) elSep2.textContent = separator;
            if (elSep3) elSep3.textContent = separator;
            if (elMs) elMs.textContent = msStr;
          }
        }
      }
      animId = requestAnimationFrame(updateVisualClock);
    };

    animId = requestAnimationFrame(updateVisualClock);
    return () => cancelAnimationFrame(animId);
  }, [isPlaying, selectedFps, isTauriMode]);

  // Update persistent mixer routing on state or volume adjustments
  const updateMixerRouting = () => {
    const ctx = audioCtxRef.current;
    const ltcGain = ltcGainNodeRef.current;
    const beepGain = beepGainNodeRef.current;
    const merger = mixerMergerNodeRef.current;

    if (!ctx || !ltcGain || !beepGain || !merger) return;

    // Disconnect outputs from merger to safely re-patch
    try {
      ltcGain.disconnect();
    } catch (e) {}
    try {
      beepGain.disconnect();
    } catch (e) {}

    // Apply LTC Routing
    if (audioSettings.ltcChannel === "both" || audioSettings.ltcChannel === "left") {
      ltcGain.connect(merger, 0, 0); // Out 0 -> Left Input
    }
    if (audioSettings.ltcChannel === "both" || audioSettings.ltcChannel === "right") {
      ltcGain.connect(merger, 0, 1); // Out 0 -> Right Input
    }

    // Apply Beep Routing
    if (audioSettings.beepChannel === "both" || audioSettings.beepChannel === "left") {
      beepGain.connect(merger, 0, 0); // Out 0 -> Left Input
    }
    if (audioSettings.beepChannel === "both" || audioSettings.beepChannel === "right") {
      beepGain.connect(merger, 0, 1); // Out 0 -> Right Input
    }

    // Apply master volumes directly to the persistent nodes in real-time
    ltcGain.gain.setValueAtTime(audioSettings.ltcVolume, ctx.currentTime);
    beepGain.gain.setValueAtTime(audioSettings.beepVolume, ctx.currentTime);
  };

  // Keep persistent mixer in sync with any slider adjustments in real-time
  useEffect(() => {
    if (!isTauriMode) {
      updateMixerRouting();
    }
  }, [audioSettings, isTauriMode]);

  // Initialize AudioContext and persistent mixer nodes on-demand.
  // In Tauri mode the cpal output stream is created exactly once and reused for
  // the lifetime of the page; subsequent calls are no-ops. Re-creating the stream
  // here would orphan the running LTC scheduler thread and silence timecode output
  // (the original "beep kills LTC" bug).
  const initAudio = async () => {
    if (isTauriMode) {
      const sampleRate = 16000;
      setActiveSampleRate(sampleRate);
      if (!tauriAudioInitRef.current) {
        tauriStartTimeRef.current = performance.now();
        try {
          await initAudioOutput(selectedSinkId, sampleRate, 0);
          tauriAudioInitRef.current = true;
        } catch (err) {
          console.warn("Failed to initialize Tauri audio output:", err);
        }
      }
      return;
    }

    if (!audioCtxRef.current) {
      const AudioCtxClass = window.AudioContext || (window as any).webkitAudioContext;
      let ctx: AudioContext;
      try {
        // Create an optimized 16,000 Hz sample rate AudioContext to reduce JS generation load and memory by 3x on slow devices.
        // It falls back to default settings gracefully if a system doesn't support the configuration.
        ctx = new AudioCtxClass({ sampleRate: 16000 });
      } catch (e) {
        console.warn("Could not create optimized 16kHz AudioContext, falling back to default constructor", e);
        ctx = new AudioCtxClass();
      }
      audioCtxRef.current = ctx;

      ctx.onstatechange = () => {
        setWebAudioStatus(`WEBAUDIO: ${ctx.state.toUpperCase()}`);
      };

      // Create persistent mixer nodes
      const ltcGain = ctx.createGain();
      const beepGain = ctx.createGain();
      const merger = ctx.createChannelMerger(2);

      // Connect mixer to destination
      merger.connect(ctx.destination);

      ltcGainNodeRef.current = ltcGain;
      beepGainNodeRef.current = beepGain;
      mixerMergerNodeRef.current = merger;

      setActiveSampleRate(ctx.sampleRate);
      applyAudioSink(ctx, selectedSinkId);
    }

    // Re-verify routing configuration
    updateMixerRouting();
  };

  // Helper: convert mono samples to stereo interleaved with channel routing
  const monoToStereo = (mono: Float32Array, channel: "left" | "right" | "both"): Float32Array => {
    const stereo = new Float32Array(mono.length * 2);
    for (let i = 0; i < mono.length; i++) {
      if (channel === "both" || channel === "left") stereo[i * 2] = mono[i];
      if (channel === "both" || channel === "right") stereo[i * 2 + 1] = mono[i];
    }
    return stereo;
  };

  // Streaming toggle controllers
  const startStreaming = async () => {
    if (isPlaying) return;

    try {
      await initAudio();
      const audioCtx = audioCtxRef.current;

      if (isTauriMode) {
        setIsPlaying(true);
        currentStreamingTcRef.current = { ...currentTimecode };
        scheduledFramesRef.current = [];
        tauriStartTimeRef.current = performance.now();

        startLtcStream(
          currentTimecode,
          selectedFps.fps,
          selectedFps.dropFrame,
          audioSettings.ltcChannel,
          audioSettings.ltcVolume,
        ).catch(console.warn);

        const fps = selectedFps.fps;
        const dropFrame = selectedFps.dropFrame;
        const clockInterval = window.setInterval(() => {
          const elapsed = (performance.now() - tauriStartTimeRef.current) / 1000;
          const frameDuration = 1 / fps;
          const totalFrames = Math.floor(elapsed / frameDuration);
          let tc = { ...currentTimecode };
          for (let i = 0; i < totalFrames; i++) {
            tc = incrementTimecode(tc, fps, dropFrame);
          }
          currentStreamingTcRef.current = tc;
        }, 50);
        schedulerTimerRef.current = clockInterval;
        return;
      }

      if (audioCtx!.state === "suspended") {
        await audioCtx!.resume();
      }
      setWebAudioStatus(`WEBAUDIO: ${audioCtx!.state.toUpperCase()}`);

      setIsPlaying(true);

      // Reset stream phase and load parameters from current displayed timecode
      currentStreamingTcRef.current = { ...currentTimecode };
      nextFrameTimeRef.current = audioCtx!.currentTime + 0.20; // 200ms startup padding to clear CPU bottlenecks during React render
      lastLevelRef.current = { raw: 1, filtered: 1.0 };
      scheduledFramesRef.current = [];

      // High-precision scheduling loop
      const scheduleLoop = () => {
        const fps = selectedFps.fps;
        const dropFrame = selectedFps.dropFrame;
        const sampleRate = audioCtx!.sampleRate;
        const frameDuration = 1 / fps;

        // Catch-up guard: if nextFrameTime is behind current audio time, skip the late blocks.
        // This prevents scheduling buffers in the past (which play simultaneously and stutter).
        if (nextFrameTimeRef.current < audioCtx!.currentTime) {
          const lateTime = audioCtx!.currentTime - nextFrameTimeRef.current;
          const framesToSkip = Math.ceil(lateTime / frameDuration);
          
          // Jump next scheduled frame ahead in time
          nextFrameTimeRef.current += framesToSkip * frameDuration;
          
          // Coherently fast-forward the timecode ref to preserve correct timing alignment
          for (let i = 0; i < framesToSkip; i++) {
            currentStreamingTcRef.current = incrementTimecode(
              currentStreamingTcRef.current,
              fps,
              dropFrame
            );
          }
          console.warn(`[LTC Engine] Jitter protection: skipped ${framesToSkip} late frames.`);
        }

        // Clean up historic active sources that have already finished playing on DAC
        activeSourcesRef.current = activeSourcesRef.current.filter((item) => {
          return item.stopTime >= audioCtx!.currentTime;
        });

        // Queue audio buffers up to 1.5 seconds ahead to prevent gaps due to heavy React re-renders / GC / clapper clicks
        while (nextFrameTimeRef.current < audioCtx!.currentTime + 1.5) {
          const tc = { ...currentStreamingTcRef.current };

          // Buffer is mono and peak volume is normalized to 1.0; actual master volume
          // is governed by the persistent, real-time-adjustable ltcGain node.
          const buffer = generateLTCFrameBuffer(
            audioCtx!,
            tc,
            fps,
            dropFrame,
            sampleRate,
            1.0,
            lastLevelRef.current
          );

          const source = audioCtx!.createBufferSource();
          source.buffer = buffer;
          
          // Connect to the persistent LTC mixer input
          source.connect(ltcGainNodeRef.current!);

          const playTime = nextFrameTimeRef.current;
          source.start(playTime);

          // Track this active source with its calculated boundary stop time
          activeSourcesRef.current.push({
            source,
            stopTime: playTime + frameDuration,
          });

          // Save frame playhead info for visual synchronization
          scheduledFramesRef.current.push({
            playTime,
            duration: frameDuration,
            tc,
          });

          // Increment current timecode for the NEXT scheduled block
          currentStreamingTcRef.current = incrementTimecode(
            currentStreamingTcRef.current,
            fps,
            dropFrame
          );

          // Progress playhead timeline
          nextFrameTimeRef.current += frameDuration;
        }

        // Clean up historic frames that have finished playing
        scheduledFramesRef.current = scheduledFramesRef.current.filter(
          (f) => f.playTime + f.duration >= audioCtx!.currentTime - 0.5
        );
      };

      // Call scheduler immediately, then loop every 50ms
      scheduleLoop();
      schedulerTimerRef.current = window.setInterval(scheduleLoop, 50);
    } catch (err) {
      console.error("Failed to start Web Audio LTC Streaming", err);
    }
  };

  const getCurrentTimecode = useCallback((): Timecode => {
    if (isPlaying) {
      if (isTauriMode) {
        return currentStreamingTcRef.current;
      }
      const now = audioCtxRef.current?.currentTime ?? 0;
      const frames = scheduledFramesRef.current;
      if (frames.length > 0) {
        if (now < frames[0].playTime) {
          return frames[0].tc;
        }
        for (let i = 0; i < frames.length; i++) {
          const f = frames[i];
          if (now >= f.playTime && now < f.playTime + f.duration) {
            return f.tc;
          }
        }
        if (now >= frames[frames.length - 1].playTime + frames[frames.length - 1].duration) {
          return frames[frames.length - 1].tc;
        }
      }
      return currentStreamingTcRef.current;
    }
    return currentTimecode;
  }, [isPlaying, currentTimecode, isTauriMode]);

  const stopStreaming = () => {
    if (!isPlaying) return;

    if (schedulerTimerRef.current !== null) {
      clearInterval(schedulerTimerRef.current);
      schedulerTimerRef.current = null;
    }

    // Sync latest real-time timecode back to React state when stream pauses/stops
    const finalTc = getCurrentTimecode();
    setCurrentTimecode(finalTc);

    setIsPlaying(false);

    if (isTauriMode) {
      stopLtcStream().catch(console.warn);
      scheduledFramesRef.current = [];
      setWebAudioStatus(`WEBAUDIO: STANDBY`);
      return;
    }

    // Stop and cancel all future scheduled LTC audio buffers immediately
    activeSourcesRef.current.forEach((item) => {
      try {
        item.source.stop();
      } catch (e) {}
    });
    activeSourcesRef.current = [];

    scheduledFramesRef.current = [];
    // Do not reset current timecode so the user can see where they stopped,
    // and resume from the same position if they press start again.
    setWebAudioStatus(`WEBAUDIO: STANDBY`);
  };

  const handleReset = () => {
    if (isPlaying) {
      currentStreamingTcRef.current = { ...startTimecode };

      if (isTauriMode) {
        resetLtcStream(startTimecode).catch(console.warn);
        scheduledFramesRef.current = [];
        tauriStartTimeRef.current = performance.now();
        currentStreamingTcRef.current = { ...startTimecode };
        return;
      }

      // Stop and cancel all currently queued future frames to let the reset be audible instantly
      activeSourcesRef.current.forEach((item) => {
        try {
          item.source.stop();
        } catch (e) {}
      });
      activeSourcesRef.current = [];
      scheduledFramesRef.current = [];
      
      // Align next frame scheduling exactly with current audio playhead
      if (audioCtxRef.current) {
        nextFrameTimeRef.current = audioCtxRef.current.currentTime + 0.05;
      }
    } else {
      setCurrentTimecode({ ...startTimecode });
    }
  };

  // Callback triggered from ClapperSlate component
  const handleClapTriggered = useCallback(async (
    timestampString: string,
    timecodeStr: string,
    msStr: string,
    note: string
  ) => {
    await initAudio();

    if (isTauriMode) {
      tauriPlayBeep(
        16000,
        audioSettings.beepFrequency,
        0.15,
        audioSettings.beepVolume * 0.5,
        audioSettings.beepChannel
      ).catch(console.warn);
    } else {
      const audioCtx = audioCtxRef.current!;

      if (audioCtx.state === "suspended") {
        audioCtx.resume().then(() => {
          setWebAudioStatus(`WEBAUDIO: ${audioCtx.state.toUpperCase()}`);
        });
      }

      // Trigger standard 1kHz clapper beep through persistent mixer input.
      // Master volume routing and levels are controlled statically by beepGainNode,
      // avoiding graph reconstruction glitches on slow cores.
      if (beepGainNodeRef.current) {
        playClapperBeep(
          audioCtx,
          beepGainNodeRef.current,
          1.0, // envelope peak is normalized; overall volume is managed by persistent beepGainNode
          audioSettings.beepFrequency,
          0.15
        );
      }
    }

    // Push clap to the synchronization tracker log
    const logItem: ClapLogItem = {
      id: crypto.randomUUID ? crypto.randomUUID() : Math.random().toString(36).substring(2, 9),
      timestamp: timestampString,
      timecode: timecodeStr,
      milliseconds: msStr,
      notes: note,
    };
    setLogs((prev) => [logItem, ...prev]);
  }, [audioSettings.beepFrequency, audioSettings.beepVolume, audioSettings.beepChannel, isTauriMode]);

  // Formatting displays
  const formattedTimecode = timecodeToString(currentTimecode, selectedFps.dropFrame);
  const formattedMilliseconds = timecodeToMillisecondsString(currentTimecode, selectedFps.fps);

  // Split timecode into components for elegant multi-color display
  const timecodeParts = formattedTimecode.split(/[:;]/);
  const separator = selectedFps.dropFrame ? ";" : ":";

  return (
    <div className="min-h-screen bg-app-bg text-text-main flex flex-col font-sans select-none antialiased">
      {/* HEADER BAR */}
      <header className="border-b border-border-main bg-app-bg px-4 md:px-6 py-3 flex items-center justify-between shrink-0">
        <div className="flex items-center gap-4">
          <div className="w-10 h-10 bg-[#FF5F1F] rounded-sm flex items-center justify-center shrink-0">
            <div className="w-6 h-1 border-t-2 border-b-2 border-black"></div>
          </div>
          <div>
            <h1 className="text-xl font-bold tracking-tight uppercase text-text-title">
              LTC ENGINE <span className="text-[#FF5F1F] opacity-90">v{import.meta.env.VITE_APP_VERSION}</span>
            </h1>
            <div className="text-[10px] text-text-muted font-mono tracking-widest uppercase">
              Linear Timecode Hub
            </div>
          </div>
        </div>

        {/* METADATA STATUS INFO */}
        <div className="flex items-center gap-4">
          <div className="hidden md:flex gap-4 lg:gap-8 text-[11px] font-mono text-text-secondary uppercase tracking-widest mr-4">
            <div className="flex flex-col">
              <span className="text-text-muted">Interface</span>
              <span className={isPlaying ? "text-[#22C55E]" : "text-amber-500 font-semibold"}>
                {isTauriMode ? "TAURI" : (isPlaying ? (audioCtxRef.current ? `WEBAUDIO ${audioCtxRef.current.destination.maxChannelCount}CH` : "WEBAUDIO ACTIVE") : "STANDBY")}
              </span>
            </div>
            <div className="flex flex-col">
              <span className="text-text-muted">Wake Lock</span>
              <span className={isWakeLockActive ? "text-[#22C55E] font-semibold" : ("wakeLock" in navigator ? "text-zinc-400" : "text-red-500 font-semibold")}>
                {isWakeLockActive ? "ACTIVE" : ("wakeLock" in navigator ? "READY" : "UNSUPPORTED")}
              </span>
            </div>
            <div className="flex flex-col">
              <span className="text-text-muted">Sample Rate</span>
              <span className="text-text-title font-semibold">{(activeSampleRate / 1000).toFixed(1)} kHz</span>
            </div>
            <div className="flex flex-col">
              <span className="text-text-muted">Buffer</span>
              <span className="text-text-title font-semibold">{Math.round(activeSampleRate / selectedFps.fps)} SMP</span>
            </div>
          </div>

          <button
            id="btn-theme-toggle"
            onClick={() => setTheme(theme === "dark" ? "light" : "dark")}
            className="p-2.5 rounded-xl bg-card-bg border border-border-main text-text-muted hover:text-text-title transition-all touch-manipulation shadow-sm cursor-pointer"
            title={theme === "dark" ? "Switch to Light Mode" : "Switch to Dark Mode"}
          >
            {theme === "dark" ? <Sun className="w-4 h-4 text-amber-500" /> : <Moon className="w-4 h-4 text-indigo-500" />}
          </button>

          <button
            id="btn-faq-toggle"
            onClick={() => setShowFaq(!showFaq)}
            className="p-2.5 rounded-xl bg-card-bg border border-border-main text-text-muted hover:text-text-title transition-all touch-manipulation shadow-sm cursor-pointer"
            title="Help / FAQ"
          >
            <HelpCircle className="w-4 h-4" />
          </button>
        </div>
      </header>

      {/* CORE CONTENT LAYOUT */}
      <main className="flex-1 overflow-y-auto px-4 py-4 md:p-6 space-y-4 md:space-y-6 max-w-7xl mx-auto w-full flex flex-col justify-between">
        <div className="space-y-4 md:space-y-6 flex-1">
          {/* FAQ ACCORDION PANEL */}
          {showFaq && (
            <div className="bg-card-bg border-2 border-border-main rounded-2xl p-6 shadow-2xl space-y-4">
              <h2 className="text-base font-bold text-text-title uppercase tracking-wider">LTC & Multi-Cam Sync - Quick Guide</h2>
              <div className="grid grid-cols-1 md:grid-cols-3 gap-6 text-xs text-text-muted leading-relaxed">
                <div className="space-y-2">
                  <h3 className="font-semibold text-[#FF5F1F] flex items-center gap-1.5 uppercase tracking-wide">
                    <Radio className="w-3.5 h-3.5" /> What is Linear Timecode (LTC)?
                  </h3>
                  <p>
                    Linear Timecode is an audible analog audio signal that encodes SMPTE timecode (Hours, Minutes, Seconds, Frames) using Bi-Phase Mark Modulation. Cameras and sound recorders listen to this signal and automatically align their footage in post-production.
                  </p>
                </div>
                <div className="space-y-2">
                  <h3 className="font-semibold text-[#FF5F1F] flex items-center gap-1.5 uppercase tracking-wide">
                    <Tv className="w-3.5 h-3.5" /> Connecting to your Cameras
                  </h3>
                  <p>
                    Connect your tablet's headphone jack (or a USB stereo audio interface) directly into the mic-in or audio track of your DSLR/Mirrorless cameras, or dedicated sync boxes (Tentacle, Deity, UltraSync). Set your camera's audio gain manually to a medium level.
                  </p>
                </div>
                <div className="space-y-2">
                  <h3 className="font-semibold text-[#FF5F1F] flex items-center gap-1.5 uppercase tracking-wide">
                    <Volume2 className="w-3.5 h-3.5" /> Synchronizing in Editing
                  </h3>
                  <p>
                    Drop all video and audio files into DaVinci Resolve, Premiere Pro, or Final Cut. Right-click and choose <strong className="text-text-title">"Update Timecode from Audio Track"</strong> or <strong className="text-text-title">"Sync Clips"</strong>. The software instantly parses this LTC waveform to match clips frame-accurately!
                  </p>
                </div>
              </div>
              <div className="flex justify-end pt-2 border-t border-border-main">
                <button
                  id="btn-faq-close"
                  onClick={() => setShowFaq(false)}
                  className="px-4 py-2 bg-deep-bg border border-border-main hover:border-text-muted text-text-secondary font-bold text-xs rounded-xl transition-all touch-manipulation uppercase tracking-wider cursor-pointer"
                >
                  Got it
                </button>
              </div>
            </div>
          )}

          {/* SECTION 1: MASTER STUDIO TIME CLOCK */}
          <section id="master-time-clock-section" className="bg-card-bg border-2 border-border-main rounded-2xl p-4 sm:p-6 shadow-2xl relative overflow-hidden">
            {/* Glow Effect Behind Clock */}
            <div className="absolute -inset-24 bg-[#FF5F1F] opacity-5 blur-[120px] rounded-full pointer-events-none" />

            <div className="relative flex flex-col items-center text-center space-y-4 md:space-y-5">
              <div className="flex items-center gap-2.5">
                <span className="text-[11px] font-bold tracking-[0.3em] text-[#FF5F1F] uppercase">
                  Linear Timecode Stream
                </span>
                <span className="relative flex h-2 w-2">
                  <span className={`animate-ping absolute inline-flex h-full w-full rounded-full opacity-75 ${isPlaying ? "bg-[#22C55E]" : "bg-amber-400"}`}></span>
                  <span className={`relative inline-flex rounded-full h-2 w-2 ${isPlaying ? "bg-[#22C55E]" : "bg-amber-500"}`}></span>
                </span>
              </div>

              {/* HIGH-CONTRAST TIMECODE DISPLAYS */}
              <div className="space-y-2 select-all font-mono">
                {/* LARGE GLOWING TIME CODE (HH:MM:SS:FF) */}
                {timecodeParts.length === 4 ? (
                  <div className="text-3xl sm:text-5xl md:text-6xl lg:text-7xl xl:text-8xl font-black tracking-tighter leading-none text-text-title flex items-center justify-center tabular-nums">
                    <span id="clock-hours">{timecodeParts[0]}</span>
                    <span id="clock-sep1" className="text-clock-sep px-0.5">{separator}</span>
                    <span id="clock-minutes">{timecodeParts[1]}</span>
                    <span id="clock-sep2" className="text-clock-sep px-0.5">{separator}</span>
                    <span id="clock-seconds">{timecodeParts[2]}</span>
                    <span id="clock-sep3" className="text-[#FF5F1F] px-0.5">{separator}</span>
                    <span id="clock-frames" className="text-[#FF5F1F]">{timecodeParts[3]}</span>
                  </div>
                ) : (
                  <div id="clock-fallback" className="text-3xl sm:text-5xl md:text-6xl lg:text-7xl xl:text-8xl font-black tracking-tighter leading-none text-[#FF5F1F]">
                    {formattedTimecode}
                  </div>
                )}

                {/* HIGH-PRECISION MILLISECOND DISPLAY */}
                <div className="text-sm sm:text-base md:text-lg font-bold text-text-secondary uppercase tracking-widest flex items-center justify-center gap-2">
                  <span className="text-text-muted text-[11px] sm:text-xs">MS MATCH:</span>
                  <span id="clock-ms" className="text-text-title font-black">{formattedMilliseconds}</span>
                </div>
              </div>

              {/* TIMECODE STATUS PILLS */}
              <div className="flex flex-wrap items-center justify-center gap-2">
                <span className="text-[10px] sm:text-[11px] bg-deep-bg border border-border-main px-3 py-1.5 rounded-lg font-mono font-bold text-text-muted uppercase tracking-wider flex items-center gap-2 shadow-sm">
                  <Activity className="w-3.5 h-3.5 text-[#FF5F1F]" />
                  FPS: {selectedFps.fps} ({selectedFps.name})
                </span>
                <span className="text-[10px] sm:text-[11px] bg-deep-bg border border-border-main px-3 py-1.5 rounded-lg font-mono font-bold text-text-muted uppercase tracking-wider flex items-center gap-2 shadow-sm">
                  <Volume2 className="w-3.5 h-3.5 text-[#FF5F1F]" />
                  ROUTE: LTC {audioSettings.ltcChannel.toUpperCase()} | CLAP {audioSettings.beepChannel.toUpperCase()}
                </span>
              </div>

              {/* PRIMARY TOUCH INTERACTION DASHBOARD */}
              <div className="flex flex-wrap sm:flex-nowrap items-center justify-center gap-2 sm:gap-3 w-full max-w-2xl pt-1">
                {/* Toggle Streaming Stream */}
                {isPlaying ? (
                  <button
                    id="btn-stream-stop"
                    onClick={stopStreaming}
                    disabled={isLocked}
                    className={`flex-1 min-w-[130px] h-12 sm:h-14 rounded-xl bg-red-600 border-2 border-red-600 hover:bg-red-500 hover:border-red-500 text-white font-extrabold tracking-wider flex items-center justify-center gap-2 transition-colors select-none touch-manipulation cursor-pointer active:scale-[0.98] ${
                      isLocked ? "opacity-30 cursor-not-allowed pointer-events-none" : ""
                    }`}
                  >
                    <Square className="w-4 h-4 fill-white text-white" />
                    <span className="text-xs sm:text-sm uppercase tracking-widest">STOP</span>
                  </button>
                ) : (
                  <button
                    id="btn-stream-start"
                    onClick={startStreaming}
                    disabled={isLocked}
                    className={`flex-1 min-w-[130px] h-12 sm:h-14 rounded-xl bg-card-bg border-2 border-border-main hover:border-[#FF5F1F]/40 hover:bg-nested-hover text-text-title font-extrabold tracking-wider flex items-center justify-center gap-2 transition-all select-none touch-manipulation cursor-pointer ${
                      isLocked ? "opacity-30 cursor-not-allowed pointer-events-none" : "active:bg-[#FF5F1F] active:text-black"
                    }`}
                  >
                    <Play className="w-4 h-4 fill-text-title text-text-title" />
                    <span className="text-xs sm:text-sm uppercase tracking-widest">START</span>
                  </button>
                )}

                {/* Persistent prominent Clap & Beep Trigger */}
                <button
                  id="btn-master-clap"
                  onClick={() => setClapTriggerCount((c) => c + 1)}
                  disabled={isLocked}
                  className={`flex-1 min-w-[140px] h-12 sm:h-14 rounded-xl bg-[#FF5F1F] hover:bg-[#ff753e] text-black font-black tracking-widest flex items-center justify-center gap-2 transition-all select-none touch-manipulation cursor-pointer active:scale-[0.98] ${
                    isLocked ? "opacity-30 cursor-not-allowed pointer-events-none" : ""
                  }`}
                  title="Trigger clapper slate strike & sound beep"
                >
                  <Video className="w-4 h-4 fill-black text-black" />
                  <span className="text-xs sm:text-sm uppercase tracking-widest">CLAP & BEEP</span>
                </button>

                {/* Reset to Start Timecode */}
                <button
                  id="btn-stream-reset"
                  onClick={handleReset}
                  disabled={isLocked}
                  className={`w-12 h-12 sm:w-14 sm:h-14 rounded-xl bg-card-bg border-2 border-border-main hover:border-[#FF5F1F]/40 text-text-secondary hover:text-text-title flex items-center justify-center transition-all select-none touch-manipulation cursor-pointer ${
                    isLocked ? "opacity-30 cursor-not-allowed pointer-events-none" : "active:bg-nested-hover"
                  }`}
                  title="Reset back to starting timecode"
                >
                  <RotateCcw className="w-5 h-5" />
                </button>

                {/* Touch Safety Lock Toggle */}
                <button
                  id="btn-touch-lock"
                  onClick={() => setIsLocked(!isLocked)}
                  className={`w-12 h-12 sm:w-14 sm:h-14 rounded-xl border-2 flex items-center justify-center transition-all select-none touch-manipulation cursor-pointer ${
                    isLocked
                      ? "bg-[#FF5F1F] border-[#FF5F1F] text-black font-extrabold"
                      : "bg-card-bg border-border-main hover:border-[#FF5F1F]/40 text-text-muted hover:text-text-title"
                  }`}
                  title={isLocked ? "Unlock interface controls" : "Lock interface from accidental touch"}
                >
                  {isLocked ? <Lock className="w-5 h-5" /> : <Unlock className="w-5 h-5" />}
                </button>
              </div>
            </div>
          </section>

          {/* SECTION 2: INTERACTIVE DUAL DECK (TABBED DECK FOR TOUCHSCREEN CONVENIENCE) */}
          <section id="interactive-dual-deck" className={`space-y-6 ${isLocked ? "opacity-30 pointer-events-none" : ""}`}>
            {/* TAB TRIGGERS */}
            <div className="flex border-b border-border-main gap-2">
              <button
                id="tab-clapper"
                onClick={() => setActiveTab("clapper")}
                className={`pb-3 px-5 text-xs font-bold tracking-widest uppercase relative transition-all touch-manipulation cursor-pointer ${
                  activeTab === "clapper" ? "text-text-title font-black" : "text-text-muted hover:text-text-title"
                }`}
              >
                Clapper Slate & Logs
                {activeTab === "clapper" && (
                  <motion.div layoutId="activeTabUnderline" className="absolute bottom-0 left-0 right-0 h-0.5 bg-[#FF5F1F]" />
                )}
              </button>
              <button
                id="tab-settings"
                onClick={() => setActiveTab("settings")}
                className={`pb-3 px-5 text-xs font-bold tracking-widest uppercase relative transition-all touch-manipulation cursor-pointer ${
                  activeTab === "settings" ? "text-text-title font-black" : "text-text-muted hover:text-text-title"
                }`}
              >
                Signal & Audio Settings
                {activeTab === "settings" && (
                  <motion.div layoutId="activeTabUnderline" className="absolute bottom-0 left-0 right-0 h-0.5 bg-[#FF5F1F]" />
                )}
              </button>
            </div>

            {/* TAB SECTIONS */}
            <div className="relative">
              <div className={activeTab === "clapper" ? "block" : "hidden"}>
                <ClapperSlate
                  getCurrentTimecode={getCurrentTimecode}
                  selectedFps={selectedFps.fps}
                  dropFrame={selectedFps.dropFrame}
                  onClapTriggered={handleClapTriggered}
                  logs={logs}
                  setLogs={setLogs}
                  clapTriggerCount={clapTriggerCount}
                />
              </div>
              <div className={activeTab === "settings" ? "block" : "hidden"}>
                <TimecodeSettings
                  startTimecode={startTimecode}
                  setStartTimecode={setStartTimecode}
                  selectedFps={selectedFps}
                  setSelectedFps={setSelectedFps}
                  audioSettings={audioSettings}
                  setAudioSettings={setAudioSettings}
                  frameRateOptions={FRAME_RATE_OPTIONS}
                  isPlaying={isPlaying}
                  selectedSinkId={selectedSinkId}
                  onSinkIdChange={setSelectedSinkId}
                />
              </div>
            </div>
          </section>
        </div>

        {/* FOOTER STATUS BAR */}
        <FooterStatusBar
          isWakeLockActive={isWakeLockActive}
          webAudioStatus={webAudioStatus}
        />
      </main>
    </div>
  );
}
