# LTC Timecode Generator — Project Guide

## Overview
High-precision SMPTE Linear Timecode (LTC) audio signal generator + digital clapper-board for multi-camera video sync. Generates bi-phase mark modulated LTC audio and beep tones, routed to selectable stereo channels. Four frontends share a common `audio-core` Rust crate:
1. **Web app** (React + Vite, browser-based)
2. **Tauri v2** desktop (WebKitGTK + Rust backend, being phased out)
3. **ltc-gui** (native Rust egui/eframe app — the target for weak-GPU tablets)
4. **ltc-slint** (experimental Slint-based GUI spike)

Both Rust GUIs delegate all audio lifecycle, state management, and CLI handling to the shared **`gui-engine`** crate via an event-driven message bus.

## Tech Stack
- **Frontend**: React 19 + TypeScript + Vite 6 + Tailwind CSS 4 + `lucide-react` icons + `motion` + `@google/genai`
- **Desktop**: Tauri v2 (`@tauri-apps/cli` v2.11.4)
- **Rust Backend**: Tauri v2.11.3, audio-core (path dep), serde/serde_json, tauri-plugin-log 2
- **audio-core** (shared crate): cpal 0.18, keepawake 0.6, ringbuf 0.3, serde, log, thread-priority 0.5; conditional pipewire on 64-bit Linux
- **gui-engine** (shared GUI engine): audio-core, arc-swap 1.7, chrono 0.4, clap 4, ctrlc 3.4, env_logger 0.11, hound 3.5, log 0.4
- **Native GUI** (ltc-gui): gui-engine, eframe 0.35 (glow), egui 0.35
- **Slint GUI** (ltc-slint): gui-engine, slint 1.17, slint-build 1.17, arboard 3
- **Build**: `npm run build` → `dist/`, `npx tauri build` → AppImage/deb/msi, `cargo build` (workspace builds all Rust crates)
- **32-bit Legacy**: `src-tauri-32bit/` (Tauri v1, Docker cross-compile via `build-32bit.sh`)

