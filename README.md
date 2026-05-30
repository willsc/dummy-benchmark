# dummy-benchmark

Synthetic low-latency trading pipeline used as a CPU / I/O microbench harness
on Intel Xeon and AMD Epyc boxes. Targets the question: *how much does losing
isolated cores cost in tail latency, and how does that cost compare across
vendors?*

The workspace builds three Rust binaries that wire together over a single
mmap'd file:

```
 +----------------+   UDP (io_uring Recv)   +----------------+   ticks ring   +-----------------+
 | mock-exchange  | ----------------------> |  feedhandler   | =============> | trading-engine  |
 +----------------+                         |  (io_uring +   |   (SPSC SHM)   | (1 dispatch +   |
                                            |   SHM bus)     | <============= |  N pinned       |
                                            +----------------+   orders ring  |  workers)       |
                                                                  (SPSC SHM)  +-----------------+
```

* **`mock-exchange`** — paced UDP publisher; emits fixed-layout `MarketTick`
  datagrams at a configurable rate.
* **`feedhandler`** — receives ticks via `io_uring` (32 in-flight `Recv` SQEs),
  stamps `recv_ts_ns`, publishes into the `ticks` SHM ring, drains the
  `orders` ring, and records a wire-latency histogram.
* **`trading-engine`** — runs in two modes:
  - `--workers 0` (default): inline single-threaded EMA strategy.
  - `--workers N`: an I/O dispatcher pulls ticks off the SHM ring and routes
    each to one of N strategy workers via per-worker SPSC channels (sharded by
    symbol hash). Workers run independent EMA state; their orders flow back
    over an MPSC channel which the dispatcher publishes into the SHM orders
    ring.

## The bus

`crates/shmbus` defines a single mmap'd file containing:

```
header (magic / version / ready bit)
ticks  : SpscRing  (feedhandler -> trading-engine)
orders : SpscRing  (trading-engine -> feedhandler)
```

Each `SpscRing` is a wait-free single-producer / single-consumer ring with
cacheline-padded head & tail cursors and a per-slot sequence number that
doubles as the publish barrier. Capacity is `16384` 96-byte slots per
direction.

## Bench scaffolding

Each binary accepts the same set of bench flags so scenarios are reproducible:

| flag                  | purpose                                                   |
| --------------------- | --------------------------------------------------------- |
| `--pin-cpu N`         | pin the main thread to CPU `N` via `sched_setaffinity`    |
| `--noise-threads N`   | spawn N busy-loop noise threads (compute + 1 MiB RMW)     |
| `--noise-cpus list`   | CPU list to round-robin pin those noise threads to        |
| `--bench-secs N`      | run for N seconds then exit cleanly                       |
| `--bench-report PATH` | write a JSON report to `PATH` on exit                     |

Engine additions:

| flag                  | purpose                                                   |
| --------------------- | --------------------------------------------------------- |
| `--workers N`         | number of strategy worker threads                         |
| `--worker-cpus list`  | CPU list to round-robin pin workers to                    |

The JSON report includes full host info (vendor, model, microcode, scaling
governor, turbo state, NUMA nodes, cache sizes), the run config, and the
final latency histograms (`wire_latency` for the feedhandler;
`shm_transit`, `end_to_end`, and per-worker `worker_dispatch` for the engine).

## Build

```
cargo build --release --workspace
```

Requires Linux ≥ 5.6 for `io_uring`.

## Run

End-to-end demo (interactive, ~10s):

```
./scripts/run-demo.sh
DURATION=30 RATE=5000 ./scripts/run-demo.sh
```

## Benchmark: isolated vs noisy cores

`scripts/run-bench.sh` runs the pipeline twice — once pinned to *isolated*
cores, once pinned to *shared* cores with noise threads contending — and
prints a comparison table.

```
SECS=20 RATE=2000 WORKERS=2 NOISE=4 ./scripts/run-bench.sh
```

