# LTC Timecode Generator — Project Guide

## Overview
High-precision SMPTE Linear Timecode (LTC) audio signal generator + digital clapper-board for multi-camera video sync. Generates bi-phase mark modulated LTC audio and beep tones, routed to selectable stereo channels. It also **decodes** LTC from WAV/video files (quality reports, trim offsets), **converts/exports** recordings (trim-to-first-LTC, timecode metadata embedding, channel splitting/dropping) via ffmpeg, and **ingests/offloads** card media (detect mounted cards, scan for recordings, copy to organised folders).

Two frontends share a common `audio-core` Rust crate:
1. **ltc-gui** (native Rust egui/eframe app — primary GUI target)
2. **ltc-slint** (Slint-based GUI, alternative frontend)

Both Rust GUIs delegate all audio lifecycle, state management, CLI handling, decoding, and conversion to the shared **`gui-engine`** crate via an event-driven message bus.

> Enumerations (command variants, state fields, dependency lists) are deliberately **not** duplicated in this file — they change too often. The named source files are the source of truth.

## Development Methodology

- Use a TTD approach: Analyse how you will implement a feature -> Create Function Stubs -> Write Tests for the Units -> Run the tests (Exect Failure) -> Implement the functions/features (fixing the tests) -> run the tests again (expect success).
- When fixing bugs, use a TDD approach: write a test that catches the bug → run the test (expect failure) → fix the bug → run the test again (expect success).

## Tech Stack

| Crate / App | Role | UI framework |
|---|---|---|
| **audio-core** | Raw audio engine: LTC/beep generation, cpal output, decoders | — |
| **gui-engine** | Shared engine: owns AudioCore + state, CLI, decoding, conversion | — |
| **ltc-gui** | Native desktop GUI (primary target, weak-GPU tablets) | egui/eframe 0.35 (glow) |
| **ltc-slint** | Alternative desktop GUI | Slint 1.17 |

- **Workspace**: `audio-core`, `gui-engine`, `ltc-gui`, `ltc-slint` (see root `Cargo.toml`). Workspace clippy lints: style/correctness/complexity/perf = warn.
- **Dependencies**: source of truth is each crate's `Cargo.toml`. Notable: `audio-core` uses cpal 0.18 (pulseaudio always; pipewire on non-32-bit Linux), `hound` (WAV IO), and `libltc-rs` (bindgen binding → requires system `libltc`, see Build & Run).
- **i686 tablet target**: shipped via `build-all-rust-targets.sh` (Docker cross-compile, `Dockerfile.gui-build`); no Tauri wrapper — ltc-gui itself targets i686.

## Project Structure
```
├── gui-engine/                    # Shared Rust GUI engine crate (lib name: gui_engine)
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs                # Module decls + re-exports (ArcSwap, decode types, converter/file_pattern/ffprobe API)
│   │   ├── command.rs            # GuiCommand enum (source of truth for all commands)
│   │   ├── state.rs              # AppStateSnapshot + ClapLogItem (source of truth for published state)
│   │   ├── engine.rs             # Threaded engine loop, AudioCore lifecycle, retry/recovery, decode handling, offload handling; EngineSeams + engine_main_with_seams for test injection
│   │   ├── job.rs                # Unified async IO job infrastructure: JobSupervisor, ProgressTracker, CancelToken, SpeedMeter, spawn_job
│   │   ├── camera_meta.rs        # Camera model detection from clips (exiftool/ffprobe probe)
│   │   ├── cli.rs                # Cli struct, parse_args(), process_cli(); headless/WAV/list-devices/decode modes
│   │   ├── timecode.rs           # FPS_OPTIONS (24/25/29.97 ND/29.97 DF/30), timecode formatting helpers
│   │   ├── log_buffer.rs         # LogBuffer ring buffer + init_logger (canonical logger)
│   │   ├── theme.rs              # Shared dark/light ThemeColors palettes used by both Rust GUIs
│   │   ├── device_name.rs        # Device-name resolution chain (XAVC sniff → camera meta → filename → volume → "unknown")
│   │   ├── decode.rs             # Shared video→extract→decode + WAV decode pipelines (engine + CLI), temp-WAV management, progress bridge
│   │   ├── clip_probe.rs         # Converter clip-probe policy: ffprobe + camera-meta sample cap + device-name resolution
│   │   ├── duration.rs           # File-duration helpers (WAV header / ffprobe), group aggregation, H:MM:SS formatting
│   │   ├── naming.rs             # Named-placeholder output-filename template engine ({filename}/{device}/{clip}/{track})
│   │   ├── subprocess.rs         # Shared process runner: Windows console suppression, timeout-kill, stderr watchdog, run_ffmpeg_collect_stderr
│   │   ├── offload.rs            # Card-offload subsystem: detect, scan, plan, copy with verify/resume/cancel
│   │   ├── video_codecs.rs       # Codec-level video-encoder registry (single source of truth for encoding)
│   │   ├── hw_device.rs          # VAAPI/Vulkan hw-device discovery, render-node enumeration, test-encode validation
│   │   ├── converter/            # Converter directory module (see Converter section)
│   │   │   ├── mod.rs            # Facade: re-exports public API from submodules
│   │   │   ├── channel_map.rs    # ChannelMap (input→output permutation)
│   │   │   ├── timecode.rs       # TimecodeMetadata, format_ffmpeg_timecode, shift_timecode_back, TC math
│   │   │   ├── capabilities.rs   # FfmpegCapabilities, HwDeviceCapabilities, query_ffmpeg_capabilities
│   │   │   ├── formats.rs       # Codec/container compatibility: supported_*, available_*, select_best_combination
│   │   │   ├── settings.rs       # ConversionPipeline, RecordingType, ConverterSettings + output-path naming
│   │   │   ├── planning.rs       # AudioKeep, VideoOutputStep, plan_*, selected_channel_pairs, plan_output_paths, preview_output_files
│   │   │   ├── checks.rs         # conversion_sanity_check*, ConvertBlocker, evaluate_readiness
│   │   │   ├── args.rs           # ffmpeg argument builders (build_*_args, push_* helpers)
│   │   │   ├── process.rs        # run_ffmpeg_process, parse_out_time, classify_step_failure, StepFailure
│   │   │   ├── runner.rs         # EncoderFallback, spawn_conversion, run_* pipeline orchestration
│   │   │   └── test_fixtures.rs  # #[cfg(test)] fixtures for converter unit tests
│   │   ├── bext_meta.rs         # Camera → WAV-bext field mapping (originator/description/date) + push_wav_bext_args
│   │   ├── tagger.rs            # In-place timecode tagging (native MOV/MP4 + WAV bext + ffmpeg fallback)
│   │   ├── file_pattern.rs      # Camera/recorder filename patterns + file grouping
│   │   ├── ffprobe.rs            # ffprobe video/audio probing + ffmpeg channel extraction + run_ffprobe_json_with
│   │   └── config.rs             # Converter/offload config persistence (last input/output/offload folders)
│   └── tests/                    # integration.rs, converter_integration.rs, video_extraction.rs
├── ltc-gui/                      # Native Rust GUI (egui/eframe) — target for weak-GPU tablets
│   ├── Cross.toml                # Cross-compilation config for i686 targets
│   └── src/
│       ├── main.rs               # process_cli() → eframe::run_native()
│       ├── app.rs                # AppState: tabs, keyboard shortcuts, toasts; reads engine state, sends commands
│       ├── theme.rs              # Bridges engine theme palettes to egui styles
│       ├── ids.rs                # egui ScrollArea id-salt constructors (sibling-widget ID-clash prevention)
│       └── widgets/              # clock.rs, clapper.rs, converter.rs, offload.rs, settings.rs, status.rs
├── ltc-slint/                    # Slint-based GUI (alternative frontend)
│   ├── build.rs                  # slint-build compiler for ui/
│   ├── src/
│   │   ├── main.rs               # Registers Slint callbacks → send GuiCommands; converter option wiring
│   │   ├── poll.rs               # Poll timer: engine_state.load() → Slint properties
│   │   ├── theme.rs, toast.rs, timecode_helpers.rs
│   └── ui/                       # app.slint (root) + clapper/clock/converter/offload/settings/status/theme/types/widgets.slint
├── audio-core/                   # Shared Rust audio crate (LTC generation + cpal output + decoders)
│   └── src/
│       ├── lib.rs                # Thin front door: module decls + re-exports + decode_ltc_with_decoder
│       ├── types.rs              # Timecode, ChannelSel, AudioEvent, AudioDeviceInfo, DecodeConfig, DecodeProgress
│       ├── wav_chunk_reader.rs   # WavChunkReader: byte-level WAV chunk reading (data-section offsets)
│       ├── chunked_decode.rs     # plan_chunk_boundaries, count_chunks* , decode_ltc_chunked (plan → run → merge)
│       ├── decoder.rs            # trait LtcDecoder + BuiltinDecoder/LibltcDecoder + decoder_for() backend seam
│       ├── audio_output.rs       # AudioCore, device/stream lifecycle, config selection, error classification, scheduler + watchdog
│       ├── ltc_encoder.rs        # get_ltc_bits, increment_timecode, generate_ltc_frame_stereo
│       ├── ltc_decoder.rs        # Builtin decoder (strategy ladder) + quality sub-analyzers
│       └── ltc_decoder_libltc.rs # libltc-binding decoder
├── scripts/                      # bump-version.sh, sonar-gate.sh, sonar-report.sh, test-lint.sh (+ test_lint.py)
├── perf-test/
│   └── perf-test.sh              # Profiling harness: pidstat/strace/perf for idle vs. busy phases
├── plans/                        # Implementation Plans & Work Packages generated by LLM Agents
├── test-data/
│   └── ltc-real-world-test-20sec.wav  # Real-world LTC sample for decode testing
├── VERSION_LOG.org               # Org-mode changelog (newest-first, prose feature summaries)
├── Cargo.toml                    # Workspace root (also the version SoT: [workspace.package])
├── build-all-rust-targets.sh, deploy-to-onedrive.sh
├── Dockerfile.gui-build
├── README.org, .dockerignore
└── assets/, logs/
```

