/**
 * @license
 * SPDX-License-Identifier: Apache-2.0
 */

import React, { useState, useEffect } from "react";

interface FooterStatusBarProps {
  isWakeLockActive: boolean;
  webAudioStatus: string;
}

function FooterStatusBar({
  isWakeLockActive,
  webAudioStatus,
}: FooterStatusBarProps) {
  const [systemTime, setSystemTime] = useState<string>(() => new Date().toLocaleTimeString());
  const [powerStatus, setPowerStatus] = useState<string>("POWER: DETECTING...");
  const [audioOutputDevices, setAudioOutputDevices] = useState<string>("AUDIO OUT: LINE / JACK");
  const [osName, setOsName] = useState<string>("WEB OS");

  // 1. Operating System Detection (Runs once on mount)
  useEffect(() => {
    const getOSName = () => {
      const ua = navigator.userAgent;
      if (/Android/i.test(ua)) return "ANDROID";
      if (/iPhone|iPad|iPod/i.test(ua)) return "IOS";
      if (/Windows/i.test(ua)) return "WINDOWS";
      if (/Macintosh/i.test(ua)) return "MACOS";
      if (/Linux/i.test(ua)) return "LINUX";
      return "WEB OS";
    };
    setOsName(getOSName());
  }, []);

  // 2. System Clock ticker (Updates local state only, preserving main App component)
  useEffect(() => {
    const t = setInterval(() => {
      setSystemTime(new Date().toLocaleTimeString());
    }, 1000);
    return () => clearInterval(t);
  }, []);

  // 3. Power / Battery Status API
  useEffect(() => {
    let active = true;
    let batteryInstance: any = null;

    const updateBattery = (battery: any) => {
      if (!active) return;
      const isCharging = battery.charging;
      const level = Math.round(battery.level * 100);
      setPowerStatus(`POWER: ${isCharging ? "AC CONNECTED" : "BATTERY"} (${level}%)`);
    };

    if ("getBattery" in navigator) {
      (navigator as any)
        .getBattery()
        .then((battery: any) => {
          if (!active) return;
          batteryInstance = battery;
          updateBattery(battery);
          
          battery.addEventListener("chargingchange", () => updateBattery(battery));
          battery.addEventListener("levelchange", () => updateBattery(battery));
        })
        .catch(() => {
          if (active) setPowerStatus("POWER: AC / BATTERY CONNECTED");
        });
    } else {
      setPowerStatus("POWER: AC / BATTERY CONNECTED");
    }

    return () => {
      active = false;
      if (batteryInstance) {
        batteryInstance.removeEventListener("chargingchange", () => updateBattery(batteryInstance));
        batteryInstance.removeEventListener("levelchange", () => updateBattery(batteryInstance));
      }
    };
  }, []);

  // 4. Audio output devices count
  useEffect(() => {
    let active = true;
    if (navigator.mediaDevices && navigator.mediaDevices.enumerateDevices) {
      const checkDevices = () => {
        navigator.mediaDevices
          .enumerateDevices()
          .then((devices) => {
            if (!active) return;
            const count = devices.filter((d) => d.kind === "audiooutput").length;
            if (count > 0) {
              setAudioOutputDevices(`AUDIO OUT: ${count} DEVICE${count > 1 ? "S" : ""} FOUND`);
            } else {
              setAudioOutputDevices("AUDIO OUT: LINE / JACK");
            }
          })
          .catch(() => {
            if (active) setAudioOutputDevices("AUDIO OUT: LINE / JACK");
          });
      };

      checkDevices();
      navigator.mediaDevices.addEventListener?.("devicechange", checkDevices);
      return () => {
        active = false;
        navigator.mediaDevices.removeEventListener?.("devicechange", checkDevices);
      };
    }
  }, []);

  return (
    <footer className="pt-8 flex flex-col md:flex-row justify-between items-center text-text-muted font-mono text-[10px] tracking-widest gap-4 border-t border-border-main mt-8 w-full">
      <div className="flex flex-wrap gap-6 items-center justify-center md:justify-start">
        <div className="flex items-center gap-2">
          <div className="w-1.5 h-1.5 rounded-full bg-[#22C55E]"></div>
          <span>
            OS: <span className="text-text-title font-semibold">{osName}</span>
          </span>
        </div>
        <div className="flex items-center gap-2">
          <div className="w-1.5 h-1.5 rounded-full bg-[#22C55E]"></div>
          <span className="text-text-title font-semibold">{audioOutputDevices}</span>
        </div>
        <div className="flex items-center gap-2">
          <div className="w-1.5 h-1.5 rounded-full bg-[#22C55E]"></div>
          <span className="text-text-title font-semibold">{webAudioStatus}</span>
        </div>
        <div className="flex items-center gap-2">
          <div
            className={`w-1.5 h-1.5 rounded-full ${
              isWakeLockActive
                ? "bg-[#22C55E]"
                : "wakeLock" in navigator
                ? "bg-amber-500"
                : "bg-red-500"
            }`}
          ></div>
          <span>
            WAKE LOCK:{" "}
            <span className="text-text-title font-semibold">
              {isWakeLockActive
                ? "ACTIVE"
                : "wakeLock" in navigator
                ? "READY"
                : "UNSUPPORTED"}
            </span>
          </span>
        </div>
      </div>
      <div className="text-center md:text-right uppercase">
        SYSTEM TIME: <span className="text-text-title font-semibold">{systemTime}</span> |{" "}
        <span className="text-text-title font-semibold">{powerStatus}</span>
      </div>
    </footer>
  );
}

export default React.memo(FooterStatusBar);