Knobs (all env vars):

| var          | default                                              | meaning                       |
| ------------ | ---------------------------------------------------- | ----------------------------- |
| `SECS`       | 15                                                   | per-scenario duration         |
| `RATE`       | 2000                                                 | ticks/sec per symbol          |
| `SYMBOLS`    | AAPL,MSFT,GOOG,NVDA,AMZN,META,TSLA,AMD               | symbol universe               |
| `WORKERS`    | 2                                                    | engine strategy workers       |
| `NOISE`      | 4                                                    | noise threads in noisy run    |
| `ISO_CPUS`   | contents of /sys/devices/system/cpu/isolated         | isolated CPU list             |
| `SHARED_CPUS`| online minus isolated                                | shared CPU list               |
| `OUT`        | bench-out                                            | report directory              |

`ISO_CPUS` defaults to whatever the kernel exposed at
`/sys/devices/system/cpu/isolated` (set via the `isolcpus=` cmdline).
If that's empty, the script synthesises an isolation set from the upper half
of online CPUs and warns — measurements are still meaningful as a *relative*
comparison, but real kernel isolation requires `isolcpus=`.

For a clean Xeon-vs-Epyc comparison you'd typically want:

```
# In the kernel cmdline of each machine:
isolcpus=4-11 nohz_full=4-11 rcu_nocbs=4-11

# Then on each host:
ISO_CPUS=4-11 SECS=60 WORKERS=4 NOISE=6 ./scripts/run-bench.sh

# Compare bench-out/iso/engine.json and bench-out/noisy/engine.json
# across the two hosts. The host block in each JSON file identifies the box.
```

### Sample output

```
host:  Intel(R) Core(TM) Ultra 9 285H (GenuineIntel)  governor=powersave  isolated=[]

scenario  wire p50  wire p99 wire p999   shm p50   shm p99   e2e p50   e2e p99 e2e p999     ticks    orders   ticks/s
---------------------------------------------------------------------------------------------------------------------
     iso   50.00us  200.00us    1.00ms   50.00us  200.00us  100.00us  500.00us    2.00ms    18.00K      244     3.60K
   noisy   20.00us   50.00us    1.00ms   50.00us    1.00ms   50.00us    1.00ms    5.00ms    18.02K       87     3.60K
```

## Production runbook (kernel tuning + cache allocation)

For results that actually reflect the hardware — instead of noise from
governor switching, IRQ thrash, deep C-states, transparent huge pages,
or L3 eviction by background load — apply the host tuning *and* allocate
cache ways before benchmarking.

### 0. Kernel cmdline (boot-time, not runtime)

These have to be in the GRUB / systemd-boot kernel cmdline. Reboot required.

```
isolcpus=managed_irq,domain,<core-list>
nohz_full=<core-list>
rcu_nocbs=<core-list>
intel_pstate=passive          # Intel
amd_pstate=active             # AMD recent kernels
default_hugepagesz=2M hugepages=128
mitigations=off               # only on dedicated benchmark hosts
skew_tick=1 clocksource=tsc tsc=reliable
```

Pick `<core-list>` to match the cores you'll pin the hot path to.
A typical 1S Xeon 16-core layout: isolate `4-15`, leave `0-3` for the kernel
and `mock-exchange`. On a 2-CCD AMD Epyc, isolate one CCD entirely.

After reboot, verify:

```
cat /sys/devices/system/cpu/isolated
cat /proc/cmdline
```

### 1. Topology inspection

```
./scripts/topology.sh
```

Prints vendor/model, NUMA, AMD CCDs (each unique L3 `shared_cpu_list`), cache
sizes, resctrl capability, and a recommended `ISO_CPUS` / `SHARED_CPUS`.

`./scripts/topology.sh --json` for the machine-readable form.

### 2. Runtime kernel tuning

