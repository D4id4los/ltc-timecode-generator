#!/usr/bin/env python3
"""WP-T7 test-lint guardrail for the LTC timecode generator workspace.

Mechanically enforces the two most recurrent AGENTS.md Test Quality Rules
over *test code* (files under tests/ directories and #[cfg(test)] module
regions of src/ files):

  Rule S (sleep)      — `thread::sleep` used as a test wait mechanism outside
                        a poll-with-deadline context. Exemptions (structural,
                        not per-site): the sleep sits inside a spawned worker
                        closure (`spawn_job` / `.spawn(`), the enclosing test
                        function carries a deadline predicate within +/-20
                        lines, or a `.join()` appears within +/-10 lines
                        (engine-start-then-join pattern).
  Rule C (text-pin)   — `.contains("` applied to an error/message-shaped
                        identifier (err|error|msg|message|status|label|details).
                        Suppression is an inline, reason-bearing comment:
                        `// test-lint: allow(text-pin): <why>` placed on the
                        violation line or anywhere earlier inside the same
                        test function. New suppressions require a decision
                        note (PR description or plan doc) per AGENTS.md.

Known fail-open blind spots (accepted; clippy + review cover the rest):
raw string literals r#"..."# are not masked, `.contains(` on a receiver
variable outside the identifier list, and sleeps spelled without the
`thread::` path. This script is a guardrail floor, not a Rust linter.

Exit codes: 0 clean, 1 violations, 2 internal error.
"""

import argparse
import re
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parent.parent
CRATE_DIRS = ["audio-core", "gui-engine", "ltc-gui", "ltc-slint"]

SLEEP_RE = re.compile(r"\b(?:std::)?thread::sleep\b")
DEADLINE_RE = re.compile(r"Instant::now\(\)|\.elapsed\(\)|\bdeadline\b", re.IGNORECASE)
JOIN_RE = re.compile(r"\.join\(\)")
# A cancel-parked worker spin (`while !x.is_cancelled() { sleep }`) is the
# worker-closure pattern in seam form: the closure parks a background worker
# (spawn_job thread, injected ScanCards seam) until the test cancels it.
CANCEL_PARK_RE = re.compile(r"\bis_cancelled\(\)")
SPAWN_RE = re.compile(r"\bspawn_job\b|\.spawn\(")
FN_RE = re.compile(r"^\s*(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+\w")
CONTAINS_RE = re.compile(r'\b(err|error|msg|message|status|label|details)\w*\s*\.contains\("')
ALLOW_RE = re.compile(r"test-lint:\s*allow\s*\(\s*text-pin\s*\)")

DEADLINE_WINDOW = 20
JOIN_WINDOW = 10
ALLOW_LOOKBACK_LINES = 0  # allow comments are honoured anywhere in the fn


def mask_line(line, state):
    """Blank out string/char literal contents and comments, keeping brace
    structure, so brace-depth tracking is not confused by `"{clip}"` etc.
    `state` is a one-element list holding in-block-comment status."""
    out = []
    i, n = 0, len(line)
    while i < n:
        c = line[i]
        if state[0]:  # inside a /* */ block comment
            j = line.find("*/", i)
            if j == -1:
                return "".join(out)
            state[0] = False
            i = j + 2
            continue
        if c == '"':
            j = i + 1
            while j < n and line[j] != '"':
                if line[j] == "\\":
                    j += 1
                j += 1
            out.append('""')
            i = j + 1
        elif c == "'" and i + 1 < n and (line[i + 1] == "\\" or (i + 2 < n and line[i + 2] == "'")):
            out.append("''")
            i = i + 2 if line[i + 1] != "\\" else i + 3
        elif c == "/" and i + 1 < n and line[i + 1] == "/":
            break
        elif c == "/" and i + 1 < n and line[i + 1] == "*":
            state[0] = True
            i += 2
        else:
            out.append(c)
            i += 1
    return "".join(out)


