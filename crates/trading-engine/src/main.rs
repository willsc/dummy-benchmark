//! Trading engine.
//!
//! * Consumes ticks from the shared-memory bus.
//! * Runs a deliberately tiny EMA-deviation strategy per symbol.
//! * Emits orders back over the orders ring.
//!
//! Single-threaded by default. With `--workers N`, the main thread becomes an
//! I/O dispatcher: it pops ticks off the SHM ring and routes each one (by
//! symbol hash) to one of N strategy worker threads via a bounded SPSC
//! channel. Workers run independent EMA state and emit orders back over an
//! MPSC channel, which the main thread drains into the SHM orders ring.
//!
//! This layout is what we use to compare Intel vs AMD when both isolated and
//! non-isolated cores are in play. Pin the dispatcher with `--pin-cpu`, the
//! workers with `--worker-cpus`, and inject contention with
//! `--noise-threads`/`--noise-cpus`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use serde::Serialize;

use shmbus::{
    cpu, host_info, noise, write_report, HostInfo, LatencyHistogram, LatencySummary, MarketTick,
    OrderMsg, Side, ShmBus, SLOT_PAYLOAD,
};

#[derive(Parser, Debug)]
#[command(about = "Trading engine (consumes ticks, emits orders)")]
struct Args {
    /// Path to the shared-memory bus file.
    #[arg(long, default_value = "/tmp/shmbus.bin")]
    bus: String,
    /// Mid-price EMA half-life in ticks.
    #[arg(long, default_value_t = 64.0)]
    ema_halflife: f64,
    /// Send a quote when deviation from EMA exceeds this many basis points.
    #[arg(long, default_value_t = 5.0)]
    threshold_bps: f64,
    /// Minimum nanos between successive orders on the same symbol.
    #[arg(long, default_value_t = 50_000_000)]
    cooldown_ns: u64,
    /// Default order quantity.
    #[arg(long, default_value_t = 100)]
    qty: u32,

    /// Pin the main (I/O dispatcher) thread to this CPU.
    #[arg(long)]
    pin_cpu: Option<usize>,
    /// Number of strategy worker threads (0 = single-threaded inline).
    #[arg(long, default_value_t = 0)]
    workers: usize,
    /// CPU list for worker pinning (e.g. "5-8"). Round-robins if shorter than `workers`.
    #[arg(long, default_value = "")]
    worker_cpus: String,
    /// Number of busy-loop noise threads to spawn.
    #[arg(long, default_value_t = 0)]
    noise_threads: usize,
    /// CPU list for noise threads (e.g. "0-3,12").
    #[arg(long, default_value = "")]
    noise_cpus: String,
    /// Run for N seconds then exit (0 = run until ctrl-c).
    #[arg(long, default_value_t = 0)]
    bench_secs: u64,
    /// Write the JSON bench report to this path on exit.
    #[arg(long)]
    bench_report: Option<String>,
    /// Suppress per-order log spam.
    #[arg(long, default_value_t = false)]
    quiet: bool,
}

#[derive(Serialize)]
struct EngineReport {
    schema: &'static str,
    host: HostInfo,
    config: EngineConfig,
    metrics: EngineMetrics,
}

#[derive(Serialize)]
struct EngineConfig {
    binary: &'static str,
    pin_cpu: Option<usize>,
    workers: usize,
    worker_cpus: String,
    noise_threads: usize,
    noise_cpus: String,
    bench_secs: u64,
    ema_halflife: f64,
    threshold_bps: f64,
    cooldown_ns: u64,
    qty: u32,
    bus: String,
}

#[derive(Serialize)]
struct EngineMetrics {
    ticks_consumed: u64,
    ticks_dropped_dispatch: u64,
    orders_emitted: u64,
    orders_dropped_ring_full: u64,
    elapsed_secs: f64,
    throughput_ticks_per_sec: f64,
    shm_transit: LatencySummary,
    end_to_end: LatencySummary,
    /// Per-worker dispatch latency (dispatcher -> worker). Empty in inline mode.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    worker_dispatch: Vec<LatencySummary>,
}

#[derive(Default)]
struct SymbolState {
    ema_px: f64,
    last_order_ns: u64,
    initialised: bool,
}

/// Message handed from the I/O thread to a worker.
struct WorkItem {
    tick: MarketTick,
    /// `now_ns()` captured by the dispatcher at hand-off; workers use it to
    /// compute dispatch latency.
    enqueued_ns: u64,
}