## gui-engine Crate Architecture

The `gui-engine` crate is the **shared engine** for both native Rust GUIs (`ltc-gui` and `ltc-slint`). It owns `AudioCore` and all application state, eliminating duplicated code and mutex contention between the two GUI frameworks.

### Data Flow
```
User action → GUI event handler → mpsc::Sender<GuiCommand>
                                       │
                                       ▼
                           Engine Thread (gui_engine::engine::engine_main)
                     (owns AudioCore + AppStateSnapshot)
                                       │
                                       ▼
                    Arc<ArcSwap<AppStateSnapshot>>
                      (lock-free, always latest)
                                       │
                                       ▼
                    GUI reads state.load() each frame
```

### Unified Async IO Job Infrastructure (`job.rs`)

The `job.rs` module provides a uniform infrastructure for all async background
tasks in the engine (conversion, offload, decode, probes, scans, etc.),
replacing the previous ad-hoc pattern of per-task mpsc channels, generation
counters, and individual `catch_unwind` handling.

#### Core types:
  - **`JobKind`** — closed enum of all task types (Conversion, OffloadCopy,
    OffloadScan, LtcDecode, LtcGroupDecode, FolderScan, ClipProbe, VideoProbe,
    DurationProbe, FfmpegCapProbe).
  - **`JobId`** — unique per-job identifier (monotonically increasing `u64`).
  - **`JobPhase`** — `Idle` / `Running` / `Indeterminate` / `Succeeded` /
    `Cancelled` / `Failed`.
  - **`JobStatus`** — published in snapshot: `phase`, `fraction`, `message`,
    `speed`, `units` (per-step/device detail), `log`, `error`. Constructed via
    `JobStatus::idle()`, `JobStatus::running(msg)` (eager pre-poll status),
    `JobStatus::from_progress(&ProgressSnapshot)`, and
    `apply_outcome(&JobOutcome)` for terminal phase transitions.
  - **`ProgressTracker`** — weighted per-unit progress with `ProgressSnapshot`,
    message, speed, log, and unit state machine (`Pending` / `Running` / `Done` /
    `Failed` / `Skipped`). Supports `push_log()` (capped rolling log, published
    in `ProgressSnapshot.log` and captured into `JobOutcome`) and `resize()`
    (weight-preserving).
  - **`UnitProgress`** — per-unit handle: `set_fraction()`, `set_message()`,
    `set_label()`, `set_state()`, `finish()`.
  - **`CancelToken`** — `Arc<AtomicBool>` wrapper; `check()` returns
    `Err(JobError::Cancelled)` when signalled.
  - **`ErrorMeter` (SpeedMeter)** — EMA-smoothed throughput tracker
    (α = 0.3, 2s stall decay).
  - **`JobSupervisor`** — manages active jobs: `is_running(kind)`,
    `cancel(kind)`, `poll()` (drain + snapshots + clean finished threads),
    `drain()` (events), `shutdown(timeout)` (cancel-all + join).
  - **`spawn_job()`** — uniform spawning with `catch_unwind`, thread naming,
    and guaranteed `JobEvent::Finished` emission (now captures `ProgressTracker`
    log into `JobOutcome`).

#### Event channel:
One `mpsc` channel carries all `JobEvent` values:
  - `JobEvent::Item { job, kind, item }` — incremental per-item results
    (`DurationResult`, `ClipLtcResult`).
  - `JobEvent::Finished { job, kind, outcome, payload }` — final result with
    `JobOutcome` (Succeeded/Cancelled/Failed) and `JobFinal` payload.

#### Closed payload enums (`JobItem`, `JobFinal`):
All possible result payloads enumerated as closed Rust enums. The engine's
dispatcher (`engine.rs::handle_job_event`) matches exhaustively on `JobKind`
(no wildcard arm — a new `JobKind` without a handler is a compile error) and
routes each kind to a named `on_*_finished`/`on_*_result` handler; a payload
that does not match its kind is logged and dropped, never silently ignored.

#### Stale-result gating:
The engine tracks the currently-active `JobId` per kind. When a `Finished`
event arrives, the engine checks if its `JobId` still matches the active one
for that kind — stale results from superseded jobs are discarded.

### Sole Source of Truth — State Ownership

The engine is the **sole source of truth** for all application state. GUIs hold only framework-level state (tab index, popup visibility, toast notifications, per-widget `EditState` shadow buffers — see below — and scroll offsets). All user-configurable options — including every converter setting (container, codecs, split/drop/trim toggles, channel map, output paths, naming templates) — are engine-owned via `ConverterUserSettings` in `AppStateSnapshot.converter.settings`.

**Data flow for all mutations:**
1. User interacts with GUI widget → GUI sends a `GuiCommand` (fine-grained, one per field)
2. Engine receives the command, mutates its owned state, applies side-effects (defaults repair, readiness recompute, config persist, channel-map resize, auto-apply from LTC decode, encoder re-selection on container change, prefix prefill on recording select)
3. Next tick: engine publishes updated `AppStateSnapshot` via `ArcSwap`
4. GUI reads the latest snapshot and re-renders

