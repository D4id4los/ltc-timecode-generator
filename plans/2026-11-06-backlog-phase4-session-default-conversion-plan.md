# [Plan] Phase 4 — Session Default Conversion (16-bit Extraction Precursor, Channel Defaults, Preflight + Orchestration)

**Date:** 2026-11-06
**Target Crates:** gui-engine (decode, ffprobe, converter/, config, engine, command, job), audio-core (decoder input — only via existing seams), ltc-gui (Convert tab)
**Branch:** `feature/session-default-conversion`
**Goal:** One-click "Convert Session": preflight-decode every recording from the active session's offload destinations using configurable LTC channel defaults with auto-scan fallback, show a per-file report, **abort on any decode failure** (Q3), and otherwise run the owner's default pipelines (Q2) over all recordings with honest partial-failure (orange) reporting. Precursor: 16-bit extraction so libltc can serve the bulk video decode fast.

**Depends on:** phase 3 (session entity + destinations). References: `plans/2026-11-06-backlog-phase3-session-foundation-plan.md`.

## Context & Objectives

Owner decisions (2026-11-06, umbrella decisions log):

- **Video pipeline default:** full re-encode, split tracks, **no** trim-to-first-LTC, **set TC metadata from LTC** — the current interactive default shape (`ConverterUserSettings::initial`, `gui-engine/src/state.rs:64-85`).
- **Audio pipeline default (TASCAM separate-file-per-track sets):** the LTC file is the TC source only; non-LTC tracks get converted with metadata + rename. Re-encode vs copy immaterial (WAV is fast) — use the existing AudioOnly path. *Verify during implementation whether the LTC file currently also produces an output; if so, exclude it (it must be a source, not a deliverable).*
- **Settings storage (ii):** a separate persisted `SessionConversionDefaults` profile — the interactive `ConverterUserSettings` keeps its per-recording reset behaviour and is **not** persisted.
- **Trigger:** button + preflight report; never auto-run after offload.
- **Channel resolution (Q3 = b):** configured default → auto-scan remaining candidates → per-device pinning (one hit pins the channel for sibling files sharing the `device_name`). Any file still failing → **abort with a prominent warning** (missing LTC is a real error); manual per-recording conversion remains the escape hatch.

Key code facts: video extraction hard-codes `pcm_s24le` (`gui-engine/src/ffprobe.rs:280`) while libltc whole-file requires 16-bit int (`audio-core/src/ltc_decoder_libltc.rs:51-63`) — pinned intentionally by `gui-engine/tests/real_world_fixtures.rs:122-146` ("If extraction ever gains a 16-bit mode, this test fails"); the audio WAV channel auto-pick is channel-0-unless-silent-then-loudest (`audio-core/src/chunked_decode.rs:146-158`), which fails when location audio occupies channel 1 and LTC sits higher; `ltc_file_idx` defaults to 0 and the offload handoff hard-codes a preset of 1 (`ltc-gui/src/app.rs:569`); there is no convert-all-groups flow today (`StartConversion` converts only the selected group, `engine.rs:2503/2626`, duplicate Starts rejected while running); `JobKind` is a closed enum with exhaustive dispatch and a compile-forced `progress_contract.rs` scenario registration.

## Proposed Changes

### Step 0 — 16-bit extraction for libltc video decode

