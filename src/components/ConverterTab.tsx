import React, { useState, useCallback, useEffect, useRef } from "react";
import { invoke } from "@tauri-apps/api/core";
import { open } from "@tauri-apps/plugin-dialog";
import { isTauri } from "../utils/audioBackend";
import {
  Upload,
  FolderOpen,
  Settings2,
  FileVideo,
  Play,
  Square,
  Copy,
  RefreshCw,
  AlertTriangle,
  CheckCircle2,
  XCircle,
  ExternalLink,
} from "lucide-react";

// ── Types ──────────────────────────────────────────────────────────────────

const PATTERNS: { name: string; description: string }[] = [
  { name: "TASCAM", description: "Tascam Portacapture X8 — name prefix + S<channel>" },
  { name: "* (any)", description: "Any audio file — select files directly" },
];

interface FfmpegCaps {
  has_ffmpeg: boolean;
  available_encoders: string[];
  available_formats: string[];
  error_message: string | null;
}

interface FileGroupInfo {
  prefix: string;
  files: string[];
  channel_count: number;
}

interface ScanResult {
  groups: FileGroupInfo[];
}

interface ConvertRequest {
  input_files: string[];
  channel_map: number[];
  container: string;
  video_encoder: string;
  audio_encoder: string;
  output_path: string;
  trim_start_secs: number;
}

interface ConvertResponse {
  success: boolean;
  message: string;
}

interface ConversionProgressInfo {
  status: string;
  progress: number;
  log: string;
}

// ── LTC detection types ──────────────────────────────────────────────────────

interface LtcFrameTimecode {
  frame_index: number;
  timecode: { hours: number; minutes: number; seconds: number; frames: number };
  timecode_secs: number;
}

type LtcDecodeStatus =
  | { type: "Success" }
  | { type: "NoSyncWord" }
  | { type: "LowConfidence" }
  | { type: "Error"; message: string };

interface LtcQualityReport {
  score: number;
  grade: string;
  missing_frames: number;
  gap_count: number;
  glitch_count: number;
  edit_count: number;
  max_drift_secs: number;
  drift_rate: number;
  largest_block: number;
  summary: string;
}

interface LtcDetectionResult {
  status: LtcDecodeStatus;
  detected_fps: number;
  drop_frame: boolean;
  total_possible_frames: number;
  valid_frames: number;
  timecodes: LtcFrameTimecode[];
  avg_confidence: number;
  details: string[];
  total_audio_duration_secs: number;
  sample_rate: number;
  processing_time_ms: number;
  first_ltc_timecode_secs: number;
  quality: LtcQualityReport | null;
}

const VIDEO_ENCODERS: [string, string][] = [
  ["prores_ks", "ProRes (Kostya) — ideal for Resolve"],
  ["libx264", "H.264 (x264) — maximum compatibility"],
  ["libx265", "H.265/HEVC (x265) — efficient, Resolve-compatible"],
  ["libsvtav1", "AV1 (SVT-AV1) — good compression"],
  ["dnxhd", "DNxHD — broadcast codec, ideal for MXF"],
];

const AUDIO_ENCODERS: [string, string][] = [
  ["pcm_s24le", "PCM 24-bit — uncompressed, Resolve-compatible"],
  ["pcm_s16le", "PCM 16-bit — uncompressed, smaller"],
  ["aac", "AAC — compressed, good for MP4"],
  ["libopus", "Opus — modern compressed, MKV/MOV only"],
];

const CONTAINERS: [string, string][] = [
  ["mov", "QuickTime MOV — ProRes native, Resolve-friendly"],
  ["mkv", "Matroska MKV — versatile, all codecs"],
  ["mp4", "MPEG-4 MP4 — universal compatibility"],
  ["mxf", "MXF (Material eXchange Format) — professional broadcast"],
];

const CONTAINER_VIDEO: Record<string, string[]> = {
  mkv: ["prores_ks", "libx264", "libx265", "libsvtav1", "dnxhd"],
  mov: ["prores_ks", "libx264", "libx265", "libsvtav1", "dnxhd"],
  mp4: ["libx264", "libx265", "libsvtav1"],
  mxf: ["dnxhd", "libx264", "libx265"],
};

const CONTAINER_AUDIO: Record<string, string[]> = {
  mkv: ["pcm_s24le", "pcm_s16le", "aac", "libopus"],
  mov: ["pcm_s24le", "pcm_s16le", "aac", "libopus"],
  mp4: ["pcm_s24le", "pcm_s16le", "aac"],
  mxf: ["pcm_s24le", "pcm_s16le", "aac"],
};

const DEFAULT_PREFERENCES: [string, string, string][] = [
  ["mov", "prores_ks", "pcm_s24le"],
  ["mxf", "dnxhd", "pcm_s24le"],
  ["mov", "libx264", "pcm_s24le"],
  ["mkv", "libx264", "pcm_s24le"],
  ["mkv", "libx265", "aac"],
  ["mp4", "libx264", "aac"],
];

function selectBestCombination(
  availableFormats: string[],
  availableEncoders: string[],
): [string, string, string] {
  for (const [c, v, a] of DEFAULT_PREFERENCES) {
    const fmt = c === "mkv" ? "matroska" : c;
    if (
      availableFormats.includes(fmt) &&
      availableEncoders.includes(v) &&
      availableEncoders.includes(a) &&
      CONTAINER_VIDEO[c]?.includes(v) &&
      CONTAINER_AUDIO[c]?.includes(a)
    ) {
      return [c, v, a];
    }
  }
  for (const [c] of CONTAINERS) {
    const fmt = c === "mkv" ? "matroska" : c;
    if (!availableFormats.includes(fmt)) continue;
    for (const [v] of VIDEO_ENCODERS) {
      if (!availableEncoders.includes(v) || !CONTAINER_VIDEO[c]?.includes(v)) continue;
      for (const [a] of AUDIO_ENCODERS) {
        if (availableEncoders.includes(a) && CONTAINER_AUDIO[c]?.includes(a)) {
          return [c, v, a];
        }
      }
    }
  }
  return ["mkv", "libx264", "pcm_s24le"];
}

