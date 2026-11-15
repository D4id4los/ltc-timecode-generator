# Backlog — Out-of-Scope Improvement Ideas

One bullet per idea, appended at the end. When an idea is picked up, replace
its bullet with a link to the plan/report that owns it. When an idea is
resolved, decided, or closed by a landed plan, move the bullet (with its
resolution note) to `backlog_archive.md` — this file lists open work only;
see AGENTS.md for the norm.

- Offload cancel path still sets `offload.error` to a cancel string rendered in red alongside the new amber cancelled summary — redundant double-signal; drop the error once the GUI surfaces cancel via `run_summary` (Phase 1 implementation report, 2026-11-15, branch `feature/offload-progress-correctness`).
- Offload day-filter default never selects files with unreadable mtimes; the new `selected_day` hint explains it, but a "select all" affordance for such files would help (Phase 1 implementation report, 2026-11-15).
- Offload stress runner's `Succeeded ⇔ bytes on disk` check uses a 64 KiB tolerance; a byte-exact per-file oracle would be tighter (Phase 1 implementation report, 2026-11-15).
- Partial-Succeeded offload run: engine status line reads "Offload complete: N device(s) copied" without distinguishing all-done vs partial — honest distinction currently carried only by `run_summary`/the GUI amber block (Phase 1 review, 2026-11-15, follow-up nit).
- ltc-slint parity for the new honest offload outcome rendering (partial amber / cancelled notice via `run_summary`): snapshot fields exist but ltc-slint does not render them yet (Phase 1 implementation report, 2026-11-15).
- Tooltip capture gap: `scripts/ui-screenshots.sh` cannot capture hover tooltips (no synthetic mouse hover; whole-desktop root-window grab, no app-only crop) — tooltip presence is only verifiable by code sweep; a hover-capable screenshot harness would close this (Phase 2 implementation report, 2026-11-15, branch `feature/transport-gui-polish`).
