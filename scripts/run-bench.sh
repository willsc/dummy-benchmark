#!/usr/bin/env bash
# Benchmark driver: runs the pipeline in two scenarios and prints a comparison.
#
#   scenario "iso"   : feedhandler + engine + workers pinned to ISO_CPUS,
#                      mock-exchange on its own CPU, no noise.
#   scenario "noisy" : same pipeline pinned to SHARED_CPUS with N noise threads
#                      pinned to the same SHARED_CPUS — i.e. simulating a host
#                      where the hot path shares cores with background work.
#
# Each binary writes a JSON report. The comparison table is parsed at the end.
#
# Knobs:
#   SECS=15        per-scenario duration in seconds
#   RATE=2000      ticks-per-second per symbol from mock-exchange
#   SYMBOLS=...    symbol list passed to mock-exchange
#   WORKERS=2      number of strategy worker threads in the engine
#   NOISE=4        noise-thread count in the noisy scenario
#   OUT=bench-out  directory under repo root where reports are written
#   ISO_CPUS=...   explicit isolated-CPU list; defaults to /sys isolated
#   SHARED_CPUS=.. explicit shared-CPU list; defaults to "online minus isolated"
#
# Example:
#   ISO_CPUS=4-7 SHARED_CPUS=0-3 WORKERS=2 NOISE=4 SECS=20 scripts/run-bench.sh

set -euo pipefail

SECS="${SECS:-15}"
RATE="${RATE:-2000}"
SYMBOLS="${SYMBOLS:-AAPL,MSFT,GOOG,NVDA,AMZN,META,TSLA,AMD}"
WORKERS="${WORKERS:-2}"
NOISE="${NOISE:-4}"
OUT="${OUT:-bench-out}"

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

read_cpu_file() {
    local path="$1"
    [[ -r "$path" ]] && cat "$path" | tr -d '\n' || true
}

# Expand a Linux CPU list "0-3,7" into space-separated indices.
expand_list() {
    python3 - "$1" <<'PY'
import sys
s = sys.argv[1].strip()
out = []
if s:
    for part in s.split(','):
        part = part.strip()
        if not part:
            continue
        if '-' in part:
            a, b = part.split('-')
            out.extend(range(int(a), int(b) + 1))
        else:
            out.append(int(part))
print(' '.join(str(x) for x in out))
PY
}

# Defaults derived from /sys.
ISO_DEFAULT="$(read_cpu_file /sys/devices/system/cpu/isolated)"
ONLINE_DEFAULT="$(read_cpu_file /sys/devices/system/cpu/online)"

ISO_CPUS="${ISO_CPUS:-$ISO_DEFAULT}"

# If still empty, pick the upper half of online cpus as "would-be-isolated".
if [[ -z "$ISO_CPUS" ]]; then
    ONLINE_ARR=($(expand_list "$ONLINE_DEFAULT"))
    N=${#ONLINE_ARR[@]}
    if (( N < 4 )); then
        echo "[bench] WARNING: only $N online CPUs; results will be noisy." >&2
        ISO_CPUS="$(IFS=,; echo "${ONLINE_ARR[*]}")"
    else
        HALF=$(( N / 2 ))
        ISO_ARR=("${ONLINE_ARR[@]:$HALF}")
        ISO_CPUS="$(IFS=,; echo "${ISO_ARR[*]}")"
    fi
    echo "[bench] /sys reports no isolated CPUs — synthesising ISO_CPUS=$ISO_CPUS"
    echo "[bench] (real isolation requires isolcpus= in the kernel cmdline)"
fi

# Shared = online minus isolated.
if [[ -z "${SHARED_CPUS:-}" ]]; then
    SHARED_CPUS="$(python3 - "$ONLINE_DEFAULT" "$ISO_CPUS" <<'PY'
import sys
def expand(s):
    out=set()
    if not s: return out
    for part in s.split(','):
        part=part.strip()
        if not part: continue
        if '-' in part:
            a,b=part.split('-')
            out.update(range(int(a), int(b)+1))
        else:
            out.add(int(part))
    return out
on = expand(sys.argv[1])
iso = expand(sys.argv[2])
shared = sorted(on - iso)
print(','.join(str(x) for x in shared))
PY
)"
fi

if [[ -z "$SHARED_CPUS" ]]; then
    echo "[bench] ERROR: SHARED_CPUS came out empty (ISO_CPUS=$ISO_CPUS covers all online cpus)." >&2
    exit 1
fi

ISO_ARR=($(expand_list "$ISO_CPUS"))
SHARED_ARR=($(expand_list "$SHARED_CPUS"))

