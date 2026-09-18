# LTC Timecode Generator — Project Guide

## Overview
High-precision SMPTE Linear Timecode (LTC) audio signal generator + digital clapper-board for multi-camera video sync. Generates bi-phase mark modulated LTC audio and beep tones, routed to selectable stereo channels. Four frontends share a common `audio-core` Rust crate:
1. **Web app** (React + Vite, browser-based)
2. **Tauri v2** desktop (WebKitGTK + Rust backend, being phased out)
3. **ltc-gui** (native Rust egui/eframe app — the target for weak-GPU tablets)
4. **ltc-slint** (experimental Slint-based GUI spike)

## Tech Stack
- **Frontend**: React 19 + TypeScript + Vite 6 + Tailwind CSS 4 + `lucide-react` icons + `motion` (framer-motion) + `@google/genai`
- **Desktop**: Tauri v2 (`@tauri-apps/cli` v2.11.4)
- **Rust Backend**: Tauri v2.11.3, audio-core (path dep), serde/serde_json, tauri-plugin-log 2
- **audio-core** (shared crate): cpal 0.18, keepawake 0.6, ringbuf 0.3, serde, log, thread-priority 0.5; conditional pipewire on 64-bit Linux
- **Native GUI** (ltc-gui): egui 0.35 + eframe (glow backend), chrono 0.4, clap 4, ctrlc 3.4, env_logger 0.11, hound 3.5, direct `audio-core` dep
- **Slint GUI** (ltc-slint): slint 1.17, slint-build 1.17, chrono 0.4, clap 4, ctrlc 3.4, hound 3.5, arboard 3, direct `audio-core` dep
- **Build**: `npm run build` → `dist/`, `npx tauri build` → AppImage/deb/msi, `cargo build` (workspace builds audio-core + ltc-gui + ltc-slint), `cd ltc-gui && cargo build` → native binary
- **32-bit Legacy**: `src-tauri-32bit/` (Tauri v1, Docker cross-compile via `build-32bit.sh`); `Dockerfile.gui-build` + `Dockerfile.i386-build`; `build-all-rust-targets.sh` cross-compiles x64 Linux, x64 Windows, x32 Linux

