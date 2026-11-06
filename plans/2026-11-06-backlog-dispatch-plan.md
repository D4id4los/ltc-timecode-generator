# [Plan] Backlog Execution — Agent Dispatch Plan (copy-pasteable prompts)

**Date:** 2026-11-06
**Scope:** Dispatch prompts for implementation + review agents, one pair per phase of the backlog-execution plan.
**Usage:** Dispatch strictly in order Phase 1 → 2 → 3 → 4; Phase 5 may run anytime no other phase is in flight. For each phase: dispatch the **Implementation** agent first, then dispatch the **Review** agent against the resulting feature branch (the review prompt references the branch). Do not start the next phase's implementation before the previous phase's review passed and the branch was merged (double-rebase per AGENTS.md). Decisions are settled — agents must not re-litigate them; all rationale lives in the plan files.

---

## Phase 1 — Offload progress correctness

### Implementation agent

```
You are implementing a work package in the repository /home/viktoria/src/ltc-timecode-generator (Rust workspace).

Mandatory first steps: read AGENTS.md at the repo root, then read plans/2026-11-06-backlog-phase1-offload-progress-correctness-plan.md in full. The plan is authoritative: it lists four verified defects (A–D) with file:line evidence, related UX defects (E–G), the exact step sequence, and the out-of-scope list. Follow the plan's Step-by-Step Implementation Sequence in order — especially: add the engine-level copy seam FIRST and land the failing tests BEFORE any fix (TDD per AGENTS.md methodology).

Create the branch feature/offload-progress-correctness from main and do all work there. Commit incrementally with messages in the house style "<type>(<module>): <short_desc>".

Hard rules:
- Never weaken a test to make it pass; assert typed/structural facts (phases, counts, snapshot fields), never status-line prose (Test Quality Rules; scripts/test-lint.sh enforces parts of this).
- Poll predicates with deadlines; no fixed sleeps; init_test_config() in every engine test.
- Stay inside the plan's scope; anything you find broken outside it gets reported, not fixed.

Before finishing, run and make pass: bash scripts/test-lint.sh, cargo fmt --all --check, cargo clippy --all-targets, cargo nextest run --profile ci. Also run the new stress example once against a temp dir.

Report back: defects fixed, tests added (names), gate results, any deviations from the plan with justification, and anything discovered that needs follow-up.
```

### Review agent

```
You are reviewing a completed work package in /home/viktoria/src/ltc-timecode-generator.

Context: read AGENTS.md, then plans/2026-11-06-backlog-phase1-offload-progress-correctness-plan.md. The implementation under review is the branch feature/offload-progress-correctness (diff against main).

Verify against the plan, with evidence (file:line):
1. All four defects (A–D) plus E–G are actually fixed, and each fix is covered by a test that would have failed before the fix (inspect test content; spot-check by reasoning, and for defect A temporarily verify the test references a second consecutive run).
2. The engine copy seam exists and the new tests go through the engine loop (not just the library seam).
3. Cancel ends as typed Cancelled, cleans .offload_tmp, does not bump the converter handoff version; partial failure renders via snapshot data an honest partial state (orange), with no "complete" framing.
4. Test Quality Rules: no text assertions on human-facing strings, no fixed sleeps, floors not ceilings, no pins of "current behavior".
5. Out-of-scope respected (no changes to copy chunking, verify semantics, ProgressTracker weighting, scan logic).
6. Gates: run bash scripts/test-lint.sh, cargo fmt --all --check, cargo clippy --all-targets, cargo nextest run --profile ci on the branch.

Deliver a verdict per checklist item (pass/fail + evidence) and an overall PASS or FAIL. Do not fix anything — report only.
```

---

## Phase 2 — Transport resume + GUI polish

### Implementation agent

