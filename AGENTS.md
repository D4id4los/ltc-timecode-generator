# LTC Timecode Generator — Project Guide

## Overview
High-precision SMPTE Linear Timecode (LTC) audio signal generator + digital clapper-board for multi-camera video sync. Generates bi-phase mark modulated LTC audio and beep tones, routed to selectable stereo channels. It also **decodes** LTC from WAV/video files (quality reports, trim offsets), **converts/exports** recordings (trim-to-first-LTC, timecode metadata embedding, channel splitting/dropping) via ffmpeg, and **ingests/offloads** card media (detect mounted cards, scan for recordings, copy to organised folders).

Two frontends share a common `audio-core` Rust crate:
1. **ltc-gui** (native Rust egui/eframe app — primary GUI target)
2. **ltc-slint** (Slint-based GUI, alternative frontend)

Both Rust GUIs delegate all audio lifecycle, state management, CLI handling, decoding, and conversion to the shared **`gui-engine`** crate via an event-driven message bus.

> Enumerations (command variants, state fields, dependency lists, per-function inventories) are deliberately **not** duplicated in this file — they change too often. The named source files are the source of truth.

## Development Methodology

- Feature work (TDD): analyse the implementation → create function stubs → write unit tests → run the tests (expect failure) → implement the features (fixing the tests) → run the tests again (expect success).
- Bug fixing (TDD): write a test that catches the bug → run it (expect failure) → fix the bug → run it again (expect success).

## Tech Stack

| Crate / App | Role | UI framework |
|---|---|---|
| **audio-core** | Raw audio engine: LTC/beep generation, cpal output, decoders | — |
| **gui-engine** | Shared engine: owns AudioCore + state, CLI, decoding, conversion | — |
| **ltc-gui** | Native desktop GUI (primary target, weak-GPU tablets) | egui/eframe 0.35 (glow) |
| **ltc-slint** | Alternative desktop GUI | Slint 1.17 |

- **Workspace**: `audio-core`, `gui-engine`, `ltc-gui`, `ltc-slint` (root `Cargo.toml`). Workspace clippy lints: style/correctness/complexity/perf = warn.
- **Dependencies**: source of truth is each crate's `Cargo.toml`. Notable: `audio-core` uses cpal 0.18, `hound` (WAV IO), and `libltc-rs` (bindgen binding → requires system `libltc`, see Build & Run).
- **i686 tablet target**: removed from the release pipeline and CI; kept as a manual local speciality via `Dockerfile.gui-build` (pinned static libltc). Re-introduce only on user demand.

## Project Structure

Crate/directory level only — file-level detail lives in the per-crate sections below and in the source.

```
├── gui-engine/                   # Shared engine crate (lib gui_engine)
│   ├── src/                      # engine.rs (loop, EngineSeams), command.rs, state.rs, job.rs (async jobs),
│   │                             #   cli.rs, decode.rs, offload.rs, converter/ (directory module),
│   │                             #   naming/duration/subprocess/device_name/ffprobe/tagger/... helpers
│   ├── examples/                 # ltc-corpus-sweep (WP-RW field-regression sweep)
│   └── tests/                    # integration, cli_decode, tagger_mp4, converter_integration,
│                                 #   video_extraction, real_world_fixtures
├── ltc-gui/                      # Primary GUI (egui/eframe): app.rs, shadows.rs, widgets/, clap_anim.rs
├── ltc-slint/                    # Alternative GUI (Slint): src/ + ui/ (split per concern)
├── audio-core/                   # Raw audio engine: encoder, decoders, cpal output, WAV readers
├── scripts/                      # bump-version.sh, sonar-gate.sh, sonar-overall-gate.sh, sonar-report.sh,
│                                 #   test-lint.sh, release-notes.sh, build-libltc-static.sh
├── perf-test/perf-test.sh        # Profiling harness (pidstat/strace/perf, idle vs. busy)
├── plans/, reports/              # AI-generated plans & analyses (gitignored; see Planning & Analysis)
├── test-data/                    # Committed fixtures: ltc-real-world-test-20sec.wav,
│                                 #   tmcd-roundtrip-trailing-moov.mp4, ltc-rw-* real-world corpus
├── VERSION_LOG.org               # Changelog (release notes source; newest first)
├── Cargo.toml                    # Workspace root + version SoT ([workspace.package])
├── build-all-rust-targets.sh     # Local release build: linux x64 (glibc 2.31 floor) + windows x64, static libltc
├── Dockerfile.gui-build          # i686 manual tablet-build image (speciality)
└── .github/workflows/            # ci.yml, release.yml, mutants.yml
```

## gui-engine Crate Architecture

The `gui-engine` crate is the **shared engine** for both Rust GUIs. It owns `AudioCore` and all application state, eliminating duplicated code and mutex contention between the two GUI frameworks.

### Data Flow
```
User action → GUI event handler → mpsc::Sender<GuiCommand>
                                       │
                                       ▼
                           Engine Thread (gui_engine::engine::engine_main)
                     (owns AudioCore + AppStateSnapshot)
                                       │
                                       ▼
                    Arc<ArcSwap<AppStateSnapshot>>  (lock-free, always latest)
                                       │
                                       ▼
                    GUI reads state.load() each frame
```

### Async Job Infrastructure (`job.rs`)

All async background tasks (conversion, offload, decode, probes, scans) run through one uniform infrastructure instead of ad-hoc per-task channels and generation counters. Key types: `JobKind`/`JobId`/`JobPhase`/`JobStatus` (published in the snapshot), `ProgressTracker`/`UnitProgress` (weighted per-unit progress + rolling log), `CancelToken`, `SpeedMeter`, `JobSupervisor` (poll/drain/cancel/shutdown), `spawn_job()`. See `job.rs` for the full API.