## Project Structure
```
├── src/                          # Frontend source (web app)
│   ├── main.tsx                  # React entry point
│   ├── App.tsx                   # Main component (1137 lines) — all audio logic, scheduling, UI
│   ├── ltcGenerator.ts           # LTC signal generation, beep generation, timecode math
│   ├── types.ts                  # TypeScript types (Timecode, AudioSettings, etc.)
│   ├── index.css                 # Tailwind CSS
│   ├── components/
│   │   ├── TimecodeSettings.tsx   # Settings panel (frame rate, audio device, routing, volume)
│   │   ├── ClapperSlate.tsx       # Clapper slate UI + log
│   │   ├── FooterStatusBar.tsx    # Status bar (OS, audio devices, wake lock, battery)
│   │   └── ToastContainer.tsx     # Toast notification overlay
│   └── utils/
│       └── audioBackend.ts        # Tauri/Web abstraction layer (device detection, audio output)
├── ltc-gui/                      # Native Rust GUI (egui/eframe) — target for weak-GPU tablets
│   ├── Cargo.toml                # Deps: eframe 0.35 (glow), egui 0.35, audio-core, clap, hound, ...
│   ├── Cross.toml                # Cross-compilation config for i686 targets
│   └── src/
│       ├── main.rs               # eframe::run_native() entry point
│       ├── app.rs                # AppState struct + eframe::App impl + audio integration
│       ├── cli.rs                # CLI args + headless mode + WAV file generation
│       ├── log_buffer.rs         # Capturing in-memory log buffer (ring buffer, 1000 entries)
│       ├── theme.rs              # Dark/light theme colors (matches CSS custom properties)
│       └── widgets/
│           ├── mod.rs            # Shared widget helpers (pill)
│           ├── clock.rs          # Glowing timecode display (large digits + milliseconds)
│           ├── clapper.rs        # Clapper board + arm animation + scene/take/roll + sync log
│           ├── settings.rs       # FPS selector, timecode steppers, device dropdown, routing, sliders
│           └── status.rs         # OS, devices, audio status, system time, LIVE/IDLE indicator
├── ltc-slint/                    # Experimental Slint-based GUI spike
│   ├── Cargo.toml                # Deps: slint 1.17, audio-core, chrono, clap, hound, arboard
│   ├── build.rs                  # slint-build compiler for ui/app.slint
│   ├── src/
│   │   ├── main.rs               # Slint AppWindow + all Rust-side logic (~1300 lines)
│   │   ├── cli.rs                # CLI args + headless mode + WAV generation (shared pattern)
│   │   └── log_buffer.rs         # Capturing in-memory log buffer
│   └── ui/
│       └── app.slint             # Slint UI markup (layout + components)
├── audio-core/                   # Shared Rust audio crate (LTC generation + cpal output + scheduler)
│   ├── Cargo.toml                # Deps: cpal 0.18, keepawake 0.6, ringbuf 0.3, serde, log, thread-priority 0.5
│   └── src/lib.rs                # AudioCore struct, LTC/beep generation, cpal stream, scheduler thread
├── src-tauri/                    # Tauri v2 (main 64-bit) Rust backend
│   ├── Cargo.toml                # Rust deps: tauri 2.11.3, audio-core, serde, serde_json, log, tauri-plugin-log
│   ├── build.rs                  # tauri-build
│   ├── tauri.conf.json           # Window 800x800, resizable, CSP null, bundleMediaFramework
│   ├── capabilities/default.json # core:default permissions
│   ├── icons/                    # App icons (png, ico, icns)
│   ├── gen/                      # Generated Tauri schema files
│   └── src/
│       ├── main.rs               # fn main() → app_lib::run()
│       └── lib.rs                # 11 Tauri commands + Builder setup
├── src-tauri-32bit/              # Legacy Tauri v1 (Docker cross-compile for i686)
│   ├── Cargo.toml                # tauri 1.8, audio-core (path dep)
│   └── src/main.rs               # Tauri v1 entry point
├── Cargo.toml                    # Workspace root: members = [audio-core, ltc-gui, ltc-slint]
├── Cargo.lock                    # Workspace lockfile
├── index.html, vite.config.ts, tsconfig.json
├── package.json                  # Scripts: dev, build, preview, clean, lint (tsc --noEmit)
├── build-32bit.sh                # Docker build script for 32-bit Tauri v1
├── build-all-rust-targets.sh     # Cross-compile x64 Linux, x64 Windows, x32 Linux
├── deploy-to-onedrive.sh         # Copy release binaries to OneDrive share
├── Dockerfile.gui-build          # 32-bit Debian 11 Docker image for ltc-gui cross-compile
├── Dockerfile.i386-build         # Ubuntu 20.04 Docker image for 32-bit Tauri v1 cross-compile
├── metadata.json                 # AI Studio metadata (Gemini capability)
├── README.org                    # Project README
├── .env.example                  # Environment variable template
├── .dockerignore                 # Docker build ignore rules
├── assets/                       # Static assets (screenshots, etc.)
├── logs/                         # Runtime logs
└── ltc-gen-test-output.txt       # LTC test output references
```

## Key Architecture — Dual Audio Backend

The app runs in **two modes**, detected at runtime via `window.__TAURI_INTERNALS__`:

### Tauri Mode (Desktop)
- **Device detection**: Rust `get_audio_devices` command via cpal (returns `{id, name, is_default, formats, channels_min, channels_max, sample_rate_min, sample_rate_max, buffer_min, buffer_max}`)
- **Audio output**: JS sends high-level commands to Rust (`start_ltc_stream`, `play_beep`). Rust's `audio-core` crate handles ALL sample generation internally (LTC + beep) using ringbuf-based scheduling and a dedicated scheduler thread. No audio data is sent over IPC.
- **Rust playback**: cpal output stream with 2-channel config, reads from ringbuf (128K stereo samples LTC, 32K stereo samples beep), writes silence on underrun
- **Timing**: `performance.now()` relative to `tauriStartTimeRef` (JS clock sync); Rust maintains its own timing via scheduler thread for audio generation
- **Sample rate**: 16000 or 48000 Hz (auto-suggested based on CPU cores), buffer size 256