fn now_ns() -> u64 {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    dur.as_secs() * 1_000_000_000 + dur.subsec_nanos() as u64
}

fn main() -> Result<()> {
    let args = Args::parse();

    if let Some(c) = args.pin_cpu {
        cpu::pin_to_cpu(c).with_context(|| format!("pinning main to cpu {c}"))?;
        eprintln!("trading-engine: pinned main thread to cpu {c}");
    }

    let host = host_info();
    eprintln!(
        "trading-engine: host {} ({}) kernel={} governor={} isolated=[{}]",
        host.cpu_model, host.cpu_vendor, host.kernel, host.governor, host.isolated_cpus,
    );

    let worker_cpus = cpu::parse_cpu_list(&args.worker_cpus)
        .map_err(|e| anyhow!("--worker-cpus: {e}"))?;
    let noise_cpus = cpu::parse_cpu_list(&args.noise_cpus)
        .map_err(|e| anyhow!("--noise-cpus: {e}"))?;

    let noise_handle = if args.noise_threads > 0 {
        eprintln!(
            "trading-engine: spawning {} noise threads on [{}]",
            args.noise_threads,
            cpu::format_cpu_list(&noise_cpus)
        );
        Some(noise::spawn_noise(args.noise_threads, &noise_cpus))
    } else {
        None
    };

    let bus = ShmBus::open_or_create(&args.bus)
        .with_context(|| format!("opening shm bus at {}", args.bus))?;
    // Safety: only one trading engine attaches to a given bus.
    let mut tick_cons = unsafe { bus.ticks_consumer() };
    let mut order_prod = unsafe { bus.orders_producer() };

    let stop = Arc::new(AtomicBool::new(false));
    let stop_h = stop.clone();
    ctrlc::set_handler(move || stop_h.store(true, Ordering::SeqCst)).ok();

    let started = Instant::now();
    let deadline = if args.bench_secs > 0 {
        Some(started + Duration::from_secs(args.bench_secs))
    } else {
        None
    };

    if args.workers == 0 {
        run_inline(args, host, worker_cpus, noise_cpus, noise_handle,
                   &mut tick_cons, &mut order_prod, stop, started, deadline)
    } else {
        run_with_workers(args, host, worker_cpus, noise_cpus, noise_handle,
                         &mut tick_cons, &mut order_prod, stop, started, deadline)
    }
}

/// Single-threaded path (workers = 0). Same shape as before.
#[allow(clippy::too_many_arguments)]
fn run_inline(
    args: Args,
    host: HostInfo,
    _worker_cpus: Vec<usize>,
    noise_cpus: Vec<usize>,
    noise_handle: Option<noise::NoiseHandle>,
    tick_cons: &mut shmbus::RingConsumer,
    order_prod: &mut shmbus::RingProducer,
    stop: Arc<AtomicBool>,
    started: Instant,
    deadline: Option<Instant>,
) -> Result<()> {
    eprintln!("trading-engine: inline mode (no worker threads)");
    let alpha = ema_alpha(args.ema_halflife);

    let mut state: HashMap<[u8; shmbus::message::SYMBOL_LEN], SymbolState> = HashMap::new();
    let mut buf = [0u8; SLOT_PAYLOAD];
    let mut last_report = Instant::now();
    let mut ticks_seen: u64 = 0;
    let mut orders_sent: u64 = 0;
    let mut orders_dropped: u64 = 0;
    let mut next_order_id: u64 = 1;
    let mut shm_transit = LatencyHistogram::new();
    let mut end_to_end = LatencyHistogram::new();
    let mut idle_spin: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }

        if tick_cons.try_consume(&mut buf).is_none() {
            idle_backoff(&mut idle_spin);
            if last_report.elapsed() >= Duration::from_secs(2) {
                report_live(ticks_seen, orders_sent, tick_cons.pending(),
                            Some(&shm_transit), Some(&end_to_end));
                last_report = Instant::now();
            }
            continue;
        }
        idle_spin = 0;
        ticks_seen += 1;

        let tick = unsafe { std::ptr::read(buf.as_ptr() as *const MarketTick) };
        let consume_ts = now_ns();
        shm_transit.record(consume_ts.saturating_sub(tick.recv_ts_ns));
        end_to_end.record(consume_ts.saturating_sub(tick.exch_ts_ns));

        let (order_opt, _) = strategy_step(&tick, &mut state, alpha,
                                           args.threshold_bps, args.cooldown_ns,
                                           args.qty, &mut next_order_id);
        if let Some(order) = order_opt {
            if !try_publish_order(order_prod, &order) {
                orders_dropped += 1;
            } else {
                orders_sent += 1;
                if !args.quiet {
                    eprintln!("trading-engine: -> {}", order);
                }
            }
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            report_live(ticks_seen, orders_sent, tick_cons.pending(),
                        Some(&shm_transit), Some(&end_to_end));
            last_report = Instant::now();
        }
    }

    let elapsed = started.elapsed();
    eprintln!("trading-engine: run finished after {:?} ({} ticks, {} orders, {} order-drops)",
              elapsed, ticks_seen, orders_sent, orders_dropped);

    finalise_report(args, host, noise_cpus, ticks_seen, 0, orders_sent, orders_dropped,
                    elapsed, shm_transit, end_to_end, Vec::new(), noise_handle)
}

