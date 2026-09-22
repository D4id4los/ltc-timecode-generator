#!/usr/bin/env bash
set -euo pipefail

# ── Config ─────────────────────────────────────────────────────────────
BINARY=""
DURATION=15
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
RESULTS_BASE="$SCRIPT_DIR/results"

# ── Help ───────────────────────────────────────────────────────────────
usage() {
    cat <<EOF
Usage: $(basename "$0") [options]

Performance profiler for ltc-gui.  Runs two phases — idle and LTC-generating
("busy") — collecting pidstat, strace, and perf data for each.

Options:
  -b PATH    Path to ltc-gui binary (auto-detected if omitted)
  -d SECS    Sampling duration per tool (default: 15)
  -h         Show this help and exit
EOF
    exit 0
}

# ── Parse args ─────────────────────────────────────────────────────────
while getopts "b:d:h" opt; do
    case "$opt" in
        b) BINARY="$OPTARG" ;;
        d) DURATION="$OPTARG" ;;
        h) usage ;;
        *) usage ;;
    esac
done

# ── Auto-detect binary ─────────────────────────────────────────────────
if [[ -z "$BINARY" ]]; then
    candidates=(
        "ltc-gui/target/release/ltc-gui"
        "target/release/ltc-gui"
    )
    for c in "${candidates[@]}"; do
        if [[ -x "$SCRIPT_DIR/../$c" ]]; then
            BINARY="$(cd "$SCRIPT_DIR/.." && pwd)/$c"
            break
        fi
    done
fi

if [[ -z "$BINARY" || ! -x "$BINARY" ]]; then
    echo "ERROR: ltc-gui binary not found. Build it first:"
    echo "  cargo build --release -p ltc-gui"
    echo "Or pass the path with -b."
    exit 1
fi

echo "Binary: $BINARY"
echo ""

# ── Preflight tools ────────────────────────────────────────────────────
missing=()
for tool in pidstat strace perf; do
    if ! command -v "$tool" &>/dev/null; then
        missing+=("$tool")
    fi
done
if [[ ${#missing[@]} -gt 0 ]]; then
    echo "ERROR: missing required tools: ${missing[*]}"
    echo "Install them (e.g. sudo apt install sysstat strace linux-tools-common)."
    exit 1
fi

# ── Check for running ltc-gui instance ─────────────────────────────────
if pidof ltc-gui &>/dev/null; then
    echo "ERROR: ltc-gui is already running. Please close it first."
    exit 1
fi

# ── Sudo / root handling ───────────────────────────────────────────────
if [[ $EUID -eq 0 ]]; then
    SUDO=""
else
    SUDO="sudo"
    echo "Caching sudo credentials (you may be prompted)..."
    $SUDO -v
fi
echo ""

# ── Results directory ──────────────────────────────────────────────────
TIMESTAMP=$(date +%Y-%m-%d_%H%M%S)
OUTDIR="$RESULTS_BASE/$TIMESTAMP"
mkdir -p "$OUTDIR/idle" "$OUTDIR/busy"
echo "Results dir: $OUTDIR"
echo ""

# ── System info ────────────────────────────────────────────────────────
SYSINFO="$OUTDIR/system-info.txt"
{
    echo "=== System Info ==="
    echo "Date:                $(date)"
    echo "Binary:              $BINARY"
    echo "Binary version:      $("$BINARY" --version 2>/dev/null || echo "(unknown)")"
    echo ""
    echo "kernel.perf_event_paranoid: $(sysctl -n kernel.perf_event_paranoid 2>/dev/null || echo '(unavailable)')"
    echo "XDG_SESSION_TYPE:    ${XDG_SESSION_TYPE:-}"
    echo "nproc:               $(nproc)"
    echo "uname -r:            $(uname -r)"
    echo ""
    echo "--- lspci VGA ---"
    lspci 2>/dev/null | grep -i vga || echo "(none)"
} > "$SYSINFO"
echo "Wrote $SYSINFO"

# ── Per-phase collection ───────────────────────────────────────────────
collect_phase() {
    local name="$1"       # "idle" or "busy"
    shift
    local extra_args=("$@")
    local out="$OUTDIR/$name"
    local app_log="$out/app-${name}.log"

    echo ""
    echo "═══ Phase: $name ═══"
    echo "Extra args: ${extra_args[*]:-(none)}"

    # Launch the app in background
    RUST_LOG=eframe=trace,egui_winit=debug,egui=debug \
        "$BINARY" "${extra_args[@]}" 2> "$app_log" &
    local APP_PID=$!
    echo "App PID: $APP_PID"

    # Wait for the process to be ready
    sleep 2
    if ! kill -0 "$APP_PID" 2>/dev/null; then
        echo "WARNING: app exited early (check $app_log). Collecting what we can."
        return
    fi

    # Let the app settle before sampling
    sleep 3

    # ── pidstat ─────────────────────────────────────────────────────────
    echo "  pidstat ($DURATION s)..."
    pidstat -t -p "$APP_PID" 1 "$DURATION" > "$out/pidstat-${name}.txt" 2>/dev/null || true

    # ── strace ──────────────────────────────────────────────────────────
    echo "  strace ($DURATION s)..."
    $SUDO strace -c -f -p "$APP_PID" -o "$out/strace-${name}.txt" &
    local STRACE_PID=$!
    sleep "$DURATION"
    $SUDO kill "$STRACE_PID" 2>/dev/null || true
    wait "$STRACE_PID" 2>/dev/null || true

    # ── perf record ─────────────────────────────────────────────────────
    echo "  perf record ($DURATION s)..."
    $SUDO perf record -F 400 -g -p "$APP_PID" -o "$out/perf-${name}.data" -- sleep "$DURATION" 2>&1 || true

    # ── perf report ─────────────────────────────────────────────────────
    echo "  perf report..."
    $SUDO perf report --stdio -i "$out/perf-${name}.data" > "$out/perf-${name}.txt" 2>/dev/null || true

    # ── perf script ─────────────────────────────────────────────────────
    echo "  perf script..."
    $SUDO perf script -i "$out/perf-${name}.data" > "$out/perf-${name}-script.txt" 2>/dev/null || true

    # ── Stop app ───────────────────────────────────────────────────────
    echo "  stopping app..."
    kill "$APP_PID" 2>/dev/null || true
    sleep 1
    kill -0 "$APP_PID" 2>/dev/null && kill -9 "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    echo "  done."
}

# ── Run phases ─────────────────────────────────────────────────────────
collect_phase idle
collect_phase busy --autostart

# ── Summary ────────────────────────────────────────────────────────────
echo ""
echo "═══ Done ═══"
echo "Results in: $OUTDIR"
echo ""
echo "Files produced:"
find "$OUTDIR" -type f | sort
echo ""
echo "Hint: the GUI window appeared during profiling — this is expected."