//! Four real parts, and what the affinity policy makes of each (§5.4, §5.5, §16).
//!
//! # What was wrong with the sentence this replaces
//!
//! §5.4 read "decode workers pinned to little cores, big cores for orchestrator and Filament".
//! R1's measurement had to correct it: the RK3566 is four Cortex-A55s in one cluster and has no
//! big cores, so the sentence described an RK3588 and not the board. A frontend that runs on an
//! RK3566, an RK3588, a VisionFive 2 and a developer's workstation cannot hold a right answer
//! about cores, and every one of those parts already reports what it is.
//!
//! So the tests below are the parts, not the policy: the same policy is asked about each, and on
//! the one that started this it asks for nothing.

use std::collections::BTreeMap;

use tessella_orchestrate::topology::{Affinity, Topology, parse_cpu_list};

/// Builds the sysfs a capacity-reporting part would have.
fn capacities(present: &str, values: &[(u32, &str)]) -> Vec<(String, String)> {
    let mut files = alloc_files(present);
    for (id, capacity) in values {
        files.push((
            format!("/sys/devices/system/cpu/cpu{id}/cpu_capacity"),
            (*capacity).to_owned(),
        ));
    }
    files
}

fn alloc_files(present: &str) -> Vec<(String, String)> {
    vec![(
        "/sys/devices/system/cpu/present".to_owned(),
        present.to_owned(),
    )]
}

fn read(files: &[(String, String)]) -> impl FnMut(&str) -> Option<String> + '_ {
    let map: BTreeMap<&str, &str> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    move |path: &str| map.get(path).map(|text| (*text).to_owned())
}

/// RK3566: four A55s in one cluster. The part that started this.
#[test]
fn a_uniform_part_asks_for_no_pinning() {
    let files = capacities("0-3", &[(0, "1024"), (1, "1024"), (2, "1024"), (3, "1024")]);
    let topology = Topology::from_sysfs(read(&files)).expect("the cpu list reads");

    assert_eq!(topology.cpus().len(), 4);
    assert!(topology.is_uniform(), "four A55s in one cluster");
    assert_eq!(topology.tiers().len(), 1);
    assert!(
        Affinity::SpareTheLargest.decode_cpus(&topology).is_empty(),
        "there is nothing to spare them from, so the policy asks for nothing — which is the \
         whole of the correction §5.4 needed"
    );
    assert!(
        Affinity::SpareTheLargest
            .foreground_cpus(&topology)
            .is_empty(),
        "and reserves nothing"
    );
}

/// RK3588: four A76 and four A55.
#[test]
fn a_big_little_part_splits_where_the_kernel_says() {
    let files = capacities(
        "0-7",
        &[
            (0, "530"),
            (1, "530"),
            (2, "530"),
            (3, "530"),
            (4, "1024"),
            (5, "1024"),
            (6, "1024"),
            (7, "1024"),
        ],
    );
    let topology = Topology::from_sysfs(read(&files)).expect("the cpu list reads");

    assert!(!topology.is_uniform());
    let tiers = topology.tiers();
    assert_eq!(tiers.len(), 2);
    assert_eq!(tiers[0].capacity, 530, "smallest first");
    assert_eq!(tiers[1].capacity, 1024);

    assert_eq!(
        Affinity::SpareTheLargest.decode_cpus(&topology),
        vec![0, 1, 2, 3],
        "decode goes on the A55s"
    );
    assert_eq!(
        Affinity::SpareTheLargest.foreground_cpus(&topology),
        vec![4, 5, 6, 7],
        "and the A76s are left for the orchestrator and the renderer"
    );
}

/// A three-tier part: everything below the top is decode's, not merely the bottom tier.
#[test]
fn three_tiers_reserve_only_the_top() {
    let files = capacities(
        "0-5",
        &[
            (0, "256"),
            (1, "256"),
            (2, "620"),
            (3, "620"),
            (4, "1024"),
            (5, "1024"),
        ],
    );
    let topology = Topology::from_sysfs(read(&files)).expect("the cpu list reads");
    assert_eq!(topology.tiers().len(), 3);
    assert_eq!(
        Affinity::SpareTheLargest.decode_cpus(&topology),
        vec![0, 1, 2, 3],
        "two tiers' worth of cores are not the ones worth reserving"
    );
}

/// An x86 hybrid, which reports no capacity: frequency stands in.
#[test]
fn frequency_stands_in_where_capacity_is_absent() {
    let mut files = alloc_files("0-3");
    for (id, khz) in [
        (0, "5000000"),
        (1, "5000000"),
        (2, "3800000"),
        (3, "3800000"),
    ] {
        files.push((
            format!("/sys/devices/system/cpu/cpu{id}/cpufreq/cpuinfo_max_freq"),
            khz.to_owned(),
        ));
    }
    let topology = Topology::from_sysfs(read(&files)).expect("the cpu list reads");

    let tiers = topology.tiers();
    assert_eq!(tiers.len(), 2, "P and E cores are told apart");
    assert_eq!(
        tiers[1].capacity, 1024,
        "the largest is normalised to the top"
    );
    assert_eq!(tiers[1].cpus, vec![0, 1]);
    assert_eq!(
        Affinity::SpareTheLargest.decode_cpus(&topology),
        vec![2, 3],
        "decode goes on the E cores"
    );
}