function availableVideoEncoders(container: string, availableEncoders: string[]): string[] {
  return (CONTAINER_VIDEO[container] ?? []).filter((e) => availableEncoders.includes(e));
}

function availableAudioEncoders(container: string, availableEncoders: string[]): string[] {
  return (CONTAINER_AUDIO[container] ?? []).filter((e) => availableEncoders.includes(e));
}

function availableContainers(availableFormats: string[]): string[] {
  return CONTAINERS.filter(([key]) => {
    const fmt = key === "mkv" ? "matroska" : key;
    return availableFormats.includes(fmt);
  }).map(([key]) => key);
}

export default function ConverterTab() {
  const isTauriMode = isTauri();

  if (!isTauriMode) {
    return <DesktopOnlyMessage />;
  }

  return <TauriConverter />;
}

function DesktopOnlyMessage() {
  return (
    <div className="flex flex-col items-center justify-center py-24 text-text-muted">
      <FileVideo className="w-16 h-16 mb-4 opacity-30" />
      <p className="text-lg font-semibold text-text-title mb-2">File Converter</p>
      <p className="text-sm text-center max-w-md">
        This feature requires the desktop application (Tauri). Please install the
        native app to use file conversion and ffmpeg integration.
      </p>
    </div>
  );
}

function TauriConverter() {
  const [ffmpegCaps, setFfmpegCaps] = useState<FfmpegCaps | null>(null);
  const [selectedPattern, setSelectedPattern] = useState<number>(0);
  const [selectedFolder, setSelectedFolder] = useState<string>("");
  const [fileGroups, setFileGroups] = useState<FileGroupInfo[]>([]);
  const [selectedGroupIdx, setSelectedGroupIdx] = useState<number>(-1);

  // Channel mapping
  const [channelMap, setChannelMap] = useState<number[]>([]);
  const [numChannels, setNumChannels] = useState<number>(0);

  // Output format
  const [container, setContainer] = useState<string>("mkv");
  const [videoEncoder, setVideoEncoder] = useState<string>("libsvtav1");
  const [audioEncoder, setAudioEncoder] = useState<string>("pcm_s24le");

  // Derived option lists (filtered by ffmpeg caps + container compatibility)
  const [filteredContainers, setFilteredContainers] = useState<[string, string][]>(CONTAINERS);
  const [filteredVideo, setFilteredVideo] = useState<[string, string][]>(VIDEO_ENCODERS);
  const [filteredAudio, setFilteredAudio] = useState<[string, string][]>(AUDIO_ENCODERS);

  // Output path
  const [outputPath, setOutputPath] = useState<string>("");

  // Conversion state
  const [convStatus, setConvStatus] = useState<string>("idle");
  const [convProgress, setConvProgress] = useState<number>(0);
  const [convLog, setConvLog] = useState<string>("");
  const pollingRef = useRef<number | null>(null);

  // LTC detection
const [ltcFileIdx, setLtcFileIdx] = useState<number>(0);
  const [ltcResult, setLtcResult] = useState<LtcDetectionResult | null>(null);
  const [ltcError, setLtcError] = useState<string | null>(null);
  const [ltcDetecting, setLtcDetecting] = useState(false);
  const [decodeFpsIdx, setDecodeFpsIdx] = useState(1);

  const DECODE_FPS_OPTIONS = [
    { id: "24", name: "24 fps", fps: 24, dropFrame: false },
    { id: "25", name: "25 fps", fps: 25, dropFrame: false },
    { id: "29.97nd", name: "29.97 ND", fps: 29.97, dropFrame: false },
    { id: "29.97df", name: "29.97 DF", fps: 29.97, dropFrame: true },
    { id: "30", name: "30 fps", fps: 30, dropFrame: false },
  ];

  // Trim to first LTC
  const [trimToFirstLtc, setTrimToFirstLtc] = useState(false);
  const [trimOffsetSecs, setTrimOffsetSecs] = useState(0);

  // Sanity check
  const [sanityMsg, setSanityMsg] = useState<string>("");

  // Query ffmpeg on mount, then apply intelligent defaults and filter options
  useEffect(() => {
    (async () => {
      try {
        const caps = await invoke<FfmpegCaps>("check_ffmpeg");
        setFfmpegCaps(caps);
        if (caps.has_ffmpeg) {
          const availContainers = availableContainers(caps.available_formats);
          const availEncoders = caps.available_encoders;
          const [bestC, bestV, bestA] = selectBestCombination(availContainers, availEncoders);
          setContainer(bestC);
          setVideoEncoder(bestV);
          setAudioEncoder(bestA);
          // Filter option lists
          setFilteredContainers(
            CONTAINERS.filter(([key]) => {
              const fmt = key === "mkv" ? "matroska" : key;
              return caps.available_formats.includes(fmt);
            })
          );
          setFilteredVideo(
            VIDEO_ENCODERS.filter(([key]) =>
              availableVideoEncoders(bestC, availEncoders).includes(key)
            )
          );
          setFilteredAudio(
            AUDIO_ENCODERS.filter(([key]) =>
              availableAudioEncoders(bestC, availEncoders).includes(key)
            )
          );
        }
      } catch {
        setFfmpegCaps({
          has_ffmpeg: false,
          available_encoders: [],
          available_formats: [],
          error_message: "Failed to query ffmpeg",
        });
      }
    })();
  }, []);

  // Run sanity check when settings change
  useEffect(() => {
    if (!ffmpegCaps?.has_ffmpeg) {
      setSanityMsg("ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH.");
      return;
    }
    if (!outputPath) {
      setSanityMsg("No output file path specified.");
      return;
    }
    if (selectedGroupIdx < 0) {
      setSanityMsg("");
      return;
    }

    const group = fileGroups[selectedGroupIdx];
    if (!group) return;

    // Basic sanity: container + encoder compatibility
    const encoders = VIDEO_ENCODERS.map(([k]) => k);
    if (!ffmpegCaps.available_encoders.includes(videoEncoder)) {
      setSanityMsg(
        `Video encoder "${videoEncoder}" is not supported by your ffmpeg installation. Common alternatives: libx264, libsvtav1, prores_ks.`
      );
      return;
    }
    if (!ffmpegCaps.available_encoders.includes(audioEncoder)) {
      setSanityMsg(
        `Audio encoder "${audioEncoder}" is not supported by your ffmpeg installation. Common alternatives: pcm_s24le, aac, libopus.`
      );
      return;
    }
    const containerFmt = container === "mkv" ? "matroska" : container;
    if (!ffmpegCaps.available_formats.includes(containerFmt)) {
      setSanityMsg(
        `Container format "${container}" is not supported by your ffmpeg installation. Run \`ffmpeg -formats\` to see available formats.`
      );
      return;
    }

    if (!CONTAINER_VIDEO[container]?.includes(videoEncoder)) {
      setSanityMsg(
        `Video encoder "${videoEncoder}" is not compatible with container "${container}".`
      );
      return;
    }
    if (!CONTAINER_AUDIO[container]?.includes(audioEncoder)) {
      setSanityMsg(
        `Audio encoder "${audioEncoder}" is not compatible with container "${container}".`
      );
      return;
    }

    setSanityMsg("");
  }, [container, videoEncoder, audioEncoder, outputPath, ffmpegCaps, selectedGroupIdx, fileGroups]);

  // Derive trim offset from LTC result
  useEffect(() => {
    if (ltcResult && (ltcResult.status.type === "Success" || ltcResult.status.type === "LowConfidence") && ltcResult.first_ltc_timecode_secs > 0) {
      setTrimOffsetSecs(ltcResult.first_ltc_timecode_secs);
    } else if (!ltcResult) {
      setTrimOffsetSecs(0);
    }
  }, [ltcResult]);

  // Auto-enable trim on successful/low-confidence LTC decode
  useEffect(() => {
    if (ltcResult && (ltcResult.status.type === "Success" || ltcResult.status.type === "LowConfidence")) {
      setTrimToFirstLtc(true);
    }
  }, [ltcResult]);

  // Clean up polling on unmount
  useEffect(() => {
    return () => {
      if (pollingRef.current !== null) {
        clearInterval(pollingRef.current);
      }
    };
  }, []);

  const handleSelectFolder = useCallback(async () => {
    try {
      const selected = await open({
        directory: true,
        multiple: false,
        title: "Select folder with audio recordings",
      });
      if (!selected) return;
      const folderPath = selected as string;
      setSelectedFolder(folderPath);
      const result = await invoke<ScanResult>("scan_folder_for_groups", {
        folderPath,
        patternIndex: 0,
      });
      setFileGroups(result.groups);
      setSelectedGroupIdx(-1);
      setOutputPath("");
    } catch (e) {
      console.error("Folder selection failed:", e);
    }
  }, []);

  const handleSelectFiles = useCallback(async () => {
    try {
      const selected = await open({
        multiple: true,
        title: "Select audio files",
        filters: [{ name: "Audio", extensions: ["*"] }],
      });
      if (!selected) return;
      const filePaths = selected as string[];
      if (filePaths.length === 0) return;

      const firstStem = filePaths[0].split("/").pop()?.split(".").shift() || "selected";
      const parent = filePaths[0].substring(0, filePaths[0].lastIndexOf("/")) || ".";
      setSelectedFolder(parent);

      const basenames = filePaths.map((fp) => fp.split("/").pop() || "?");
      setFileGroups([{
        prefix: firstStem,
        files: basenames,
        channel_count: filePaths.length,
      }]);
      setSelectedGroupIdx(0);
      setOutputPath(`${parent}/${firstStem}-multi-audio-vid.${container}`);
      setNumChannels(filePaths.length);
      setChannelMap(Array.from({ length: filePaths.length }, (_, i) => i));
    } catch (e) {
      console.error("File selection failed:", e);
    }
  }, [container]);

  // Select a file group
  const handleSelectGroup = useCallback(
    (idx: number) => {
      setSelectedGroupIdx(idx);
      const group = fileGroups[idx];
      if (!group) return;
      const n = group.channel_count;
      setNumChannels(n);
      setChannelMap(Array.from({ length: n }, (_, i) => i));
      const defaultName = `${group.prefix}-multi-audio-vid.${container}`;
      setOutputPath(selectedFolder ? `${selectedFolder}/${defaultName}` : defaultName);
    },
    [fileGroups, container, selectedFolder]
  );

  // Channel matrix: swap on click
  const handleMatrixClick = useCallback(
    (inputRow: number, targetCol: number) => {
      setChannelMap((prev) => {
        const next = [...prev];
        const swappedInput = next.indexOf(targetCol);
        if (swappedInput >= 0 && swappedInput !== inputRow) {
          [next[inputRow], next[swappedInput]] = [next[swappedInput], next[inputRow]];
        }
        return next;
      });
    },
    []
  );

  // LTC detection
  const handleDetectLtc = useCallback(async () => {
    if (selectedGroupIdx < 0) return;
    const group = fileGroups[selectedGroupIdx];
    if (!group || ltcFileIdx >= group.files.length) return;
    const folder = selectedFolder.endsWith("/") ? selectedFolder : selectedFolder + "/";
    const filePath = `${folder}${group.files[ltcFileIdx]}`;
    const opt = DECODE_FPS_OPTIONS[decodeFpsIdx];

    setLtcDetecting(true);
    setLtcResult(null);
    setLtcError(null);
    try {
      const result = await invoke<LtcDetectionResult>("detect_ltc_in_file", {
        path: filePath,
        fps: opt.fps,
        dropFrame: opt.dropFrame,
      });
      setLtcResult(result);
    } catch (e) {
      setLtcError(String(e));
    } finally {
      setLtcDetecting(false);
    }
  }, [selectedGroupIdx, fileGroups, ltcFileIdx, selectedFolder, decodeFpsIdx]);

  // Start conversion
  const handleStartConvert = useCallback(async () => {
    if (selectedGroupIdx < 0) return;
    const group = fileGroups[selectedGroupIdx];
    if (!group) return;
    const folder = selectedFolder.endsWith("/") ? selectedFolder : selectedFolder + "/";
    const inputFiles = group.files.map((f) => `${folder}${f}`);

    const request: ConvertRequest = {
      input_files: inputFiles,
      channel_map: channelMap,
      container,
      video_encoder: videoEncoder,
      audio_encoder: audioEncoder,
      output_path: outputPath,
      trim_start_secs: trimToFirstLtc ? trimOffsetSecs : 0,
    };

    try {
      const response = await invoke<ConvertResponse>("start_convert", { request });
      if (!response.success) {
        setConvStatus("failed");
        setConvLog(response.message);
        return;
      }
      setConvStatus("running");
      setConvProgress(0);

      // Start polling
      pollingRef.current = window.setInterval(async () => {
        try {
          const prog = await invoke<ConversionProgressInfo>("get_conversion_progress");
          setConvStatus(prog.status);
          setConvProgress(prog.progress);
          setConvLog(prog.log);
          if (prog.status === "completed" || prog.status === "failed") {
            if (pollingRef.current !== null) {
              clearInterval(pollingRef.current);
              pollingRef.current = null;
            }
          }
        } catch {
          if (pollingRef.current !== null) {
            clearInterval(pollingRef.current);
            pollingRef.current = null;
          }
        }
      }, 200);
    } catch (e) {
      setConvStatus("failed");
      setConvLog(String(e));
    }
  }, [selectedGroupIdx, fileGroups, selectedFolder, channelMap, container, videoEncoder, audioEncoder, outputPath]);

  // Cancel conversion
  const handleCancel = useCallback(async () => {
    try {
      await invoke("cancel_conversion");
    } catch {}
    if (pollingRef.current !== null) {
      clearInterval(pollingRef.current);
      pollingRef.current = null;
    }
    setConvStatus("idle");
  }, []);

  // Copy log
  const handleCopyLog = useCallback(() => {
    navigator.clipboard.writeText(convLog).catch(() => {});
  }, [convLog]);

  const canConvert =
    ffmpegCaps?.has_ffmpeg &&
    selectedGroupIdx >= 0 &&
    outputPath.length > 0 &&
    !sanityMsg &&
    convStatus !== "running";

  return (
    <div className="space-y-6">
      {/* Step 1: Select Files */}
      <StepHeader number="1" label="SELECT FILES" />
      <div className="space-y-4">
        <div className="flex items-center gap-2">
          <span className="text-xs text-text-muted font-semibold">Naming pattern:</span>
          <select
            value={selectedPattern}
            onChange={(e) => {
              setSelectedPattern(Number(e.target.value));
              setFileGroups([]);
              setSelectedGroupIdx(-1);
              setSelectedFolder("");
              setOutputPath("");
              setNumChannels(0);
              setChannelMap([]);
            }}
            className="px-2 py-1 bg-card-bg border border-border-main rounded-lg text-sm text-text-title font-mono focus:outline-none focus:border-[#FF5F1F]"
          >
            {PATTERNS.map((p, i) => (
              <option key={i} value={i}>{p.name}</option>
            ))}
          </select>
          <span className="text-xs text-text-secondary">
            — {PATTERNS[selectedPattern].description}
          </span>
        </div>

        {selectedPattern === 0 ? (
          <button
            onClick={handleSelectFolder}
            className="flex items-center gap-2 px-4 py-2 bg-card-bg border border-border-main rounded-lg hover:bg-nested-bg transition-colors text-sm text-text-title"
          >
            <FolderOpen className="w-4 h-4" />
            {selectedFolder ? "Change Folder…" : "Select Folder…"}
          </button>
        ) : (
          <button
            onClick={handleSelectFiles}
            className="flex items-center gap-2 px-4 py-2 bg-card-bg border border-border-main rounded-lg hover:bg-nested-bg transition-colors text-sm text-text-title"
          >
            <FolderOpen className="w-4 h-4" />
            {fileGroups.length > 0 ? "Change Files…" : "Select Files…"}
          </button>
        )}
        {selectedFolder && (
          <p className="text-xs text-text-muted font-mono truncate">{selectedFolder}</p>
        )}

        {fileGroups.length > 0 && (
          <div>
            <label className="text-xs text-text-muted font-semibold block mb-1">{selectedPattern === 0 ? "Recording:" : "Selected files:"}</label>
            <div className="space-y-1">
              {fileGroups.map((g, i) => (
                <button
                  key={g.prefix}
                  onClick={() => handleSelectGroup(i)}
                  className={`w-full text-left px-3 py-2 rounded-lg border text-sm transition-colors ${
                    selectedGroupIdx === i
                      ? "border-[#FF5F1F] bg-[#FF5F1F]/10 text-text-title"
                      : "border-border-main bg-card-bg text-text-muted hover:border-[#FF5F1F]/50"
                  }`}
                >
                  <span className="font-mono font-semibold">{g.prefix}</span>
                  <span className="text-xs ml-2">
                    ({g.channel_count} file{g.channel_count !== 1 ? "s" : ""}: {g.files.join(", ")})
                  </span>
                </button>
              ))}
            </div>
          </div>
        )}

        {fileGroups.length === 0 && selectedFolder && selectedPattern === 0 && (
          <p className="text-xs text-[#EF4444]">
            No files matching the TASCAM pattern were found in this folder.
          </p>
        )}
      </div>

      {/* LTC Verification */}
      {selectedGroupIdx >= 0 && (
        <>
          <StepHeader number="L" label="VERIFY LTC TRACK" />
          <p className="text-xs text-text-secondary mb-2">
            Select the mono file that carries the LTC timecode signal, then click
            "Detect LTC" to verify it can be read successfully.
          </p>

          <div className="flex items-center gap-2 mb-3">
            <select
              value={ltcFileIdx}
              onChange={(e) => {
                setLtcFileIdx(Number(e.target.value));
                setLtcResult(null);
                setLtcError(null);
              }}
              className="flex-1 px-3 py-2 bg-card-bg border border-border-main rounded-lg text-sm text-text-title font-mono focus:outline-none focus:border-[#FF5F1F]"
            >
              {fileGroups[selectedGroupIdx].files.map((f, i) => (
                <option key={i} value={i}>
                  {f}
                </option>
              ))}
            </select>
            <div className="flex gap-1">
              {DECODE_FPS_OPTIONS.map((opt, i) => (
                <button
                  key={opt.id}
                  onClick={() => setDecodeFpsIdx(i)}
                  className={`px-2 py-2 rounded-lg text-xs font-semibold transition-colors ${
                    decodeFpsIdx === i
                      ? "bg-[#FF5F1F] text-black"
                      : "bg-deep-bg text-text-muted hover:text-text-title border border-border-main"
                  }`}
                  title={opt.name}
                >
                  {opt.name}
                </button>
              ))}
            </div>
            <button
              onClick={handleDetectLtc}
              disabled={ltcDetecting}
              className={`px-4 py-2 rounded-lg text-sm font-semibold transition-colors flex items-center gap-2 ${
                ltcDetecting
                  ? "bg-border-main/30 text-text-muted cursor-not-allowed"
                  : "bg-[#FF5F1F] text-black hover:bg-[#E0551C]"
              }`}
            >
              {ltcDetecting ? (
                <>
                  <RefreshCw className="w-4 h-4 animate-spin" />
                  Detecting…
                </>
              ) : (
                <>
                  <Upload className="w-4 h-4" />
                  Detect LTC
                </>
              )}
            </button>
          </div>

          {/* LTC result display */}
          {ltcDetecting && (
            <div className="flex items-center gap-2 p-3 bg-deep-bg border border-border-main rounded-lg text-xs text-text-muted animate-pulse">
              <RefreshCw className="w-4 h-4 animate-spin" />
              Scanning audio file for LTC timecode…
            </div>
          )}

          {ltcError && (
            <div className="flex items-start gap-2 p-3 bg-[#EF4444]/10 border border-[#EF4444]/20 rounded-lg text-xs text-[#EF4444]">
              <XCircle className="w-4 h-4 mt-0.5 shrink-0" />
              <span>{ltcError}</span>
            </div>
          )}

          {ltcResult && !ltcDetecting && <LtcResultDisplay result={ltcResult} />}
        </>
      )}

      {/* Step 2: Channel Mapping */}
      {numChannels > 0 && (
        <>
          <StepHeader number="2" label="CHANNEL MAPPING" />
          <p className="text-xs text-text-secondary mb-2">
            Click a radio button to swap the input channel (row) with the channel currently mapped
            to the selected output (column).
          </p>
          <div
            className="inline-grid gap-1"
            style={{
              gridTemplateColumns: `60px repeat(${numChannels}, 44px)`,
            }}
          >
            {/* Header row */}
            <div />
            {Array.from({ length: numChannels }, (_, col) => (
              <div key={`h-${col}`} className="text-center text-[10px] text-text-muted font-mono font-semibold">
                OUT {col + 1}
              </div>
            ))}

            {/* Rows */}
            {Array.from({ length: numChannels }, (_, row) => (
              <React.Fragment key={`r-${row}`}>
                <div className="text-xs text-text-title font-mono font-semibold flex items-center">
                  CH {row + 1}
                </div>
                {Array.from({ length: numChannels }, (_, col) => {
                  const isSelected = channelMap[row] === col;
                  return (
                    <button
                      key={`c-${row}-${col}`}
                      onClick={() => !isSelected && handleMatrixClick(row, col)}
                      className={`w-10 h-10 flex items-center justify-center rounded-full transition-all ${
                        isSelected
                          ? "bg-[#FF5F1F]/30 border-2 border-[#FF5F1F]"
                          : "border border-border-main hover:border-[#FF5F1F]/50"
                      }`}
                      title={`Map CH ${row + 1} → OUT ${col + 1}`}
                    >
                      {isSelected && <div className="w-3 h-3 rounded-full bg-[#FF5F1F]" />}
                    </button>
                  );
                })}
              </React.Fragment>
            ))}
          </div>
        </>
      )}

      {/* Step 3: Output Format */}
      <StepHeader number="3" label="OUTPUT FORMAT" />
      <div className="grid grid-cols-1 sm:grid-cols-3 gap-4">
        <SelectField
          label="Container"
          value={container}
          options={filteredContainers}
          disabledOptions={ffmpegCaps?.has_ffmpeg ? undefined : undefined}
          onChange={(c) => {
            setContainer(c);
            // Re-filter encoders for the new container
            if (ffmpegCaps?.has_ffmpeg) {
              const availV = availableVideoEncoders(c, ffmpegCaps.available_encoders);
              const availA = availableAudioEncoders(c, ffmpegCaps.available_encoders);
              setFilteredVideo(VIDEO_ENCODERS.filter(([k]) => availV.includes(k)));
              setFilteredAudio(AUDIO_ENCODERS.filter(([k]) => availA.includes(k)));
              if (!availV.includes(videoEncoder) && availV.length > 0) {
                setVideoEncoder(availV[0]);
              }
              if (!availA.includes(audioEncoder) && availA.length > 0) {
                setAudioEncoder(availA[0]);
              }
            }
          }}
        />
        <SelectField
          label="Video Encoder"
          value={videoEncoder}
          options={filteredVideo}
          disabledOptions={ffmpegCaps?.has_ffmpeg ? ffmpegCaps.available_encoders : undefined}
          onChange={setVideoEncoder}
        />
        <SelectField
          label="Audio Encoder"
          value={audioEncoder}
          options={filteredAudio}
          disabledOptions={ffmpegCaps?.has_ffmpeg ? ffmpegCaps.available_encoders : undefined}
          onChange={setAudioEncoder}
        />
      </div>

      {/* Compatibility status */}
      {ffmpegCaps && !ffmpegCaps.has_ffmpeg && (
        <div className="flex items-start gap-2 p-3 bg-[#EF4444]/10 border border-[#EF4444]/20 rounded-lg text-xs text-[#EF4444]">
          <XCircle className="w-4 h-4 mt-0.5 shrink-0" />
          <span>
            ffmpeg is not available. Please install ffmpeg and ensure it is in your PATH.
          </span>
        </div>
      )}
      {sanityMsg && ffmpegCaps?.has_ffmpeg && (
        <div className="flex items-start gap-2 p-3 bg-[#F59E0B]/10 border border-[#F59E0B]/20 rounded-lg text-xs text-[#F59E0B]">
          <AlertTriangle className="w-4 h-4 mt-0.5 shrink-0" />
          <span>{sanityMsg}</span>
        </div>
      )}
      {!sanityMsg && ffmpegCaps?.has_ffmpeg && selectedGroupIdx >= 0 && (
        <div className="flex items-start gap-2 p-3 bg-[#22C55E]/10 border border-[#22C55E]/20 rounded-lg text-xs text-[#22C55E]">
          <CheckCircle2 className="w-4 h-4 mt-0.5 shrink-0" />
          <span>Settings are compatible.</span>
        </div>
      )}

      {/* Step 4: Output File */}
      <StepHeader number="4" label="OUTPUT FILE" />
      <div className="flex items-center gap-2">
        <input
          type="text"
          value={outputPath}
          onChange={(e) => setOutputPath(e.target.value)}
          className="flex-1 px-3 py-2 bg-card-bg border border-border-main rounded-lg text-sm text-text-title font-mono focus:outline-none focus:border-[#FF5F1F]"
          placeholder="Path to output file…"
        />
      </div>

      {/* Trim to first LTC */}
      <div className="flex items-center gap-2 mt-1">
        <input
          type="checkbox"
          checked={trimToFirstLtc}
          disabled={ltcDetecting || (ltcResult === null && ltcError === null)}
          onChange={(e) => setTrimToFirstLtc(e.target.checked)}
          className="accent-[#FF5F1F]"
        />
        <label className={"text-xs " + (trimToFirstLtc && trimOffsetSecs > 0 ? "text-text-secondary" : "text-text-muted")}>
          Cut start to first LTC frame
          {trimToFirstLtc && trimOffsetSecs > 0 && (
            <span className="text-text-muted ml-1">
              (trim {trimOffsetSecs.toFixed(3)}s of silence)
            </span>
          )}
          {!ltcResult && !ltcError && !ltcDetecting && (
            <span className="text-[10px] text-text-muted ml-1 italic">(Detect LTC first)</span>
          )}
        </label>
      </div>

      {/* Convert Button */}
      <div className="pt-2">
        {convStatus === "running" ? (
          <button
            onClick={handleCancel}
            className="w-full py-3 bg-[#DC2626] text-white font-bold text-sm rounded-lg hover:bg-[#B91C1C] transition-colors flex items-center justify-center gap-2"
          >
            <Square className="w-4 h-4" />
            CANCEL CONVERSION
          </button>
        ) : (
          <button
            onClick={handleStartConvert}
            disabled={!canConvert}
            className={`w-full py-3 font-bold text-sm rounded-lg transition-colors flex items-center justify-center gap-2 ${
              canConvert
                ? "bg-[#FF5F1F] text-black hover:bg-[#E0551C]"
                : "bg-border-main/30 text-text-muted cursor-not-allowed"
            }`}
          >
            <Play className="w-4 h-4" />
            CONVERT TO MULTI-AUDIO VIDEO
          </button>
        )}

        {!canConvert && convStatus !== "running" && (
          <p className="text-xs text-text-secondary mt-2 text-center">
            {!ffmpegCaps?.has_ffmpeg && "ffmpeg is not available. "}
            {selectedGroupIdx < 0 && "Select files. "}
            {!outputPath && "Set an output file path. "}
            {!!sanityMsg && "Fix the compatibility issue above. "}
          </p>
        )}
      </div>

      {/* Progress & Log */}
      {convStatus !== "idle" && (
        <div className="space-y-3">
          {convStatus === "running" && (
            <div>
              <div className="flex justify-between text-xs text-text-muted mb-1">
                <span>Converting…</span>
                <span>{Math.round(convProgress * 100)}%</span>
              </div>
              <div className="w-full h-2 bg-deep-bg rounded-full overflow-hidden">
                <div
                  className="h-full bg-[#FF5F1F] rounded-full transition-all duration-200"
                  style={{ width: `${convProgress * 100}%` }}
                />
              </div>
            </div>
          )}

          {convStatus === "completed" && (
            <div className="flex items-start gap-2 p-3 bg-[#22C55E]/10 border border-[#22C55E]/20 rounded-lg text-xs text-[#22C55E]">
              <CheckCircle2 className="w-4 h-4 mt-0.5 shrink-0" />
              <div>
                <p className="font-semibold">Conversion completed successfully!</p>
                <p className="text-text-muted mt-1">File saved to: {outputPath}</p>
              </div>
            </div>
          )}

          {convStatus === "failed" && (
            <div className="flex items-start gap-2 p-3 bg-[#EF4444]/10 border border-[#EF4444]/20 rounded-lg text-xs">
              <XCircle className="w-4 h-4 mt-0.5 shrink-0 text-[#EF4444]" />
              <div className="flex-1 min-w-0">
                <p className="font-semibold text-[#EF4444] mb-2">Conversion failed</p>
                <LogViewer text={convLog} />
              </div>
            </div>
          )}

          {/* Log display */}
          {convLog && convStatus === "running" && <LogViewer text={convLog} />}

          {/* Copy log button */}
          {convLog && (convStatus === "failed" || convStatus === "completed") && (
            <div className="flex gap-2">
              <button
                onClick={handleCopyLog}
                className="flex items-center gap-1 px-3 py-1.5 text-xs bg-card-bg border border-border-main rounded-lg hover:bg-nested-bg transition-colors text-text-muted"
              >
                <Copy className="w-3 h-3" />
                Copy Full Log
              </button>
              {convStatus === "failed" && (
                <button
                  onClick={() => {
                    setConvStatus("idle");
                    setConvLog("");
                    setConvProgress(0);
                  }}
                  className="flex items-center gap-1 px-3 py-1.5 text-xs bg-card-bg border border-border-main rounded-lg hover:bg-nested-bg transition-colors text-text-muted"
                >
                  <RefreshCw className="w-3 h-3" />
                  Try Again
                </button>
              )}
              {convStatus === "completed" && (
                <button
                  onClick={() => {
                    setConvStatus("idle");
                    setConvLog("");
                    setConvProgress(0);
                  }}
                  className="flex items-center gap-1 px-3 py-1.5 text-xs bg-card-bg border border-border-main rounded-lg hover:bg-nested-bg transition-colors text-text-muted"
                >
                  <RefreshCw className="w-3 h-3" />
                  Start New Conversion
                </button>
              )}
            </div>
          )}
        </div>
      )}
    </div>
  );
}

