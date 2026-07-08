# LTC Timecode Generator — Project Guide

## Overview
High-precision SMPTE Linear Timecode (LTC) audio signal generator + digital clapper-board for multi-camera video sync. Generates bi-phase mark modulated LTC audio and beep tones, routed to selectable stereo channels. Three frontends share a common `audio-core` Rust crate:
1. **Web app** (React + Vite, browser-based)
2. **Tauri v2** desktop (WebKitGTK + Rust backend, being phased out)
3. **ltc-gui** (native Rust egui/eframe app — the target for weak-GPU tablets)

## Tech Stack
- **Frontend**: React 19 + TypeScript + Vite 6 + Tailwind CSS 4 + `lucide-react` icons + `motion` (framer-motion)
- **Desktop**: Tauri v2 (`@tauri-apps/cli` v2.11.4)
- **Rust Backend**: Tauri v2.11.3, cpal 0.18, serde/serde_json, tauri-plugin-log, std::sync::mpsc
- **Native GUI**: egui 0.35 + eframe (glow backend), direct `audio-core` dependency, pure Rust
- **Build**: `npm run build` → `dist/`, `npx tauri build` → AppImage/deb/msi, `cargo build` in `ltc-gui/` → native binary
- **32-bit Legacy**: `src-tauri-32bit/` (Tauri v1, Docker cross-compile via `build-32bit.sh`)

## Project Structure
```
├── src/                          # Frontend source (web app)
│   ├── main.tsx                  # React entry point
│   ├── App.tsx                   # Main component (1034 lines) — all audio logic, scheduling, UI
│   ├── ltcGenerator.ts           # LTC signal generation, beep generation, timecode math
│   ├── types.ts                  # TypeScript types (Timecode, AudioSettings, etc.)
│   ├── index.css                 # Tailwind CSS
│   ├── components/
│   │   ├── TimecodeSettings.tsx   # Settings panel (frame rate, audio device, routing, volume)
│   │   ├── ClapperSlate.tsx       # Clapper slate UI + log
│   │   └── FooterStatusBar.tsx    # Status bar (OS, audio devices, wake lock, battery)
│   └── utils/
│       └── audioBackend.ts        # Tauri/Web abstraction layer (device detection, audio output)
├── ltc-gui/                      # Native Rust GUI (egui/eframe) — target for weak-GPU tablets
│   ├── Cargo.toml                # Deps: eframe 0.35 (glow), egui 0.35, audio-core
│   └── src/
│       ├── main.rs               # eframe::run_native() entry point
│       ├── app.rs                # AppState struct + eframe::App impl + audio integration
│       ├── theme.rs              # Dark/light theme colors (matches CSS custom properties)
│       └── widgets/
│           ├── mod.rs            # Shared widget helpers (pill)
│           ├── clock.rs          # Glowing timecode display (large digits + milliseconds)
│           ├── clapper.rs        # Clapper board + arm animation + scene/take/roll + sync log
│           ├── settings.rs       # FPS selector, timecode steppers, device dropdown, routing, sliders
│           └── status.rs         # OS, devices, audio status, system time, LIVE/IDLE indicator
├── audio-core/                   # Shared Rust audio crate (LTC generation + cpal output + scheduler)
│   ├── Cargo.toml                # Deps: cpal 0.18, serde, log
│   └── src/lib.rs                # AudioCore struct, LTC/beep generation, cpal stream, scheduler thread
├── src-tauri/                    # Tauri v2 (main 64-bit) Rust backend
│   ├── Cargo.toml                # Rust deps: tauri 2.11.3, cpal 0.18, serde, log
│   ├── tauri.conf.json           # Window 800x800, resizable, CSP null, bundleMediaFramework
│   ├── capabilities/default.json # core:default permissions
│   └── src/lib.rs                # Rust audio commands (cpal + mpsc channel)
├── src-tauri-32bit/              # Legacy Tauri v1 (Docker cross-compile for i686)
├── index.html, vite.config.ts, tsconfig.json
├── package.json                  # Scripts: dev, build, preview, clean, lint (tsc --noEmit)
└── build-32bit.sh                # Docker build script for 32-bit
```

## Key Architecture — Dual Audio Backend

The app runs in **two modes**, detected at runtime via `window.__TAURI_INTERNALS__`:

### Tauri Mode (Desktop)
- **Device detection**: Rust `get_audio_devices` command via cpal (returns `{id, name, is_default}`)
- **Audio output**: JS generates raw Float32Array samples via `generateLTCFrameSamples()` / `generateBeepSamples()`, interleaves to stereo, sends to Rust via IPC `push_audio_samples`
- **Rust playback**: cpal output stream with 2-channel config, reads from `mpsc::sync_channel` (128-capacity), writes silence on underrun
- **Timing**: `performance.now()` relative to `tauriStartTimeRef`
- **Sample rate**: 16000 Hz fixed, buffer size 256 fixed

