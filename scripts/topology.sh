#!/usr/bin/env bash
# Print CPU / NUMA / cache topology and recommended pin sets for the bench.
#
#   ./scripts/topology.sh           # human-readable summary
#   ./scripts/topology.sh --json    # machine-readable
#
# On AMD Epyc each CCD has its own L3 instance, so cores that share an L3
# (read from /sys/devices/system/cpu/cpuN/cache/index3/shared_cpu_list) form
# a CCD. Keeping the hot path inside a single CCD is the single biggest
# latency lever on Epyc.
#
# On Intel Xeon the L3 is unified per socket; the equivalent latency lever is
# CAT (cache way pinning) via resctrl — set up by scripts/cache-alloc.sh.

set -euo pipefail

MODE="text"
case "${1:-}" in
    --json) MODE="json" ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    "") ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
esac

read_or_empty() { [[ -r "$1" ]] && cat "$1" 2>/dev/null || true; }

VENDOR=$(awk -F: '/^vendor_id/ {print $2; exit}' /proc/cpuinfo | xargs)
MODEL=$(awk -F: '/^model name/ {print $2; exit}' /proc/cpuinfo | xargs)
MICROCODE=$(awk -F: '/^microcode/ {print $2; exit}' /proc/cpuinfo | xargs)
KERNEL=$(uname -r)
NCPU=$(nproc)
ONLINE=$(read_or_empty /sys/devices/system/cpu/online)
ISOLATED=$(read_or_empty /sys/devices/system/cpu/isolated)
GOVERNOR=$(read_or_empty /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor)
SMT_ACTIVE=$(read_or_empty /sys/devices/system/cpu/smt/active)
SMT_CONTROL=$(read_or_empty /sys/devices/system/cpu/smt/control)
CMDLINE=$(read_or_empty /proc/cmdline)

# Build a {core_id -> "tid_list"} mapping by walking each online cpu's
# topology directory. Pinning a thread on cpu N and another on its sibling is
# almost always wrong — they share L1/L2.
declare -A SIBLINGS
for cpu in /sys/devices/system/cpu/cpu[0-9]*; do
    [[ -d "$cpu" ]] || continue
    [[ -r "$cpu/topology/thread_siblings_list" ]] || continue
    sib=$(cat "$cpu/topology/thread_siblings_list")
    SIBLINGS["$sib"]=1
done

# resctrl capability.
RESCTRL_AVAIL=0
RESCTRL_MOUNTED=0
[[ -d /sys/fs/resctrl ]] && RESCTRL_MOUNTED=1
[[ -e /sys/fs/resctrl/schemata ]] && RESCTRL_AVAIL=1
L3_NUM_CLOSIDS=$(read_or_empty /sys/fs/resctrl/info/L3/num_closids)
L3_CBM_MASK=$(read_or_empty /sys/fs/resctrl/info/L3/cbm_mask)
L3_MIN_CBM=$(read_or_empty /sys/fs/resctrl/info/L3/min_cbm_bits)

# Group CPUs by L3 shared_cpu_list. Each unique list is one cache instance
# (== one CCD on AMD, one socket-LLC on Intel).
declare -A L3_GROUPS
for cpu in $(seq 0 $((NCPU-1))); do
    f="/sys/devices/system/cpu/cpu${cpu}/cache/index3/shared_cpu_list"
    [[ -r "$f" ]] || continue
    list=$(cat "$f")
    L3_GROUPS["$list"]=1
done

# NUMA nodes.
declare -A NUMA
if [[ -d /sys/devices/system/node ]]; then
    for nd in /sys/devices/system/node/node*/; do
        [[ -d "$nd" ]] || continue
        id=$(basename "$nd" | sed 's/^node//')
        list=$(cat "$nd/cpulist" 2>/dev/null || echo "")
        NUMA[$id]="$list"
    done
fi

# Determine vendor flavour and recommended pin layout.
SUGG_ISO=""
SUGG_SHARED=""
SUGG_NOTE=""

