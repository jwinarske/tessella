//! What the cores actually are, and what to do about it (§5.4, §5.5, §16).
//!
//! # Why this is not a constant
//!
//! §5.4 said "decode workers pinned to little cores, big cores for orchestrator and Filament",
//! and R1's measurement had to correct it: an RK3566 is four Cortex-A55s in one cluster and has
//! no big cores, so the sentence describes an RK3588 and not the board. The correction is not to
//! write a different sentence. A frontend that runs on an RK3566, an RK3588, a VisionFive 2 and
//! a developer's workstation cannot have a right answer baked into it, and every one of those
//! parts already reports what it is.
//!
//! §16 asks whether affinity should be explicit pinning or scheduler hints, per target. Asking
//! the part turns that from a question per target into one policy: pin when there is something
//! to pin to, and otherwise leave the scheduler alone — which is the same answer on a part with
//! one tier as writing no policy at all, arrived at by measurement rather than by assertion.
//!
//! # Where the number comes from
//!
//! `cpu_capacity`, which the kernel computes and the scheduler itself uses: a value out of 1024
//! derived on arm64 from the device tree's `capacity-dmips-mhz`, present exactly where
//! capacity-aware scheduling is. Asking the same source the scheduler asks is the difference
//! between a policy that agrees with the scheduler and one that fights it.
//!
//! Where it is absent — x86, including hybrid parts with P and E cores — `cpufreq/
//! cpuinfo_max_freq` stands in, normalised so the largest core is 1024. That is a worse
//! measure, because frequency is not throughput across microarchitectures, but it separates
//! tiers on the parts where it has to and it is what is there.
//!
//! # Why no file is opened here
//!
//! This crate is `no_std` and has no business growing an I/O dependency for four reads. Every
//! path and every parse is here; the caller supplies the bytes. That also makes a part testable
//! without owning one — the cases below are an RK3566, an RK3588, an Intel hybrid and a
//! uniform server, none of which is the machine running the tests.
//!
//! # What this does not do
//!
//! Pin anything. Applying an affinity is `sched_setaffinity`, which is a syscall, and this crate
//! is `deny(unsafe_code)` with no allowance and no libc. The policy says which CPUs a class of
//! work wants; the embedder, which already owns thread creation, is where that becomes a call.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

/// One CPU as the kernel describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cpu {
    /// Its number, as in `cpuN`.
    pub id: u32,
    /// Capacity out of 1024, the scheduler's own scale.
    pub capacity: u32,
}

/// The cores this part has.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Topology {
    cpus: Vec<Cpu>,
}

/// A group of cores of equal capacity, which is what "big" and "little" actually mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tier {
    /// Capacity out of 1024.
    pub capacity: u32,
    /// The CPUs in it, ascending.
    pub cpus: Vec<u32>,
}

impl Topology {
    /// A topology from CPUs already known.
    #[must_use]
    pub fn new(mut cpus: Vec<Cpu>) -> Self {
        cpus.sort_unstable();
        cpus.dedup_by_key(|cpu| cpu.id);
        Self { cpus }
    }

    /// Reads the topology from sysfs, through a caller-supplied reader.
    ///
    /// `read` is given an absolute sysfs path and returns its contents, or `None` if it does not
    /// exist — `|path| std::fs::read_to_string(path).ok()` on Linux. Absent files are the normal
    /// case rather than an error: `cpu_capacity` exists only where capacity-aware scheduling
    /// does, and a part with neither it nor `cpufreq` reports one uniform tier, which is the
    /// right answer for a machine that will not say otherwise.
    ///
    /// `None` only when the CPU list itself cannot be read, which means this is not Linux or the
    /// path was refused — a caller that gets it should leave affinity alone rather than guess.
    pub fn from_sysfs<F>(mut read: F) -> Option<Self>
    where
        F: FnMut(&str) -> Option<String>,
    {
        let present = read("/sys/devices/system/cpu/present")?;
        let ids = parse_cpu_list(&present);
        if ids.is_empty() {
            return None;
        }

        // Capacity where the kernel offers it, frequency where it does not. Not mixed: a part
        // that reports capacity for some cores and frequency for others would have the two on
        // incomparable scales, and the tiers would be an artefact of which file existed.
        let capacities: Vec<Option<u32>> = ids
            .iter()
            .map(|id| {
                read(&format!("/sys/devices/system/cpu/cpu{id}/cpu_capacity"))
                    .and_then(|text| text.trim().parse().ok())
            })
            .collect();

        let raw: Vec<u32> = if capacities.iter().all(Option::is_some) {
            capacities.into_iter().flatten().collect()
        } else {
            ids.iter()
                .map(|id| {
                    read(&format!(
                        "/sys/devices/system/cpu/cpu{id}/cpufreq/cpuinfo_max_freq"
                    ))
                    .and_then(|text| text.trim().parse().ok())
                    .unwrap_or(0)
                })
                .collect()
        };

        Some(Self::new(normalise(&ids, &raw)))
    }