### Web Mode (Browser)
- **Device detection**: `navigator.mediaDevices.enumerateDevices()` + `getUserMedia()` for permission
- **Audio output**: Web Audio API (AudioContext, AudioBufferSourceNode, OscillatorNode, ChannelMergerNode)
- **Timing**: `audioCtx.currentTime`
- **Sample rate**: 16000 Hz (optimized, falls back to default)

## Audio Backend Abstraction (`src/utils/audioBackend.ts`)
- `isTauri()` — checks `window.__TAURI_INTERNALS__`
- `getAudioDevices()` — calls `get_audio_devices` Rust command OR `enumerateDevices`
- `initAudioOutput(deviceId, sampleRate, bufferSize)` — starts cpal stream
- `pushAudioSamples(samples: Float32Array)` — sends stereo samples to Rust
- `stopAudioOutput()` — drops cpal stream
- `playBeep(sampleRate, freq, dur, vol, channel)` — triggers Rust-side beep generation (no audio data over IPC)
- `requestAudioPermission()` — getUserMedia in web, no-op in Tauri
- `selectAudioOutputNative()` — browser picker, no-op in Tauri
- No `@tauri-apps/api` npm dependency — uses raw `window.__TAURI_INTERNALS__.invoke`

## Native Rust GUI (`ltc-gui/`)

The native GUI is built with **egui 0.35 + eframe** (glow backend) and talks directly to `audio-core` — no IPC, no serialization, no webview. This is the target frontend for weak-GPU tablets (Intel Atom + GMA 500) where WebKitGTK performance is unusable.

### Architecture
- **`AppState` struct** (`app.rs`): holds all UI state + `Mutex<AudioCore>`. Implements `eframe::App` with `logic()` (state updates, clock polling, animation) and `ui()` (rendering).
- **Direct audio-core calls**: `self.audio_core.lock().unwrap().start_ltc(...)`, `.play_beep(...)`, `.current_timecode()` — no IPC overhead.
- **Theme system** (`theme.rs`): Dark/light mode with `egui::Context::set_visuals()`. Colors match the CSS custom properties from `src/index.css` (e.g., `--bg-app: #0A0A0B`).
- **Custom widgets** (`widgets/`): `clock` (large timecode display), `clapper` (animated slate board), `settings` (fps/device/routing), `status` (footer bar).

### Rendering Backend
- Uses `eframe` with `glow` (OpenGL) backend, `default-features = false`, features: `["default_fonts", "glow", "wayland", "x11"]`.
- On hardware without GPU acceleration (GMA 500): Mesa's `llvmpipe` software OpenGL renderer provides fallback via `LIBGL_ALWAYS_SOFTWARE=1`.
- No GPU required for acceptable performance — egui is immediate-mode, only visible widgets are drawn.

### Build & Run
```bash
cd ltc-gui
cargo run                           # Debug build (fast compile, slower runtime)
cargo run --release                 # Release build (optimized, ~12MB stripped binary)
LIBGL_ALWAYS_SOFTWARE=1 cargo run   # Force software OpenGL rendering (for testing on GPU-less systems)
```

### Key Differences from Web/Tauri Frontends
| Aspect | Web/Tauri | ltc-gui |
|--------|-----------|---------|
| Audio IPC | JSON serialization over Tauri commands | Direct `AudioCore` method calls |
| Timing | `performance.now()` / `audioCtx.currentTime` | `AudioCore::current_timecode()` polling |
| UI framework | React 19 + Tailwind CSS | egui 0.35 (immediate mode) |
| Rendering | WebKitGTK (WebView) | glow (OpenGL) / llvmpipe (software) |
| Binary size | ~100MB+ (with WebKit runtime) | ~12MB stripped |
| Target | Modern hardware | Weak-GPU tablets (Intel Atom) |


| Command | Args | Returns | Description |
|---------|------|---------|-------------|
| `get_audio_devices` | none | `Vec<AudioDeviceInfo>` | Lists output devices via cpal |
| `init_audio_output` | `device_id`, `sample_rate`, `buffer_size` | `()` | Creates cpal 2-channel output stream, stores in AppState |
| `push_audio_samples` | `samples: Vec<f32>` | `()` | Pushes stereo interleaved samples to mpsc channel |
| `stop_audio_output` | none | `()` | Drops stream and sender |
| `play_beep` | `sample_rate`, `frequency`, `duration`, `volume`, `channel` | `()` | Generates sine beep in Rust, mixed into output stream |

**AppState** managed via `tauri::Builder::manage()`: `Mutex<Option<AudioOutputState>>` containing `(SyncSender<Vec<f32>>, Stream, Arc<Mutex<BeepState>>)`.

**Beep mixing**: The audio callback reads from the LTC mpsc channel AND the `BeepState` buffer. Beep samples are generated in Rust via `generate_beep_samples()` with envelope (5ms attack, 20ms release) and channel routing (left/right/both). The callback sums `ltc_val + beep_val` per sample. This avoids sending audio data over IPC — the JS only sends a lightweight command with parameters.

