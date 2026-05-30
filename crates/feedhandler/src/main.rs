//! Market-data feedhandler.
//!
//! * Receives synthetic ticks over UDP using io_uring (batched `Recv` SQEs).
//! * Decodes the fixed-layout `MarketTick` and publishes into the `ticks`
//!   shared-memory ring buffer.
//! * Drains the `orders` ring buffer back from the trading engine and logs
//!   each fill request.
//! * Tracks exchange-to-feedhandler wire latency in a log-linear histogram
//!   and emits a structured JSON report on exit for benchmark comparison.

use std::mem::MaybeUninit;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use io_uring::{opcode, types, IoUring};
use serde::Serialize;

use shmbus::{
    cpu, host_info, noise, resctrl, write_report, HostInfo, LatencyHistogram, LatencySummary,
    MarketTick, OrderMsg, ShmBus, SLOT_PAYLOAD,
};

/// Number of in-flight `recv` SQEs we keep posted at all times.
const RECV_BATCH: usize = 32;
const BUF_SIZE: usize = 128;
/// Sentinel user_data on the periodic Timeout SQE.
const TIMEOUT_UD: u64 = u64::MAX;

#[derive(Parser, Debug)]
#[command(about = "Market data feedhandler (io_uring UDP -> shared memory)")]
struct Args {
    /// UDP address to bind for incoming market data.
    #[arg(long, default_value = "127.0.0.1:9001")]
    bind: SocketAddr,
    /// Path to the shared-memory bus file.
    #[arg(long, default_value = "/tmp/shmbus.bin")]
    bus: String,
    /// io_uring queue depth.
    #[arg(long, default_value_t = 256)]
    ring_depth: u32,
    /// Pin the main thread to this CPU.
    #[arg(long)]
    pin_cpu: Option<usize>,
    /// Number of busy-loop noise threads to spawn.
    #[arg(long, default_value_t = 0)]
    noise_threads: usize,
    /// CPU list to round-robin pin noise threads to (e.g. "0-3,8").
    #[arg(long, default_value = "")]
    noise_cpus: String,
    /// resctrl group to join (main thread). Provision via scripts/cache-alloc.sh.
    #[arg(long, default_value = "")]
    resctrl_group: String,
    /// resctrl group for noise threads (typically a more restrictive CBM).
    #[arg(long, default_value = "")]
    noise_resctrl_group: String,
    /// Run for N seconds then exit (0 = run until ctrl-c).
    #[arg(long, default_value_t = 0)]
    bench_secs: u64,
    /// Write the JSON bench report to this path on exit.
    #[arg(long)]
    bench_report: Option<String>,
    /// Suppress per-event log spam; only emit the periodic stats line.
    #[arg(long, default_value_t = false)]
    quiet: bool,
}

#[derive(Serialize)]
struct FeedhandlerReport {
    schema: &'static str,
    host: HostInfo,
    config: FeedhandlerConfig,
    metrics: FeedhandlerMetrics,
}

#[derive(Serialize)]
struct FeedhandlerConfig {
    binary: &'static str,
    pin_cpu: Option<usize>,
    noise_threads: usize,
    noise_cpus: String,
    resctrl_group: String,
    noise_resctrl_group: String,
    resctrl_available: bool,
    schemata: String,
    bench_secs: u64,
    bind: String,
    bus: String,
    ring_depth: u32,
    recv_batch: usize,
}