    /// Every CPU, ascending.
    #[must_use]
    pub fn cpus(&self) -> &[Cpu] {
        &self.cpus
    }

    /// The tiers, smallest capacity first.
    #[must_use]
    pub fn tiers(&self) -> Vec<Tier> {
        let mut tiers: Vec<Tier> = Vec::new();
        let mut sorted = self.cpus.clone();
        sorted.sort_unstable_by_key(|cpu| (cpu.capacity, cpu.id));
        for cpu in sorted {
            match tiers.last_mut() {
                Some(tier) if tier.capacity == cpu.capacity => tier.cpus.push(cpu.id),
                _ => tiers.push(Tier {
                    capacity: cpu.capacity,
                    cpus: alloc::vec![cpu.id],
                }),
            }
        }
        tiers
    }

    /// Whether every core is the same size, so there is nothing to pin *to*.
    ///
    /// True of an empty topology, and of an RK3566: four A55s in one cluster.
    #[must_use]
    pub fn is_uniform(&self) -> bool {
        self.tiers().len() <= 1
    }
}

/// Puts capacities on the kernel's 1024 scale.
///
/// A no-op for values already on it. For frequencies it divides through by the largest, which is
/// the best that can be done with the measure available — and is why a part reporting capacity is
/// preferred whenever it does.
fn normalise(ids: &[u32], raw: &[u32]) -> Vec<Cpu> {
    let largest = raw.iter().copied().max().unwrap_or(0);
    ids.iter()
        .zip(raw)
        .map(|(&id, &value)| Cpu {
            id,
            capacity: match largest {
                0 => 1024,
                largest if largest <= 1024 => value,
                largest => {
                    u32::try_from(u64::from(value) * 1024 / u64::from(largest)).unwrap_or(1024)
                }
            },
        })
        .collect()
}

/// Parses a sysfs CPU list: `0-3`, `0,2,4`, `0-1,4-7`.
#[must_use]
pub fn parse_cpu_list(text: &str) -> Vec<u32> {
    let mut ids = Vec::new();
    for part in text.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.split_once('-') {
            Some((first, last)) => {
                let (Ok(first), Ok(last)) = (first.trim().parse(), last.trim().parse::<u32>())
                else {
                    continue;
                };
                for id in first..=last {
                    ids.push(id);
                }
            }
            None => {
                if let Ok(id) = part.parse() {
                    ids.push(id);
                }
            }
        }
    }
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// What a deployment wants done about core sizes.
///
/// A preference rather than a discovery: the part says what it has, and this says what to make of
/// it. The default leaves the scheduler alone, which is what a part with one tier deserves and
/// what a workstation certainly does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Affinity {
    /// Leave placement to the scheduler.
    ///
    /// The right default. A capacity-aware scheduler already moves work toward the cores that
    /// suit it, and pinning against it is how a decode worker ends up queued behind another on a
    /// small core while a large one idles.
    #[default]
    Scheduler,
    /// Keep decode work off the largest cores, so the orchestrator and the renderer have them.
    ///
    /// §5.4's intent, stated as a policy instead of as a fact about one board. On a part with one
    /// tier it asks for nothing, which is the whole of the RK3566 correction.
    SpareTheLargest,
}

impl Affinity {
    /// Which CPUs decode workers should be confined to.
    ///
    /// Empty means "do not confine them", which is not the same as "confine them to nothing" —
    /// a caller that pinned to an empty set would pin the pool out of existence.
    #[must_use]
    pub fn decode_cpus(self, topology: &Topology) -> Vec<u32> {
        match self {
            Self::Scheduler => Vec::new(),
            Self::SpareTheLargest => {
                let tiers = topology.tiers();
                if tiers.len() < 2 {
                    return Vec::new();
                }
                // Everything below the top tier, not merely the bottom one: a three-tier part
                // has two tiers' worth of cores that are not the ones worth reserving.
                tiers[..tiers.len() - 1]
                    .iter()
                    .flat_map(|tier| tier.cpus.iter().copied())
                    .collect()
            }
        }
    }

    /// Which CPUs the orchestrator and the renderer should have.
    ///
    /// The complement, and empty for the same reason.
    #[must_use]
    pub fn foreground_cpus(self, topology: &Topology) -> Vec<u32> {
        match self {
            Self::Scheduler => Vec::new(),
            Self::SpareTheLargest => {
                let tiers = topology.tiers();
                if tiers.len() < 2 {
                    return Vec::new();
                }
                tiers
                    .last()
                    .map(|tier| tier.cpus.clone())
                    .unwrap_or_default()
            }
        }
    }
}