Invariants:
- `JobItem`/`JobFinal` are **closed payload enums**; the engine dispatcher (`engine.rs::handle_job_event`) matches exhaustively on `JobKind` (no wildcard arm — a new `JobKind` without a handler is a compile error). A payload that does not match its kind is logged and dropped, never silently ignored.
- `spawn_job()` guarantees a `JobEvent::Finished` emission (even on panic, via `catch_unwind`) and captures the `ProgressTracker` log into `JobOutcome`.
- **Stale-result gating**: the engine tracks the active `JobId` per kind; `Finished` events from superseded jobs are discarded.
- Cancellation is the typed `JobError::Cancelled` variant, never an error string (Error-Handling R2).
- **Progress-granularity contract**: a long-running job must advance its fraction *while the work happens*, never only at unit (file/device/step) boundaries. Forward the work primitive's incremental callback — copy chunks, ffmpeg `out_time`, decode chunks — into `UnitProgress` as the work advances; for byte-counted work use `UnitProgress::set_bytes` (the single home of the bytes→fraction mapping). Unit weights must reflect real work shares (exemplar: `offload_copy_unit_specs` byte-weights the offload devices; equal weights are only acceptable for equal-sized units). This contract lives at the **runner layer** and is guarded there: inject the work via the runner's `_with` seam, sample the tracker fraction *inside* the injected fake after every reported increment, and assert strictly-increasing intermediate values — reference pattern `copy_job_reports_byte_granular_progress_during_file_copy` (`offload.rs` tests). Sampling inside the seam is deterministic; polling the tracker from outside the worker is not and must not be used. GUIs render `jobs[kind]` fractions verbatim — no GUI-local progress substitutes.

### Sole Source of Truth — State Ownership

The engine is the **sole source of truth** for all application state. GUIs hold only framework-level state (tab index, popup visibility, toasts, per-widget `EditState` shadow buffers, scroll offsets). All user-configurable options — every converter setting included — are engine-owned via `ConverterUserSettings` in `AppStateSnapshot.converter.settings`.

**Data flow for all mutations:**
1. User interacts with GUI widget → GUI sends a fine-grained `GuiCommand` (one per field)
2. Engine mutates its owned state and applies side-effects (defaults repair, readiness recompute, config persist, channel-map resize, auto-apply from LTC decode, encoder re-selection on container change, prefix prefill on recording select)
3. Next tick: engine publishes the updated `AppStateSnapshot` via `ArcSwap`
4. GUI reads the latest snapshot and re-renders

`GuiCommand` categories: transport, timecode/FPS, audio (device/routing/volume/beep), clapper metadata, theme, logs, LTC decode, converter (fine-grained setter per option), offload, durations, steppers, shutdown — see `command.rs` for the full enum.

### AppStateSnapshot
Published each tick as `Arc<ArcSwap<AppStateSnapshot>>`; GUIs `state.load()` lock-free, no queue management. Field groups (see `state.rs`): generation counter; transport; FPS; audio routing + device state; clapper metadata + clap log + monotonic `clap_seq` clap trigger; theme; per-subsystem status channels; decode state; video probe info; per-clip LTC group results; `ffmpeg_caps` (engine-owned, async); unified job status map (`jobs: HashMap<JobKind, JobStatus>`); offload snapshot. Clap flash/arm animation values are **GUI-local**: the engine only bumps `clap_seq` — ltc-gui samples decay curves in `clap_anim.rs`, ltc-slint animates declaratively off a one-shot `flash-strike` trigger.

### Engine Thread Loop
~25 fps (40 ms ticks) — `engine.rs::engine_main`; loop-carried mutable state lives in the `EngineLoopState` struct (not separate locals):
0. Before the loop: spawn the ffmpeg capability probe (`JobKind::FfmpegCapProbe`).
1. **Drain commands** — non-blocking `try_recv()`; `Shutdown` or channel disconnect exits (after `supervisor.shutdown()` cancels + joins active jobs). Everything else dispatches through the single site `process_command()`: heavy arms → named `cmd_*` handlers, simple converter setters → `apply_simple_converter_setting()`, converter/offload commands → their dedicated handlers.
2. **Poll supervisor + drain events** — progress snapshots published into `state.jobs[kind]`; `JobEvent`s dispatched through `handle_job_event()` per the job.rs invariants.
3. **Poll timecode** — `core.current_timecode()` when playing.
4. **Drain audio events** — injectable via `EngineSeams.audio_events`; each event goes through the pure `recovery_action` decision table to the recovery ladder and is forwarded to the GUI over a one-shot `mpsc<AudioEvent>` mailbox (not snapshot state).
5. **Recompute converter-derived data** on demand (`recompute_converter_derived()`).
6. **Update system time**.
7. **Publish** — only when the snapshot differs; the `PartialEq` compare happens *before* the clone so idle ticks skip the deep clone (pinned by `test_idle_engine_does_not_republish_snapshot`).
8. **Sleep** until next tick.

### Audio Lifecycle
- **Init**: 3 retries with exponential backoff (50 → 100 → 200 ms); permanent errors (permission denied) are not retried, transient (device busy) are.
- **Recovery**: on `StreamDied`/`RecoveryNeeded`, up to 3 recovery cycles (stop → reinit → restart LTC if it was playing). The branch logic is the pure `recovery_action(event, attempts)` decision function (`StreamDead` → hard reset without consuming a soft attempt; soft events → `Attempt`/`Exhausted` vs `MAX_RECOVERY_ATTEMPTS`), unit-tested as a decision table and integration-tested via `EngineSeams.audio_events`; the attempt counter is published as `audio_recovery_attempts` and reset by a successful re-init.
- **Device switching**: stop LTC → stop output → re-init → restart LTC; reverts to the previous device on failure.

