//! Producing at the rate the stream is consumed, and counting the shape of it (§12.8).
//!
//! # What §12.8 asks for
//!
//! "Produce at the consumption rate the reverse channel reports, not at loop speed"; "parked
//! extends to the scheduler — a parked view holds no timers except cache expiry";
//! "sustained-idle-then-burst beats constant medium load". Pacing counters land in R4, which is
//! this.
//!
//! # Why this is a decision and not a loop
//!
//! Nothing in this crate drives frames. §3.2 puts the tick on the consumer's side — a frame
//! callback before the renderer builds its command set — and the producer is called from it. So
//! what belongs here is the answer to *should this tick produce a frame*, and the counters that
//! say what pattern of answers came out. The caller keeps the loop; this keeps the policy.
//!
//! # Why waking cheaply is not the same as working cheaply
//!
//! A DVFS governor reads utilisation, not usefulness. A producer that wakes every tick and does
//! a little work each time holds the part at a middling frequency indefinitely, which costs more
//! than the same work done in bursts with the part idle between them — that is §12.8's
//! "sustained-idle-then-burst beats constant medium load", and it is why §10's "parked bytes are
//! zero" is not the whole of the property. A parked view that sends nothing but still builds a
//! frame to discover that has spent the power anyway.
//!
//! So the counters here are about *wakeups* rather than bytes, and the two assertions they carry
//! are different: §9.3's parked identity says nothing left the producer, and [`Pacing::emitted`]
//! being zero over a parked run says nothing was built either.
//!
//! # The rule, and why it has a bound
//!
//! Emit when there is something to send and the consumer has drained what it was already sent.
//! That is the consumption rate, read where it is actually reported: the ring's `tail`, which
//! the consumer publishes and [`Producer::consumed_through`] reads.
//!
//! With that alone a consumer that stalls forever stalls the map forever, so it is bounded: a
//! change held longer than the latency budget goes anyway, and the ring's own backpressure is
//! what stops the producer running away after that. The bound is what separates pacing from
//! blocking — a slow consumer makes the map update less often, not stop.

use tessella_capture_abi::ring::Producer;

/// Why a tick produced no frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Idle {
    /// Nothing changed. The cheapest tick there is: no frame built, nothing to send.
    Parked,
    /// The consumer has not finished what it was already sent.
    ///
    /// Producing anyway would grow ring occupancy without the map being any newer when it is
    /// finally drawn — the frames in front of it are drawn first, so the newest is delayed by
    /// exactly the ones that need not have been sent.
    Draining,
}

/// What a tick should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tick {
    /// Build and emit a frame.
    Emit,
    /// Do nothing, for this reason.
    Skip(Idle),
}

/// What the pacer needs to know about a tick.
#[derive(Debug, Clone, Copy)]
pub struct Demand {
    /// Whether anything would be sent: damage, a moved camera, a tile that arrived.
    ///
    /// The caller's own damage state, not a guess. A pacer that decided this for itself would
    /// be a second damage model, and two of those disagree eventually.
    pub changed: bool,
    /// Bytes written but not yet consumed.
    pub outstanding: u64,
    /// Now, in nanoseconds, on any clock that does not step backwards.
    pub now_ns: u64,
}

impl Demand {
    /// Reads what the ring already reports, for a caller that has one.
    ///
    /// The consumption rate needs no new field anywhere: `head` and `tail` are the producer's
    /// and consumer's own counters, and the difference between them is what the consumer has
    /// not caught up on.
    #[must_use]
    pub fn from_ring(producer: &Producer, changed: bool, now_ns: u64) -> Self {
        Self {
            changed,
            outstanding: producer.head() - producer.consumed_through(),
            now_ns,
        }
    }
}

/// Paces production against consumption, and counts the shape of the result.
#[derive(Debug)]
pub struct Pacer {
    latency_budget_ns: u64,
    /// When the oldest unsent change appeared, for the latency bound.
    pending_since: Option<u64>,
    pacing: Pacing,
    /// Whether the last tick emitted, for run lengths.
    last_emitted: bool,
}