- `build_extract_args` (`ffprobe.rs:264-286`) gains a sample-format parameter: `pcm_s16le` when the libltc backend is active, `pcm_s24le` otherwise. `decode.rs` passes the backend flag through (it already carries `use_libltc`).
- Update `real_world_fixtures.rs:122-146`: the pinned `UnsupportedBitDepth { bits: 24 }` assertion flips to a libltc-success assertion for the 16-bit mode (this test's own comment defines it as the tripwire for exactly this change — updating it is the designed flow, not a weakening).
- Decoder-touching WP: re-run the corpus sweep and diff before merge.

### `SessionConversionDefaults` (persisted profile)

- New persisted config `<config>/ltc-timecode-generator/session_defaults.json` (schema `"ltc-session-defaults/v1"`, atomic write, `LTC_CONFIG_HOME`-resolved): video `{ container_override?, video_codec_override?, audio_encoder_override?, split_tracks = true, drop_ltc_track = true, set_start_from_ltc = true, embed_camera_metadata = true, naming prefix/suffix templates = engine defaults }`; audio `{ ltc_file_idx (default 1, matching the owner's TASCAM rig) }`; channels `{ audio_ltc_channel_override?, video_ltc_stream = 0, video_ltc_channel = 0 }`. Absent overrides fall back to `select_best_combination()` from live ffmpeg caps — no stale codec pinned in config.
- UI: "Session Conversion Defaults" section in the Convert tab (next to the feature it drives; Settings-tab placement is a one-move alternative if the owner prefers). The hard-coded `SetLtcFileIndex(1)` handoff preset (`app.rs:569`) is replaced by this configured default.

### Channel resolution strategy (pure decision core + thin I/O shell)

- Pure function, table-testable: inputs = file kind, configured defaults, per-device pinned channel (from `device_name.rs` resolution), decode results so far; output = ordered candidate list (pinned → configured → scan) and post-hoc pinning updates.
- Audio WAVs: candidate channels beyond the configured default are decoded by feeding `chunked_decode` an explicit channel selection (extend the seam rather than bypassing it).
- Video: per candidate (stream, channel) extract + decode via the existing path; the *actually used* channel is reported per file.
- Failure after candidate exhaustion → typed `SessionPreflightError::NoLtc { file, tried }`.

### Preflight + orchestration

- **Preflight** (new `JobKind::SessionPreflight`): scan the session's `offload_destinations` → `match_files_all_patterns` grouping → per group: resolve channel strategy → decode → collect `{ group, files, resolved channels, start TC, confidence }` or failure. Progress: one unit per group; per-file decode granularity forwarded (progress contract: granularity/monotonicity/termination scenario registered — compile-forced).
- **Report UI**: per-file table (recording, resolved channel / pinned-from, TC, confidence) in the Convert tab; any failure renders prominently in red with "Fix channel defaults or convert this recording manually" guidance; the Convert-Session button is blocked while a failure is unacknowledged.
- **Run** (new `JobKind::SessionConversion`): iterates preflight-validated groups through the default pipelines (video per Q2; audio per Q2 with LTC-file exclusion if verified necessary); reuses `assemble_converter_settings`-shaped assembly with the defaults profile; output paths via `plan_output_paths` (single enumeration home); collisions handled by existing naming-index recovery. Encode failure mid-run → continue remaining groups, end in an honest **partial** state (orange, phase 1 semantics); decode failures cannot occur post-preflight (gated).
- Engine state: `state.session_conversion` snapshot section (preflight report, run status) following the sole-source-of-truth rules; GUI renders verbatim.

## Step-by-Step Implementation Sequence

1. Step 0 (16-bit extraction) + fixture-test flip + corpus sweep.
2. `SessionConversionDefaults` store + roundtrip tests.
3. Channel-resolution pure core + exhaustive table tests (configured/pinned/scan/abort matrix, incl. drop-frame).
4. Preflight job + report state + contract scenario; integration test: synthetic session folder (TASCAM `_S<n>.wav` set + camera clip fixtures generated with ffmpeg, `converter_integration.rs` precedent) → happy path report; LTC-less file → `NoLtc` abort.
5. Conversion orchestration job + contract scenario; integration: happy path converts both media types end-to-end; injected encoder failure → partial outcome data (no text-pin assertions).
6. GUI: defaults section, preflight report, Convert Session button, orange partial rendering.
7. Replace the `SetLtcFileIndex(1)` hard-coded preset; VERSION_LOG entry.

## Not Touched / Out of Scope

- Interactive `ConverterUserSettings` persistence and its per-recording resets.
- Trim-to-first-LTC in the session profile (owner's workflow trims nothing; the machinery stays available for future exposure).
- Automatic conversion after offload (button + preflight by decision).
- Per-recording overrides of session defaults inside the preflight report (future nicety).
- ltc-slint beyond compile compatibility.

## Verification, Testing & Acceptance Strategy

- Unit: defaults roundtrip; channel-strategy decision table; 16-bit arg construction (ffmpeg args are legitimate string contracts).
- Integration (real ffmpeg, loud skips): synthetic session happy path; `NoLtc` abort; partial encoder failure; naming/collision behaviour across a multi-group session.
- Contract suite: scenarios registered for both new `JobKind`s (compile-forced by the exhaustive table).
- Corpus sweep re-run + diff (step 0 is decoder-touching).
- `bash scripts/test-lint.sh`, clippy, fmt, `--profile ci` nextest; `scripts/ui-screenshots.sh` for the new Convert-tab sections.
- Acceptance: on a real two-card session folder, one click produces a preflight report naming each recording's resolved LTC channel and start TC; a missing-LTC file aborts with a red, specific message; the completed run yields per-Q2 deliverables for every recording with a truthful success/partial summary.
