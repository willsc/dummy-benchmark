//! CPU affinity helpers and host topology probing.
//!
//! Bench output needs to be unambiguous about *which* machine produced it, so
//! we read enough of /proc and /sys to capture: vendor, model, microcode,
//! kernel, scaling governor, turbo state, NUMA topology, and cache sizes.
//! Anything we can't read degrades to an empty string / empty vec.

use std::fs;
use std::io;
use std::mem::{size_of, zeroed};
use std::num::ParseIntError;
use std::path::Path;

use libc::{cpu_set_t, sched_setaffinity, CPU_SET, CPU_ZERO};
use serde::Serialize;

/// Pin the calling thread to a single CPU.
pub fn pin_to_cpu(cpu: usize) -> io::Result<()> {
    pin_to_cpus(&[cpu])
}

/// Pin the calling thread to the given CPU set.
pub fn pin_to_cpus(cpus: &[usize]) -> io::Result<()> {
    if cpus.is_empty() {
        return Ok(());
    }
    // Safety: cpu_set_t is C POD; we zero, populate, and pass by pointer.
    unsafe {
        let mut set: cpu_set_t = zeroed();
        CPU_ZERO(&mut set);
        for &c in cpus {
            CPU_SET(c, &mut set);
        }
        if sched_setaffinity(0, size_of::<cpu_set_t>(), &set) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Parse a Linux-style CPU list ("0,1,3-5,8") into individual indices.
/// Empty input yields an empty vec.
pub fn parse_cpu_list(s: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            let lo = a.trim().parse::<usize>().map_err(stringify_err)?;
            let hi = b.trim().parse::<usize>().map_err(stringify_err)?;
            if hi < lo {
                return Err(format!("invalid range: {lo}-{hi}"));
            }
            out.extend(lo..=hi);
        } else {
            out.push(part.parse::<usize>().map_err(stringify_err)?);
        }
    }
    Ok(out)
}

fn stringify_err(e: ParseIntError) -> String {
    e.to_string()
}

