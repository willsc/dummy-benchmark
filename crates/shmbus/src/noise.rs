//! Background noise threads — busy compute loops used to simulate competing
//! workloads on non-isolated cores. Each thread can be pinned to a specific
//! CPU; if `pin_cpus` is empty, the scheduler is free to place them anywhere.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use crate::cpu;

pub struct NoiseHandle {
    threads: Vec<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl NoiseHandle {
    pub fn shutdown(self) {
        self.stop.store(true, Ordering::SeqCst);
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// Spawn `n_threads` noise workers, round-robin pinned to `pin_cpus`.
pub fn spawn_noise(n_threads: usize, pin_cpus: &[usize]) -> NoiseHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let pinned: Vec<usize> = pin_cpus.to_vec();
    let mut threads = Vec::with_capacity(n_threads);
    for i in 0..n_threads {
        let stop_c = stop.clone();
        let pin = if pinned.is_empty() {
            None
        } else {
            Some(pinned[i % pinned.len()])
        };
        let h = thread::Builder::new()
            .name(format!("noise-{i}"))
            .spawn(move || noise_worker(stop_c, pin))
            .expect("failed to spawn noise thread");
        threads.push(h);
    }
    NoiseHandle { threads, stop }
}

fn noise_worker(stop: Arc<AtomicBool>, pin_cpu: Option<usize>) {
    if let Some(c) = pin_cpu {
        if let Err(e) = cpu::pin_to_cpu(c) {
            eprintln!("noise: failed to pin to cpu {c}: {e}");
        }
    }
    // A 1 MiB scratch keeps an L2-sized working set hot — this is what causes
    // the most realistic interference with cache-sensitive hot paths.
    let mut buf: Vec<u64> = vec![0; 128 * 1024];
    let mut x: u64 = 0xCAFE_BABE_DEAD_BEEF;
    while !stop.load(Ordering::Relaxed) {
        for _ in 0..256 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            x ^= x >> 17;
            let idx = (x as usize) & (buf.len() - 1);
            // Read-modify-write to dirty cachelines.
            buf[idx] = buf[idx].wrapping_add(x);
        }
        std::hint::black_box(&buf);
        std::hint::black_box(x);
    }
}