```
You are implementing a work package in /home/viktoria/src/ltc-timecode-generator (Rust workspace).

Mandatory first steps: read AGENTS.md, then read plans/2026-11-06-backlog-execution-plan.md — this phase's full specification is the "Phase 2 (inline)" section (2.1 play/pause resume, 2.2 tab rename, 2.3 tooltip sweep). Follow it exactly; the semantics are settled decisions (Q4 in the decisions log) — do not redesign them.

Key implementation facts from the plan: StartLtc must start from state.current_timecode (engine.rs:584 currently restarts from start_timecode); any start-TC mutation while NOT playing must also set current_timecode := start_timecode (this is the entire Q4 rule — no extra state); Reset stays as-is. Tests are engine-integration, poll-with-deadline, and must assert floors/monotonicity only (no ceilings).

Create branch feature/transport-gui-polish from main. Commits in house style. Tooltips: ltc-gui ONLY (on_hover_text; the plan's 2.3 lists the control inventory locations). Tab rename: the wide nav label in ltc-gui/src/app.rs:85-92.

Before finishing: bash scripts/test-lint.sh, cargo fmt --all --check, cargo clippy --all-targets, cargo nextest run --profile ci, and scripts/ui-screenshots.sh to capture the renamed tab and tooltips.

Report: behaviour changes, tests added, gate + screenshot results, deviations.
```

### Review agent

```
You are reviewing a completed work package in /home/viktoria/src/ltc-timecode-generator.

Context: read AGENTS.md, then the "Phase 2 (inline)" section and "Decisions Log" of plans/2026-11-06-backlog-execution-plan.md. Implementation under review: branch feature/transport-gui-polish (diff vs main).

Verify with evidence:
1. Resume semantics exactly per Q4: Start resumes from frozen current_timecode; start-TC edits while stopped move the resume position to the new start TC; Reset still works mid-play; steppers remain locked while playing. Confirm via the engine tests AND the code path (no hidden extra state that can desync).
2. The wide tab label is now "Settings"; screenshots updated.
3. Tooltip sweep covers the plan's inventory; ltc-gui only; no ltc-slint tooltip additions.
4. No behaviour changes beyond the plan (e.g. beep, device switching, recovery untouched).
5. Test Quality Rules hold for the new tests (no ceilings, no fixed sleeps, no text assertions on human strings).
6. Gates pass on the branch: test-lint.sh, fmt --check, clippy --all-targets, nextest --profile ci.

Deliver per-item pass/fail + evidence and an overall PASS or FAIL. Report only; fix nothing.
```

---

## Phase 3 — Session foundation

### Implementation agent

```
You are implementing a work package in /home/viktoria/src/ltc-timecode-generator (Rust workspace).

Mandatory first steps: read AGENTS.md, then read plans/2026-11-06-backlog-phase3-session-foundation-plan.md in full (schema, lifecycle, persistence triggers, offload link, per-file convert selection). The design is settled — JSON one-file-per-session storage under the config base dir, exactly as specified; do not substitute SQLite or a single monolithic file.

Create branch feature/session-foundation from main. Follow the plan's Step-by-Step Implementation Sequence (store + unit tests first, then continue-TC computation, then engine commands/triggers, then offload link, then per-file selection, then GUI).

Hard rules:
- Engine tests use init_test_config(); poll-with-deadline; no wall-clock assertions (the periodic last-emitted persist must be tick-counter driven and deterministic in tests).
- Errors typed per the Error-Handling Policy (R1/R3); no new Result<_, String> public APIs (R7).
- Per-file selection must keep plan_output_paths as the single enumeration home (subset flows through it; no parallel enumeration).
- Out-of-scope list in the plan is binding (no session rename/delete UI, no clapper-metadata persistence, no ltc-slint full UI).

Gates before finishing: bash scripts/test-lint.sh, cargo fmt --all --check, cargo clippy --all-targets, cargo nextest run --profile ci, scripts/ui-screenshots.sh for the session bar and convert selection.

Report: what landed, tests added, gate results, deviations with justification, follow-ups found.
```

### Review agent

