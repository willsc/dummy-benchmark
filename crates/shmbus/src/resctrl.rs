//! Intel RDT / AMD CAT helpers.
//!
//! `resctrl` is the kernel's filesystem-style interface to Intel Cache
//! Allocation Technology and the AMD equivalent. A "group" is a directory
//! under `/sys/fs/resctrl/`; its `schemata` file pins L3 cache ways (and
//! optionally memory bandwidth), and writing a TID into `tasks` migrates
//! that thread into the group.
//!
//! The provisioning (creating groups, writing schemata, chmod-ing `tasks`)
//! is done by `scripts/cache-alloc.sh`. This module only handles the
//! per-thread join step — each pinned thread (main, workers, noise) calls
//! `join_group` after `pin_to_cpu` so the kernel can apply the right CBM
//! from the moment that thread touches memory.

use std::fs;
use std::io;
use std::path::PathBuf;

const RESCTRL_ROOT: &str = "/sys/fs/resctrl";

/// Move the *calling thread* into the named resctrl group.
///
/// Writes the current TID (not PID — resctrl assigns per-thread) to
/// `/sys/fs/resctrl/<group>/tasks`. An empty `group` name is a no-op so the
/// CLI flag can stay optional.
///
/// Returns `Ok(())` if resctrl isn't mounted (`NotFound`) or the group
/// doesn't exist *and* `allow_missing` is true — production deployments
/// should reject silent fallbacks, so callers set `allow_missing=false`
/// during bench runs and `=true` for sanity-only invocations.
pub fn join_group(group: &str, allow_missing: bool) -> io::Result<()> {
    if group.is_empty() {
        return Ok(());
    }
    let tasks_path = PathBuf::from(RESCTRL_ROOT).join(group).join("tasks");
    if !tasks_path.exists() {
        if allow_missing {
            return Ok(());
        }
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("resctrl group {:?} does not exist (run scripts/cache-alloc.sh)", group),
        ));
    }
    let tid = gettid();
    fs::write(&tasks_path, format!("{}", tid)).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("writing tid={tid} to {}: {e}", tasks_path.display()),
        )
    })
}

/// Is resctrl mounted at the expected path?
pub fn is_available() -> bool {
    PathBuf::from(RESCTRL_ROOT).join("schemata").exists()
}

/// Read and return the schemata that the calling thread is currently bound
/// to (for sanity-logging at startup).
pub fn current_schemata(group: &str) -> io::Result<String> {
    let path = if group.is_empty() {
        PathBuf::from(RESCTRL_ROOT).join("schemata")
    } else {
        PathBuf::from(RESCTRL_ROOT).join(group).join("schemata")
    };
    fs::read_to_string(path).map(|s| s.trim().to_string())
}

#[inline]
fn gettid() -> libc::pid_t {
    // Safety: gettid() is always safe to call on Linux.
    unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t }
}
