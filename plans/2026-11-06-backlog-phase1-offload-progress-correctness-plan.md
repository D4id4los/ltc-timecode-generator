# [Plan] Phase 1 — Offload Progress/Status Correctness + Regression Net

**Date:** 2026-11-06
**Target Crates:** gui-engine (engine, offload, job), ltc-gui (offload tab)
**Branch:** `feature/offload-progress-correctness`
**Goal:** Kill the recurring "100% / green completed while still copying" bug class for good: fix the four identified defects, make cancel and partial-failure outcomes honest (typed Cancelled; orange partial state), add the missing engine-level copy seam, and land the two-layer regression net (default-suite engine tests + local-only real-disk stress runner). Also fixes: stale red error line (backlog item 9) and card file-list collapse with n > 1 cards (item 10).

## Context & Objectives

Four defects verified in source on 2026-11-06:

- **A — Stale terminal status across runs (the primary "100% while copying" bug).** `StartOffload` (`gui-engine/src/engine.rs:2391-2447`) never resets `jobs[OffloadCopy]` and never clears `offload.completed_devices`. `sync_job_statuses` (`engine.rs:343`) refuses to overwrite a terminal phase (Succeeded/Cancelled/Failed) with polled Running snapshots. Result: after the first offload of an app session ends, every later offload renders the previous run's 100% bar, stale green "Completed" list, and the Start button instead of Cancel, for its entire duration. The Conversion job already received the equivalent fix (commit `45bb571`, reset at `engine.rs:2688`) — OffloadCopy never did.
- **B — Fraction hits 1.0 before the last file is durable.** The chunk callback fires after each `write_all` but before `sync_all`, timestamp apply and atomic rename (`gui-engine/src/offload.rs:1129-1143`, forwarded at `offload.rs:1462`), so the bar can read 100% while a large file is still flushing. Precedent fix: converter caps in-flight progress at 0.99 (`converter/process.rs:59`).
- **C — Cancellation swallowed.** `CopyError::Cancelled` → `break` → `Ok(JobFinal::OffloadCopy …)` (`offload.rs:1487`, `:1507`), so `spawn_job` records **Succeeded**, the status says "Offload complete", and the converter handoff version bumps — after a user cancel. The engine's Cancelled branch (`engine.rs:2020-2026`) is unreachable via the runner.
- **D — Partial failure framed as success.** Per-device verify/copy failures mark the unit Failed but the job ends Succeeded with "Offload complete: N device(s) copied" (`offload.rs:1500-1509`, `engine.rs:2048`); the egui tab has no failed-device summary.

Related UX defects (same phase):

- **E — Stale red error line (backlog item 9).** `offload.error` is the only red text in the tab (`ltc-gui/src/widgets/offload.rs:450-456`); it survives from earlier cancels/failed starts (set at `engine.rs:2452`, `:2397-2419`; cleared only at `engine.rs:2320`, `:2338`, `:2423`). Additionally the post-scan default selection is latest-recording-day-only (`offload.rs:1033-1046`) with no explanation, and files with unreadable mtimes are never selected (0/N).
- **F — Card file-list collapse (backlog item 10).** Per-card `ScrollArea::vertical().max_height(240).auto_shrink([false; 2])` (`ltc-gui/src/widgets/offload.rs:183-207`) nested in the tab's outer ScrollArea: with vertical auto-shrink off, each inner area fills *remaining viewport* height — card 1 gets 240 px, later cards bottom out at egui's 64 px clamp ≈ 3 rows.
- **G — GUI symptom-masking.** The `offload_start_pending` 3 s repaint-timeout latch (`ltc-gui/src/app.rs:22`, `:160-180`, `:516-524`) exists only because the engine may never publish a Running status for a new run; it becomes dead weight once A is fixed.

**Why existing tests missed all of this:** `progress_contract.rs` and the offload unit tests exercise one run through the library seam (`run_offload_copy_job_with` + `CopyFileFn`). Every defect above lives above that seam, in the engine's spawn/poll/publish loop — and there is no engine-level copy seam (`StartOffload` hardcodes the real runner at `engine.rs:2444`).

## Proposed Changes (file/type checklist)

- `gui-engine/src/engine.rs`
  - `EngineSeams` gains an injectable offload copy runner (mirror the `scan_cards` seam shape, `engine.rs:55-57`); default = real `run_offload_copy_job`.
  - `StartOffload`: reset `jobs[OffloadCopy]` to a fresh running status; clear `offload.completed_devices`, run-summary fields, `device_totals` before storing the new plan.
  - `on_offload_copy_finished`: Cancelled branch now reachable; composes an honest summary from unit states; no converter-handoff version bump on cancel; Succeeded branch distinguishes all-done vs partial.
  - Clear `offload.error` at scan start (and on `RefreshCards`).