### Web Mode (Browser)
- **Device detection**: `navigator.mediaDevices.enumerateDevices()` + `getUserMedia()` for permission
- **Audio output**: Web Audio API (AudioContext, AudioBufferSourceNode, OscillatorNode, ChannelMergerNode)
- **Timing**: `audioCtx.currentTime`
- **Sample rate**: 16000 or 48000 Hz (auto-suggested based on CPU cores, falls back to default)

## Audio Backend Abstraction (`src/utils/audioBackend.ts`)
- `isTauri()` — checks `window.__TAURI_INTERNALS__`
- `getAudioBackendType()` — returns `'tauri' | 'web'`
- `getAudioDevices()` — calls `get_audio_devices` Rust command OR `enumerateDevices`
- `initAudioOutput(deviceId, sampleRate, bufferSize)` — starts cpal stream, returns actual sample rate
- `stopAudioOutput()` — drops cpal stream
- `startLtcStream(tc, fps, dropFrame, channel, volume)` — starts Rust-side LTC generation
- `stopLtcStream()` — stops Rust-side LTC generation
- `resetLtcStream(tc)` — resets LTC to given timecode
- `getCurrentTimecode()` — polls current timecode from Rust AudioCore
- `playBeep(sampleRate, freq, dur, vol, channel)` — triggers Rust-side beep generation (no audio data over IPC)
- `drainAudioEvents()` — drains event queue (errors, underruns, recovery)
- `requestAudioPermission()` — getUserMedia in web, no-op in Tauri
- `selectAudioOutputNative()` — browser picker, no-op in Tauri
- `audioOutputSinkSupported()` — checks `AudioContext.setSinkId` availability
- `SAMPLE_RATE_OPTIONS` — `[16000, 48000]`
- `suggestSampleRate()` — picks 16000 for <4 CPU cores, else 48000
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
| `init_audio_output` | `device_id`, `sample_rate`, `buffer_size` | `u32` | Creates cpal 2-channel output stream, stores in AppState, returns actual sample rate |
| `start_ltc_stream` | `tc`, `fps`, `drop_frame`, `ltc_channel`, `ltc_volume` | `()` | Starts Rust-side LTC generation via audio-core scheduler |
| `stop_ltc_stream` | none | `()` | Stops Rust-side LTC generation |
| `reset_ltc_stream` | `tc` | `()` | Resets LTC scheduler to given timecode |
| `get_current_timecode` | none | `Timecode` | Polls current timecode from audio-core |
| `push_audio_samples` | `samples: Vec<f32>` | `()` | Legacy: pushes stereo samples to ringbuf |
| `stop_audio_output` | none | `()` | Drops cpal stream and audio state |
| `play_beep` | `sample_rate`, `frequency`, `duration`, `volume`, `channel` | `()` | Generates sine beep in Rust, mixed into output stream |
| `drain_audio_events` | none | `Vec<AudioEvent>` | Drains event queue (errors, underruns, recovery) |
| `get_wake_lock_status` | none | `bool` | Checks if keepawake is active |

**AppState** managed via `tauri::Builder::manage()`: `Mutex<AudioCore>`. The `AudioCore` struct (from `audio-core` crate) encapsulates all audio state: cpal stream handle, ringbuf producer/consumer, scheduler thread join handle, beep generator, wake lock, and event queue.

**Beep mixing**: The audio callback reads from both ringbufs (LTC and beep) within `AudioCore`. Beep samples are generated in Rust with envelope (5ms attack, 20ms release) and channel routing (left/right/both). The callback sums `ltc_val + beep_val` per sample. No audio data is ever sent over IPC — the JS only sends lightweight parameter commands.

