# [Plan] Phase 3 — Recording-Session Foundation (Entity, Storage, Per-File Convert Selection)

**Date:** 2026-11-06
**Target Crates:** gui-engine (config, state, command, engine, offload), ltc-gui (Clapper tab session bar, Convert tab selection)
**Branch:** `feature/session-foundation`
**Goal:** Introduce the recording-session entity (decision Q1 = full entity, JSON storage): named sessions that persist fps/drop-frame/start/last-emitted timecode across days, track their offload destinations, and make "continue tomorrow at last TC + gap" a one-click load. Plus: per-file selection in the Convert tab (backlog item 3). This phase delivers the foundation that phase 4 (session default conversion) consumes.

## Context & Objectives

- Multi-day shoots need timecode continuity: files from day 2 must land after day 1 on the editor timeline. Today nothing about generation persists (`config.rs` stores only `last_input_folder`, `last_offload_parent`, `text_scale_percent`), so the start TC resets to `01:00:00:00` every launch.
- The owner settled (2026-11-06): full session entity; JSON one-file-per-session storage (SQLite rejected — see umbrella decisions log; revisit only if sessions grow queryable event history); restart keeps today's reset-to-default behaviour, and **loading** a session overwrites fps/DF/start TC with the session's continuation value.
- Per-file selection: the Convert tab's unit of conversion is currently the whole `MatchedGroup` (`gui-engine/src/engine.rs:2503`, `:2626`); file "pills" are read-only (`ltc-gui/src/widgets/converter.rs:334-360`). The offload tab already has the command pattern to copy (`SetFileSelected`/`SetAllFilesSelected`, `command.rs:181-185`).

## Proposed Changes

### Session storage (JSON, one file per session)

- Location: `<config_base_dir>/ltc-timecode-generator/sessions/<session-id>.json`, where `config_base_dir()` is the existing single resolution point (`config.rs:24-35`, honors `LTC_CONFIG_HOME`) — engine tests get isolation for free via `init_test_config()`.
- Session id: `YYYY-MM-DD-<kebab-slug>` from the session name (creation date), `-2`/`-3` suffix on collision (mirrors offload collision naming).
- Schema (additive, versioned; unknown fields ignored):

```json
{
  "schema": "ltc-session/v1",
  "id": "2026-11-05-forest-shoot",
  "name": "Forest Shoot",
  "created_date": "2026-11-05",
  "last_active_date": "2026-11-06",
  "fps": 25,
  "drop_frame": false,
  "start_timecode": "01:00:00:00",
  "last_emitted_timecode": "01:47:13:20",
  "continue_gap_seconds": 60,
  "offload_destinations": ["/media/backup/2026-11-05-forest-shoot"]
}
```

- Writer: engine thread only, atomic write (temp + rename, house pattern); errors typed per Error-Handling R1 (`SessionStoreError`), display at the status boundary per R3.
- `config.rs`: `load_sessions()` (startup, skips corrupt files with a loud warning), `save_session(&SessionRecord)`, `delete_session(id)`.

### Engine integration

- `AppStateSnapshot` gains `session: Option<ActiveSessionView>` (id, name, continue-preview TC, last-active date) — small, `PartialEq`-friendly.
- New `GuiCommand::Session(SessionCommand)` variants: `Create { name }`, `Load { id }`, `Close`, `SetContinueGapSeconds(u64)` (per-session, default 60).
- `Create`: records fps/DF/current start TC at creation moment; makes the session active.
- `Load`: stop playback; set fps/drop_frame/start_timecode := `increment_timecode(last_emitted, gap_frames)`; `current_timecode` := same (position follows start while not playing — Q4 rule); mark active; update `last_active_date`.
- Persist `last_emitted_timecode`: on `StopLtc`, on engine `Shutdown`, and periodically while playing with a session active (tick-counter driven, ~60 s; deterministic via a small injectable/overridable constant — no wall-clock assertions in tests).
- Offload link: while a session is active, `offload.parent_name` defaults to the session id (replacing the bare ISO date default, `offload.rs:273-275`); on offload completion with ≥1 device, append the destination to `session.offload_destinations` and persist. No active session → today's behaviour.
- The hard-coded `SetLtcFileIndex(1)` handoff preset (`ltc-gui/src/app.rs:569`) stays for now; phase 4 replaces it with the configured default.

### GUI (ltc-gui)

- Clapper tab session bar: current session name (or "No active session"), New Session (name dialog, slug preview), Continue (session list with saved TC + date), Close Session.
- Continue-preview shows `last_emitted + gap` before loading.
- ltc-slint: minimal compile-compat rendering of the same state (plain label); full parity deferred.

### Per-file selection in the Convert tab

- Engine: per-group selected-set (keyed by file path) in the converter snapshot, default = all files of the group; commands `SetFileSelected { group, path, selected }` and `SetAllGroupFiles { group, selected }`; `apply_recording_selection` resets to all-selected.
- `assemble_converter_settings` builds `input_files` from the selected subset (`engine.rs:2626`); `plan_output_paths` enumeration and `naming_index` derivation adapt to the subset (single home of path enumeration stays intact).
- Selection must be non-empty → new `ConvertBlocker::NoFilesSelected` when everything is deselected.
- GUI: file pills become checkboxes with per-group Select All / None.

## Step-by-Step Implementation Sequence

1. `SessionRecord` + store functions + roundtrip/corruption/collision unit tests (`config.rs`).
2. Continue-TC computation (pure: `last_emitted + gap`, DF-aware via `increment_timecode`) unit tests.
3. Engine commands + snapshot field + persistence triggers (create/load/close, periodic last-emitted persist); integration tests: create → play → stop → engine restart → load → start TC continues at last + gap; gap reconfiguration.
4. Offload link (parent-name default, destinations append) integration tests via the existing scan seam + fake copy runner (phase 1 seam).
5. Per-file selection: engine commands + subset assembly + blocker; unit tests on subset planning, engine test on command routing.
6. GUI: session bar; Convert-tab checkboxes + select all/none.
7. VERSION_LOG entry, full gates.

## Not Touched / Out of Scope

- Any conversion-orchestration or channel-default logic (phase 4).
- Session editing beyond create/load/close/gap (no rename, no notes, no delete UI — delete is store-level only).
- Exporting/copying `session.json` next to the media folder (field-portability nicety — revisit later).
- Clapper metadata (scene/take/roll) persistence — only TC/fps/DF persist for now.
- ltc-slint full session UI.

## Verification, Testing & Acceptance Strategy

- Unit: store roundtrip, corrupt-file skip, id slug/collision, continue-TC table (incl. drop-frame), gap validation.
- Integration (engine, poll-with-deadline, `init_test_config()`): session lifecycle persistence across engine restarts; offload destination capture; per-file selection excludes files from a conversion (fake runner seam) and the empty-selection blocker engages.
- `bash scripts/test-lint.sh`, clippy, fmt, `--profile ci` nextest; `scripts/ui-screenshots.sh` for the session bar + convert selection.
- Acceptance: a two-"day" simulation (engine restart) resumes generation at last TC + 60 s with matching fps/DF; the session's offloads accumulate destinations visible to phase 4; individual files can be excluded from conversion.