- `gui-engine/src/offload.rs`
  - Runner returns `Err(JobError::Cancelled)` when cancelled (Error-Handling R2; completed-before-cancel devices remain visible via their unit states, which the summary counts).
  - In-flight fraction of a device capped below 1.0 until `finish()` (defect B; mechanism local to the runner — exact clamp at implementation, byte-exact display preserved at finish).
  - Selection summary gains day-filter explanation data (which date the default selection covers).
  - `OffloadSnapshot` gains a run-summary structure: per-run completed count, failed count, failed device names.
- `gui-engine/src/job.rs` — only if the clamp needs a `UnitProgress` hook; no semantics changes to `ProgressTracker`.
- `ltc-gui/src/widgets/offload.rs` — orange partial rendering (new `warning_orange` theme colour), failed-device summary block, honest overall status line; day-filter hint under the selection summary; per-card lists: vertical auto-shrink → content-sized with `max_height` kept (defect F).
- `ltc-gui/src/app.rs` — delete the `offload_start_pending` latch and its timeout.
- `gui-engine/tests/integration.rs` — consecutive-run / cancel / partial-failure engine tests via the new seam.
- `gui-engine/examples/offload-progress-stress.rs` — new local-only real-disk stress runner.
- `VERSION_LOG.org` — entry.

## Step-by-Step Implementation Sequence

1. **Seam first.** Add the engine copy seam (fake-able runner), no behaviour change. Wire `StartOffload` through it.
2. **Failing tests (TDD), all through the engine loop with the fake runner, poll-with-deadline:**
   - `offload_second_run_publishes_fresh_progress`: run 1 completes; start run 2 with a fake that stalls mid-copy; assert published `jobs[OffloadCopy]` phase is Running and fraction < 1.0 while stalled, and `completed_devices` was cleared at start. (Catches A.)
   - `offload_cancel_ends_cancelled_and_cleans_up`: cancel mid-file → phase Cancelled, no handoff version bump, no `.offload_tmp` left on disk, completed-before-cancel devices still visible as Done. (Catches C.)
   - `offload_partial_failure_reports_partial_summary`: one device fails verify → outcome data distinguishes partial from full success; failed device name present. (Catches D.)
   - `offload_fraction_stays_below_one_until_durable`: fake reports all bytes written but stalls before returning from the file copy → published fraction < 1.0. (Catches B.)
   - Assert on typed/structural facts (phases, counts, snapshot fields) — **not** on status-line prose (Test Quality Rules: no text assertions).
3. **Fixes:** reset + clear at `StartOffload` (A); `Err(JobError::Cancelled)` mapping (C); run-summary fields + honest engine status composition (D); in-flight clamp (B).
4. **Error-line lifecycle (E):** clear `offload.error` on scan start; add day-filter explanation to the selection summary; render hint in ltc-gui.
5. **GUI:** orange partial state + failed-device summary + honest status line; delete the `offload_start_pending` latch (G); per-card list sizing fix (F).
6. **Stress runner** (`examples/offload-progress-stress.rs`, corpus-sweep tradition — measurement tool, exit 0 on completed scenarios, JSON out): drives the real engine + real copy backend against a user-supplied target directory. Scenarios: many-small-files; few-huge-files; multi-device size skew; source-grows-during-copy; cancel at varying fractions; N consecutive runs. Invariants sampled continuously from the published snapshot (both GUIs render `jobs[kind]` verbatim, so the snapshot **is** the display): published fraction monotonic non-decreasing; fraction < 1.0 while the job is active and destination bytes < total; no terminal phase before all renames done; phase matches outcome; no `.offload_tmp` residue after cancel/finish. Args: `--dest <dir>`, `--scenario <name|--all>`, `--json <out>`, `--files/--size` knobs.
7. VERSION_LOG entry, full local gates.

## Not Touched / Out of Scope

- Copy backend chunk size, verify semantics, timestamp preservation.
- Offload scan logic and `EngineSeams.scan_cards`.
- `ProgressTracker` weighting semantics (`offload_copy_unit_specs` byte weights are correct).
- ltc-slint offload rendering, except where new snapshot fields force compile updates — its partial-state rendering parity is a follow-up.
- Converter handoff behaviour on success (unchanged).

## Verification, Testing & Acceptance Strategy

- All step-2 tests red before fixes, green after; run with `cargo nextest run --profile ci`.
- `bash scripts/test-lint.sh` (text-pin detector will keep status-line assertions honest by construction).
- `cargo clippy --all-targets`, `cargo fmt --all --check`.
- `scripts/ui-screenshots.sh` Offload tab: multi-card layout shows equally sized lists (F), partial-state colouring.
- Stress runner executed locally against a temp dir (and ideally a real card) — scenarios all completed, JSON digest sensible; not part of CI.
- Acceptance: a second offload in the same app session never displays the previous run's completion state; cancel shows Cancelled with clean temps; partial failure shows orange with a truthful summary.