def test_regions(rel_path, lines):
    """Return [(start, end)] 0-based half-open line ranges of test code."""
    parts = Path(rel_path).parts
    if "tests" in parts:
        return [(0, len(lines))]
    regions = []
    i = 0
    while i < len(lines):
        if "#[cfg(test)]" in lines[i]:
            j = i
            while j < len(lines) and not re.search(r"\bmod\s+\w", lines[j]):
                j += 1
            if j == len(lines):
                break
            open_idx = lines[j].find("{")
            k = j
            depth = 0
            started = False
            while k < len(lines):
                depth += lines[k].count("{") - lines[k].count("}")
                if depth > 0:
                    started = True
                if started and depth <= 0:
                    break
                k += 1
            regions.append((i, min(k + 1, len(lines))))
            i = k + 1
        else:
            i += 1
    return regions


def enclosing_fn(lines, idx):
    """(fn_start, fn_end) line range (0-based, half-open) containing idx."""
    start = 0
    for i in range(idx - 1, -1, -1):
        if FN_RE.search(lines[i]):
            start = i
            break
    for i in range(idx + 1, len(lines)):
        if FN_RE.search(lines[i]):
            return start, i
    return start, len(lines)


def in_spawn_closure(masked_lines, idx):
    """True if line idx sits inside the braces of a spawn call's closure."""
    stack = []
    depth = 0
    for i, line in enumerate(masked_lines):
        if SPAWN_RE.search(line):
            stack.append(depth)
        depth += line.count("{") - line.count("}")
        # pop only when depth drops strictly below the spawn call's own
        # depth: equal depth still means we are inside the call's arguments
        while stack and depth < stack[-1]:
            stack.pop()
        if i == idx:
            return bool(stack)
    return False


def rule_sleep_violations(rel_path, lines, start, end, verbose):
    hits = []
    state = [False]
    masked = [mask_line(l, state) for l in lines]
    for idx in range(start, end):
        if not SLEEP_RE.search(lines[idx]):
            continue
        if in_spawn_closure(masked, idx):
            continue
        fn_start, fn_end = enclosing_fn(lines, idx)
        # Cancel-parked spin exemption: a sleep whose loop condition (or a
        # near neighbor) observes a cancel token is a parked worker, not a
        # wait-for-state mechanism.
        park_lo, park_hi = max(fn_start, idx - 3), min(fn_end, idx + 4)
        if any(CANCEL_PARK_RE.search(lines[i]) for i in range(park_lo, park_hi)):
            continue
        win_lo = max(fn_start, idx - DEADLINE_WINDOW)
        win_hi = min(fn_end, idx + DEADLINE_WINDOW + 1)
        if any(DEADLINE_RE.search(lines[i]) for i in range(win_lo, win_hi)):
            continue
        jlo = max(start, idx - JOIN_WINDOW)
        jhi = min(end, idx + JOIN_WINDOW + 1)
        if any(JOIN_RE.search(lines[i]) for i in range(jlo, jhi)):
            continue
        hits.append((idx, "sleep",
                     "thread::sleep in test code outside a deadline poll, "
                     "worker closure, or join context"))
    return hits


def rule_text_pin_violations(rel_path, lines, start, end, verbose):
    hits = []
    fn_start, fn_end = start, end
    for idx in range(start, end):
        if not CONTAINS_RE.search(lines[idx]):
            continue
        fn_start, fn_end = enclosing_fn(lines, idx)
        # allow comments live inside the fn, or just above it (fn attribute /
        # doc-comment zone, at most a few lines back)
        allow_lo = max(fn_start - 4, 0)
        allowed = any(
            ALLOW_RE.search(lines[i]) for i in range(allow_lo, idx + 1)
        )
        if allowed:
            continue
        hits.append((idx, "text-pin",
                     '.contains(" on an error/message-shaped identifier — '
                     'assert a typed variant/structural fact, or add a '
                     'reasoned `test-lint: allow(text-pin): <why>` comment '
                     'inside the test function'))
    return hits


def scan_file(path):
    rel = path.relative_to(REPO_ROOT).as_posix()
    text = path.read_text(encoding="utf-8", errors="replace")
    lines = text.split("\n")
    hits = []
    for start, end in test_regions(rel, lines):
        hits += rule_sleep_violations(rel, lines, start, end, False)
        hits += rule_text_pin_violations(rel, lines, start, end, False)
    return [(rel, i + 1, rule, msg, lines[i].strip()) for i, rule, msg in hits]