### Offload / Card-Ingest Subsystem
`gui-engine/src/offload.rs`; dedicated **Offload** tab in both GUIs.
- **Card detection** is platform-specific (Linux `/proc/mounts` + `/sys/class/block` with udisksctl auto-mount; Windows drive APIs; macOS `/Volumes`); the **scan** walks each card (depth ≤ 6, ≤ 10 000 files) classifying media by extension.
- **Device naming** uses the `device_name.rs` chain (below). **Selection**: all/none/latest-recording-day per card + per-file toggles. **Copy plan**: flat `parent/<ISO-date>/<device>/` layout with `name (2).ext` collision renaming.
- **Execution** via `spawn_job`: 1 MiB chunked streaming, temporary `.offload_tmp` → atomic rename, size-only verification, idempotent resume (destination with matching size is skipped), `CancelToken` cancellation, per-device state machine `Pending`/`Copying`/`Done`/`Failed`/`Skipped`. Card scans run under `catch_unwind`.
- **Config**: parent folder persists via `config::save_offload_parent()`, restored by `seed_snapshot_from_config()`.
- **Converter handoff**: a `last_offload_parent` change makes ltc-gui auto-switch the converter to the fresh offload destination (`SelectFolder` + `SelectRecording(0)`).

### Shared Subprocess Helpers (converter + offload + decoder)
- **`subprocess.rs`** — every ffmpeg/ffprobe/exiftool/udisksctl spawn: concurrent pipe drain (avoids deadlock), timeout-kill, streaming stderr watchdog (30 s of no stderr = `Stalled`); `run_ffmpeg_collect_stderr()` is the single watchdog driver.
- **`duration.rs`** — WAV duration via fast `hound` header parse, all else via ffprobe; group aggregation (`MultiTrackAudio` → max, `VideoClipSequence` → sum); `H:MM:SS` formatting.
- **`naming.rs`** — output-filename template engine: `{filename}`, `{device}`, `{clip}`, `{track}` plus zero-padded `{clip:0Nd}`/`{track:0Nd}` (N=1..9); defines the engine defaults for converter output naming.
- **`device_name.rs`** — resolution chain shared by offload and the converter `{device}` template: XAVC binary sniff → camera metadata (exiftool/ffprobe) → filename pattern (Sony/Canon/Panasonic/GoPro/TASCAM) → volume label → `"unknown"`.

## Converter / Export Subsystem

Turns raw recordings into deliverables: trim each file to its first LTC frame, embed the start timecode as ffmpeg `-timecode` metadata, drop or split the LTC track, remux/encode into the chosen container. Exposed as the ltc-gui **Convert** tab and the ltc-slint converter section.

### Engine Ownership
All converter settings live in `ConverterUserSettings`, owned by the engine; GUIs send fine-grained commands, the engine applies side-effects and republishes. Derived UI data (readiness blockers, collision warning, output preview, encoder chain description) is recomputed by `recompute_converter_derived()` and published for both GUIs. `StartConversion` → `assemble_converter_settings()` → `spawn_job(JobKind::Conversion)`; progress polled each tick via `supervisor.poll()`.

### Pipelines
`ConversionPipeline`: `AudioOnly { generate_synthetic_video }` (multi-track WAV → audio/video outputs), `VideoPassthrough` (camera clips → video outputs), `MetadataOnly` (tag originals in place, rename, extract audio — lightweight preflight only: ffmpeg present, file existence, template validation).

### Encoder Selection & HW Validation
- **Codec-level** (`video_codecs.rs`, single source of truth): `ConverterSettings.video_encoder` stores a codec id (`av1`, `h264`, `h265`, `prores`, `dnxhd`); the concrete ffmpeg encoder is resolved at conversion time from a priority-ordered candidate chain — hardware first (nvenc/qsv/amf/mf/vaapi/vulkan/v4l2m2m), software fallbacks last. **Every codec keeps a software fallback.**
- **Two-stage startup probe** (`hw_cache.rs`): Stage 1 (`FfmpegCapProbe`) publishes caps with **all HW encoders withheld**, except entries vouched for by the pass-result cache (`<config_dir>/ltc-timecode-generator/hw_encoder_cache.json`, keyed on ffmpeg version + encoder list + hw devices; failures never cached; corrupt file = empty cache). Stage 2 (`JobKind::HwValidate`) **always** runs the full 1-frame test-encode validation pass and reconciles — **the cache accelerates visibility, validation always runs** — then republishes and writes passing entries back. Withholding is safe because of the software fallbacks (no readiness blocker appears).
- **Untouched-latch**: if the user has not sent `SetContainer`/`SetVideoCodec`/`SetAudioEncoder` this session, stage-2 arrival re-runs `select_best_combination()` so a newly validated HW encoder can become the default; once any of the three is touched, the engine never auto-flips again.
- **Runtime fallback**: `spawn_conversion()` resolves the codec into an ordered encoder chain; failures are classified as `StepFailure::EncoderInit` (no output produced → retry with the next encoder) or `StepFailure::Fatal`; failed encoders are memoized for the run and the first success is pinned.

### Stream-Copy Mode (`copy_video`, VideoPassthrough only)
Video is remuxed with `-c:v copy` — no encoder chain, no `-r`; muxed audio is `-c:a copy` unless channel filtering forces an audio-only re-encode. `prepare_copy_mode()` derives the output container from the input, snaps each trim offset to the nearest video keyframe at-or-before it (`ffprobe::snap_trim_to_keyframe()` packet scan), and re-anchors the start timecode via `shift_timecode_back()` (DF-aware, inverse of `audio_core::increment_timecode`) so the embedded TC matches the actual first video frame. Sanity checks skip video-codec validation in this mode.