/// Multi-worker path: one I/O dispatcher + N strategy threads + order publisher.
#[allow(clippy::too_many_arguments)]
fn run_with_workers(
    args: Args,
    host: HostInfo,
    worker_cpus: Vec<usize>,
    noise_cpus: Vec<usize>,
    noise_handle: Option<noise::NoiseHandle>,
    tick_cons: &mut shmbus::RingConsumer,
    order_prod: &mut shmbus::RingProducer,
    stop: Arc<AtomicBool>,
    started: Instant,
    deadline: Option<Instant>,
) -> Result<()> {
    let n = args.workers;
    eprintln!("trading-engine: spawning {n} strategy workers on [{}]",
              cpu::format_cpu_list(&worker_cpus));

    // Per-worker SPSC tick channels and a single MPSC orders channel.
    let mut tick_txs: Vec<Sender<WorkItem>> = Vec::with_capacity(n);
    let (order_tx, order_rx) = bounded::<(OrderMsg, &'static str)>(8192);
    let mut handles: Vec<JoinHandle<WorkerStats>> = Vec::with_capacity(n);

    let alpha = ema_alpha(args.ema_halflife);

    for w in 0..n {
        let (tx, rx) = bounded::<WorkItem>(8192);
        tick_txs.push(tx);
        let order_tx_w = order_tx.clone();
        let pin = worker_cpus.get(w % worker_cpus.len().max(1)).copied()
            .filter(|_| !worker_cpus.is_empty());
        let cooldown_ns = args.cooldown_ns;
        let threshold_bps = args.threshold_bps;
        let qty = args.qty;
        let stop_w = stop.clone();
        let h = thread::Builder::new()
            .name(format!("strategy-{w}"))
            .spawn(move || worker_main(w, rx, order_tx_w, pin, stop_w, alpha,
                                       threshold_bps, cooldown_ns, qty))
            .context("spawning worker thread")?;
        handles.push(h);
    }
    drop(order_tx);

    let mut buf = [0u8; SLOT_PAYLOAD];
    let mut last_report = Instant::now();
    let mut ticks_seen: u64 = 0;
    let mut ticks_dropped: u64 = 0;
    let mut orders_sent: u64 = 0;
    let mut orders_dropped: u64 = 0;
    let mut shm_transit = LatencyHistogram::new();
    let mut end_to_end = LatencyHistogram::new();
    let mut idle_spin: u32 = 0;

    while !stop.load(Ordering::Relaxed) {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }

        let mut did_work = false;

        if tick_cons.try_consume(&mut buf).is_some() {
            did_work = true;
            ticks_seen += 1;
            let tick = unsafe { std::ptr::read(buf.as_ptr() as *const MarketTick) };
            let consume_ts = now_ns();
            shm_transit.record(consume_ts.saturating_sub(tick.recv_ts_ns));
            end_to_end.record(consume_ts.saturating_sub(tick.exch_ts_ns));

            let target = (symbol_hash(&tick.symbol) as usize) % n;
            let item = WorkItem { tick, enqueued_ns: now_ns() };
            match tick_txs[target].try_send(item) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => ticks_dropped += 1,
                Err(TrySendError::Disconnected(_)) => break,
            }
        }

        // Drain orders without blocking.
        while let Ok((order, _src)) = order_rx.try_recv() {
            did_work = true;
            if try_publish_order(order_prod, &order) {
                orders_sent += 1;
                if !args.quiet {
                    eprintln!("trading-engine: -> {}", order);
                }
            } else {
                orders_dropped += 1;
            }
        }

        if !did_work {
            idle_backoff(&mut idle_spin);
        } else {
            idle_spin = 0;
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            report_live(ticks_seen, orders_sent, tick_cons.pending(),
                        Some(&shm_transit), Some(&end_to_end));
            last_report = Instant::now();
        }
    }

    // Tell workers to stop and join them.
    drop(tick_txs);
    let mut worker_stats: Vec<WorkerStats> = Vec::with_capacity(n);
    for h in handles {
        worker_stats.push(h.join().unwrap_or_else(|_| WorkerStats::default()));
    }
    // Final drain of any orders the workers emitted between our last try_recv
    // and their shutdown.
    while let Ok((order, _src)) = order_rx.try_recv() {
        if try_publish_order(order_prod, &order) {
            orders_sent += 1;
        } else {
            orders_dropped += 1;
        }
    }

    let elapsed = started.elapsed();
    eprintln!(
        "trading-engine: run finished after {:?} ({} ticks, {} dispatched-dropped, {} orders, {} order-drops)",
        elapsed, ticks_seen, ticks_dropped, orders_sent, orders_dropped,
    );

    let worker_dispatch_summaries: Vec<LatencySummary> =
        worker_stats.iter().map(|s| s.dispatch_lat.summary()).collect();

    finalise_report(args, host, noise_cpus, ticks_seen, ticks_dropped, orders_sent,
                    orders_dropped, elapsed, shm_transit, end_to_end,
                    worker_dispatch_summaries, noise_handle)
}