#[derive(Serialize)]
struct FeedhandlerMetrics {
    ticks_published: u64,
    publish_drops: u64,
    bad_packets: u64,
    orders_received: u64,
    elapsed_secs: f64,
    throughput_ticks_per_sec: f64,
    wire_latency: LatencySummary,
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
        eprintln!("feedhandler: pinned main thread to cpu {c}");
    }
    if !args.resctrl_group.is_empty() {
        resctrl::join_group(&args.resctrl_group, false)
            .with_context(|| format!("joining resctrl group {}", args.resctrl_group))?;
        eprintln!(
            "feedhandler: joined resctrl group {} (schemata: {})",
            args.resctrl_group,
            resctrl::current_schemata(&args.resctrl_group).unwrap_or_default()
        );
    }

    let host = host_info();
    eprintln!(
        "feedhandler: host {} ({}) kernel={} governor={} isolated=[{}]",
        host.cpu_model, host.cpu_vendor, host.kernel, host.governor, host.isolated_cpus,
    );

    let noise_cpus = cpu::parse_cpu_list(&args.noise_cpus)
        .map_err(|e| anyhow!("--noise-cpus: {e}"))?;
    let noise_handle = if args.noise_threads > 0 {
        eprintln!(
            "feedhandler: spawning {} noise threads on [{}] (resctrl={})",
            args.noise_threads,
            cpu::format_cpu_list(&noise_cpus),
            if args.noise_resctrl_group.is_empty() {
                "<none>"
            } else {
                &args.noise_resctrl_group
            }
        );
        Some(noise::spawn_noise(
            args.noise_threads,
            &noise_cpus,
            &args.noise_resctrl_group,
        ))
    } else {
        None
    };

    let bus = ShmBus::open_or_create(&args.bus)
        .with_context(|| format!("opening shm bus at {}", args.bus))?;
    // Safety: this binary is the sole producer of ticks and sole consumer of orders.
    let mut tick_prod = unsafe { bus.ticks_producer() };
    let mut order_cons = unsafe { bus.orders_consumer() };

    let sock = UdpSocket::bind(args.bind)
        .with_context(|| format!("binding udp socket on {}", args.bind))?;
    sock.set_nonblocking(true)?;
    eprintln!("feedhandler: listening udp on {}", args.bind);
    eprintln!("feedhandler: shm bus  {}", args.bus);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_h = stop.clone();
    ctrlc::set_handler(move || stop_h.store(true, Ordering::SeqCst)).ok();

    let mut ring: IoUring = IoUring::builder()
        .build(args.ring_depth)
        .context("creating io_uring")?;

    let mut buffers: Vec<[u8; BUF_SIZE]> = (0..RECV_BATCH).map(|_| [0u8; BUF_SIZE]).collect();
    let sock_fd = sock.as_raw_fd();

    for idx in 0..RECV_BATCH {
        submit_recv(&mut ring, sock_fd, &mut buffers[idx], idx as u64)?;
    }

    let mut last_report = Instant::now();
    let started = Instant::now();
    let deadline = if args.bench_secs > 0 {
        Some(started + Duration::from_secs(args.bench_secs))
    } else {
        None
    };

    let mut recv_count: u64 = 0;
    let mut publish_drops: u64 = 0;
    let mut bad_packets: u64 = 0;
    let mut order_count: u64 = 0;

    // Cumulative wire-latency histogram; serialised verbatim into the report.
    let mut wire_lat = LatencyHistogram::new();

    let mut order_buf = [0u8; SLOT_PAYLOAD];
    let timeout_ts = types::Timespec::new().sec(0).nsec(5_000_000);

    while !stop.load(Ordering::Relaxed) {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }

        let timeout_sqe = opcode::Timeout::new(&timeout_ts as *const _)
            .build()
            .user_data(TIMEOUT_UD);
        {
            let mut sq = ring.submission();
            let _ = unsafe { sq.push(&timeout_sqe) };
        }

        match ring.submit_and_wait(1) {
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
            Err(e) => return Err(anyhow!("submit_and_wait: {e}")),
        }

        let mut done: Vec<(usize, i32)> = Vec::with_capacity(RECV_BATCH);
        {
            let mut cq = ring.completion();
            cq.sync();
            for cqe in &mut cq {
                if cqe.user_data() == TIMEOUT_UD {
                    continue;
                }
                done.push((cqe.user_data() as usize, cqe.result()));
            }
        }

        for (idx, res) in done.drain(..) {
            if res < 0 {
                eprintln!("feedhandler: recv error errno={}", -res);
            } else if (res as usize) < std::mem::size_of::<MarketTick>() {
                bad_packets += 1;
            } else if let Some(mut t) = parse_tick(&buffers[idx][..res as usize]) {
                t.recv_ts_ns = now_ns();
                let lat_ns = t.recv_ts_ns.saturating_sub(t.exch_ts_ns);
                wire_lat.record(lat_ns);
                let bytes = unsafe {
                    std::slice::from_raw_parts(
                        &t as *const MarketTick as *const u8,
                        MarketTick::SIZE,
                    )
                };
                if !tick_prod.try_publish(bytes) {
                    publish_drops += 1;
                } else {
                    recv_count += 1;
                }
            } else {
                bad_packets += 1;
            }
            submit_recv(&mut ring, sock_fd, &mut buffers[idx], idx as u64)?;
        }

        while order_cons.try_consume(&mut order_buf).is_some() {
            let order = unsafe { std::ptr::read(order_buf.as_ptr() as *const OrderMsg) };
            order_count += 1;
            if !args.quiet {
                eprintln!("feedhandler: <- order {}", order);
            }
        }

        if last_report.elapsed() >= Duration::from_secs(2) {
            eprintln!(
                "feedhandler: stats ticks_published={} drops={} bad={} orders_seen={}",
                recv_count, publish_drops, bad_packets, order_count,
            );
            if wire_lat.count() > 0 {
                eprintln!("feedhandler: wire-latency {}", wire_lat.summary());
            }
            last_report = Instant::now();
        }
    }

    let elapsed = started.elapsed();
    eprintln!(
        "feedhandler: run finished after {:?} ({} ticks, {} drops, {} orders)",
        elapsed, recv_count, publish_drops, order_count,
    );

    let report = FeedhandlerReport {
        schema: "dummy-benchmark/feedhandler/v1",
        host,
        config: FeedhandlerConfig {
            binary: "feedhandler",
            pin_cpu: args.pin_cpu,
            noise_threads: args.noise_threads,
            noise_cpus: cpu::format_cpu_list(&noise_cpus),
            resctrl_group: args.resctrl_group.clone(),
            noise_resctrl_group: args.noise_resctrl_group.clone(),
            resctrl_available: resctrl::is_available(),
            schemata: if args.resctrl_group.is_empty() {
                String::new()
            } else {
                resctrl::current_schemata(&args.resctrl_group).unwrap_or_default()
            },
            bench_secs: args.bench_secs,
            bind: args.bind.to_string(),
            bus: args.bus.clone(),
            ring_depth: args.ring_depth,
            recv_batch: RECV_BATCH,
        },
        metrics: FeedhandlerMetrics {
            ticks_published: recv_count,
            publish_drops,
            bad_packets,
            orders_received: order_count,
            elapsed_secs: elapsed.as_secs_f64(),
            throughput_ticks_per_sec: if elapsed.as_secs_f64() > 0.0 {
                recv_count as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            },
            wire_latency: wire_lat.summary(),
        },
    };

    if let Some(path) = args.bench_report.as_deref() {
        write_report(path, &report).with_context(|| format!("writing report to {path}"))?;
        eprintln!("feedhandler: wrote bench report to {path}");
    } else if args.bench_secs > 0 {
        println!("{}", serde_json::to_string_pretty(&report)?);
    }

    if let Some(h) = noise_handle {
        h.shutdown();
    }
    Ok(())
}

fn submit_recv(
    ring: &mut IoUring,
    fd: i32,
    buf: &mut [u8; BUF_SIZE],
    user_data: u64,
) -> Result<()> {
    let sqe = opcode::Recv::new(types::Fd(fd), buf.as_mut_ptr(), buf.len() as u32)
        .build()
        .user_data(user_data);
    loop {
        let mut sq = ring.submission();
        match unsafe { sq.push(&sqe) } {
            Ok(()) => {
                drop(sq);
                ring.submit().context("ring submit")?;
                return Ok(());
            }
            Err(_) => {
                drop(sq);
                ring.submit().context("ring submit (full)")?;
            }
        }
    }
}

fn parse_tick(bytes: &[u8]) -> Option<MarketTick> {
    if bytes.len() < MarketTick::SIZE {
        return None;
    }
    let mut tick = MaybeUninit::<MarketTick>::uninit();
    unsafe {
        std::ptr::copy_nonoverlapping(
            bytes.as_ptr(),
            tick.as_mut_ptr() as *mut u8,
            MarketTick::SIZE,
        );
        Some(tick.assume_init())
    }
}
