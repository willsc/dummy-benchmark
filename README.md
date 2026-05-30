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