## Project Structure
```
├── src/                          # Frontend source (web app)
│   ├── main.tsx                  # React entry point
│   ├── App.tsx                   # Main component — all audio logic, scheduling, UI
│   ├── ltcGenerator.ts           # LTC signal generation, beep generation, timecode math
│   ├── types.ts                  # TypeScript types (Timecode, AudioSettings, etc.)
│   ├── index.css                 # Tailwind CSS
│   ├── components/
│   │   ├── TimecodeSettings.tsx   # Settings panel
│   │   ├── ClapperSlate.tsx       # Clapper slate UI + log
│   │   ├── FooterStatusBar.tsx    # Status bar
│   │   └── ToastContainer.tsx     # Toast notification overlay
│   └── utils/
│       └── audioBackend.ts        # Tauri/Web abstraction layer
├── gui-engine/                   # Shared Rust GUI engine (owns AudioCore + state)
│   ├── Cargo.toml                # Deps: audio-core, arc-swap, chrono, clap, ctrlc, env_logger, hound, log
│   └── src/
│       ├── lib.rs                # Re-exports: ArcSwap, AudioEvent, SAMPLE_RATE_OPTIONS, etc.
│       ├── command.rs            # GuiCommand enum (26 variants)
│       ├── state.rs              # AppStateSnapshot struct (30 fields), ClapLogItem
│       ├── timecode.rs           # FPS_OPTIONS, timecode_to_string, timecode_to_ms_string, chrono_now_string
│       ├── cli.rs                # Cli struct, parse_args(), process_cli(), headless/WAV/list-device modes
│       ├── log_buffer.rs         # LogBuffer ring-buffer + CapturingLogger (canonical version)
│       └── engine.rs             # Threaded engine loop, AudioCore lifecycle, retry/recovery, clap, animations
├── ltc-gui/                      # Native Rust GUI (egui/eframe) — target for weak-GPU tablets
│   ├── Cargo.toml                # Deps: gui-engine, eframe 0.35 (glow), egui 0.35
│   ├── Cross.toml                # Cross-compilation config for i686 targets
│   └── src/
│       ├── main.rs               # 17 lines: process_cli() → eframe::run_native()
│       ├── app.rs                # Thin AppState: reads engine state, sends commands, renders UI
│       ├── theme.rs              # Dark/light theme colors (matches CSS custom properties)
│       └── widgets/
│           ├── mod.rs
│           ├── clock.rs          # Glowing timecode display
│           ├── clapper.rs        # Clapper board + arm + scene/take/roll + sync log
│           ├── settings.rs       # FPS selector, steppers, device, routing, sliders
│           └── status.rs         # Footer bar (OS, devices, audio status, wake lock)
├── ltc-slint/                    # Experimental Slint-based GUI spike
│   ├── Cargo.toml                # Deps: gui-engine, slint 1.17, arboard 3
│   ├── build.rs                  # slint-build compiler for ui/app.slint
│   ├── src/
│   │   └── main.rs               # ~640 lines: callbacks send commands, timer reads ArcSwap state
│   └── ui/
│       └── app.slint             # Slint UI markup (layout + components)
├── audio-core/                   # Shared Rust audio crate (LTC generation + cpal output + scheduler)
│   ├── Cargo.toml                # Deps: cpal 0.18, keepawake 0.6, ringbuf 0.3, serde, log, thread-priority 0.5
│   └── src/lib.rs                # AudioCore, LTC/beep generation, cpal stream, scheduler thread
├── src-tauri/                    # Tauri v2 (main 64-bit) Rust backend
│   ├── Cargo.toml, build.rs, tauri.conf.json, capabilities/, icons/, gen/
│   └── src/
│       ├── main.rs
│       └── lib.rs                # 11 Tauri commands + Builder setup
├── src-tauri-32bit/              # Legacy Tauri v1 (Docker cross-compile for i686)
├── Cargo.toml                    # Workspace root: members = [audio-core, gui-engine, ltc-gui, ltc-slint]
├── Cargo.lock
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
                           Engine Thread (gui-engine)
                     (owns AudioCore + AppStateSnapshot)
                                       │
                                       ▼
                    Arc<ArcSwap<AppStateSnapshot>>
                      (lock-free, always latest)
                                       │
                                       ▼
                    GUI reads state.load() each frame
```

### GuiCommand (26 variants)
Commands are sent from the GUI thread to the engine via `mpsc::Sender<GuiCommand>`:
- **Transport**: `StartLtc`, `StopLtc`, `Reset`, `ToggleLock`, `Clap`
- **Timecode/FPS**: `SetStartTimecode(Timecode)`, `SetFpsIndex(usize)`
- **Audio**: `InitAudio`, `SetSampleRate(u32)`, `SetDevice(usize)`, `RefreshDevices`, `SetLtcChannel(String)`, `SetBeepChannel(String)`, `SetLtcVolume(f32)`, `SetBeepVolume(f32)`, `SetBeepFrequency(f32)`, `SetBeepDuration(f32)`
- **Clapper**: `SetScene(u32)`, `SetTake(u32)`, `SetRoll(String)`, `SetAutoIncrement(bool)`
- **Steppers**: `SceneUp/Down`, `TakeUp/Down`, `HourUp/Down`, `MinuteUp/Down`, `SecondUp/Down`, `FrameUp/Down`

### AppStateSnapshot
The full application state is published as an `AppStateSnapshot` struct (~500 bytes) wrapped in `Arc<ArcSwap<AppStateSnapshot>>`. The engine thread calls `state.store(Arc::new(snapshot))` after each tick. The GUI calls `state.load()` to get the latest snapshot — this is lock-free and always returns the latest state without queue management.

Fields: `generation`, `is_playing`, `is_locked`, `current_timecode`, `start_timecode`, `fps_index`, `fps`, `drop_frame`, `ltc_channel`, `beep_channel`, `ltc_volume`, `beep_volume`, `beep_frequency`, `beep_duration`, `devices`, `selected_device`, `audio_initialized`, `sample_rate`, `sample_format_name`, `wake_lock_active`, `scene`, `take`, `roll`, `auto_increment_take`, `logs`, `clap_flash_alpha`, `clap_arm_angle`, `status_message`, `system_time`, `events`.