if (( ${#ISO_ARR[@]} < 3 )); then
    echo "[bench] WARNING: ISO_CPUS only has ${#ISO_ARR[@]} cpus; engine workers will overlap." >&2
fi
if (( ${#SHARED_ARR[@]} < 3 )); then
    echo "[bench] WARNING: SHARED_CPUS only has ${#SHARED_ARR[@]} cpus; engine workers will overlap." >&2
fi

# Build per-binary pin lists for each scenario. We need a CPU for the
# mock-exchange (any), one for the feedhandler, one for the engine main thread,
# and a list for the engine's workers.
pick_pinning() {
    local -n arr=$1
    local count=${#arr[@]}
    if (( count == 0 )); then
        echo ""; echo ""; echo ""; echo ""
        return
    fi
    local me_cpu="${arr[0]}"
    local fh_cpu="${arr[$(( 1 % count ))]}"
    local eng_cpu="${arr[$(( 2 % count ))]}"
    # Workers consume CPUs starting at index 3, wrapping if needed.
    local workers_cpus=()
    for ((w=0; w<WORKERS; w++)); do
        workers_cpus+=("${arr[$(( (3 + w) % count ))]}")
    done
    local workers_join
    workers_join="$(IFS=,; echo "${workers_cpus[*]}")"
    echo "$me_cpu"
    echo "$fh_cpu"
    echo "$eng_cpu"
    echo "$workers_join"
}

readarray -t ISO_PIN < <(pick_pinning ISO_ARR)
readarray -t SHARED_PIN < <(pick_pinning SHARED_ARR)

ISO_ME="${ISO_PIN[0]}"
ISO_FH="${ISO_PIN[1]}"
ISO_ENG="${ISO_PIN[2]}"
ISO_WK="${ISO_PIN[3]}"

SH_ME="${SHARED_PIN[0]}"
SH_FH="${SHARED_PIN[1]}"
SH_ENG="${SHARED_PIN[2]}"
SH_WK="${SHARED_PIN[3]}"

echo "[bench] online=$ONLINE_DEFAULT"
echo "[bench] iso   =$ISO_CPUS  -> exchange=$ISO_ME, feedhandler=$ISO_FH, engine=$ISO_ENG, workers=$ISO_WK"
echo "[bench] shared=$SHARED_CPUS -> exchange=$SH_ME, feedhandler=$SH_FH, engine=$SH_ENG, workers=$SH_WK"

echo "[bench] building release binaries..."
cargo build --release --workspace --quiet

mkdir -p "$OUT/iso" "$OUT/noisy"

BIND="127.0.0.1:9101"
BUS_ISO="/tmp/shmbus-iso.bin"
BUS_NOISY="/tmp/shmbus-noisy.bin"

FH="./target/release/feedhandler"
TE="./target/release/trading-engine"
ME="./target/release/mock-exchange"

run_scenario() {
    local name="$1" bus="$2" me_cpu="$3" fh_cpu="$4" eng_cpu="$5" workers_cpus="$6" noise_n="$7"
    local outdir="$OUT/$name"

    rm -f "$bus"

    echo
    echo "================================================================"
    echo "[bench] scenario=$name secs=$SECS rate=$RATE workers=$WORKERS noise=$noise_n"
    echo "================================================================"

    local fh_extra=""
    local eng_extra=""
    local me_extra=""
    if (( noise_n > 0 )); then
        eng_extra="--noise-threads $noise_n --noise-cpus $SHARED_CPUS"
    fi

    "$FH" --bind "$BIND" --bus "$bus" \
        --pin-cpu "$fh_cpu" \
        --bench-secs "$SECS" \
        --bench-report "$outdir/feedhandler.json" \
        --quiet \
        >"$outdir/feedhandler.log" 2>&1 &
    local FH_PID=$!

    sleep 0.3

    # shellcheck disable=SC2086
    "$TE" --bus "$bus" \
        --pin-cpu "$eng_cpu" \
        --workers "$WORKERS" --worker-cpus "$workers_cpus" \
        --bench-secs "$SECS" \
        --bench-report "$outdir/engine.json" \
        --quiet \
        $eng_extra \
        >"$outdir/engine.log" 2>&1 &
    local TE_PID=$!

    sleep 0.2

    "$ME" --target "$BIND" --rate "$RATE" --symbols "$SYMBOLS" \
        --pin-cpu "$me_cpu" \
        --bench-secs "$SECS" \
        >"$outdir/mock-exchange.log" 2>&1 &
    local ME_PID=$!

    # Bound the wait: scenario length + a small drain margin.
    local timeout=$(( SECS + 5 ))
    local elapsed=0
    while (( elapsed < timeout )); do
        if ! kill -0 "$FH_PID" 2>/dev/null && ! kill -0 "$TE_PID" 2>/dev/null; then
            break
        fi
        sleep 1
        elapsed=$(( elapsed + 1 ))
    done

    kill "$ME_PID" "$FH_PID" "$TE_PID" 2>/dev/null || true
    wait "$ME_PID" "$FH_PID" "$TE_PID" 2>/dev/null || true
}

run_scenario "iso"   "$BUS_ISO"   "$ISO_ME"  "$ISO_FH" "$ISO_ENG" "$ISO_WK" 0
run_scenario "noisy" "$BUS_NOISY" "$SH_ME"   "$SH_FH"  "$SH_ENG"  "$SH_WK"  "$NOISE"

echo
echo "================================================================"
echo "[bench] comparison (lower is better for latency, higher for throughput)"
echo "================================================================"

python3 - "$OUT" <<'PY'
import json, sys, os

root = sys.argv[1]

def load(p):
    try:
        with open(p) as f:
            return json.load(f)
    except FileNotFoundError:
        return None

def fmt_ns(ns):
    if ns is None: return "-"
    if ns >= 1_000_000_000: return f"{ns/1e9:.2f}s"
    if ns >= 1_000_000:     return f"{ns/1e6:.2f}ms"
    if ns >= 1_000:         return f"{ns/1e3:.2f}us"
    return f"{ns}ns"

def fmt_n(n):
    if n is None: return "-"
    if n >= 1e9: return f"{n/1e9:.2f}G"
    if n >= 1e6: return f"{n/1e6:.2f}M"
    if n >= 1e3: return f"{n/1e3:.2f}K"
    return f"{n:.0f}"

scenarios = ["iso", "noisy"]
rows = []
for s in scenarios:
    fh = load(os.path.join(root, s, "feedhandler.json"))
    te = load(os.path.join(root, s, "engine.json"))
    if fh and te:
        wm = fh["metrics"]
        em = te["metrics"]
        rows.append({
            "scenario": s,
            "wire_p50":  wm["wire_latency"]["p50_ns"],
            "wire_p99":  wm["wire_latency"]["p99_ns"],
            "wire_p999": wm["wire_latency"]["p999_ns"],
            "shm_p50":   em["shm_transit"]["p50_ns"],
            "shm_p99":   em["shm_transit"]["p99_ns"],
            "e2e_p50":   em["end_to_end"]["p50_ns"],
            "e2e_p99":   em["end_to_end"]["p99_ns"],
            "e2e_p999":  em["end_to_end"]["p999_ns"],
            "ticks":     em["ticks_consumed"],
            "orders":    em["orders_emitted"],
            "tps":       em["throughput_ticks_per_sec"],
            "cpu":       fh["host"].get("cpu_model", "?"),
            "vendor":    fh["host"].get("cpu_vendor", "?"),
            "iso":       fh["host"].get("isolated_cpus", ""),
            "gov":       fh["host"].get("governor", ""),
        })

if not rows:
    print("[bench] no reports found in", root)
    sys.exit(1)

# Host banner (assume both scenarios ran on same host).
r0 = rows[0]
print(f"host:  {r0['cpu']} ({r0['vendor']})  governor={r0['gov']}  isolated=[{r0['iso']}]")
print()

cols = [
    ("scenario",  "{scenario:>8}"),
    ("wire p50",  "{wire_p50:>8}"),
    ("wire p99",  "{wire_p99:>8}"),
    ("wire p999", "{wire_p999:>9}"),
    ("shm p50",   "{shm_p50:>8}"),
    ("shm p99",   "{shm_p99:>8}"),
    ("e2e p50",   "{e2e_p50:>8}"),
    ("e2e p99",   "{e2e_p99:>8}"),
    ("e2e p999",  "{e2e_p999:>9}"),
    ("ticks",     "{ticks:>8}"),
    ("orders",    "{orders:>7}"),
    ("ticks/s",   "{tps:>8}"),
]

header = "  ".join(f"{c[0]:>8}" if c[0] != "wire p999" and c[0] != "e2e p999" else f"{c[0]:>9}" for c in cols)
print(header)
print("-" * len(header))
for r in rows:
    fmtd = {
        "scenario":  r["scenario"],
        "wire_p50":  fmt_ns(r["wire_p50"]),
        "wire_p99":  fmt_ns(r["wire_p99"]),
        "wire_p999": fmt_ns(r["wire_p999"]),
        "shm_p50":   fmt_ns(r["shm_p50"]),
        "shm_p99":   fmt_ns(r["shm_p99"]),
        "e2e_p50":   fmt_ns(r["e2e_p50"]),
        "e2e_p99":   fmt_ns(r["e2e_p99"]),
        "e2e_p999":  fmt_ns(r["e2e_p999"]),
        "ticks":     fmt_n(r["ticks"]),
        "orders":    fmt_n(r["orders"]),
        "tps":       fmt_n(r["tps"]),
    }
    print("  ".join(fmt.format(**fmtd) for _name, fmt in cols))
print()
print(f"reports: {root}/iso/ and {root}/noisy/")
PY
