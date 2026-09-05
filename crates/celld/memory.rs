// Copyright 2026 Deno Land Inc. Apache-2.0 license.

//! What the process holds, and how much of that a cell actually holds.
//!
//! Process RSS and cgroup memory answer different questions. RSS describes the
//! process, while `memory.current` is the complete charge that the kernel
//! constrains. `memory.stat` identifies inactive file pages that the kernel can
//! reclaim, so pressure measurements do not treat that cache as
//! a cell working set. [`sample`] obtains the measurements in one sampling
//! turn and keeps their relationship intact.

/// A memory sample from one sampling turn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub rss_bytes: u64,
    pub in_use_bytes: u64,
    pub cgroup_working_set_bytes: Option<u64>,
    pub cgroup_current_bytes: Option<u64>,
}

/// The resident set size and the memory a cell holds, together.
pub fn sample() -> Sample {
    let rss_bytes = resident_bytes();
    let cgroup = cgroup_memory();
    Sample {
        rss_bytes,
        in_use_bytes: rss_bytes.saturating_sub(allocator_slack_bytes()),
        cgroup_working_set_bytes: cgroup.map(|memory| memory.working_set_bytes),
        cgroup_current_bytes: cgroup.map(|memory| memory.current_bytes),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CgroupMemory {
    current_bytes: u64,
    working_set_bytes: u64,
}

#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // cgroup files are kernel telemetry.
fn cgroup_memory() -> Option<CgroupMemory> {
    for (current_path, stat_path, inactive_key) in [
        (
            "/sys/fs/cgroup/memory.current",
            "/sys/fs/cgroup/memory.stat",
            "inactive_file",
        ),
        (
            "/sys/fs/cgroup/memory/memory.usage_in_bytes",
            "/sys/fs/cgroup/memory/memory.stat",
            "total_inactive_file",
        ),
    ] {
        let Ok(current) = std::fs::read_to_string(current_path) else {
            continue;
        };
        let Some(current_bytes) = current.trim().parse::<u64>().ok() else {
            continue;
        };
        let stat = std::fs::read_to_string(stat_path).unwrap_or_default();
        return Some(cgroup_memory_from_stat(current_bytes, &stat, inactive_key));
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn cgroup_memory() -> Option<CgroupMemory> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn memory_stat_value(stat: &str, key: &str) -> Option<u64> {
    stat.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == key)
            .then(|| fields.next()?.parse::<u64>().ok())
            .flatten()
    })
}

#[cfg(any(target_os = "linux", test))]
fn cgroup_memory_from_stat(current_bytes: u64, stat: &str, inactive_key: &str) -> CgroupMemory {
    let inactive_file_bytes = memory_stat_value(stat, inactive_key)
        .or_else(|| {
            (inactive_key == "total_inactive_file")
                .then(|| memory_stat_value(stat, "inactive_file"))
                .flatten()
        })
        .unwrap_or_default();
    CgroupMemory {
        current_bytes,
        // The files are separate kernel snapshots. If the inactive charge
        // races above the earlier current charge, using zero would hide all
        // active memory from the pressure and rollout gates.
        working_set_bytes: current_bytes
            .checked_sub(inactive_file_bytes)
            .unwrap_or(current_bytes),
    }
}

/// The active memory-cgroup limit. `None` means that no finite cgroup limit is
/// readable, so the caller must use the host memory size.
#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // cgroup files are kernel telemetry.
pub(crate) fn cgroup_limit_bytes() -> Option<u64> {
    for path in [
        "/sys/fs/cgroup/memory.max",
        "/sys/fs/cgroup/memory/memory.limit_in_bytes",
    ] {
        if let Ok(raw) = std::fs::read_to_string(path) {
            if let Ok(limit) = raw.trim().parse::<u64>() {
                // cgroup v1 reports "no limit" as a huge page-rounded value.
                if limit < (1 << 50) {
                    return Some(limit);
                }
            }
        }
    }
    None
}

#[cfg(target_os = "linux")]
#[allow(clippy::disallowed_methods)] // `/proc` is host telemetry, not node storage.
pub fn resident_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/statm")
        .ok()
        .and_then(|statm| statm.split_whitespace().nth(1)?.parse::<u64>().ok())
        .map(|pages| pages.saturating_mul(page_size()))
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn page_size() -> u64 {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    u64::try_from(size).ok().filter(|s| *s > 0).unwrap_or(4_096)
}

#[cfg(target_os = "macos")]
pub fn resident_bytes() -> u64 {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_taskinfo>() as libc::c_int;
    let got = unsafe {
        libc::proc_pidinfo(
            // This samples the actual process RSS, so it needs the real host pid.
            libc::getpid(),
            libc::PROC_PIDTASKINFO,
            0,
            (&raw mut info).cast(),
            size,
        )
    };
    if got == size {
        info.pti_resident_size
    } else {
        0
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub fn resident_bytes() -> u64 {
    0
}

/// The pages jemalloc holds but nothing uses: `stats.resident` less
/// `stats.allocated`. The statistics are cached, so the epoch must advance
/// first. A failure gives zero, which makes the sample equal to RSS.
pub fn allocator_slack_bytes() -> u64 {
    if tikv_jemalloc_ctl::epoch::advance().is_err() {
        return 0;
    }
    let Ok(resident) = tikv_jemalloc_ctl::stats::resident::read() else {
        return 0;
    };
    let Ok(allocated) = tikv_jemalloc_ctl::stats::allocated::read() else {
        return 0;
    };
    (resident as u64).saturating_sub(allocated as u64)
}

/// Ask jemalloc to give freed pages back on a timer. Its 10-second decay runs
/// only when a thread next calls the allocator, which a node that just shed its
/// working set does not do. This repairs what RSS reports, not the decision.
///
/// macOS has no background thread, so the failure is expected there and is
/// logged rather than raised. It matters to an operator: without the thread,
/// retention is never purged, and the absolute cap in `PressureConfig` is the
/// only thing between the process and a kill by the operating system.
pub fn tune_allocator() {
    if let Err(error) = tikv_jemalloc_ctl::background_thread::write(true) {
        tracing::warn!(
            %error,
            "the allocator will not run a background thread, so freed pages \
             return only when a thread allocates again"
        );
    }
}

#[cfg(all(test, celld_internal_tests))]
mod internal_tests {
    include!(env!("CELLD_INTERNAL_MEMORY_TESTS"));
}

#[cfg(test)]
mod cgroup_stat_tests {
    use super::cgroup_memory_from_stat;

    #[test]
    fn inactive_file_deduction_keeps_anonymous_and_kernel_charges() {
        let memory = cgroup_memory_from_stat(
            1_073_741_824,
            "anon 281075712\nfile 760799232\nkernel 25444352\ninactive_file 750202880\nactive_file 10588160\n",
            "inactive_file",
        );
        assert_eq!(memory.current_bytes, 1_073_741_824);
        assert_eq!(memory.working_set_bytes, 323_538_944);
    }

    #[test]
    fn missing_invalid_or_racing_stats_keep_the_full_charge() {
        for stat in [
            "",
            "anon 900",
            "inactive_file invalid",
            "inactive_file 18446744073709551616",
            "inactive_file 1001",
        ] {
            let memory = cgroup_memory_from_stat(1000, stat, "inactive_file");
            assert_eq!(memory.working_set_bytes, 1000, "{stat:?}");
        }
    }

    #[test]
    fn legacy_cgroup_uses_hierarchical_inactive_file_with_local_fallback() {
        assert_eq!(
            cgroup_memory_from_stat(
                1000,
                "total_inactive_file 400\ninactive_file 100",
                "total_inactive_file"
            )
            .working_set_bytes,
            600
        );
        assert_eq!(
            cgroup_memory_from_stat(1000, "inactive_file 100", "total_inactive_file")
                .working_set_bytes,
            900
        );
    }
}
