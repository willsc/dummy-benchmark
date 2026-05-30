#!/usr/bin/env bash
# Production-grade runtime tuning for low-latency benchmarks.
#
#   sudo ./scripts/host-prep.sh [--apply|--dry-run|--revert] [--no-irq] [--no-hugepages]
#
# What this script does (apply mode):
#   * CPU frequency governor -> performance (all online cpus)
#   * Disables deep C-states (C2+ on Intel, C2+ on AMD acpi_idle)
#   * Ensures turbo is enabled (Intel intel_pstate / AMD cpufreq boost)
#   * Disables Transparent Huge Pages defrag, sets THP enabled=madvise
#   * vm.swappiness=0, vm.zone_reclaim_mode=0, vm.stat_interval=10
#   * Disables the NMI watchdog (kernel.nmi_watchdog=0)
#   * Bumps UDP socket buffer caps (net.core.rmem_max / wmem_max / backlog)
#   * Allocates 2MB hugepages (--no-hugepages skips)
#   * Rebinds all IRQs off any cpus listed in /sys/devices/system/cpu/isolated
#     (--no-irq skips)
#
# Every individual write is snapshotted under /var/run/dummy-benchmark/ before
# being touched, so `--revert` restores the prior values exactly. State files
# are keyed on the path so re-running --apply is idempotent.
#
# What this CANNOT do at runtime — these need to be in your kernel cmdline:
#   isolcpus=managed_irq,domain,<core-list>
#   nohz_full=<core-list>
#   rcu_nocbs=<core-list>
#   intel_pstate=passive | amd_pstate=active
#   default_hugepagesz=2M
#   mitigations=off            # only on dedicated benchmark hosts!
# The script prints the recommendation at the end.

set -euo pipefail

ACTION="apply"
DO_IRQ=1
DO_HUGEPAGES=1
HUGEPAGES_N="${HUGEPAGES:-128}"   # 128 * 2MiB = 256 MiB

while (( $# )); do
    case "$1" in
        --apply)         ACTION="apply" ;;
        --dry-run|-n)    ACTION="dry-run" ;;
        --revert)        ACTION="revert" ;;
        --no-irq)        DO_IRQ=0 ;;
        --no-hugepages)  DO_HUGEPAGES=0 ;;
        -h|--help)
            sed -n '2,30p' "$0"; exit 0 ;;
        *)
            echo "unknown arg: $1" >&2; exit 2 ;;
    esac
    shift
done

if [[ "$ACTION" != "dry-run" && $EUID -ne 0 ]]; then
    echo "host-prep: must run as root (action=$ACTION). Use --dry-run to preview." >&2
    exit 1
fi

STATE_DIR="/var/run/dummy-benchmark"
[[ "$ACTION" != "dry-run" ]] && mkdir -p "$STATE_DIR"

# Snapshot-aware sysfs/procfs writer.
write_sysfs() {
    local path="$1" value="$2"
    if [[ ! -e "$path" ]]; then
        echo "  skip (missing): $path"
        return 0
    fi
    if [[ ! -w "$path" && "$ACTION" != "dry-run" ]]; then
        echo "  skip (not writable): $path"
        return 0
    fi
    local current
    current=$(cat "$path" 2>/dev/null || echo "")
    local key
    key=$(echo "$path" | sed 's|/|_|g')
    local saved="$STATE_DIR/$key"
    case "$ACTION" in
        dry-run)
            echo "  would write: $path  '$current' -> '$value'"
            ;;
        apply)
            [[ -f "$saved" ]] || echo "$current" > "$saved"
            if [[ "$current" != "$value" ]]; then
                echo "$value" > "$path" 2>/dev/null || echo "  write failed: $path"
                echo "  set: $path -> $value (was '$current')"
            else
                echo "  already: $path = $value"
            fi
            ;;
        revert)
            if [[ -f "$saved" ]]; then
                local prev
                prev=$(cat "$saved")
                echo "$prev" > "$path" 2>/dev/null || true
                echo "  reverted: $path -> $prev"
                rm -f "$saved"
            else
                echo "  no snapshot: $path"
            fi
            ;;
    esac
}

section() { echo; echo "===== $* ====="; }

section "CPU frequency: performance governor"
for f in /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; do
    write_sysfs "$f" "performance"
done

section "C-states: keep POLL + C1 only"
for state_dir in /sys/devices/system/cpu/cpu*/cpuidle/state*/; do
    [[ -d "$state_dir" ]] || continue
    local_name=$(cat "$state_dir/name" 2>/dev/null || echo "")
    case "$local_name" in
        # Anything matching POLL or C1 (incl. C1E, C1_ACPI) we keep enabled.
        POLL|C1|C1E|C1_*) : ;;
        # Disable everything else: deeper acpi_idle (C2_ACPI, C3_ACPI),
        # intel_idle (C2..C10, MWAIT), AMD cpuidle states.
        *)
            write_sysfs "$state_dir/disable" "1"
            ;;
    esac
done

section "Turbo / boost: enable"
if [[ -e /sys/devices/system/cpu/intel_pstate/no_turbo ]]; then
    write_sysfs /sys/devices/system/cpu/intel_pstate/no_turbo "0"
fi
if [[ -e /sys/devices/system/cpu/cpufreq/boost ]]; then
    write_sysfs /sys/devices/system/cpu/cpufreq/boost "1"
fi
if [[ -e /sys/devices/system/cpu/amd_pstate/status ]]; then
    # If amd_pstate is loaded, prefer the "active" mode for low-latency.
    write_sysfs /sys/devices/system/cpu/amd_pstate/status "active"
fi