### Planning & Invariants
- `selected_channel_pairs()` is the single home of the channel-map/drop iteration; `preview_output_files`/`output_collision_warning` are projections of `plan_output_paths()` — the one enumeration of produced files for all three pipelines. `plan_video_outputs()` yields an ordered `VideoOutputStep` list, each carrying a `naming_index` for collision-guarded path recovery.
- Trim offsets + per-file `TimecodeMetadata` (start TC, fps, drop-frame) drive the conversion; `find_timecode_at_offset()` maps decode results → per-file start TC; `format_ffmpeg_timecode()` renders HH:MM:SS:FF (or `;FF` drop-frame).
- **Camera metadata**: clip probing runs `camera_meta::probe_camera_info()` alongside ffprobe; the `embed_camera_metadata` toggle (default `true`) gates `-metadata make/model` (or `com.apple.quicktime.*` for MOV) on video outputs and the WAV bext block (`bext_meta.rs`). Originator defaults to `"LTC Timecode Generator"`, origination date falls back to file mtime.
- **`tagger.rs`** tags in place: native O(1) MOV/MP4 tmcd (trailing moov → free + appended tmcd track), WAV with existing bext (patches `time_reference`/`originator`/`origination_date`; undersized bext chunks → ffmpeg remux fallback), ffmpeg stream-copy remux otherwise. `MetadataOnly` orchestrates per file via `run_metadata_only()` (probe → extract → tag/rename → summarize); a `FailureLedger` records every attempted step — a run in which nothing succeeded ends in `mark_failed` with a `--- N STEP(S) FAILED ---` block. Logic-level tests drive the phases through `run_metadata_only_with`'s injectable prober.

**Flow**: select files → group by naming pattern (`file_pattern.rs`) → probe (ffprobe + camera meta) → readiness/blockers (`evaluate_readiness()`/`ConvertBlocker`/`conversion_sanity_check()`) → channel mapping + LTC track handling → trim + TC metadata → `spawn_conversion` (progress + cancel).

## Native Rust GUI (`ltc-gui/`)

egui 0.35 + eframe (glow, vsync off); a thin rendering shell over gui-engine — all audio, decode, and conversion logic lives in the engine thread. Target frontend for weak-GPU tablets.