### GuiCommand
Commands are sent from the GUI thread to the engine via `mpsc::Sender<GuiCommand>` (see `command.rs` for the full enum). Categories:
- **Transport** — start/stop LTC, reset, lock, clap
- **Timecode/FPS** — start timecode, FPS index (generate + decode)
- **Audio** — init, sample rate, device selection/refresh, LTC/beep channel + volume, beep frequency/duration
- **Clapper metadata** — scene/take/roll, auto-increment
- **Theme** — set/toggle dark-light
- **Logs** — clear clap log
- **LTC decode** — parse WAV file, probe video, parse video (stream/channel selection), cancel decode
- **Converter** — fine-grained setters for every converter option: pipeline mode (`SetMetadataOnly`, `SetGenerateSyntheticVideo`, `SetCopyVideo`), track handling (`SetSplitTracks`, `SetDropLtcTrack`, `SetConcatAudio`, `SetStartFromLtc`, `SetEmbedCameraMetadata`, `SetTrimEnabled`, `SetLtcFileIndex`, `SwapChannelMapCells`), format/codec (`SetContainer`, `SetVideoCodec`, `SetAudioEncoder`), output paths (`SetOutputFolder`, `SetFilenamePrefix`, `SetAudioSuffixTemplate`, `SetVideoSuffixTemplate`), naming pattern (`SetNamingPattern`), folder/recording selection (`SelectFolder`, `SelectRecording`), and conversion lifecycle (`StartConversion`, `CancelConversion`).
- **Offload** — `Offload(OffloadCommand)`: `ScanCards`, `SetParentFolder`/`SetParentName`, `SetDeviceName`, `SetFileSelected`/`SetAllFilesSelected`/`SelectLatestDay`, `StartOffload`, `CancelOffload`
- **Durations** — `ProbeFileDurations(Vec<PathBuf>)`
- **Shutdown** — graceful engine stop
- **Steppers** — up/down nudges for scene, take, and timecode segments

### AppStateSnapshot
The full application state is published as an `AppStateSnapshot` struct wrapped in `Arc<ArcSwap<AppStateSnapshot>>`. The engine thread calls `state.store(Arc::new(snapshot))` after each tick. The GUI calls `state.load()` to get the latest snapshot — this is lock-free and always returns the latest state without queue management.

Field groups (see `state.rs` for the full struct): generation counter; transport (is_playing/is_locked, current + start timecode); FPS; audio routing + device state; clapper metadata + clap log; engine-computed animations (clap flash alpha, arm angle); theme; per-subsystem status channels (`StatusChannels`: audio/decode/converter/offload + last-writer tag); decode state (decode FPS, decoder selection, decode result/error); video probe info; per-clip LTC group results; ffmpeg capability probe (`ffmpeg_caps` — engine-owned, async); unified job status map (`jobs: HashMap<JobKind, JobStatus>`) covering all async tasks; offload (`OffloadSnapshot`: cards + per-file selection, parent folder/name, `device_totals` from copy plans, completed devices, last_offload_parent + handoff version, error, per-file durations).

### Engine Thread Loop
The engine runs at ~25 fps (40ms ticks) — see `engine.rs::engine_main`:
0. **Spawn ffmpeg capability probe** — before the loop starts, a `spawn_job` with `JobKind::FfmpegCapProbe` runs `query_ffmpeg_capabilities()`.
1. **Drain commands** — non-blocking `try_recv()`; `Shutdown` or channel disconnect exits the loop (calling `supervisor.shutdown()` to cancel+join active jobs first). Every other command is dispatched through the single dispatch site `process_command()` (`engine.rs`), which owns the full `GuiCommand` match: heavy arms are delegated to named `cmd_*` handlers (`cmd_select_folder`, `cmd_start_conversion`, `cmd_probe_video`, `cmd_decode_ltc_video_group`, `cmd_parse_ltc_video`, `cmd_parse_ltc_wav_file`, …), simple converter setters collapse into `apply_simple_converter_setting()`, and converter/offload commands route through `handle_converter_command()`/`handle_offload_command()`. Loop-carried mutable state lives in the `EngineLoopState` struct (snapshot, recovery attempts, auto-apply latches, deferred recording selection, publish gate, ack counter) rather than separate locals.
2. **Poll supervisor + drain events** — `supervisor.poll()` returns progress snapshots for all active job types; progress is published into `state.jobs[kind]`. Then `supervisor.drain()` dispatches `JobEvent::Finished` and `JobEvent::Item` events through `handle_job_event()`, which matches exhaustively on all 10 `JobKind` variants (Conversion, FfmpegCapProbe, FolderScan, VideoProbe, OffloadScan, DurationProbe, OffloadCopy, LtcDecode, LtcGroupDecode, ClipProbe) and calls a named per-kind handler (`on_conversion_finished`, `on_ffmpeg_caps_finished`, `on_folder_scan_finished`, `on_video_probe_finished`, `on_offload_scan_finished`, `on_offload_copy_finished`, `on_ltc_decode_finished`, `on_group_decode_finished`, `on_clip_probes_finished`, `on_duration_result`/`on_group_clip_result` items):
    - Conversion → updates `state.jobs[Conversion]`, manages status transitions
    - FfmpegCapProbe → `apply_ffmpeg_probe_result()`
    - FolderScan → updates `converter.groups`, applies deferred `SelectRecording`
    - VideoProbe → updates `ltc_probe` for decoder stream/channel selection
    - OffloadScan → updates `offload.cards`, spawns `DurationProbe` for file durations
    - DurationProbe `Item` → fills `file_durations` + `offload.file_durations`
    - OffloadCopy → completes device names, updates `last_offload_version`
    - LtcDecode → updates `ltc_decode_result`, auto-applies settings once per generation
    - LtcGroupDecode `Item` → fills per-clip results; `Finished` → auto-applies group settings
    - ClipProbe → populates `converter.probes`/`camera_meta`/`device_name`
3. **Poll timecode** — `core.current_timecode()` when playing
4. **Drain audio events** — drains `core.drain_events()`, dispatches to recovery, and sends each event through the engine→GUI `mpsc::Sender<AudioEvent>` channel (events are a one-shot mailbox, not snapshot state)
5. **Animate** — flash alpha decay (2.0/s), arm angle exponential decay toward rest (4.0/s); determines if clap animation is still visibly in progress.
6. **Recompute converter-derived data** — on demand via `recompute_converter_derived()` (readiness, collision warning, output preview, encoder chain desc)
7. **Update system time**
8. **Publish** — stores the snapshot only when it differs from the last published one; the structural `PartialEq` compare happens *before* the clone, so idle ticks skip the deep clone entirely (regression-pinned by `test_idle_engine_does_not_republish_snapshot`)
9. **Sleep** until next tick

### Audio Lifecycle
- **Init**: 3 retries with exponential backoff (50ms → 100ms → 200ms). Distinguishes permanent errors (permission denied — no retry) from transient (device busy — retry).
- **Recovery**: When `StreamDied` or `RecoveryNeeded` events are detected, the engine attempts up to 3 recovery cycles (stop → reinit → restart LTC if was playing).
- **Device switching**: Stops LTC, stops output, re-initializes on new device, restarts LTC. Reverts to previous device on failure.

### Offload / Card-Ingest Subsystem

The offload subsystem ingests camera media from SD cards into organised folders. Implemented in `gui-engine/src/offload.rs` (~2000 lines) and exposed as a dedicated **Offload** tab in both Rust GUIs.