## Key App.tsx Functions (branching on `isTauriMode`)
- `initAudio()` — Tauri: calls `initAudioOutput()` + sets `tauriStartTimeRef`; Web: creates AudioContext + mixer graph
- `startStreaming()` — Tauri: `generateLTCFrameSamples` → `monoToStereo` → `pushAudioSamples` loop (setInterval 30ms, schedules 500ms ahead); Web: `generateLTCFrameBuffer` → `AudioBufferSourceNode` loop (setInterval 50ms, schedules 1.5s ahead)
- `stopStreaming()` — Tauri: `tauriStopAudio`; Web: stop all AudioBufferSourceNodes
- `handleClapTriggered()` — Tauri: `tauriPlayBeep` (lightweight IPC, no audio data); Web: `playClapperBeep` (OscillatorNode)
- `handleReset()` — Tauri: clear scheduled frames + reset timer; Web: stop sources + reset nextFrameTime
- `getCurrentTimecode()`, `updateVisualClock()` — both use `isTauriMode ? performance.now() : audioCtx.currentTime`
- `monoToStereo(mono, channel)` — interleaves mono to stereo with L/R/both routing, used only in Tauri mode

## LTC Generation (`src/ltcGenerator.ts`)
- `getLTCBits(h,m,s,f,dropFrame)` — encodes 80-bit SMPTE LTC frame (LSB first, sync word at end)
- `generateLTCFrameBuffer(audioCtx, tc, fps, ...)` — returns AudioBuffer (Web path)
- `generateLTCFrameSamples(tc, fps, ...)` — returns Float32Array (Tauri path, no AudioContext needed)
- `generateBeepSamples(sampleRate, freq, dur, vol)` — sine wave with envelope (Tauri path)
- `playClapperBeep(audioCtx, dest, vol, freq, dur)` — OscillatorNode (Web path)
- `incrementTimecode(tc, fps, dropFrame)` — SMPTE frame increment with drop-frame rules
- `timecodeToString()`, `timecodeToMillisecondsString()` — formatting
- Performance: cached arrays (`bitsCache`, `rawSamplesCache`), first-order IIR low-pass filter (alpha=0.35)

## Version Management

The **single source of truth** is `package.json`'s `"version"` field. All other files are derived from it.

### How to bump the version
```bash
npm version patch    # 0.1.0 → 0.1.1 (syncs all files, creates git tag v0.1.1)
npm version minor    # 0.1.0 → 0.2.0 (syncs all files, creates git tag v0.2.0)
npm version major    # 0.1.0 → 1.0.0 (syncs all files, creates git tag v1.0.0)
```

The `"version"` npm lifecycle hook runs `scripts/sync-version.js` automatically during `npm version` — after bumping `package.json` but before the git commit and tag. The script propagates the version to:

- `src-tauri/tauri.conf.json`
- `src-tauri-32bit/tauri.conf.json`
- `src-tauri/Cargo.toml`
- `src-tauri-32bit/Cargo.toml`
- `audio-core/Cargo.toml`
- `ltc-gui/Cargo.toml`

After syncing, run `npm install` (to update `package-lock.json`) and `cargo build` in each crate (to update `Cargo.lock` files).

### Manual sync (without bumping)
```bash
npm run version:sync
```

### UI Display
The version is injected at build time via Vite's `define` (`import.meta.env.VITE_APP_VERSION`) and displayed in the app header as `LTC ENGINE v{version}`.

## Build & Run
```bash
npm run dev          # Vite dev server on port 3000
npx tauri dev        # Tauri dev mode (starts Vite + Rust)
npx tauri build      # Production build → src-tauri/target/release/bundle/
npm run lint         # tsc --noEmit
npm run clean        # rm -rf dist src-tauri/target src-tauri-32bit/target
./build-32bit.sh     # Docker cross-compile for i686 (Tauri v1 legacy)
cd ltc-gui && cargo run --release  # Native Rust GUI (egui/eframe)
```

## Current State
- Both Rust backend (`cargo build`) and frontend (`vite build`) compile successfully
- Tauri capabilities still at default `core:default` — may need verification
- Audio output in Tauri mode has NOT been tested yet (WebKitGTK compatibility may still cause issues)
- The `isTauriMode` variable is evaluated once at component mount (environment doesn't change at runtime)

## Known Issues / TODOs
- Tauri: the `isTauriMode` branching in `useEffect` for sink re-application (line 119-129) is redundant — both branches do the same thing
- Tauri: `stop_audio_output` drops the stream but doesn't re-init on restart; `startStreaming` calls `initAudio` which calls `initAudioOutput` — this might fail if the old stream isn't fully dropped
- Tauri: `handleReset` in Tauri mode just clears the schedule and resets the timer, but doesn't flush the Rust audio buffer — old frames may still play
- The `applyAudioSink` effect is only used in Web mode but the if/else is identical
- `metadata.json` references `MAJOR_CAPABILITY_SERVER_SIDE_GEMINI_API` but Gemini API is not used in the code