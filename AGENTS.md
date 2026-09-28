# LTC Timecode Generator — Project Guide

## Overview
High-precision SMPTE Linear Timecode (LTC) audio signal generator + digital clapper-board for multi-camera video sync. Generates bi-phase mark modulated LTC audio and beep tones, routed to selectable stereo channels. It also **decodes** LTC from WAV/video files (quality reports, trim offsets) and **converts/exports** recordings (trim-to-first-LTC, timecode metadata embedding, channel splitting/dropping) via ffmpeg.

Four frontends share a common `audio-core` Rust crate:
1. **ltc-gui** (native Rust egui/eframe app — primary GUI target)
2. **ltc-slint** (Slint-based GUI, alternative frontend)
3. **Web app** (React + Vite, browser-based — legacy)
4. **Tauri v2** desktop (WebKitGTK + Rust backend — legacy, being phased out)

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
| **Web app** (legacy) | Browser frontend | React 19 + TypeScript + Vite 6 + Tailwind CSS 4 |
| **Tauri v2** (legacy) | Desktop wrapper for web frontend | Tauri 2 + `src-tauri/` |
| **32-bit legacy** | i686 builds for old tablets | Tauri v1, Docker cross-compile (`build-32bit.sh`) |

- **Workspace**: `audio-core`, `gui-engine`, `ltc-gui`, `ltc-slint` (see root `Cargo.toml`; `src-tauri` and `src-tauri-32bit` are excluded standalone crates). Workspace clippy lints: style/correctness/complexity/perf = warn.
- **Dependencies**: source of truth is each crate's `Cargo.toml`. Notable: `audio-core` uses cpal 0.18 (pulseaudio always; pipewire on non-32-bit Linux), `hound` (WAV IO), and `libltc-rs` (bindgen binding → requires system `libltc`, see Build & Run).