#[derive(Default)]
struct WorkerStats {
    ticks: u64,
    orders: u64,
    dispatch_lat: LatencyHistogram,
}

#[allow(clippy::too_many_arguments)]
fn worker_main(
    id: usize,
    rx: Receiver<WorkItem>,
    order_tx: Sender<(OrderMsg, &'static str)>,
    pin_cpu: Option<usize>,
    stop: Arc<AtomicBool>,
    alpha: f64,
    threshold_bps: f64,
    cooldown_ns: u64,
    qty: u32,
) -> WorkerStats {
    if let Some(c) = pin_cpu {
        if let Err(e) = cpu::pin_to_cpu(c) {
            eprintln!("worker-{id}: failed to pin cpu {c}: {e}");
        } else {
            eprintln!("worker-{id}: pinned to cpu {c}");
        }
    }
    let mut state: HashMap<[u8; shmbus::message::SYMBOL_LEN], SymbolState> = HashMap::new();
    let mut stats = WorkerStats::default();
    let mut next_order_id: u64 = (id as u64) << 32; // disjoint id space per worker
    while !stop.load(Ordering::Relaxed) {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(item) => {
                let dispatch_lat = now_ns().saturating_sub(item.enqueued_ns);
                stats.dispatch_lat.record(dispatch_lat);
                stats.ticks += 1;
                let (order_opt, _) = strategy_step(&item.tick, &mut state, alpha,
                                                   threshold_bps, cooldown_ns, qty,
                                                   &mut next_order_id);
                if let Some(order) = order_opt {
                    stats.orders += 1;
                    if order_tx.send((order, "worker")).is_err() {
                        return stats;
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }
    stats
}

#[inline]
fn idle_backoff(spin: &mut u32) {
    *spin = spin.saturating_add(1);
    if *spin < 64 {
        std::hint::spin_loop();
    } else if *spin < 1024 {
        std::thread::yield_now();
    } else {
        std::thread::sleep(Duration::from_micros(50));
    }
}

#[inline]
fn symbol_hash(s: &[u8; shmbus::message::SYMBOL_LEN]) -> u64 {
    // Tiny FNV-1a — fine for routing, not cryptographic.
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in s {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[allow(clippy::too_many_arguments)]
fn strategy_step(
    tick: &MarketTick,
    state: &mut HashMap<[u8; shmbus::message::SYMBOL_LEN], SymbolState>,
    alpha: f64,
    threshold_bps: f64,
    cooldown_ns: u64,
    qty: u32,
    next_order_id: &mut u64,
) -> (Option<OrderMsg>, f64) {
    let mid = (tick.bid_px as f64 + tick.ask_px as f64) * 0.5 / 1e6;
    let entry = state.entry(tick.symbol).or_default();
    if !entry.initialised {
        entry.ema_px = mid;
        entry.initialised = true;
        return (None, 0.0);
    }
    entry.ema_px = alpha * mid + (1.0 - alpha) * entry.ema_px;
    let dev_bps = (mid - entry.ema_px) / entry.ema_px * 10_000.0;
    let now = now_ns();
    if dev_bps.abs() < threshold_bps {
        return (None, dev_bps);
    }
    if now - entry.last_order_ns < cooldown_ns {
        return (None, dev_bps);
    }
    let side = if dev_bps > 0.0 { Side::Sell } else { Side::Buy };
    let px = if side == Side::Buy { tick.ask_px } else { tick.bid_px };
    *next_order_id += 1;
    entry.last_order_ns = now;
    let order = OrderMsg {
        order_id: *next_order_id,
        submit_ts_ns: now,
        symbol: tick.symbol,
        px,
        qty,
        side: side as u8,
        _pad: [0; 3],
    };
    (Some(order), dev_bps)
}

fn try_publish_order(prod: &mut shmbus::RingProducer, order: &OrderMsg) -> bool {
    let bytes = unsafe {
        std::slice::from_raw_parts(order as *const OrderMsg as *const u8, OrderMsg::SIZE)
    };
    prod.try_publish(bytes)
}

fn report_live(
    ticks: u64,
    orders: u64,
    pending: u64,
    shm_transit: Option<&LatencyHistogram>,
    end_to_end: Option<&LatencyHistogram>,
) {
    eprintln!(
        "trading-engine: stats ticks={} orders={} pending={}",
        ticks, orders, pending
    );
    if let Some(h) = shm_transit {
        if h.count() > 0 {
            eprintln!("trading-engine: shm-transit  {}", h.summary());
        }
    }
    if let Some(h) = end_to_end {
        if h.count() > 0 {
            eprintln!("trading-engine: end-to-end   {}", h.summary());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn finalise_report(
    args: Args,
    host: HostInfo,
    noise_cpus: Vec<usize>,
    ticks_seen: u64,
    ticks_dropped: u64,
    orders_sent: u64,
    orders_dropped: u64,
    elapsed: Duration,
    shm_transit: LatencyHistogram,
    end_to_end: LatencyHistogram,
    worker_dispatch: Vec<LatencySummary>,
    noise_handle: Option<noise::NoiseHandle>,
) -> Result<()> {
    let report = EngineReport {
        schema: "dummy-benchmark/trading-engine/v1",
        host,
        config: EngineConfig {
            binary: "trading-engine",
            pin_cpu: args.pin_cpu,
            workers: args.workers,
            worker_cpus: args.worker_cpus.clone(),
            noise_threads: args.noise_threads,
            noise_cpus: cpu::format_cpu_list(&noise_cpus),
            bench_secs: args.bench_secs,
            ema_halflife: args.ema_halflife,
            threshold_bps: args.threshold_bps,
            cooldown_ns: args.cooldown_ns,
            qty: args.qty,
            bus: args.bus.clone(),
        },
        metrics: EngineMetrics {
            ticks_consumed: ticks_seen,
            ticks_dropped_dispatch: ticks_dropped,
            orders_emitted: orders_sent,
            orders_dropped_ring_full: orders_dropped,
            elapsed_secs: elapsed.as_secs_f64(),
            throughput_ticks_per_sec: if elapsed.as_secs_f64() > 0.0 {
                ticks_seen as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            },
            shm_transit: shm_transit.summary(),
            end_to_end: end_to_end.summary(),
            worker_dispatch,
        },
    };

    if let Some(path) = args.bench_report.as_deref() {
        write_report(path, &report).with_context(|| format!("writing report to {path}"))?;
        eprintln!("trading-engine: wrote bench report to {path}");
    } else if args.bench_secs > 0 {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    if let Some(h) = noise_handle {
        h.shutdown();
    }
    Ok(())
}

fn ema_alpha(halflife_ticks: f64) -> f64 {
    if halflife_ticks <= 0.0 {
        1.0
    } else {
        1.0 - (-(2f64.ln()) / halflife_ticks).exp()
    }
}