if [[ "$VENDOR" == "AuthenticAMD" ]]; then
    # Recommend: pin hot path to *one* CCD; noise on another CCD.
    # Pick the two largest L3 groups.
    SORTED_L3=$(for k in "${!L3_GROUPS[@]}"; do echo "$k"; done | sort -u)
    CCDS=()
    while IFS= read -r line; do
        CCDS+=("$line")
    done <<< "$SORTED_L3"
    if (( ${#CCDS[@]} >= 2 )); then
        SUGG_ISO="${CCDS[0]}"
        SUGG_SHARED="${CCDS[1]}"
        SUGG_NOTE="AMD: pinning hot path to a single CCD keeps L3 local. Use --resctrl-group hot on top."
    elif (( ${#CCDS[@]} == 1 )); then
        SUGG_NOTE="AMD: only one CCD detected; split by core ranges within it."
    fi
else
    # Intel: typically one big LLC. Split online cpus in half and lean on CAT.
    if [[ -n "$ISOLATED" ]]; then
        SUGG_ISO="$ISOLATED"
        SUGG_NOTE="Intel: ISO from kernel cmdline; combine with CAT (scripts/cache-alloc.sh)."
    else
        # Use the upper half of online as the iso candidate.
        SUGG_NOTE="Intel: no isolcpus in cmdline. Set it at boot, then use CAT to wall off the L3."
    fi
fi

if [[ "$MODE" == "json" ]]; then
    python3 - "$VENDOR" "$MODEL" "$MICROCODE" "$KERNEL" "$ONLINE" "$ISOLATED" \
             "$GOVERNOR" "$SMT_ACTIVE" "$SMT_CONTROL" \
             "$RESCTRL_AVAIL" "$RESCTRL_MOUNTED" \
             "$L3_NUM_CLOSIDS" "$L3_CBM_MASK" "$L3_MIN_CBM" \
             "$SUGG_ISO" "$SUGG_SHARED" "$SUGG_NOTE" "$CMDLINE" \
             <<'PY' "${!L3_GROUPS[@]}"
import json, sys, os
keys = sys.argv[1:19]
ll3 = sys.argv[19:]
labels = ["vendor","model","microcode","kernel","online","isolated","governor",
          "smt_active","smt_control",
          "resctrl_available","resctrl_mounted","l3_num_closids",
          "l3_cbm_mask","l3_min_cbm_bits","suggested_iso","suggested_shared",
          "note","cmdline"]
d = dict(zip(labels, keys))
d["l3_instances"] = ll3
# NUMA
numa = {}
nroot = "/sys/devices/system/node"
if os.path.isdir(nroot):
    for n in sorted(os.listdir(nroot)):
        if n.startswith("node") and n[4:].isdigit():
            p = os.path.join(nroot, n, "cpulist")
            if os.path.exists(p):
                numa[n] = open(p).read().strip()
d["numa_nodes"] = numa
# Thread siblings
siblings = []
seen = set()
croot = "/sys/devices/system/cpu"
if os.path.isdir(croot):
    for entry in sorted(os.listdir(croot)):
        if not entry.startswith("cpu") or not entry[3:].isdigit():
            continue
        p = os.path.join(croot, entry, "topology/thread_siblings_list")
        if os.path.exists(p):
            s = open(p).read().strip()
            if s not in seen:
                seen.add(s)
                siblings.append(s)
d["thread_siblings"] = siblings
print(json.dumps(d, indent=2))
PY
    exit 0
fi

# Human-readable.
echo "host:        $(hostname)"
echo "kernel:      $KERNEL"
echo "vendor:      $VENDOR"
echo "cpu model:   $MODEL"
echo "microcode:   $MICROCODE"
echo "online:      $ONLINE  (nproc=$NCPU)"
echo "isolated:    ${ISOLATED:-<none>}"
echo "governor:    $GOVERNOR (cpu0)"
echo "smt:         control=${SMT_CONTROL:-?} active=${SMT_ACTIVE:-?}"
if [[ "$SMT_ACTIVE" == "1" ]]; then
    echo "             -> recommend: sudo ./scripts/host-prep.sh --apply (will disable SMT)"
fi
echo
echo "Thread siblings (each line is one physical core):"
echo "${!SIBLINGS[@]}" | tr ' ' '\n' | sort -V -u | sed 's/^/  /'
echo

echo "NUMA nodes:"
for k in $(echo "${!NUMA[@]}" | tr ' ' '\n' | sort -n); do
    echo "  node $k: ${NUMA[$k]}"
done
echo

echo "L3 cache instances (CCDs on AMD / LLC slices on Intel):"
i=0
for k in $(echo "${!L3_GROUPS[@]}" | tr ' ' '\n' | sort -u); do
    echo "  L3[$i]: cpus=$k"
    i=$((i+1))
done
echo

echo "resctrl:     mounted=$RESCTRL_MOUNTED available=$RESCTRL_AVAIL"
if (( RESCTRL_AVAIL )); then
    echo "  num_closids:    $L3_NUM_CLOSIDS"
    echo "  cbm_mask (max): $L3_CBM_MASK"
    echo "  min_cbm_bits:   $L3_MIN_CBM"
fi
echo

echo "/proc/cmdline:"
echo "  $CMDLINE"
echo

echo "Suggested pinning for scripts/run-bench.sh:"
if [[ -n "$SUGG_ISO" ]]; then
    echo "  ISO_CPUS=$SUGG_ISO"
fi
if [[ -n "$SUGG_SHARED" ]]; then
    echo "  SHARED_CPUS=$SUGG_SHARED"
fi
echo
echo "note: $SUGG_NOTE"
