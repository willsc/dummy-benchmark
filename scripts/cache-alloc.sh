#!/usr/bin/env bash
# Provision Intel CAT / AMD CAT cache allocation groups via the kernel
# `resctrl` filesystem.
#
# Two groups are created:
#   $HOT_GROUP   — exclusive top L3 ways (and full memory bandwidth on Intel)
#   $NOISE_GROUP — bottom L3 ways (and capped memory bandwidth on Intel)
#
# Same CBM is applied per L3 cache instance, which on AMD Epyc means each CCD
# gets the same split — combine with CCD-aware pinning (see topology.sh).
#
#   sudo ./scripts/cache-alloc.sh             # setup hot/noise groups
#   sudo ./scripts/cache-alloc.sh --clean     # tear them down
#   ./scripts/cache-alloc.sh --show           # print current schemata
#
# Env vars:
#   HOT_GROUP=hot         resctrl group name for hot path threads
#   NOISE_GROUP=noise     resctrl group name for noise threads
#   HOT_WAYS_PCT=75       percentage of L3 ways given to HOT (rest -> NOISE)
#   NOISE_MB_PCT=30       Intel MBA: % of memory bandwidth allowed to NOISE
#   OWNER=$(id -un)       user who'll be running the bench (chowns tasks file)

set -euo pipefail

HOT_GROUP="${HOT_GROUP:-hot}"
NOISE_GROUP="${NOISE_GROUP:-noise}"
HOT_WAYS_PCT="${HOT_WAYS_PCT:-75}"
NOISE_MB_PCT="${NOISE_MB_PCT:-30}"
OWNER="${OWNER:-$(id -un)}"

ACTION="setup"
case "${1:-}" in
    --setup|"") ACTION="setup" ;;
    --clean)    ACTION="clean" ;;
    --show)     ACTION="show"  ;;
    -h|--help)  sed -n '2,28p' "$0"; exit 0 ;;
    *)          echo "unknown arg: $1" >&2; exit 2 ;;
esac

require_root() {
    if [[ $EUID -ne 0 ]]; then
        echo "cache-alloc: must run as root for $ACTION (try: sudo $0 $*)" >&2
        exit 1
    fi
}

ensure_resctrl_mounted() {
    if [[ -e /sys/fs/resctrl/schemata ]]; then
        return
    fi
    if [[ ! -d /sys/fs/resctrl ]]; then
        echo "cache-alloc: /sys/fs/resctrl missing — kernel without CONFIG_X86_CPU_RESCTRL or unsupported CPU." >&2
        exit 1
    fi
    require_root
    echo "cache-alloc: mounting resctrl..."
    mount -t resctrl resctrl /sys/fs/resctrl
}

if [[ "$ACTION" == "show" ]]; then
    if [[ ! -e /sys/fs/resctrl/schemata ]]; then
        echo "cache-alloc: resctrl not mounted (run: sudo $0 --setup)" >&2
        exit 1
    fi
    echo "default schemata:"
    sed 's/^/  /' /sys/fs/resctrl/schemata
    for g in /sys/fs/resctrl/*/; do
        [[ -d "$g" ]] || continue
        [[ -f "$g/schemata" ]] || continue
        name=$(basename "$g")
        echo
        echo "group: $name"
        echo "  schemata:"
        sed 's/^/    /' "$g/schemata"
        if [[ -r "$g/tasks" ]]; then
            tids_n=$(wc -l < "$g/tasks")
            echo "  tasks: $tids_n TID(s)"
        fi
        if [[ -r "$g/cpus" ]]; then
            echo "  cpus:  $(cat "$g/cpus")"
        fi
    done
    exit 0
fi

if [[ "$ACTION" == "clean" ]]; then
    require_root
    ensure_resctrl_mounted
    for g in "$HOT_GROUP" "$NOISE_GROUP"; do
        if [[ -d "/sys/fs/resctrl/$g" ]]; then
            echo "  removing /sys/fs/resctrl/$g"
            rmdir "/sys/fs/resctrl/$g"
        else
            echo "  not present: /sys/fs/resctrl/$g"
        fi
    done
    exit 0
fi

# === setup ===
require_root
ensure_resctrl_mounted

CBM_MASK=$(cat /sys/fs/resctrl/info/L3/cbm_mask)
MIN_CBM_BITS=$(cat /sys/fs/resctrl/info/L3/min_cbm_bits)
NUM_CLOSIDS=$(cat /sys/fs/resctrl/info/L3/num_closids)

WAYS=$(python3 -c "print(bin(int('$CBM_MASK', 16)).count('1'))")
echo "cache-alloc: L3 cbm_mask=0x$CBM_MASK ($WAYS ways), min_cbm_bits=$MIN_CBM_BITS, num_closids=$NUM_CLOSIDS"

if (( NUM_CLOSIDS < 3 )); then
    echo "cache-alloc: WARNING — only $NUM_CLOSIDS CLOSIDs available; need >=3 for default+hot+noise." >&2
fi

# Compute the way split.
read -r HOT_WAYS NOISE_WAYS HOT_CBM NOISE_CBM <<EOF_PY
$(python3 - <<PY
ways = $WAYS
min_bits = $MIN_CBM_BITS
hot_pct = $HOT_WAYS_PCT
hot = max(int(ways * hot_pct / 100), min_bits)
noise = max(ways - hot, min_bits)
if hot + noise > ways:
    hot = ways - noise
hot_mask  = ((1 << hot) - 1) << (ways - hot)
noise_mask = (1 << noise) - 1
print(hot, noise, format(hot_mask, 'x'), format(noise_mask, 'x'))
PY
)
EOF_PY

echo "cache-alloc: hot=$HOT_WAYS ways (CBM=0x$HOT_CBM) noise=$NOISE_WAYS ways (CBM=0x$NOISE_CBM)"

# Enumerate L3 instances by parsing the default schemata.
L3_IDS=$(sed -n 's/^[[:space:]]*L3:\(.*\)$/\1/p' /sys/fs/resctrl/schemata \
            | head -1 | tr ';' '\n' | sed 's/=.*//' | tr -d '[:space:]' \
            | grep -v '^$' || true)