## Project Structure
```
├── src/                          # Web frontend (legacy)
│   ├── main.tsx                  # React entry point
│   ├── App.tsx                   # Main component — audio logic, scheduling, converter UI
│   ├── ltcGenerator.ts           # LTC signal/beep generation, timecode math
│   ├── ltcGenerator.test.ts      # Vitest unit tests
│   ├── ltcGoldenVectors.ts       # Golden test vectors
│   ├── types.ts                  # TypeScript types (Timecode, AudioSettings, etc.)
│   ├── index.css                 # Tailwind CSS
│   ├── components/               # TimecodeSettings, ClapperSlate, ConverterTab, FooterStatusBar, ToastContainer
│   └── utils/
│       └── audioBackend.ts       # Tauri/Web abstraction layer
├── gui-engine/                    # Shared Rust GUI engine crate (lib name: gui_engine)
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs                # Module decls + re-exports (ArcSwap, decode types, converter/file_pattern/ffprobe API)
│   │   ├── command.rs            # GuiCommand enum (source of truth for all commands)
│   │   ├── state.rs              # AppStateSnapshot + ClapLogItem (source of truth for published state)
│   │   ├── engine.rs             # Threaded engine loop, AudioCore lifecycle, retry/recovery, decode handling
│   │   ├── camera_meta.rs        # Camera model detection from clips (exiftool/ffprobe probe)
│   │   ├── cli.rs                # Cli struct, parse_args(), process_cli(); headless/WAV/list-devices/decode modes
│   │   ├── timecode.rs           # FPS_OPTIONS (24/25/29.97 ND/29.97 DF/30), timecode formatting helpers
│   │   ├── log_buffer.rs         # LogBuffer ring buffer + init_logger (canonical logger)
│   │   ├── theme.rs              # Shared dark/light ThemeColors palettes used by both Rust GUIs
│   │   ├── converter/            # Converter directory module (see Converter section)
│   │   │   ├── mod.rs            # Facade: re-exports public API from submodules
│   │   │   ├── progress.rs       # ConversionState, ConversionStatus, SharedConversionState, CancelFlag
│   │   │   ├── channel_map.rs    # ChannelMap (input→output permutation)
│   │   │   ├── timecode.rs       # TimecodeMetadata, format_ffmpeg_timecode, shift_timecode_back, TC math
│   │   │   ├── capabilities.rs   # FfmpegCapabilities, HwDeviceCapabilities, query_ffmpeg_capabilities
│   │   │   ├── formats.rs       # Codec/container compatibility: supported_*, available_*, select_best_combination
│   │   │   ├── settings.rs       # ConversionPipeline, RecordingType, ConverterSettings + output-path naming
│   │   │   ├── planning.rs       # AudioKeep, VideoOutputStep, plan_*, preview_output_files
│   │   │   ├── checks.rs         # conversion_sanity_check*, ConvertBlocker, evaluate_readiness
│   │   │   ├── args.rs           # ffmpeg argument builders (build_*_args, push_* helpers)
│   │   │   ├── process.rs        # run_ffmpeg_process, parse_out_time, classify_step_failure, StepFailure
│   │   │   └── runner.rs         # EncoderFallback, spawn_conversion, run_* pipeline orchestration
│   │   ├── tagger.rs            # In-place timecode tagging (native MOV/MP4 + WAV bext + ffmpeg fallback)
│   │   ├── file_pattern.rs      # Camera/recorder filename patterns + file grouping
│   │   ├── ffprobe.rs            # ffprobe video/audio probing + ffmpeg channel extraction
│   │   └── config.rs             # Converter config persistence (last input/output folders)
│   └── tests/                    # integration.rs, converter_integration.rs, video_extraction.rs
├── ltc-gui/                      # Native Rust GUI (egui/eframe) — target for weak-GPU tablets
│   ├── Cross.toml                # Cross-compilation config for i686 targets
│   └── src/
│       ├── main.rs               # process_cli() → eframe::run_native()
│       ├── app.rs                # AppState: tabs, keyboard shortcuts, toasts; reads engine state, sends commands
│       ├── theme.rs              # Bridges engine theme palettes to egui styles
│       └── widgets/              # clock.rs, clapper.rs, settings.rs, status.rs, converter.rs
├── ltc-slint/                    # Slint-based GUI (alternative frontend)
│   ├── build.rs                  # slint-build compiler for ui/
│   ├── src/
│   │   ├── main.rs               # Registers Slint callbacks → send GuiCommands; converter option wiring
│   │   ├── poll.rs               # Poll timer: engine_state.load() → Slint properties
│   │   └── theme.rs, toast.rs, timecode_helpers.rs
│   └── ui/                       # app.slint (root) + clapper/clock/converter/settings/status/theme/types/widgets.slint
├── audio-core/                   # Shared Rust audio crate (LTC generation + cpal output + decoders)
│   └── src/
│       ├── audio_output.rs       # AudioCore, device/stream lifecycle, config selection, error classification, scheduler thread
│       ├── lib.rs                # Types + re-exports + WavChunkReader + chunked decode
│       ├── ltc_encoder.rs        # get_ltc_bits, increment_timecode, generate_ltc_frame_stereo
│       ├── ltc_decoder.rs        # Builtin decoder + quality report
│       └── ltc_decoder_libltc.rs # libltc-binding decoder
├── src-tauri/                    # Tauri v2 (legacy, 64-bit) Rust backend: commands + Builder setup
├── src-tauri-32bit/              # Legacy Tauri v1 (Docker cross-compile for i686)
├── scripts/
│   └── sync-version.js           # Version propagation (see Version Management)
├── Cargo.toml                    # Workspace root
├── index.html, vite.config.ts, tsconfig.json, package.json
├── build-32bit.sh, build-all-rust-targets.sh, deploy-to-onedrive.sh
├── Dockerfile.gui-build, Dockerfile.i386-build
├── metadata.json, README.org, .env.example, .dockerignore
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

### Sole Source of Truth — State Ownership

The engine is the **sole source of truth** for all application state. GUIs hold only framework-level state (tab index, popup visibility, toast notifications, text-edit-in-progress buffers, scroll offsets). All user-configurable options — including every converter setting (container, codecs, split/drop/trim toggles, channel map, output paths, naming templates) — are engine-owned via `ConverterUserSettings` in `AppStateSnapshot.converter.settings`.

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
- **Offload** — scan cards, set parent folder/name, device naming, start/cancel offload
- **Shutdown** — graceful engine stop
- **Steppers** — up/down nudges for scene, take, and timecode segments

### AppStateSnapshot
The full application state is published as an `AppStateSnapshot` struct wrapped in `Arc<ArcSwap<AppStateSnapshot>>`. The engine thread calls `state.store(Arc::new(snapshot))` after each tick. The GUI calls `state.load()` to get the latest snapshot — this is lock-free and always returns the latest state without queue management.

Field groups (see `state.rs` for the full struct): generation counter; transport (is_playing/is_locked, current + start timecode); FPS; audio routing + device state; clapper metadata + clap log; engine-computed animations (clap flash alpha, arm angle); theme; status message + system time; drained `AudioEvent`s (surfaced as toasts by the GUI); decode state (decode FPS, decoder selection, decode result/error, in-flight flag + generation, video probe info, selected stream/channel, chunked decode progress); ffmpeg capability probe (`ffmpeg_caps`, `ffmpeg_probe_running` — engine-owned, async).

### Engine Thread Loop
The engine runs at ~25 fps (40ms ticks) — see `engine.rs::engine_main`:
0. **Spawn ffmpeg capability probe** — before the loop starts, a background thread runs `query_ffmpeg_capabilities()` (hw-validated) and sends the result via an internal mpsc channel.
1. **Drain folder scan results** — async scan result from background thread; applies any deferred `SelectRecording` that arrived while the scan was in flight.
2. **Drain commands** — non-blocking `try_recv()`; `Shutdown` or channel disconnect exits the loop. Converter commands are handled inline (selectors, setters, side-effects, config persistence, deferred `SelectRecording` while `groups_loading`) or dispatched to `process_command`.
3. **Drain async decode results** — generation-stamped, so stale results from rapid re-clicks are discarded. On completion, auto-applies LTC settings to converter (split/drop/start-from-LTC) once per generation.
4. **Drain async group decode results** — same generation gating; on full group completion, same auto-apply.
5. **Drain ffmpeg probe result** — sets `current.ffmpeg_caps`; calls `apply_available_defaults` to repair stale converter settings and `recompute_converter_derived`.
6. **Drain converter clip probe results** — populates `ltc_probe` for video groups; resizes `channel_map` to identity if probe channel count changed.
7. **Poll chunked/group decode progress** — updates progress pct from atomics
8. **Poll conversion progress** — reads engine-owned `SharedConversionState` and writes into snapshot's `converter.conversion_state`; resets shared state on completion/failure.
9. **Poll timecode** — `core.current_timecode()` when playing
10. **Drain events** — clears `state.events` first, then drains `core.drain_events()`, dispatches to recovery or forwards to `state.events`
11. **Animate** — flash alpha decay (2.0/s), arm angle exponential decay toward rest (4.0/s)
12. **Recompute converter-derived data** — on demand via `recompute_converter_derived()` (readiness, collision warning, output preview, encoder chain desc)
13. **Update system time**
14. **Publish** — increments generation, calls `state.store(Arc::new(snapshot))`
15. **Sleep** until next tick

### Audio Lifecycle
- **Init**: 3 retries with exponential backoff (50ms → 100ms → 200ms). Distinguishes permanent errors (permission denied — no retry) from transient (device busy — retry).
- **Recovery**: When `StreamDied` or `RecoveryNeeded` events are detected, the engine attempts up to 3 recovery cycles (stop → reinit → restart LTC if was playing).
- **Device switching**: Stops LTC, stops output, re-initializes on new device, restarts LTC. Reverts to previous device on failure.

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

Conversion execution runs in the engine thread via `StartConversion` which calls `assemble_converter_settings()` then `spawn_conversion()` with engine-owned `SharedConversionState` and `CancelFlag`. Progress is polled each tick and published into `ConverterSnapshot.conversion_state`.

### Components (`converter/` directory module)
  - `ConversionPipeline`: `AudioOnly { generate_synthetic_video }` (multi-track WAV → audio/video outputs), `VideoPassthrough` (camera clips → video outputs), and `MetadataOnly` (tag originals in place, rename, extract audio).
  - `ConverterSettings`: input files, `RecordingType` (MultiTrackAudio / VideoClipSequence), `ChannelMap` (input→output permutation), `split_tracks` / `drop_ltc_track` / `ltc_video_source`, container + video codec / audio encoder, output folder + naming templates (defaults `_audio_track{:01d}` / `_video_clip{:02d}`), `trim_to_first_ltc` + per-file trim offsets, per-file `TimecodeMetadata` (start TC, fps, drop-frame).
  - `conversion_sanity_check_metadata_only()` — lightweight preflight for `MetadataOnly` pipeline (only ffmpeg, file existence, template validation).
  - **Video encoder selection is codec-level** (see `video_codecs.rs` below): `ConverterSettings.video_encoder` stores a codec id (`"av1"`, `"h265"`, …); `resolved_video_encoder` holds the concrete ffmpeg encoder chosen at conversion time.
  - `query_ffmpeg_capabilities()` probes ffmpeg once; after listing encoders/formats and discovering HW devices (VAAPI/Vulkan), it **validates each hardware encoder candidate with a 1-frame test encode** (`-f lavfi -i testsrc=... -c:v <enc> -f null -` with 10s timeout). Non-functional encoders (missing driver, incompatible GPU) are removed from `available_encoders` so they never appear in the dropdown or encoder chain. `available_*_for_container()` filters audio encoders/containers; `select_best_combination()` picks defaults (codec-aware); `apply_available_defaults()` repairs stale settings.
  - `plan_video_outputs()` → ordered `VideoOutputStep` list (`VideoOnly`, `VideoMux` with `AudioKeep`, `AudioChannel` extraction); caller executes each step.
  - `spawn_conversion()` takes `caps: Option<&FfmpegCapabilities>`, resolves the codec into an ordered encoder chain, and runs the steps on a background thread, publishing `ConversionState` (`Idle`/`Running{progress}`/`Completed`/`Failed` + ffmpeg output lines); cancellation via `CancelFlag`. Failures are classified as `StepFailure::EncoderInit` (no output produced → retry with next encoder in the chain) or `StepFailure::Fatal`; failed encoders are memoized for the rest of the run and the first successful one is pinned (reported as `Video encoder used: …` in the log).
  - **Stream-copy mode** (`copy_video`, UI: "Leave Video Encoding Untouched", VideoPassthrough only): video is remuxed with `-c:v copy` — no encoder chain, no `-r`; muxed audio is `-c:a copy` unless channel filtering forces an audio-only re-encode. `prepare_copy_mode()` derives the output container from the input (`copy_mode_container_for_input()`: mp4/m4v→mp4, mov→mov, mkv→mkv, mxf→mxf, mts/m2ts/ts→mp4, else mkv), snaps each trim offset to the nearest video keyframe at-or-before it (`ffprobe::snap_trim_to_keyframe()` packet scan), and re-anchors the start timecode via `shift_timecode_back()` (DF-aware, inverse of `audio_core::increment_timecode`) so the embedded TC matches the actual first video frame. Sanity check: `conversion_sanity_check_with_naming(…, copy_video)` / `conversion_sanity_check_copy()` skip video-codec validation in this mode.
  - `evaluate_readiness()` / `ConvertBlocker` / `conversion_sanity_check()` — preflight validation surfaced in the UI before starting.
  - `format_ffmpeg_timecode()` (HH:MM:SS:FF or HH:MM:SS;FF), `find_timecode_at_offset()` (maps decode results → per-file start TC).
  - **`push_metadata_args()`** (`converter/args.rs`) — adds camera metadata to ffmpeg arg lists: `-metadata make/model` (or `com.apple.quicktime.*` for MOV) on video outputs, `-write_bext 1` + `originator` + `origination_date` on WAV outputs. Gated by `embed_camera_metadata`. Originator defaults to `"LTC Timecode Generator"` when no camera is detected. Origination date falls back to file mtime via `chrono`.
  - **`tagger.rs` bext extension** — in-place WAV tagging now writes `originator` (payload offset 256, 32 B, NUL-padded) and `origination_date` (payload offset 320, 10 B) in addition to `time_reference` (offset 338). A `chunk_size >= 346` guard rejects undersized bext chunks, falling back to ffmpeg remux.
  - **Camera metadata probe** — during converter clip probing, the engine also runs `camera_meta::probe_camera_info()` for each file (cheap second subprocess parallel to ffprobe). Results are published as `Vec<Option<CameraInfo>>` in `ConverterSnapshot.camera_meta`, generation-gated and cleared on recording re-selection.
  - **`embed_camera_metadata` toggle** — `ConverterUserSettings.embed_camera_metadata` (default `true`) is engine-owned. Both GUIs expose a checkbox (ltc-gui: converter widget step 3; ltc-slint: step 4 near Set Start Time from LTC). The converter metadata-only pipeline (`tagger::tag_file`, `run_tagging`) also accepts and embeds camera info.
- **`video_codecs.rs`** — codec → encoder registry (single source of truth for video encoding):
  - `VIDEO_CODECS`: per codec (`av1`, `h264`, `h265`, `prores`, `dnxhd`) the user-facing label, allowed containers, codec-level args (e.g. `-tag:v hvc1` for HEVC), and a priority-ordered `EncoderCandidate` chain — hardware encoders (nvenc/qsv/amf/mf/vaapi/vulkan/v4l2m2m) first, software encoders (libsvtav1/libaom-av1/libx264/libx265/prores_ks/dnxhd) as fallbacks. Candidates carry their own args (e.g. `pix_fmt=yuv420p`); hardware candidates negotiate pixel format themselves.
  - `available_video_codecs()` (dropdown source), `resolve_encoder_chain()` (ordered available candidates, gated by hw-device availability for vaapi/vulkan), `static_encoder_chain()`, `normalize_video_codec()` (maps legacy concrete encoder names like `libx264` to codec ids), `describe_chain()` (UI summary "av1_nvenc (hardware) → libsvtav1").
  - VAAPI / Vulkan candidates carry `hw_frames: Some(HwFramePath::…)` and are filtered by `FfmpegCapabilities::hw` in `resolve_encoder_chain`. At conversion time, the arg builders inject `-init_hw_device` / `-filter_hw_device` prelude args (pre-input) and `-vf format=nv12,hwupload` (per-encoder). Device discovery (`hw_device.rs`) enumerates `/dev/dri/renderD*` and probes with `ffmpeg -init_hw_device` at capability-query time; runtime fallback demotes candidates without an available device.
  - **`hw_device.rs`** — hardware probe primitives: `discover()` (VAAPI/Vulkan init probes), `list_vaapi_render_nodes()`, `probe_vaapi()`/`probe_vulkan()`. Also contains `test_encode()` / `test_encode_with()` (1-frame null encode with timeout) and `validate_hw_encoders()` / `validate_hw_encoders_with()` (walk all `VIDEO_CODECS` hw candidates, run test encode, remove failures from `available_encoders`). All functions accept the ffmpeg path for testability; injectable runner closures enable pure unit tests.
- **`file_pattern.rs`** — groups input files by naming convention. `BUILTIN_PATTERNS` (TASCAM Portacapture X8 `nameS<ch>`, `*` any) + `CAMERA_PATTERNS` (Sony Handycam, Sony FS100, Canon `MVI_`, Panasonic `GH`, GoPro `GOPR`/`GP`). `match_files_to_groups()` / `wrap_user_selected_files()` / `match_files_all_patterns()`; `default_output_filename()` derives the output name from the group.
- **`ffprobe.rs`** — `probe_video_audio()` (ffprobe JSON → `VideoAudioProbe` with per-stream channels/codec/sample-rate), `path_is_video()`, `extract_audio_channel()` (ffmpeg extraction used by decode), `snap_trim_to_keyframe()` / `parse_last_keyframe()` (keyframe packet scan used by stream-copy trim snapping).
- **`config.rs`** — persists last input/output folders to `<config_dir>/ltc-timecode-generator/converter_config.json`.
- **`tagger.rs`** — in-place timecode metadata tagger: dispatches MOV/MP4 (native O(1) in-place tagger: trailing moov → free + appended tmcd track + tiny mdat), WAV with existing bext (patches time_reference), and ffmpeg stream-copy remux (temp file + atomic rename) as fallback for other containers. `tag_file()` dispatches. `run_tagging()` runs batch tagging with progress/cancel for the metadata-only pipeline.

### Flow
Select files → group by naming pattern → probe (ffprobe) → (ffmpeg capabilities already probed async at engine startup) → readiness/blockers check → channel mapping + LTC track handling → trim-to-first-LTC + timecode metadata → `spawn_conversion` (progress + cancel). `MetadataOnly` pipeline skips trimming and encoder checks: originals are tagged in place (native MP4/MOV or ffmpeg remux), renamed, and audio extracted per channel. See `run_metadata_only()` and `tagger::tag_file()`.

## Native Rust GUI (`ltc-gui/`)

The native GUI is built with **egui 0.35 + eframe** (glow backend, vsync off). It is a thin rendering shell over `gui-engine` — all audio, decode, and conversion logic lives in the engine thread. This is the target frontend for weak-GPU tablets (Intel Atom + GMA 500) where WebKitGTK performance is unusable.

### Architecture
- **`AppState` struct** (`app.rs`): holds `cmd_tx` (command sender), `engine_state` (ArcSwap handle), theme, notifications, tab state (Clapper / Settings / Convert), debug log buffer. Implements `eframe::App`.
- **Frame loop**: `logic()` syncs `self.latest` from `engine_state.load()`, drains `self.latest.events` into toast notifications, processes keyboard shortcuts (Space/C/R/L/Ctrl+D → send GuiCommand), handles repaint scheduling.
- **Widgets** (`widgets/`): `clock` (glowing timecode display), `clapper` (board + arm + scene/take/roll + sync log), `settings` (FPS selector, steppers, device, routing, sliders), `status` (footer bar), `converter` (file picker via rfd, pattern/encoder selection, conversion progress). Widgets read from `state.latest.*` for display and call `state.send(GuiCommand::...)` for mutations.
- **Theme** (`theme.rs`): bridges the shared engine palettes (`gui_engine::theme`) to egui styles.
- **File dialogs**: `rfd` (native open/save dialogs) — used by the converter and decode flows.

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
- **`main.rs`** registers Slint callbacks that send `GuiCommand` variants and wires converter option models (containers/encoders from ffmpeg capabilities).
- **`poll.rs`** sets up the poll timer that reads `engine_state.load()` and updates Slint properties (timecode segments, FPS names, routing pills, clapper metadata, decode results, device names, debug log entries).
- **`toast.rs` / `theme.rs` / `timecode_helpers.rs`** — GUI-side toast management, palette application, and timecode segment formatting.
- **`ui/`** is split per concern: `app.slint` (root window + tabs) plus `clapper/clock/converter/settings/status/theme/types/widgets.slint`.
- Uses `rfd` for file dialogs and `arboard` for clipboard access.

## audio-core Crate

The `audio-core` crate provides the raw audio engine, split by concern:
- **`audio_output.rs`** — `AudioCore` (cpal output stream, ring buffers 128K LTC + 32K beep, scheduler thread, wake lock, event queue); `list_audio_devices()` / `AudioDeviceInfo`; config selection, stream building, device enumeration; error classification (`is_transient_audio_error`, `is_permanent_device_error`); `suggest_sample_rate()`; `SAMPLE_RATE_OPTIONS = &[44100, 48000]`. Extracted from `lib.rs` to isolate all device/stream lifecycle logic.
- **`lib.rs`** — public types (`Timecode`, `AudioEvent`, `AudioDeviceInfo`) + re-exports from `audio_output` + chunked parallel WAV decode (`WavChunkReader`, `DecodeConfig`, `DecodeProgress`, `decode_ltc_chunked`); `decode_ltc_with_decoder()`.
- **`ltc_encoder.rs`** — `get_ltc_bits()` (80-bit bi-phase mark frame), `increment_timecode()`, `compute_frame_sample_count()`, `generate_ltc_frame_stereo()`.
- **`ltc_decoder.rs`** — builtin pure-Rust decoder: `decode_ltc_samples()` / `decode_ltc_from_wav()`, first-coherent-frame alignment, `compute_ltc_quality()` (confidence, gaps, glitches), `quick_check_ltc()`; types `LtcDetectionResult`, `FrameTimecode`, `LtcQualityReport`, `LtcDecodeStatus`.
- **`ltc_decoder_libltc.rs`** — `decode_ltc_from_wav_libltc()` / `decode_ltc_samples_libltc()` via the `libltc-rs` binding (requires system `libltc`).
- Common types defined in `lib.rs`: `Timecode {hours, minutes, seconds, frames}`, `AudioEvent` (StreamError/StreamDied/StreamRecovering/StreamDead/RecoveryNeeded/Underrun/FramesDropped).

## Legacy Frontends

- **Web app** (`src/`, React + Vite): runs in two modes detected via `window.__TAURI_INTERNALS__` — Tauri mode sends high-level commands to Rust (audio-core does all sample generation, no audio data over IPC) while browser mode uses the Web Audio API directly. Includes a converter tab (`ConverterTab.tsx`) and vitest tests with golden vectors.
- **Tauri v2** (`src-tauri/`): 19 Tauri commands backed by `audio-core` (path dependency). Production builds via `npx tauri build` → AppImage/deb/msi.
- **32-bit** (`src-tauri-32bit/`): Tauri v1 codebase cross-compiled for i686 inside Docker via `build-32bit.sh`.

## CLI

`gui_engine::cli` (shared by both Rust GUIs; binary `ltc-gui`). Modes: `--list-devices/-l`, `--output-to-file <PATH>` (WAV render), `--headless/-H` (live playback, ctrlc handler), `--decode <PATH>` (decode + summary), otherwise GUI.

Flag groups (see `cli.rs::Cli` for the full list with defaults):
- **Playback**: `--device <NAME>` / `--device-index <N>`, `--start-timecode` (default `01:00:00:00`), `--fps` (24/25/29.97/30), `--drop-frame`, `--channel left|right|both`, `--volume`, `--sample-rate`, `--duration`
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

The **single source of truth** is `package.json`'s `"version"` field. All other files are derived from it.

### How to bump the version
```bash
npm version patch    # 0.3.2 → 0.3.3 (syncs files, creates git commit + tag v0.3.3)
npm version minor    # 0.3.2 → 0.4.0
npm version major    # 0.3.2 → 1.0.0
```

The `"version"` npm lifecycle hook runs `scripts/sync-version.js` automatically during `npm version` — after bumping `package.json` but before the git commit and tag. The script:
1. Propagates the version to `src-tauri/tauri.conf.json`, `src-tauri-32bit/tauri.conf.json`, and the `[package]` version in all six crate manifests: `audio-core/Cargo.toml`, `gui-engine/Cargo.toml`, `ltc-gui/Cargo.toml`, `ltc-slint/Cargo.toml`, `src-tauri/Cargo.toml`, `src-tauri-32bit/Cargo.toml`
2. Updates `package-lock.json`, workspace `Cargo.lock` (covers all four workspace members), `src-tauri/Cargo.lock`, `src-tauri-32bit/Cargo.lock` — lock files are synced with `cargo update --workspace`, which only re-locks the workspace/path-dep crate versions and never re-resolves third-party dependencies
3. Stages all affected files with `git add` (they become part of the `npm version` commit)

### Manual sync (without bumping)
```bash
node scripts/sync-version.js
```

### UI Display
The version is injected at build time via Vite's `define` (`import.meta.env.VITE_APP_VERSION`) and displayed in the app header as `LTC ENGINE v{version}`. The Slint GUI reads it from `CARGO_PKG_VERSION`.

## Testing

```bash
cargo test                           # All Rust crates: unit tests in modules + integration suites
cargo test -p gui-engine             # Just the gui-engine crate
npm test                             # Web: vitest run (ltcGenerator tests + golden vectors)
npm run lint                         # TypeScript typecheck (tsc --noEmit)
cargo clippy --all-targets           # Lint all workspace crates
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
   adapts to machine load. Fix examples in `test_conversion_cancellation`:
   it polls until `Running` before cancelling, never sleeping a fixed 200ms.