### Engine Thread Loop
The engine runs at ~25 fps (40ms ticks):
1. **Drain commands** — non-blocking `try_recv()` on the `mpsc::Receiver`
2. **Poll timecode** — `core.current_timecode()` when playing
3. **Drain events** — `core.drain_events()`, dispatches to recovery or forwards to state.events
4. **Animate** — flash alpha decay (2.0/s), arm angle exponential decay (4.0/s)
5. **Publish** — increments generation, calls `state.store(Arc::new(snapshot))`

### Audio Lifecycle
- **Init**: 3 retries with exponential backoff (50ms → 100ms → 200ms). Distinguishes permanent errors (permission denied — no retry) from transient (device busy — retry).
- **Recovery**: When `StreamDied` or `RecoveryNeeded` events are detected, the engine attempts up to 3 recovery cycles (stop → reinit → restart LTC if was playing).
- **Device switching**: Stops LTC, stops output, re-initializes on new device, restarts LTC. Reverts to previous device on failure.

### CLI Modes
The `gui_engine::cli::process_cli()` function handles all non-GUI modes:
- `--list-devices` / `-l` — prints devices and exits
- `--output-to-file <PATH>` — generates WAV file and exits
- `--headless` / `-H` — runs headless playback with ctrlc handler
- Otherwise — returns `CliOutcome::RunGui { cmd_tx, state }` for the GUI to consume

## Native Rust GUI (`ltc-gui/`)

The native GUI is built with **egui 0.35 + eframe** (glow backend). It is a thin rendering shell over `gui-engine` — all audio and state logic lives in the engine thread. This is the target frontend for weak-GPU tablets (Intel Atom + GMA 500) where WebKitGTK performance is unusable.

### Architecture
- **`AppState` struct** (`app.rs`): holds `cmd_tx` (command sender), `engine_state` (ArcSwap handle), theme, notifications, tab state, debug log buffer. Implements `eframe::App`.
- **Frame loop**: `logic()` syncs `self.latest` from `engine_state.load()`, drains `self.latest.events` into toast notifications, processes keyboard shortcuts (Space/C/R/L/Ctrl+D → send GuiCommand), handles repaint scheduling.
- **Widgets** (`widgets/`): read from `state.latest.*` for display, call `state.send(GuiCommand::...)` for mutations.
- **Theme system** (`theme.rs`): unchanged — purely GUI-side.

### Threading
- **GUI thread**: egui immediate-mode rendering at 25-60 fps. Never touches AudioCore. Reads lock-free from ArcSwap.
- **Engine thread**: Owns AudioCore. Receives commands via mpsc. Publishes state via ArcSwap. Sleeps 40ms between ticks.
- **Zero mutex contention**: The GUI thread never locks AudioCore. The engine thread owns it exclusively.

### Build & Run
```bash
cd ltc-gui
cargo run                           # Debug build
cargo run --release                 # Release build (~12MB stripped)
LIBGL_ALWAYS_SOFTWARE=1 cargo run   # Force software OpenGL rendering
```

## Slint GUI (`ltc-slint/`)

The Slint GUI follows the same pattern as ltc-gui — thin shell over `gui-engine`. The `main.rs` (~640 lines, down from 1855) registers Slint callbacks that send `GuiCommand` variants, and a poll timer reads state from `engine_state.load()` and updates Slint properties (timecode segments, FPS name, routing pills, clapper metadata, events, device names, debug log entries). Toast management and pulse-phase dot animation remain GUI-side.

## audio-core Crate

