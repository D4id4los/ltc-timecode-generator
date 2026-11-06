# [Plan] Backlog Execution — Umbrella Plan (Offload Correctness, Session Workflow, Transport, Infra)

**Date:** 2026-11-06
**Target Crates:** gui-engine, ltc-gui, audio-core (ltc-slint touched only where snapshot changes force it)
**Branch:** one feature branch per phase (named in each phase plan); double-rebase merge per house style
**Goal:** Execute the 2026-11 backlog in five phases, in dependency order, with the design decisions settled with the owner on 2026-11-06 baked in. This file is the umbrella: it carries the decisions log and full detail for the two small phases (2 and 5); phases 1, 3 and 4 have their own plan files.

## Context & Objectives

Backlog items grouped by concern:

- **A. Offload progress/status correctness** — recurring "100% + green completed while copying" bug, stress-test harness, red "X out of Y" investigation (root cause found: stale `offload.error`, see phase 1).
- **B. Offload tab layout** — card file-lists collapse to ~3 rows when n > 1 card.
- **C. Transport semantics** — Start should resume from the paused timecode until Reset.
- **D. Recording-session workflow** — session concept for LTC generation; default conversion of offloaded session files (+ configurable default LTC channels); per-file selection in the Convert tab.
- **E. GUI polish** — rename Settings tab; hover text for buttons.
- **F. Codec/test infra** — 16-bit extraction for libltc video decode; container×codec ffmpeg support matrix tool.

## Decisions Log (settled with owner, 2026-11-06)

- **Q1 — Session model: full session entity (option b).** Named sessions carrying fps, drop-frame, start/last timecode, dates, and offload destinations. Storage: **JSON, one file per session** under `<config>/ltc-timecode-generator/sessions/` (SQLite rejected: no new C dependency for cross-compile targets, single-writer tiny data, free test isolation via `LTC_CONFIG_HOME`, corruption containment; revisit trigger = sessions growing queryable event history). Revisit trigger recorded in phase 3 plan.
- **Q2 — Default conversion pipelines.** Video: full re-encode, split tracks, **no** trim-to-first-LTC, **set TC metadata from LTC** (current interactive default shape). Audio (TASCAM separate-file-per-track sets): LTC file is the TC source only; non-LTC tracks get converted with metadata + rename; re-encode vs copy is irrelevant to the owner (WAV processing is fast) — use the existing AudioOnly path. Settings storage: **(ii) separate persisted `SessionConversionDefaults`**, independent of the interactive `ConverterUserSettings` (which is hard-reset per recording selection and stays that way). Trigger UX: **button + preflight report**, no auto-run after offload.
- **Q3 — LTC channel defaults: (b) configured default, then auto-scan fallback**, with per-device pinning (a hit pins the channel for sibling files of the same `device_name`). Decode failure after the full strategy: **abort the session conversion with a prominent warning** (missing LTC is a real error in this workflow); manual per-recording conversion is the escape hatch. The audio default is the *file index* carrying LTC in a TASCAM set (today hard-coded: `ltc_file_idx = 0` default, handoff preset `1` at `ltc-gui/src/app.rs:569`); the video default is stream/channel (today `0/0`).
- **Q4 — Pause/resume semantics.** Start resumes from the frozen `current_timecode`; Reset returns to the configured start TC (already works mid-stream). Editing the start-TC steppers while stopped/paused resets the resume position to the (new) start TC. App restart keeps today's behaviour (position = configured start); loading a session overwrites fps/DF/start TC with the session's continuation value.
- **Q5 — Offload stress harness: split.** Default-suite logic tests through a new engine-level copy seam + a local-only real-disk example runner (corpus-sweep tradition: measurement tool, opt-in, JSON output).
- **Q6 — Offload outcomes.** Cancel ends as `Cancelled` (typed, per Error-Handling R2) and cleans up in-flight `.offload_tmp` partial copies; already-completed (atomically renamed) files are kept and stay visible as Done. Partial device failure renders as an **honest partial state in orange** (new warning colour) with a truthful status line ("2/3 devices copied — 1 failed"), never "complete" framing.
- **Q7 — Tooltips: ltc-gui only.** ltc-slint has no tooltip primitive; deferred.
- **Item 9 (red "X out of Y")** — root-caused to stale `offload.error` (only red text in the tab; owner corroborates non-fresh app session). Fix in phase 1.

## Phase Ordering & Dependencies

| # | Phase | Plan file | Depends on |
|---|---|---|---|
| 1 | Offload progress/status correctness + regression net + card-list sizing | `2026-11-06-backlog-phase1-offload-progress-correctness-plan.md` | — |
| 2 | Transport resume + GUI polish (tab rename, tooltips) | inline below | — |
| 3 | Recording-session foundation (entity + storage + per-file convert selection) | `2026-11-06-backlog-phase3-session-foundation-plan.md` | — (uses Q4 semantics from 2 only for load-session behaviour; implementable in parallel) |
| 4 | Session default conversion (+ 16-bit extraction as step 0) | `2026-11-06-backlog-phase4-session-default-conversion-plan.md` | 3 |
| 5 | ffmpeg container×codec matrix tool | inline below | — |