```
You are reviewing a completed work package in /home/viktoria/src/ltc-timecode-generator.

Context: read AGENTS.md, then plans/2026-11-06-backlog-phase3-session-foundation-plan.md. Implementation under review: branch feature/session-foundation (diff vs main).

Verify with evidence:
1. Session storage matches the plan: one JSON file per session under the config base dir resolved via config_base_dir()/LTC_CONFIG_HOME; schema field "ltc-session/v1"; atomic temp+rename writes; corrupt files skipped loudly, not fatal.
2. Lifecycle per plan: Create captures fps/DF/start; Load stops playback, sets start TC = last_emitted + gap (DF-aware), marks active; last_emitted persisted on Stop, Shutdown, and periodically while playing (tick-driven, deterministic under test).
3. Offload link: parent_name defaults to the session id while active; completed offloads append to offload_destinations and persist; no active session preserves today's ISO-date behaviour.
4. Per-file selection: default all-selected; subset flows through plan_output_paths; empty selection yields the NoFilesSelected blocker; per-recording selection resets restore all-selected.
5. The two-day continuation integration test genuinely restarts the engine and asserts the resumed start TC (floor + fps/DF match), without text assertions on human strings.
6. Error-Handling Policy: typed errors at decision points, string only at display boundaries; no new public Result<_, String>.
7. Gates pass on the branch: test-lint.sh, fmt --check, clippy --all-targets, nextest --profile ci.

Deliver per-item pass/fail + evidence and an overall PASS or FAIL. Report only.
```

---

## Phase 4 — Session default conversion

### Implementation agent

```
You are implementing a work package in /home/viktoria/src/ltc-timecode-generator (Rust workspace). It depends on the already-merged phase 3 (session foundation).

Mandatory first steps: read AGENTS.md, then read plans/2026-11-06-backlog-phase4-session-default-conversion-plan.md in full. Follow its Step-by-Step Implementation Sequence exactly — step 0 is the 16-bit extraction precursor (ffprobe.rs build_extract_args gains a sample-format parameter; the real_world_fixtures.rs:122-146 tripwire test is UPDATED to assert libltc success in 16-bit mode — its own comment defines this as the designed flow).

Create branch feature/session-default-conversion from main.

Settled decisions you must implement, not redesign: separate persisted SessionConversionDefaults profile (interactive ConverterUserSettings stays non-persisted); video pipeline = full re-encode + split + drop LTC + TC-from-LTC + NO trim; audio pipeline = LTC file is TC source only (verify whether it currently also produces an output — if so, exclude it and note it); channel resolution = configured default → auto-scan → per-device pinning; ANY preflight decode failure aborts with a prominent report; encode failure mid-run = honest orange partial, not failure of the whole run.

Hard rules:
- New JobKinds (SessionPreflight, SessionConversion) require exhaustive engine handlers AND registered progress_contract.rs scenarios — the contract suite makes omission a compile error; honour the progress-granularity contract (per-group units, forwarded decode/encode granularity).
- Channel-resolution decision logic must be a pure, table-tested function; I/O shells stay thin.
- Engine tests: init_test_config(), poll-with-deadline, loud ffmpeg skips; real-ffmpeg integration tests follow converter_integration.rs fixture patterns.
- This is a decoder-touching WP: after step 0, run the corpus sweep example against your local corpus (or state clearly it was unavailable) and diff.

Gates before finishing: bash scripts/test-lint.sh, cargo fmt --all --check, cargo clippy --all-targets, cargo nextest run --profile ci, scripts/ui-screenshots.sh for the new Convert-tab sections.

Report: what landed, tests added, corpus-sweep result, deviations, follow-ups.
```

### Review agent

