//! Synthetic market-data publisher.
//!
//! Emits `MarketTick` packets over UDP at a configurable rate. Each symbol
//! follows a mean-reverting random walk so the trading engine sees real
//! deviations to trade against.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use rand::Rng;

use shmbus::message::SYMBOL_LEN;
use shmbus::{cpu, noise, MarketTick};

#[derive(Parser, Debug)]
#[command(about = "Synthetic market data publisher (UDP)")]
struct Args {
    /// Destination address to send ticks to.
    #[arg(long, default_value = "127.0.0.1:9001")]
    target: SocketAddr,
    /// Symbols to publish. Comma-separated. Max 8 chars each.
    #[arg(long, default_value = "AAPL,MSFT,GOOG,NVDA")]
    symbols: String,
    /// Ticks per second per symbol.
    #[arg(long, default_value_t = 1_000)]
    rate: u32,
    /// Total ticks to publish before exiting (0 = run forever).
    #[arg(long, default_value_t = 0)]
    count: u64,
    /// Pin the main thread to this CPU.
    #[arg(long)]
    pin_cpu: Option<usize>,
    /// Number of busy-loop noise threads to spawn.
    #[arg(long, default_value_t = 0)]
    noise_threads: usize,
    /// CPU list for noise threads (e.g. "0-3,8").
    #[arg(long, default_value = "")]
    noise_cpus: String,
    /// Run for N seconds then exit (0 = run until count or ctrl-c).
    #[arg(long, default_value_t = 0)]
    bench_secs: u64,
}

struct SymState {
    name: [u8; SYMBOL_LEN],
    anchor: f64,
    mid: f64,
}

fn pack_symbol(s: &str) -> [u8; SYMBOL_LEN] {
    let mut out = [0u8; SYMBOL_LEN];
    let bytes = s.as_bytes();
    let n = bytes.len().min(SYMBOL_LEN);
    out[..n].copy_from_slice(&bytes[..n]);
    out
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
        eprintln!("mock-exchange: pinned main thread to cpu {c}");
    }

    let noise_cpus = cpu::parse_cpu_list(&args.noise_cpus)
        .map_err(|e| anyhow!("--noise-cpus: {e}"))?;
    let noise_handle = if args.noise_threads > 0 {
        eprintln!(
            "mock-exchange: spawning {} noise threads on [{}]",
            args.noise_threads,
            cpu::format_cpu_list(&noise_cpus)
        );
        Some(noise::spawn_noise(args.noise_threads, &noise_cpus))
    } else {
        None
    };

    let mut rng = rand::thread_rng();
    let mut symbols: Vec<SymState> = args
        .symbols
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|s| {
            let anchor = 50.0 + rng.gen::<f64>() * 450.0;
            SymState { name: pack_symbol(s.trim()), anchor, mid: anchor }
        })
        .collect();
    if symbols.is_empty() {
        anyhow::bail!("no symbols specified");
    }

    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.connect(args.target)
        .with_context(|| format!("connecting udp to {}", args.target))?;
    eprintln!(
        "mock-exchange: publishing {} symbols at {} t/s/sym to {}",
        symbols.len(),
        args.rate,
        args.target
    );

    let stop = Arc::new(AtomicBool::new(false));
    let stop_h = stop.clone();
    ctrlc::set_handler(move || stop_h.store(true, Ordering::SeqCst)).ok();

    let per_tick_dur = Duration::from_secs_f64(1.0 / args.rate as f64);
    let mut seq: u64 = 0;
    let mut sent_total: u64 = 0;
    let start = Instant::now();
    let deadline = if args.bench_secs > 0 {
        Some(start + Duration::from_secs(args.bench_secs))
    } else {
        None
    };
    let mut next_due = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        if let Some(d) = deadline {
            if Instant::now() >= d {
                break;
            }
        }
        for sym in symbols.iter_mut() {
            // Mean-reverting OU-ish step.
            let drift = (sym.anchor - sym.mid) * 0.001;
            let jitter = (rng.gen::<f64>() - 0.5) * 0.05;
            sym.mid = (sym.mid + drift + jitter).max(0.01);

            let spread_half = (sym.mid * 0.00005).max(0.005);
            let bid = sym.mid - spread_half;
            let ask = sym.mid + spread_half;
            let ts = now_ns();
            seq += 1;
            let tick = MarketTick {
                seq_num: seq,
                exch_ts_ns: ts,
                recv_ts_ns: 0,
                symbol: sym.name,
                bid_px: (bid * 1e6) as i64,
                ask_px: (ask * 1e6) as i64,
                bid_sz: 100 + rng.gen_range(0..900),
                ask_sz: 100 + rng.gen_range(0..900),
                _pad: 0,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    &tick as *const MarketTick as *const u8,
                    MarketTick::SIZE,
                )
            };
            // Drop on EAGAIN — this is best-effort, like real multicast.
            let _ = sock.send(bytes);
            sent_total += 1;
            if args.count > 0 && sent_total >= args.count {
                eprintln!(
                    "mock-exchange: published {} ticks in {:?}",
                    sent_total,
                    start.elapsed()
                );
                return Ok(());
            }
        }
        next_due += per_tick_dur;
        let now = Instant::now();
        if next_due > now {
            std::thread::sleep(next_due - now);
        } else if now - next_due > Duration::from_millis(50) {
            // Pacing fell behind — resync so we don't try to "catch up" forever.
            next_due = now;
        }
    }
    eprintln!(
        "mock-exchange: stopped after {} ticks ({:?} elapsed)",
        sent_total,
        start.elapsed()
    );
    if let Some(h) = noise_handle {
        h.shutdown();
    }
    Ok(())
}
