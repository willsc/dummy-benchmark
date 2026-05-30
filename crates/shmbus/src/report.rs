//! Serializable scaffolding for the bench report each binary writes on exit.
//!
//! Each binary defines its own top-level `Report` struct that embeds
//! `HostInfo` and a binary-specific config + metrics block. We just provide
//! the common pieces and a small helper to write the JSON.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::cpu::{self, CpuInfo};

#[derive(Debug, Clone, Serialize)]
pub struct HostInfo {
    pub hostname: String,
    pub kernel: String,
    pub cpu_vendor: String,
    pub cpu_model: String,
    pub microcode: String,
    pub cpu_mhz_nominal: f64,
    pub governor: String,
    pub turbo: cpu::TurboState,
    pub online_cpus: String,
    pub isolated_cpus: String,
    pub numa_nodes: Vec<cpu::NumaNode>,
    pub caches: Vec<cpu::CacheLevel>,
}

pub fn host_info() -> HostInfo {
    let info: CpuInfo = cpu::read_cpu_info();
    HostInfo {
        hostname: read_hostname(),
        kernel: info.kernel,
        cpu_vendor: info.vendor,
        cpu_model: info.model,
        microcode: info.microcode,
        cpu_mhz_nominal: info.cpu_mhz_nominal,
        governor: info.governor,
        turbo: info.turbo,
        online_cpus: cpu::format_cpu_list(&info.online_cpus),
        isolated_cpus: cpu::format_cpu_list(&info.isolated_cpus),
        numa_nodes: info.numa_nodes,
        caches: info.caches,
    }
}

fn read_hostname() -> String {
    fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Common configuration knobs that every binary records in its report so
/// scenarios are reproducible from the JSON alone.
#[derive(Debug, Clone, Serialize)]
pub struct RunConfig {
    pub binary: String,
    pub pin_cpu: Option<usize>,
    pub noise_threads: usize,
    pub noise_cpus: String,
    pub bench_secs: u64,
    /// Engine-only: number of strategy workers. Other binaries report `None`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<usize>,
    /// Engine-only: CPU list each worker is pinned to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_cpus: Option<String>,
    pub started_unix: u64,
}

impl RunConfig {
    pub fn new(binary: &str, bench_secs: u64) -> Self {
        Self {
            binary: binary.to_string(),
            pin_cpu: None,
            noise_threads: 0,
            noise_cpus: String::new(),
            bench_secs,
            workers: None,
            worker_cpus: None,
            started_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        }
    }
}

/// Write any serializable report to `path` (parent dirs are *not* created).
pub fn write_report<P: AsRef<Path>, T: Serialize>(path: P, report: &T) -> io::Result<()> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    fs::write(path, json)
}