/// Render a vector of CPU indices into Linux range syntax ("0,2-4,8").
pub fn format_cpu_list(cpus: &[usize]) -> String {
    if cpus.is_empty() {
        return String::new();
    }
    let mut sorted: Vec<usize> = cpus.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut j = i;
        while j + 1 < sorted.len() && sorted[j + 1] == sorted[j] + 1 {
            j += 1;
        }
        if !out.is_empty() {
            out.push(',');
        }
        if i == j {
            out.push_str(&start.to_string());
        } else {
            use std::fmt::Write;
            let _ = write!(out, "{}-{}", start, sorted[j]);
        }
        i = j + 1;
    }
    out
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CacheLevel {
    pub level: u32,
    pub kind: String,    // "Data", "Instruction", "Unified"
    pub size_kib: u64,
    pub line_size_b: u32,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct NumaNode {
    pub node: u32,
    pub cpus: String, // formatted CPU list
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct CpuInfo {
    pub vendor: String,
    pub model: String,
    pub microcode: String,
    pub kernel: String,
    pub cpu_mhz_nominal: f64,
    pub governor: String,
    pub turbo: TurboState,
    pub online_cpus: Vec<usize>,
    pub isolated_cpus: Vec<usize>,
    pub numa_nodes: Vec<NumaNode>,
    pub caches: Vec<CacheLevel>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub enum TurboState {
    #[default]
    Unknown,
    Enabled,
    Disabled,
}

pub fn read_cpu_info() -> CpuInfo {
    let mut info = CpuInfo::default();

    // /proc/cpuinfo for vendor, model name, microcode, MHz.
    if let Ok(s) = fs::read_to_string("/proc/cpuinfo") {
        for line in s.lines() {
            if info.vendor.is_empty() {
                if let Some(v) = extract_after_colon(line, "vendor_id") {
                    info.vendor = v;
                }
            }
            if info.model.is_empty() {
                if let Some(v) = extract_after_colon(line, "model name") {
                    info.model = v;
                }
            }
            if info.microcode.is_empty() {
                if let Some(v) = extract_after_colon(line, "microcode") {
                    info.microcode = v;
                }
            }
            if info.cpu_mhz_nominal == 0.0 {
                if let Some(v) = extract_after_colon(line, "cpu MHz") {
                    info.cpu_mhz_nominal = v.parse().unwrap_or(0.0);
                }
            }
            if !info.vendor.is_empty()
                && !info.model.is_empty()
                && !info.microcode.is_empty()
                && info.cpu_mhz_nominal > 0.0
            {
                break;
            }
        }
    }

    info.kernel = read_trim("/proc/sys/kernel/osrelease");
    info.governor = read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor");

    // Intel exposes /sys/devices/system/cpu/intel_pstate/no_turbo (1 = disabled).
    // AMD acpi-cpufreq exposes /sys/devices/system/cpu/cpufreq/boost (1 = enabled).
    info.turbo = read_turbo_state();

    info.online_cpus = parse_cpu_list(read_trim("/sys/devices/system/cpu/online").as_str())
        .unwrap_or_default();
    info.isolated_cpus = parse_cpu_list(read_trim("/sys/devices/system/cpu/isolated").as_str())
        .unwrap_or_default();

    info.numa_nodes = read_numa_nodes();
    info.caches = read_caches();

    info
}

fn extract_after_colon(line: &str, key: &str) -> Option<String> {
    let trimmed = line.trim_start();
    if !trimmed.starts_with(key) {
        return None;
    }
    let after_key = &trimmed[key.len()..];
    let mut chars = after_key.chars();
    while let Some(c) = chars.next() {
        if c == ':' {
            let rest: String = chars.collect();
            return Some(rest.trim().to_string());
        }
        if !c.is_whitespace() && c != '\t' {
            return None;
        }
    }
    None
}

fn read_trim(path: &str) -> String {
    fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn read_turbo_state() -> TurboState {
    // Intel pstate
    if let Ok(s) = fs::read_to_string("/sys/devices/system/cpu/intel_pstate/no_turbo") {
        return match s.trim() {
            "0" => TurboState::Enabled,
            "1" => TurboState::Disabled,
            _ => TurboState::Unknown,
        };
    }
    // AMD / acpi-cpufreq boost
    if let Ok(s) = fs::read_to_string("/sys/devices/system/cpu/cpufreq/boost") {
        return match s.trim() {
            "1" => TurboState::Enabled,
            "0" => TurboState::Disabled,
            _ => TurboState::Unknown,
        };
    }
    TurboState::Unknown
}

fn read_numa_nodes() -> Vec<NumaNode> {
    let root = Path::new("/sys/devices/system/node");
    let mut out = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix("node") {
            if let Ok(node_id) = rest.parse::<u32>() {
                let cpulist_path = entry.path().join("cpulist");
                if let Ok(s) = fs::read_to_string(&cpulist_path) {
                    let cpus = s.trim().to_string();
                    out.push(NumaNode { node: node_id, cpus });
                }
            }
        }
    }
    out.sort_by_key(|n| n.node);
    out
}

fn read_caches() -> Vec<CacheLevel> {
    // Read cpu0's cache topology — assume homogeneous.
    let root = Path::new("/sys/devices/system/cpu/cpu0/cache");
    let mut out = Vec::new();
    let entries = match fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let level = match fs::read_to_string(path.join("level")) {
            Ok(s) => s.trim().parse::<u32>().unwrap_or(0),
            Err(_) => continue,
        };
        let kind = fs::read_to_string(path.join("type"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let size_kib = fs::read_to_string(path.join("size"))
            .ok()
            .and_then(parse_size_kib)
            .unwrap_or(0);
        let line_size_b = fs::read_to_string(path.join("coherency_line_size"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
            .unwrap_or(0);
        out.push(CacheLevel { level, kind, size_kib, line_size_b });
    }
    out.sort_by(|a, b| (a.level, a.kind.clone()).cmp(&(b.level, b.kind.clone())));
    out
}

fn parse_size_kib(s: String) -> Option<u64> {
    let t = s.trim();
    if let Some(num) = t.strip_suffix('K') {
        num.parse().ok()
    } else if let Some(num) = t.strip_suffix('M') {
        num.parse::<u64>().ok().map(|n| n * 1024)
    } else {
        t.parse().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_list() {
        assert_eq!(parse_cpu_list("").unwrap(), vec![] as Vec<usize>);
        assert_eq!(parse_cpu_list("0,1,2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_cpu_list("0-2").unwrap(), vec![0, 1, 2]);
        assert_eq!(parse_cpu_list(" 0, 2-4 , 8 ").unwrap(), vec![0, 2, 3, 4, 8]);
    }

    #[test]
    fn rejects_inverted_range() {
        assert!(parse_cpu_list("5-3").is_err());
    }

    #[test]
    fn format_collapses_ranges() {
        assert_eq!(format_cpu_list(&[]), "");
        assert_eq!(format_cpu_list(&[3, 1, 2, 0]), "0-3");
        assert_eq!(format_cpu_list(&[0, 1, 3, 5, 6, 7]), "0-1,3,5-7");
    }

    #[test]
    fn extract_after_colon_handles_tabs() {
        assert_eq!(
            extract_after_colon("vendor_id\t: GenuineIntel", "vendor_id"),
            Some("GenuineIntel".to_string())
        );
        assert_eq!(extract_after_colon("flags : fpu", "vendor_id"), None);
    }
}