section "Memory: THP, swappiness, zone reclaim"
write_sysfs /sys/kernel/mm/transparent_hugepage/enabled "madvise"
write_sysfs /sys/kernel/mm/transparent_hugepage/defrag "never"
write_sysfs /sys/kernel/mm/transparent_hugepage/khugepaged/defrag "0"
write_sysfs /proc/sys/vm/swappiness "0"
write_sysfs /proc/sys/vm/zone_reclaim_mode "0"
write_sysfs /proc/sys/vm/stat_interval "10"

if (( DO_HUGEPAGES )); then
    section "Hugepages: allocate $HUGEPAGES_N x 2MiB"
    write_sysfs /proc/sys/vm/nr_hugepages "$HUGEPAGES_N"
fi

section "NMI watchdog: off"
write_sysfs /proc/sys/kernel/nmi_watchdog "0"

section "Scheduler: full migration cost so the kernel doesn't move our threads"
write_sysfs /proc/sys/kernel/sched_migration_cost_ns "5000000"
write_sysfs /proc/sys/kernel/sched_min_granularity_ns "10000000"
write_sysfs /proc/sys/kernel/sched_wakeup_granularity_ns "15000000"

section "Networking: bigger UDP buffers"
write_sysfs /proc/sys/net/core/rmem_max     "$((64*1024*1024))"
write_sysfs /proc/sys/net/core/rmem_default "$((16*1024*1024))"
write_sysfs /proc/sys/net/core/wmem_max     "$((64*1024*1024))"
write_sysfs /proc/sys/net/core/wmem_default "$((16*1024*1024))"
write_sysfs /proc/sys/net/core/netdev_max_backlog "10000"
write_sysfs /proc/sys/net/core/netdev_budget      "600"

if (( DO_IRQ )); then
    section "IRQ affinity: move IRQs off isolated cores"
    ISOLATED=$(cat /sys/devices/system/cpu/isolated 2>/dev/null || true)
    if [[ -z "$ISOLATED" ]]; then
        echo "  /sys reports no isolated CPUs — nothing to do"
    elif ! command -v python3 >/dev/null 2>&1; then
        echo "  python3 not available — skipping IRQ rebind"
    else
        NCPU=$(nproc)
        MASK=$(python3 - "$ISOLATED" "$NCPU" <<'PY'
import sys
iso_str, ncpu = sys.argv[1], int(sys.argv[2])
iso = set()
for part in iso_str.split(','):
    part = part.strip()
    if not part: continue
    if '-' in part:
        a,b = part.split('-')
        iso.update(range(int(a), int(b)+1))
    else:
        iso.add(int(part))
m = 0
for c in range(ncpu):
    if c not in iso:
        m |= 1 << c
# smp_affinity wants a comma-grouped hex bitmask, 32 bits per group, LSB group first... actually
# smp_affinity is one long hex value with optional commas every 32 bits. A single hex w/o commas
# is accepted by the kernel for cpus < 32; for >32 we need grouping.
hex_full = format(m, 'x')
# Pad to multiple of 8 hex chars (32 bits) and insert commas every 8 chars from the right.
pad = (-len(hex_full)) % 8
hex_full = '0' * pad + hex_full
out = ','.join(hex_full[i:i+8] for i in range(0, len(hex_full), 8))
print(out)
PY
)
        echo "  non-isolated mask: $MASK"
        write_sysfs /proc/irq/default_smp_affinity "$MASK"
        for irq_dir in /proc/irq/[0-9]*; do
            [[ -d "$irq_dir" ]] || continue
            write_sysfs "$irq_dir/smp_affinity" "$MASK"
        done
    fi
fi

section "Cmdline check"
echo "  /proc/cmdline:"
echo "    $(cat /proc/cmdline)"
echo
echo "  Recommended additions for an isolated benchmark host:"
echo "    isolcpus=managed_irq,domain,<core-list>"
echo "    nohz_full=<core-list>"
echo "    rcu_nocbs=<core-list>"
echo "    intel_pstate=passive          # Intel"
echo "    amd_pstate=active             # AMD recent kernels"
echo "    default_hugepagesz=2M hugepages=$HUGEPAGES_N"
echo "    mitigations=off               # only on dedicated benchmark hosts"
echo "    skew_tick=1 clocksource=tsc tsc=reliable"
echo

section "Sanity"
echo "  governor (cpu0):   $(cat /sys/devices/system/cpu/cpu0/cpufreq/scaling_governor 2>/dev/null || echo '?')"
echo "  turbo state:       $(
    if [[ -e /sys/devices/system/cpu/intel_pstate/no_turbo ]]; then
        [[ $(cat /sys/devices/system/cpu/intel_pstate/no_turbo) == 0 ]] && echo enabled || echo disabled
    elif [[ -e /sys/devices/system/cpu/cpufreq/boost ]]; then
        [[ $(cat /sys/devices/system/cpu/cpufreq/boost) == 1 ]] && echo enabled || echo disabled
    else echo unknown
    fi)"
echo "  THP enabled:       $(cat /sys/kernel/mm/transparent_hugepage/enabled)"
echo "  swappiness:        $(cat /proc/sys/vm/swappiness)"
echo "  nr_hugepages:      $(cat /proc/sys/vm/nr_hugepages)"
echo "  nmi_watchdog:      $(cat /proc/sys/kernel/nmi_watchdog)"
echo "  rmem_max:          $(cat /proc/sys/net/core/rmem_max)"
echo "  isolated cpus:     $(cat /sys/devices/system/cpu/isolated 2>/dev/null || echo '<none>')"

echo
echo "host-prep: action=$ACTION complete."