3. **Decay-tolerant tolerances** — Temporal assertions (e.g. flash-alpha after a
   clap) must account for engine-tick decay. If testing through the state-snapshot
   path, the observed snapshot may be 1-2 ticks old. Prefer synchronous unit tests
   for fine-grained state checks; keep integration assertions coarse.

4. **No real user config** — Engine tests set `XDG_CONFIG_HOME` to a tempdir via
   `init_test_config()`, preventing writes to `~/.config/`. All test engine
   spawns must call `init_test_config()` first.

5. **No real ffmpeg probe** — Engine tests use `engine_main_with_probe()` with
   `fake_probe()`, skipping the real ffmpeg-capability subprocess probe.
   This removes N concurrent `ffmpeg -encoders` calls per test run and the
   mid-test `apply_available_defaults` mutation. The real probe path is exercised
   by converter integration tests that call `query_ffmpeg_capabilities()` directly.

6. **Loud skips** — Tests that require ffmpeg/ffprobe use `eprintln!("--- SKIPPED: ...")`
   so skips are visible in the output, never silent.

7. **Tooling** — `cargo-nextest` is installed and configured (`.config/nextest.toml`).
   Each test runs in its own process, eliminating shared-state races. Use
   `npm run test:flaky` (alias for `cargo nextest run --profile ci`) for CI
   repeat runs: `retries=3`, `final-status-level=flaky` highlights tests that
   passed only on retry. Locally, `cargo nextest run --retries 10 -E 'test(...)'`
   ruthlessly shakes out timing flakes in a target test.