// ── Sub-components ─────────────────────────────────────────────────────────

function StepHeader({ number, label }: { number: string; label: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="inline-flex items-center justify-center w-6 h-6 rounded bg-[#FF5F1F] text-black text-xs font-bold font-mono">
        {number}
      </span>
      <span className="text-sm font-bold text-text-title tracking-wider">{label}</span>
    </div>
  );
}

function SelectField({
  label,
  value,
  options,
  disabledOptions,
  onChange,
}: {
  label: string;
  value: string;
  options: [string, string][];
  disabledOptions?: string[];
  onChange: (v: string) => void;
}) {
  return (
    <div>
      <label className="text-xs text-text-muted font-semibold block mb-1">{label}</label>
      <select
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className="w-full px-3 py-2 bg-card-bg border border-border-main rounded-lg text-sm text-text-title font-mono focus:outline-none focus:border-[#FF5F1F]"
      >
        {options.map(([key, desc]) => {
          const disabled = disabledOptions && !disabledOptions.includes(key);
          return (
            <option key={key} value={key} disabled={disabled}>
              {key} — {desc}
            </option>
          );
        })}
      </select>
    </div>
  );
}

function formatTc(tc: { hours: number; minutes: number; seconds: number; frames: number }, sep = ":"): string {
  return `${String(tc.hours).padStart(2, "0")}${sep}${String(tc.minutes).padStart(2, "0")}${sep}${String(tc.seconds).padStart(2, "0")}${sep}${String(tc.frames).padStart(2, "0")}`;
}