```
sudo ./scripts/host-prep.sh --dry-run     # preview every write
sudo ./scripts/host-prep.sh --apply       # apply
sudo ./scripts/host-prep.sh --revert      # restore prior values
```

Applies:

- `performance` cpufreq governor on every online CPU
- Disables every C-state below `C1` (deep states cause µs-scale wake jitter)
- Enables turbo / boost (Intel `intel_pstate`, AMD `cpufreq/boost`)
- THP `enabled=madvise`, `defrag=never`, swappiness=0, zone_reclaim_mode=0
- `nr_hugepages=128` (256 MiB of 2MB pages — override with `HUGEPAGES=…`)
- Disables the NMI watchdog
- Bumps scheduler granularity so threads stay on their pinned core
- Bumps `net.core.{r,w}mem_max` to 64 MiB for UDP
- Rebinds every IRQ off the isolated cores (`--no-irq` to skip)

Every individual sysfs/procfs write is snapshotted under
`/var/run/dummy-benchmark/` so `--revert` restores prior values exactly.
Re-running `--apply` is idempotent.

### 3. Cache allocation (Intel CAT / AMD CAT)

Provisions two `resctrl` groups: `hot` (top L3 ways, full memory BW) and
`noise` (bottom L3 ways, capped memory BW on Intel).

```
sudo ./scripts/cache-alloc.sh --setup     # create groups + schemata
./scripts/cache-alloc.sh --show           # inspect
sudo ./scripts/cache-alloc.sh --clean     # tear down
```

Tunable via env vars:
- `HOT_WAYS_PCT=75` — % of L3 ways for hot path (rest goes to noise)
- `NOISE_MB_PCT=30` — Intel MBA cap for the noise group
- `OWNER=$(id -un)` — who's running the bench (`tasks` files get `chown`ed
  so non-root processes can join)

On AMD Epyc the same per-cache-id CBM is applied to every L3 instance, which
combines naturally with single-CCD pinning to give exclusive L3 to the hot
path.

### 4. Bench

The Rust binaries each take `--resctrl-group NAME` and the engine also takes
`--noise-resctrl-group NAME`. Threads call `join_group(name)` from
`shmbus::resctrl` right after pinning, so the kernel applies the right CBM
from the first memory touch.

The easiest path: `PROD=1` runs steps 2–4 automatically and reverts on exit.

```
sudo PROD=1 ISO_CPUS=4-11 WORKERS=4 NOISE=6 SECS=60 ./scripts/run-bench.sh
```

This runs `host-prep --apply`, `cache-alloc --setup`, the two scenarios with
`--resctrl-group hot` / `--noise-resctrl-group noise`, then `cache-alloc
--clean` and `host-prep --revert` on exit.

### Cross-vendor comparison workflow

```
# On each machine (Xeon, Epyc):
sudo PROD=1 ISO_CPUS=4-11 WORKERS=4 NOISE=6 SECS=60 ./scripts/run-bench.sh
scp bench-out/iso/engine.json     user@plotbox:results/$(hostname)-iso-engine.json
scp bench-out/noisy/engine.json   user@plotbox:results/$(hostname)-noisy-engine.json
```

Every JSON report carries a full `host` block (vendor, model, microcode,
governor, turbo state, NUMA nodes, cache sizes) so the source machine is
unambiguous when diffing across boxes.

## Running components by hand

```
# terminal 1 — feedhandler pinned to cpu 5
./target/release/feedhandler --bind 127.0.0.1:9001 --bus /tmp/shmbus.bin \
  --pin-cpu 5

# terminal 2 — engine with 2 workers pinned to cpus 6,7
./target/release/trading-engine --bus /tmp/shmbus.bin \
  --pin-cpu 4 --workers 2 --worker-cpus 6,7

# terminal 3 — mock-exchange
./target/release/mock-exchange --target 127.0.0.1:9001 --rate 2000
```

Delete `/tmp/shmbus.bin` between runs to start from a clean ring state.