### Integration Suites

- `gui-engine/tests/integration.rs` — engine-thread command processing
- `gui-engine/tests/converter_integration.rs` — conversion pipelines (real ffmpeg)
- `gui-engine/tests/video_extraction.rs` — ffprobe/ffmpeg extraction (real ffmpeg)

Golden vectors for the web LTC generator live in `src/ltcGoldenVectors.ts`.

## Build & Run

**Important:** `libltc-rs` requires the system `libltc` library. Install it and set `PKG_CONFIG_PATH`:
```bash
sudo apt install libltc-dev
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
```

**Optional:** `exiftool` is auto-detected at runtime for camera metadata extraction during card scans (AVCHD SEI, MP4/MOV tags). Install it for more accurate device auto-naming:
```bash
sudo apt install exiftool
# Or from source: https://exiftool.org
```
The probe gracefully falls back to ffprobe tags or filename patterns when exiftool is absent.

```bash
npm run dev                          # Vite dev server on port 3000
npx tauri dev                        # Tauri dev mode (starts Vite + Rust)
npx tauri build                      # Production build (AppImage/deb/msi)
npm run lint                         # tsc --noEmit
npm run clean                        # rm -rf dist src-tauri/target src-tauri-32bit/target ltc-gui/target
cargo build                          # Build all Rust crates (workspace; needs PKG_CONFIG_PATH)
cargo test                           # Run all Rust tests
cargo clippy --all-targets           # Lint all workspace crates
cd ltc-gui && cargo run --release    # Native Rust GUI (egui/eframe)
cd ltc-slint && cargo run --release  # Slint-based GUI
./build-32bit.sh                     # Docker cross-compile for i686
```