function LtcResultDisplay({ result }: { result: LtcDetectionResult }) {
  const isSuccess = result.status.type === "Success";
  const isLowConf = result.status.type === "LowConfidence";
  const isError = result.status.type === "Error";
  const isFailed = result.status.type === "NoSyncWord" || isError;

  const statusColor = isSuccess
    ? "text-[#22C55E]"
    : isLowConf
      ? "text-[#F59E0B]"
      : "text-[#EF4444]";

  const statusBg = isSuccess
    ? "bg-[#22C55E]/10 border-[#22C55E]/20"
    : isLowConf
      ? "bg-[#F59E0B]/10 border-[#F59E0B]/20"
      : "bg-[#EF4444]/10 border-[#EF4444]/20";

  const statusIcon = isSuccess ? "✅" : isLowConf ? "⚠️" : "❌";

  const dropFlag = result.drop_frame ? " (Drop Frame)" : "";
  const fpsStr = result.detected_fps > 0 ? `${result.detected_fps.toFixed(2)} fps${dropFlag}` : "—";

  const firstTc = result.timecodes[0];
  const lastTc = result.timecodes[result.timecodes.length - 1];
  const tcSummary =
    result.timecodes.length > 0
      ? `${formatTc(firstTc.timecode, result.drop_frame ? ";" : ":")} → ${formatTc(lastTc.timecode, result.drop_frame ? ";" : ":")}`
      : "—";

  return (
    <div className={`space-y-2 p-3 rounded-lg border ${statusBg}`}>
      {/* Status header */}
      <div className={`flex items-center gap-2 text-xs font-semibold ${statusColor}`}>
        <span>{statusIcon}</span>
        {isSuccess && <span>LTC detected successfully</span>}
        {isLowConf && <span>LTC detected with low confidence</span>}
        {result.status.type === "NoSyncWord" && <span>No LTC timecode found</span>}
        {isError && <span>Detection error: {(result.status as { type: "Error"; message: string }).message}</span>}
      </div>

      {!isError && (
        <div className="grid grid-cols-2 gap-x-4 gap-y-1 text-xs text-text-muted">
          <span>Detected rate:</span>
          <span className="text-text-title font-mono font-semibold">{fpsStr}</span>

          <span>Confidence:</span>
          <span className="text-text-title font-mono font-semibold">
            {(result.avg_confidence * 100).toFixed(1)}%
          </span>

          <span>Valid frames:</span>
          <span className="text-text-title font-mono font-semibold">
            {result.valid_frames} / {result.total_possible_frames}
          </span>

          <span>Timecode range:</span>
          <span className="text-text-title font-mono font-semibold">{tcSummary}</span>

          <span>Sample rate:</span>
          <span className="text-text-title font-mono font-semibold">
            {result.sample_rate} Hz
          </span>

          <span>Audio duration:</span>
          <span className="text-text-title font-mono font-semibold">
            {result.total_audio_duration_secs.toFixed(2)}s
          </span>

          <span>Processing time:</span>
          <span className="text-text-title font-mono font-semibold">
            {result.processing_time_ms.toFixed(1)} ms
          </span>
        </div>
      )}

      {/* Quality report */}
      {result.quality && (
        <div
          className={`mt-2 p-2 rounded border ${
            result.quality.score >= 0.95
              ? "bg-[#22C55E]/10 border-[#22C55E]/30"
              : result.quality.score >= 0.8
                ? "bg-[#22C55E]/5 border-[#22C55E]/20"
                : result.quality.score >= 0.6
                  ? "bg-[#F59E0B]/10 border-[#F59E0B]/30"
                  : "bg-[#EF4444]/10 border-[#EF4444]/30"
          }`}
        >
          <div className="flex items-center gap-2 text-xs">
            <span className="text-text-muted">Quality:</span>
            <span
              className={`font-mono font-bold ${
                result.quality.score >= 0.8
                  ? "text-[#22C55E]"
                  : result.quality.score >= 0.6
                    ? "text-[#F59E0B]"
                    : "text-[#EF4444]"
              }`}
            >
              {(result.quality.score * 100).toFixed(0)}%
            </span>
            <span
              className={`font-semibold ${
                result.quality.score >= 0.8
                  ? "text-[#22C55E]"
                  : result.quality.score >= 0.6
                    ? "text-[#F59E0B]"
                    : "text-[#EF4444]"
              }`}
            >
              {result.quality.grade}
            </span>
          </div>
          <div className="flex flex-wrap gap-x-3 gap-y-0.5 text-[11px] text-text-muted font-mono mt-0.5">
            <span>
              {result.quality.edit_count > 0
                ? `${result.quality.edit_count} edit(s)`
                : result.quality.glitch_count > 0 || result.quality.gap_count > 0
                  ? `${result.quality.gap_count} gap(s), ${result.quality.glitch_count} glitch(es)`
                  : result.quality.missing_frames > 0
                    ? `${result.quality.missing_frames} missing`
                    : "All frames contiguous"}
            </span>
            {result.quality.max_drift_secs > 0.01 && (
              <span>drift {result.quality.max_drift_secs.toFixed(3)}s</span>
            )}
          </div>
        </div>
      )}

      {/* Timecode list (collapsible) */}
      {result.timecodes.length > 0 && (
        <details className="mt-1">
          <summary className="text-xs text-text-muted cursor-pointer hover:text-text-title font-semibold">
            Show {result.timecodes.length} decoded timecodes
          </summary>
          <div className="mt-1 max-h-40 overflow-y-auto bg-deep-bg rounded border border-border-main p-2">
            {result.timecodes.map((ftc, i) => (
              <div
                key={i}
                className="text-xs font-mono text-text-muted hover:text-text-title"
              >
                [{String(ftc.frame_index).padStart(4, " ")}]{" "}
                {formatTc(ftc.timecode, result.drop_frame ? ";" : ":")}{" "}
                (+{ftc.timecode_secs.toFixed(3)}s)
              </div>
            ))}
          </div>
        </details>
      )}

      {/* Details list */}
      {result.details.length > 0 && (
        <details className="mt-1">
          <summary className="text-xs text-text-muted cursor-pointer hover:text-text-title font-semibold">
            Show debug details
          </summary>
          <div className="mt-1 space-y-0.5">
            {result.details.map((d, i) => (
              <p key={i} className="text-[10px] font-mono text-text-muted">
                {d}
              </p>
            ))}
          </div>
        </details>
      )}
    </div>
  );
}

function LogViewer({ text }: { text: string }) {
  const ref = useRef<HTMLPreElement>(null);
  useEffect(() => {
    if (ref.current) {
      ref.current.scrollTop = ref.current.scrollHeight;
    }
  }, [text]);

  return (
    <pre
      ref={ref}
      className="bg-[#0D0D0F] text-[#88CC88] text-xs font-mono p-3 rounded-lg border border-border-main max-h-40 overflow-auto"
    >
      {text}
    </pre>
  );
}