- **Card detection** — platform-specific: Linux parses `/proc/mounts` + walks `/sys/class/block` for removable partitions (auto-mounts unmounted ones via `udisksctl`); Windows uses `GetLogicalDrives` / `GetDriveTypeW`; macOS lists `/Volumes`.
- **Scan** — walks each card up to depth 6 (max 10 000 files), classifying media files by extension (video + `.wav`). Returns `SdCardInfo` with mount path, volume label, device name, file list.
- **Device naming** — uses `device_name.rs` resolution chain: XAVC binary sniff → camera metadata → filename pattern (Sony/Canon/Panasonic/GoPro/TASCAM) → volume label → `"unknown"`.
- **Selection** — `apply_selection()`: all/none/latest-recording-day per card; per-file toggles via `SetFileSelected`.
- **Copy planning** — `plan_copies_for_files()`: flat `parent/<ISO-date>/<device>/` layout with `name (2).ext` collision renaming.
- **Execution** — `run_offload_copy_job()` runs via `spawn_job` on a background thread: chunked 1 MiB streaming reads, temporary `.offload_tmp` → atomic rename, size-only verification at the end. Idempotent resume: skips files whose destination already exists with matching size. Cancellation via `CancelToken`. Per-device state machine: `Pending` / `Copying` / `Done` / `Failed(String)` / `Skipped`.
- **Events** — offload scan and copy are managed via `spawn_job` with `JobKind::OffloadScan`/`OffloadCopy`. Progress reported via `ProgressTracker` and polled by `supervisor.poll()`. Duration probing uses `JobKind::DurationProbe` with per-file `JobItem::DurationResult` emissions.
- **Config** — persists `parent_folder` via `config::save_offload_parent()`; restored by `seed_snapshot_from_config()` at startup.
- **Converter handoff** — `last_offload_parent` is published in the snapshot; ltc-gui's `logic()` watches for changes and auto-switches the converter to the fresh offload destination.
- **Safety** — card scans run under `catch_unwind` (panic → empty list); copy thread runs independently of the engine tick.

### Shared Subprocess Helpers

These modules are used by both the converter and offload subsystems (and the decoder):

- **`subprocess.rs`** — Shared process-runner primitives: Windows console suppression, `run_output_with_timeout()` / `run_with_timeout()` for ffmpeg/ffprobe/exiftool/udisksctl spawns (concurrent pipe drain avoids deadlock), streaming stderr watchdog with stall kill (30 s of no stderr = `Stalled`). Types: `SubprocessFailure` (`Io` / `TimedOut`), `FfmpegRunError` + `run_ffmpeg_collect_stderr()` (single watchdog driver: spawn → stderr line pump → stall/cancel → terminal-state classification; used by the converter step runner and audio extraction). Used by every ffmpeg, ffprobe, and udisksctl invocation.
- **`duration.rs`** — File-duration helpers: WAV duration via fast `hound` header-only parse, all other formats via ffprobe (`format=duration`, 10 s timeout). Group aggregation: `MultiTrackAudio` → max = take duration, `VideoClipSequence` → sum = total. `H:MM:SS` formatting.
- **`naming.rs`** — Named-placeholder output-filename template engine. Supports `{filename}`, `{device}`, `{clip}`, `{track}` plus zero-padded `{clip:0Nd}`/`{track:0Nd}` (N=1..9). Errors: `UnknownPlaceholder`, `InvalidWidth`, `UnbalancedBraces`. Defines the engine defaults for converter output naming.
- **`device_name.rs`** — Device-name resolution chain shared by offload (card→folder naming) and converter (`{device}` naming template). Priority: 1. XAVC binary sniff (head/tail bytes, `modelName` XML / `ILCE-`/`ILME-`/`DSC-`/`HDR-`), 2. camera metadata (exiftool/ffprobe), 3. filename pattern (Sony/Canon/Panasonic/GoPro/TASCAM), 4. volume label (rejects "usb"), 5. `"unknown"`. Returns `(name, DeviceNameSource, pattern_name)`.

### CLI Modes
`gui_engine::cli::process_cli()` handles all non-GUI modes (see CLI section below for flags):
- `--list-devices` / `-l` — prints devices and exits
- `--output-to-file <PATH>` — generates WAV file and exits (implies headless)
- `--headless` / `-H` — runs headless playback with ctrlc handler
- `--decode <PATH>` — decodes LTC from a WAV or video file and prints results (implies headless)
- Otherwise — returns `CliOutcome::RunGui { cmd_tx, state }` for the GUI to consume

## Converter / Export Subsystem

The converter turns raw recordings into deliverables: trim each file to its first LTC frame, embed start timecode as ffmpeg `-timecode` metadata, drop or split the LTC track, and remux/encode into the chosen container. Implemented in `gui_engine` and exposed in both Rust GUIs (ltc-gui **Convert** tab, ltc-slint converter section).

### Engine Ownership

All converter user settings live in `ConverterUserSettings` (`state.rs`), owned by the engine. GUIs send `ConverterCommand` variants for every mutation. The engine applies side-effects (defaults repair, prefix prefill, channel-map resize, LTC auto-apply, config persistence, encoder re-selection on container change) and publishes the updated snapshot. Derived UI data (readiness blockers, collision warning, output preview, encoder chain description) is recomputed by `recompute_converter_derived()` in the engine and published for GUIs to render.

Conversion execution runs in the engine thread via `StartConversion` which calls `assemble_converter_settings()` then `spawn_job` with `JobKind::Conversion`. The `spawn_conversion_job()` wrapper bridges the `ConversionReport` trait to the unified `ProgressTracker`/`CancelToken`. Progress is polled each tick via `supervisor.poll()` and published into `state.jobs[JobKind::Conversion]`.

### Components (`converter/` directory module)
  - `ConversionPipeline`: `AudioOnly { generate_synthetic_video }` (multi-track WAV → audio/video outputs), `VideoPassthrough` (camera clips → video outputs), and `MetadataOnly` (tag originals in place, rename, extract audio).
  - `ConverterSettings`: input files, `RecordingType` (MultiTrackAudio / VideoClipSequence), `ChannelMap` (input→output permutation), `split_tracks` / `drop_ltc_track` / `ltc_video_source`, container + video codec / audio encoder, output folder + naming templates (defaults via `naming.rs`: `{device}` prefix, `_clip{clip:01d}_tr{track:01d}` audio suffix, `_clip{clip:01d}` video suffix), `trim_to_first_ltc` + per-file trim offsets, per-file `TimecodeMetadata` (start TC, fps, drop-frame).
  - `conversion_sanity_check_metadata_only()` — lightweight preflight for `MetadataOnly` pipeline (only ffmpeg, file existence, template validation).
  - **Video encoder selection is codec-level** (see `video_codecs.rs` below): `ConverterSettings.video_encoder` stores a codec id (`"av1"`, `"h265"`, …); `resolved_video_encoder` holds the concrete ffmpeg encoder chosen at conversion time.
  - `query_ffmpeg_capabilities()` probes ffmpeg once; after listing encoders/formats and discovering HW devices (VAAPI/Vulkan), it **validates each hardware encoder candidate with a 1-frame test encode** (`-f lavfi -i testsrc=... -c:v <enc> -f null -` with 10s timeout). Non-functional encoders (missing driver, incompatible GPU) are removed from `available_encoders` so they never appear in the dropdown or encoder chain. `available_*_for_container()` filters audio encoders/containers; `select_best_combination()` picks defaults (codec-aware); `apply_available_defaults()` repairs stale settings.
  - `plan_video_outputs()` → ordered `VideoOutputStep` list (`VideoOnly`, `VideoMux` with `AudioKeep`, `AudioChannel` extraction — each carrying a `naming_index` for collision-guarded path recovery); caller executes each step.
  - `selected_channel_pairs()` is the single home of the channel-map/drop iteration (output-slot order, unmapped/out-of-bounds slots skipped, LTC dropped per recording type); `preview_output_files` and `output_collision_warning` are projections of `plan_output_paths()`, the one enumeration of produced files for all three pipelines (its `PlannedOutput` carries both the collision-guarded and unguarded paths).
  - `spawn_conversion()` takes `caps: Option<&FfmpegCapabilities>`, resolves the codec into an ordered encoder chain, and runs the steps on a background thread, publishing progress via the `ConversionReport` trait; cancellation via `CancelToken`. Failures are classified as `StepFailure::EncoderInit` (no output produced → retry with next encoder in the chain) or `StepFailure::Fatal`; failed encoders are memoized for the rest of the run and the first successful one is pinned (reported as `Video encoder used: …` in the log).
  - **Stream-copy mode** (`copy_video`, UI: "Leave Video Encoding Untouched", VideoPassthrough only): video is remuxed with `-c:v copy` — no encoder chain, no `-r`; muxed audio is `-c:a copy` unless channel filtering forces an audio-only re-encode. `prepare_copy_mode()` derives the output container from the input (`copy_mode_container_for_input()`: mp4/m4v→mp4, mov→mov, mkv→mkv, mxf→mxf, mts/m2ts/ts→mp4, else mkv), snaps each trim offset to the nearest video keyframe at-or-before it (`ffprobe::snap_trim_to_keyframe()` packet scan), and re-anchors the start timecode via `shift_timecode_back()` (DF-aware, inverse of `audio_core::increment_timecode`) so the embedded TC matches the actual first video frame. Sanity check: `conversion_sanity_check_with_naming(…, copy_video)` / `conversion_sanity_check_copy()` skip video-codec validation in this mode.
  - `evaluate_readiness()` / `ConvertBlocker` / `conversion_sanity_check()` — preflight validation surfaced in the UI before starting.
  - `format_ffmpeg_timecode()` (HH:MM:SS:FF or HH:MM:SS;FF), `find_timecode_at_offset()` (maps decode results → per-file start TC).
  - **`push_metadata_args()`** (`converter/args.rs`) — adds camera metadata to ffmpeg arg lists: `-metadata make/model` (or `com.apple.quicktime.*` for MOV) on video outputs; the WAV `-write_bext 1` + originator/date/description block comes from `bext_meta::push_wav_bext_args`. Gated by `embed_camera_metadata`. Originator defaults to `"LTC Timecode Generator"` when no camera is detected. Origination date falls back to file mtime via `chrono`.
  - **`tagger.rs` bext extension** — in-place WAV tagging now writes `originator` (payload offset 256, 32 B, NUL-padded) and `origination_date` (payload offset 320, 10 B) in addition to `time_reference` (offset 338). A `chunk_size >= 346` guard rejects undersized bext chunks, falling back to ffmpeg remux.
  - **Camera metadata probe** — during converter clip probing, the engine also runs `camera_meta::probe_camera_info()` for each file (cheap second subprocess parallel to ffprobe). Results are published as `Vec<Option<CameraInfo>>` in `ConverterSnapshot.camera_meta`, generation-gated and cleared on recording re-selection.
  - **`embed_camera_metadata` toggle** — `ConverterUserSettings.embed_camera_metadata` (default `true`) is engine-owned. Both GUIs expose a checkbox (ltc-gui: converter widget step 3; ltc-slint: step 4 near Set Start Time from LTC). The converter metadata-only pipeline (`tagger::tag_file`) also accepts and embeds camera info.
