/**
 * @license
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState, useEffect } from "react";
import { Timecode, ClapLogItem } from "../types";
import { timecodeToString, timecodeToMillisecondsString } from "../ltcGenerator";
import { motion, AnimatePresence } from "motion/react";
import { Clipboard, Trash2, CheckCircle2, Video, Plus, Minus, ListRestart } from "lucide-react";

const STRIPES = [0, 1, 2, 3, 4, 5];

interface ClapperSlateProps {
  getCurrentTimecode: () => Timecode;
  selectedFps: number;
  dropFrame: boolean;
  onClapTriggered: (timestampString: string, timecodeStr: string, msStr: string, note: string) => void;
  logs: ClapLogItem[];
  setLogs: React.Dispatch<React.SetStateAction<ClapLogItem[]>>;
  clapTriggerCount?: number;
}

function ClapperSlate({
  getCurrentTimecode,
  selectedFps,
  dropFrame,
  onClapTriggered,
  logs,
  setLogs,
  clapTriggerCount = 0,
}: ClapperSlateProps) {
  // Slate Fields state
  const [scene, setScene] = useState<number>(1);
  const [take, setTake] = useState<number>(1);
  const [roll, setRoll] = useState<string>("A001");
  const [autoIncrementTake, setAutoIncrementTake] = useState<boolean>(true);
  const [copied, setCopied] = useState<boolean>(false);

  // States to drive clapper arm animation and visual flash
  const [armAngle, setArmAngle] = useState<number>(-25);
  const [isFlashing, setIsFlashing] = useState<boolean>(false);

  // Watch for external clap trigger (from master studio panel)
  useEffect(() => {
    if (clapTriggerCount > 0) {
      executeClap();
    }
  }, [clapTriggerCount]);

  const executeClap = () => {
    // 1. Physical Clapper Strike Animation
    // Rapidly pivot arm down to 0 degrees, then spring back up to -25 degrees
    setArmAngle(0);
    setIsFlashing(true);
    
    // Captured Timecode Metrics
    const activeTc = getCurrentTimecode();
    const tcStr = timecodeToString(activeTc, dropFrame);
    const msStr = timecodeToMillisecondsString(activeTc, selectedFps);
    const localTime = new Date().toLocaleTimeString();
    const note = `Scene ${scene}, Take ${take}, Roll ${roll}`;

    // Invoke parent trigger (to play the precise synchronized 1kHz sine beep)
    onClapTriggered(localTime, tcStr, msStr, note);

    // Timers to reset clapper arm and end the visual flash
    setTimeout(() => {
      setArmAngle(-25);
    }, 120);

    setTimeout(() => {
      setIsFlashing(false);
    }, 150);

    // Auto-increment take if selected
    if (autoIncrementTake) {
      setTake((t) => t + 1);
    }
  };

  const copyLogsToClipboard = () => {
    if (logs.length === 0) return;
    const text = logs
      .map((log) => `[${log.timestamp}] LTC: ${log.timecode} | MS: ${log.milliseconds} | ${log.notes || ""}`)
      .join("\n");
    
    navigator.clipboard.writeText(text).then(() => {
      setCopied(true);
      setTimeout(() => setCopied(false), 2000);
    });
  };

  const clearLogs = () => {
    if (window.confirm("Are you sure you want to clear the clap synchronization logs?")) {
      setLogs([]);
    }
  };

  return (
    <div id="clapper-slate-container" className="grid grid-cols-1 xl:grid-cols-12 gap-6">
      {/* LEFT: SLATE INTERACTIVE BOARD */}
      <div className="xl:col-span-7 bg-card-bg border-2 border-border-main rounded-2xl p-6 shadow-2xl flex flex-col justify-between relative overflow-hidden">
        {/* FLASH OVERLAY FOR VISUAL SYNC */}
        <AnimatePresence>
          {isFlashing && (
            <motion.div
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={{ duration: 0.05 }}
              className="absolute inset-0 bg-white z-50 pointer-events-none flex items-center justify-center"
            >
              <span className="text-black font-mono text-5xl font-black tracking-widest uppercase">
                CLAP SYNC
              </span>
            </motion.div>
          )}
        </AnimatePresence>

        <div className="space-y-6">
          {/* Header */}
          <div className="flex items-center justify-between">
            <h3 className="text-xs font-bold tracking-wider text-text-muted uppercase flex items-center gap-2">
              <Video className="w-4 h-4 text-[#FF5F1F]" />
              Smart Clapper Slate
            </h3>
            <label className="flex items-center gap-2 text-xs text-text-muted cursor-pointer">
              <input
                id="chk-auto-inc-take"
                type="checkbox"
                checked={autoIncrementTake}
                onChange={(e) => setAutoIncrementTake(e.target.checked)}
                className="rounded bg-deep-bg border-border-main text-[#FF5F1F] focus:ring-[#FF5F1F] w-4 h-4 cursor-pointer"
              />
              Auto-Increment Take
            </label>
          </div>

          {/* PHYSICAL ANIMATED SLATE TOP BAR */}
          <div className="flex flex-col items-center py-4 bg-deep-bg rounded-2xl border-2 border-border-main relative h-24 sm:h-32 justify-end select-none">
            {/* Clapper Hinge & Base Bar */}
            <div className="absolute bottom-3 sm:bottom-4 w-72 h-8 bg-deep-bg border-t-2 border-b-2 border-border-main rounded flex overflow-hidden">
              {/* Stripe patterns on standard board */}
              {STRIPES.map((idx) => (
                <div
                  key={idx}
                  className="w-12 h-16 origin-bottom transform -skew-x-[35deg]"
                  style={{
                    backgroundColor: idx % 2 === 0 ? "var(--bg-deep)" : "#FF5F1F",
                  }}
                />
              ))}
            </div>

            {/* Rotating Arm */}
            <motion.div
              animate={{ rotate: armAngle }}
              transition={{ type: "spring", stiffness: 400, damping: 25 }}
              style={{ originX: 0.1, originY: 0.9 }}
              className="absolute bottom-[36px] sm:bottom-[40px] left-[calc(50%-144px)] w-72 h-8 bg-deep-bg border-t-2 border-b-2 border-border-main rounded flex overflow-hidden cursor-pointer"
              onClick={executeClap}
            >
              {STRIPES.map((idx) => (
                <div
                  key={idx}
                  className="w-12 h-16 origin-bottom transform -skew-x-[35deg]"
                  style={{
                    backgroundColor: idx % 2 === 0 ? "var(--bg-deep)" : "#FF5F1F",
                  }}
                />
              ))}
            </motion.div>
          </div>

          {/* CINEMA SLATE METADATA GRID */}
          <div className="grid grid-cols-3 gap-3">
            {/* ROLL FIELD */}
            <div className="bg-deep-bg rounded-xl p-3 border border-border-main flex flex-col justify-between h-24">
              <span className="text-[10px] font-bold text-text-muted uppercase tracking-widest block">
                ROLL
              </span>
              <input
                id="input-roll"
                type="text"
                value={roll}
                onChange={(e) => setRoll(e.target.value.toUpperCase())}
                className="bg-transparent border-b border-border-main text-text-title font-mono text-xl font-bold focus:outline-none focus:border-[#FF5F1F] w-full text-center tracking-wide"
              />
              <div className="h-4" /> {/* Spacer to align visually with steppers */}
            </div>

            {/* SCENE FIELD */}
            <div className="bg-deep-bg rounded-xl p-2 sm:p-3 border border-border-main flex flex-col justify-between h-24">
              <span className="text-[10px] font-bold text-text-muted uppercase tracking-widest text-center">
                SCENE
              </span>
              <div className="text-xl sm:text-2xl font-mono font-black text-center text-text-title">
                {scene}
              </div>
              <div className="flex gap-1.5 justify-center">
                <button
                  id="btn-scene-dec"
                  onClick={() => setScene((s) => Math.max(1, s - 1))}
                  className="p-1 rounded-lg bg-card-bg border border-border-main hover:border-[#FF5F1F]/40 text-text-secondary active:bg-nested-hover shadow-sm touch-manipulation cursor-pointer"
                >
                  <Minus className="w-3 h-3" />
                </button>
                <button
                  id="btn-scene-inc"
                  onClick={() => setScene((s) => s + 1)}
                  className="p-1 rounded-lg bg-card-bg border border-border-main hover:border-[#FF5F1F]/40 text-text-secondary active:bg-nested-hover shadow-sm touch-manipulation cursor-pointer"
                >
                  <Plus className="w-3 h-3" />
                </button>
              </div>
            </div>

            {/* TAKE FIELD */}
            <div className="bg-deep-bg rounded-xl p-2 sm:p-3 border border-border-main flex flex-col justify-between h-24">
              <span className="text-[10px] font-bold text-text-muted uppercase tracking-widest text-center">
                TAKE
              </span>
              <div className="text-xl sm:text-2xl font-mono font-black text-center text-[#FF5F1F]">
                {take}
              </div>
              <div className="flex gap-1.5 justify-center">
                <button
                  id="btn-take-dec"
                  onClick={() => setTake((t) => Math.max(1, t - 1))}
                  className="p-1 rounded-lg bg-card-bg border border-border-main hover:border-[#FF5F1F]/40 text-text-secondary active:bg-nested-hover shadow-sm touch-manipulation cursor-pointer"
                >
                  <Minus className="w-3 h-3" />
                </button>
                <button
                  id="btn-take-inc"
                  onClick={() => setTake((t) => t + 1)}
                  className="p-1 rounded-lg bg-card-bg border border-border-main hover:border-[#FF5F1F]/40 text-text-secondary active:bg-nested-hover shadow-sm touch-manipulation cursor-pointer"
                >
                  <Plus className="w-3 h-3" />
                </button>
              </div>
            </div>
          </div>
        </div>

        {/* TRIGGER MASSIVE ACTION BUTTON */}
        <button
          id="btn-trigger-clap"
          onClick={executeClap}
          className="mt-4 sm:mt-6 w-full py-4 rounded-xl bg-[#FF5F1F] hover:bg-[#ff753e] text-black font-black text-base sm:text-lg tracking-widest uppercase shadow-lg shadow-[#FF5F1F]/10 active:scale-[0.98] transition-all cursor-pointer select-none touch-manipulation h-14 sm:h-16 flex items-center justify-center gap-3"
        >
          <div className="w-2.5 h-2.5 bg-black rounded-full animate-ping" />
          CLAP & BEEP
        </button>
      </div>

      {/* RIGHT: SYNC MARKERS LOGS */}
      <div className="xl:col-span-5 bg-card-bg border-2 border-border-main rounded-2xl p-6 shadow-2xl flex flex-col justify-between min-h-[400px]">
        <div className="space-y-4 flex-1 flex flex-col">
          {/* Header */}
          <div className="flex items-center justify-between">
            <h3 className="text-xs font-bold tracking-wider text-text-muted uppercase flex items-center gap-2">
              <ListRestart className="w-4 h-4 text-[#FF5F1F]" />
              Synchronization Logs
            </h3>
            {logs.length > 0 && (
              <div className="flex gap-2">
                <button
                  id="btn-copy-logs"
                  onClick={copyLogsToClipboard}
                  className="p-2 rounded-lg bg-deep-bg border border-border-main hover:border-[#FF5F1F]/40 hover:text-text-title text-text-secondary transition-all flex items-center gap-1.5 text-xs touch-manipulation cursor-pointer shadow-sm"
                  title="Copy logs to clipboard"
                >
                  {copied ? <CheckCircle2 className="w-3.5 h-3.5 text-[#22C55E]" /> : <Clipboard className="w-3.5 h-3.5" />}
                  {copied ? "Copied" : "Copy"}
                </button>
                <button
                  id="btn-clear-logs"
                  onClick={clearLogs}
                  className="p-2 rounded-lg bg-deep-bg border border-border-main hover:border-red-500/30 hover:text-red-400 text-text-secondary transition-all flex items-center gap-1.5 text-xs touch-manipulation cursor-pointer shadow-sm"
                  title="Clear all logs"
                >
                  <Trash2 className="w-3.5 h-3.5" />
                  Clear
                </button>
              </div>
            )}
          </div>

          {/* Logs Area */}
          <div className="flex-1 bg-deep-bg rounded-2xl border border-border-main p-4 font-mono text-xs overflow-y-auto max-h-[380px] min-h-[220px]">
            {logs.length === 0 ? (
              <div className="h-full flex flex-col items-center justify-center text-center text-zinc-600 space-y-2 py-10">
                <Video className="w-8 h-8 text-zinc-700" />
                <div>No clapper marks recorded yet.</div>
                <div className="text-[10px] text-zinc-700 max-w-xs">
                  Tap CLAP & BEEP to capture precise frame-accurate LTC and millisecond markings.
                </div>
              </div>
            ) : (
              <div className="space-y-3">
                {logs.map((log) => (
                  <div
                    key={log.id}
                    className="p-3 rounded-lg bg-nested-bg border border-border-main hover:bg-nested-hover transition-all flex flex-col gap-1 text-text-secondary"
                  >
                    <div className="flex items-center justify-between text-[11px] text-text-muted border-b border-border-main pb-1.5 mb-1.5">
                      <span className="font-bold text-[#FF5F1F]">{log.notes}</span>
                      <span>{log.timestamp}</span>
                    </div>
                    <div className="flex flex-col gap-1">
                      <div className="flex justify-between">
                        <span className="text-text-muted">LTC Timecode:</span>
                        <span className="text-text-title font-black">{log.timecode}</span>
                      </div>
                      <div className="flex justify-between">
                        <span className="text-text-muted">Milliseconds:</span>
                        <span className="text-[#FF5F1F] font-bold">{log.milliseconds}</span>
                      </div>
                    </div>
                  </div>
                ))}
              </div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

export default React.memo(ClapperSlate);