/// How long a change may wait for a consumer that has stopped draining.
///
/// One sixtieth of a second. Not a tuning constant so much as a definition: past this the map is
/// visibly behind the world whatever the consumer is doing, and the ring's backpressure is a
/// better place to be than a held change — it at least fails loudly.
pub const DEFAULT_LATENCY_BUDGET_NS: u64 = 16_666_667;

impl Default for Pacer {
    fn default() -> Self {
        Self::new(DEFAULT_LATENCY_BUDGET_NS)
    }
}

impl Pacer {
    /// A pacer that holds a change no longer than `latency_budget_ns`.
    #[must_use]
    pub fn new(latency_budget_ns: u64) -> Self {
        Self {
            latency_budget_ns,
            pending_since: None,
            pacing: Pacing::default(),
            last_emitted: false,
        }
    }

    /// What this tick should do.
    ///
    /// Call once per tick, whether or not the answer is acted on — the counters are of *ticks*,
    /// and a caller that only asked when it already intended to emit would be measuring its own
    /// intentions.
    pub fn tick(&mut self, demand: &Demand) -> Tick {
        let decision = self.decide(demand);
        self.pacing.record(decision, self.last_emitted);
        self.last_emitted = decision == Tick::Emit;
        if decision == Tick::Emit {
            self.pending_since = None;
        }
        decision
    }

    fn decide(&mut self, demand: &Demand) -> Tick {
        if !demand.changed {
            // A change that was being held is not forgotten by a quiet tick; it is still held.
            return Tick::Skip(Idle::Parked);
        }
        let since = *self.pending_since.get_or_insert(demand.now_ns);
        if demand.outstanding == 0 {
            return Tick::Emit;
        }
        // Held, unless it has been held long enough. `saturating_sub` rather than a subtraction:
        // a clock that went backwards is a caller fault, and reading it as an enormous wait
        // would turn one into a burst of frames.
        if demand.now_ns.saturating_sub(since) >= self.latency_budget_ns {
            return Tick::Emit;
        }
        Tick::Skip(Idle::Draining)
    }

    /// The counters.
    #[must_use]
    pub fn pacing(&self) -> &Pacing {
        &self.pacing
    }
}

/// What the ticks did.
///
/// §9.3's counters are of bytes on the wire; these are of wakeups, which is the other half of
/// §12.8 — a producer can send nothing and still have spent the power to find that out.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Pacing {
    /// Ticks seen.
    pub wakeups: u64,
    /// Ticks that produced a frame.
    pub emitted: u64,
    /// Ticks that skipped because nothing had changed.
    pub parked: u64,
    /// Ticks that skipped because the consumer had not caught up.
    pub draining: u64,
    /// Runs of consecutive skips.
    pub idle_runs: u64,
    /// The longest such run.
    pub longest_idle_run: u64,
    /// Runs of consecutive emissions.
    pub bursts: u64,
    /// The longest such run.
    pub longest_burst: u64,
    /// Length of the run in progress.
    run: u64,
}

impl Pacing {
    fn record(&mut self, decision: Tick, was_emitting: bool) {
        self.wakeups += 1;
        let emitting = decision == Tick::Emit;
        match decision {
            Tick::Emit => self.emitted += 1,
            Tick::Skip(Idle::Parked) => self.parked += 1,
            Tick::Skip(Idle::Draining) => self.draining += 1,
        }
        if self.wakeups > 1 && emitting == was_emitting {
            self.run += 1;
        } else {
            self.run = 1;
            if emitting {
                self.bursts += 1;
            } else {
                self.idle_runs += 1;
            }
        }
        if emitting {
            self.longest_burst = self.longest_burst.max(self.run);
        } else {
            self.longest_idle_run = self.longest_idle_run.max(self.run);
        }
    }

    /// Ticks that produced nothing.
    #[must_use]
    pub const fn idle(&self) -> u64 {
        self.parked + self.draining
    }

    /// Whether the wakeups came in bursts rather than as a steady dribble.
    ///
    /// §12.8's shape, made checkable: what a DVFS governor punishes is a producer that is busy a
    /// little of every tick, so the question is whether the emissions clumped. A run of one is a
    /// dribble however many of them there are.
    ///
    /// True for a producer that never emitted: nothing to clump, and no power spent.
    #[must_use]
    pub const fn is_bursty(&self) -> bool {
        self.bursts == 0 || self.longest_burst > 1
    }
}