## Key App.tsx Functions (branching on `isTauriMode`)
- `initAudio()` — Tauri: calls `initAudioOutput()` + sets `tauriStartTimeRef`; Web: creates AudioContext + mixer graph
- `startStreaming()` — Tauri: `startLtcStream()` high-level command + visual clock via `setInterval` (50ms); Web: `generateLTCFrameBuffer` → `AudioBufferSourceNode` loop (setInterval 50ms, schedules 1.5s ahead)
- `stopStreaming()` — Tauri: `stopLtcStream()`; Web: stop all AudioBufferSourceNodes
- `handleClapTriggered()` — Tauri: `tauriPlayBeep` (lightweight IPC, no audio data); Web: `playClapperBeep` (OscillatorNode)
- `handleReset()` — Tauri: `resetLtcStream()` + reset timer; Web: stop sources + reset nextFrameTime
- `getCurrentTimecode()`, `updateVisualClock()` — both use `isTauriMode ? currentStreamingTcRef (polled) : audioCtx.currentTime`
- `monoToStereo(mono, channel)` — interleaves mono to stereo with L/R/both routing, legacy Tauri path
- Audio event polling via `drainAudioEvents()` → toast notifications (500ms interval)
- Theme toggle via `localStorage` persisted `"theme"` key
- Screen Wake Lock API integration tied to play state

## LTC Generation (`src/ltcGenerator.ts`)
- `getLTCBits(h,m,s,f,dropFrame)` — encodes 80-bit SMPTE LTC frame (LSB first, sync word at end)
- `generateLTCFrameBuffer(audioCtx, tc, fps, ...)` — returns AudioBuffer (Web path)
- `generateLTCFrameSamples(tc, fps, ...)` — returns Float32Array (Tauri legacy path, no AudioContext needed)
- `generateBeepSamples(sampleRate, freq, dur, vol)` — sine wave with envelope (Tauri legacy path)
- `playClapperBeep(audioCtx, dest, vol, freq, dur)` — OscillatorNode (Web path)
- `incrementTimecode(tc, fps, dropFrame)` — SMPTE frame increment with drop-frame rules
- `timecodeToString()`, `timecodeToMillisecondsString()` — formatting
- Performance: cached arrays (`bitsCache`, `rawSamplesCache`), first-order IIR low-pass filter (alpha=0.35)
- Note: In Tauri mode, LTC generation is handled entirely within Rust `audio-core` — the JS-side functions above are used only by the Web Audio path.

## Version Management

The **single source of truth** is `package.json`'s `"version"` field. All other files are derived from it.

### How to bump the version
```bash
npm version patch    # 0.1.0 → 0.1.1 (syncs all files, updates lock files, creates git commit + tag v0.1.1)
npm version minor    # 0.1.0 → 0.2.0 (syncs all files, updates lock files, creates git commit + tag v0.2.0)
npm version major    # 0.1.0 → 1.0.0 (syncs all files, updates lock files, creates git commit + tag v1.0.0)
```

The `"version"` npm lifecycle hook runs `scripts/sync-version.js` automatically during `npm version` — after bumping `package.json` but before the git commit and tag. The script:

1. Propagates the version to all Cargo.toml and tauri.conf.json files
2. Runs `npm install` to update `package-lock.json`
3. Runs `cargo generate-lockfile` in the workspace and each standalone crate to update `Cargo.lock` files

All changes are included in the git commit that `npm version` creates, and a `v{version}` tag is added automatically. No manual post-processing is needed.

### Manual sync (without bumping)
```bash
node scripts/sync-version.js
```

### UI Display
The version is injected at build time via Vite's `define` (`import.meta.env.VITE_APP_VERSION`) and displayed in the app header as `LTC ENGINE v{version}`.

## Build & Run
```bash
npm run dev          # Vite dev server on port 3000
npx tauri dev        # Tauri dev mode (starts Vite + Rust)
npx tauri build      # Production build → src-tauri/target/release/bundle/
npm run lint         # tsc --noEmit
npm run clean        # rm -rf dist src-tauri/target src-tauri-32bit/target ltc-gui/target
./build-32bit.sh     # Docker cross-compile for i686 (Tauri v1 legacy)
cd ltc-gui && cargo run --release  # Native Rust GUI (egui/eframe)
```