/// A part that reports neither is one uniform tier, not an error.
#[test]
fn a_silent_part_is_uniform() {
    let files = alloc_files("0-15");
    let topology = Topology::from_sysfs(read(&files)).expect("the cpu list reads");
    assert_eq!(topology.cpus().len(), 16);
    assert!(
        topology.is_uniform(),
        "a machine that will not say otherwise is treated as saying they are the same"
    );
}

/// Capacity is preferred to frequency, not mixed with it.
///
/// A part reporting capacity for some cores and frequency for others would have the two on
/// incomparable scales, and the tiers would be an artefact of which file happened to exist.
#[test]
fn the_two_measures_are_not_mixed() {
    let mut files = capacities("0-3", &[(0, "1024"), (1, "1024")]);
    for id in 2..4u32 {
        files.push((
            format!("/sys/devices/system/cpu/cpu{id}/cpufreq/cpuinfo_max_freq"),
            "1800000".to_owned(),
        ));
    }
    let topology = Topology::from_sysfs(read(&files)).expect("the cpu list reads");
    let tiers = topology.tiers();
    assert_eq!(
        tiers.len(),
        2,
        "the two that answered on the frequency scale are one tier and the silent ones another"
    );
    assert!(
        tiers.iter().all(|tier| tier.capacity <= 1024),
        "and everything is on one scale, whichever it was: {tiers:?}"
    );
}

/// The default asks for nothing at all.
#[test]
fn the_default_leaves_the_scheduler_alone() {
    let files = capacities("0-1", &[(0, "512"), (1, "1024")]);
    let topology = Topology::from_sysfs(read(&files)).expect("the cpu list reads");
    assert!(!topology.is_uniform(), "there is a split to be had");
    assert!(
        Affinity::default().decode_cpus(&topology).is_empty(),
        "and the default declines it: a capacity-aware scheduler already moves work toward the \
         cores that suit it, and pinning against it is how a decode worker queues behind another \
         on a small core while a large one idles"
    );
}

/// Nothing at all is not a topology.
#[test]
fn an_unreadable_cpu_list_is_no_answer() {
    assert!(
        Topology::from_sysfs(read(&[])).is_none(),
        "a caller that cannot read the list should leave affinity alone rather than guess"
    );
    assert!(
        Topology::from_sysfs(read(&alloc_files(""))).is_none(),
        "and an empty one says nothing either"
    );
}

/// The list format, which is sysfs's and not ours.
#[test]
fn cpu_lists_parse_the_way_the_kernel_writes_them() {
    assert_eq!(parse_cpu_list("0-3\n"), vec![0, 1, 2, 3]);
    assert_eq!(parse_cpu_list("0,2,4"), vec![0, 2, 4]);
    assert_eq!(parse_cpu_list("0-1,4-7"), vec![0, 1, 4, 5, 6, 7]);
    assert_eq!(parse_cpu_list("3"), vec![3]);
    assert!(parse_cpu_list("").is_empty());
    assert!(
        parse_cpu_list("nonsense").is_empty(),
        "refused, not guessed"
    );
}

/// The paths, against the machine running the test.
///
/// Everything above is a fake sysfs, which proves the parsing and proves nothing about whether
/// the paths are the ones Linux actually uses — a typo in one of them would pass every test in
/// this file. So this reads the real thing where there is one, and asserts only what is true of
/// any machine: that the CPU list is readable, that it agrees with what the kernel says is
/// online, and that every core got a capacity on the 1024 scale.
#[test]
fn the_paths_are_the_ones_linux_uses() {
    let Some(present) = std::fs::read_to_string("/sys/devices/system/cpu/present").ok() else {
        // Not Linux, or a kernel without sysfs. Nothing to check.
        return;
    };

    let topology = Topology::from_sysfs(|path| std::fs::read_to_string(path).ok())
        .expect("a machine with a cpu list has a topology");

    assert_eq!(
        topology.cpus().len(),
        parse_cpu_list(&present).len(),
        "one entry per present cpu"
    );
    assert!(
        topology
            .cpus()
            .iter()
            .all(|cpu| cpu.capacity > 0 && cpu.capacity <= 1024),
        "every capacity is on the kernel's scale: {:?}",
        topology.cpus()
    );
    assert_eq!(
        topology.tiers().last().map(|tier| tier.capacity),
        Some(1024),
        "and the largest core is the top of it, whichever measure answered"
    );
}