- **`video_codecs.rs`** — codec → encoder registry (single source of truth for video encoding):
  - `VIDEO_CODECS`: per codec (`av1`, `h264`, `h265`, `prores`, `dnxhd`) the user-facing label, allowed containers, codec-level args (e.g. `-tag:v hvc1` for HEVC), and a priority-ordered `EncoderCandidate` chain — hardware encoders (nvenc/qsv/amf/mf/vaapi/vulkan/v4l2m2m) first, software encoders (libsvtav1/libaom-av1/libx264/libx265/prores_ks/dnxhd) as fallbacks. Candidates carry their own args (e.g. `pix_fmt=yuv420p`); hardware candidates negotiate pixel format themselves.
  - `available_video_codecs()` (dropdown source), `resolve_encoder_chain()` (ordered available candidates, gated by hw-device availability for vaapi/vulkan), `static_encoder_chain()`, `normalize_video_codec()` (maps legacy concrete encoder names like `libx264` to codec ids), `describe_chain()` (UI summary "av1_nvenc (hardware) → libsvtav1").
  - VAAPI / Vulkan candidates carry `hw_frames: Some(HwFramePath::…)` and are filtered by `FfmpegCapabilities::hw` in `resolve_encoder_chain`. At conversion time, the arg builders inject `-init_hw_device` / `-filter_hw_device` prelude args (pre-input) and `-vf format=nv12,hwupload` (per-encoder). Device discovery (`hw_device.rs`) enumerates `/dev/dri/renderD*` and probes with `ffmpeg -init_hw_device` at capability-query time; runtime fallback demotes candidates without an available device.
  - **`hw_device.rs`** — hardware probe primitives: `discover()` (VAAPI/Vulkan init probes), `list_vaapi_render_nodes()`, `probe_vaapi()`/`probe_vulkan()`. Also contains `test_encode()` / `test_encode_with()` (1-frame null encode with timeout) and `validate_hw_encoders()` / `validate_hw_encoders_with()` (walk all `VIDEO_CODECS` hw candidates, run test encode, remove failures from `available_encoders`). All functions accept the ffmpeg path for testability; injectable runner closures enable pure unit tests.
- **`file_pattern.rs`** — groups input files by naming convention. `BUILTIN_PATTERNS` (TASCAM Portacapture X8 `nameS<ch>`, `*` any) + `CAMERA_PATTERNS` (Sony Handycam, Sony FS100, Canon `MVI_`, Panasonic `GH`, GoPro `GOPR`/`GP`). `match_files_to_groups()` / `wrap_user_selected_files()` / `match_files_all_patterns()`; `default_output_filename()` derives the output name from the group.
- **`ffprobe.rs`** — `probe_video_audio()` (ffprobe JSON → `VideoAudioProbe` with per-stream channels/codec/sample-rate), `path_is_video()`, `extract_audio_channel()` (ffmpeg extraction used by decode), `snap_trim_to_keyframe()` / `parse_last_keyframe()` (keyframe packet scan used by stream-copy trim snapping), and `run_ffprobe_json_with()` (single spawn/timeout/exit/JSON-parse helper behind all ffprobe JSON calls).
- **`config.rs`** — persists last input/output folders to `<config_dir>/ltc-timecode-generator/converter_config.json`; also persists `last_offload_parent` via `save_offload_parent()` / `seed_snapshot_from_config()`.
- **`tagger.rs`** — in-place timecode metadata tagger: dispatches MOV/MP4 (native O(1) in-place tagger: trailing moov → free + appended tmcd track + tiny mdat), WAV with existing bext (patches time_reference), and ffmpeg stream-copy remux (temp file + atomic rename) as fallback for other containers. `tag_file()` dispatches per-file tagging. The metadata-only pipeline calls `tag_file()` inline for each input.

### Flow
Select files → group by naming pattern → probe (ffprobe) → (ffmpeg capabilities already probed async at engine startup) → readiness/blockers check → channel mapping + LTC track handling → trim-to-first-LTC + timecode metadata → `spawn_conversion` (progress + cancel). `MetadataOnly` pipeline skips trimming and encoder checks: originals are tagged in place (native MP4/MOV or ffmpeg remux), renamed, and audio extracted per channel. `run_metadata_only()` is a phase orchestrator (`probe_all_metadata_files` → `extract_metadata_audio` → `tag_and_rename_files` → `summarize_metadata_failures`) with a single `check_cancelled` helper; every attempted step is recorded in a `FailureLedger`, so a run in which nothing succeeded ends in `mark_failed` and partial failures surface a `--- N STEP(S) FAILED ---` block. `StepFailure` payloads (ffmpeg exit reasons) are formatted into every failure message. Logic-level tests drive the phases through `run_metadata_only_with`'s injectable prober.

## Native Rust GUI (`ltc-gui/`)

The native GUI is built with **egui 0.35 + eframe** (glow backend, vsync off). It is a thin rendering shell over `gui-engine` — all audio, decode, and conversion logic lives in the engine thread. This is the target frontend for weak-GPU tablets (Intel Atom + GMA 500) where WebKitGTK performance is unusable.