Phases 2 and 5 can interleave anywhere. Execute phases in the order 1 → 2 → 3 → 4, slotting 5 whenever convenient. Each phase: TDD per house methodology, VERSION_LOG entry, feature branch, double-rebase merge.

## Phase 2 (inline) — Transport Resume + GUI Polish

Branch: `feature/transport-gui-polish`. Target crates: gui-engine, ltc-gui.

### 2.1 Play/Pause resume (Q4)

Current bug: `cmd_start_ltc` always restarts from `state.start_timecode` (`gui-engine/src/engine.rs:584`), discarding the frozen position; polling only runs while playing so `current_timecode` survives a stop (`engine.rs:295`).

Changes:
- `StartLtc` starts from `state.current_timecode` instead of `state.start_timecode` (`core.start_ltc(state.current_timecode, …)`; on success do **not** overwrite `current_timecode`).
- Any start-TC mutation while **not playing** (`SetStartTimecode`, all stepper handlers `engine.rs:1372-1392`) additionally sets `current_timecode = start_timecode`. This yields exactly the Q4 semantics with no extra state: after a plain stop, Start resumes; after a stepper edit while stopped, Start begins from the new start TC; fresh app start, position = start TC.
- `Reset` unchanged (already resets live via `reset_ltc`, `audio-core/src/audio_output.rs:406`).
- Steppers stay locked while playing (existing `!is_playing` gating in `ltc-gui/src/widgets/settings.rs:94`).

Tests (engine integration, poll-with-deadline, no fixed sleeps):
- start → let TC advance → stop → start → resumed TC ≥ frozen TC and monotonic from there (floor only, no upper bound — Test Quality Rules).
- stepper edit while stopped → start → begins at the edited start TC.
- reset while playing → TC jumps back to start TC and continues.

### 2.2 Settings tab rename

`Tab::nav_label` wide label `"Signal & Audio Settings"` → `"Settings"` (`ltc-gui/src/app.rs:85-92`). Narrow label and ltc-slint already say "Settings". Refresh any `ui-screenshots.sh` references.

### 2.3 Tooltip sweep (ltc-gui only)

Add `on_hover_text` to all major interactive controls — inventory established 2026-11-06: transport row (start/stop, clap&beep, reset, lock — `app.rs:709-790`), clapper controls (`widgets/clapper.rs`), settings steppers/sliders/chips (`widgets/settings.rs`), converter checkboxes/action buttons (`widgets/converter.rs`), offload actions (`widgets/offload.rs`). Short, English, one line each. No ltc-slint changes (Q7).

### Verification

New engine tests red→green; `bash scripts/test-lint.sh`; `cargo clippy --all-targets`; `cargo fmt --all --check`; `scripts/ui-screenshots.sh` for tab rename + tooltips; full `cargo nextest run --profile ci`.

## Phase 5 (inline) — ffmpeg Container×Codec Support Matrix Tool

Branch: `feature/ffmpeg-matrix-tool`. Target crate: gui-engine (example only).

`gui-engine/examples/ffmpeg-matrix.rs`, modelled on `ltc-corpus-sweep` (local-only measurement tool; exit 0 on completion regardless of results; operational errors only exit non-zero):

1. **Enumerate combinations from the registry at runtime** — never a hand-copied matrix: containers × video codecs from `converter/formats.rs` (per-container compatibility matrix `formats.rs:27-35`), × audio encoders (`formats.rs:6-13`). This keeps the tool correct as the supported set grows (its whole purpose).
2. Per combination: resolve the encoder chain (`video_codecs.rs::resolve_encoder_chain`), generate a ~1 s `testsrc` + sine file through the chain (record **which** encoder served, hardware or software — chain order mirrors production), then ffprobe the result back and verify container/codec/sample format actually landed.
3. Output: aligned human table (combination → serving encoder → probe verdict) + `--json <out>` machine digest with `"schema": "ffmpeg-matrix/v1"`, ffmpeg version, per-combination record. Args: `--json`, `--ffmpeg <path>` override, `--container/--codec` filters.
4. No changes to production code; reuses `subprocess.rs` helpers.

### Verification

Run locally with the system ffmpeg; eyeball table; `jq` the JSON; clippy/fmt on the example.

## Not Touched / Out of Scope (umbrella)

- ltc-slint tooltips (Q7 — deferred; no primitive exists).
- SQLite session storage (decided against; revisit trigger in phase 3 plan).
- Auto-run conversion after offload (button + preflight chosen, Q2).
- Interactive `ConverterUserSettings` persistence — only `SessionConversionDefaults` get persistence (phase 4); making the interactive tab persistent is a separate future decision.
- i686 target, MSRV changes, CI workflow changes.
- Trim-to-first-LTC exposure in the GUI beyond the session-defaults profile (owner's default workflow trims nothing, Q2).

## Verification, Testing & Acceptance Strategy (cross-phase)

- TDD per house methodology (failing test → fix → green) for every behavioural change.
- Any new `JobKind` must register a `progress_contract.rs` scenario (compile-forced) — applies to phase 4.
- Decoder-touching work (phase 4 step 0) re-runs the corpus sweep and diffs before merge.
- GUI-visible changes verified with `scripts/ui-screenshots.sh`.
- Each phase plan's own Verification section is the acceptance authority for that phase.
