// Copyright 2026 Deno Land Inc. Apache-2.0 license.

use celld_logic::pressure::{Latches, Load, PressureConfig, SHED_RSS_HARD};

const MIB: u64 = 1024 * 1024;

fn load(rss: u64, in_use: u64, working_set: Option<u64>, current: Option<u64>) -> Load {
    Load {
        resident_cells: 2,
        rss_bytes: rss,
        in_use_bytes: in_use,
        cgroup_working_set_bytes: working_set,
        cgroup_current_bytes: current,
    }
}

#[test]
fn streamed_file_cache_does_not_latch_or_hold_admission() {
    // Recorded at the false hard-cap crossing during Radio's 1 GiB upload.
    let observed = load(
        271_724_544,
        252_496_976,
        Some(266_256_384),
        Some(1_073_500_160),
    );
    let config = PressureConfig::from_limits(Some(1024 * MIB), None);
    assert_eq!(observed.hard_bytes(), 271_724_544);
    assert_eq!(
        config.classify(observed, Latches::default()),
        (Latches::default(), None)
    );
    assert_eq!(
        config.classify(
            observed,
            Latches {
                memory: false,
                rss_hard: true
            }
        ),
        (Latches::default(), None)
    );
    assert!(config.has_headroom(observed));
}

#[test]
fn active_cgroup_charges_still_cross_the_hard_limit_outside_process_rss() {
    let config = PressureConfig::from_limits(Some(1024 * MIB), None);
    let active = load(256 * MIB, 192 * MIB, Some(1000 * MIB), Some(1024 * MIB));
    let (latches, reason) = config.classify(active, Latches::default());
    assert!(latches.rss_hard);
    assert_eq!(reason, Some(SHED_RSS_HARD));
    assert!(!config.has_headroom(active));
}

#[test]
fn allocator_retention_and_process_rss_remain_hard_floors() {
    let config = PressureConfig::from_limits(Some(1024 * MIB), None);
    // Even when the cgroup sample is lower, high RSS cannot be hidden by the
    // ordinary metric's allocator discount or differences in shared accounting.
    let retained = load(1000 * MIB, 128 * MIB, Some(300 * MIB), Some(1024 * MIB));
    let (latches, reason) = config.classify(retained, Latches::default());
    assert!(!latches.memory);
    assert!(latches.rss_hard);
    assert_eq!(reason, Some(SHED_RSS_HARD));
}

#[test]
fn missing_working_set_uses_raw_charge_and_non_cgroup_hosts_use_rss() {
    let config = PressureConfig::from_limits(Some(1024 * MIB), None);
    for sample in [
        load(256 * MIB, 192 * MIB, None, Some(1000 * MIB)),
        load(1000 * MIB, 192 * MIB, None, None),
    ] {
        assert_eq!(sample.hard_bytes(), 1000 * MIB);
        assert_eq!(
            config.classify(sample, Latches::default()).1,
            Some(SHED_RSS_HARD)
        );
    }
}