### Architecture
- **`AppState` struct** (`app.rs`): holds `cmd_tx` via a sequence-assigning `send()` sink (matches the engine's `applied_command_seq` ack counter 1:1 — every command must go through it), `engine_state` (ArcSwap handle), theme, notifications, tab state (Clapper / Settings / Convert / Offload), debug log buffer, offload→converter handoff tracking. Implements `eframe::App`.
- **Shadow-state interaction architecture** (`shadows.rs` + `widgets/bound.rs`, shared state machine in `gui-engine/src/edit_state.rs`): every interactive widget renders from a typed `EditState<T>` shadow instead of the snapshot. Wrappers (`bound::text/checkbox/slider/select_value/set_value`) enforce one contract: sync against engine truth (adopt only when unfocused and no unconfirmed send — acked via `applied_command_seq >= sent seq`, with a 2 s timeout fallback), draw from shadow, on change send through the sink and mark pending, track focus. Engine-initiated setting changes (caps repair, decode auto-apply, probe resizes, recording resets) propagate through sync. There is deliberately no three-way merge, no per-field epochs, and no debouncing.
- **Frame loop**: `logic()` syncs `self.latest` from `engine_state.load()`, drains the engine's `Receiver<AudioEvent>` into toast notifications, processes keyboard shortcuts (Space/C/R/L/Ctrl+D → send GuiCommand), handles repaint scheduling. Also watches `offload.last_offload_version` and auto-switches the converter to the fresh offload destination (`SelectFolder` + `SelectRecording(0)`).
- **Widgets** (`widgets/`): `clock` (glowing timecode display), `clapper` (board + arm + scene/take/roll + sync log), `settings` (FPS selector, steppers, device, routing, sliders), `status` (footer bar), `converter` (file picker via rfd, pattern/encoder selection, conversion progress), `offload` (3-step ingest UI: parent folder + date name, card/file selection with per-card device naming and per-file checkboxes, offload progress with per-device status). Widgets read from `state.latest.*` for display and call `state.send(GuiCommand::...)` for mutations.
- **`ids.rs`** — egui `ScrollArea` id-salt constructors preventing sibling-widget ID clashes in egui's stable-ID system.
- **Theme** (`theme.rs`): bridges the shared engine palettes (`gui_engine::theme`) to egui styles.
- **File dialogs**: `rfd` (native open/save dialogs) — used by the converter, offload, and decode flows.

### Threading
- **GUI thread**: egui immediate-mode rendering at 25-60 fps. Never touches AudioCore. Reads lock-free from ArcSwap.
- **Engine thread**: Owns AudioCore. Receives commands via mpsc. Publishes state via ArcSwap. Sleeps 40ms between ticks.
- **Zero mutex contention**: The GUI thread never locks AudioCore. The engine thread owns it exclusively.

### Build & Run
```bash
cd ltc-gui
cargo run                           # Debug build
cargo run --release                 # Release build
LIBGL_ALWAYS_SOFTWARE=1 cargo run   # Force software OpenGL rendering
```

## Slint GUI (`ltc-slint/`)

The Slint GUI follows the same pattern as ltc-gui — thin shell over `gui-engine`:
- **`main.rs`** registers Slint callbacks that send `GuiCommand` variants and wires converter option models (containers/encoders from ffmpeg capabilities). Offload callbacks: `on_off_select_parent_folder` (rfd → `SetParentFolder`), `on_off_parent_name_changed` (`SetParentName`), `on_off_card_name_changed` (`SetDeviceName`), `on_off_rescan` (`ScanCards`), `on_off_start` / `on_off_cancel` (`StartOffload` / `CancelOffload`), `on_off_file_toggled` (`SetFileSelected`), `on_off_select_all_files` (`SetAllFilesSelected`), `on_off_select_latest_day` (`SelectLatestDay`).
- **`poll.rs`** sets up the poll timer that reads `engine_state.load()` and updates Slint properties (timecode segments, FPS names, routing pills, clapper metadata, decode results, device names, debug log entries, and offload state sync — `OffloadCardInfo`/`OffloadFileInfo`/per-device status). The poll timer also drains the engine's `Receiver<AudioEvent>` into toasts.
- **`toast.rs` / `theme.rs` / `timecode_helpers.rs`** — GUI-side toast management, palette application, and timecode segment formatting.
- **`ui/`** is split per concern: `app.slint` (root window + tabs) plus `clapper/clock/converter/offload/settings/status/theme/types/widgets.slint`. `offload.slint` exports `OffloadSection` with 4th tab wiring.
- Uses `rfd` for file dialogs and `arboard` for clipboard access.

## audio-core Crate

The `audio-core` crate provides the raw audio engine, split by concern:
- **`lib.rs`** — thin front door (~70 lines): module declarations, re-exports, and `decode_ltc_with_decoder()` (one-line delegation to the decoder seam).
- **`types.rs`** — shared types: `Timecode`, `ChannelSel`, `AudioEvent`, `AudioDeviceInfo`, plus the chunked-decode `DecodeConfig`/`DecodeProgress`.
- **`wav_chunk_reader.rs`** — `WavChunkReader`: low-level WAV reading from data-section offsets (byte-level `read_raw_bytes` + `decode_le_int_sample` helpers, f32/i16 mono extraction, 8/16/24/32-bit int + float).
- **`chunked_decode.rs`** — chunked parallel decode: `chunk_geometry()` + `plan_chunk_boundaries()` (the single home of the chunk-boundary math — `count_chunks` is `plan(..).len()` by construction), `count_chunks_in_wav`, `decode_ltc_chunked` decomposed into `plan_chunks` → `run_sequential`/`run_parallel` (over `&dyn LtcDecoder`) → `merge_results` (offset/dedup/reindex/confidence aggregation, unit-tested with a `MockDecoder`).
- **`decoder.rs`** — backend-selection seam: `trait LtcDecoder` (`name`/`decode_wav`/`decode_chunk`), `BuiltinDecoder`/`LibltcDecoder` unit structs, and `decoder_for(use_libltc)` — the only `if use_libltc` in the crate.
- **`audio_output.rs`** — `AudioCore` (cpal output stream, ring buffers 128K LTC + 32K beep, scheduler thread, wake lock, event queue); `list_audio_devices()` / `AudioDeviceInfo` (built via `DeviceConfigSummary` + `from_summary`); config selection, stream building, device enumeration; error classification (`is_permanent_device_error`); `suggest_sample_rate()`; `SAMPLE_RATE_OPTIONS = &[44100, 48000]`. The scheduler's callback-stall recovery is a pure `CallbackWatchdog` state machine (virtual-clock tested) plus a device-free `push_frame` helper.
- **`ltc_encoder.rs`** — `get_ltc_bits()` (80-bit bi-phase mark frame), `increment_timecode()`, `compute_frame_sample_count()`, `generate_ltc_frame_stereo()`.
- **`ltc_decoder.rs`** — builtin pure-Rust decoder: `decode_ltc_samples()` (crate-private; the strategy ladder — ZC-interval attempt, `scan_windows`, `score_candidate`, `prefer_zc_or_detailed` epilogue, `decode_full_file` — with all `ScoredResult` construction funneled through `from_frame_starts`), `decode_ltc_from_wav()`, first-coherent-frame alignment (`find_first_coherent_index`, `apply_coherent_first_timecode` — crate-private), and `compute_ltc_quality()` as an orchestrator over pure sub-analyzers (`split_segments`, `analyze_drift`, `analyze_gaps`, `analyze_glitches`, `missing_in_span`, `quality_score`); types `LtcDetectionResult`, `FrameTimecode`, `LtcQualityReport`, `LtcDecodeStatus`.
- **`ltc_decoder_libltc.rs`** — `decode_ltc_from_wav_libltc()` via the `libltc-rs` binding (requires system `libltc`); the sample-level entry is crate-private.
- Common `AudioEvent` variants: StreamError/StreamDied/StreamRecovering/StreamDead/RecoveryNeeded/Underrun/FramesDropped.

## CLI

`gui_engine::cli` (shared by both Rust GUIs; binary `ltc-gui`). Modes: `--list-devices/-l`, `--output-to-file <PATH>` (WAV render), `--headless/-H` (live playback, ctrlc handler), `--decode <PATH>` (decode + summary), otherwise GUI.

Flag groups (see `cli.rs::Cli` for the full list with defaults):
- **Playback**: `--device <NAME>` / `--device-index <N>`, `--start-timecode` (default `01:00:00:00`), `--fps` (24/25/29.97/30), `--drop-frame`, `--channel left|right|both`, `--volume`, `--sample-rate`, `--duration`, `--autostart`
- **Decode**: `--decoder builtin|libltc`, `--decode-fps`, `--decode-drop-frame`, `--audio-stream <N>` / `--audio-channel <N>` (video files), `--single-pass`, `--context-frames <N>`, `--list-timecodes/-t`
- **Misc**: `--verbose/-v` (timecode progression / detailed quality report), `--debug/-d`

### LTC Decoding
Two decoders produce the same `LtcDetectionResult`:
```bash
# builtin — pure Rust, chunked parallel decode with progress + cancel (default)
ltc-gui --decode file.wav --decoder builtin
# libltc — binding to the C library, very fast
ltc-gui --decode file.wav --decoder libltc
```
- `--single-pass` forces non-chunked decode (small files); `-v` prints a detailed quality report (confidence, gaps, glitches with `--context-frames` context); `-t` lists all decoded timecodes.
- **Video files are auto-detected** by extension (mp4/mov/mkv/mts/m2ts/mxf/avi/webm): ffprobe lists the audio streams, ffmpeg extracts the selected stream/channel (`--audio-stream`, `--audio-channel`), then decode proceeds on the extracted audio.
- **GUI decode** uses the same engine (`ParseLtcWavFile` / `ProbeVideo` / `ParseLtcVideo` commands): decode runs on a background thread with generation-stamped results, progress reporting, and cancellation.

## Version Management

The **single source of truth** is the `version` field under `[workspace.package]` in the root `Cargo.toml`. All four workspace members inherit it via `version.workspace = true` in their manifests; `Cargo.lock` tracks the resolved member versions. Both Rust GUIs read the version from `CARGO_PKG_VERSION` at compile time.

### How to bump the version
```bash
scripts/bump-version.sh patch    # 0.4.7 → 0.4.8 (creates git commit + tag v0.4.8)
scripts/bump-version.sh minor    # 0.4.7 → 0.5.0
scripts/bump-version.sh major    # 0.4.7 → 1.0.0
scripts/bump-version.sh 0.6.0    # explicit version
scripts/bump-version.sh patch --dry-run   # print the plan, apply nothing
```

The script:
1. Refuses to run on a dirty working tree (npm-version parity)
2. Rewrites the `[workspace.package]` version in the root `Cargo.toml`
3. Runs `cargo update --workspace` to refresh `Cargo.lock` member versions (never re-resolves third-party dependencies) and verifies all four members show the new version in the lock
4. Commits `Cargo.toml` + `Cargo.lock` as `chore(release): bump version to X.Y.Z` and tags `vX.Y.Z`

## Testing

```bash
cargo test                           # All Rust crates: unit tests in modules + integration suites
cargo test -p gui-engine             # Just the gui-engine crate
cargo clippy --all-targets           # Lint all workspace crates
cargo clean                          # Remove all build artifacts (the old `npm run clean`)
```

Repeat-run aliases (formerly `npm run test:flaky` / `npm run test:lint`):
```bash
cargo nextest run --profile ci       # CI retry profile: retries=3, final-status-level=flaky
bash scripts/test-lint.sh            # Test-lint guardrails (sleep / text-pin rules)
```

### Flaky-Test Methodology

Integration tests that drive the real engine thread or real ffmpeg subprocesses
can be timing-sensitive. Follow these rules to keep them deterministic:

1. **Predicate waits, not signals** — Never break a poll loop on `generation > 0`
   or similar coarse signals. Always wait for the *specific postcondition* the
   test asserts (e.g. `logs.len() == 3`, `status_message == "Clap!"`). See
   `run_engine()` in `gui-engine/tests/integration.rs` for the canonical pattern.

2. **Deadlines, not fixed sleeps** — Never use `thread::sleep(Duration::from_millis(N))`
   to wait for an async operation. Poll with a deadline and a predicate; this
   adapts to machine load. Example: `test_conversion_cancellation` polls the
   conversion report (progress > 0, i.e. the first step is running) before
   cancelling, never sleeping a fixed delay.

3. **Decay-tolerant tolerances** — Temporal assertions (e.g. flash-alpha after a
   clap) must account for engine-tick decay. If testing through the state-snapshot
   path, the observed snapshot may be 1-2 ticks old. Prefer synchronous unit tests
   for fine-grained state checks; keep integration assertions coarse.

4. **No real user config** — Engine tests set `XDG_CONFIG_HOME` to a tempdir via
   `init_test_config()`, preventing writes to `~/.config/`. All test engine
   spawns must call `init_test_config()` first.

5. **No real ffmpeg probe** — Engine tests use `engine_main_with_probe()` (or `engine_main_with_seams`, which additionally injects the offload card-detection source) with
   `fake_probe()`, skipping the real ffmpeg-capability subprocess probe.
   This removes N concurrent `ffmpeg -encoders` calls per test run and the
   mid-test `apply_available_defaults` mutation. The real probe path is exercised
   by converter integration tests that call `query_ffmpeg_capabilities()` directly.

6. **Loud skips** — Tests that require ffmpeg/ffprobe use `eprintln!("--- SKIPPED: ...")`
   so skips are visible in the output, never silent.

7. **Tooling** — `cargo-nextest` is installed and configured (`.config/nextest.toml`).
   Each test runs in its own process, eliminating shared-state races. Use
   `cargo nextest run --profile ci` for CI
   repeat runs: `retries=3`, `final-status-level=flaky` highlights tests that
   passed only on retry. Locally, `cargo nextest run --retries 10 -E 'test(...)'`
   ruthlessly shakes out timing flakes in a target test.

### Test Quality Rules

A test must fail when behavior regresses and pass when behavior improves.

- **Never weaken tests that find actual bugs.** When you write a test
  that fails and your analysis shows you have found a genuine bug, you
  may never, under any circumstance, weaken the test to make it
  pass. If your current implementation plan does not include
  production code changes, you are to leave the test failing, note it
  down in your final report, and also provide a prompt for the next
  agent who will be tasked with fixing the bug you have found.
- **No quality ceilings.** Never assert upper bounds on success (decode rate ≤ X%,
  "at most N frames") or exact failure/retry/skip counts for things that should
  ideally succeed. Floors ("≥ N frames decoded") and false-positive ceilings on
  *garbage* input ("noise must not decode") are fine. Litmus test: if the code
  got strictly better, would this test still pass?
- **No text assertions.** Never assert error message, status, log, or UI label
  text (`assert_eq!`/`contains` on strings meant for humans) — including
  substrings and disjunctions. Assert typed outcomes (`matches!(err, E::Variant)`)
  or structural facts instead. Legitimate strings: ffmpeg/ffprobe args, SMPTE
  timecode format, file/template/config syntax, on-disk naming tokens.
- **No pins of "today's behavior".** If a comment says "current behavior" or
  "today's ...", the assertion is probably a limitation, not a contract. Test
  the *desired* semantics; if undesired behavior must be tolerated temporarily,
  say so in the test name (`..._currently_pins_...`) so it reads as debt.
- **One behavior, one test, lowest layer.** Don't duplicate a unit test through
  the engine thread or the integration suite; higher layers test routing only.
  Don't test test-helpers, derived trait impls, or inline reimplementations of
  production code — drive the real function/constant.
- **Deterministic by construction.** No fixed sleeps waiting for state; poll a
  predicate with a deadline (see Flaky-Test Methodology), join handles, or
  inject a clock/runner seam. Never assert on wall-clock elapsed time or on
  observing a transient intermediate state; assert terminal state instead.

### Test-Lint Guardrails (CI)

`bash scripts/test-lint.sh` (the first CI step
after checkout) fails on the two most recurrent violations of the rules above,
over test code only (`tests/` dirs and `#[cfg(test)]` regions):

- **sleep** — `thread::sleep` used as a test wait mechanism outside a
  poll-with-deadline context. Worker-closure sleeps, deadline-adjacent polls,
  and engine-start-then-join patterns are structurally exempt (WP-T1's
  do-not-touch categories) — prefer fixing the *rule* over per-site allows
  when a new legitimate shape appears.
- **text-pin** — `.contains("` on an error/message-shaped identifier
  (`err|error|msg|message|status|label|details`). Suppress only with an
  inline, reason-bearing `// test-lint: allow(text-pin): <why>` comment
  inside the test function, and only when the text *is* the contract (e.g.
  a formatter's Display output, template/naming tokens) — additions need a
  decision note in the PR, same as the "decide, then assert" workflow.

Run `--self-test` (CI does) to verify the detector itself; exit code 2 means
the lint is broken and must red rather than pass.

### Integration Suites

- `gui-engine/tests/integration.rs` — engine-thread command processing, incl. offload scan/copy integration tests driven through `EngineSeams.scan_cards` (fake card, real tempdir copies, cancel + guard branches) and the `SetDevice` bogus-id revert test
- `gui-engine/tests/cli_decode.rs` — CLI dispatch via `process_cli_result` (output-to-file, WAV/video decode, error paths)
- `gui-engine/tests/tagger_mp4.rs` — native MP4 tmcd in-place tagging against the committed fixture `test-data/tmcd-roundtrip-trailing-moov.mp4` (stco-offset regression net + ffprobe round-trip)
- `gui-engine/tests/converter_integration.rs` — conversion pipelines (real ffmpeg)
- `gui-engine/tests/video_extraction.rs` — ffprobe/ffmpeg extraction (real ffmpeg)

In-module `#[cfg(test)]` unit tests cover offload (46 tests), naming (38), duration (20), device_name (14), and subprocess (11). `converter/test_fixtures.rs` provides shared fixtures for converter unit tests.

Golden vectors for the web LTC generator live in `src/ltcGoldenVectors.ts`.

## Planning & Analysis

- Place any analysis reports or the like in `reports/` (local dir, not commited)
- Place any planning artefacts (Work Packages, Implementation Plans, etc.) in `plans/` (local dir, not commited)
- When planning PRs/Commits, use the project's commit msg style of `<type>(<module>): <short_desc>` and include a body detailing the changes made.
- When planning work packages or the like, plan to create a feature branch for all associated commits/PRs.
- The project merges completed feature branches into main through the double rebase style: rebase `feature_x` on `main`, fix any merge conflicts that arise, run tests again if you needed code changes, then rebase `main` on `feature_x`. This way no merge-commits end up in the `main` branch.

## CI & SonarQube Cloud

GitHub Actions (`.github/workflows/ci.yml`) runs on push to `main` and on PRs: the test-lint guardrail, clippy (JSON report), Rust tests via `cargo llvm-cov nextest --workspace --profile ci` + LCOV report via `cargo llvm-cov report` (cargo-llvm-cov ≥0.9 split the old `--nextest` flag into a `nextest` subcommand), and a SonarQube Cloud scan. The Sonar step is skipped when the `SONAR_TOKEN` repo secret is absent, so the workflow works in forks without setup; when the token is present, the scan passes `-Dsonar.qualitygate.wait=true`, so the **LTC gate** quality gate can red CI on a failing push/PR (revert to advisory-only by deleting the `args` line). A second job, `windows-cross-check`, runs `cargo check --workspace --all-targets --target x86_64-pc-windows-gnu` on ubuntu-latest (mingw-w64 + host `libltc-dev` headers suffice for bindgen at check time) so the Windows-only `cfg` blocks (`offload::win_driver`, `subprocess` console suppression, unix-only test fixtures being properly gated) compile on every PR — check-only, no link and no tests; a macOS cross-check is deliberately out of scope (SDK sysroot fragility).

- **Sonar project**: `D4id4los_ltc-timecode-generator` in organization `d4id4los` (sonarcloud.io). Analysis config lives in `sonar-project.properties` (sources = the four workspace crates; exclusions = `*.slint` — Slint markup has no Sonar analyzer; the legacy React web app and Tauri crates were removed from the repo in Phase 1).
- **Quality gate**: enforcement runs on SonarCloud's built-in **Sonar way** gate (the org's plan entitlement rejects assigning custom gates — HTTP 403). The idempotent `SONAR_TOKEN=<token> scripts/sonar-gate.sh` creates the custom **LTC gate** with the four WP-5 new-code conditions (coverage > 80 %, reliability/security/maintainability ratings on new code rating A) and verifies the assigned gate covers all four metrics — Sonar way is a strict superset (it adds new-code duplication < 3 % and 100 % hotspots reviewed). Overall-code conditions are a deliberate non-goal until the pre-existing CRITICAL `S3776` smells are burned down (F-2 baseline: 29 `S3776` + 1 `S2208` as of the first gated scan).
- **Rust analysis** uses the official SonarSource Rust analyzer (CI-based only — no automatic analysis for Rust). Clippy findings are imported as external issues from `clippy-report.json` (`cargo clippy --message-format=json`), coverage from `lcov.info` (gitignored).
- **CI environment**: `libasound2-dev` + `libpipewire-0.3-dev` + `libpulse-dev` + `libfontconfig1-dev` + `libltc-dev` + `libclang-dev` + `ffmpeg` via apt (every pkg-config probe in the dependency graph: cpal→alsa, pipewire/pulse backends, slint/fontique→fontconfig, libltc-sys, bindgen→libclang), `PKG_CONFIG_PATH` set, Rust stable with `clippy` + `llvm-tools-preview`, cargo build cache via `Swatinem/rust-cache`, nextest + cargo-llvm-cov via `taiki-e/install-action`, SonarScanner CLI binaries cached in `~/.sonar/cache` via `actions/cache`. No Node/npm steps — the repo is Rust-only.
- **Local reports**: `SONAR_TOKEN=<token> scripts/sonar-report.sh` (needs `jq` on the dev machine; CI is unaffected) pulls quality-gate status (including a per-condition **new-code conditions** table), measures, open issues, and security hotspots from the SonarQube Cloud Web API into fixed-name JSON + `summary.md` under `reports/sonar/` (gitignored) — deterministic textual reports for AI agents or diffing.

## Build & Run

**Important:** `libltc-rs` requires the system `libltc` library. Install it and set `PKG_CONFIG_PATH`:
```bash
sudo apt install libltc-dev
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
```

**Windows x86_64 cross-compile** (Linux host → Windows binary): requires mingw-w64 linker + a prebuilt libltc for Windows. See the `[target.x86_64-pc-windows-gnu]` sections in `README.org` ("Cross Compiling Libltc on Linux for Windows builds") and the local `.cargo/config.toml` for the required env vars (`BINDGEN_EXTRA_CLANG_ARGS`).

**Optional:** `exiftool` is auto-detected at runtime for camera metadata extraction during card scans (AVCHD SEI, MP4/MOV tags). Install it for more accurate device auto-naming:
```bash
sudo apt install exiftool
# Or from source: https://exiftool.org
```
The probe gracefully falls back to ffprobe tags or filename patterns when exiftool is absent.

```bash
cargo build                          # Build all Rust crates (workspace; needs PKG_CONFIG_PATH)
cargo test                           # Run all Rust tests
cargo clippy --all-targets           # Lint all workspace crates
cd ltc-gui && cargo run --release    # Native Rust GUI (egui/eframe)
cd ltc-slint && cargo run --release  # Slint-based GUI
./build-all-rust-targets.sh          # Full ship matrix: linux x64, windows x64, linux i686 (docker cross-compile)
```