if [[ -z "$L3_IDS" ]]; then
    echo "cache-alloc: ERROR — no L3 lines in default schemata; is L3 CAT supported?" >&2
    exit 1
fi
echo "cache-alloc: L3 cache IDs: $(echo "$L3_IDS" | tr '\n' ' ')"

build_l3_line() {
    local cbm="$1"
    local line="L3:"
    local first=1
    while read -r id; do
        [[ -z "$id" ]] && continue
        if (( first )); then
            line+="${id}=${cbm}"
            first=0
        else
            line+=";${id}=${cbm}"
        fi
    done <<< "$L3_IDS"
    echo "$line"
}

HOT_SCHEMATA="$(build_l3_line "$HOT_CBM")"
NOISE_SCHEMATA="$(build_l3_line "$NOISE_CBM")"

# Detect MBA (Intel-style percentage only — AMD's kbps form needs different
# semantics so we skip it conservatively).
HAVE_INTEL_MBA=0
if [[ -d /sys/fs/resctrl/info/MB ]]; then
    DELAY_LINEAR=$(cat /sys/fs/resctrl/info/MB/delay_linear 2>/dev/null || echo "1")
    if [[ "$DELAY_LINEAR" == "1" ]]; then
        HAVE_INTEL_MBA=1
    else
        echo "cache-alloc: detected non-linear MBA (likely AMD); skipping MB throttling."
    fi
fi

if (( HAVE_INTEL_MBA )); then
    MB_IDS=$(sed -n 's/^[[:space:]]*MB:\(.*\)$/\1/p' /sys/fs/resctrl/schemata \
                | head -1 | tr ';' '\n' | sed 's/=.*//' | tr -d '[:space:]' \
                | grep -v '^$' || true)
    build_mb_line() {
        local pct="$1"
        local line="MB:"
        local first=1
        while read -r id; do
            [[ -z "$id" ]] && continue
            if (( first )); then
                line+="${id}=${pct}"
                first=0
            else
                line+=";${id}=${pct}"
            fi
        done <<< "$MB_IDS"
        echo "$line"
    }
    HOT_MB_LINE="$(build_mb_line 100)"
    NOISE_MB_LINE="$(build_mb_line "$NOISE_MB_PCT")"
    HOT_SCHEMATA+=$'\n'"$HOT_MB_LINE"
    NOISE_SCHEMATA+=$'\n'"$NOISE_MB_LINE"
fi

# Create groups.
for g in "$HOT_GROUP" "$NOISE_GROUP"; do
    if [[ ! -d "/sys/fs/resctrl/$g" ]]; then
        mkdir "/sys/fs/resctrl/$g"
        echo "cache-alloc: created /sys/fs/resctrl/$g"
    else
        echo "cache-alloc: reusing /sys/fs/resctrl/$g"
    fi
done

echo "cache-alloc: applying schemata for $HOT_GROUP:"
sed 's/^/  /' <<< "$HOT_SCHEMATA"
printf '%s\n' "$HOT_SCHEMATA" > "/sys/fs/resctrl/$HOT_GROUP/schemata"

echo "cache-alloc: applying schemata for $NOISE_GROUP:"
sed 's/^/  /' <<< "$NOISE_SCHEMATA"
printf '%s\n' "$NOISE_SCHEMATA" > "/sys/fs/resctrl/$NOISE_GROUP/schemata"

# Chown tasks so the bench user can join groups without sudo.
for g in "$HOT_GROUP" "$NOISE_GROUP"; do
    chown "$OWNER" "/sys/fs/resctrl/$g/tasks"
    chmod 0664 "/sys/fs/resctrl/$g/tasks"
done
echo "cache-alloc: $HOT_GROUP and $NOISE_GROUP tasks files chowned to $OWNER"

echo
echo "Bench binaries can now use:"
echo "  --resctrl-group $HOT_GROUP --noise-resctrl-group $NOISE_GROUP"