```
You are reviewing a completed work package in /home/viktoria/src/ltc-timecode-generator.

Context: read AGENTS.md, then plans/2026-11-06-backlog-phase4-session-default-conversion-plan.md and the umbrella decisions log (plans/2026-11-06-backlog-execution-plan.md). Implementation under review: branch feature/session-default-conversion (diff vs main).

Verify with evidence:
1. Step 0: extraction sample format is pcm_s16le iff libltc is active; the fixtures tripwire test now asserts libltc success in 16-bit mode (updated, not deleted or skipped); corpus sweep was run and diffed (or its unavailability documented).
2. SessionConversionDefaults persist separately from ConverterUserSettings; codec overrides fall back to select_best_combination() when unset (nothing stale pinned in config).
3. Channel strategy: pure decision core with table tests covering configured/pinned/scan/abort; per-device pinning keyed on device_name; any exhaustion produces the typed NoLtc abort and blocks the run until acknowledged.
4. Orchestration: both new JobKinds have exhaustive handlers and registered progress-contract scenarios with real granularity (sample inside work, not at unit boundaries); decode failures cannot reach the run (preflight-gated); injected encoder failure yields the orange partial outcome.
5. Pipelines match the settled decisions exactly (video: re-encode + split + drop LTC + TC-from-LTC + no trim; audio: LTC file as source only — exclusion handled if needed).
6. Test Quality Rules hold throughout (no text assertions on human strings, loud ffmpeg skips, floors not ceilings).
7. Gates pass on the branch: test-lint.sh, fmt --check, clippy --all-targets, nextest --profile ci.

Deliver per-item pass/fail + evidence and an overall PASS or FAIL. Report only.
```

---

## Phase 5 — ffmpeg matrix tool (anytime; no other phase in flight)

### Implementation agent

```
You are implementing a work package in /home/viktoria/src/ltc-timecode-generator (Rust workspace).

Mandatory first steps: read AGENTS.md, then read the "Phase 5 (inline)" section of plans/2026-11-06-backlog-execution-plan.md. The deliverable is a single new local-only measurement tool: gui-engine/examples/ffmpeg-matrix.rs, modelled on examples/ltc-corpus-sweep.rs (read that example for the house pattern: hand-rolled argv parsing, JSON with a "schema" field, exit 0 on completion regardless of results).

Key requirement: enumerate container × video codec × audio encoder combinations FROM THE REGISTRY AT RUNTIME (converter/formats.rs per-container matrix, VIDEO_CODECS via video_codecs.rs, audio encoder list) — never a hand-copied list; the tool must stay correct as the supported set grows. Per combination: resolve the encoder chain, generate a ~1 s testsrc+sine file through the chain (record which encoder served, hardware or software), ffprobe it back, verify what actually landed. Output an aligned human table plus --json with "schema": "ffmpeg-matrix/v1" and the ffmpeg version. Args: --json, --ffmpeg override, --container/--codec filters.

Create branch feature/ffmpeg-matrix-tool from main. No production-code changes; reuse subprocess.rs helpers. Gates: cargo fmt --all --check, cargo clippy --all-targets, cargo nextest run --profile ci (must be unaffected). Demonstrate a local run (paste the table head into your report).

Report: tool behaviour, example run output, deviations.
```

### Review agent

```
You are reviewing a completed work package in /home/viktoria/src/ltc-timecode-generator.

Context: read AGENTS.md and the "Phase 5 (inline)" section of plans/2026-11-06-backlog-execution-plan.md. Implementation under review: branch feature/ffmpeg-matrix-tool (diff vs main).

Verify with evidence:
1. The combination set is derived from the live registry (formats.rs / video_codecs.rs) — grep for any hand-copied container/codec lists in the example; there must be none.
2. Behaviour matches the corpus-sweep measurement-tool contract: exit 0 on completed runs regardless of results; JSON carries "schema": "ffmpeg-matrix/v1" and the ffmpeg version; operational errors are the only non-zero exits.
3. The serving encoder (incl. hardware vs software position in the chain) and the ffprobe round-trip verdict are both recorded per combination.
4. No production code changed; only the example (and, if needed, non-semantic re-exports — justify any).
5. Gates pass: fmt --check, clippy --all-targets, nextest --profile ci unaffected.

Deliver per-item pass/fail + evidence and an overall PASS or FAIL. Report only.
```

---

## Post-review merge note (for the coordinating human/agent)

After a review PASS: rebase the feature branch on main, fix conflicts, re-run `cargo nextest run --profile ci` if code changed, then rebase main on the feature branch (double-rebase, no merge commits), per AGENTS.md. On a review FAIL: send the review report back to an implementation agent on the same branch; do not merge.