The `audio-core` crate provides the raw audio engine:
- **`AudioCore`**: Encapsulates cpal output stream, ring buffers (128K LTC + 32K beep), scheduler thread, wake lock, event queue.
- **`Timecode`**: `{hours, minutes, seconds, frames}` with Clone, Copy, Debug, PartialEq, Serialize, Deserialize.
- **`AudioEvent`**: `StreamError`, `StreamDied`, `StreamRecovering`, `StreamDead`, `RecoveryNeeded`, `Underrun`, `FramesDropped`.
- **`AudioDeviceInfo`**: `{id, name, is_default, formats, channels_min/max, sample_rate_min/max, buffer_min/max}` with Clone, Debug, Serialize.
- **Free functions**: `list_audio_devices()`, `get_ltc_bits()`, `increment_timecode()`, `generate_ltc_frame_stereo()`, `suggest_sample_rate()`, `is_transient_audio_error()`, `is_permanent_device_error()`.
- **Constants**: `SAMPLE_RATE_OPTIONS = &[16000, 48000]`.

## Key Architecture — Dual Audio Backend

The app runs in **two modes**, detected at runtime via `window.__TAURI_INTERNALS__`:

### Tauri Mode (Desktop)
- **Device detection**: Rust `get_audio_devices` command via cpal
- **Audio output**: JS sends high-level commands to Rust (`start_ltc_stream`, `play_beep`). Rust's `audio-core` handles ALL sample generation internally. No audio data over IPC.
- **Rust playback**: cpal output stream with 2-channel config, reads from ringbufs, sums LTC + beep, writes silence on underrun
- **Timing**: `performance.now()` relative to `tauriStartTimeRef`; Rust maintains own timing via scheduler thread
- **Sample rate**: 16000 or 48000 Hz (auto-suggested based on CPU cores)

### Web Mode (Browser)
- **Device detection**: `navigator.mediaDevices.enumerateDevices()` + `getUserMedia()`
- **Audio output**: Web Audio API (AudioContext, AudioBufferSourceNode, OscillatorNode, ChannelMergerNode)
- **Timing**: `audioCtx.currentTime`
- **Sample rate**: 16000 or 48000 Hz (auto-suggested, falls back to default)

## Version Management

The **single source of truth** is `package.json`'s `"version"` field. All other files are derived from it.

### How to bump the version
```bash
npm version patch    # 0.1.0 → 0.1.1 (syncs all files, creates git commit + tag v0.1.1)
npm version minor    # 0.1.0 → 0.2.0
npm version major    # 0.1.0 → 1.0.0
```

The `"version"` npm lifecycle hook runs `scripts/sync-version.js` automatically during `npm version` — after bumping `package.json` but before the git commit and tag. The script:
1. Propagates the version to all Cargo.toml and tauri.conf.json files
2. Runs `npm install` to update `package-lock.json`
3. Runs `cargo generate-lockfile` in the workspace and each standalone crate to update `Cargo.lock` files

### Manual sync (without bumping)
```bash
node scripts/sync-version.js
```

### UI Display
The version is injected at build time via Vite's `define` (`import.meta.env.VITE_APP_VERSION`) and displayed in the app header as `LTC ENGINE v{version}`.

## Build & Run

**Important:** `libltc-rs` requires the system `libltc` library. Install it and set `PKG_CONFIG_PATH`:
```bash
sudo apt install libltc-dev
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
```

```bash
npm run dev                          # Vite dev server on port 3000
npx tauri dev                        # Tauri dev mode (starts Vite + Rust)
npx tauri build                      # Production build
npm run lint                         # tsc --noEmit
npm run clean                        # rm -rf dist src-tauri/target ...
cargo build                          # Build all Rust crates (workspace; needs PKG_CONFIG_PATH)
cd ltc-gui && cargo run --release    # Native Rust GUI (egui/eframe)
cd ltc-slint && cargo run --release  # Slint-based GUI spike
cargo clippy --all-targets            # Run clippy on all workspace crates
./build-32bit.sh                     # Docker cross-compile for i686
```

### LTC Decoding (CLI)

Two decoders are available, selectable via `--decoder`:
```bash
# builtin — pure Rust, accurate but slow (~74s for a 20s file)
ltc-gui --decode file.wav --decoder builtin
# libltc — C library, fast (~135ms for a 20s file)  
ltc-gui --decode file.wav --decoder libltc
```

The `--decode` flag reads a WAV file, decodes all LTC frames, and prints a summary. Both decoders share the same `LtcDetectionResult` output type.