FIXTURES = [
    # (name, expected rule or None, source)
    ("sleep_bare_test", "sleep", """
#[test]
fn t() {
    std::thread::sleep(Duration::from_millis(50));
    assert!(x);
}
"""),
    ("sleep_inside_deadline_loop", None, """
#[test]
fn t() {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if done() { break; }
        std::thread::sleep(Duration::from_millis(10));
    }
}
"""),
    ("sleep_worker_closure", None, """
#[test]
fn t() {
    spawn_job::<JobFinal, _>(&mut sup, spec, |_ctx| {
        std::thread::sleep(Duration::from_millis(100));
        Ok(JobFinal::NoPayload)
    });
}
"""),
    ("sleep_worker_closure_multiline_call", None, """
#[test]
fn t() {
    spawn_job::<JobFinal, _>(
        &mut sup,
        spec,
        move |_ctx| {
            while !cancel.is_cancelled() {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(JobFinal::NoPayload)
        },
    );
}
"""),
    ("sleep_cancel_parked_seam_closure", None, """
#[test]
fn t() {
    let parked = cancel.clone();
    els.scan_cards = Arc::new(move |_c, _p| {
        while !parked.is_cancelled() {
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(Vec::new())
    });
}
"""),
    ("sleep_then_join", None, """
#[test]
fn t() {
    let handle = thread::spawn(move || engine_main(rx));
    std::thread::sleep(Duration::from_millis(150));
    handle.join().expect("engine thread panicked");
}
"""),
    ("production_code_ignored", None, """
fn worker() {
    std::thread::sleep(Duration::from_millis(50));
}
"""),
    ("text_pin_error", "text-pin", """
#[test]
fn t() {
    assert!(err.contains("boom"));
}
"""),
    ("text_pin_allowed", None, """
// test-lint: allow(text-pin): formatter Display contract per WP-T5 plan
#[test]
fn t() {
    let msg = format_blockers(&blockers);
    assert!(msg.contains("select a recording"));
}
"""),
    ("text_pin_in_production_ignored", None, """
fn classify(s: &str) -> bool {
    s.contains("panic")
}
"""),
]


def self_test():
    failures = []
    for name, expected, source in FIXTURES:
        lines = source.split("\n")
        # fixtures exercising production-code exclusion use a src/ path;
        # everything else is treated as an integration-tests file
        rel = "fixture/src/lib.rs" if name == "production_code_ignored" or name == "text_pin_in_production_ignored" else "fixture/tests/mod.rs"
        hits = []
        for start, end in test_regions(rel, lines):
            hits += rule_sleep_violations(rel, lines, start, end, False)
            hits += rule_text_pin_violations(rel, lines, start, end, False)
        rules = {rule for _, rule, _ in hits}
        got = next(iter(rules)) if rules else None
        if got != expected:
            failures.append(f"{name}: expected {expected!r}, got {rules!r}")
    if failures:
        for f in failures:
            print(f"SELF-TEST FAIL: {f}", file=sys.stderr)
        return 1
    print(f"test-lint self-test: {len(FIXTURES)} fixtures OK")
    return 0


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("paths", nargs="*", help="files or dirs to scan (default: the four workspace crates)")
    parser.add_argument("--self-test", action="store_true", help="run embedded fixture tests and exit")
    parser.add_argument("--verbose", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        return self_test()

    roots = [Path(p) for p in args.paths] if args.paths else [REPO_ROOT / d for d in CRATE_DIRS]
    files = sorted(
        p for root in roots
        for p in (root.rglob("*.rs") if root.is_dir() else [root])
        if "target" not in p.parts and p.is_file()
    )
    hits = []
    try:
        for f in files:
            hits += scan_file(f)
    except Exception as exc:  # noqa: BLE001 — a broken run must red, not pass
        print(f"test-lint: internal error: {exc}", file=sys.stderr)
        return 2

    for rel, lineno, rule, msg, src in hits:
        print(f"{rel}:{lineno}: [{rule}] {msg}")
        print(f"    {src}")
    if hits:
        print(f"\ntest-lint: {len(hits)} violation(s) in test code.")
        return 1
    if args.verbose:
        print(f"test-lint: clean ({len(files)} files scanned)")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except KeyboardInterrupt:
        sys.exit(2)
