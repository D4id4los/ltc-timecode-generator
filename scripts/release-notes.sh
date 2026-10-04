#!/bin/bash
# release-notes.sh — extract a VERSION_LOG.org release entry as a GitHub
# release body (markdown).
#
# Usage:
#   release-notes.sh <version-tag> <VERSION_LOG-path>   print entry to stdout
#   release-notes.sh --self-test                        run built-in fixtures
#
# Exit codes: 0 = entry found and printed; 2 = usage error; 3 = missing or
# empty entry (loud failure, no fallback body — project rule).
set -u

usage() {
    echo "usage: $0 <version-tag> <VERSION_LOG-path>" >&2
    echo "       $0 --self-test" >&2
}

# extract_entry <tag> <file> — print the org section for <tag> (from the
# "^* <tag>" heading up to, excluding, the next "^* " heading), trimmed of
# leading/trailing blank lines. Empty output when absent.
extract_entry() {
    awk -v tag="$1" '
        $0 == "* " tag { found = 1; next }
        found && /^\* / { exit }
        found { lines[++n] = $0 }
        END {
            start = 1; while (start <= n && lines[start] ~ /^[ \t]*$/) start++
            end = n;   while (end >= start && lines[end] ~ /^[ \t]*$/) end--
            for (i = start; i <= end; i++) print lines[i]
        }
    ' "$2"
}

_SELF_TEST_TMPDIR=""
cleanup() { [ -n "$_SELF_TEST_TMPDIR" ] && rm -rf "$_SELF_TEST_TMPDIR"; }
trap cleanup EXIT

self_test() {
    tmpdir=$(mktemp -d)
    _SELF_TEST_TMPDIR="$tmpdir"
    local failures=0

    # Fixture: three entries, newest first (first-in-file), plain prose +
    # hyphen bullets (must be markdown-valid as-is), varying blank lines.
    cat > "$tmpdir/log.org" <<'EOF'
#+TITLE: Version History

* v0.6.0

First entry prose.
- bullet one
- bullet two


* v0.5.1

Middle entry prose.

List of Changes:
- fix A
- fix B

* v0.4.7

Trailing entry.
EOF

    # 1. newest (first-in-file) entry, byte-exact incl. trailing-blank trim
    extract_entry v0.6.0 "$tmpdir/log.org" > "$tmpdir/out1" 2>/dev/null
    printf 'First entry prose.\n- bullet one\n- bullet two\n' > "$tmpdir/exp1"
    if ! diff -u "$tmpdir/exp1" "$tmpdir/out1"; then
        echo "FAIL: newest entry extraction" >&2
        failures=$((failures + 1))
    fi

    # 2. middle entry (immediately followed by the next heading)
    extract_entry v0.5.1 "$tmpdir/log.org" > "$tmpdir/out2" 2>/dev/null
    printf 'Middle entry prose.\n\nList of Changes:\n- fix A\n- fix B\n' > "$tmpdir/exp2"
    if ! diff -u "$tmpdir/exp2" "$tmpdir/out2"; then
        echo "FAIL: middle entry extraction" >&2
        failures=$((failures + 1))
    fi

    # 3. missing entry → empty extraction (caller exits non-zero)
    if [ -n "$(extract_entry v9.9.9 "$tmpdir/log.org" 2>/dev/null)" ]; then
        echo "FAIL: missing entry must extract empty" >&2
        failures=$((failures + 1))
    fi

    # 4. missing entry → main mode exits 3 with a loud message
    if "$0" v9.9.9 "$tmpdir/log.org" >/dev/null 2>"$tmpdir/err4"; then
        echo "FAIL: missing entry must exit non-zero" >&2
        failures=$((failures + 1))
    fi

    # 5. missing file → non-zero
    if "$0" v0.6.0 "$tmpdir/nope.org" >/dev/null 2>&1; then
        echo "FAIL: missing file must exit non-zero" >&2
        failures=$((failures + 1))
    fi

    # 6. arg validation → exit 2
    if "$0" >/dev/null 2>&1; then
        echo "FAIL: missing args must exit 2" >&2
        failures=$((failures + 1))
    fi

    if [ "$failures" -ne 0 ]; then
        echo "SELF-TEST: $failures failure(s)" >&2
        return 1
    fi
    echo "SELF-TEST: all fixtures pass"
}

case "${1:-}" in
    --self-test) self_test ;;
    -h|--help) usage ;;
    "")
        usage >&2
        exit 2
        ;;
    *)
        if [ $# -ne 2 ] || [ ! -f "$2" ]; then
            usage >&2
            exit 2
        fi
        body=$(extract_entry "$1" "$2")
        if [ -z "$body" ]; then
            echo "release-notes: no VERSION_LOG.org entry for $1 — refusing to publish an empty release body" >&2
            exit 3
        fi
        printf '%s\n' "$body"
        ;;
esac