- **`AppState`** (`app.rs`): `cmd_tx` via a sequence-assigning `send()` sink (matches the engine's `applied_command_seq` ack counter 1:1 — every command must go through it), engine-state ArcSwap handle, theme, toasts, tabs (Clapper / Settings / Convert / Offload).
- **Shadow-state interaction** (`shadows.rs` + `widgets/bound.rs`, shared state machine in `gui-engine/src/edit_state.rs`): every interactive widget renders from a typed `EditState<T>` shadow instead of the snapshot. The wrappers enforce one contract: sync against engine truth (adopt only when unfocused and no unconfirmed send — acked via `applied_command_seq >= sent seq`, 2 s timeout fallback), draw from shadow, on change send through the sink and mark pending, track focus. Engine-initiated setting changes propagate through sync. There is deliberately no three-way merge, no per-field epochs, and no debouncing.
- **Frame loop** (`logic()`): syncs the snapshot, drains the engine's `Receiver<AudioEvent>` into toasts, processes keyboard shortcuts (Space/C/R/L/Ctrl+D), handles repaint scheduling. Watches `offload.last_offload_version` (auto-switch converter to the fresh offload destination) and `clapper.clap_seq` (drives the GUI-local `ClapAnim`, which self-requests ~60 fps repaints and clears at settle conditions).
- **`ids.rs`** — egui ScrollArea id-salt constructors (sibling-widget ID-clash prevention); **`theme.rs`** bridges engine palettes to egui styles; file dialogs via `rfd`.

Threading: the GUI thread never touches AudioCore — lock-free ArcSwap reads only; the engine thread owns AudioCore exclusively (zero mutex contention).

## Slint GUI (`ltc-slint/`)

Same pattern as ltc-gui — thin shell over gui-engine. `main.rs` registers Slint callbacks that send `GuiCommand`s and wires converter option models; `poll.rs` sets up the timer that loads snapshots into Slint properties, pulses the one-shot `flash-strike` clap trigger on `clap_seq` change (decay animated declaratively in `ui/app.slint`'s states block), and drains audio events into toasts. `ui/` is split per concern (app/clapper/clock/converter/offload/settings/status/theme/types/widgets.slint). Uses `rfd` for dialogs and `arboard` for clipboard.

## audio-core Crate

Raw audio engine, split by concern (details in source):
- **`types.rs`** — `Timecode`, `ChannelSel`, `AudioEvent`, `AudioDeviceInfo`, chunked-decode `DecodeConfig`/`DecodeProgress`.
- **`audio_output.rs`** — `AudioCore`: cpal output stream, ring buffers (128K LTC + 32K beep), scheduler thread with the pure `CallbackWatchdog` stall-recovery state machine, event queue, device enumeration/config, error classification (`is_permanent_device_error`), `SAMPLE_RATE_OPTIONS = &[44100, 48000]`.
- **`ltc_encoder.rs`** — 80-bit bi-phase mark frame bits, `increment_timecode()`, stereo frame generation, `generate_ltc_lead_in_stereo()` (clock preamble emitted at playback/render start so a recorder's settle window can't swallow the first frame), dBFS↔UI-volume helpers.
- **`ltc_decoder.rs`** — builtin pure-Rust decoder: strategy ladder, first-coherent-frame alignment, `compute_ltc_quality()` as an orchestrator over pure sub-analyzers.
- **`ltc_decoder_libltc.rs`** — decoder via the `libltc-rs` binding.
- **`decoder.rs`** — `trait LtcDecoder` + `decoder_for(use_libltc)` backend seam (the only `if use_libltc` in the crate).
- **`chunked_decode.rs`** — `plan_chunk_boundaries()` (single home of the chunk-boundary math) → sequential/parallel run over `&dyn LtcDecoder` → merge (offset/dedup/reindex/confidence).
- **`wav_chunk_reader.rs`** — byte-level WAV chunk reading from data-section offsets.
- `lib.rs` is a thin front door (module decls + re-exports). `AudioEvent` variants: StreamError/StreamDied/StreamRecovering/StreamDead/RecoveryNeeded/Underrun/FramesDropped.

## CLI

`gui_engine::cli` (binary `ltc-gui`, shared by both GUIs). Modes: `--list-devices/-l`, `--output-to-file <PATH>` (WAV render), `--headless/-H` (live playback, ctrlc handler), `--decode <PATH>` (WAV or video), otherwise GUI. Flag groups — playback (`--device`, `--start-timecode` default `01:00:00:00`, `--fps` 24/25/29.97/30, `--drop-frame`, `--channel`, `--volume` with per-mode defaults: render −12 dBFS, live −24.1 dBFS, `--sample-rate`, `--duration`, `--autostart`), decode (`--decoder builtin|libltc`, `--decode-fps`, `--audio-stream`/`--audio-channel`, `--single-pass`, `--context-frames`, `--list-timecodes/-t`), misc (`--verbose/-v`, `--debug/-d`) — full list with defaults in `cli.rs::Cli`.

- **Decoding**: builtin (pure Rust, chunked parallel, progress + cancel; default) and libltc (C binding, very fast) produce the same `LtcDetectionResult`. `-v` prints the detailed quality report; `-t` lists all timecodes. Video files are auto-detected by extension: ffprobe lists audio streams, ffmpeg extracts the selected stream/channel, decode proceeds on the extracted audio. GUI decode uses the same engine (background thread, generation-stamped results, cancellation).

## Version Management

Single source of truth: the `version` field under `[workspace.package]` in the root `Cargo.toml`; all four members inherit via `version.workspace = true`; GUIs read `CARGO_PKG_VERSION` at compile time.

```bash
scripts/bump-version.sh patch|minor|major|X.Y.Z [--dry-run]   # commit + tag vX.Y.Z
```

The script refuses a dirty tree, rewrites the workspace version, refreshes member versions in `Cargo.lock` via `cargo update --workspace` (never re-resolves third-party deps), commits as `chore(release): bump version to X.Y.Z`, and tags `vX.Y.Z`.

## Testing

```bash
cargo test                                # unit + integration suites
cargo nextest run --profile ci            # CI retry profile: retries=3, final-status-level=flaky
cargo clippy --all-targets                # lint all workspace crates
cargo fmt --all --check                   # formatting gate (rustfmt defaults; CI-enforced)
bash scripts/test-lint.sh                 # test-lint guardrails (below)
```

Locally, shake out timing flakes with `cargo nextest run --retries 10 -E 'test(...)'`. nextest (`.config/nextest.toml`) runs each test in its own process.

### Corpus sweep (WP-RW)

```bash
cargo run --release -p gui-engine --example ltc-corpus-sweep -- <corpus-root> [--json reports/corpus-sweep/sweep.json] [--decoder builtin|libltc] [--limit N] [--day YYYY-MM-DD]
```

Decodes the local real-world recording corpus (root defaults to `$LTC_YT_TESTS_DIR`; never committed, never required by CI), reports per-file results plus a per-day cross-device timeline-consistency oracle (per-device TC-range envelopes must pairwise overlap; per-file TC span ≈ audio duration). **Measurement only** — exit 0 on a completed sweep; pass/fail assertions live solely in the committed `ltc-rw-*` fixture tests. Re-run and diff after every decoder-touching WP — this is the decoder's field-regression net.

### Flaky-Test Methodology

1. **Predicate waits, not signals** — poll for the *specific postcondition* the test asserts, never coarse signals like `generation > 0`. Canonical pattern: `run_engine()` in `gui-engine/tests/integration.rs`.
2. **Deadlines, not fixed sleeps** — never `thread::sleep` to wait for an async operation; poll a predicate with a deadline (adapts to machine load).
3. **Decay-tolerant tolerances** — snapshots read through the engine may be 1-2 ticks old; prefer synchronous unit tests for fine-grained state checks, keep integration assertions coarse.
4. **No real user config** — engine tests set `XDG_CONFIG_HOME` **and** `LTC_CONFIG_HOME` to a tempdir via `init_test_config()` (Windows ignores `XDG_CONFIG_HOME`; `config::config_base_dir()` honors `LTC_CONFIG_HOME` first — the single resolution point for the converter config path, the `hw_encoder_cache` dir, and portable installs). All test engine spawns must call it.
5. **No real ffmpeg probe** — engine tests use `engine_main_with_probe()` / `engine_main_with_seams()` + `fake_probe()`; the real probe path is exercised by converter integration tests calling `query_ffmpeg_capabilities()` directly.
6. **Loud skips** — tests requiring ffmpeg/ffprobe `eprintln!("--- SKIPPED: ...")` so skips are visible, never silent.
7. **Tooling** — nextest per-process isolation; `--profile ci` for repeat runs.

### Mutation Testing (cargo-mutants)

Audits the test suite itself: mutates production code one change at a time and checks whether some test fails. Config: `.cargo/mutants.toml` (`test_tool = "nextest"`). Scope: **audio-core only** — pure logic, fast suite; gui-engine integration tests spawn real ffmpeg and are far too slow per mutant (phase-2 deferred, see `reports/2026-10-06-quality-tooling-assessment-report.md`).

Run **only sharded** — a monolithic run OOM-crashed desktops and exceeds the hosted-runner cap; mutant order is deterministic, so shards partition identically and merge by concatenation. Three hardening rules, each paid for by a real incident (memory math + merge recipe: `plans/2026-10-06-cargo-mutants-oom-safe-sharded-execution-plan.md`): (1) cgroup caps far below machine RAM — orphaned work trees die by the scope, not the global OOM killer; (2) `TMPDIR` on disk, never tmpfs — each tree copy is unswappable RAM and killed-run trees leak; (3) `-t 120` per-command timeout — caps allocation-bombing loop mutants.

```bash
systemd-run --user --scope -p MemoryHigh=10G -p MemoryMax=14G -p MemorySwapMax=4G \
  env TMPDIR="$HOME/mutants-tmp" \
  cargo mutants -p audio-core -j 3 -t 120 --baseline skip \
    -o reports/mutants/<date>-<label>-shard<i> --shard <i>/6   # 0-based: i in 0..5
```

One shard per sitting (≈ 2.5-3.5 h); merge the `mutants.out/{caught,missed,timeout,unviable}.txt` lists by concatenation + `sort -u`. CI runs the same 6-way matrix weekly (`.github/workflows/mutants.yml`, also `workflow_dispatch`; never per-PR — too slow for review latency).

Triage policy: **every survivor is classified, no exceptions** — (1) missing/weak assertion → write a killing test at the lowest layer (Test Quality Rules apply); (2) equivalent/unobservable mutant → suppress with `#[mutants::skip]` or `exclude_re`, always with an inline reason. `exclude_re` gotchas: entries are unanchored regexes matched against the full mutant name — escape `*`, `+`, `|`, `.` (`||` in a pattern is an empty alternation that matches every mutant; ` * ` never matches); struct-field-delete mutants are not reachable by `exclude_re` at all; never reuse line numbers from a stale `missed.txt` — re-derive via `cargo mutants --list`. (3) Timeouts count as caught, but inspect a sample — a mutant that unbounds a loop can flag a missing cancellation contract.

### Property-Based Testing (proptest)

Dev-dependency of `audio-core` and `gui-engine` (pinned `=1.8.0` for the workspace `rust-version`). Adopted **targetedly**, not globally:

1. **Where properties are the tool**: arbitrary-input string parsers (`naming.rs` templates, native `HH:MM:SS:FF` round-trip) and generative decoder-degradation testing (`ltc_decoder.rs` `mod prop` — all seeds are part of the generated input, so failures shrink and persist deterministically).
2. **Where exhaustive sweeps remain the tool**: finite timecode/chunk domains — sampling cannot beat exhaustive coverage of a small finite space; do not convert these.
3. **Oracle discipline**: independent oracles only; never re-derive expected values with production code; never a "validate agrees with parse" property when `validate_template` delegates to `NameTemplate::parse` (vacuous). The two documented reporting artifacts (head/tail frame loss on lead-in-free signals, corruption-boundary repeat) are explicitly tolerated — wrong values never.
4. **Regressions-file policy**: `*/proptest-regressions/` dirs are **committed when present, never gitignored**; nextest retries replay persisted seeds first so genuine failures stay deterministic. Stale entries are pruned when the input envelope changes, counterexamples recorded in a `reports/` doc.
5. **Case budgets**: 64 for decode properties (≤ 15 s debug runtime — lower `with_cases`, never the floors), default 256 for pure-string properties.
6. **Floor calibration**: degradation floors come from deterministic random-seed sweeps (observed margins minus tolerance) — never equalities, never a single hand-picked seed. A calibrated floor close to observed margins is decoder-headroom signal, not a reason to loosen. A property that finds a genuine bug stays failing per the Test Quality Rules.

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

### Lint policy (2026-10-05)

`clippy::too_many_arguments` is **denied** at workspace level (threshold 7, parity with
Sonar `S107` — change both in lockstep or neither). The early-phase blanket allow is
retired. Fix shape: group cohesive parameters into a context/input struct
(`DecodeCtx`, `SanityCheckInput`, `ExtractSeam`, `AssembleStats` precedent); never a
kitchen-sink bag. A local `#[allow(clippy::too_many_arguments)]` requires an inline
justification comment naming the cohesive reason (same style as
`// test-lint: allow(...)`).

**`rust:S1612` (Sonar) / `redundant_closure_for_method_calls` (clippy): won't-fix.**
The workspace deliberately enables only the style/correctness/complexity/perf clippy
groups; this pedantic rule is cosmetic-only and the closure form (`|e| e.to_str()`) is
preferred for readability. The 48 open S1612 issues were bulk-resolved as Won't Fix in
SonarCloud (2026-10-05); new ones will be resolved the same way. Do not rewrite call
sites to satisfy it.

### Test-Lint Guardrails (CI)

`bash scripts/test-lint.sh` (the first CI step after checkout) fails on the two most
recurrent Test-Quality-Rule violations, over test code only (`tests/` dirs and
`#[cfg(test)]` regions):

- **sleep** — `thread::sleep` used as a test wait mechanism outside a
  poll-with-deadline context. Worker-closure sleeps, deadline-adjacent polls,
  and engine-start-then-join patterns are structurally exempt — prefer fixing
  the *rule* over per-site allows when a new legitimate shape appears.
- **text-pin** — `.contains("` on an error/message-shaped identifier
  (`err|error|msg|message|status|label|details`). Suppress only with an
  inline, reason-bearing `// test-lint: allow(text-pin): <why>` comment
  inside the test function, and only when the text *is* the contract (e.g.
  a formatter's Display output, template/naming tokens) — additions need a
  decision note in the PR.

Run `--self-test` (CI does) to verify the detector itself; exit code 2 means
the lint is broken and must red rather than pass.

### Integration Suites

- `gui-engine/tests/integration.rs` — engine-thread command processing: offload scan/copy via `EngineSeams.scan_cards` (fake card, real tempdir copies), the `SetDevice` bogus-id revert test, and the audio recovery ladder via `EngineSeams.audio_events` (+ `init_output` injection so it behaves identically on deviceless CI and audio-equipped machines).
- `gui-engine/tests/cli_decode.rs` — CLI dispatch via `process_cli_result` (output-to-file, WAV/video decode, error paths).
- `gui-engine/tests/tagger_mp4.rs` — native MP4 tmcd in-place tagging vs the committed fixture (stco-offset regression net + ffprobe round-trip).
- `gui-engine/tests/converter_integration.rs` — conversion pipelines (real ffmpeg).
- `gui-engine/tests/video_extraction.rs` — ffprobe/ffmpeg extraction (real ffmpeg).
- `gui-engine/tests/real_world_fixtures.rs` — committed `ltc-rw-*` fixtures through the real ffprobe→extract→decode pipeline (loud-skips without ffmpeg); WAV fixtures through `decode_wav_core` (routing-only).

`converter/test_fixtures.rs` provides shared fixtures for converter unit tests.

## Error-Handling Policy

Normative rules for error types and `Result<_, String>` (G6, 2026-10; full
inventory and rationale live in the local `reports/2026-10-04-error-boundary-policy-report.md` —
this section deliberately carries no inventories):

- **R1 — Typed at decision points.** Any error the code branches on (retry/fallback classification, cancel-vs-fail, readiness gating, step orchestration) is an enum with matchable variants. Exemplar: `StepFailure` (`converter/process.rs`).
- **R2 — Cancellation is a variant, not an error string.** Every cancellable subsystem carries a `Cancelled` variant whose doc comment states callers must not surface it as an error; typed cancellation never round-trips through string matching. Exemplar: `JobError::Cancelled` (`job.rs`).
- **R3 — `String` at display boundaries is the contract, not a smell.** Snapshot error fields, `JobStatus`/`JobOutcome` error strings, toast messages, and CLI stderr are display surfaces: the last writer stringifies, exactly once, via `Display`; do not re-type or re-parse them (no sentinel-string matching on anything typed underneath). Exemplar: `JobOutcome::Failed { error: String, .. }` (`job.rs`).
- **R4 — Migration style: hand-rolled enum + manual `Display`.** When replacing a former `Result<_, String>` public API, `Display` renders byte-identical legacy strings and the doc comment records the contract; deviations are allowed only for consumer-less error paths and must be stated in the variant's doc comment. Exemplar: `CliError::ListDevices` (`cli.rs`).
- **R5 — `std::error::Error` impl on demand, not by default.** Implement it when the type propagates via `?` into `Box<dyn Error>`/anyhow-like contexts or is public-API facing; `Display` alone suffices for internal match-only types. Existing impls stay. Exemplar: `TagError` (`tagger.rs`).
- **R6 — Leaf-IO `String` errors: tolerated, typed when touched.** Leaf-IO helpers currently returning `Result<_, String>` are standing debt; type them per R1 when their module is next touched — no sweep scheduled, no permanent exemption. Exemplar: `wav_chunk_reader.rs` (wrapped via `map_err(LtcDecodeError::Failed)` in `chunked_decode.rs`).
- **R7 — No new production `Result<_, String>` in public APIs.** New code classifies at the boundary per R1/R3. Enforcement is review discipline, not a lint.
- **R8 — Classification of remaining sites: stay vs type-if-touched.** Display-bound sites (e.g. the `AudioCore` `Result<_, String>` family in `audio_output.rs`) stay; sites inconsistent with their own subsystem's typed siblings (e.g. `tag_mp4_tmcd`/`tag_wav_bext` in `tagger.rs`, `verify_copy` in `offload.rs`) are typed when the module is next touched.
- **R9 — `thiserror`: not adopted.** The house pattern renders contractual hand-tuned `Display` prose (R4), which defeats derived `Display`; the type count is stable. Revisit only if a single future phase adds >5 new error types.

## Planning & Analysis

All AI-generated non-code artifacts live in two local, gitignored parent dirs at the repo root — never dump notes, analyses, or specs into the root or source folders:

- `plans/` — forward-looking implementation specs, work packages, roadmaps. Generated *before* multi-file refactors, complex features, or architectural changes; they give reviewers visibility into agent intent and serve as execution state across context resets.
- `reports/` — backward-looking analysis, benchmark results, test summaries, diagnostics, audits; historical context for future agent sessions. Script-generated subdirs (`reports/sonar/`, `reports/corpus-sweep/`) keep their own names.

**Naming** — strict `YYYY-MM-DD` ISO-8601 date prefix (creation date; lexicographic sort = chronological), then lower-case kebab-case topic: `YYYY-MM-DD-<topic>-plan.md` / `-report.md`; phased plans of one overall plan: `YYYY-MM-DD-<overall>-phase<N>-<name>-plan.md`.

**Plan structure**: `# [Plan] Title` + header (Date / Target Crates / Branch — feature branches, see merge policy below / Goal), then: Context & Objectives; Proposed Changes (file/type checklist); Step-by-Step Implementation Sequence; Not Touched / Out-of-Scope; Verification, Testing & Acceptance Strategy.

**Report structure**: `# [Report] Title` + header (Date / Author-Agent / Scope), then: Executive Summary; Diagnostics & Data Findings (raw data blocks welcome); Architectural Impact; Recommended Action Items; Matters for Further Analysis/Work; Additional Notes / Caveats.

**Handoff discipline**: reference prior artifacts by exact filename (e.g. "Read `plans/2026-10-05-wp-dr-decoder-robustness-improvements-plan.md` and execute steps 1–3") — never let the model re-invent the sequence.

**Commits & branches**: commit style `<type>(<module>): <short_desc>` with a body detailing the changes. Work packages run on feature branches. Merging uses the double rebase style: rebase `feature_x` on `main`, fix conflicts, re-run tests if code changed, then rebase `main` on `feature_x` — no merge commits end up on main.

## CI & SonarQube Cloud

The GitHub CLI (`gh`) is installed and authenticated on the dev machine (`gh auth status` to verify) — use it to triage workflows without leaving the terminal (`gh run list/watch/view --log-failed`, `gh pr checks`). Dispatch: `gh workflow run <file> --ref <branch> [-f <input>=<value>]` — only workflows with a `workflow_dispatch` trigger on the **default branch** are dispatchable, but a dispatched run executes the workflow file **from the selected ref**, so branch-local workflow edits are exercised as written; push the branch first. PR runs already validate branch-local CI changes (the `pull_request` event executes the PR's merge-ref workflow version) — dispatch is for pre-PR validation and rehearsals.

Jobs in `.github/workflows/ci.yml` (push to main + PRs) — the workflow files are the source of truth:

| Job | Purpose |
|---|---|
| `build-test-analyze` | test-lint → rustfmt → clippy (JSON for Sonar **plus** a native `cargo clippy --workspace --all-targets -- -D warnings` gate — clippy findings reach Sonar only as external issues, so warnings red the build directly) → `cargo llvm-cov nextest` + LCOV → gui-engine tests under `TZ=Pacific/Kiritimati` (UTC+14; the chrono `Local` sites in offload/log/timecode never cross a date boundary on TZ=UTC runners otherwise) → Sonar scan (skipped without `SONAR_TOKEN`, so forks work; with the token it passes `-Dsonar.qualitygate.wait=true` and can red CI) → overall-code gate step (below) |
| `windows-cross-check` | `cargo check --target x86_64-pc-windows-gnu` on linux — compile-guards the Windows-only `cfg` blocks on every PR (check-only, no link/tests) |
| `macos-check` | `cargo check` on native macOS — compile-guards the macOS-only `cfg` blocks (check-only guard, not a test job) |
| `windows-tests` | first-class Windows test platform: full nextest suite on the **shipped** target `x86_64-pc-windows-gnu` with ffmpeg installed (converter tests execute instead of loud-skipping); libltc built from a pinned source release in MSYS2, cached by tag; a test-count guard fails suspiciously empty runs ("all skipped" cannot hide behind green) |
| `msrv-check` | `cargo check` on the pinned toolchain enforcing the manifests' `rust-version` (currently **1.92**). If a dep bump breaks MSRV resolution: `cargo update -p <crate> --precise <ver>` pins — never raise the MSRV |
| `advisories` | `cargo-deny check advisories` (RustSec scan only — licenses/bans/sources deliberately unenforced). Ignored advisories each carry a justification + revisit date; triage findings, never blanket-ignore |

`mutants.yml` runs cargo-mutants on audio-core weekly (see Testing → Mutation Testing).

- **Sonar project**: `D4id4los_ltc-timecode-generator` (sonarcloud.io), config in `sonar-project.properties` (sources = the four crates; `*.slint` excluded — no analyzer exists). Official SonarSource Rust analyzer (CI-based only); clippy imported as external issues from `clippy-report.json`, coverage from `lcov.info`.
- **Quality gate**: enforcement runs on SonarCloud's built-in **Sonar way** gate (custom-gate assignment 403s on this org plan). The idempotent `SONAR_TOKEN=<token> scripts/sonar-gate.sh` creates the custom **LTC gate** (new-code: coverage > 80 %, reliability/security/maintainability A; plus overall-code conditions: ratings ≤ A, duplication < 3 %, hotspots 100 % reviewed) and verifies the assigned gate covers the new-code metrics — Sonar way is a strict superset. **Overall-code enforcement is a CI step**, not a gate condition: `scripts/sonar-overall-gate.sh` runs after the scan (main branch, token-gated), queries project measures and reds the job on violations (`--self-test` validates the evaluator against committed fixtures). Overall-code **coverage** is deliberately excluded — overall coverage is dominated by untestable GUI drawing code; new-code coverage at 80 % already forces every change to be tested.
- **Local reports**: `SONAR_TOKEN=<token> scripts/sonar-report.sh` (needs `jq` on the dev machine) pulls gate status, measures, issues, and hotspots into `reports/sonar/` (gitignored) — deterministic reports for agents or diffing.

## Build & Run

**Important:** `libltc-rs` requires the system `libltc` library:

```bash
sudo apt install libltc-dev
export PKG_CONFIG_PATH=/usr/lib/x86_64-linux-gnu/pkgconfig
```

```bash
cargo build                          # Build all Rust crates
cargo test                           # Run all Rust tests
cargo clippy --all-targets           # Lint all workspace crates
cd ltc-gui && cargo run --release    # Native Rust GUI (LIBGL_ALWAYS_SOFTWARE=1 forces software OpenGL)
cd ltc-slint && cargo run --release  # Slint-based GUI
./build-all-rust-targets.sh          # Local release builds: linux x64 + windows x64 (static libltc, glibc 2.31 floor)
```

**Windows x86_64 cross-compile** (Linux host → Windows binary): mingw-w64 linker + prebuilt libltc for Windows — see the `[target.x86_64-pc-windows-gnu]` sections in `README.org` ("Cross Compiling Libltc on Linux for Windows builds") and `.cargo/config.toml` (`BINDGEN_EXTRA_CLANG_ARGS`).

**Optional:** `exiftool` improves camera auto-naming during card scans; the probe gracefully falls back to ffprobe tags or filename patterns without it.

### Release Pipeline

`.github/workflows/release.yml` publishes official releases on tag pushes; job graph `guard → build-linux-x64 + build-windows-x64 → release`, plus a `workflow_dispatch` **dry-run** mode (default true) that rehearses on any branch without publishing.

- **Trigger**: glob prefilter `v[0-9]*.[0-9]*.[0-9]*` + guard enforcing the strict regex `^v[0-9]+\.[0-9]+\.[0-9]+$` (`v1.2.3-rc1`, `v1.2` fail). Do **not** "simplify" the trigger to `v*` — the loose-glob + strict-guard pair is deliberate (GHA tag filters are glob-only; recorded in the YAML).
- **Guard**: asserts `[workspace.package]` version == tag; extracts the `VERSION_LOG.org` entry via `scripts/release-notes.sh` (loud failure on missing entry — no fallback body).
- **Static libltc**: build jobs provision a static-only prefix via `scripts/build-libltc-static.sh` (pinned `LIBLTC_TAG`, cached by tag) — never `apt install libltc-dev` in release jobs; verification fails the release if `ldd` shows `libltc`.
- **glibc 2.31 floor**: linux x64 builds via `cargo zigbuild --target x86_64-unknown-linux-gnu.2.31`; max required `GLIBC_*` symbol version verified ≤ 2.31.
- **Release job**: concatenates `SHA256SUMS`, `gh release create`s with the VERSION_LOG entry as the body; assets: linux tar.xz, `ltc-gui.exe`, `SHA256SUMS`.
- **Hardening**: one-time repo setting — tag-protection rule `v*` restricted to Maintainers/Owners (glob-only, hence the second regex layer in the guard).
- **Forward-only**: no retro-releases for pre-existing tags. Failed smoke test → delete the release, delete the tag, fix forward, re-tag. i686 out of scope.
